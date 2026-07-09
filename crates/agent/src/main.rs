//! `gsa` — the game-streamer-agent binary. `gsa run` is the daemon;
//! every other subcommand is a thin admin-socket client (spec 12).

mod doctor;
mod factories;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use gsa_core::config::{AgentConfig, default_control_socket};
use gsa_protocol::grant::Scope;
use gsa_session::admin::{AdminRequest, AdminResponse, PairingStatusReport};
use gsa_session::{AgentState, PairingState, serve_connection, serve_pairing};

#[derive(Parser, Debug)]
#[command(name = "gsa", version, about = "Game streamer agent")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the agent daemon (foreground).
    Run {
        /// Path to a TOML config file.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Override the QUIC listen address (e.g. 127.0.0.1:0 for ephemeral).
        #[arg(long)]
        listen: Option<std::net::SocketAddr>,
        /// Override the admin socket path / pipe name.
        #[arg(long)]
        control_socket: Option<PathBuf>,
        /// Video bitrate in megabits/sec (overrides config; higher = sharper
        /// text, more bandwidth).
        #[arg(long)]
        bitrate: Option<u32>,
    },
    /// Query a running agent's status.
    Status {
        /// Emit raw JSON (scripting/CI).
        #[arg(long)]
        json: bool,
        /// Admin socket path / pipe name of the target agent.
        #[arg(long)]
        control_socket: Option<PathBuf>,
    },
    /// Check host readiness (capture/injection permissions, backends).
    Doctor {
        /// Emit raw JSON (scripting/CI).
        #[arg(long)]
        json: bool,
    },
    /// List the agent's available capture sources.
    Sources {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        control_socket: Option<PathBuf>,
    },
    /// List the agent's active streaming sessions.
    Sessions {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        control_socket: Option<PathBuf>,
    },
    /// Print the agent's recent log lines.
    Logs {
        /// How many recent lines to show.
        #[arg(long, default_value_t = 200)]
        lines: usize,
        #[arg(long)]
        control_socket: Option<PathBuf>,
    },
    /// Pair a new client: arm a pairing window and print the code to enter
    /// on the client.
    Pair {
        /// Scope to grant the peer.
        #[arg(long, value_enum, default_value_t = ScopeArg::Interact)]
        scope: ScopeArg,
        #[arg(long)]
        control_socket: Option<PathBuf>,
    },
    /// List paired peers.
    Peers {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        control_socket: Option<PathBuf>,
    },
    /// Revoke a paired peer by its pin (see `gsa peers`).
    Revoke {
        /// The peer's pin (cert fingerprint).
        pin: String,
        #[arg(long)]
        control_socket: Option<PathBuf>,
    },
}

/// CLI spelling of [`Scope`].
#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum ScopeArg {
    View,
    Interact,
    Manage,
}

