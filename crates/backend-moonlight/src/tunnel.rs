//! The tunnel links: Moonlight over one WebTransport session, on any runtime.
//!
//! Everything here is protocol; the session itself — a browser's
//! `WebTransport` object or a native QUIC client — is supplied through
//! [`TunnelSession`], so the same code serves the web client and the native
//! test client that proves the server end. The wire format is
//! [`gsa_tunnel`]'s; this module is what a [`StreamLinks`] implementation
//! over it looks like.

use crate::links::StreamLinks;
use crate::{
    AudioReceive, ControlLink, Delivery, LinkEvent, MediaDatagram, MediaLink, Negotiated, Outgoing,
    Rtsp, RtspExchange, SinkBox,
};
use gsa_core::runtime::MaybeSend;
use gsa_core::{Error, Result};
use gsa_tunnel::{Datagram, Frame, FrameReader, Hello, Reject, StreamKind, Welcome, datagram};
use std::future::Future;

/// The sending half of one bidirectional stream.
pub trait StreamWriter: MaybeSend + 'static {
    /// Put `bytes` on the stream, in order; completes once accepted for
    /// sending, not once delivered.
    fn write(&mut self, bytes: &[u8]) -> impl Future<Output = Result<()>> + MaybeSend;
    /// Tell the peer no more is coming.
    fn finish(&mut self) -> impl Future<Output = ()> + MaybeSend;
}

/// The receiving half of one bidirectional stream.
pub trait StreamReader: MaybeSend + 'static {
    /// The next bytes the peer sent, in whatever pieces the transport
    /// delivers them; `None` once the peer finished the stream.
    fn read(&mut self) -> impl Future<Output = Result<Option<Vec<u8>>>> + MaybeSend;
}

/// One established WebTransport session, cheap to clone: the pump that
/// demultiplexes datagrams and the links that send on it each hold one.
pub trait TunnelSession: Clone + MaybeSend + 'static {
    type Writer: StreamWriter;
    type Reader: StreamReader;

    /// Open a bidirectional stream to the server.
    fn open_stream(&self)
    -> impl Future<Output = Result<(Self::Writer, Self::Reader)>> + MaybeSend;
    /// Send one datagram; dropped rather than queued when the transport is
    /// congested, which is what unreliable delivery means.
    fn send_datagram(&self, bytes: &[u8]) -> Result<()>;
    /// The next datagram from the server; `None` once the session is gone.
    fn recv_datagram(&self) -> impl Future<Output = Option<Vec<u8>>> + MaybeSend;
}

/// [`StreamLinks`] over a tunnel session.
pub struct TunnelLinks<S: TunnelSession> {
    session: S,
    token: Vec<u8>,
    audio: Option<SinkBox>,
    control_rx: Option<tokio::sync::mpsc::UnboundedReceiver<LinkEvent>>,
    media_rx: Option<tokio::sync::mpsc::UnboundedReceiver<MediaDatagram>>,
    pump: Option<Pump>,
}

impl<S: TunnelSession> std::fmt::Debug for TunnelLinks<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TunnelLinks")
            .field("token_len", &self.token.len())
            .field("pump_started", &self.pump.is_none())
            .finish_non_exhaustive()
    }
}

/// The sending ends the datagram pump feeds, held until it starts.
struct Pump {
    control: tokio::sync::mpsc::UnboundedSender<LinkEvent>,
    media: tokio::sync::mpsc::UnboundedSender<MediaDatagram>,
}

impl<S: TunnelSession> TunnelLinks<S> {
    /// Links over `session`, joining the Moonlight session `token` was
    /// minted for. Opus audio goes to `audio`: the browser decodes it with
    /// WebCodecs, so no PCM comes back through these links.
    pub fn new(session: S, token: impl Into<Vec<u8>>, audio: SinkBox) -> Self {
        let (control_tx, control_rx) = tokio::sync::mpsc::unbounded_channel();
        let (media_tx, media_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            session,
            token: token.into(),
            audio: Some(audio),
            control_rx: Some(control_rx),
            media_rx: Some(media_rx),
            pump: Some(Pump {
                control: control_tx,
                media: media_tx,
            }),
        }
    }

