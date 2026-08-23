//! De-interleave decoded PCM into one WAV file per channel.
//!
//! The listening tool for a surround probe: which channel carries what is
//! the entire question, and a per-channel file answers it in any audio
//! player. Never routed to speakers — an experimental layout decoded wrongly
//! is violent noise, and that mistake gets made exactly once.

use std::io::{Seek, SeekFrom, Write};

/// One WAV per channel, headers patched on drop.
pub struct WavDump {
    files: Vec<std::fs::File>,
    /// Samples written per channel, for the header patch.
    samples: u64,
    channels: usize,
}

impl std::fmt::Debug for WavDump {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WavDump")
            .field("channels", &self.channels)
            .field("samples", &self.samples)
            .finish_non_exhaustive()
    }
}

const SAMPLE_RATE: u32 = 48_000;

impl WavDump {
    /// Create `channels` files under `dir`, named `audio_ch<N>.wav`.
    ///
    /// # Errors
    /// The directory or a file cannot be created.
    pub fn new(dir: &std::path::Path, channels: usize) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let mut files = Vec::with_capacity(channels);
        for ch in 0..channels {
            let mut file = std::fs::File::create(dir.join(format!("audio_ch{ch}.wav")))?;
            // Placeholder sizes; patched when the dump closes.
            file.write_all(&wav_header(0))?;
            files.push(file);
        }
        tracing::info!(?dir, channels, "dumping decoded audio per channel");
        Ok(Self {
            files,
            samples: 0,
            channels,
        })
    }

    /// Append one interleaved PCM chunk.
    ///
    /// # Errors
    /// A write fails; the dump is abandoned rather than left inconsistent.
    pub fn push(&mut self, interleaved: &[i16]) -> std::io::Result<()> {
        let frames = interleaved.len() / self.channels;
        for (ch, file) in self.files.iter_mut().enumerate() {
            let mut plane = Vec::with_capacity(frames * 2);
            for frame in 0..frames {
                plane.extend_from_slice(&interleaved[frame * self.channels + ch].to_le_bytes());
            }
            file.write_all(&plane)?;
        }
        self.samples += frames as u64;
        Ok(())
    }
}

impl Drop for WavDump {
    fn drop(&mut self) {
        #[allow(clippy::cast_possible_truncation)]
        let data = (self.samples * 2).min(u64::from(u32::MAX)) as u32;
        for file in &mut self.files {
            let _ = file.seek(SeekFrom::Start(0));
            let _ = file.write_all(&wav_header(data));
        }
        tracing::info!(samples = self.samples, "audio dump closed");
    }
}

/// A 44-byte mono 16-bit PCM WAV header for `data_len` bytes of samples.
fn wav_header(data_len: u32) -> [u8; 44] {
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data_len).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    h[22..24].copy_from_slice(&1u16.to_le_bytes()); // mono
    h[24..28].copy_from_slice(&SAMPLE_RATE.to_le_bytes());
    h[28..32].copy_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    h[32..34].copy_from_slice(&2u16.to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_len.to_le_bytes());
    h
}

#[cfg(test)]
mod tests {
    use super::WavDump;

    /// The de-interleave must put channel N's samples in file N, or every
    /// conclusion drawn from listening is about the wrong channel.
    #[test]
    fn channels_land_in_their_own_files() {
        let dir = std::env::temp_dir().join(format!("gsa-wav-test-{}", std::process::id()));
        {
            let mut dump = WavDump::new(&dir, 2).unwrap();
            // ch0 = 100s, ch1 = 200s.
            dump.push(&[100, 200, 100, 200]).unwrap();
        }
        let ch0 = std::fs::read(dir.join("audio_ch0.wav")).unwrap();
        let ch1 = std::fs::read(dir.join("audio_ch1.wav")).unwrap();
        assert_eq!(&ch0[44..48], &[100, 0, 100, 0]);
        assert_eq!(&ch1[44..48], &[200, 0, 200, 0]);
        assert_eq!(&ch0[40..44], &4u32.to_le_bytes());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
