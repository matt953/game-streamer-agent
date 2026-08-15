//! Minimal HTTP/1.1 for the Moonlight control endpoints.
//!
//! Deliberately hand-rolled rather than pulling a general HTTP client in: the
//! surface is a handful of GETs returning small XML bodies, the paired path
//! needs a custom certificate verifier anyway (self-signed certs pinned at
//! pairing, the same shape `gsa-transport` already solves), and this crate is
//! embedded in a mobile app where dependency weight is a real cost.
//!
//! Hosts answer with `Content-Length` and close the connection, so there is no
//! chunked decoding or connection reuse here. Anything richer is a sign the
//! host is not what we think it is, and is reported as an error rather than
//! guessed at.

use gsa_core::{Error, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Cap on a control response. The largest legitimate body is an app list;
/// megabytes of it means something is wrong, and an unbounded read from an
/// unauthenticated port is a denial-of-service waiting to happen.
const MAX_BODY: usize = 4 * 1024 * 1024;

/// One cleartext `GET` against the host's HTTP port.
///
/// `path_and_query` is sent as-is, so callers own percent-encoding.
pub(crate) async fn get(addr: std::net::SocketAddr, path_and_query: &str) -> Result<Vec<u8>> {
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| Error::Transport(format!("connect {addr}: {e}")))?;
    // Nagle would sit on this request waiting for more to send.
    let _ = stream.set_nodelay(true);
    exchange(&mut stream, addr, path_and_query).await
}

/// Send one `GET` and read the whole response off an already-connected
/// stream, so the plain and TLS paths share their framing.
pub(crate) async fn exchange<S>(
    stream: &mut S,
    host: std::net::SocketAddr,
    path_and_query: &str,
) -> Result<Vec<u8>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let request =
        format!("GET {path_and_query} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| Error::Transport(format!("send request: {e}")))?;

    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = stream
            .read(&mut buf)
            .await
            .map_err(|e| Error::Transport(format!("read response: {e}")))?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
        if raw.len() > MAX_BODY {
            return Err(Error::Transport("response too large".into()));
        }
    }
    parse_response(&raw)
}

/// Split a raw HTTP/1.1 response into its status and body, rejecting anything
/// that is not a plain `Content-Length`-framed 200.
fn parse_response(raw: &[u8]) -> Result<Vec<u8>> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| Error::Transport("no header terminator in response".into()))?;
    let head = std::str::from_utf8(&raw[..split])
        .map_err(|_| Error::Transport("non-UTF-8 response headers".into()))?;
    let body = &raw[split + 4..];

    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| Error::Transport("empty response".into()))?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| Error::Transport(format!("bad status line: {status_line}")))?;
    if status != 200 {
        return Err(Error::Transport(format!("host returned HTTP {status}")));
    }
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(Error::Transport(format!(
                "unsupported transfer-encoding: {}",
                value.trim()
            )));
        }
    }
    Ok(body.to_vec())
}

#[cfg(test)]
mod tests {
    use super::parse_response;

    #[test]
    fn extracts_a_content_length_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        assert_eq!(parse_response(raw).unwrap(), b"hello");
    }

    #[test]
    fn rejects_non_200() {
        // The host answers 404 for endpoints that need pairing; surfacing it
        // as an error is what turns "empty app list" into "not paired yet".
        let raw = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
        assert!(parse_response(raw).is_err());
    }

    #[test]
    fn rejects_chunked_rather_than_mis_framing_it() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        assert!(parse_response(raw).is_err());
    }

    #[test]
    fn needs_a_header_terminator() {
        assert!(parse_response(b"HTTP/1.1 200 OK\r\n").is_err());
    }
}