    /// Start routing datagrams by tag, once. Runs until the session ends;
    /// closing the channels then tells both links.
    fn start_pump(&mut self) {
        let Some(Pump { control, media }) = self.pump.take() else {
            return;
        };
        let session = self.session.clone();
        gsa_core::runtime::spawn(async move {
            let clock = gsa_core::time::MediaClock::new();
            while let Some(bytes) = session.recv_datagram().await {
                let arrival_us = clock.now_us();
                let Ok(Datagram { tag, payload }) = Datagram::decode(&bytes) else {
                    continue;
                };
                match tag {
                    datagram::VIDEO | datagram::AUDIO => {
                        if media
                            .send(MediaDatagram {
                                bytes: payload.to_vec(),
                                arrival_us,
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                    datagram::CONTROL => {
                        if control.send(LinkEvent::Frame(payload.to_vec())).is_err() {
                            return;
                        }
                    }
                    other => tracing::debug!(tag = other, "datagram with an unknown tag"),
                }
            }
        });
    }
}

/// Reads the server's one-byte answer, keeping whatever followed it.
async fn read_welcome<R: StreamReader>(reader: &mut R, rest: &mut FrameReader) -> Result<()> {
    let Some(bytes) = reader.read().await? else {
        return Err(Error::Transport("tunnel closed before answering".into()));
    };
    let Some((first, tail)) = bytes.split_first() else {
        return Err(Error::Transport("tunnel answered with nothing".into()));
    };
    rest.push(tail);
    match Welcome::decode(*first).map_err(|e| Error::Transport(format!("tunnel: {e}")))? {
        Welcome::Accepted => Ok(()),
        Welcome::Rejected(reject) => Err(match reject {
            Reject::Token => Error::Auth(reject.to_string()),
            Reject::SessionGone => Error::Session(reject.to_string()),
            Reject::Version | Reject::Protocol => Error::Transport(reject.to_string()),
        }),
    }
}

impl<S: TunnelSession> StreamLinks for TunnelLinks<S> {
    type Rtsp = TunnelRtsp<S::Writer, S::Reader>;
    type Control = TunnelControlLink<S>;
    type Media = TunnelMediaLink;

    async fn rtsp(&mut self, _rtsp: &Rtsp) -> Result<Self::Rtsp> {
        let (mut writer, mut reader) = self.session.open_stream().await?;
        let mut opening = StreamKind::Rtsp.encode();
        opening.extend(Frame::encode(&Hello::new(self.token.clone()).encode()));
        writer.write(&opening).await?;
        let mut frames = FrameReader::new();
        read_welcome(&mut reader, &mut frames).await?;
        Ok(TunnelRtsp {
            writer,
            reader,
            frames,
        })
    }

    async fn control(&mut self, _negotiated: &Negotiated) -> Result<Self::Control> {
        let events = self
            .control_rx
            .take()
            .ok_or_else(|| Error::Transport("control link already opened".into()))?;
        let Some(Pump { control, .. }) = self.pump.as_ref() else {
            return Err(Error::Transport("control link already opened".into()));
        };
        let control = control.clone();
        // One stream per ENet channel: the host routes on the channel, and
        // each keeps its own ordering as ENet channels do.
        let mut writers = Vec::new();
        for channel in [
            crate::input::channel::GENERIC,
            crate::input::channel::URGENT,
        ] {
            let (mut writer, mut reader) = self.session.open_stream().await?;
            writer
                .write(&StreamKind::Control { channel }.encode())
                .await?;
            let mut frames = FrameReader::new();
            read_welcome(&mut reader, &mut frames).await?;
            writers.push((channel, writer));
            let events = control.clone();
            gsa_core::runtime::spawn(read_control_stream(reader, frames, events));
        }
        self.start_pump();
        Ok(TunnelControlLink {
            session: self.session.clone(),
            writers,
            events,
            connected_reported: false,
            ended: false,
        })
    }

    async fn media(&mut self, _negotiated: &Negotiated) -> Result<Self::Media> {
        let datagrams = self
            .media_rx
            .take()
            .ok_or_else(|| Error::Transport("media link already opened".into()))?;
        self.start_pump();
        Ok(TunnelMediaLink { datagrams })
    }

    fn audio(
        &mut self,
        _negotiated: &Negotiated,
    ) -> Result<(AudioReceive, std::sync::mpsc::Receiver<Vec<i16>>)> {
        let sink = self
            .audio
            .take()
            .ok_or_else(|| Error::Transport("audio already opened".into()))?;
        // Nothing ever arrives here: the sink decodes elsewhere.
        let (_never, pcm) = std::sync::mpsc::channel();
        Ok((AudioReceive::with_sink(sink), pcm))
    }
}

/// Reliable control frames from one stream become link events until the
/// server finishes it.
async fn read_control_stream<R: StreamReader>(
    mut reader: R,
    mut frames: FrameReader,
    events: tokio::sync::mpsc::UnboundedSender<LinkEvent>,
) {
    loop {
        match frames.next_frame() {
            Ok(Some(frame)) => {
                if events.send(LinkEvent::Frame(frame)).is_err() {
                    return;
                }
                continue;
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, "control stream corrupt");
                return;
            }
        }
        match reader.read().await {
            Ok(Some(bytes)) => frames.push(&bytes),
            Ok(None) => return,
            Err(e) => {
                tracing::debug!(error = %e, "control stream ended");
                return;
            }
        }
    }
}

/// RTSP over the session stream: one framed request, one framed reply.
pub struct TunnelRtsp<W, R> {
    writer: W,
    reader: R,
    frames: FrameReader,
}

impl<W, R> std::fmt::Debug for TunnelRtsp<W, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TunnelRtsp")
            .field("pending", &self.frames.pending())
            .finish_non_exhaustive()
    }
}