impl From<ScopeArg> for Scope {
    fn from(s: ScopeArg) -> Self {
        match s {
            ScopeArg::View => Scope::View,
            ScopeArg::Interact => Scope::Interact,
            ScopeArg::Manage => Scope::Manage,
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let runtime = tokio::runtime::Runtime::new().context("tokio runtime")?;
    match cli.command {
        Command::Run {
            config,
            listen,
            control_socket,
            bitrate,
        } => {
            let logs = init_tracing();
            runtime.block_on(run(config, listen, control_socket, bitrate, logs))
        }
        Command::Status {
            json,
            control_socket,
        } => runtime.block_on(status(json, control_socket)),
        Command::Doctor { json } => std::process::exit(doctor::run(json)),
        Command::Sources {
            json,
            control_socket,
        } => runtime.block_on(sources_cmd(json, control_socket)),
        Command::Sessions {
            json,
            control_socket,
        } => runtime.block_on(sessions_cmd(json, control_socket)),
        Command::Logs {
            lines,
            control_socket,
        } => runtime.block_on(logs_cmd(lines, control_socket)),
        Command::Pair {
            scope,
            control_socket,
        } => runtime.block_on(pair_cmd(scope.into(), control_socket)),
        Command::Peers {
            json,
            control_socket,
        } => runtime.block_on(peers_cmd(json, control_socket)),
        Command::Revoke {
            pin,
            control_socket,
        } => runtime.block_on(revoke_cmd(pin, control_socket)),
    }
}

/// Best-effort primary LAN IPv4: opens a UDP socket and asks the OS which
/// local address it would route from (no packets sent). `None` with no
/// default route.
fn primary_lan_ip() -> Option<std::net::IpAddr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    sock.local_addr().ok().map(|a| a.ip())
}

/// A tracing writer that appends each formatted (plain-text) line to the shared
/// [`LogBuffer`], so `gsa logs` can return recent output.
struct RingWriter(gsa_session::admin::LogBuffer);

impl std::io::Write for RingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(s) = std::str::from_utf8(buf) {
            let line = s.trim_end();
            if !line.is_empty() {
                self.0.push(line.to_string());
            }
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for RingWriter {
    type Writer = RingWriter;
    fn make_writer(&'a self) -> RingWriter {
        RingWriter(self.0.clone())
    }
}

/// Set up tracing to stderr plus an in-memory ring (for `gsa logs`); returns
/// the shared buffer.
fn init_tracing() -> gsa_session::admin::LogBuffer {
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{EnvFilter, fmt};

    let logs = gsa_session::admin::LogBuffer::default();
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(
            fmt::layer()
                .with_ansi(false)
                .with_writer(RingWriter(logs.clone())),
        )
        .init();
    logs
}

fn load_config(
    path: Option<PathBuf>,
    listen: Option<std::net::SocketAddr>,
    control_socket: Option<PathBuf>,
    bitrate_mbps: Option<u32>,
) -> Result<AgentConfig> {
    let mut cfg = match path {
        Some(p) => {
            let text = std::fs::read_to_string(&p)
                .with_context(|| format!("read config {}", p.display()))?;
            toml::from_str(&text).with_context(|| format!("parse config {}", p.display()))?
        }
        None => AgentConfig::default(),
    };
    if let Some(listen) = listen {
        cfg.listen = listen;
    }
    if let Some(sock) = control_socket {
        cfg.control_socket = Some(sock);
    }
    if let Some(mbps) = bitrate_mbps {
        cfg.video.bitrate_bps = mbps.saturating_mul(1_000_000);
    }
    Ok(cfg)
}

async fn run(
    config: Option<PathBuf>,
    listen: Option<std::net::SocketAddr>,
    control_socket: Option<PathBuf>,
    bitrate_mbps: Option<u32>,
    logs: gsa_session::admin::LogBuffer,
) -> Result<()> {
    let cfg = load_config(config, listen, control_socket, bitrate_mbps)?;

    let data_dir = gsa_core::config::data_dir();
    let identity = gsa_transport::Identity::load_or_generate(&data_dir).context("load identity")?;
    let peers =
        Arc::new(gsa_transport::PeerStore::load_or_empty(&data_dir).context("load peer store")?);
    let pairing = Arc::new(PairingState::new());

    // Dev-open mode (e2e/CI) disables pinned mutual TLS: all clients connect
    // anonymously and stream without pairing. Off by default — the secure
    // path requires a paired, pinned client.
    let open = std::env::var_os("GSA_DEV_OPEN").is_some();
    if open {
        tracing::warn!("GSA_DEV_OPEN set — pairing disabled, accepting any client (dev only)");
    }
    let endpoint =
        gsa_transport::server_endpoint(cfg.listen, &identity, (!open).then(|| peers.clone()))
            .context("bind QUIC")?;
    let local_addr = endpoint.local_addr().context("local addr")?;

    let socket_path = cfg
        .control_socket
        .clone()
        .unwrap_or_else(default_control_socket);
    let state = Arc::new(AgentState::new(
        AgentConfig {
            listen: local_addr,
            ..cfg
        },
        identity.fingerprint(),
    ));

    tracing::info!(
        listen = %local_addr,
        fingerprint = state.fingerprint,
        version = env!("CARGO_PKG_VERSION"),
        "gsa agent running"
    );

    // When bound to all interfaces, surface a concrete LAN address so a
    // client on another machine knows where to connect.
    if local_addr.ip().is_unspecified() {
        match primary_lan_ip() {
            Some(ip) => tracing::info!(
                "reachable on this LAN at {ip}:{} (e.g. `gsa-client-dev --connect {ip}:{}`)",
                local_addr.port(),
                local_addr.port()
            ),
            None => tracing::info!("bound to all interfaces on port {}", local_addr.port()),
        }
    } else if local_addr.ip().is_loopback() {
        tracing::info!("listening on loopback only — pass `--listen 0.0.0.0:PORT` for LAN access");
    }

    let sources = Arc::new(factories::Sources::new(state.clock.clone()));
    let encoders = Arc::new(factories::Encoders::new(state.clock.clone()));

    let admin_state = state.clone();
    let admin_sources = sources.clone();
    let admin_peers = peers.clone();
    let admin_pairing = pairing.clone();
    let admin_socket = socket_path.clone();
    tokio::spawn(async move {
        if let Err(e) = gsa_session::admin::serve(
            admin_state,
            admin_sources,
            logs,
            admin_peers,
            admin_pairing,
            &admin_socket,
        )
        .await
        {
            tracing::error!(error = %e, "admin socket failed");
        }
    });

    loop {
        tokio::select! {
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                let state = state.clone();
                let sources = sources.clone();
                let encoders = encoders.clone();
                let peers = peers.clone();
                let pairing = pairing.clone();
                tokio::spawn(async move {
                    match incoming.await {
                        Ok(conn) => {
                            // A pinned peer (cert in the store) streams at its
                            // granted scope; an anonymous connection pairs.
                            let scope = gsa_transport::peer_pin(&conn)
                                .and_then(|pin| peers.get(&pin))
                                .map(|p| p.scope);
                            if open {
                                serve_connection(conn, state, sources, encoders, Scope::Interact).await;
                            } else if let Some(scope) = scope {
                                serve_connection(conn, state, sources, encoders, scope).await;
                            } else {
                                serve_pairing(conn, state, peers, pairing).await;
                            }
                        }
                        Err(e) => tracing::warn!(error = %e, "handshake failed"),
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutting down");
                break;
            }
        }
    }
    endpoint.close(0u32.into(), b"agent shutdown");
    Ok(())
}

/// `gsa pair`: arm a pairing window and print the code, then poll until a
/// client completes it (or the window expires).
async fn pair_cmd(scope: Scope, control_socket: Option<PathBuf>) -> Result<()> {
    let socket = control_socket.unwrap_or_else(default_control_socket);
    let code =
        match gsa_session::admin::request(&socket, &AdminRequest::BeginPairing { scope }).await? {
            AdminResponse::Pairing { code } => code,
            AdminResponse::Error { message } => anyhow::bail!("agent error: {message}"),
            other => anyhow::bail!("unexpected reply: {other:?}"),
        };
    println!("Pairing code: {code}   (grant: {scope:?})");
    println!("On the client:  gsa-client-dev --connect <agent-addr> --pair --code {code}");
    println!("Waiting for the client...");

    loop {
        tokio::time::sleep(Duration::from_millis(1000)).await;
        match gsa_session::admin::request(&socket, &AdminRequest::PairingStatus).await? {
            AdminResponse::PairingStatus(PairingStatusReport::Completed { name, pin, scope }) => {
                println!("✓ paired with \"{name}\" (scope {scope:?})");
                println!("  pin {pin}");
                return Ok(());
            }
            AdminResponse::PairingStatus(PairingStatusReport::Expired) => {
                anyhow::bail!("pairing window expired with no client");
            }
            // Idle/Waiting: keep polling.
            AdminResponse::PairingStatus(_) => {}
            AdminResponse::Error { message } => anyhow::bail!("agent error: {message}"),
            other => anyhow::bail!("unexpected reply: {other:?}"),
        }
    }
}

async fn peers_cmd(json: bool, control_socket: Option<PathBuf>) -> Result<()> {
    let socket = control_socket.unwrap_or_else(default_control_socket);
    match gsa_session::admin::request(&socket, &AdminRequest::Peers).await? {
        AdminResponse::Peers { peers } => {
            if json {
                println!("{}", serde_json::to_string_pretty(&peers)?);
            } else if peers.is_empty() {
                println!("no paired peers");
            } else {
                for p in &peers {
                    println!("  {}  scope={:?}  pin={}", p.name, p.scope, p.pin);
                }
            }
            Ok(())
        }
        AdminResponse::Error { message } => anyhow::bail!("agent error: {message}"),
        other => anyhow::bail!("unexpected reply: {other:?}"),
    }
}

async fn revoke_cmd(pin: String, control_socket: Option<PathBuf>) -> Result<()> {
    let socket = control_socket.unwrap_or_else(default_control_socket);
    match gsa_session::admin::request(&socket, &AdminRequest::Revoke { pin: pin.clone() }).await? {
        AdminResponse::Revoke { removed } => {
            if removed {
                println!("revoked {pin}");
            } else {
                println!("no peer with pin {pin}");
            }
            Ok(())
        }
        AdminResponse::Error { message } => anyhow::bail!("agent error: {message}"),
        other => anyhow::bail!("unexpected reply: {other:?}"),
    }
}

async fn status(json: bool, control_socket: Option<PathBuf>) -> Result<()> {
    let socket = control_socket.unwrap_or_else(default_control_socket);
    let reply = gsa_session::admin::request(&socket, &AdminRequest::Status).await?;
    match reply {
        AdminResponse::Status(s) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&s)?);
            } else {
                println!("gsa agent v{} — up {}s", s.agent_version, s.uptime_s);
                println!("  listening:   {}", s.listen);
                println!("  fingerprint: {}", s.fingerprint);
                if s.sessions.is_empty() {
                    println!("  sessions:    none");
                } else {
                    for sess in &s.sessions {
                        println!(
                            "  session {}: {} {}x{}@{} — {} frames sent",
                            sess.id, sess.peer, sess.width, sess.height, sess.fps, sess.frames_sent
                        );
                    }
                }
            }
            Ok(())
        }
        AdminResponse::Error { message } => anyhow::bail!("agent error: {message}"),
        other => anyhow::bail!("unexpected reply: {other:?}"),
    }
}

