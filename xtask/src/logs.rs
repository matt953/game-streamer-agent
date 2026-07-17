//! Dev log collector: listens for the log streams the agent (`GSA_LOG_SINK`)
//! and debug-build clients (auto-discovered via `SessionParams.log_sink`)
//! push, prefixing each line with its stream label on stdout and teeing to
//! per-stream files under `devlogs/` (repo root — deliberately outside
//! `target/` so `cargo clean` cannot destroy session evidence).

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result};

static CONN_SEQ: AtomicU32 = AtomicU32::new(0);

pub fn logs(listen: &str) -> Result<()> {
    let dir = PathBuf::from("devlogs");
    std::fs::create_dir_all(&dir)?;
    let listener = TcpListener::bind(listen).with_context(|| format!("bind {listen}"))?;
    println!(
        "[logs] collecting on {} -> {}",
        listener.local_addr()?,
        dir.display()
    );
    println!(
        "[logs] agent: set GSA_LOG_SINK=<this-host>:<port>; debug clients follow automatically"
    );
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    if let Err(e) = serve(s, &dir) {
                        eprintln!("[logs] stream ended: {e}");
                    }
                });
            }
            Err(e) => eprintln!("[logs] accept: {e}"),
        }
    }
    Ok(())
}

fn serve(stream: TcpStream, dir: &std::path::Path) -> Result<()> {
    let peer = stream.peer_addr()?;
    let mut lines = BufReader::new(stream).lines();
    // First line is the sender's self-label ("hello role=agent os=windows ...").
    let hello = lines.next().transpose()?.unwrap_or_default();
    let label = hello
        .split_whitespace()
        .filter_map(|kv| kv.split_once('='))
        .filter(|(k, _)| *k == "role" || *k == "os")
        .map(|(_, v)| v)
        .collect::<Vec<_>>()
        .join("-");
    let label = if label.is_empty() {
        "unknown".into()
    } else {
        label
    };
    let seq = CONN_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!("{label}-{seq}.log"));
    let mut file = std::fs::File::create(&path)?;
    println!("[logs] + {label} ({peer}) -> {}", path.display());
    for line in lines {
        let line = line?;
        println!("[{label}] {line}");
        writeln!(file, "{line}")?;
    }
    println!("[logs] - {label} disconnected");
    Ok(())
}