impl<W: StreamWriter, R: StreamReader> RtspExchange for TunnelRtsp<W, R> {
    async fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>> {
        self.writer.write(&Frame::encode(request)).await?;
        loop {
            if let Some(reply) = self
                .frames
                .next_frame()
                .map_err(|e| Error::Transport(format!("rtsp over tunnel: {e}")))?
            {
                return Ok(reply);
            }
            match self.reader.read().await? {
                Some(bytes) => self.frames.push(&bytes),
                None => return Err(Error::Transport("tunnel closed during rtsp".into())),
            }
        }
    }
}

/// The control channel: reliable frames on per-channel streams, unreliable
/// ones as tagged datagrams, everything from the server merged into one
/// event queue.
pub struct TunnelControlLink<S: TunnelSession> {
    session: S,
    writers: Vec<(u8, S::Writer)>,
    events: tokio::sync::mpsc::UnboundedReceiver<LinkEvent>,
    connected_reported: bool,
    ended: bool,
}

impl<S: TunnelSession> std::fmt::Debug for TunnelControlLink<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TunnelControlLink")
            .field("streams", &self.writers.len())
            .field("ended", &self.ended)
            .finish_non_exhaustive()
    }
}

impl<S: TunnelSession> ControlLink for TunnelControlLink<S> {
    async fn send(&mut self, out: &Outgoing) -> Result<()> {
        match out.delivery {
            Delivery::Reliable => {
                // A channel without its own stream rides the first one: the
                // host routes on the message, the channel only orders it.
                let index = self
                    .writers
                    .iter()
                    .position(|(channel, _)| *channel == out.channel)
                    .unwrap_or(0);
                let (_, writer) = self
                    .writers
                    .get_mut(index)
                    .ok_or_else(|| Error::Transport("control link has no stream".into()))?;
                writer.write(&Frame::encode(&out.frame)).await
            }
            Delivery::Unreliable => self.session.send_datagram(
                &Datagram {
                    tag: datagram::CONTROL,
                    payload: &out.frame,
                }
                .encode(),
            ),
        }
    }

    async fn recv(&mut self) -> Option<LinkEvent> {
        if !self.connected_reported {
            // Every control stream was welcomed before this link existed.
            self.connected_reported = true;
            return Some(LinkEvent::Connected);
        }
        if self.ended {
            return None;
        }
        match self.events.recv().await {
            Some(event) => Some(event),
            None => {
                self.ended = true;
                Some(LinkEvent::Disconnected)
            }
        }
    }