async fn sources_cmd(json: bool, control_socket: Option<PathBuf>) -> Result<()> {
    let socket = control_socket.unwrap_or_else(default_control_socket);
    match gsa_session::admin::request(&socket, &AdminRequest::Sources).await? {
        AdminResponse::Sources { sources } => {
            if json {
                println!("{}", serde_json::to_string_pretty(&sources)?);
            } else if sources.is_empty() {
                println!("no sources");
            } else {
                for s in &sources {
                    println!("  {} [{:?}] {}", s.id.0, s.kind, s.name);
                }
            }
            Ok(())
        }
        AdminResponse::Error { message } => anyhow::bail!("agent error: {message}"),
        other => anyhow::bail!("unexpected reply: {other:?}"),
    }
}

async fn logs_cmd(lines: usize, control_socket: Option<PathBuf>) -> Result<()> {
    let socket = control_socket.unwrap_or_else(default_control_socket);
    match gsa_session::admin::request(&socket, &AdminRequest::Logs { lines }).await? {
        AdminResponse::Logs { lines } => {
            for line in &lines {
                println!("{line}");
            }
            Ok(())
        }
        AdminResponse::Error { message } => anyhow::bail!("agent error: {message}"),
        other => anyhow::bail!("unexpected reply: {other:?}"),
    }
}

async fn sessions_cmd(json: bool, control_socket: Option<PathBuf>) -> Result<()> {
    let socket = control_socket.unwrap_or_else(default_control_socket);
    match gsa_session::admin::request(&socket, &AdminRequest::Sessions).await? {
        AdminResponse::Sessions { sessions } => {
            if json {
                println!("{}", serde_json::to_string_pretty(&sessions)?);
            } else if sessions.is_empty() {
                println!("no active sessions");
            } else {
                for s in &sessions {
                    println!(
                        "  session {}: {} {}x{}@{} — {} frames sent",
                        s.id, s.peer, s.width, s.height, s.fps, s.frames_sent
                    );
                }
            }
            Ok(())
        }
        AdminResponse::Error { message } => anyhow::bail!("agent error: {message}"),
        other => anyhow::bail!("unexpected reply: {other:?}"),
    }
}
