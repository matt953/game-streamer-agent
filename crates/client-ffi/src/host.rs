//! Backend-neutral host references (spec 16).
//!
//! The app stores one **opaque blob** per enrolled host and hands it back when
//! listing a library or starting a session. It must not parse the blob: what a
//! backend keeps in there (keys, certificates, tokens) is the backend's
//! business, and that is what keeps per-backend concepts out of the app.
//!
//! The encoding is newline-separated `key=value` with newlines escaped, so it
//! survives a C string and a keychain item. It is private to this crate;
//! nothing else may depend on its shape.

use std::ffi::{CStr, c_char};

/// Which protocol a host speaks.
///
/// No enrolment call produces [`GSA_BACKEND_GSA`] yet — the gsa agent still has
/// its own entry point — but the blob names the backend so the neutral calls
/// can tell them apart the moment it moves here.
pub const GSA_BACKEND_GSA: u32 = 0;
pub const GSA_BACKEND_MOONLIGHT: u32 = 1;

/// An enrolled host: how to reach it, and the credentials to prove we may.
#[derive(Debug, Clone)]
pub(crate) struct HostRef {
    pub backend: u32,
    pub addr: std::net::SocketAddr,
    pub fields: Vec<(String, String)>,
}

impl HostRef {
    /// A paired Moonlight host.
    pub(crate) fn moonlight(
        addr: std::net::SocketAddr,
        key_pem: &str,
        host_cert_pem: &str,
        client_id: &str,
    ) -> Self {
        Self {
            backend: GSA_BACKEND_MOONLIGHT,
            addr,
            fields: vec![
                ("key".to_owned(), key_pem.to_owned()),
                ("cert".to_owned(), host_cert_pem.to_owned()),
                ("client_id".to_owned(), client_id.to_owned()),
            ],
        }
    }

    pub(crate) fn encode(&self) -> String {
        let mut out = format!("backend={}\naddr={}\n", self.backend, self.addr);
        for (key, value) in &self.fields {
            // Values are multi-line (PEMs), so newlines are escaped rather
            // than the format gaining quoting rules.
            out.push_str(&format!("{key}={}\n", value.replace('\n', "\\n")));
        }
        out
    }

    pub(crate) fn decode(text: &str) -> Option<Self> {
        let mut backend = None;
        let mut addr = None;
        let mut fields = Vec::new();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.replace("\\n", "\n");
            match key {
                "backend" => backend = value.parse().ok(),
                "addr" => addr = value.parse().ok(),
                _ => fields.push((key.to_owned(), value)),
            }
        }
        Some(Self {
            backend: backend?,
            addr: addr?,
            fields,
        })
    }

    pub(crate) fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Copy `text` into a caller-supplied buffer as a NUL-terminated string.
///
/// Returns the byte count *excluding* the terminator, so a caller can size a
/// buffer by calling with a small one first. Too small returns `-(needed + 1)`;
/// it never truncates, since a partial credential blob still parses.
pub(crate) fn write_out(text: &str, out: *mut c_char, cap: usize) -> i32 {
    let needed = text.len();
    if out.is_null() || cap == 0 {
        return needed as i32;
    }
    if needed + 1 > cap {
        return -(needed as i32) - 1;
    }
    // SAFETY: the length check above guarantees room for the bytes and the
    // terminator, and the caller contract makes `out` writable for `cap`.
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr().cast::<c_char>(), out, needed);
        *out.add(needed) = 0;
    }
    needed as i32
}

/// Read a C string argument, or `None` if it is null or not UTF-8.
///
/// # Safety
/// `ptr` must be a valid NUL-terminated string for the duration of the call.
pub(crate) unsafe fn read_str<'a>(ptr: *const c_char) -> Option<&'a str> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: caller contract.
    unsafe { CStr::from_ptr(ptr) }.to_str().ok()
}

