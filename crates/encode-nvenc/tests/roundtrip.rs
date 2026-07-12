//! Encode a known image with NVENC and decode it with the reference decoder.
//!
//! This is the only check that can catch a swapped colour channel, a wrong
//! `NV_ENC_BUFFER_FORMAT`, or a rate-control config that never took — none of
//! which produce an error, only a wrong picture. It also pins the two stream
//! properties the client depends on: the first frame is an IDR carrying SPS
//! and PPS, and subsequent frames are P-frames (no automatic GOP).
//!
//! Skips itself where there is no NVENC, so CI's GPU-less Windows runner
//! compiles it and moves on.

#![cfg(windows)]

use std::sync::Arc;

use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_SUBRESOURCE_DATA,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

use gsa_capture_api::{GpuFrame, GpuHandle, PlatformFrame};
use gsa_capture_windows::D3D11Frame;
use gsa_core::media::{Codec, FrameKind, H264Profile, PixelFormat, VideoMode};
use gsa_core::time::MediaClock;
use gsa_encode_api::{EncodeConfig, Encoder, FrameDirectives};
use gsa_encode_nvenc::NvencEncoder;

const W: u32 = 640;
const H: u32 = 480;

/// Left half pure blue, right half pure red, in BGRA byte order.
fn bgra_halves() -> Vec<u8> {
    let mut buf = vec![0u8; (W * H * 4) as usize];
    for y in 0..H {
        for x in 0..W {
            let p = ((y * W + x) * 4) as usize;
            if x < W / 2 {
                buf[p] = 255; // B
            } else {
                buf[p + 2] = 255; // R
            }
            buf[p + 3] = 255; // A
        }
    }
    buf
}

fn texture(device: &ID3D11Device, pixels: &[u8]) -> ID3D11Texture2D {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: W,
        Height: H,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let init = D3D11_SUBRESOURCE_DATA {
        pSysMem: pixels.as_ptr().cast(),
        SysMemPitch: W * 4,
        SysMemSlicePitch: 0,
    };
    let mut texture = None;
    // SAFETY: `desc` and `init` describe the same WxH BGRA image.
    unsafe {
        device
            .CreateTexture2D(&desc, Some(&init), Some(&mut texture))
            .expect("CreateTexture2D");
    }
    texture.expect("texture")
}

fn frame(device: &ID3D11Device, texture: ID3D11Texture2D, ts: u64) -> GpuFrame {
    GpuFrame {
        handle: GpuHandle::Platform(
            Arc::new(D3D11Frame::new(texture, device.clone())) as Arc<dyn PlatformFrame>
        ),
        format: PixelFormat::Bgra8,
        width: W,
        height: H,
        capture_ts_us: ts,
        dirty_rects: None,
    }
}

/// Annex-B NAL unit types, in order, extracted with `header`. H.264 packs the
/// type in the low 5 bits of the byte after the start code; HEVC uses bits 1..6
/// of a two-byte header. Passing the extractor keeps one start-code scanner.
fn nal_types(data: &[u8], header: fn(&[u8]) -> u8) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= data.len() {
        let four = data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 && data[i + 3] == 1;
        let three = data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1;
        if four {
            out.push(header(&data[i + 4..]));
            i += 5;
        } else if three {
            out.push(header(&data[i + 3..]));
            i += 4;
        } else {
            i += 1;
        }
    }
    out
}

/// H.264: 7 = SPS, 8 = PPS, 5 = IDR, 1 = non-IDR.
fn h264_nal(rest: &[u8]) -> u8 {
    rest[0] & 0x1F
}

/// HEVC: type is bits 1..6 of the first header byte. 32 = VPS, 33 = SPS,
/// 34 = PPS, 19/20 = IDR, <32 non-IDR VCL.
fn hevc_nal(rest: &[u8]) -> u8 {
    (rest[0] >> 1) & 0x3F
}

fn mean(
    plane: &[u8],
    stride: usize,
    rows: std::ops::Range<usize>,
    cols: std::ops::Range<usize>,
) -> f64 {
    let mut sum = 0u64;
    let mut n = 0u64;
    for y in rows {
        for x in cols.clone() {
            sum += u64::from(plane[y * stride + x]);
            n += 1;
        }
    }
    sum as f64 / n as f64
}

