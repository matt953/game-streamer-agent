//! NVENC ABI: types, constants and the function table, for one pinned API
//! version. Nothing here is platform-specific — the same definitions serve
//! Linux, where only the library name and the device/resource types differ.
//!
//! Provenance: transcribed against the MIT-licensed `nvenc` crate's
//! re-expression of the ABI, not from NVIDIA's `nvEncodeAPI.h` (whose licence
//! is incompatible with this repo). Every struct carries a size assertion, so
//! a transcription slip is a compile error rather than silent corruption —
//! NVENC's `version` field encodes the API version, *not* the struct size, so
//! the driver cannot catch a wrong layout for us.
//!
//! Fields the driver *writes* are typed `u32`, never a Rust `enum`: a value
//! outside our variant list would be instant undefined behaviour.

#![allow(non_snake_case)]

use std::ffi::{c_char, c_void};

/// The API version we speak. NVENC is backward compatible — a newer driver
/// keeps serving older API versions — so this is a *minimum driver* floor,
/// not a ceiling. Raising it strands users on older drivers; see
/// [`super::Nvenc::load`], which checks it and fails soft.
pub const NVENC_MAJOR_VERSION: u32 = 13;
pub const NVENC_MINOR_VERSION: u32 = 0;
pub const NVENC_API_VERSION: u32 = NVENC_MAJOR_VERSION | (NVENC_MINOR_VERSION << 24);

/// `NVENCAPI_STRUCT_VERSION`: the API version, the struct's own revision, and
/// a fixed tag. `big` marks the structs NVIDIA flagged with bit 31.
const fn struct_version(revision: u32, big: bool) -> u32 {
    let v = NVENC_API_VERSION | (revision << 16) | (0x7 << 28);
    if big { v | (1 << 31) } else { v }
}

pub const NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER: u32 = struct_version(1, false);
pub const NV_ENC_INITIALIZE_PARAMS_VER: u32 = struct_version(7, true);
pub const NV_ENC_CONFIG_VER: u32 = struct_version(9, true);
pub const NV_ENC_RC_PARAMS_VER: u32 = struct_version(1, false);
pub const NV_ENC_PRESET_CONFIG_VER: u32 = struct_version(5, true);
pub const NV_ENC_PIC_PARAMS_VER: u32 = struct_version(7, true);
pub const NV_ENC_LOCK_BITSTREAM_VER: u32 = struct_version(2, true);
pub const NV_ENC_REGISTER_RESOURCE_VER: u32 = struct_version(5, false);
pub const NV_ENC_MAP_INPUT_RESOURCE_VER: u32 = struct_version(4, false);
pub const NV_ENC_CREATE_BITSTREAM_BUFFER_VER: u32 = struct_version(1, false);
pub const NV_ENCODE_API_FUNCTION_LIST_VER: u32 = struct_version(2, false);

/// No automatic IDR: the client asks for keyframes (spec 04).
pub const NVENC_INFINITE_GOPLENGTH: u32 = 0xffff_ffff;

pub type NvencStatus = i32;
pub const NV_ENC_SUCCESS: NvencStatus = 0;
pub const NV_ENC_ERR_INVALID_VERSION: NvencStatus = 15;
pub const NV_ENC_ERR_NEED_MORE_INPUT: NvencStatus = 17;

// Device / resource kinds. Linux swaps DIRECTX for CUDA here and nowhere else.
pub const NV_ENC_DEVICE_TYPE_DIRECTX: u32 = 0;
pub const NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX: u32 = 0;
pub const NV_ENC_INPUT_IMAGE: u32 = 0;

/// 8-bit packed A8R8G8B8. In memory that is B,G,R,A — i.e. exactly
/// `DXGI_FORMAT_B8G8R8A8_UNORM`, which is what WGC hands us. Verified by
/// encoding a known blue/red image and reading the chroma back.
pub const NV_ENC_BUFFER_FORMAT_ARGB: u32 = 0x0100_0000;

pub const NV_ENC_PIC_STRUCT_FRAME: u32 = 0x01;
pub const NV_ENC_PIC_TYPE_P: u32 = 0x00;
pub const NV_ENC_PIC_TYPE_IDR: u32 = 0x03;

