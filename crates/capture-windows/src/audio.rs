//! WASAPI loopback capture of the default render endpoint (spec 07).
//!
//! Loopback taps what the machine is *playing*, so there is no permission to
//! grant and no device to choose. What the audio engine hands back is whatever
//! format the endpoint negotiated — any rate, any channel count — while the
//! Opus pipeline downstream takes exactly 48 kHz interleaved stereo i16. Every
//! conversion between those two therefore happens here.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
    IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator, WAVE_FORMAT_PCM,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eConsole, eRender,
};
use windows::Win32::Media::KernelStreaming::{
    KSDATAFORMAT_SUBTYPE_PCM, SPEAKER_FRONT_CENTER, WAVE_FORMAT_EXTENSIBLE,
};
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};

use gsa_capture_api::{AudioFrame, AudioSink};
use gsa_core::time::MediaClock;
use gsa_core::{Error, Result};

/// What the Opus pipeline requires (spec 07).
const OUT_RATE: u32 = 48_000;
const OUT_CHANNELS: u16 = 2;

/// Endpoint buffer, in 100 ns units. Far larger than a 5 ms poll needs; the
/// surplus only matters when the thread is descheduled.
const BUFFER_DURATION_100NS: i64 = 200_000;

/// Poll period. Loopback's event handle does not fire while the render stream
/// is silent, so we poll the packet queue instead of waiting on it.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// How long `start` waits for WASAPI to accept the stream.
const READY_TIMEOUT: Duration = Duration::from_secs(5);

/// A front-centre channel is folded into both fronts at −3 dB (the ITU
/// downmix coefficient). Dropping it instead would silently delete dialogue.
const CENTER_GAIN: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// Handle to the running loopback thread; stops it on drop.
#[derive(Debug)]
pub(crate) struct AudioCapture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AudioCapture {
    pub(crate) fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start loopback capture on its own thread.
///
/// Blocks until WASAPI has accepted the stream, so a host with no audio
/// endpoint — or one whose mix format we cannot convert — fails here and
/// streams video only, rather than opening an audio pipeline that never
/// produces a sample.
pub(crate) fn start(sink: AudioSink, clock: MediaClock) -> Result<AudioCapture> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<MixFormat>>(1);

    let thread = std::thread::Builder::new()
        .name("gsa-audio-cap".into())
        .spawn(move || run(&sink, &clock, &stop_thread, &ready_tx))
        .map_err(|e| Error::Capture(format!("spawn audio thread: {e}")))?;

    match ready_rx.recv_timeout(READY_TIMEOUT) {
        Ok(Ok(format)) => {
            tracing::info!(
                rate = format.rate,
                channels = format.channels,
                sample = ?format.sample,
                center = ?format.center,
                "WASAPI loopback capture started"
            );
            Ok(AudioCapture {
                stop,
                thread: Some(thread),
            })
        }
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            stop.store(true, Ordering::Release);
            Err(Error::Capture("WASAPI loopback did not start".into()))
        }
    }
}

/// The default render endpoint's mix format, as `(sample_rate, channels)`.
///
/// Opens a real loopback stream and drops it, so `Ok` here means capture will
/// work. Reported by `gsa doctor`.
pub(crate) fn probe() -> Result<(u32, u16)> {
    crate::device::ensure_mta();
    let stream = Stream::open()?;
    Ok((stream.format.rate, stream.format.channels as u16))
}

/// The capture thread: COM apartment, stream, drain loop.
fn run(
    sink: &AudioSink,
    clock: &MediaClock,
    stop: &AtomicBool,
    ready: &SyncSender<Result<MixFormat>>,
) {
    // SAFETY: no preconditions. Balanced by the `CoUninitialize` below, which
    // runs after every COM object opened on this thread has been dropped.
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if let Err(e) = hr.ok() {
        let _ = ready.send(Err(Error::Capture(format!("CoInitializeEx: {e}"))));
        return;
    }
    match Stream::open() {
        Ok(stream) => {
            let _ = ready.send(Ok(stream.format));
            stream.capture(sink, clock, stop);
        }
        Err(e) => {
            let _ = ready.send(Err(e));
        }
    }
    // SAFETY: balances the `CoInitializeEx` above on this thread.
    unsafe { CoUninitialize() };
}

/// An initialized — but not yet started — loopback stream.
#[derive(Debug)]
struct Stream {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    format: MixFormat,
}

