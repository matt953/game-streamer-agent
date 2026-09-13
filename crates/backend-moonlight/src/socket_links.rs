//! The native links: TCP for RTSP, ENet over UDP for control, UDP for media,
//! libopus for audio.

use crate::links::StreamLinks;
use crate::{AudioReceive, EnetLink, Negotiated, Rtsp, TcpRtsp, UdpMediaLink};
use gsa_core::Result;

/// Sockets straight to `host_ip`, as a client on the same network dials.
#[derive(Debug, Clone, Copy)]
pub struct SocketLinks {
    host_ip: std::net::IpAddr,
}

impl SocketLinks {
    #[must_use]
    pub fn new(host_ip: std::net::IpAddr) -> Self {
        Self { host_ip }
    }
}

impl StreamLinks for SocketLinks {
    type Rtsp = TcpRtsp;
    type Control = EnetLink;
    type Media = UdpMediaLink;

    async fn rtsp(&mut self, rtsp: &Rtsp) -> Result<TcpRtsp> {
        Ok(TcpRtsp::new(rtsp.addr()?))
    }

    async fn control(&mut self, negotiated: &Negotiated) -> Result<EnetLink> {
        let addr = std::net::SocketAddr::new(self.host_ip, negotiated.control_port);
        EnetLink::connect(addr, negotiated.connect_data.unwrap_or(0))
    }

    async fn media(&mut self, negotiated: &Negotiated) -> Result<UdpMediaLink> {
        let video = std::net::SocketAddr::new(self.host_ip, negotiated.video_port);
        UdpMediaLink::open(video, negotiated.audio_port, negotiated.ping_payload)
    }

    fn audio(
        &mut self,
        negotiated: &Negotiated,
    ) -> Result<(AudioReceive, std::sync::mpsc::Receiver<Vec<i16>>)> {
        AudioReceive::new(negotiated.surround.as_ref())
    }
}