pub const NV_ENC_PIC_FLAG_FORCEIDR: u32 = 0x2;
/// Emit SPS/PPS with this picture. Cheaper and clearer than the `repeatSPSPPS`
/// config bit: a client that joins late, or recovers from loss, needs the
/// parameter sets attached to the IDR it actually receives.
pub const NV_ENC_PIC_FLAG_OUTPUT_SPSPPS: u32 = 0x4;

pub const NV_ENC_PARAMS_RC_CBR: u32 = 0x02;

pub const NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY: u32 = 3;

pub const NV_ENC_H264_ENTROPY_CODING_MODE_CABAC: u32 = 0x1;
pub const NV_ENC_H264_ENTROPY_CODING_MODE_CAVLC: u32 = 0x2;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

const fn guid(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> Guid {
    Guid {
        data1,
        data2,
        data3,
        data4,
    }
}

pub const NV_ENC_CODEC_H264_GUID: Guid = guid(
    0x6bc8_2762,
    0x4e63,
    0x4ca4,
    [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf],
);
pub const NV_ENC_H264_PROFILE_BASELINE_GUID: Guid = guid(
    0x0727_bcaa,
    0x78c4,
    0x4c83,
    [0x8c, 0x2f, 0xef, 0x3d, 0xff, 0x26, 0x7c, 0x6a],
);
pub const NV_ENC_H264_PROFILE_MAIN_GUID: Guid = guid(
    0x60b5_c1d4,
    0x67fe,
    0x4790,
    [0x94, 0xd5, 0xc4, 0x72, 0x6d, 0x7b, 0x6e, 0x6d],
);
pub const NV_ENC_H264_PROFILE_HIGH_GUID: Guid = guid(
    0xe7cb_c309,
    0x4f7a,
    0x4b89,
    [0xaf, 0x2a, 0xd5, 0x37, 0xc9, 0x2b, 0xe3, 0x10],
);

/// P3: the fastest preset that still uses a real motion search. P1/P2 trade
/// too much quality for latency we already have in hand.
pub const NV_ENC_PRESET_P3_GUID: Guid = guid(
    0x3685_0110,
    0x3a07,
    0x441f,
    [0x94, 0xd5, 0x36, 0x70, 0x63, 0x1f, 0x91, 0xf6],
);

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct NvEncQp {
    pub qpInterP: u32,
    pub qpInterB: u32,
    pub qpIntra: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NvEncRcParams {
    pub version: u32,
    pub rateControlMode: u32,
    pub constQP: NvEncQp,
    pub averageBitRate: u32,
    pub maxBitRate: u32,
    pub vbvBufferSize: u32,
    pub vbvInitialDelay: u32,
    pub bitfields: u32,
    pub minQP: NvEncQp,
    pub maxQP: NvEncQp,
    pub initialRCQP: NvEncQp,
    pub temporalLayerIdxMask: u32,
    pub temporalLayerQP: [u8; 8],
    pub targetQuality: u8,
    pub targetQualityLSB: u8,
    pub lookaheadDepth: u16,
    pub lowDelayKeyFrameScale: u8,
    pub yDcQPIndexOffset: i8,
    pub uDcQPIndexOffset: i8,
    pub vDcQPIndexOffset: i8,
    pub qpMapMode: u32,
    pub multiPass: u32,
    pub alphaLayerBitrateRatio: u32,
    pub cbQPIndexOffset: i8,
    pub crQPIndexOffset: i8,
    pub reserved2: u16,
    pub lookaheadLevel: u32,
    pub viewBitrateRatios: [u8; 7],
    pub reserved3: u8,
    pub reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NvEncConfigH264Vui {
    pub fields: [u32; 16],
    pub reserved: [u32; 12],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NvEncConfigH264 {
    pub bitfields: u32,
    pub level: u32,
    pub idrPeriod: u32,
    pub separateColourPlaneFlag: u32,
    pub disableDeblockingFilterIDC: u32,
    pub numTemporalLayers: u32,
    pub spsId: u32,
    pub ppsId: u32,
    pub adaptiveTransformMode: u32,
    pub fmoMode: u32,
    pub bdirectMode: u32,
    pub entropyCodingMode: u32,
    pub stereoMode: u32,
    pub intraRefreshPeriod: u32,
    pub intraRefreshCnt: u32,
    pub maxNumRefFrames: u32,
    pub sliceMode: u32,
    pub sliceModeData: u32,
    pub h264VUIParameters: NvEncConfigH264Vui,
    pub ltrNumFrames: u32,
    pub ltrTrustMode: u32,
    pub chromaFormatIDC: u32,
    pub maxTemporalLayers: u32,
    pub useBFramesAsRef: u32,
    pub numRefL0: u32,
    pub numRefL1: u32,
    pub outputBitDepth: u32,
    pub inputBitDepth: u32,
    pub tfLevel: u32,
    pub reserved1: [u32; 264],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union NvEncCodecConfig {
    pub h264Config: NvEncConfigH264,
    pub reserved: [u64; 224],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NvEncConfig {
    pub version: u32,
    pub profileGUID: Guid,
    pub gopLength: u32,
    pub frameIntervalP: i32,
    pub monoChromeEncoding: u32,
    pub frameFieldMode: u32,
    pub mvPrecision: u32,
    pub rcParams: NvEncRcParams,
    pub encodeCodecConfig: NvEncCodecConfig,
    pub reserved: [u32; 278],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NvEncMeHintCountsPerBlocktype {
    pub fields: [u32; 4],
}

#[repr(C)]
pub struct NvEncInitializeParams {
    pub version: u32,
    pub encodeGUID: Guid,
    pub presetGUID: Guid,
    pub encodeWidth: u32,
    pub encodeHeight: u32,
    pub darWidth: u32,
    pub darHeight: u32,
    pub frameRateNum: u32,
    pub frameRateDen: u32,
    pub enableEncodeAsync: u32,
    pub enablePTD: u32,
    pub bitfields: u32,
    pub privDataSize: u32,
    pub reserved: u32,
    pub privData: *mut c_void,
    pub encodeConfig: *mut NvEncConfig,
    pub maxEncodeWidth: u32,
    pub maxEncodeHeight: u32,
    pub maxMEHintCountsPerBlock: [NvEncMeHintCountsPerBlocktype; 2],
    pub tuningInfo: u32,
    pub bufferFormat: u32,
    pub numStateBuffers: u32,
    pub outputStatsLevel: u32,
    pub reserved1: [u32; 284],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
pub struct NvEncPresetConfig {
    pub version: u32,
    pub reserved: u32,
    pub presetCfg: NvEncConfig,
    pub reserved1: [u32; 256],
    pub reserved2: [*mut c_void; 64],
}

/// Codec-specific picture parameters. We never populate them for H.264 — IDR
/// and SPS/PPS ride on `encodePicFlags` — so the union stays opaque.
#[repr(C)]
#[derive(Clone, Copy)]
pub union NvEncCodecPicParams {
    pub reserved: [u64; 193],
}

#[repr(C)]
pub struct NvEncPicParams {
    pub version: u32,
    pub inputWidth: u32,
    pub inputHeight: u32,
    pub inputPitch: u32,
    pub encodePicFlags: u32,
    pub frameIdx: u32,
    pub inputTimeStamp: u64,
    pub inputDuration: u64,
    pub inputBuffer: *mut c_void,
    pub outputBitstream: *mut c_void,
    pub completionEvent: *mut c_void,
    pub bufferFmt: u32,
    pub pictureStruct: u32,
    pub pictureType: u32,
    pub codecPicParams: NvEncCodecPicParams,
    pub meHintCountsPerBlock: [NvEncMeHintCountsPerBlocktype; 2],
    pub meExternalHints: *mut c_void,
    pub reserved1: [u32; 7],
    pub reserved2: [*mut c_void; 2],
    pub qpDeltaMap: *mut i8,
    pub qpDeltaMapSize: u32,
    pub reservedBitFields: u32,
    pub meHintRefPicDist: [u16; 2],
    pub reserved3: u32,
    pub alphaBuffer: *mut c_void,
    pub meExternalSbHints: *mut c_void,
    pub meSbHintsCount: u32,
    pub stateBufferIdx: u32,
    pub outputReconBuffer: *mut c_void,
    pub reserved4: [u32; 284],
    pub reserved5: [*mut c_void; 57],
}

#[repr(C)]
pub struct NvEncLockBitstream {
    pub version: u32,
    pub bitfields: u32,
    pub outputBitstream: *mut c_void,
    pub sliceOffsets: *mut u32,
    pub frameIdx: u32,
    pub hwEncodeStatus: u32,
    pub numSlices: u32,
    pub bitstreamSizeInBytes: u32,
    pub outputTimeStamp: u64,
    pub outputDuration: u64,
    pub bitstreamBufferPtr: *mut c_void,
    pub pictureType: u32,
    pub pictureStruct: u32,
    pub frameAvgQP: u32,
    pub frameSatd: u32,
    pub ltrFrameIdx: u32,
    pub ltrFrameBitmap: u32,
    pub temporalId: u32,
    pub interMbCount: u32,
    pub averageMVX: i32,
    pub averageMVY: i32,
    pub alphaLayerSizeInBytes: u32,
    pub outputStatsPtrSize: u32,
    pub reserved: u32,
    pub outputStatsPtr: *mut c_void,
    pub frameIdxDisplay: u32,
    pub reserved1: [u32; 219],
    pub reserved2: [*mut c_void; 63],
    pub reservedInternal: [u32; 8],
}

#[repr(C)]
pub struct NvEncRegisterResource {
    pub version: u32,
    pub resourceType: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub subResourceIndex: u32,
    pub resourceToRegister: *mut c_void,
    pub registeredResource: *mut c_void,
    pub bufferFormat: u32,
    pub bufferUsage: u32,
    pub pInputFencePoint: *mut c_void,
    pub chromaOffset: [u32; 2],
    pub chromaOffsetIn: [u32; 2],
    pub reserved1: [u32; 244],
    pub reserved2: [*mut c_void; 61],
}

#[repr(C)]
pub struct NvEncMapInputResource {
    pub version: u32,
    pub subResourceIndex: u32,
    pub inputResource: *mut c_void,
    pub registeredResource: *mut c_void,
    pub mappedResource: *mut c_void,
    pub mappedBufferFmt: u32,
    pub reserved1: [u32; 251],
    pub reserved2: [*mut c_void; 63],
}

#[repr(C)]
pub struct NvEncCreateBitstreamBuffer {
    pub version: u32,
    pub size: u32,
    pub memoryHeap: u32,
    pub reserved: u32,
    pub bitstreamBuffer: *mut c_void,
    pub bitstreamBufferPtr: *mut c_void,
    pub reserved1: [u32; 58],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
pub struct NvEncOpenEncodeSessionExParams {
    pub version: u32,
    pub deviceType: u32,
    pub device: *mut c_void,
    pub reserved: *mut c_void,
    pub apiVersion: u32,
    pub reserved1: [u32; 253],
    pub reserved2: [*mut c_void; 64],
}

/// `NVENCAPI` is `__stdcall` on Windows, which *is* the x64 calling
/// convention, and nothing on Linux — so `system` is correct on both.
type Fn1 = Option<unsafe extern "system" fn(*mut c_void) -> NvencStatus>;
type Fn2 = Option<unsafe extern "system" fn(*mut c_void, *mut c_void) -> NvencStatus>;

/// The driver's dispatch table. Every slot must exist and be in order: the
/// driver writes function pointers into it by offset.
#[repr(C)]
pub struct NvEncodeApiFunctionList {
    pub version: u32,
    pub reserved: u32,
    pub nvEncOpenEncodeSession: Fn2,
    pub nvEncGetEncodeGUIDCount: Fn2,
    pub nvEncGetEncodeProfileGUIDCount: Fn2,
    pub nvEncGetEncodeProfileGUIDs: Fn2,
    pub nvEncGetEncodeGUIDs: Fn2,
    pub nvEncGetInputFormatCount: Fn2,
    pub nvEncGetInputFormats: Fn2,
    pub nvEncGetEncodeCaps: Fn2,
    pub nvEncGetEncodePresetCount: Fn2,
    pub nvEncGetEncodePresetGUIDs: Fn2,
    pub nvEncGetEncodePresetConfig: Fn2,
    pub nvEncInitializeEncoder:
        Option<unsafe extern "system" fn(*mut c_void, *mut NvEncInitializeParams) -> NvencStatus>,
    pub nvEncCreateInputBuffer: Fn2,
    pub nvEncDestroyInputBuffer: Fn2,
    pub nvEncCreateBitstreamBuffer: Option<
        unsafe extern "system" fn(*mut c_void, *mut NvEncCreateBitstreamBuffer) -> NvencStatus,
    >,
    pub nvEncDestroyBitstreamBuffer: Fn2,
    pub nvEncEncodePicture:
        Option<unsafe extern "system" fn(*mut c_void, *mut NvEncPicParams) -> NvencStatus>,
    pub nvEncLockBitstream:
        Option<unsafe extern "system" fn(*mut c_void, *mut NvEncLockBitstream) -> NvencStatus>,
    pub nvEncUnlockBitstream: Fn2,
    pub nvEncLockInputBuffer: Fn2,
    pub nvEncUnlockInputBuffer: Fn2,
    pub nvEncGetEncodeStats: Fn2,
    pub nvEncGetSequenceParams: Fn2,
    pub nvEncRegisterAsyncEvent: Fn2,
    pub nvEncUnregisterAsyncEvent: Fn2,
    pub nvEncMapInputResource:
        Option<unsafe extern "system" fn(*mut c_void, *mut NvEncMapInputResource) -> NvencStatus>,
    pub nvEncUnmapInputResource: Fn2,
    pub nvEncDestroyEncoder: Fn1,
    pub nvEncInvalidateRefFrames: Fn2,
    pub nvEncOpenEncodeSessionEx: Option<
        unsafe extern "system" fn(
            *mut NvEncOpenEncodeSessionExParams,
            *mut *mut c_void,
        ) -> NvencStatus,
    >,
    pub nvEncRegisterResource:
        Option<unsafe extern "system" fn(*mut c_void, *mut NvEncRegisterResource) -> NvencStatus>,
    pub nvEncUnregisterResource: Fn2,
    pub nvEncReconfigureEncoder: Fn2,
    pub reserved1: *mut c_void,
    pub nvEncCreateMVBuffer: Fn2,
    pub nvEncDestroyMVBuffer: Fn2,
    pub nvEncRunMotionEstimationOnly: Fn2,
    pub nvEncGetLastErrorString: Option<unsafe extern "system" fn(*mut c_void) -> *const c_char>,
    pub nvEncSetIOCudaStreams: Fn2,
    pub nvEncGetEncodePresetConfigEx: Option<
        unsafe extern "system" fn(
            *mut c_void,
            Guid,
            Guid,
            u32,
            *mut NvEncPresetConfig,
        ) -> NvencStatus,
    >,
    pub nvEncGetSequenceParamEx: Fn2,
    pub nvEncStoreEncoderState: Fn2,
    pub nvEncLookaheadPicture: Fn2,
    pub reserved2: [*mut c_void; 275],
}

/// The ABI, pinned. These numbers were measured against the reference
/// definitions for API 13.0; if a field is added, removed, or reordered above,
/// one of these stops compiling.
const _: () = {
    assert!(size_of::<Guid>() == 16);
    assert!(size_of::<NvEncQp>() == 12);
    assert!(size_of::<NvEncRcParams>() == 128);
    assert!(size_of::<NvEncConfigH264Vui>() == 112);
    assert!(size_of::<NvEncConfigH264>() == 1792);
    assert!(size_of::<NvEncCodecConfig>() == 1792);
    assert!(size_of::<NvEncConfig>() == 3584);
    assert!(size_of::<NvEncInitializeParams>() == 1800);
    assert!(size_of::<NvEncPresetConfig>() == 5128);
    assert!(size_of::<NvEncCodecPicParams>() == 1544);
    assert!(size_of::<NvEncPicParams>() == 3360);
    assert!(size_of::<NvEncLockBitstream>() == 1544);
    assert!(size_of::<NvEncRegisterResource>() == 1536);
    assert!(size_of::<NvEncMapInputResource>() == 1544);
    assert!(size_of::<NvEncCreateBitstreamBuffer>() == 776);
    assert!(size_of::<NvEncOpenEncodeSessionExParams>() == 1552);
    assert!(size_of::<NvEncodeApiFunctionList>() == 2552);
    // Alignment matters as much as size: an 8-aligned struct starting at a
    // 4-aligned offset would shift every field after it.
    assert!(align_of::<NvEncConfig>() == 8);
    assert!(align_of::<NvEncPicParams>() == 8);
};