impl Stream {
    /// Activate the default render endpoint for loopback capture at the
    /// engine's own mix format. Shared mode cannot negotiate a format, so
    /// whatever it reports is what we must convert from.
    fn open() -> Result<Stream> {
        // SAFETY: standard COM activation; the class is a documented in-proc server.
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|e| Error::Capture(format!("MMDeviceEnumerator: {e}")))?;
        // SAFETY: `enumerator` is live. The *render* endpoint is the one whose
        // output we want to tap.
        let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
            .map_err(|e| Error::Capture(format!("no default render endpoint: {e}")))?;
        // SAFETY: `device` is live; IAudioClient is its documented activation
        // interface and takes no activation parameters.
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
            .map_err(|e| Error::Capture(format!("activate IAudioClient: {e}")))?;

        // SAFETY: `client` is live; the format block is CoTaskMem-allocated
        // and ours to free.
        let wave = unsafe { client.GetMixFormat() }
            .map_err(|e| Error::Capture(format!("GetMixFormat: {e}")))?;
        // SAFETY: `wave` is the engine's own format block, valid until freed.
        let opened = unsafe { MixFormat::parse(wave) }.and_then(|format| {
            // SAFETY: `wave` is a valid format block, and shared mode requires
            // a zero periodicity. `Initialize` copies the format it is given.
            unsafe {
                client.Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK,
                    BUFFER_DURATION_100NS,
                    0,
                    wave,
                    None,
                )
            }
            .map_err(|e| Error::Capture(format!("IAudioClient::Initialize (loopback): {e}")))?;
            Ok(format)
        });
        // SAFETY: `wave` came from `GetMixFormat`, nothing references it now,
        // and this runs whether the parse or the initialize failed.
        unsafe { CoTaskMemFree(Some(wave.cast())) };
        let format = opened?;

        // SAFETY: `client` is initialized; a loopback stream serves the
        // capture interface even though the endpoint renders.
        let capture: IAudioCaptureClient = unsafe { client.GetService() }
            .map_err(|e| Error::Capture(format!("GetService(IAudioCaptureClient): {e}")))?;
        Ok(Stream {
            client,
            capture,
            format,
        })
    }

    /// Run until `stop`, submitting every packet the engine produces.
    fn capture(self, sink: &AudioSink, clock: &MediaClock, stop: &AtomicBool) {
        // SAFETY: `self.client` is initialized and not running.
        if let Err(e) = unsafe { self.client.Start() } {
            tracing::warn!(error = %e, "IAudioClient::Start failed; no audio");
            return;
        }
        let mut resampler = Resampler::new(self.format.rate);
        while !stop.load(Ordering::Acquire) {
            match self.pump(&mut resampler, sink, clock) {
                Ok(0) => std::thread::sleep(POLL_INTERVAL),
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "loopback capture stopped");
                    break;
                }
            }
        }
        // SAFETY: `self.client` was started above.
        let _ = unsafe { self.client.Stop() };
    }

    /// Drain every packet the engine currently holds; returns how many.
    ///
    /// Silence is skipped rather than submitted: the client's Opus concealment
    /// and playback ring cover the gap, and a quiet desktop should not cost
    /// 128 kbps of zeros.
    fn pump(
        &self,
        resampler: &mut Resampler,
        sink: &AudioSink,
        clock: &MediaClock,
    ) -> windows::core::Result<usize> {
        let mut packets = 0;
        loop {
            // SAFETY: `self.capture` is live and its client is started.
            let available = unsafe { self.capture.GetNextPacketSize()? };
            if available == 0 {
                return Ok(packets);
            }
            packets += 1;

            let mut data = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            // SAFETY: three valid out-params. On success `data` addresses
            // `frames * nBlockAlign` readable bytes until `ReleaseBuffer`.
            unsafe {
                self.capture.GetBuffer(
                    &raw mut data,
                    &raw mut frames,
                    &raw mut flags,
                    None,
                    None,
                )?;
            }

            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
            if frames > 0 && !silent {
                let len = frames as usize * self.format.block_align;
                // SAFETY: `data` is non-null and addresses `len` bytes, per the
                // `GetBuffer` contract; the borrow ends before `ReleaseBuffer`.
                let bytes = unsafe { std::slice::from_raw_parts(data, len) };
                self.submit(bytes, frames as usize, resampler, sink, clock);
            }
            // SAFETY: releases exactly the frames `GetBuffer` handed us.
            unsafe { self.capture.ReleaseBuffer(frames)? };
        }
    }

    /// Downmix, resample and hand one packet to the encode pipeline.
    fn submit(
        &self,
        bytes: &[u8],
        frames: usize,
        resampler: &mut Resampler,
        sink: &AudioSink,
        clock: &MediaClock,
    ) {
        let stereo: Vec<[f32; 2]> = (0..frames).map(|f| self.format.stereo(bytes, f)).collect();
        let mut samples = Vec::with_capacity(frames * OUT_CHANNELS as usize);
        resampler.push(&stereo, &mut samples);
        if samples.is_empty() {
            return;
        }
        // The stamp names the packet's first sample, not the moment we noticed
        // it — the engine had already buffered `frames` of them.
        let span_us = frames as u64 * 1_000_000 / u64::from(self.format.rate);
        sink.submit(AudioFrame {
            samples,
            sample_rate: OUT_RATE,
            channels: OUT_CHANNELS,
            capture_ts_us: clock.now_us().saturating_sub(span_us),
        });
    }
}