    async fn close(&mut self) {
        for (_, writer) in &mut self.writers {
            writer.finish().await;
        }
        self.writers.clear();
    }
}

/// Media datagrams as the pump routes them, arrival already stamped.
#[derive(Debug)]
pub struct TunnelMediaLink {
    datagrams: tokio::sync::mpsc::UnboundedReceiver<MediaDatagram>,
}

impl MediaLink for TunnelMediaLink {
    async fn recv(&mut self) -> Option<MediaDatagram> {
        self.datagrams.recv().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::OpusSink;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    /// An in-memory session: streams are channel pairs the test's "server"
    /// side answers by hand.
    #[derive(Clone)]
    struct MockSession {
        inner: Arc<Mutex<MockInner>>,
        datagrams_in: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<Vec<u8>>>>,
    }

    struct MockInner {
        /// Streams the client opened, in order: (client→server bytes, server→client sender).
        opened: Vec<ServerEnd>,
        sent_datagrams: Vec<Vec<u8>>,
    }

    struct ServerEnd {
        from_client: mpsc::UnboundedReceiver<Vec<u8>>,
        to_client: mpsc::UnboundedSender<Vec<u8>>,
        finished: Arc<std::sync::atomic::AtomicBool>,
    }

    struct MockWriter {
        tx: mpsc::UnboundedSender<Vec<u8>>,
        finished: Arc<std::sync::atomic::AtomicBool>,
    }
    struct MockReader {
        rx: mpsc::UnboundedReceiver<Vec<u8>>,
    }

    impl StreamWriter for MockWriter {
        async fn write(&mut self, bytes: &[u8]) -> Result<()> {
            self.tx
                .send(bytes.to_vec())
                .map_err(|_| Error::Transport("mock stream closed".into()))
        }
        async fn finish(&mut self) {
            self.finished
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl StreamReader for MockReader {
        async fn read(&mut self) -> Result<Option<Vec<u8>>> {
            Ok(self.rx.recv().await)
        }
    }

    impl TunnelSession for MockSession {
        type Writer = MockWriter;
        type Reader = MockReader;

        async fn open_stream(&self) -> Result<(MockWriter, MockReader)> {
            let (c2s_tx, c2s_rx) = mpsc::unbounded_channel();
            let (s2c_tx, s2c_rx) = mpsc::unbounded_channel();
            let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
            self.inner.lock().unwrap().opened.push(ServerEnd {
                from_client: c2s_rx,
                to_client: s2c_tx,
                finished: finished.clone(),
            });
            Ok((
                MockWriter {
                    tx: c2s_tx,
                    finished,
                },
                MockReader { rx: s2c_rx },
            ))
        }

        fn send_datagram(&self, bytes: &[u8]) -> Result<()> {
            self.inner
                .lock()
                .unwrap()
                .sent_datagrams
                .push(bytes.to_vec());
            Ok(())
        }

        async fn recv_datagram(&self) -> Option<Vec<u8>> {
            self.datagrams_in.lock().await.recv().await
        }
    }

    fn mock() -> (MockSession, mpsc::UnboundedSender<Vec<u8>>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            MockSession {
                inner: Arc::new(Mutex::new(MockInner {
                    opened: Vec::new(),
                    sent_datagrams: Vec::new(),
                })),
                datagrams_in: Arc::new(tokio::sync::Mutex::new(rx)),
            },
            tx,
        )
    }

    #[derive(Default)]
    struct CountingSink(Arc<std::sync::atomic::AtomicUsize>);
    impl OpusSink for CountingSink {
        fn frame(&mut self, _opus: &[u8]) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        fn lost(&mut self, _count: u16) {}
    }

    fn negotiated() -> Negotiated {
        Negotiated {
            video_port: 0,
            audio_port: 0,
            control_port: 0,
            ping_payload: None,
            connect_data: None,
            encryption_supported: 0,
            encryption_requested: 0,
            reference_invalidation: false,
            surround: None,
        }
    }

    /// The test's server: takes the n-th opened stream's ends.
    fn server_end(session: &MockSession, index: usize) -> ServerEnd {
        let mut inner = session.inner.lock().unwrap();
        assert!(inner.opened.len() > index, "stream {index} was not opened");
        let end = &mut inner.opened[index];
        let (tx, _) = mpsc::unbounded_channel();
        let (_, rx) = mpsc::unbounded_channel();
        ServerEnd {
            from_client: std::mem::replace(&mut end.from_client, rx),
            to_client: std::mem::replace(&mut end.to_client, tx),
            finished: end.finished.clone(),
        }
    }

    #[tokio::test]
    async fn rtsp_stream_says_hello_then_exchanges_framed_requests() {
        let (session, _dg) = mock();
        let mut links = TunnelLinks::new(
            session.clone(),
            b"tok".to_vec(),
            Box::new(CountingSink::default()),
        );
        let rtsp = Rtsp::new("rtsp://127.0.0.1:48010").unwrap();

        // The server side: welcome, then answer one request.
        let server = tokio::spawn({
            let session = session.clone();
            async move {
                // Give the client a moment to open the stream.
                while session.inner.lock().unwrap().opened.is_empty() {
                    tokio::task::yield_now().await;
                }
                let mut end = server_end(&session, 0);
                let opening = end.from_client.recv().await.unwrap();
                let (kind, used) = StreamKind::decode(&opening).unwrap();
                assert_eq!(kind, StreamKind::Rtsp);
                let mut frames = FrameReader::new();
                frames.push(&opening[used..]);
                let hello = Hello::decode(&frames.next_frame().unwrap().unwrap()).unwrap();
                assert_eq!(hello, Hello::new(b"tok".to_vec()));
                end.to_client
                    .send(vec![Welcome::Accepted.encode()])
                    .unwrap();
                let request = end.from_client.recv().await.unwrap();
                frames.push(&request);
                let request = frames.next_frame().unwrap().unwrap();
                assert_eq!(request, b"OPTIONS rtsp://x RTSP/1.0\r\n\r\n");
                // Reply split across two writes to prove reassembly.
                let reply = Frame::encode(b"RTSP/1.0 200 OK\r\n\r\n");
                end.to_client.send(reply[..3].to_vec()).unwrap();
                end.to_client.send(reply[3..].to_vec()).unwrap();
            }
        });

        let mut exchange = links.rtsp(&rtsp).await.unwrap();
        let reply = exchange
            .exchange(b"OPTIONS rtsp://x RTSP/1.0\r\n\r\n")
            .await
            .unwrap();
        assert_eq!(reply, b"RTSP/1.0 200 OK\r\n\r\n");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_rejected_token_is_an_auth_error() {
        let (session, _dg) = mock();
        let mut links = TunnelLinks::new(
            session.clone(),
            b"bad".to_vec(),
            Box::new(CountingSink::default()),
        );
        let rtsp = Rtsp::new("rtsp://127.0.0.1:48010").unwrap();
        tokio::spawn({
            let session = session.clone();
            async move {
                while session.inner.lock().unwrap().opened.is_empty() {
                    tokio::task::yield_now().await;
                }
                let mut end = server_end(&session, 0);
                let _ = end.from_client.recv().await;
                end.to_client
                    .send(vec![Welcome::Rejected(Reject::Token).encode()])
                    .unwrap();
            }
        });
        match links.rtsp(&rtsp).await {
            Err(Error::Auth(msg)) => assert!(msg.contains("token"), "{msg}"),
            other => panic!("expected an auth error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn control_uses_a_stream_per_channel_and_datagrams_for_unreliable() {
        let (session, datagrams_in) = mock();
        let mut links = TunnelLinks::new(
            session.clone(),
            b"tok".to_vec(),
            Box::new(CountingSink::default()),
        );

        // Server: welcome both control streams as they appear.
        let welcomer = tokio::spawn({
            let session = session.clone();
            async move {
                let mut ends = Vec::new();
                for index in 0..2 {
                    while session.inner.lock().unwrap().opened.len() <= index {
                        tokio::task::yield_now().await;
                    }
                    let mut end = server_end(&session, index);
                    let header = end.from_client.recv().await.unwrap();
                    let expected = if index == 0 {
                        crate::input::channel::GENERIC
                    } else {
                        crate::input::channel::URGENT
                    };
                    assert_eq!(
                        StreamKind::decode(&header).unwrap().0,
                        StreamKind::Control { channel: expected }
                    );
                    end.to_client
                        .send(vec![Welcome::Accepted.encode()])
                        .unwrap();
                    ends.push(end);
                }
                ends
            }
        });
        let mut link = links.control(&negotiated()).await.unwrap();
        let mut ends = welcomer.await.unwrap();
        assert_eq!(link.recv().await, Some(LinkEvent::Connected));

        // Reliable on the urgent channel lands on the urgent stream, framed.
        link.send(&Outgoing {
            frame: b"urgent".to_vec(),
            delivery: Delivery::Reliable,
            channel: crate::input::channel::URGENT,
        })
        .await
        .unwrap();
        assert_eq!(
            ends[1].from_client.recv().await.unwrap(),
            Frame::encode(b"urgent")
        );
        assert!(ends[0].from_client.try_recv().is_err());

        // Unreliable goes out as a tagged datagram.
        link.send(&Outgoing {
            frame: b"motion".to_vec(),
            delivery: Delivery::Unreliable,
            channel: crate::input::channel::GENERIC,
        })
        .await
        .unwrap();
        assert_eq!(
            session.inner.lock().unwrap().sent_datagrams,
            vec![[&[datagram::CONTROL][..], b"motion"].concat()]
        );

        // From the server: a framed message on a stream and an unreliable
        // datagram both arrive as frames.
        ends[0]
            .to_client
            .send(Frame::encode(b"from-stream"))
            .unwrap();
        assert_eq!(
            link.recv().await,
            Some(LinkEvent::Frame(b"from-stream".to_vec()))
        );
        datagrams_in
            .send([&[datagram::CONTROL][..], b"from-datagram"].concat())
            .unwrap();
        assert_eq!(
            link.recv().await,
            Some(LinkEvent::Frame(b"from-datagram".to_vec()))
        );

        // Closing finishes every stream; the server finishing its side is
        // reported once as a disconnect, then the link is done.
        link.close().await;
        assert!(
            ends.iter()
                .all(|e| e.finished.load(std::sync::atomic::Ordering::SeqCst))
        );
        drop(ends);
        drop(datagrams_in);
        assert_eq!(link.recv().await, Some(LinkEvent::Disconnected));
        assert_eq!(link.recv().await, None);
    }

    #[tokio::test]
    async fn media_datagrams_lose_their_tag_and_gain_an_arrival_stamp() {
        let (session, datagrams_in) = mock();
        let mut links = TunnelLinks::new(
            session.clone(),
            b"tok".to_vec(),
            Box::new(CountingSink::default()),
        );
        let mut media = links.media(&negotiated()).await.unwrap();
        datagrams_in
            .send([&[datagram::VIDEO][..], b"video-rtp"].concat())
            .unwrap();
        datagrams_in
            .send([&[datagram::AUDIO][..], b"audio-rtp"].concat())
            .unwrap();
        datagrams_in.send(vec![9, 1, 2]).unwrap(); // unknown tag: dropped
        datagrams_in.send(Vec::new()).unwrap(); // malformed: dropped
        let first = media.recv().await.unwrap();
        assert_eq!(first.bytes, b"video-rtp");
        let second = media.recv().await.unwrap();
        assert_eq!(second.bytes, b"audio-rtp");
        assert!(second.arrival_us >= first.arrival_us);
        drop(datagrams_in);
        assert!(media.recv().await.is_none());
        // The link and audio come once each.
        assert!(links.media(&negotiated()).await.is_err());
        let (_audio, pcm) = links.audio(&negotiated()).unwrap();
        assert!(pcm.try_recv().is_err());
        assert!(links.audio(&negotiated()).is_err());
    }
}