/// List what a host can launch, one callback per entry.
///
/// `host` is the blob an enrolment call produced; this function does not care
/// which backend made it. `kind` is 0 game, 1 desktop, 2 shell — a console
/// returns a single shell entry rather than a library.
///
/// Returns the entry count, or negative: `-1` bad arguments, `-2` runtime,
/// `-3` the host refused (often permissions rather than the network).
///
/// # Safety
/// `host` must be a valid NUL-terminated string for the call; `ctx` must stay
/// valid until this function returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsa_catalog(
    host: *const c_char,
    on_app: Option<
        unsafe extern "C" fn(
            ctx: *mut std::ffi::c_void,
            id: u32,
            kind: u32,
            running: bool,
            title: *const c_char,
        ),
    >,
    ctx: *mut std::ffi::c_void,
) -> i32 {
    // SAFETY: caller contract.
    let Some(host) = (unsafe { read_str(host) }) else {
        return -1;
    };
    let Some(host) = HostRef::decode(host) else {
        return -1;
    };
    if host.backend != GSA_BACKEND_MOONLIGHT {
        // Backends without a host-side library are not an error; they simply
        // have nothing to list until their catalog is implemented.
        return 0;
    }
    let (Some(key), Some(cert), Some(client_id)) = (
        host.field("key"),
        host.field("cert"),
        host.field("client_id"),
    ) else {
        return -1;
    };
    let Ok(identity) = gsa_backend_moonlight::ClientIdentity::from_key_pem(key) else {
        return -1;
    };
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return -2;
    };

    rt.block_on(async move {
        let Ok(info) = gsa_backend_moonlight::probe(host.addr, client_id).await else {
            return -3;
        };
        let session = gsa_backend_moonlight::PairedSession::new(
            std::net::SocketAddr::new(host.addr.ip(), info.https_port),
            cert.to_owned(),
            identity,
            client_id.to_owned(),
        );
        let Ok(entries) = session.catalog().await else {
            return -3;
        };
        if let Some(cb) = on_app {
            for entry in &entries {
                // An interior NUL cannot occur in a title; skip if it somehow does.
                let Ok(title) = std::ffi::CString::new(entry.title.as_str()) else {
                    continue;
                };
                let kind = match entry.kind {
                    gsa_client_core::CatalogKind::Desktop => 1,
                    gsa_client_core::CatalogKind::Shell => 2,
                    _ => 0,
                };
                // SAFETY: `title` outlives the call; `ctx` valid per contract.
                unsafe { cb(ctx, entry.id, kind, entry.running, title.as_ptr()) };
            }
        }
        entries.len() as i32
    })
}

