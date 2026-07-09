//! `gsa` — the game-streamer-agent binary. `gsa run` is the daemon;
//! every other subcommand is a thin admin-socket client (spec 12).

mod doctor;
mod factories;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use gsa_core::config::{AgentConfig, default_control_socket};
use gsa_session::admin::{AdminRequest, AdminResponse};
use gsa_session::{AgentState, serve_connection};

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
            init_tracing();
            runtime.block_on(run(config, listen, control_socket, bitrate))
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

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
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
) -> Result<()> {
    let cfg = load_config(config, listen, control_socket, bitrate_mbps)?;

    let identity = gsa_transport::Identity::generate().context("generate identity")?;
    let endpoint = gsa_transport::server_endpoint(cfg.listen, &identity).context("bind QUIC")?;
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
    let admin_socket = socket_path.clone();
    tokio::spawn(async move {
        if let Err(e) = gsa_session::admin::serve(admin_state, admin_sources, &admin_socket).await {
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
                tokio::spawn(async move {
                    match incoming.await {
                        Ok(conn) => serve_connection(conn, state, sources, encoders).await,
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