#[test]
fn encodes_a_d3d11_texture_that_decodes_with_the_right_colours() {
    let Some(support) = gsa_encode_nvenc::probe() else {
        eprintln!("no NVENC on this machine; skipping");
        return;
    };
    let device = gsa_capture_windows::create_device_on(support.adapter_luid).expect("d3d11 device");

    let mode = VideoMode {
        width: W,
        height: H,
        fps: 60,
    };
    let mut enc = NvencEncoder::new(MediaClock::new());
    enc.open(EncodeConfig {
        codec: Codec::H264,
        mode,
        bitrate_bps: 20_000_000,
        // High so NVENC uses CABAC; openh264 decodes it.
        h264_profile: H264Profile::High,
    })
    .expect("open");

    let pixels = bgra_halves();

    // ---- frame 0: must be an IDR, and must carry SPS + PPS ----
    enc.submit(
        &frame(&device, texture(&device, &pixels), 0),
        FrameDirectives::default(),
    )
    .expect("submit idr");
    let idr = enc.poll_bitstream().expect("poll").expect("a chunk");
    assert_eq!(idr.kind, FrameKind::Idr, "first frame must be an IDR");
    let types = nal_types(&idr.data, h264_nal);
    assert!(
        types.contains(&7) && types.contains(&8) && types.contains(&5),
        "IDR must carry SPS(7), PPS(8) and an IDR slice(5); got {types:?}"
    );

    // ---- colours survive the BGRA -> NV12 -> H.264 -> I420 trip ----
    let mut dec = Decoder::new().expect("openh264");
    let yuv = dec
        .decode(&idr.data)
        .expect("decode")
        .expect("a decoded frame");
    let (dw, dh) = yuv.dimensions();
    assert_eq!((dw as u32, dh as u32), (W, H));

    let (_, ustride, _) = yuv.strides();
    let (cw, ch) = (dw / 2, dh / 2);
    let rows = ch / 4..ch * 3 / 4;
    let left = 2..cw / 2 - 2;
    let right = cw / 2 + 2..cw - 2;

    let lu = mean(yuv.u(), ustride, rows.clone(), left.clone());
    let lv = mean(yuv.v(), ustride, rows.clone(), left);
    let ru = mean(yuv.u(), ustride, rows.clone(), right.clone());
    let rv = mean(yuv.v(), ustride, rows, right);

    // Blue is high Cb / low Cr; red is the reverse. A channel swap inverts both.
    assert!(
        lu > 160.0 && lv < 128.0,
        "left half should decode blue, got U={lu:.1} V={lv:.1}"
    );
    assert!(
        ru < 128.0 && rv > 160.0,
        "right half should decode red, got U={ru:.1} V={rv:.1}"
    );

    // ---- frame 1: no automatic GOP, so this must be a P-frame ----
    enc.submit(
        &frame(&device, texture(&device, &pixels), 16_666),
        FrameDirectives::default(),
    )
    .expect("submit p");
    let p = enc.poll_bitstream().expect("poll").expect("a chunk");
    assert_eq!(p.kind, FrameKind::P, "second frame must not be an IDR");
    let types = nal_types(&p.data, h264_nal);
    assert!(
        !types.contains(&5),
        "infinite GOP means no unrequested IDR; got {types:?}"
    );
    assert_eq!(p.frame_id, idr.frame_id.next());

    // ---- an explicit keyframe request produces a fresh IDR with headers ----
    enc.force_idr();
    enc.submit(
        &frame(&device, texture(&device, &pixels), 33_333),
        FrameDirectives::default(),
    )
    .expect("submit forced idr");
    let forced = enc.poll_bitstream().expect("poll").expect("a chunk");
    assert_eq!(forced.kind, FrameKind::Idr);
    let types = nal_types(&forced.data, h264_nal);
    assert!(
        types.contains(&7) && types.contains(&8),
        "a recovering client needs SPS/PPS on every IDR; got {types:?}"
    );
}

/// HEVC has no reference decoder here (openh264 is H.264-only), so this proves
/// what a decoder-free test can: `initialize` accepts the HEVC path, it emits
/// Annex-B chunks, VPS/SPS/PPS ride each IDR in-band, and the IDR vs P mapping
/// the client gates on is intact. The BGRA→NV12 GPU path itself is codec-
/// agnostic and already colour-verified by the H.264 test above.
#[test]
fn hevc_initializes_and_emits_parameter_sets_on_every_idr() {
    let Some(support) = gsa_encode_nvenc::probe() else {
        eprintln!("no NVENC on this machine; skipping");
        return;
    };
    let device = gsa_capture_windows::create_device_on(support.adapter_luid).expect("d3d11 device");

    let mode = VideoMode {
        width: W,
        height: H,
        fps: 60,
    };
    let mut enc = NvencEncoder::new(MediaClock::new());
    enc.open(EncodeConfig {
        codec: Codec::Hevc,
        mode,
        bitrate_bps: 20_000_000,
        // Ignored on the HEVC path (Main is forced); set to a real value.
        h264_profile: H264Profile::High,
    })
    .expect("open hevc");

    let pixels = bgra_halves();

    // ---- frame 0: IDR carrying VPS(32) + SPS(33) + PPS(34) + an IDR slice ----
    enc.submit(
        &frame(&device, texture(&device, &pixels), 0),
        FrameDirectives::default(),
    )
    .expect("submit idr");
    let idr = enc.poll_bitstream().expect("poll").expect("a chunk");
    assert_eq!(idr.kind, FrameKind::Idr, "first HEVC frame must be an IDR");
    let types = nal_types(&idr.data, hevc_nal);
    assert!(
        types.contains(&32)
            && types.contains(&33)
            && types.contains(&34)
            && types.iter().any(|&t| t == 19 || t == 20),
        "HEVC IDR must carry VPS(32), SPS(33), PPS(34) and an IDR slice(19/20); got {types:?}"
    );

    // ---- frame 1: infinite GOP, so no unrequested IDR ----
    enc.submit(
        &frame(&device, texture(&device, &pixels), 16_666),
        FrameDirectives::default(),
    )
    .expect("submit p");
    let p = enc.poll_bitstream().expect("poll").expect("a chunk");
    assert_eq!(p.kind, FrameKind::P, "second HEVC frame must not be an IDR");
    let types = nal_types(&p.data, hevc_nal);
    assert!(
        !types.iter().any(|&t| t == 19 || t == 20),
        "infinite GOP means no unrequested IDR; got {types:?}"
    );
    assert_eq!(p.frame_id, idr.frame_id.next());

    // ---- a forced keyframe carries the parameter sets again ----
    enc.force_idr();
    enc.submit(
        &frame(&device, texture(&device, &pixels), 33_333),
        FrameDirectives::default(),
    )
    .expect("submit forced idr");
    let forced = enc.poll_bitstream().expect("poll").expect("a chunk");
    assert_eq!(forced.kind, FrameKind::Idr);
    let types = nal_types(&forced.data, hevc_nal);
    assert!(
        types.contains(&32) && types.contains(&33) && types.contains(&34),
        "a recovering client needs VPS/SPS/PPS on every IDR; got {types:?}"
    );
}

