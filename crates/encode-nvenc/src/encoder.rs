//! The `Encoder` implementation: a `GpuFrame` carrying a D3D11 texture in,
//! Annex-B H.264 or HEVC out, with nothing crossing the PCIe bus in between.

use bytes::Bytes;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;

use gsa_capture_api::GpuFrame;
use gsa_capture_windows::D3D11Frame;
use gsa_core::id::FrameId;
use gsa_core::media::{Codec, FrameKind, H264Profile, PixelFormat};
use gsa_core::time::MediaClock;
use gsa_core::{Error, Result};
use gsa_encode_api::{EncodeConfig, EncodedChunk, Encoder, EncoderCaps, FrameDirectives};

use crate::Support;
use crate::session::Session;

/// NVENC's H.264 ceiling on every GPU that supports the API version we pin.
const MAX_WIDTH: u32 = 4096;
const MAX_HEIGHT: u32 = 4096;

/// Is there a usable NVENC on this machine, and on which adapter?
///
/// Opens and immediately drops a real session: a driver can be present and a
/// GPU absent, or the GPU can be an NVIDIA one that cannot encode. Only an
/// actual session proves it. Never panics — a machine with no NVIDIA anything
/// simply gets `None`.
pub(crate) fn probe() -> Option<Support> {
    /// PCI vendor id. NVENC exists on no other vendor's silicon.
    const NVIDIA: u32 = 0x10DE;

    let adapters = gsa_capture_windows::list_adapters().ok()?;
    for adapter in adapters.iter().filter(|a| a.vendor_id == NVIDIA) {
        let Ok(device) = gsa_capture_windows::create_device_on(adapter.luid) else {
            continue;
        };
        match Session::open(&device) {
            Ok(session) => {
                drop(session);
                tracing::info!(gpu = adapter.name, "NVENC available");
                return Some(Support {
                    adapter_luid: adapter.luid,
                });
            }
            Err(e) => tracing::debug!(gpu = adapter.name, error = %e, "NVENC session refused"),
        }
    }
    tracing::info!("no NVENC encoder; the software encoder will be used");
    None
}

/// Hardware H.264/HEVC encoder backed by NVENC.
pub struct NvencEncoder {
    clock: MediaClock,
    config: Option<EncodeConfig>,
    /// Opened on the first `submit`, once a frame has told us which device
    /// the texture lives on.
    session: Option<Session>,
    next_frame_id: FrameId,
    pending: Option<EncodedChunk>,
    force_idr: bool,
    frames_submitted: u64,
}

impl std::fmt::Debug for NvencEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NvencEncoder")
            .field("open", &self.config.is_some())
            .field("session", &self.session.is_some())
            .finish()
    }
}

impl NvencEncoder {
    #[must_use]
    pub fn new(clock: MediaClock) -> Self {
        Self {
            clock,
            config: None,
            session: None,
            next_frame_id: FrameId::ZERO,
            pending: None,
            force_idr: false,
            frames_submitted: 0,
        }
    }

    /// The session, opened against the device the frame arrived on.
    fn session(&mut self, device: &ID3D11Device) -> Result<&Session> {
        if self.session.is_none() {
            let cfg = self
                .config
                .ok_or_else(|| Error::Encode("encoder not open".into()))?;
            let mut session = Session::open(device)?;
            session.initialize(cfg.codec, cfg.mode, cfg.bitrate_bps, cfg.h264_profile)?;
            tracing::info!(
                codec = ?cfg.codec,
                width = cfg.mode.width,
                height = cfg.mode.height,
                bitrate = cfg.bitrate_bps,
                profile = ?cfg.h264_profile,
                "NVENC session initialized"
            );
            self.session = Some(session);
        }
        Ok(self.session.as_ref().expect("session just set"))
    }
}

impl Encoder for NvencEncoder {
    fn caps(&self) -> EncoderCaps {
        EncoderCaps {
            name: "nvenc",
            // HEVC first: preferred where the client can decode it.
            codecs: vec![Codec::Hevc, Codec::H264],
            // WGC gives us BGRA; NVENC converts to NV12 on the GPU.
            input_formats: vec![PixelFormat::Bgra8],
            max_width: MAX_WIDTH,
            max_height: MAX_HEIGHT,
            supports_slices: false,
            supports_intra_refresh: false,
            // NVENC can invalidate references, but the MVP falls back to
            // force_idr (spec 04 ladder); see `invalidate_refs`.
            supports_ref_invalidation: false,
            max_h264_profile: H264Profile::High,
        }
    }

    fn open(&mut self, cfg: EncodeConfig) -> Result<()> {
        if !matches!(cfg.codec, Codec::H264 | Codec::Hevc) {
            return Err(Error::Encode(format!(
                "nvenc backend does H264 and HEVC, got {:?}",
                cfg.codec
            )));
        }
        if cfg.mode.width > MAX_WIDTH || cfg.mode.height > MAX_HEIGHT {
            return Err(Error::Encode(format!(
                "nvenc: {}x{} exceeds {MAX_WIDTH}x{MAX_HEIGHT}",
                cfg.mode.width, cfg.mode.height
            )));
        }
        // The session waits for the first frame, which carries the device.
        self.config = Some(cfg);
        self.force_idr = true; // first frame is always an IDR
        Ok(())
    }