/// How a sample is stored in the engine's buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sample {
    F32,
    I16,
}

impl Sample {
    fn size(self) -> usize {
        match self {
            Self::F32 => 4,
            Self::I16 => 2,
        }
    }

    fn bits(self) -> u16 {
        (self.size() * 8) as u16
    }

    /// The `i`th sample of `data`, normalized to −1.0..=1.0.
    fn read(self, data: &[u8], i: usize) -> f32 {
        let at = i * self.size();
        match self {
            Self::F32 => f32::from_le_bytes(data[at..at + 4].try_into().expect("4 bytes")),
            Self::I16 => {
                f32::from(i16::from_le_bytes(
                    data[at..at + 2].try_into().expect("2 bytes"),
                )) / 32768.0
            }
        }
    }
}

/// The engine's mix format, reduced to what the conversion needs.
#[derive(Debug, Clone, Copy)]
struct MixFormat {
    rate: u32,
    channels: usize,
    /// Bytes per frame, across all channels.
    block_align: usize,
    sample: Sample,
    /// Index of the front-centre channel, when the mask names one.
    center: Option<usize>,
}

impl MixFormat {
    /// Reduce a `WAVEFORMATEX` to the fields the conversion needs.
    ///
    /// # Safety
    /// `wave` must point at a valid `WAVEFORMATEX` with its declared extension
    /// bytes; an extensible tag means the block is really a
    /// `WAVEFORMATEXTENSIBLE`.
    unsafe fn parse(wave: *const WAVEFORMATEX) -> Result<MixFormat> {
        // `WAVEFORMATEX` is `packed(1)`: every field is copied out before use,
        // because borrowing one — as `format!` would — is undefined behaviour.
        // SAFETY: the caller guarantees `wave` is a valid format block, and a
        // packed struct needs no alignment beyond the byte.
        let base = unsafe { *wave };
        let (tag, bits) = (u32::from(base.wFormatTag), base.wBitsPerSample);
        let (rate, channels, block_align) = (
            base.nSamplesPerSec,
            base.nChannels as usize,
            base.nBlockAlign as usize,
        );

        let (sample, center) = if tag == WAVE_FORMAT_EXTENSIBLE {
            // SAFETY: an extensible tag means `cbSize >= 22` and the block is a
            // `WAVEFORMATEXTENSIBLE`, per the format's own contract.
            let ext = unsafe { *wave.cast::<WAVEFORMATEXTENSIBLE>() };
            (subformat(ext.SubFormat)?, center_index(ext.dwChannelMask))
        } else {
            (tagged(tag)?, None)
        };

        if channels == 0 || rate == 0 {
            return Err(Error::Capture(
                "audio endpoint reports an empty format".into(),
            ));
        }
        if bits != sample.bits() {
            return Err(Error::Capture(format!(
                "audio endpoint is {bits} bit {sample:?}, which is not a sample size we read"
            )));
        }
        // Every read below indexes `frame * channels + ch` into a slice sized
        // `frames * block_align`, so the two must agree or those reads overrun.
        if block_align != channels * sample.size() {
            return Err(Error::Capture(format!(
                "audio endpoint packs {channels} channels into {block_align} bytes"
            )));
        }
        Ok(MixFormat {
            rate,
            channels,
            block_align,
            sample,
            center,
        })
    }

