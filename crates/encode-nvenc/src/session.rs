//! A safe wrapper over one NVENC encode session.
//!
//! Everything the driver hands back is an opaque pointer with a matching
//! destroy call; this module is where those pairs are kept honest. Nothing
//! above it touches a raw pointer, and nothing here prints or panics.

use std::ffi::c_void;
use std::ptr;

use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::core::Interface;

use gsa_core::media::{H264Profile, VideoMode};
use gsa_core::{Error, Result};

use crate::sys::{self, Nvenc};

/// A registered + mapped D3D11 texture, released in reverse order on drop.
pub(crate) struct MappedTexture<'a> {
    session: &'a Session,
    registered: *mut c_void,
    mapped: *mut c_void,
}

impl MappedTexture<'_> {
    pub(crate) fn input(&self) -> *mut c_void {
        self.mapped
    }
}

impl Drop for MappedTexture<'_> {
    fn drop(&mut self) {
        let f = self.session.nvenc.functions();
        if let Some(unmap) = f.nvEncUnmapInputResource {
            // SAFETY: `mapped` came from nvEncMapInputResource on this session.
            let _ = unsafe { unmap(self.session.encoder, self.mapped) };
        }
        if let Some(unregister) = f.nvEncUnregisterResource {
            // SAFETY: `registered` came from nvEncRegisterResource on this session.
            let _ = unsafe { unregister(self.session.encoder, self.registered) };
        }
    }
}

/// One NVENC encode session bound to a D3D11 device.
pub(crate) struct Session {
    nvenc: &'static Nvenc,
    encoder: *mut c_void,
    bitstream: *mut c_void,
    /// Retained so the device outlives the session that encodes from it.
    _device: ID3D11Device,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session").finish_non_exhaustive()
    }
}

// SAFETY: NVENC session calls are safe from any single thread, and the encode
// pipeline owns this session on one thread at a time. The D3D11 device is
// free-threaded.
unsafe impl Send for Session {}

impl Session {
    /// Open a session on `device` without configuring an encoder. Used both by
    /// [`crate::probe`] and by the real encoder.
    pub(crate) fn open(device: &ID3D11Device) -> Result<Self> {
        let nvenc = Nvenc::get()?;
        let open = nvenc
            .functions()
            .nvEncOpenEncodeSessionEx
            .ok_or_else(|| Error::Encode("driver exposes no nvEncOpenEncodeSessionEx".into()))?;

        let mut params = sys::NvEncOpenEncodeSessionExParams {
            version: sys::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
            deviceType: sys::NV_ENC_DEVICE_TYPE_DIRECTX,
            device: device.as_raw(),
            reserved: ptr::null_mut(),
            apiVersion: sys::NVENC_API_VERSION,
            reserved1: [0; 253],
            reserved2: [ptr::null_mut(); 64],
        };
        let mut encoder: *mut c_void = ptr::null_mut();
        // SAFETY: `params` is correctly versioned and `device` is a live
        // ID3D11Device, which is what DEVICE_TYPE_DIRECTX means.
        let status = unsafe { open(&raw mut params, &raw mut encoder) };
        if status == sys::NV_ENC_ERR_INVALID_VERSION {
            return Err(Error::Encode(
                "NVENC rejected our API version; the driver is too old".into(),
            ));
        }
        sys::check(status, "nvEncOpenEncodeSessionEx")?;
        if encoder.is_null() {
            return Err(Error::Encode(
                "nvEncOpenEncodeSessionEx returned null".into(),
            ));
        }

        Ok(Session {
            nvenc,
            encoder,
            bitstream: ptr::null_mut(),
            _device: device.clone(),
        })
    }

