//! Shared loopback test harness. A [`MockAgent`] stands up a real QUIC endpoint,
//! completes `client-core`'s control handshake (Hello/HelloAck + clock-sync
//! pings), and then relays any [`A2C`] message a test pushes to the connected
//! client. Reuse it for any test that needs the client to *receive*
//! control-stream messages (notifications, session events, …) without a real
//! agent.
//!
//! ```ignore
//! let agent = MockAgent::start().await;
//! let mut client = Client::connect(agent.addr, "t", H264Profile::High, &[Codec::H264], ServerAuth::Open).await?;
//! let mut events = client.take_control_events().unwrap();
//! agent.push(A2C::Notification(Notification::GamepadConnected { seat: 0 }));
//! assert!(matches!(events.recv().await, Some(ControlEvent::GamepadConnected { .. })));
//! ```

use std::net::SocketAddr;
use std::time::Duration;

use gsa_core::media::Codec;
use gsa_protocol::PROTO_VERSION;
use gsa_protocol::control::{A2C, C2A, HelloAck};
use gsa_transport::{Identity, recv_msg, send_msg, server_endpoint};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// A loopback mock agent for control-stream tests. Drop it to let the
/// connection wind down (the client should `close()` first).
pub struct MockAgent {
    /// The address to point `Client::connect` at.
    pub addr: SocketAddr,
    push_tx: mpsc::UnboundedSender<A2C>,
    _task: JoinHandle<()>,
}

impl MockAgent {
    /// Start the agent: it accepts one connection, answers the handshake and
    /// clock-sync, then forwards messages sent via [`MockAgent::push`] to the
    /// client on the control stream.
    pub async fn start() -> Self {
        let identity = Identity::generate().expect("identity");
        let server = server_endpoint("127.0.0.1:0".parse().unwrap(), &identity, None)
            .expect("server endpoint");
        let addr = server.local_addr().expect("local addr");
        let (push_tx, mut push_rx) = mpsc::unbounded_channel::<A2C>();

        let task = tokio::spawn(async move {
            let conn = match server.accept().await {
                Some(incoming) => match incoming.await {
                    Ok(conn) => conn,
                    Err(_) => return,
                },
                None => return,
            };
            let Ok((mut send, mut recv)) = conn.accept_bi().await else {
                return;
            };

            // Hello → HelloAck.
            if recv_msg::<C2A>(&mut recv).await.is_err() {
                return;
            }
            let ack = A2C::HelloAck(HelloAck {
                proto: PROTO_VERSION,
                agent_name: "mock-agent".into(),
                encode_codecs: vec![Codec::H264],
            });
            if send_msg(&mut send, &ack).await.is_err() {
                return;
            }

            // Answer clock-sync pings. A short read timeout after the last one —
            // rather than a hard-coded round count — detects that the client has
            // finished syncing, keeping this decoupled from `connect`'s internals.
            loop {
                match tokio::time::timeout(Duration::from_millis(300), recv_msg::<C2A>(&mut recv))
                    .await
                {
                    Ok(Ok(C2A::Ping { client_ts_us })) => {
                        let pong = A2C::Pong {
                            client_ts_us,
                            agent_ts_us: 0,
                        };
                        if send_msg(&mut send, &pong).await.is_err() {
                            return;
                        }
                    }
                    Ok(Ok(_)) => break, // some other client message; stop syncing
                    Ok(Err(_)) => return, // stream closed
                    Err(_) => break,    // no more pings → sync done
                }
            }

            // Relay pushed messages until the handle is dropped. (Tests that need
            // the agent to also read late client messages can extend this; the
            // clients under test send nothing more on control after sync.)
            while let Some(msg) = push_rx.recv().await {
                if send_msg(&mut send, &msg).await.is_err() {
                    return;
                }
            }
            conn.closed().await;
        });

        Self {
            addr,
            push_tx,
            _task: task,
        }
    }

    /// Push one `A2C` message to the connected client over the control stream.
    pub fn push(&self, msg: A2C) {
        let _ = self.push_tx.send(msg);
    }
}
