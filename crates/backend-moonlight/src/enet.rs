//! Driving the control channel's ENet connection.
//!
//! Hosts run a fork of ENet. Its wire structures are byte-identical to
//! upstream and its other changes are local behaviour — timers, IPv6
//! addressing, a lower default MTU (negotiated down to whatever the host
//! proposes; do not assume 1392).
//!
//! One fork change is *not* local, and a stock ENet does not interoperate
//! without matching it: the fork drops upstream's check that a datagram's
//! source address equals the peer's, identifying the peer by the header's ids
//! instead. See [`PeerAddr`] for why that matters and what it costs.
//!
//! ENet is poll-driven rather than async, so this runs on its own thread and
//! talks to the rest of the client over channels. Only transport lives here:
//! the protocol on top is [`ControlSession`](crate::control_session), which
//! this link drives through [`ControlLink`].

use crate::control_session::{ControlLink, Delivery, LinkEvent, Outgoing};
use gsa_core::{Error, Result};
use rusty_enet as enet;

/// Poll interval. ENet needs servicing regularly to retransmit and to
/// surface received packets.
/// The longest the control loop waits with nothing arriving. Arrivals wake it
/// immediately (see the socket peek below); this only bounds how stale queued
/// *outgoing* work can get.
const SERVICE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(4);
/// How often the measured link round-trip is republished to the embedder.
const RTT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Frames sent per service tick before ENet is serviced again. High enough
/// that ordinary input never queues, bounded so a runaway producer cannot
/// starve the connection's own upkeep (acks, pings, retransmits).
const MAX_SENDS_PER_TICK: usize = 64;

/// Channel count to request. Reference clients open this many and place some
/// controller streams on the higher channels.
const CHANNELS: usize = 48;

/// The host's address, compared the way this protocol needs rather than the
/// way ENet compares addresses by default.
///
/// **A datagram is matched to the peer by the ENet header's peer and session
/// ids, not by where it came from.** Upstream ENet also requires the source
/// address to equal the one dialled; reference clients bundle a fork with that
/// comparison commented out, so a host answering from a *different* local
/// address than the one it was reached on is ordinary here. A machine with two
/// interfaces on one subnet, a NAT that remaps, a VPN — the reply arrives from
/// an address the client never dialled, upstream ENet discards it, and the
/// channel simply never connects while video and audio flow normally. That
/// failure is silent and looks like the host ignoring us.
///
/// Accepting any source costs little: the peer and session ids must still
/// match, the payloads are authenticated by the session key, and the host is
/// free to move — ENet re-points the peer at whatever address answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PeerAddr(std::net::SocketAddr);

impl enet::Address for PeerAddr {
    fn same_host(&self, _other: &Self) -> bool {
        true
    }

    fn same(&self, _other: &Self) -> bool {
        true
    }

    fn is_broadcast(&self) -> bool {
        false
    }
}

/// The control channel's socket, wrapping the standard one so the address type
/// above is the one ENet compares. Behaviour is otherwise the crate's own.
struct ControlSocket {
    socket: std::net::UdpSocket,
    /// Where the host was dialled, so an answer from anywhere else is worth
    /// one line: it is legal, and it is also the first thing to know when a
    /// host behaves unexpectedly.
    dialled: std::net::SocketAddr,
    reported_elsewhere: bool,
}

impl enet::Socket for ControlSocket {
    type Address = PeerAddr;
    type Error = std::io::Error;

    fn init(&mut self, _options: enet::SocketOptions) -> std::io::Result<()> {
        self.socket.set_nonblocking(true)?;
        self.socket.set_broadcast(true)?;
        Ok(())
    }

    fn send(&mut self, address: PeerAddr, buffer: &[u8]) -> std::io::Result<usize> {
        match self.socket.send_to(buffer, address.0) {
            Ok(sent) => Ok(sent),
            // A full send buffer is back-pressure, not a failure: ENet
            // retransmits what it must.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(0),
            Err(e) => Err(e),
        }
    }

