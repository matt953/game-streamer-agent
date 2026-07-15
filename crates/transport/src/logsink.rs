//! Dev-only remote log push: mirrors formatted log lines over TCP to a
//! collector (`cargo xtask logs`) so both ends of a live session can be
//! watched without ferrying files between machines.
//!
//! Activation is opt-in per process (the agent gates on `GSA_LOG_SINK`,
//! clients on the sink address the agent advertises — debug builds only).
//! The sender must never affect the pipeline: lines go through a bounded
//! queue into a background thread that owns the socket; a full queue drops
//! the line, a dead connection reconnects with backoff.

use std::io::Write as _;
use std::sync::mpsc;
use std::time::Duration;

/// Lines buffered while the collector is slow or unreachable. A few seconds
/// of chatty debug output; beyond that, drop (freshness beats completeness).
const QUEUE_CAP: usize = 4096;
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Handle to a running log-push thread. Cheap to clone; each formatted line
/// is one [`push`](Self::push) call.
#[derive(Debug, Clone)]
pub struct LogSink {
    tx: mpsc::SyncSender<String>,
}

impl LogSink {
    /// Spawn the push thread targeting `addr` (`host:port`). `hello` is sent
    /// as the first line of every (re)connection so the collector can label
    /// the stream (e.g. `hello role=agent host=gamer-pc`).
    #[must_use]
    pub fn spawn(addr: String, hello: String) -> Self {
        let (tx, rx) = mpsc::sync_channel::<String>(QUEUE_CAP);
        std::thread::Builder::new()
            .name("gsa-logsink".into())
            .spawn(move || run(&addr, &hello, &rx))
            .expect("spawn log sink thread");
        Self { tx }
    }

    /// Queue one line (no trailing newline). Never blocks: on a full queue
    /// the line is dropped.
    pub fn push(&self, line: String) {
        let _ = self.tx.try_send(line);
    }
}

fn run(addr: &str, hello: &str, rx: &mpsc::Receiver<String>) {
    loop {
        let Some(mut stream) = connect(addr) else {
            // Collector unreachable: park until it might be back. Lines keep
            // queueing (then dropping) meanwhile; the backlog that fits is
            // delivered on reconnect.
            std::thread::sleep(RECONNECT_DELAY);
            continue;
        };
        if writeln!(stream, "{hello}").is_err() {
            continue;
        }
        // Forward until the channel closes (process exit) or a write fails
        // (collector went away — reconnect and re-hello).
        loop {
            match rx.recv() {
                Ok(line) => {
                    if writeln!(stream, "{line}").is_err() {
                        break;
                    }
                }
                Err(_) => return,
            }
        }
    }
}

fn connect(addr: &str) -> Option<std::net::TcpStream> {
    let resolved = std::net::ToSocketAddrs::to_socket_addrs(&addr)
        .ok()?
        .next()?;
    let stream = std::net::TcpStream::connect_timeout(&resolved, CONNECT_TIMEOUT).ok()?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT)).ok()?;
    stream.set_nodelay(true).ok();
    Some(stream)
}
