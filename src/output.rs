//! Play decoded audio on the default output device.
//!
//! [rodio](https://crates.io/crates/rodio) 0.23 is MIT OR Apache-2.0. It
//! decodes with symphonia and writes samples through
//! [cpal](https://crates.io/crates/cpal): WASAPI on Windows, CoreAudio on
//! macOS, and ALSA on Linux. Symphonia is MPL-2.0, a file-level weak
//! copyleft, not GPL. rodio's MIT and Apache decoders do not read the AAC
//! inside the MP4 container YouTube serves, so symphonia is the decoder.
//! The same decoder reads local mp3, flac, ogg Vorbis, m4a/aac, and wav.
//!
//! Opening the device happens on the first play, on whichever thread asks
//! for it. Spotify PCM and YouTube or local audio share that one device.
//! A second WASAPI stream opened while the first is still held stays silent
//! on Windows, and cpal only prints the failure. `--smoke` never constructs
//! a [`RodioSink`].

use std::io::Cursor;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(test)]
use std::fs::File;
#[cfg(test)]
use std::io::Write;
#[cfg(test)]
use std::path::Path;

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

/// The one output device. Spotify and the stream player both borrow its mixer.
pub struct SharedOutput {
    opened: Mutex<Option<Opened>>,
    failed: Arc<AtomicBool>,
}

struct Opened {
    /// Kept so the stream stays open. The mixer is cloned out to players.
    #[allow(dead_code, reason = "dropping the device stops the stream")]
    device: rodio::MixerDeviceSink,
    mixer: rodio::mixer::Mixer,
}

