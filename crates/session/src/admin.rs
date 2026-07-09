//! Local admin API — the `ControlService` (spec 12): one implementation,
//! consumed by the CLI now, the tray UI and media server later.
//!
//! Transport at M0: newline-delimited JSON over a Unix domain socket
//! (macOS/Linux) or named pipe (Windows). Caller identity = OS user (the
//! socket is owner-permissioned); local socket ⇒ implicit admin scope.

use std::path::Path;
use std::sync::Arc;

use gsa_core::{Error, Result};
use gsa_protocol::control::SourceInfo;
use serde::{Deserialize, Serialize};

use crate::service::SourceFactory;
use crate::state::{AgentState, SessionSummary};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum AdminRequest {
    Status,
    Sources,
    Sessions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum AdminResponse {
    Status(StatusReport),
    Sources { sources: Vec<SourceInfo> },
    Sessions { sessions: Vec<SessionSummary> },
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusReport {
    pub agent_version: String,
    pub uptime_s: u64,
    pub listen: String,
    pub fingerprint: String,
    pub sessions: Vec<SessionSummary>,
}

fn handle_request(
    state: &AgentState,
    sources: &dyn SourceFactory,
    req: &AdminRequest,
) -> AdminResponse {
    match req {
        AdminRequest::Status => AdminResponse::Status(StatusReport {
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_s: state.started.elapsed().as_secs(),
            listen: state.config.listen.to_string(),
            fingerprint: state.fingerprint.clone(),
            sessions: state.session_summaries(),
        }),
        AdminRequest::Sources => AdminResponse::Sources {
            sources: sources.list().into_iter().map(|d| d.info).collect(),
        },
        AdminRequest::Sessions => AdminResponse::Sessions {
            sessions: state.session_summaries(),
        },
    }
}

fn process_line(state: &AgentState, sources: &dyn SourceFactory, line: &str) -> String {
    let response = match serde_json::from_str::<AdminRequest>(line) {
        Ok(req) => handle_request(state, sources, &req),
        Err(e) => AdminResponse::Error {
            message: format!("bad request: {e}"),
        },
    };
    serde_json::to_string(&response)
        .unwrap_or_else(|e| format!(r#"{{"reply":"error","message":"serialize failed: {e}"}}"#))
}

/// Serve the admin socket forever. Call in a spawned task.
#[cfg(unix)]
pub async fn serve(
    state: Arc<AgentState>,
    sources: Arc<dyn SourceFactory>,
    socket_path: &Path,
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    // Stale socket from a previous run.
    let _ = std::fs::remove_file(socket_path);
    let listener = tokio::net::UnixListener::bind(socket_path)
        .map_err(|e| Error::Session(format!("bind admin socket {}: {e}", socket_path.display())))?;
    // Owner-only: the OS user is the authenticator (spec 12).
    let perms = <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600);
    std::fs::set_permissions(socket_path, perms)?;
    tracing::info!(socket = %socket_path.display(), "admin socket listening");

    loop {
        let (stream, _) = listener
            .accept()
            .await
            .map_err(|e| Error::Session(format!("admin accept: {e}")))?;
        let state = state.clone();
        let sources = sources.clone();
        tokio::spawn(async move {
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let reply = process_line(&state, sources.as_ref(), &line);
                if write.write_all(reply.as_bytes()).await.is_err()
                    || write.write_all(b"\n").await.is_err()
                {
                    break;
                }
            }
        });
    }
}

/// One-shot admin client used by the CLI.
#[cfg(unix)]
pub async fn request(socket_path: &Path, req: &AdminRequest) -> Result<AdminResponse> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let stream = tokio::net::UnixStream::connect(socket_path)
        .await
        .map_err(|e| {
            Error::Session(format!(
                "agent not reachable at {} (is `gsa run` running?): {e}",
                socket_path.display()
            ))
        })?;
    let (read, mut write) = stream.into_split();
    let line = serde_json::to_string(req).map_err(|e| Error::Session(e.to_string()))?;
    write.write_all(line.as_bytes()).await?;
    write.write_all(b"\n").await?;
    let mut lines = BufReader::new(read).lines();
    let reply = lines
        .next_line()
        .await?
        .ok_or_else(|| Error::Session("agent closed admin connection".into()))?;
    serde_json::from_str(&reply).map_err(|e| Error::Session(format!("bad admin reply: {e}")))
}

/// Serve the admin named pipe forever. Call in a spawned task.
#[cfg(windows)]
pub async fn serve(
    state: Arc<AgentState>,
    sources: Arc<dyn SourceFactory>,
    socket_path: &Path,
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::windows::named_pipe::ServerOptions;

    let name = socket_path.to_string_lossy().to_string();
    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&name)
        .map_err(|e| Error::Session(format!("create admin pipe {name}: {e}")))?;
    tracing::info!(pipe = name, "admin pipe listening");

    loop {
        server
            .connect()
            .await
            .map_err(|e| Error::Session(format!("admin pipe connect: {e}")))?;
        let connected = server;
        server = ServerOptions::new()
            .create(&name)
            .map_err(|e| Error::Session(format!("recreate admin pipe: {e}")))?;

        let state = state.clone();
        let sources = sources.clone();
        tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(connected);
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let reply = process_line(&state, sources.as_ref(), &line);
                if write.write_all(reply.as_bytes()).await.is_err()
                    || write.write_all(b"\n").await.is_err()
                {
                    break;
                }
            }
        });
    }
}

