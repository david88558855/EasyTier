use std::{
    io,
    net::Ipv4Addr,
    pin::Pin,
    task::{Context, Poll},
};

use crate::{
    common::{
        error::Error,
        global_ctx::ArcGlobalCtx,
        ifcfg::{IfConfiger, IfConfiguerTrait},
    },
    tunnel::{
        common::{FramedWriter, TunnelWrapper, ZCPacketToBytes},
        packet_def::ZCPacket,
        StreamItem, Tunnel, TunnelError,
    },
};

use byteorder::WriteBytesExt as _;
use futures::{lock::BiLock, ready, Stream};
use pin_project_lite::pin_project;
use tokio::io::AsyncWrite;
use tokio_util::{bytes::Bytes, io::poll_read_buf};
use tun::{create_as_async, AsyncDevice, Configuration, Device as _, Layer};
use zerocopy::{NativeEndian, NetworkEndian};

pin_project! {
    pub struct TunStream {
        #[pin]
        l: BiLock<AsyncDevice>,
        cur_packet: Option<ZCPacket>,
    }
}

impl TunStream {
    pub fn new(l: BiLock<AsyncDevice>) -> Self {
        Self {
            l,
            cur_packet: None,
        }
    }
}

impl Stream for TunStream {
    type Item = StreamItem;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<StreamItem>> {
        let self_mut = self.project();
        let mut g = ready!(self_mut.l.poll_lock(cx));
        if self_mut.cur_packet.is_none() {
            *self_mut.cur_packet = Some(ZCPacket::new_with_reserved_payload(2048));
        }
        let cur_packet = self_mut.cur_packet.as_mut().unwrap();
        match ready!(poll_read_buf(
            g.as_pin_mut(),
            cx,
            &mut cur_packet.mut_inner()
        )) {
            Ok(0) => Poll::Ready(None),
            Ok(_n) => Poll::Ready(Some(Ok(self_mut.cur_packet.take().unwrap()))),
            Err(err) => {
                println!("tun stream error: {:?}", err);
                Poll::Ready(None)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
enum PacketProtocol {
    #[default]
    IPv4,
    IPv6,
    Other(u8),
}

// Note: the protocol in the packet information header is platform dependent.
impl PacketProtocol {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn into_pi_field(self) -> Result<u16, io::Error> {
        use nix::libc;
        match self {
            PacketProtocol::IPv4 => Ok(libc::ETH_P_IP as u16),
            PacketProtocol::IPv6 => Ok(libc::ETH_P_IPV6 as u16),
            PacketProtocol::Other(_) => Err(io::Error::new(
                io::ErrorKind::Other,
                "neither an IPv4 nor IPv6 packet",
            )),
        }
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    fn into_pi_field(self) -> Result<u16, io::Error> {
        use nix::libc;
        match self {
            PacketProtocol::IPv4 => Ok(libc::PF_INET as u16),
            PacketProtocol::IPv6 => Ok(libc::PF_INET6 as u16),
            PacketProtocol::Other(_) => Err(io::Error::new(
                io::ErrorKind::Other,
                "neither an IPv4 nor IPv6 packet",
            )),
        }
    }

    #[cfg(target_os = "windows")]
    fn into_pi_field(self) -> Result<u16, io::Error> {
        unimplemented!()
    }
}

/// Infer the protocol based on the first nibble in the packet buffer.
fn infer_proto(buf: &[u8]) -> PacketProtocol {
    match buf[0] >> 4 {
        4 => PacketProtocol::IPv4,
        6 => PacketProtocol::IPv6,
        p => PacketProtocol::Other(p),
    }
}

struct TunZCPacketToBytes {
    has_packet_info: bool,
}

impl TunZCPacketToBytes {
    pub fn new(has_packet_info: bool) -> Self {
        Self { has_packet_info }
    }

    pub fn fill_packet_info(&self, mut buf: &mut [u8]) -> Result<(), io::Error> {
        // flags is always 0
        buf.write_u16::<NativeEndian>(0)?;
        // write the protocol as network byte order
        buf.write_u16::<NetworkEndian>(infer_proto(&buf).into_pi_field()?)?;
        Ok(())
    }
}

impl ZCPacketToBytes for TunZCPacketToBytes {
    fn into_bytes(&self, zc_packet: ZCPacket) -> Result<Bytes, TunnelError> {
        let payload_offset = zc_packet.payload_offset();
        let mut inner = zc_packet.inner();
        // we have peer manager header, so payload offset must larger than 4
        assert!(payload_offset >= 4);

        let ret = if self.has_packet_info {
            let mut inner = inner.split_off(payload_offset - 4);
            self.fill_packet_info(&mut inner[0..4])?;
            inner
        } else {
            inner.split_off(payload_offset)
        };

        tracing::debug!(?ret, ?payload_offset, "convert zc packet to tun packet");

        Ok(ret.into())
    }
}

pin_project! {
    pub struct TunAsyncWrite {
        #[pin]
        l: BiLock<AsyncDevice>,
    }
}

impl AsyncWrite for TunAsyncWrite {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        let self_mut = self.project();
        let mut g = ready!(self_mut.l.poll_lock(cx));
        g.as_pin_mut().poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let self_mut = self.project();
        let mut g = ready!(self_mut.l.poll_lock(cx));
        g.as_pin_mut().poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let self_mut = self.project();
        let mut g = ready!(self_mut.l.poll_lock(cx));
        g.as_pin_mut().poll_shutdown(cx)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<Result<usize, io::Error>> {
        let self_mut = self.project();
        let mut g = ready!(self_mut.l.poll_lock(cx));
        g.as_pin_mut().poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        true
    }
}

pub struct VirtualNic {
    dev_name: String,
    queue_num: usize,

    global_ctx: ArcGlobalCtx,

    ifname: Option<String>,
    ifcfg: Box<dyn IfConfiguerTrait + Send + Sync + 'static>,
}

impl VirtualNic {
    pub fn new(global_ctx: ArcGlobalCtx) -> Self {
        Self {
            dev_name: "".to_owned(),
            queue_num: 1,
            global_ctx,
            ifname: None,
            ifcfg: Box::new(IfConfiger {}),
        }
    }

    pub fn set_dev_name(mut self, dev_name: &str) -> Result<Self, Error> {
        self.dev_name = dev_name.to_owned();
        Ok(self)
    }

    pub fn set_queue_num(mut self, queue_num: usize) -> Result<Self, Error> {
        self.queue_num = queue_num;
        Ok(self)
    }

    async fn create_dev_ret_err(&mut self) -> Result<Box<dyn Tunnel>, Error> {
        let mut config = Configuration::default();
        let has_packet_info = cfg!(target_os = "macos");
        config.layer(Layer::L3);

        #[cfg(target_os = "linux")]
        {
            config.platform(|config| {
                // detect protocol by ourselves for cross platform
                config.packet_information(false);
            });
        }

        if self.queue_num != 1 {
            todo!("queue_num != 1")
        }
        config.queues(self.queue_num);
        config.up();

        let dev = {
            let _g = self.global_ctx.net_ns.guard();
            create_as_async(&config)?
        };

        let ifname = dev.get_ref().name()?;
        self.ifcfg.wait_interface_show(ifname.as_str()).await?;

        let (a, b) = BiLock::new(dev);

        let ft = TunnelWrapper::new(
            TunStream::new(a),
            FramedWriter::new_with_converter(
                TunAsyncWrite { l: b },
                TunZCPacketToBytes::new(has_packet_info),
            ),
            None,
        );

        self.ifname = Some(ifname.to_owned());
        Ok(Box::new(ft))
    }

    pub async fn create_dev(&mut self) -> Result<Box<dyn Tunnel>, Error> {
        self.create_dev_ret_err().await
    }

    pub fn ifname(&self) -> &str {
        self.ifname.as_ref().unwrap().as_str()
    }

    pub async fn link_up(&self) -> Result<(), Error> {
        let _g = self.global_ctx.net_ns.guard();
        self.ifcfg.set_link_status(self.ifname(), true).await?;
        Ok(())
    }

    pub async fn add_route(&self, address: Ipv4Addr, cidr: u8) -> Result<(), Error> {
        let _g = self.global_ctx.net_ns.guard();
        self.ifcfg
            .add_ipv4_route(self.ifname(), address, cidr)
            .await?;
        Ok(())
    }

    pub async fn remove_ip(&self, ip: Option<Ipv4Addr>) -> Result<(), Error> {
        let _g = self.global_ctx.net_ns.guard();
        self.ifcfg.remove_ip(self.ifname(), ip).await?;
        Ok(())
    }

    pub async fn add_ip(&self, ip: Ipv4Addr, cidr: i32) -> Result<(), Error> {
        let _g = self.global_ctx.net_ns.guard();
        self.ifcfg
            .add_ipv4_ip(self.ifname(), ip, cidr as u8)
            .await?;
        Ok(())
    }

    pub fn get_ifcfg(&self) -> impl IfConfiguerTrait {
        IfConfiger {}
    }
}
#[cfg(test)]
mod tests {
    use crate::common::{error::Error, global_ctx::tests::get_mock_global_ctx};

    use super::VirtualNic;

    async fn run_test_helper() -> Result<VirtualNic, Error> {
        let mut dev = VirtualNic::new(get_mock_global_ctx());
        let _tunnel = dev.create_dev().await?;

        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        dev.link_up().await?;
        dev.remove_ip(None).await?;
        dev.add_ip("10.144.111.1".parse().unwrap(), 24).await?;
        Ok(dev)
    }

    #[tokio::test]
    async fn tun_test() {
        let _dev = run_test_helper().await.unwrap();

        // let mut stream = nic.pin_recv_stream();
        // while let Some(item) = stream.next().await {
        //     println!("item: {:?}", item);
        // }

        // let framed = dev.into_framed();
        // let (mut s, mut b) = framed.split();
        // loop {
        //     let tmp = b.next().await.unwrap().unwrap();
        //     let tmp = EthernetPacket::new(tmp.get_bytes());
        //     println!("ret: {:?}", tmp.unwrap());
        // }
    }
}