/// ABR moves the bitrate every few hundred milliseconds. Each step must
/// reconfigure the live session, never reopen it: a forced keyframe is a fat
/// frame injected exactly when ABR is trying to shed bits. The observable
/// contract is that the frame after `update_rate` is still a P-frame, with no
/// break in the frame ids.
#[test]
fn a_bitrate_change_emits_no_keyframe() {
    let Some(support) = gsa_encode_nvenc::probe() else {
        eprintln!("no NVENC on this machine; skipping");
        return;
    };
    let device = gsa_capture_windows::create_device_on(support.adapter_luid).expect("d3d11 device");

    let mode = VideoMode {
        width: W,
        height: H,
        fps: 60,
    };
    let mut enc = NvencEncoder::new(MediaClock::new());
    enc.open(EncodeConfig {
        codec: Codec::H264,
        mode,
        bitrate_bps: 20_000_000,
        h264_profile: H264Profile::High,
    })
    .expect("open");

    let pixels = bgra_halves();
    let submit = |enc: &mut NvencEncoder, ts: u64| {
        enc.submit(
            &frame(&device, texture(&device, &pixels), ts),
            FrameDirectives::default(),
        )
        .expect("submit");
        enc.poll_bitstream().expect("poll").expect("a chunk")
    };

    // Before the session exists there is nothing to reconfigure; the rate is
    // stored and the session opens at it.
    enc.update_rate(15_000_000)
        .expect("update_rate while closed");

    let idr = submit(&mut enc, 0);
    assert_eq!(idr.kind, FrameKind::Idr, "first frame must be an IDR");
    let p = submit(&mut enc, 16_666);
    assert_eq!(p.kind, FrameKind::P);

    // ---- the step ABR actually makes under congestion: cut the rate ----
    enc.update_rate(3_000_000).expect("update_rate down");
    let after = submit(&mut enc, 33_333);
    assert_eq!(
        after.kind,
        FrameKind::P,
        "a bitrate cut must not force a keyframe"
    );
    assert!(
        !nal_types(&after.data, h264_nal).contains(&5),
        "no IDR slice may appear after a bitrate change"
    );
    assert_eq!(
        after.frame_id,
        p.frame_id.next(),
        "reconfigure must not restart the stream"
    );

    // ---- repeating a rate is a no-op, and recovering upwards also holds ----
    enc.update_rate(3_000_000).expect("update_rate unchanged");
    enc.update_rate(25_000_000).expect("update_rate up");
    let recovered = submit(&mut enc, 50_000);
    assert_eq!(recovered.kind, FrameKind::P);
    assert_eq!(recovered.frame_id, after.frame_id.next());
}

#[test]
fn rejects_a_cpu_frame() {
    if gsa_encode_nvenc::probe().is_none() {
        return;
    }
    use gsa_capture_api::CpuFrame;

    let mut enc = NvencEncoder::new(MediaClock::new());
    enc.open(EncodeConfig {
        codec: Codec::H264,
        mode: VideoMode {
            width: W,
            height: H,
            fps: 60,
        },
        bitrate_bps: 10_000_000,
        h264_profile: H264Profile::High,
    })
    .expect("open");

    let cpu = GpuFrame {
        handle: GpuHandle::Cpu(CpuFrame {
            data: Arc::new(vec![0; (W * H * 4) as usize]),
            stride: (W * 4) as usize,
        }),
        format: PixelFormat::Bgra8,
        width: W,
        height: H,
        capture_ts_us: 0,
        dirty_rects: None,
    };
    assert!(
        enc.submit(&cpu, FrameDirectives::default()).is_err(),
        "nvenc must refuse a CPU frame rather than silently misencode"
    );
}
