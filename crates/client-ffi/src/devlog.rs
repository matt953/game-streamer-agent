//! Debug-build remote logging: mirrors the client's tracing output to the dev
//! log collector the agent advertises in `SessionParams.log_sink`, so a live
//! session can be watched from both ends without pulling logs off the device.
//!
//! Everything here is compiled but inert in release builds
//! (`cfg!(debug_assertions)` gates both the subscriber and the sink), matching
//! the agent's `GSA_LOG_SINK` being a dev-only switch.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use gsa_transport::logsink::LogSink;

static SINK: OnceLock<LogSink> = OnceLock::new();
/// Lines logged before the sink address is known (connect/negotiate — often
/// the interesting part); flushed on activation, oldest dropped past the cap.
static PREBUFFER: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());
const PREBUFFER_CAP: usize = 512;

/// Install the tracing subscriber (debug builds; once). `try_init` so an
/// embedder that already installed its own subscriber wins quietly.
pub fn init() {
    use tracing_subscriber::Layer as _;
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;

    if !cfg!(debug_assertions) {
        return;
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let filter = tracing_subscriber::EnvFilter::new(
            "info,gsa_client_ffi=debug,gsa_client_core=debug,gsa_transport=debug",
        );
        let _ = tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(DevLogWriter)
                    .with_filter(filter),
            )
            .try_init();
        // Desktop debug runs can point at a collector before any session.
        if let Ok(addr) = std::env::var("GSA_LOG_SINK") {
            activate(&addr);
        }
    });
}

/// Start pushing to the collector at `addr` (from the agent's advertisement).
/// Debug builds only; first address wins, later calls are no-ops.
pub fn activate(addr: &str) {
    if !cfg!(debug_assertions) {
        return;
    }
    let mut fresh = false;
    let sink = SINK.get_or_init(|| {
        fresh = true;
        let hello = format!(
            "hello role=client os={} pid={}",
            std::env::consts::OS,
            std::process::id()
        );
        LogSink::spawn(addr.to_string(), hello)
    });
    if fresh && let Ok(mut buf) = PREBUFFER.lock() {
        for line in buf.drain(..) {
            sink.push(line);
        }
    }
}

/// Tracing writer: forwards to the live sink, or to the pre-activation buffer.
struct DevLogWriter;

impl std::io::Write for DevLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(s) = std::str::from_utf8(buf) {
            let line = s.trim_end();
            if !line.is_empty() {
                if let Some(sink) = SINK.get() {
                    sink.push(line.to_string());
                } else if let Ok(mut pre) = PREBUFFER.lock() {
                    if pre.len() >= PREBUFFER_CAP {
                        pre.pop_front();
                    }
                    pre.push_back(line.to_string());
                }
            }
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for DevLogWriter {
    type Writer = DevLogWriter;
    fn make_writer(&'a self) -> DevLogWriter {
        DevLogWriter
    }
}