    /// Configure the encoder and allocate its output buffer.
    pub(crate) fn initialize(
        &mut self,
        mode: VideoMode,
        bitrate_bps: u32,
        profile: H264Profile,
    ) -> Result<()> {
        let mut preset = self.preset_config()?;
        let config = &mut preset.presetCfg;

        // Low-latency contract (spec 03): no B-frames, no automatic IDR, CBR
        // sized so one frame's worth of bits never stalls behind the VBV.
        config.version = sys::NV_ENC_CONFIG_VER;
        config.profileGUID = profile_guid(profile);
        config.gopLength = sys::NVENC_INFINITE_GOPLENGTH;
        config.frameIntervalP = 1;
        config.rcParams.version = sys::NV_ENC_RC_PARAMS_VER;
        config.rcParams.rateControlMode = sys::NV_ENC_PARAMS_RC_CBR;
        config.rcParams.averageBitRate = bitrate_bps;
        config.rcParams.maxBitRate = bitrate_bps;
        let fps = mode.fps.max(1);
        config.rcParams.vbvBufferSize = bitrate_bps / fps;
        config.rcParams.vbvInitialDelay = config.rcParams.vbvBufferSize;
        config.rcParams.lookaheadDepth = 0;

        // SAFETY: the union's H.264 arm is the one the preset filled, because
        // we asked for the H.264 preset config.
        let h264 = unsafe { &mut config.encodeCodecConfig.h264Config };
        h264.idrPeriod = sys::NVENC_INFINITE_GOPLENGTH;
        // Baseline forbids CABAC; picking the wrong one silently produces a
        // stream the client cannot decode.
        h264.entropyCodingMode = match profile {
            H264Profile::ConstrainedBaseline => sys::NV_ENC_H264_ENTROPY_CODING_MODE_CAVLC,
            _ => sys::NV_ENC_H264_ENTROPY_CODING_MODE_CABAC,
        };

        let mut params = sys::NvEncInitializeParams {
            version: sys::NV_ENC_INITIALIZE_PARAMS_VER,
            encodeGUID: sys::NV_ENC_CODEC_H264_GUID,
            presetGUID: sys::NV_ENC_PRESET_P3_GUID,
            encodeWidth: mode.width,
            encodeHeight: mode.height,
            darWidth: mode.width,
            darHeight: mode.height,
            frameRateNum: fps,
            frameRateDen: 1,
            enableEncodeAsync: 0,
            // Picture-type decision stays with NVENC; we only force IDR.
            enablePTD: 1,
            bitfields: 0,
            privDataSize: 0,
            reserved: 0,
            privData: ptr::null_mut(),
            encodeConfig: &raw mut preset.presetCfg,
            maxEncodeWidth: mode.width,
            maxEncodeHeight: mode.height,
            maxMEHintCountsPerBlock: [sys::NvEncMeHintCountsPerBlocktype { fields: [0; 4] }; 2],
            tuningInfo: sys::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
            bufferFormat: sys::NV_ENC_BUFFER_FORMAT_ARGB,
            numStateBuffers: 0,
            outputStatsLevel: 0,
            reserved1: [0; 284],
            reserved2: [ptr::null_mut(); 64],
        };

        let init = self
            .nvenc
            .functions()
            .nvEncInitializeEncoder
            .ok_or_else(|| Error::Encode("driver exposes no nvEncInitializeEncoder".into()))?;
        // SAFETY: `params` is correctly versioned and points at a config that
        // outlives this call.
        let status = unsafe { init(self.encoder, &raw mut params) };
        self.check(status, "nvEncInitializeEncoder")?;

        self.bitstream = self.create_bitstream()?;
        Ok(())
    }

    /// Ask the driver to fill a config for our preset + tuning, then we adjust
    /// only what the low-latency contract requires. Hand-building an
    /// `NV_ENC_CONFIG` from zero would silently diverge from driver defaults.
    fn preset_config(&self) -> Result<Box<sys::NvEncPresetConfig>> {
        let get = self
            .nvenc
            .functions()
            .nvEncGetEncodePresetConfigEx
            .ok_or_else(|| {
                Error::Encode("driver exposes no nvEncGetEncodePresetConfigEx".into())
            })?;
        // SAFETY: all-zero is the documented initial state; the driver fills
        // everything except the two version fields we set.
        let mut preset: Box<sys::NvEncPresetConfig> = unsafe { Box::new(std::mem::zeroed()) };
        preset.version = sys::NV_ENC_PRESET_CONFIG_VER;
        preset.presetCfg.version = sys::NV_ENC_CONFIG_VER;
        // SAFETY: correctly versioned out-param on a live session.
        let status = unsafe {
            get(
                self.encoder,
                sys::NV_ENC_CODEC_H264_GUID,
                sys::NV_ENC_PRESET_P3_GUID,
                sys::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                &raw mut *preset,
            )
        };
        self.check(status, "nvEncGetEncodePresetConfigEx")?;
        Ok(preset)
    }

