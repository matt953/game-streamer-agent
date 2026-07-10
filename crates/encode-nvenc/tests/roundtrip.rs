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

/// Annex-B NAL unit types, in order. 7 = SPS, 8 = PPS, 5 = IDR, 1 = non-IDR.
fn nal_types(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= data.len() {
        let four = data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 && data[i + 3] == 1;
        let three = data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1;
        if four {
            out.push(data[i + 4] & 0x1F);
            i += 5;
        } else if three {
            out.push(data[i + 3] & 0x1F);
            i += 4;
        } else {
            i += 1;
        }
    }
    out
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
    let types = nal_types(&idr.data);
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
    let types = nal_types(&p.data);
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
    let types = nal_types(&forced.data);
    assert!(
        types.contains(&7) && types.contains(&8),
        "a recovering client needs SPS/PPS on every IDR; got {types:?}"
    );
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