/// Start streaming from an enrolled host.
///
/// The one entry point for every backend: `host` is the blob enrolment
/// produced, `target_id` is a catalog entry's id (ignored by backends that
/// have no library), and everything after the session starts goes through the
/// returned handle exactly as it does for any other backend.
///
/// Returns an owned session handle, or NULL on failure. Blocks until the
/// session is streaming or has failed. Release with [`crate::gsa_session_stop`].
///
/// # Safety
/// `host` must be a valid NUL-terminated string for the call. `decode_codecs`
/// must point to `decode_codecs_len` `GSA_CODEC_*` values, or be null. The function
/// pointers and `ctx` in `callbacks` must stay valid until
/// [`crate::gsa_session_stop`] returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsa_host_session_start(
    host: *const c_char,
    target_id: u32,
    bitrate_kbps: u32,
    mode: crate::GsaStreamMode,
    decode_codecs: *const u32,
    decode_codecs_len: usize,
    callbacks: crate::GsaCallbacks,
    err: *mut c_char,
    err_cap: usize,
) -> *mut crate::GsaSession {
    crate::devlog::init();
    // Every failure path writes `err`; NULL alone is not a diagnosis.
    let fail = |reason: &str| -> *mut crate::GsaSession {
        write_out(reason, err, err_cap);
        std::ptr::null_mut()
    };

    // SAFETY: caller contract.
    let Some(host) = (unsafe { read_str(host) }) else {
        return fail("no host given");
    };
    let Some(host) = HostRef::decode(host) else {
        return fail("this host's saved details are unreadable — pair again");
    };
    if host.backend != GSA_BACKEND_MOONLIGHT {
        return fail("this kind of host cannot be streamed from yet");
    }
    let (Some(key), Some(cert), Some(client_id)) = (
        host.field("key"),
        host.field("cert"),
        host.field("client_id"),
    ) else {
        return fail("this host's saved details are incomplete — pair again");
    };

    let opts = crate::moonlight::MoonlightOpts {
        addr: host.addr,
        key_pem: key.to_owned(),
        host_cert_pem: cert.to_owned(),
        client_id: client_id.to_owned(),
        app_id: target_id,
        bitrate_kbps: if bitrate_kbps == 0 {
            20_000
        } else {
            bitrate_kbps
        },
        mode: {
            let (width, height, fps) = mode.resolve();
            gsa_backend_moonlight::StreamMode {
                width,
                height,
                fps,
                allow_host_mode_change: mode.allow_host_mode_change != 0,
                hdr: mode.hdr != 0,
                ..Default::default()
            }
        },
        // What the embedder says it can decode, richest first. H.264 is added
        // whatever is passed: a session with nothing to negotiate is worse
        // than one that falls back.
        // SAFETY: caller contract — `decode_codecs_len` values, or null.
        decode_codecs: unsafe { crate::codecs_from_list(decode_codecs, decode_codecs_len) },
    };

    let stop = std::sync::Arc::new(tokio::sync::Notify::new());
    let thread_stop = stop.clone();
    let cbs = crate::SendCallbacks(callbacks);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<crate::SessionReady>();
    let thread = std::thread::spawn(move || {
        let cbs = cbs;
        crate::boost_thread_qos();
        crate::moonlight::run_session(opts, cbs, thread_stop, ready_tx);
    });

    match ready_rx.recv() {
        Ok(crate::SessionReady::Streaming {
            input,
            knobs,
            presented,
            decode_error,
            dejitter,
            pacing,
            codec,
            pad_caps,
        }) => Box::into_raw(Box::new(crate::GsaSession {
            stop,
            thread: Some(thread),
            input,
            knobs,
            presented,
            decode_error,
            dejitter,
            pacing,
            codec,
            pad_caps,
        })),
        // Either the session failed or the thread died; join so its failure is
        // not left running behind a NULL return.
        other => {
            stop.notify_waiters();
            let _ = thread.join();
            let reason = match other {
                Ok(crate::SessionReady::Failed(reason)) if !reason.is_empty() => reason,
                _ => "the host did not start the session".to_owned(),
            };
            fail(&reason)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{GSA_BACKEND_MOONLIGHT, HostRef};

    #[test]
    fn a_host_blob_round_trips_with_multi_line_credentials() {
        let host = HostRef::moonlight(
            "192.168.1.5:47989".parse().unwrap(),
            "-----BEGIN PRIVATE KEY-----\nabc\ndef\n-----END PRIVATE KEY-----\n",
            "-----BEGIN CERTIFICATE-----\nxyz\n-----END CERTIFICATE-----\n",
            "0123456789ABCDEF",
        );
        let decoded = HostRef::decode(&host.encode()).expect("decodes");
        assert_eq!(decoded.backend, GSA_BACKEND_MOONLIGHT);
        assert_eq!(decoded.addr, host.addr);
        // PEMs are multi-line; losing the newlines would silently produce an
        // unusable key rather than an obvious error.
        assert_eq!(decoded.field("key"), host.field("key"));
        assert_eq!(decoded.field("cert"), host.field("cert"));
        assert_eq!(decoded.field("client_id"), Some("0123456789ABCDEF"));
    }

    #[test]
    fn a_blob_without_the_essentials_is_rejected() {
        assert!(HostRef::decode("addr=1.2.3.4:1").is_none());
        assert!(HostRef::decode("backend=1").is_none());
        assert!(HostRef::decode("nonsense").is_none());
    }
}