    fn create_bitstream(&self) -> Result<*mut c_void> {
        let create = self
            .nvenc
            .functions()
            .nvEncCreateBitstreamBuffer
            .ok_or_else(|| Error::Encode("driver exposes no nvEncCreateBitstreamBuffer".into()))?;
        let mut params = sys::NvEncCreateBitstreamBuffer {
            version: sys::NV_ENC_CREATE_BITSTREAM_BUFFER_VER,
            size: 0,
            memoryHeap: 0,
            reserved: 0,
            bitstreamBuffer: ptr::null_mut(),
            bitstreamBufferPtr: ptr::null_mut(),
            reserved1: [0; 58],
            reserved2: [ptr::null_mut(); 64],
        };
        // SAFETY: correctly versioned out-param on an initialized session.
        let status = unsafe { create(self.encoder, &raw mut params) };
        self.check(status, "nvEncCreateBitstreamBuffer")?;
        Ok(params.bitstreamBuffer)
    }

    /// Register and map a texture for one encode. The returned guard releases
    /// both in the right order.
    pub(crate) fn map(
        &self,
        texture: &ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> Result<MappedTexture<'_>> {
        let f = self.nvenc.functions();
        let register = f
            .nvEncRegisterResource
            .ok_or_else(|| Error::Encode("driver exposes no nvEncRegisterResource".into()))?;
        let map = f
            .nvEncMapInputResource
            .ok_or_else(|| Error::Encode("driver exposes no nvEncMapInputResource".into()))?;

        let mut reg = sys::NvEncRegisterResource {
            version: sys::NV_ENC_REGISTER_RESOURCE_VER,
            resourceType: sys::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX,
            width,
            height,
            // DirectX resources carry their own pitch.
            pitch: 0,
            subResourceIndex: 0,
            resourceToRegister: texture.as_raw(),
            registeredResource: ptr::null_mut(),
            bufferFormat: sys::NV_ENC_BUFFER_FORMAT_ARGB,
            bufferUsage: sys::NV_ENC_INPUT_IMAGE,
            pInputFencePoint: ptr::null_mut(),
            chromaOffset: [0; 2],
            chromaOffsetIn: [0; 2],
            reserved1: [0; 244],
            reserved2: [ptr::null_mut(); 61],
        };
        // SAFETY: `texture` is a live D3D11 texture on this session's device.
        let status = unsafe { register(self.encoder, &raw mut reg) };
        self.check(status, "nvEncRegisterResource")?;

        let mut mapping = sys::NvEncMapInputResource {
            version: sys::NV_ENC_MAP_INPUT_RESOURCE_VER,
            subResourceIndex: 0,
            inputResource: ptr::null_mut(),
            registeredResource: reg.registeredResource,
            mappedResource: ptr::null_mut(),
            mappedBufferFmt: 0,
            reserved1: [0; 251],
            reserved2: [ptr::null_mut(); 63],
        };
        // SAFETY: `registeredResource` was just produced by this session.
        let status = unsafe { map(self.encoder, &raw mut mapping) };
        if let Err(e) = self.check(status, "nvEncMapInputResource") {
            if let Some(unregister) = f.nvEncUnregisterResource {
                // SAFETY: releasing the resource we registered a moment ago.
                let _ = unsafe { unregister(self.encoder, reg.registeredResource) };
            }
            return Err(e);
        }

        Ok(MappedTexture {
            session: self,
            registered: reg.registeredResource,
            mapped: mapping.mappedResource,
        })
    }

    /// Submit one picture. `idr` forces a keyframe and attaches SPS/PPS to it.
    pub(crate) fn encode(
        &self,
        input: &MappedTexture<'_>,
        mode: VideoMode,
        frame_index: u32,
        idr: bool,
    ) -> Result<()> {
        let encode = self
            .nvenc
            .functions()
            .nvEncEncodePicture
            .ok_or_else(|| Error::Encode("driver exposes no nvEncEncodePicture".into()))?;

        let flags = if idr {
            sys::NV_ENC_PIC_FLAG_FORCEIDR | sys::NV_ENC_PIC_FLAG_OUTPUT_SPSPPS
        } else {
            0
        };
        // SAFETY: all-zero is the documented initial state for the fields we
        // do not set, including the codec-specific union.
        let mut params: sys::NvEncPicParams = unsafe { std::mem::zeroed() };
        params.version = sys::NV_ENC_PIC_PARAMS_VER;
        params.inputWidth = mode.width;
        params.inputHeight = mode.height;
        params.inputPitch = mode.width;
        params.encodePicFlags = flags;
        params.frameIdx = frame_index;
        params.inputTimeStamp = u64::from(frame_index);
        params.inputBuffer = input.input();
        params.outputBitstream = self.bitstream;
        params.bufferFmt = sys::NV_ENC_BUFFER_FORMAT_ARGB;
        params.pictureStruct = sys::NV_ENC_PIC_STRUCT_FRAME;
        params.pictureType = if idr {
            sys::NV_ENC_PIC_TYPE_IDR
        } else {
            sys::NV_ENC_PIC_TYPE_P
        };

        // SAFETY: correctly versioned params referencing a mapped input and a
        // bitstream buffer this session owns.
        let status = unsafe { encode(self.encoder, &raw mut params) };
        // With no B-frames and no lookahead the encoder never asks us to feed
        // it more before producing output; treat it as a hard error rather
        // than silently emitting nothing.
        if status == sys::NV_ENC_ERR_NEED_MORE_INPUT {
            return Err(Error::Encode(
                "NVENC wants more input despite a zero-latency config".into(),
            ));
        }
        self.check(status, "nvEncEncodePicture")
    }