impl SharedOutput {
    pub fn new() -> Self {
        Self {
            opened: Mutex::new(None),
            failed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The mixer for the open device, opening or replacing it when needed.
    pub fn mixer(&self) -> Result<rodio::mixer::Mixer, SinkError> {
        let mut opened = self
            .opened
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.failed.swap(false, Ordering::SeqCst) && opened.is_some() {
            log::error!("playback: reopening the output device after the stream stopped");
            *opened = None;
        }
        if opened.is_none() {
            let device = open_device(Arc::clone(&self.failed))?;
            let mixer = device.mixer().clone();
            *opened = Some(Opened { device, mixer });
        }
        Ok(opened
            .as_ref()
            .expect("the device was just opened")
            .mixer
            .clone())
    }
}

fn open_device(failed: Arc<AtomicBool>) -> Result<rodio::MixerDeviceSink, SinkError> {
    match rodio::DeviceSinkBuilder::from_default_device() {
        Ok(builder) => {
            let flag = Arc::clone(&failed);
            match builder
                .with_error_callback(move |error: rodio::CpalError| {
                    log::error!("playback: audio output stopped: {error}");
                    flag.store(true, Ordering::SeqCst);
                })
                .open_stream()
            {
                Ok(mut device) => {
                    device.log_on_drop(false);
                    Ok(device)
                }
                Err(error) => {
                    log::error!("playback: default output failed ({error}); trying another device");
                    fallback_device()
                }
            }
        }
        Err(error) => {
            log::error!("playback: default output failed ({error}); trying another device");
            fallback_device()
        }
    }
}

fn fallback_device() -> Result<rodio::MixerDeviceSink, SinkError> {
    let mut device = rodio::DeviceSinkBuilder::open_default_sink()
        .map_err(|error| SinkError::Device(error.to_string()))?;
    device.log_on_drop(false);
    Ok(device)
}

/// cpal output. The stream stays open until the [`SharedOutput`] is dropped.
pub struct RodioSink {
    output: Arc<SharedOutput>,
    player: Option<rodio::Player>,
    playing: bool,
    paused: bool,
    started_at: Option<Instant>,
    stall_logged: AtomicBool,
}

impl RodioSink {
    pub fn new(output: Arc<SharedOutput>) -> Self {
        Self {
            output,
            player: None,
            playing: false,
            paused: false,
            started_at: None,
            stall_logged: AtomicBool::new(false),
        }
    }
}

impl AudioSink for RodioSink {
    fn start(&mut self, source: AudioSource) -> Result<(), SinkError> {
        let byte_len = source.bytes.len();
        let mime = source.mime.clone();
        let decoded = open_decoder(source.bytes, &mime)?;
        let decoded = ReportingDecoder::new(decoded);
        let mixer = self.output.mixer()?;

        // Drop the previous track before connecting. `connect_new` only
        // needs the mixer for the call; the player keeps its own queue.
        self.player = None;
        let player = rodio::Player::connect_new(&mixer);
        log::info!("playback: playing {mime} ({byte_len} bytes)");
        player.append(decoded);
        self.player = Some(player);
        self.playing = true;
        self.paused = false;
        self.started_at = Some(Instant::now());
        self.stall_logged.store(false, Ordering::Relaxed);
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
        self.started_at = None;
    }

    fn elapsed(&self) -> Duration {
        let position = if self.playing || self.paused {
            self.player
                .as_ref()
                .map(rodio::Player::get_pos)
                .unwrap_or_default()
        } else {
            Duration::ZERO
        };
        self.note_stall(position);
        position
    }

    fn finished(&self) -> bool {
        self.playing && !self.paused && self.player.as_ref().is_some_and(rodio::Player::empty)
    }
}

impl RodioSink {
    /// The callback updates position as soon as it pulls a sample. A position
    /// that stays at zero means that callback is not running.
    fn note_stall(&self, position: Duration) {
        if !self.playing || self.paused || !position.is_zero() {
            return;
        }
        let Some(started) = self.started_at else {
            return;
        };
        if started.elapsed() < Duration::from_secs(2) {
            return;
        }
        if self.stall_logged.swap(true, Ordering::Relaxed) {
            return;
        }
        log::error!(
            "playback: output produced no samples for 2s; the device callback is not running"
        );
    }
}

/// Samples from the start of `bytes`, stopped after `limit`.
///
/// This does not open an audio device. The ignored live test uses it as a
/// null sink. Playback decodes with the same [`open_decoder`] inside the
/// rodio player.
#[cfg(test)]
pub fn decode_prefix(bytes: &[u8], mime: &str, limit: Duration) -> Result<DecodedPrefix, String> {
    let decoder = open_decoder(bytes.to_vec(), mime).map_err(|error| error.to_string())?;
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

#[cfg(test)]
impl DecodedPrefix {
    pub(crate) fn seconds(&self) -> f32 {
        let frame = u32::from(self.channels.max(1)) * self.sample_rate.max(1);
        self.samples.len() as f32 / frame as f32
    }

    pub(crate) fn loud_samples(&self) -> usize {
        self.samples
            .iter()
            .filter(|sample| sample.abs() > 0.01)
            .count()
    }
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

/// The bytes are already in memory. Tell symphonia the length and that it
/// can seek, or a fragmented MP4 (YouTube itag 140, with an `sidx`) is read
/// as a forward-only stream. Once a sample offset falls behind the 64 KiB
/// cache, the demuxer stops and rodio turns that into a silent end.
fn open_decoder(data: Vec<u8>, mime: &str) -> Result<rodio::Decoder<Cursor<Vec<u8>>>, SinkError> {
    let len = u64::try_from(data.len()).unwrap_or(u64::MAX);
    let mime = mime.to_ascii_lowercase();
    let mut builder = rodio::Decoder::builder()
        .with_data(Cursor::new(data))
        .with_byte_len(len);
    if mime.contains("mp4") || mime.contains("m4a") || mime.contains("aac") {
        builder = builder.with_mime_type("audio/mp4");
    } else if mime.contains("wav") {
        builder = builder.with_hint("wav");
    }
    builder
        .build()
        .map_err(|error| SinkError::Decode(error.to_string()))
}

/// Logs why the device callback stopped pulling samples.
struct ReportingDecoder {
    inner: rodio::Decoder<Cursor<Vec<u8>>>,
    samples: u64,
    sample_rate: rodio::SampleRate,
    channels: rodio::ChannelCount,
    reported: bool,
    dead: bool,
}

impl ReportingDecoder {
    fn new(inner: rodio::Decoder<Cursor<Vec<u8>>>) -> Self {
        let sample_rate = inner.sample_rate();
        let channels = inner.channels();
        Self {
            inner,
            samples: 0,
            sample_rate,
            channels,
            reported: false,
            dead: false,
        }
    }

    fn finish(&mut self, reason: &str) {
        if self.reported {
            return;
        }
        self.reported = true;
        let frame = u64::from(self.sample_rate.get()) * u64::from(self.channels.get().max(1));
        let secs = self.samples as f32 / frame.max(1) as f32;
        if reason == "end of stream" && secs >= 0.25 {
            log::info!(
                "playback: output ended after {secs:.2}s ({} samples): {reason}",
                self.samples
            );
        } else {
            log::error!(
                "playback: output ended after {secs:.2}s ({} samples): {reason}",
                self.samples
            );
        }
    }
}

impl Iterator for ReportingDecoder {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.dead {
            return None;
        }
        let pulled = catch_unwind(AssertUnwindSafe(|| self.inner.next()));
        match pulled {
            Ok(Some(sample)) => {
                self.samples = self.samples.saturating_add(1);
                Some(sample)
            }
            Ok(None) => {
                self.finish("end of stream");
                None
            }
            Err(payload) => {
                self.dead = true;
                self.finish(&panic_reason(&payload));
                None
            }
        }
    }
}

impl Source for ReportingDecoder {
    fn current_span_len(&self) -> Option<usize> {
        if self.dead {
            Some(0)
        } else {
            self.inner.current_span_len()
        }
    }

    fn channels(&self) -> rodio::ChannelCount {
        self.channels
    }

    fn sample_rate(&self) -> rodio::SampleRate {
        self.sample_rate
    }

    fn total_duration(&self) -> Option<Duration> {
        if self.dead {
            None
        } else {
            self.inner.total_duration()
        }
    }
}

fn panic_reason(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "decoder panicked".to_string()
    }
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

    /// Generated sine, AAC in a fragmented MP4 with an `sidx` and three
    /// `moof`/`mdat` pairs. Same container as a visionOS itag 140 stream.
    #[test]
    fn fragmented_aac_reaches_the_live_player_graph() {
        let bytes = include_bytes!("../tests/fixtures/fragmented-aac.mp4");
        let decoded = decode_prefix(
            bytes,
            "audio/mp4; codecs=\"mp4a.40.2\"",
            Duration::from_secs(5),
        )
        .expect("seekable decoder reads the fragmented file");
        assert_eq!(decoded.channels, 2);
        assert_eq!(decoded.sample_rate, 44_100);
        assert!(
            decoded.seconds() >= 1.8,
            "decoded {:.2}s, wanted the whole clip",
            decoded.seconds()
        );
        assert!(
            decoded.loud_samples() > 1_000,
            "{} loud samples",
            decoded.loud_samples()
        );

        // `Player::connect_new` wraps the queue before `append`. Pull that
        // graph at a device rate. No hardware.
        let (player, queue) = rodio::Player::new();
        let mut heard = rodio::source::UniformSourceIterator::new(
            queue,
            rodio::math::nz!(2),
            rodio::math::nz!(48_000),
        );
        player.append(open_decoder(bytes.to_vec(), "audio/mp4").expect("the sink's decoder"));
        let mut loud = 0usize;
        let limit = 48_000usize * 2 * 4;
        for _ in 0..limit {
            match heard.next() {
                Some(sample) if sample.abs() > 0.01 => loud += 1,
                Some(_) => {}
                None => break,
            }
        }
        let heard_secs = loud as f32 / 48_000.0 / 2.0;
        assert!(
            heard_secs >= 1.5,
            "device graph heard {heard_secs:.2}s ({loud} loud samples)"
        );
    }
}
