//! The tunnel session over the browser's `WebTransport`.

use crate::js_error;
use gsa_backend_moonlight::tunnel::{StreamReader, StreamWriter, TunnelSession};
use gsa_core::{Error, Result};
use js_sys::Uint8Array;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    ReadableStreamDefaultReader, ReadableStreamReadResult, WebTransport,
    WebTransportBidirectionalStream, WebTransportCongestionControl, WebTransportHash,
    WebTransportOptions, WritableStreamDefaultWriter,
};

/// One connected WebTransport session to altc's tunnel endpoint.
///
/// Cheap to clone: every clone is the same JavaScript object.
#[wasm_bindgen]
#[derive(Debug, Clone)]
pub struct WebTunnel {
    transport: WebTransport,
    datagram_reader: ReadableStreamDefaultReader,
    datagram_writer: WritableStreamDefaultWriter,
}

#[wasm_bindgen]
impl WebTunnel {
    /// Connect to `url` and wait until the session is ready.
    ///
    /// `cert_sha256` is the tunnel certificate's SHA-256 as the API handed it
    /// out; with it the browser trusts that exact certificate and nothing
    /// else, which is WebTransport's own mechanism for self-hosted endpoints.
    /// Without it the browser applies its usual trust store.
    #[wasm_bindgen]
    pub async fn connect(url: &str, cert_sha256: Option<Box<[u8]>>) -> Result<WebTunnel, JsValue> {
        let options = WebTransportOptions::new();
        // Datagrams are the whole point; a session that could only offer
        // streams would fall back to something we refuse to run over.
        options.set_require_unreliable(true);
        options.set_congestion_control(WebTransportCongestionControl::LowLatency);
        if let Some(hash) = cert_sha256 {
            let entry = WebTransportHash::new();
            entry.set_algorithm("sha-256");
            entry.set_value_u8_array(&Uint8Array::from(&hash[..]));
            options.set_server_certificate_hashes(&[entry]);
        }
        let transport = WebTransport::new_with_options(url, &options)?;
        JsFuture::from(transport.ready()).await?;
        let datagrams = transport.datagrams();
        // Stale datagrams are worse than lost ones for a live stream: the
        // depacketiser would rather see a gap than a late shard.
        datagrams.set_incoming_max_age(50.0);
        datagrams.set_outgoing_max_age(50.0);
        let datagram_reader = ReadableStreamDefaultReader::new(&datagrams.readable())?;
        let datagram_writer = datagram_writable(&datagrams).await?.get_writer()?;
        Ok(Self {
            transport,
            datagram_reader,
            datagram_writer,
        })
    }

    /// The largest datagram this session can carry.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn max_datagram_size(&self) -> u32 {
        self.transport.datagrams().max_datagram_size()
    }

    /// Close the session. Every stream and link over it ends.
    #[wasm_bindgen]
    pub fn close(&self) {
        self.transport.close();
    }
}

/// The stream outgoing datagrams are written to.
///
/// The specification renamed it: `datagrams.writable` became
/// `datagrams.createWritable()`, a promise of a send stream. Chrome still
/// offers both; Safari only the new form, which web-sys does not bind yet,
/// so it is called by name where it exists.
async fn datagram_writable(
    datagrams: &web_sys::WebTransportDatagramDuplexStream,
) -> Result<web_sys::WritableStream, JsValue> {
    let create = js_sys::Reflect::get(datagrams, &JsValue::from_str("createWritable"))?;
    if let Some(create) = create.dyn_ref::<js_sys::Function>() {
        let stream = JsFuture::from(js_sys::Promise::resolve(&create.call0(datagrams)?)).await?;
        return stream.dyn_into::<web_sys::WritableStream>();
    }
    Ok(datagrams.writable())
}

/// One chunk from a reader: `Ok(None)` at the end of the stream.
async fn read_chunk(reader: &ReadableStreamDefaultReader) -> Result<Option<Vec<u8>>> {
    let result = JsFuture::from(reader.read())
        .await
        .map_err(|e| js_error("read", &e))?;
    let result: ReadableStreamReadResult = result.unchecked_into();
    if result.get_done().unwrap_or(false) {
        return Ok(None);
    }
    let value = result.get_value();
    let bytes: Uint8Array = value
        .dyn_into()
        .map_err(|_| Error::Transport("stream chunk was not bytes".into()))?;
    Ok(Some(bytes.to_vec()))
}

/// The sending half of a bidirectional stream.
#[derive(Debug)]
pub struct WebStreamWriter {
    writer: WritableStreamDefaultWriter,
}

impl StreamWriter for WebStreamWriter {
    async fn write(&mut self, bytes: &[u8]) -> Result<()> {
        JsFuture::from(self.writer.write_with_chunk(&Uint8Array::from(bytes)))
            .await
            .map(|_| ())
            .map_err(|e| js_error("write", &e))
    }

    async fn finish(&mut self) {
        let _ = JsFuture::from(self.writer.close()).await;
    }
}

/// The receiving half of a bidirectional stream.
#[derive(Debug)]
pub struct WebStreamReader {
    reader: ReadableStreamDefaultReader,
}

impl StreamReader for WebStreamReader {
    async fn read(&mut self) -> Result<Option<Vec<u8>>> {
        read_chunk(&self.reader).await
    }
}

impl TunnelSession for WebTunnel {
    type Writer = WebStreamWriter;
    type Reader = WebStreamReader;

    async fn open_stream(&self) -> Result<(WebStreamWriter, WebStreamReader)> {
        let stream: WebTransportBidirectionalStream =
            JsFuture::from(self.transport.create_bidirectional_stream())
                .await
                .map_err(|e| js_error("open stream", &e))?;
        let writer = stream
            .writable()
            .get_writer()
            .map_err(|e| js_error("stream writer", &e))?;
        let reader = ReadableStreamDefaultReader::new(&stream.readable())
            .map_err(|e| js_error("stream reader", &e))?;
        Ok((WebStreamWriter { writer }, WebStreamReader { reader }))
    }

    fn send_datagram(&self, bytes: &[u8]) -> Result<()> {
        // The write settles when the datagram is sent or expires; either
        // way it is unreliable delivery and not this call's concern.
        let promise = self
            .datagram_writer
            .write_with_chunk(&Uint8Array::from(bytes));
        wasm_bindgen_futures::spawn_local(async move {
            let _ = JsFuture::from(promise).await;
        });
        Ok(())
    }

    async fn recv_datagram(&self) -> Option<Vec<u8>> {
        match read_chunk(&self.datagram_reader).await {
            Ok(datagram) => datagram,
            Err(e) => {
                tracing::debug!(error = %e, "datagram stream ended");
                None
            }
        }
    }
}
