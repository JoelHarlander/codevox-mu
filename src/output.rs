//! Play decoded audio on the default output device.
//!
//! [rodio](https://crates.io/crates/rodio) 0.23 is MIT OR Apache-2.0. It
//! decodes with symphonia and writes samples through
//! [cpal](https://crates.io/crates/cpal): WASAPI on Windows, CoreAudio on
//! macOS, and ALSA on Linux. Symphonia is MPL-2.0, a file-level weak
//! copyleft, not GPL. rodio's MIT and Apache decoders do not read the AAC
//! inside the MP4 container YouTube serves, so symphonia is the decoder.
//!
//! Opening the device happens on the first play, on the playback thread.
//! `--smoke` never constructs a [`RodioSink`].

#[cfg(test)]
use std::fs::File;
use std::io::Cursor;
#[cfg(test)]
use std::io::Write;
#[cfg(test)]
use std::path::Path;
use std::time::Duration;

#[cfg(test)]
use rodio::Source;

use crate::extract::AudioSource;

/// The device could not be opened, or the bytes were not audio we can read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SinkError {
    Device(String),
    Decode(String),
}

impl std::fmt::Display for SinkError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SinkError::Device(message) | SinkError::Decode(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for SinkError {}

/// What the player asks the speakers to do.
pub trait AudioSink {
    fn start(&mut self, source: AudioSource) -> Result<(), SinkError>;
    fn pause(&mut self);
    fn resume(&mut self);
    fn stop(&mut self);
    fn elapsed(&self) -> Duration;
    fn finished(&self) -> bool;
}

/// cpal output. The stream stays open until this value is dropped.
pub struct RodioSink {
    output: Option<rodio::MixerDeviceSink>,
    player: Option<rodio::Player>,
    playing: bool,
    paused: bool,
}

impl RodioSink {
    pub fn new() -> Self {
        Self {
            output: None,
            player: None,
            playing: false,
            paused: false,
        }
    }
}

impl AudioSink for RodioSink {
    fn start(&mut self, source: AudioSource) -> Result<(), SinkError> {
        let decoded = open_decoder(Cursor::new(source.bytes), &source.mime)?;
        if self.output.is_none() {
            let mut output = rodio::DeviceSinkBuilder::open_default_sink()
                .map_err(|error| SinkError::Device(error.to_string()))?;
            // Closing the window drops the stream on purpose.
            output.log_on_drop(false);
            self.output = Some(output);
        }

        // Drop the previous track before borrowing the mixer. `connect_new`
        // only needs the mixer for the call; the player keeps its own queue.
        self.player = None;
        let player = {
            let mixer = self
                .output
                .as_ref()
                .ok_or_else(|| SinkError::Device("audio output was not opened".into()))?
                .mixer();
            rodio::Player::connect_new(mixer)
        };
        player.append(decoded);
        self.player = Some(player);
        self.playing = true;
        self.paused = false;
        Ok(())
    }

    fn pause(&mut self) {
        if self.playing && !self.paused {
            if let Some(player) = &self.player {
                player.pause();
            }
            self.paused = true;
        }
    }

    fn resume(&mut self) {
        if self.paused {
            if let Some(player) = &self.player {
                player.play();
            }
            self.paused = false;
        }
    }

    fn stop(&mut self) {
        self.player = None;
        self.playing = false;
        self.paused = false;
    }

    fn elapsed(&self) -> Duration {
        if self.playing || self.paused {
            self.player
                .as_ref()
                .map(rodio::Player::get_pos)
                .unwrap_or_default()
        } else {
            Duration::ZERO
        }
    }

    fn finished(&self) -> bool {
        self.playing && !self.paused && self.player.as_ref().is_some_and(rodio::Player::empty)
    }
}

/// Samples from the start of `bytes`, stopped after `limit`.
///
/// This does not open an audio device. The ignored live test uses it as a
/// null sink, then writes the same samples to a wav. Playback itself
/// decodes inside the rodio player.
#[cfg(test)]
pub fn decode_prefix(bytes: &[u8], mime: &str, limit: Duration) -> Result<DecodedPrefix, String> {
    let decoder =
        open_decoder(Cursor::new(bytes.to_vec()), mime).map_err(|error| error.to_string())?;
    let sample_rate = decoder.sample_rate().get();
    let channels = decoder.channels().get();
    let frame = u64::from(sample_rate) * u64::from(channels);
    let max = frame.saturating_mul(limit.as_secs().max(1));
    let samples: Vec<f32> = decoder
        .take(usize::try_from(max).unwrap_or(usize::MAX))
        .collect();
    Ok(DecodedPrefix {
        sample_rate,
        channels,
        samples,
    })
}

/// Interleaved float samples from [`decode_prefix`].
#[cfg(test)]
#[derive(Clone, Debug, PartialEq)]
pub struct DecodedPrefix {
    pub sample_rate: u32,
    pub channels: u16,
    pub samples: Vec<f32>,
}

/// Write interleaved `f32` samples as 16-bit PCM wav.
#[cfg(test)]
pub fn write_wav(path: &Path, decoded: &DecodedPrefix) -> Result<(), String> {
    let mut file = File::create(path).map_err(|error| error.to_string())?;
    let channels = u16::max(decoded.channels, 1);
    let data_len = decoded.samples.len().saturating_mul(2);
    let data_len = u32::try_from(data_len).map_err(|_| "wav is larger than 4 GB".to_string())?;
    file.write_all(b"RIFF").map_err(|error| error.to_string())?;
    file.write_all(&(36u32.saturating_add(data_len)).to_le_bytes())
        .map_err(|error| error.to_string())?;
    file.write_all(b"WAVEfmt ")
        .map_err(|error| error.to_string())?;
    file.write_all(&16u32.to_le_bytes())
        .map_err(|error| error.to_string())?;
    file.write_all(&1u16.to_le_bytes())
        .map_err(|error| error.to_string())?;
    file.write_all(&channels.to_le_bytes())
        .map_err(|error| error.to_string())?;
    file.write_all(&decoded.sample_rate.to_le_bytes())
        .map_err(|error| error.to_string())?;
    let byte_rate = decoded.sample_rate.saturating_mul(u32::from(channels) * 2);
    file.write_all(&byte_rate.to_le_bytes())
        .map_err(|error| error.to_string())?;
    let block_align = channels.saturating_mul(2);
    file.write_all(&block_align.to_le_bytes())
        .map_err(|error| error.to_string())?;
    file.write_all(&16u16.to_le_bytes())
        .map_err(|error| error.to_string())?;
    file.write_all(b"data").map_err(|error| error.to_string())?;
    file.write_all(&data_len.to_le_bytes())
        .map_err(|error| error.to_string())?;
    for sample in &decoded.samples {
        let clamped = sample.clamp(-1.0, 1.0);
        let pcm = (clamped * f32::from(i16::MAX)) as i16;
        file.write_all(&pcm.to_le_bytes())
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn open_decoder(
    bytes: Cursor<Vec<u8>>,
    mime: &str,
) -> Result<rodio::Decoder<Cursor<Vec<u8>>>, SinkError> {
    let mime = mime.to_ascii_lowercase();
    let decoded = if mime.contains("mp4") || mime.contains("m4a") || mime.contains("aac") {
        rodio::Decoder::new_mp4(bytes)
    } else if mime.contains("wav") {
        rodio::Decoder::new_wav(bytes)
    } else {
        rodio::Decoder::new(bytes)
    };
    decoded.map_err(|error| SinkError::Decode(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_prefix_reads_a_wav_without_an_audio_device() {
        let mut samples = Vec::with_capacity(8_000);
        for index in 0..8_000 {
            let step = (index % 16) as i16;
            samples.push(step * 1000);
        }
        let bytes = pcm_wav(8_000, &samples);
        let decoded =
            decode_prefix(&bytes, "audio/wav", Duration::from_secs(1)).expect("wav decodes");

        assert_eq!(decoded.sample_rate, 8_000);
        assert_eq!(decoded.channels, 1);
        assert!(
            decoded.samples.len() >= 7_000,
            "decoded {} samples",
            decoded.samples.len()
        );
        assert!(decoded.samples.iter().any(|sample| sample.abs() > 0.01));
    }

    fn pcm_wav(sample_rate: u32, samples: &[i16]) -> Vec<u8> {
        let data_len = u32::try_from(samples.len() * 2).expect("fits");
        let mut bytes = Vec::with_capacity(44 + data_len as usize);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&sample_rate.to_le_bytes());
        bytes.extend_from_slice(&(sample_rate * 2).to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }
}