    /// Copy out the bitstream produced by the last [`Session::encode`].
    /// Returns the Annex-B bytes and whether the picture was an IDR.
    pub(crate) fn take_bitstream(&self) -> Result<(Vec<u8>, bool)> {
        let f = self.nvenc.functions();
        let lock = f
            .nvEncLockBitstream
            .ok_or_else(|| Error::Encode("driver exposes no nvEncLockBitstream".into()))?;
        let unlock = f
            .nvEncUnlockBitstream
            .ok_or_else(|| Error::Encode("driver exposes no nvEncUnlockBitstream".into()))?;

        // SAFETY: all-zero initial state; the driver fills the rest.
        let mut params: sys::NvEncLockBitstream = unsafe { std::mem::zeroed() };
        params.version = sys::NV_ENC_LOCK_BITSTREAM_VER;
        params.outputBitstream = self.bitstream;
        // bitfields = 0 → doNotWait = 0, so the driver blocks until ready.

        // SAFETY: correctly versioned; `outputBitstream` is ours.
        let status = unsafe { lock(self.encoder, &raw mut params) };
        self.check(status, "nvEncLockBitstream")?;

        let len = params.bitstreamSizeInBytes as usize;
        let mut data = Vec::with_capacity(len);
        if !params.bitstreamBufferPtr.is_null() && len > 0 {
            // SAFETY: the lock guarantees `len` readable bytes at this pointer
            // until we unlock, and `data` has capacity for exactly that many.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    params.bitstreamBufferPtr.cast::<u8>(),
                    data.as_mut_ptr(),
                    len,
                );
                data.set_len(len);
            }
        }
        let idr = params.pictureType == sys::NV_ENC_PIC_TYPE_IDR;

        // SAFETY: matches the lock above, same bitstream buffer.
        let status = unsafe { unlock(self.encoder, self.bitstream) };
        self.check(status, "nvEncUnlockBitstream")?;
        Ok((data, idr))
    }

    /// Attach the driver's own message to a failing status. Without it every
    /// NVENC error reads the same.
    fn check(&self, status: sys::NvencStatus, call: &str) -> Result<()> {
        if status == sys::NV_ENC_SUCCESS {
            return Ok(());
        }
        let detail = sys::last_error(self.nvenc, self.encoder);
        Err(Error::Encode(if detail.is_empty() {
            format!("{call}: NVENC status {status}")
        } else {
            format!("{call}: NVENC status {status}: {detail}")
        }))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let f = self.nvenc.functions();
        if !self.bitstream.is_null()
            && let Some(destroy) = f.nvEncDestroyBitstreamBuffer
        {
            // SAFETY: the buffer was created by this session.
            let _ = unsafe { destroy(self.encoder, self.bitstream) };
        }
        if let Some(destroy) = f.nvEncDestroyEncoder {
            // SAFETY: the session handle came from nvEncOpenEncodeSessionEx.
            let _ = unsafe { destroy(self.encoder) };
        }
    }
}

fn profile_guid(profile: H264Profile) -> sys::Guid {
    match profile {
        H264Profile::ConstrainedBaseline => sys::NV_ENC_H264_PROFILE_BASELINE_GUID,
        H264Profile::Main => sys::NV_ENC_H264_PROFILE_MAIN_GUID,
        H264Profile::High => sys::NV_ENC_H264_PROFILE_HIGH_GUID,
    }
}