    fn submit(&mut self, frame: &GpuFrame, directives: FrameDirectives) -> Result<()> {
        if frame.format != PixelFormat::Bgra8 {
            return Err(Error::Encode(format!(
                "nvenc needs Bgra8, got {:?}",
                frame.format
            )));
        }
        let d3d = frame
            .handle
            .downcast_platform::<D3D11Frame>()
            .ok_or_else(|| Error::Encode("nvenc needs a D3D11 platform frame".into()))?;

        let mode = self
            .config
            .ok_or_else(|| Error::Encode("encoder not open".into()))?
            .mode;
        if frame.width != mode.width || frame.height != mode.height {
            return Err(Error::Encode(format!(
                "nvenc opened at {}x{}, got a {}x{} frame",
                mode.width, mode.height, frame.width, frame.height
            )));
        }

        let idr = directives.idr || self.force_idr;
        let frame_id = self.next_frame_id;
        let capture_ts_us = frame.capture_ts_us;

        let session = self.session(d3d.device())?;
        // Phase timings answer "who is slow": `map` blocks behind the GPU's
        // 3D queue (a busy game lands here), `lock` blocks on the encode ASIC.
        let t0 = std::time::Instant::now();
        let mapped = session.map(d3d.texture(), mode.width, mode.height)?;
        let t_map = t0.elapsed();
        session.encode(&mapped, mode, frame_id.wire(), idr)?;
        let t_enc = t0.elapsed() - t_map;
        let (data, was_idr) = session.take_bitstream()?;
        let t_lock = t0.elapsed() - t_map - t_enc;
        drop(mapped);

        self.frames_submitted += 1;
        if self.frames_submitted.is_multiple_of(120) {
            let gpu = crate::nvml::Nvml::get().map(crate::nvml::Nvml::sample);
            tracing::debug!(
                map_ms = t_map.as_secs_f64() * 1000.0,
                enc_ms = t_enc.as_secs_f64() * 1000.0,
                lock_ms = t_lock.as_secs_f64() * 1000.0,
                gpu_util = gpu.and_then(|s| s.gpu_util),
                enc_util = gpu.and_then(|s| s.encoder_util),
                sm_mhz = gpu.and_then(|s| s.sm_mhz),
                temp_c = gpu.and_then(|s| s.temp_c),
                throttle = gpu.map(|s| s.throttle_names()).as_deref(),
                "nvenc health"
            );
        }

        self.force_idr = false;
        self.next_frame_id = frame_id.next();
        if data.is_empty() {
            return Ok(());
        }
        self.pending = Some(EncodedChunk {
            frame_id,
            kind: if was_idr {
                FrameKind::Idr
            } else {
                FrameKind::P
            },
            data: Bytes::from(data),
            capture_ts_us,
            encode_done_ts_us: self.clock.now_us(),
        });
        Ok(())
    }

    fn poll_bitstream(&mut self) -> Result<Option<EncodedChunk>> {
        Ok(self.pending.take())
    }

    fn next_chunk(&mut self, _timeout: std::time::Duration) -> Result<Option<EncodedChunk>> {
        // Synchronous: `submit` blocks on nvEncLockBitstream, so the chunk is
        // already here.
        self.poll_bitstream()
    }

    fn update_rate(&mut self, bitrate_bps: u32) -> Result<()> {
        let cfg = self
            .config
            .as_mut()
            .ok_or_else(|| Error::Encode("encoder not open".into()))?;
        if cfg.bitrate_bps == bitrate_bps {
            return Ok(());
        }
        cfg.bitrate_bps = bitrate_bps;
        // Reconfigure in place: reopening the session would cost an IDR, and
        // ABR cuts the rate precisely when a fat frame hurts most (spec 04).
        // With no session yet, the next `session()` opens at the new rate.
        if let Some(session) = self.session.as_mut() {
            session.reconfigure(bitrate_bps)?;
            tracing::debug!(bitrate = bitrate_bps, "NVENC reconfigured");
        }
        Ok(())
    }

    fn force_idr(&mut self) {
        self.force_idr = true;
    }

    fn invalidate_refs(&mut self, last_good_wire: u32) -> bool {
        let Some(session) = &self.session else {
            return false;
        };
        let current = self.next_frame_id.wire();
        let lost = current.wrapping_sub(last_good_wire).saturating_sub(1);
        // Selective invalidation is only sound while `last_good` itself is
        // certainly still resident in the DPB; a wide window risks frames
        // referencing evicted pictures — silent corruption, not an error.
        // Half the nominal DPB is the conservative bound; beyond it an IDR
        // is the only safe recovery.
        if lost == 0 || lost > 8 {
            tracing::debug!(
                last_good_wire,
                current,
                lost,
                "invalidation window unsafe; IDR"
            );
            return false;
        }
        for i in 1..=lost {
            let ts = u64::from(last_good_wire.wrapping_add(i));
            if let Err(e) = session.invalidate_ref(ts) {
                tracing::warn!(error = %e, "ref invalidation failed; falling back to IDR");
                return false;
            }
        }
        tracing::debug!(last_good_wire, lost, "references invalidated");
        true
    }

    fn close(&mut self) {
        self.session = None;
        self.config = None;
        self.pending = None;
    }
}