    /// Fold one interleaved source frame down to stereo.
    fn stereo(&self, data: &[u8], frame: usize) -> [f32; 2] {
        let at = |ch: usize| self.sample.read(data, frame * self.channels + ch);
        if self.channels == 1 {
            let mono = at(0);
            return [mono, mono];
        }
        let mut out = [at(0), at(1)];
        // A centre below index 2 would *be* one of the fronts we already took.
        if let Some(center) = self.center.filter(|c| (2..self.channels).contains(c)) {
            let fold = at(center) * CENTER_GAIN;
            out[0] += fold;
            out[1] += fold;
        }
        out
    }
}

/// Map an extensible subformat GUID to a sample layout.
fn subformat(guid: windows::core::GUID) -> Result<Sample> {
    if guid == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
        Ok(Sample::F32)
    } else if guid == KSDATAFORMAT_SUBTYPE_PCM {
        Ok(Sample::I16)
    } else {
        Err(Error::Capture(format!(
            "audio endpoint mix subformat {guid:?} is neither float nor PCM"
        )))
    }
}

/// Map a plain `wFormatTag` to a sample layout.
fn tagged(tag: u32) -> Result<Sample> {
    match tag {
        WAVE_FORMAT_IEEE_FLOAT => Ok(Sample::F32),
        WAVE_FORMAT_PCM => Ok(Sample::I16),
        _ => Err(Error::Capture(format!(
            "audio endpoint mix format tag {tag} is neither float nor PCM"
        ))),
    }
}

/// Where the front-centre channel sits in an interleaved frame.
///
/// Channels appear in ascending channel-mask bit order, so the centre's index
/// is simply the number of speakers named below it.
fn center_index(mask: u32) -> Option<usize> {
    (mask & SPEAKER_FRONT_CENTER != 0)
        .then(|| (mask & (SPEAKER_FRONT_CENTER - 1)).count_ones() as usize)
}

/// Float sample (−1.0..1.0) → i16.
fn f32_to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * 32767.0) as i16
}

/// Linear-interpolating stereo resampler to [`OUT_RATE`].
///
/// The state carries across packets on purpose: interpolating each packet from
/// silence, rather than from its predecessor's last frame, would click at every
/// packet boundary. At 48 kHz in, `step` is 1.0 and this is an exact copy.
#[derive(Debug)]
struct Resampler {
    /// Source frames consumed per output frame.
    step: f64,
    /// Read position over the virtual buffer `[carry] ++ input`, in source frames.
    pos: f64,
    /// The previous packet's last frame, which the next one interpolates from.
    carry: [f32; 2],
}

impl Resampler {
    fn new(in_rate: u32) -> Self {
        Self {
            step: f64::from(in_rate) / f64::from(OUT_RATE),
            // The first packet has no predecessor, so start past the (silent)
            // carry rather than blending with it.
            pos: 1.0,
            carry: [0.0; 2],
        }
    }