    fn receive(
        &mut self,
        buffer: &mut [u8; enet::MTU_MAX],
    ) -> std::io::Result<Option<(PeerAddr, enet::PacketReceived)>> {
        match self.socket.recv_from(buffer) {
            Ok((len, from)) => {
                if from != self.dialled && !self.reported_elsewhere {
                    self.reported_elsewhere = true;
                    tracing::info!(
                        dialled = %self.dialled,
                        answered = %from,
                        "host answers the control channel from another address"
                    );
                }
                Ok(Some((PeerAddr(from), enet::PacketReceived::Complete(len))))
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// What the driver hands the ENet thread.
enum ToWire {
    Frame(Outgoing),
    Stop,
}

/// The control channel over ENet, as the transport-neutral session sees it.
///
/// Connection, retransmission, ordering and the round-trip measurement are
/// ENet's; the thread it runs on is owned here and stops with the link.
#[derive(Debug)]
pub struct EnetLink {
    to_wire: std::sync::mpsc::Sender<ToWire>,
    from_wire: tokio::sync::mpsc::UnboundedReceiver<LinkEvent>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for ToWire {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Frame(out) => f.debug_tuple("Frame").field(&out.frame.len()).finish(),
            Self::Stop => f.write_str("Stop"),
        }
    }
}

impl EnetLink {
    /// Dial the host's control port. The connect data binds this ENet peer to
    /// the RTSP session; the host does not identify the peer by source
    /// address. Connection completes on the thread and is reported as the
    /// first [`LinkEvent::Connected`].
    pub fn connect(addr: std::net::SocketAddr, connect_data: u32) -> Result<Self> {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0")
            .map_err(|e| Error::Transport(format!("bind control socket: {e}")))?;
        // A second handle onto the same socket, used only to *wait* for
        // readability between service passes. Sleeping a fixed interval
        // instead leaves every acknowledgement sitting unread for up to that
        // interval — which inflates the measured round trip by the polling
        // loop rather than the wire, and delays every host message the same
        // way. Peeking does not consume: the ENet host still reads the
        // datagram itself.
        let waker = socket.try_clone().ok();
        if let Some(w) = &waker {
            let _ = w.set_read_timeout(Some(SERVICE_INTERVAL));
        }
        // `Host::new` initialises the socket (non-blocking, broadcast) itself.
        let mut host = enet::Host::new(
            ControlSocket {
                socket,
                dialled: addr,
                reported_elsewhere: false,
            },
            enet::HostSettings {
                peer_limit: 1,
                channel_limit: CHANNELS,
                ..enet::HostSettings::default()
            },
        )
        .map_err(|e| Error::Transport(format!("create control host: {e}")))?;
        host.connect(PeerAddr(addr), CHANNELS, connect_data)
            .map_err(|_| Error::Transport("no ENet peer slot".into()))?;

        let (to_wire, wire_rx) = std::sync::mpsc::channel();
        let (wire_tx, from_wire) = tokio::sync::mpsc::unbounded_channel();
        let thread = std::thread::Builder::new()
            .name("moonlight-control".into())
            .spawn(move || {
                if let Err(e) = service(host, waker, &wire_rx, &wire_tx) {
                    tracing::warn!(error = %e, "control link ended");
                }
                let _ = wire_tx.send(LinkEvent::Disconnected);
            })
            .map_err(|e| Error::Transport(format!("spawn control thread: {e}")))?;
        Ok(Self {
            to_wire,
            from_wire,
            thread: Some(thread),
        })
    }
}

impl ControlLink for EnetLink {
    async fn send(&mut self, out: &Outgoing) -> Result<()> {
        self.to_wire
            .send(ToWire::Frame(out.clone()))
            .map_err(|_| Error::Transport("control link is closed".into()))
    }

    async fn recv(&mut self) -> Option<LinkEvent> {
        self.from_wire.recv().await
    }

    async fn close(&mut self) {
        let _ = self.to_wire.send(ToWire::Stop);
        if let Some(thread) = self.thread.take() {
            // The thread notices the stop within one service interval.
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
    }
}

impl Drop for EnetLink {
    fn drop(&mut self) {
        let _ = self.to_wire.send(ToWire::Stop);
    }
}

/// The thread: service ENet, forward what arrives, send what is queued.
fn service(
    mut host: enet::Host<ControlSocket>,
    waker: Option<std::net::UdpSocket>,
    to_wire: &std::sync::mpsc::Receiver<ToWire>,
    events: &tokio::sync::mpsc::UnboundedSender<LinkEvent>,
) -> Result<()> {
    let mut connected = false;
    let mut last_rtt = std::time::Instant::now();
    loop {
        while let Some(event) = host
            .service()
            .map_err(|e| Error::Transport(format!("control service: {e}")))?
        {
            match event {
                enet::Event::Connect { .. } => {
                    // ENet grants the lesser of what we ask for and what the
                    // host allows, and a send above that count fails locally
                    // rather than on the wire — so the granted count decides
                    // whether input can leave at all.
                    let granted = host
                        .connected_peers()
                        .map(rusty_enet::Peer::channel_count)
                        .max()
                        .unwrap_or(0);
                    tracing::info!(requested = CHANNELS, granted, "control channel connected");
                    connected = true;
                    let _ = events.send(LinkEvent::Connected);
                }
                enet::Event::Disconnect { .. } => {
                    return Ok(());
                }
                enet::Event::Receive { packet, .. } => {
                    if events
                        .send(LinkEvent::Frame(packet.data().to_vec()))
                        .is_err()
                    {
                        return Ok(());
                    }
                }
            }
        }

        // Drain the queue rather than taking one frame per service tick: at
        // one per `SERVICE_INTERVAL` the channel tops out around 250 messages
        // a second, and motion alone can exceed that — the backlog would show
        // as input lag that grows for as long as the user keeps moving.
        for _ in 0..MAX_SENDS_PER_TICK {
            match to_wire.try_recv() {
                Ok(ToWire::Stop) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    for peer in host.connected_peers_mut() {
                        peer.disconnect(0);
                    }
                    let _ = host.service();
                    return Ok(());
                }
                Ok(ToWire::Frame(out)) => send(&mut host, &out),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
            }
        }

        // Publish the link's round trip about once a second. ENet keeps its
        // own smoothed estimate from acknowledgements; reading it costs
        // nothing and the receiver treats it as telemetry, not control.
        if connected && last_rtt.elapsed() >= RTT_INTERVAL {
            last_rtt = std::time::Instant::now();
            if let Some(peer) = host.connected_peers_mut().next() {
                #[allow(clippy::cast_possible_truncation)]
                let rtt_us = peer.round_trip_time().as_micros().min(u128::from(u32::MAX)) as u32;
                let _ = events.send(LinkEvent::Rtt { rtt_us });
            }
        }

        match &waker {
            // Wake the moment a datagram lands (or after the interval, for
            // outgoing work), instead of letting it wait out a sleep.
            Some(w) => {
                let mut probe = [0u8; 1];
                let _ = w.peek(&mut probe);
            }
            None => std::thread::sleep(SERVICE_INTERVAL),
        }
    }
}

fn send(host: &mut enet::Host<ControlSocket>, out: &Outgoing) {
    let packet = match out.delivery {
        Delivery::Reliable => enet::Packet::reliable(out.frame.as_slice()),
        Delivery::Unreliable => enet::Packet::unreliable(out.frame.as_slice()),
    };
    let mut sent = false;
    for peer in host.connected_peers_mut() {
        match peer.send(out.channel, &packet) {
            Ok(()) => sent = true,
            // Logged, not dropped: a dead control channel is otherwise
            // indistinguishable from an idle one.
            Err(e) => tracing::warn!(error = ?e, "control send failed"),
        }
    }
    if !sent {
        tracing::warn!("control message dropped: no connected peer");
    }
}

/// Run the control channel over ENet on the calling thread until it ends.
///
/// The blocking form for tools and examples that own a thread rather than a
/// runtime: connects, drives [`ControlSession`](crate::ControlSession) on a
/// runtime local to this thread, and returns when the host or the caller
/// ends the session.
pub fn run_control(
    addr: std::net::SocketAddr,
    connect_data: u32,
    modern_start: bool,
    crypto: crate::Crypto,
    commands: tokio::sync::mpsc::UnboundedReceiver<crate::Command>,
    events: std::sync::mpsc::Sender<crate::HostMessage>,
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Transport(format!("control runtime: {e}")))?;
    let link = EnetLink::connect(addr, connect_data)?;
    let session = crate::ControlSession::new(crypto, modern_start);
    runtime.block_on(crate::drive(link, session, commands, events))
}