/// One-shot admin client used by the CLI.
#[cfg(windows)]
pub async fn request(socket_path: &Path, req: &AdminRequest) -> Result<AdminResponse> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::windows::named_pipe::ClientOptions;

    let name = socket_path.to_string_lossy().to_string();
    let pipe = ClientOptions::new().open(&name).map_err(|e| {
        Error::Session(format!(
            "agent not reachable at {name} (is `gsa run` running?): {e}"
        ))
    })?;
    let (read, mut write) = tokio::io::split(pipe);
    let line = serde_json::to_string(req).map_err(|e| Error::Session(e.to_string()))?;
    write.write_all(line.as_bytes()).await?;
    write.write_all(b"\n").await?;
    let mut lines = BufReader::new(read).lines();
    let reply = lines
        .next_line()
        .await?
        .ok_or_else(|| Error::Session("agent closed admin connection".into()))?;
    serde_json::from_str(&reply).map_err(|e| Error::Session(format!("bad admin reply: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gsa_capture_api::{RenderSource, SourceDescriptor};
    use gsa_core::config::AgentConfig;
    use gsa_core::id::SourceId;

    struct NoSources;
    impl SourceFactory for NoSources {
        fn list(&self) -> Vec<SourceDescriptor> {
            Vec::new()
        }
        fn create(&self, _id: SourceId) -> Result<Box<dyn RenderSource>> {
            Err(Error::Session("no sources".into()))
        }
    }

    #[test]
    fn status_request_round_trips_as_json() {
        let state = AgentState::new(AgentConfig::default(), "ab".repeat(32));
        let reply = process_line(&state, &NoSources, r#"{"cmd":"status"}"#);
        let parsed: AdminResponse = serde_json::from_str(&reply).unwrap();
        match parsed {
            AdminResponse::Status(s) => {
                assert_eq!(s.listen, "127.0.0.1:47420");
                assert!(s.sessions.is_empty());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn sources_and_sessions_reply() {
        let state = AgentState::new(AgentConfig::default(), String::new());
        assert!(matches!(
            serde_json::from_str(&process_line(&state, &NoSources, r#"{"cmd":"sources"}"#)).unwrap(),
            AdminResponse::Sources { sources } if sources.is_empty()
        ));
        assert!(matches!(
            serde_json::from_str(&process_line(&state, &NoSources, r#"{"cmd":"sessions"}"#))
                .unwrap(),
            AdminResponse::Sessions { sessions } if sessions.is_empty()
        ));
    }

    #[test]
    fn bad_request_yields_error_reply() {
        let state = AgentState::new(AgentConfig::default(), String::new());
        let reply = process_line(&state, &NoSources, "not json");
        assert!(reply.contains(r#""reply":"error""#));
    }
}
