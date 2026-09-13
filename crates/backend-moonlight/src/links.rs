//! The two seams a transport plugs into.
//!
//! A [`SessionAuthority`] starts and stops sessions on the host (natively the
//! paired HTTPS endpoints; in a browser the altc API), and [`StreamLinks`]
//! opens the channels the negotiated session needs (natively TCP, UDP and
//! ENet; in a browser one WebTransport session). Everything between —
//! negotiation, the control session, reassembly — is shared.

use crate::{
    AudioReceive, ControlLink, LaunchedSession, MediaLink, Negotiated, Rtsp, RtspExchange,
    ServerInfo, StreamMode,
};
use gsa_core::Result;
use gsa_core::runtime::MaybeSend;

/// Starts, rejoins and stops sessions on a host.
pub trait SessionAuthority {
    /// The host's own description of itself and what it is running.
    fn server_info(&self) -> impl std::future::Future<Output = Result<ServerInfo>>;
    /// Start `app_id` and get the session's RTSP address and control key.
    fn launch(
        &self,
        app_id: u32,
        mode: StreamMode,
    ) -> impl std::future::Future<Output = Result<LaunchedSession>>;
    /// Rejoin the session the host already holds for this client.
    fn resume(
        &self,
        mode: StreamMode,
    ) -> impl std::future::Future<Output = Result<LaunchedSession>>;
    /// Stop whatever the host is streaming. Safe to call when idle.
    fn cancel(&self) -> impl std::future::Future<Output = Result<()>>;
}

/// Opens the channels of one negotiated session.
pub trait StreamLinks {
    type Rtsp: RtspExchange;
    type Control: ControlLink + MaybeSend + 'static;
    type Media: MediaLink + MaybeSend + 'static;

    /// The RTSP exchange for `rtsp`, before negotiation. `launched` is the
    /// launch that produced it, for links that bind to a session by token.
    fn rtsp(
        &mut self,
        launched: &LaunchedSession,
        rtsp: &Rtsp,
    ) -> impl std::future::Future<Output = Result<Self::Rtsp>>;
    /// The control link, bound to the session the host just negotiated.
    fn control(
        &mut self,
        negotiated: &Negotiated,
    ) -> impl std::future::Future<Output = Result<Self::Control>>;
    /// The media link, pinging whatever the host needs pinged.
    fn media(
        &mut self,
        negotiated: &Negotiated,
    ) -> impl std::future::Future<Output = Result<Self::Media>>;
    /// The audio receiver and the PCM channel it fills — a channel that never
    /// fills where decoding happens elsewhere (the browser).
    fn audio(
        &mut self,
        negotiated: &Negotiated,
    ) -> Result<(AudioReceive, std::sync::mpsc::Receiver<Vec<i16>>)>;
}
