//! Audio playback (dev harness): pull decoded interleaved-i16 PCM from
//! client-core's output channel into a bounded ring, and feed it to the default
//! output device via cpal. Handles f32/i16 device formats and assumes a 48 kHz
//! stereo device (Opus's rate); resampling is a later concern.

use std::collections::VecDeque;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
/// Cap buffered audio (~200 ms) so playback stays close to real time.
const MAX_BUFFERED: usize = SAMPLE_RATE as usize * CHANNELS as usize / 5;

type Ring = Arc<Mutex<VecDeque<i16>>>;

/// Keeps the output stream (and thus playback) alive for its lifetime.
pub struct AudioPlayback {
    _stream: cpal::Stream,
}

/// Start playing PCM arriving on `rx` through the default output device.
pub fn start(rx: Receiver<Vec<i16>>) -> Result<AudioPlayback> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .context("no default audio output device")?;
    let default_cfg = device
        .default_output_config()
        .context("no default output config")?;
    tracing::info!(
        device = device.name().unwrap_or_default(),
        device_rate = default_cfg.sample_rate().0,
        format = ?default_cfg.sample_format(),
        "audio output"
    );

    let ring: Ring = Arc::new(Mutex::new(VecDeque::new()));

    // Feeder: move decoded PCM into the ring, dropping oldest to bound latency.
    let feed = ring.clone();
    std::thread::Builder::new()
        .name("gsa-audio-play".into())
        .spawn(move || {
            while let Ok(pcm) = rx.recv() {
                let mut b = feed.lock().expect("audio ring");
                b.extend(pcm);
                let overflow = b.len().saturating_sub(MAX_BUFFERED);
                drop(b.drain(..overflow));
            }
        })
        .context("spawn audio feeder")?;

    let config = cpal::StreamConfig {
        channels: CHANNELS,
        sample_rate: cpal::SampleRate(SAMPLE_RATE),
        buffer_size: cpal::BufferSize::Default,
    };
    let err_fn = |e| tracing::warn!(error = %e, "audio stream error");
    let cb = ring.clone();
    let stream = match default_cfg.sample_format() {
        cpal::SampleFormat::I16 => device.build_output_stream(
            &config,
            move |out: &mut [i16], _| {
                let mut b = cb.lock().expect("audio ring");
                for s in out.iter_mut() {
                    *s = b.pop_front().unwrap_or(0);
                }
            },
            err_fn,
            None,
        )?,
        cpal::SampleFormat::F32 => device.build_output_stream(
            &config,
            move |out: &mut [f32], _| {
                let mut b = cb.lock().expect("audio ring");
                for s in out.iter_mut() {
                    *s = b.pop_front().map_or(0.0, |v| f32::from(v) / 32768.0);
                }
            },
            err_fn,
            None,
        )?,
        other => anyhow::bail!("unsupported output sample format {other:?}"),
    };
    stream.play().context("start audio stream")?;
    Ok(AudioPlayback { _stream: stream })
}