    /// Resample `input`, appending interleaved i16 to `out`.
    fn push(&mut self, input: &[[f32; 2]], out: &mut Vec<i16>) {
        let n = input.len();
        if n == 0 {
            return;
        }
        let carry = self.carry;
        let at = |k: usize| if k == 0 { carry } else { input[k - 1] };
        // `pos < n` keeps `i` in 0..n, so `at(i + 1)` reads at most `input[n-1]`.
        while self.pos < n as f64 {
            let i = self.pos as usize;
            let frac = (self.pos - i as f64) as f32;
            let (a, b) = (at(i), at(i + 1));
            out.push(f32_to_i16(a[0] + (b[0] - a[0]) * frac));
            out.push(f32_to_i16(a[1] + (b[1] - a[1]) * frac));
            self.pos += self.step;
        }
        // `pos` was < n before the last step, so it lands in 0..step: the next
        // packet picks up exactly where this one left off.
        self.pos -= n as f64;
        self.carry = input[n - 1];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Standard 5.1 mask: FL FR FC LFE BL BR.
    const MASK_5_1: u32 = 0x3F;
    const MASK_STEREO: u32 = 0x3;

    fn f32_bytes(samples: &[f32]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    fn format(channels: usize, center: Option<usize>) -> MixFormat {
        MixFormat {
            rate: 48_000,
            channels,
            block_align: channels * 4,
            sample: Sample::F32,
            center,
        }
    }

    #[test]
    fn center_is_the_third_channel_of_a_5_1_mix() {
        assert_eq!(center_index(MASK_5_1), Some(2));
        assert_eq!(center_index(MASK_STEREO), None);
        // Quadraphonic (FL FR BL BR) names no centre.
        assert_eq!(center_index(0x33), None);
    }

    #[test]
    fn stereo_passes_through_and_mono_duplicates() {
        let data = f32_bytes(&[0.25, -0.5]);
        assert_eq!(format(2, None).stereo(&data, 0), [0.25, -0.5]);

        let data = f32_bytes(&[0.75]);
        assert_eq!(format(1, None).stereo(&data, 0), [0.75, 0.75]);
    }

    #[test]
    fn surround_folds_the_centre_and_drops_the_rest() {
        // FL FR FC LFE BL BR
        let data = f32_bytes(&[0.1, 0.2, 0.4, 0.9, 0.9, 0.9]);
        let [l, r] = format(6, Some(2)).stereo(&data, 0);
        assert!((l - (0.1 + 0.4 * CENTER_GAIN)).abs() < 1e-6);
        assert!((r - (0.2 + 0.4 * CENTER_GAIN)).abs() < 1e-6);
    }

    #[test]
    fn a_centre_that_is_already_a_front_is_not_folded_twice() {
        let data = f32_bytes(&[0.3, 0.4]);
        assert_eq!(format(2, Some(1)).stereo(&data, 0), [0.3, 0.4]);
    }

    #[test]
    fn reading_the_second_frame_skips_a_whole_block() {
        let data = f32_bytes(&[0.1, 0.2, 0.3, 0.4]);
        assert_eq!(format(2, None).stereo(&data, 1), [0.3, 0.4]);
    }

    #[test]
    fn i16_samples_normalize_to_unit_range() {
        let data: Vec<u8> = [i16::MIN, 0, i16::MAX]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        assert_eq!(Sample::I16.read(&data, 0), -1.0);
        assert_eq!(Sample::I16.read(&data, 1), 0.0);
        assert!((Sample::I16.read(&data, 2) - 1.0).abs() < 1e-4);
    }

    /// At the output rate the resampler must not touch the samples, and must
    /// emit each source frame exactly once across packet boundaries.
    #[test]
    fn at_48k_every_frame_survives_exactly_once() {
        let mut r = Resampler::new(48_000);
        let mut out = Vec::new();
        let a: Vec<[f32; 2]> = (0..4).map(|i| [i as f32 / 8.0, 0.0]).collect();
        let b: Vec<[f32; 2]> = (4..8).map(|i| [i as f32 / 8.0, 0.0]).collect();
        r.push(&a, &mut out);
        r.push(&b, &mut out);

        // The last frame of `b` is still held as carry, so 7 of 8 have emerged.
        let left: Vec<i16> = out.chunks(2).map(|f| f[0]).collect();
        let want: Vec<i16> = (0..7).map(|i| f32_to_i16(i as f32 / 8.0)).collect();
        assert_eq!(left, want);
    }

    /// 44.1 kHz in must come out longer, monotonic, and free of the drop to
    /// zero that a stateless resampler would produce at each packet boundary.
    #[test]
    fn resampling_44k1_stretches_without_a_boundary_click() {
        let mut r = Resampler::new(44_100);
        let mut out = Vec::new();
        // A ramp split across two packets: any boundary discontinuity shows up
        // as a non-monotonic step.
        let ramp: Vec<[f32; 2]> = (0..441).map(|i| [i as f32 / 441.0, 0.0]).collect();
        r.push(&ramp[..220], &mut out);
        let first = out.len();
        r.push(&ramp[220..], &mut out);

        assert!(first > 0, "the first packet must produce output");
        let left: Vec<i16> = out.chunks(2).map(|f| f[0]).collect();
        assert!(
            left.windows(2).all(|w| w[1] >= w[0]),
            "ramp stayed monotonic"
        );
        // 441 source frames at 44.1 kHz is 10 ms, i.e. ~480 frames at 48 kHz.
        assert!(
            (left.len() as i64 - 480).abs() <= 2,
            "got {} frames",
            left.len()
        );
    }

    #[test]
    fn a_silent_packet_produces_no_samples() {
        let mut r = Resampler::new(48_000);
        let mut out = Vec::new();
        r.push(&[], &mut out);
        assert!(out.is_empty());
    }
}
