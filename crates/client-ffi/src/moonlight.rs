//! Moonlight **enrolment** (spec 16).
//!
//! Only pairing lives here. Listing a host's library and starting a session
//! are backend-neutral and live in [`crate::host`] — the app must never grow
//! a per-protocol path for those, or it starts knowing which backend it is
//! talking to, which is the thing the seam exists to prevent.
//!
//! Enrolment is per backend by design: a PIN typed on a PC, an OAuth round
//! trip, and a vendor token exchange take different inputs and different UI.
//! All they share is their output — the opaque host blob the neutral calls
//! take.
//!
//! Two things the embedder must persist, both inside that blob:
//!
//! - **The client key.** Minted once by [`gsa_moonlight_identity`]. A host
//!   remembers a paired client by the certificate derived from it, so losing
//!   the key un-pairs every host. Keep it somewhere private (a keychain).
//! - **The host certificate**, returned by pairing. Later connections are
//!   pinned to it, which is what makes them mutually authenticated.

use std::ffi::c_char;

use gsa_backend_moonlight::ClientIdentity;

use crate::host::{HostRef, read_str, write_out};

/// Mint a client identity, writing its private key as PEM into `out`.
///
/// Slow (RSA key generation) — call once, off the UI thread, and store the
/// result. Returns the PEM length, or a negative value whose magnitude minus
/// one is the buffer size required.
///
/// # Safety
/// `out` must be writable for `cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsa_moonlight_identity(out: *mut c_char, cap: usize) -> i32 {
    let Ok(identity) = ClientIdentity::generate() else {
        return -1;
    };
    write_out(identity.key_pem(), out, cap)
}

/// Pair with a Moonlight host, producing the host blob the neutral calls take.
///
/// **Blocks until the operator enters `pin` on the host**, which can take
/// minutes — call it off the UI thread and show the PIN while waiting.
///
/// Returns the blob's length, `-1` on bad arguments, `-2` if the runtime could
/// not start, `-3` if pairing failed.
///
/// # Safety
/// All string arguments must be valid NUL-terminated strings for the call;
/// `out` must be writable for `cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsa_moonlight_pair(
    url: *const c_char,
    key_pem: *const c_char,
    client_id: *const c_char,
    device_name: *const c_char,
    pin: *const c_char,
    out: *mut c_char,
    cap: usize,
) -> i32 {
    // SAFETY: caller contract for every string argument.
    let (Some(url), Some(key), Some(client_id), Some(device_name), Some(pin)) = (unsafe {
        (
            read_str(url),
            read_str(key_pem),
            read_str(client_id),
            read_str(device_name),
            read_str(pin),
        )
    }) else {
        return -1;
    };
    let (Ok(addr), Ok(identity)) = (
        url.parse::<std::net::SocketAddr>(),
        ClientIdentity::from_key_pem(key),
    ) else {
        return -1;
    };
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return -2;
    };
    let paired = match rt.block_on(gsa_backend_moonlight::pair(
        addr,
        client_id,
        device_name,
        pin,
        &identity,
    )) {
        Ok(paired) => paired,
        Err(e) => {
            tracing::warn!(error = %e, "moonlight pairing failed");
            return -3;
        }
    };

    let blob = HostRef::moonlight(addr, key, &paired.host_cert_pem, client_id);
    write_out(&blob.encode(), out, cap)
}

/// Everything the session loop needs, read out of the host blob so the loop
/// works with plain Rust values.
pub(crate) struct MoonlightOpts {
    pub addr: std::net::SocketAddr,
    pub key_pem: String,
    pub host_cert_pem: String,
    pub client_id: String,
    pub app_id: u32,
    pub bitrate_kbps: u32,
    pub mode: gsa_backend_moonlight::StreamMode,
}

impl MoonlightOpts {
    /// Bring the session up and hand back the running stream.
    async fn start(
        &self,
    ) -> gsa_core::Result<(
        gsa_backend_moonlight::PairedSession,
        gsa_backend_moonlight::MoonlightStream,
    )> {
        let identity = ClientIdentity::from_key_pem(&self.key_pem)?;
        let info = gsa_backend_moonlight::probe(self.addr, &self.client_id).await?;
        let mut session = gsa_backend_moonlight::PairedSession::new(
            std::net::SocketAddr::new(self.addr.ip(), info.https_port),
            self.host_cert_pem.clone(),
            identity,
            self.client_id.clone(),
        );
        let stream = gsa_backend_moonlight::start(
            &mut session,
            self.addr.ip(),
            self.app_id,
            self.mode,
            self.bitrate_kbps,
        )
        .await?;
        Ok((session, stream))
    }
}

/// Drive a Moonlight session, feeding the same callbacks every backend does.
///
/// Backend-specific only in how frames are produced: once they are whole they
/// go through the shared [`gsa_client_core::StreamSession`], so the gate,
/// de-jitter and health accounting are the ones the gsa path already uses.
pub(crate) fn run_session(
    opts: MoonlightOpts,
    cbs: crate::SendCallbacks,
    stop: std::sync::Arc<tokio::sync::Notify>,
    ready_tx: std::sync::mpsc::Sender<crate::SessionReady>,
) {
    let cbs = cbs.0;
    let Ok(rt) = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .on_thread_start(crate::boost_thread_qos)
        .build()
    else {
        let _ = ready_tx.send(crate::SessionReady::Failed(
            "could not start a runtime".into(),
        ));
        return;
    };

    rt.block_on(async move {
        let (session, mut stream) = match opts.start().await {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(error = %e, "moonlight session failed to start");
                // The host's own words: it knows causes we cannot infer, such
                // as this device lacking permission.
                let _ = ready_tx.send(crate::SessionReady::Failed(e.to_string()));
                return;
            }
        };

        let audio_rx = stream.audio_channel();
        let Some(frames) = stream.take_frames() else {
            let _ = ready_tx.send(crate::SessionReady::Failed("frames already claimed".into()));
            return;
        };
        let mut core = gsa_client_core::StreamSession::with_capture_clock(
            frames,
            stream.recovery.clone(),
            gsa_core::time::MediaClock::new(),
            gsa_client_core::ClockSync::default(),
            stream.dropped.clone(),
            stream.recovered.clone(),
            // The host's stamps are a stream clock: cadence and jitter are
            // real, absolute latency is not.
            gsa_client_core::CaptureClock::StreamPts,
        );

        let _ = ready_tx.send(crate::SessionReady::Streaming {
            input: Some(stream.input.clone()),
            // A Moonlight host fixes bitrate at negotiation, so there are no
            // live quality controls to offer.
            knobs: None,
            presented: core.presented_sink(),
            decode_error: core.decode_error_flag(),
            dejitter: core.dejitter_flag(),
            codec: crate::GSA_CODEC_H264,
            pad_caps: u32::from(stream.pad_caps().bits()),
        });

        // Audio drains on its own thread so PCM keeps flowing while the frame
        // loop is parked waiting for the next picture.
        let audio_ctx = crate::SendPtr(cbs.ctx);
        let on_audio = cbs.on_audio;
        let audio_thread = std::thread::spawn(move || {
            crate::boost_thread_qos();
            let audio_ctx = audio_ctx;
            while let Ok(pcm) = audio_rx.recv() {
                if let Some(cb) = on_audio {
                    // SAFETY: pointer+len describe this owned buffer for the
                    // call; the embedder copies whatever it keeps.
                    unsafe { cb(audio_ctx.0, pcm.as_ptr(), pcm.len()) };
                }
            }
        });

        loop {
            tokio::select! {
                () = stop.notified() => break,
                frame = core.recv_encoded() => match frame {
                    Ok(Some(f)) => {
                        if let Some(cb) = cbs.on_video {
                            // SAFETY: pointer+len describe f.data for this call only.
                            unsafe {
                                cb(
                                    cbs.ctx,
                                    f.data.as_ptr(),
                                    f.data.len(),
                                    f.keyframe,
                                    f.capture_ts_us,
                                    // Always 0 here: the host's stamp is a
                                    // stream clock, so there is no absolute
                                    // latency. 0 means unmeasured, not instant.
                                    f.latency_us.unwrap_or(0),
                                );
                            }
                        }
                    }
                    Ok(None) | Err(_) => break,
                },
            }
            // Host feedback (rumble today) is drained off the frame path so a
            // quiet stream cannot delay it indefinitely.
            while let Ok(message) = stream.events.try_recv() {
                let Some(event) = message.neutral() else {
                    continue;
                };
                // The full effect, not just its existence: amplitudes, colours
                // and motion requests all reach the embedder here.
                crate::fire_pad_feedback(&cbs, &event);
            }
        }

        // Tear the host session down explicitly: a session left behind stops
        // the next one from starting.
        drop(core);
        drop(stream);
        let _ = session.cancel().await;
        let _ = audio_thread.join();
    });
}
