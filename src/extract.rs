//! Resolve a YouTube video id to audio bytes.
//!
//! [`Extractor`] is the only thing the player knows about stream URLs, so
//! this client can be replaced. The client today is
//! [innertube-rs](https://crates.io/crates/innertube-rs) 0.10.1 (MIT). It
//! speaks InnerTube and deciphers signatures inside the process.
//!
//! [rusty_ytdl](https://crates.io/crates/rusty_ytdl) 0.7.4 is MIT OR
//! Apache-2.0, but its last release was August 2024 and its Android client
//! URLs have been rejected with HTTP 403. [rustypipe](https://crates.io/crates/rustypipe)
//! is GPL-3.0. This MIT project does not depend on it.
//!
//! [`InnertubeExtractor::audio`] blocks on the network. The playback thread
//! calls it. Nothing here runs for `--smoke` or for `cargo test`.

use innertube_rs::{FormatFilter, FormatType, GetVideoInfoOptions, Innertube, QualityPreference};

/// Bytes the decoder can read without opening a socket.
#[derive(Clone, Debug)]
pub struct AudioSource {
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// A video id that did not become audio.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtractError(String);

impl ExtractError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(brief(message.into()))
    }
}

impl std::fmt::Display for ExtractError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ExtractError {}

/// Turns a video id into an [`AudioSource`].
///
/// The player calls this off the UI thread. A blank id must not touch the
/// network.
pub trait Extractor {
    fn audio(&mut self, video_id: &str) -> Result<AudioSource, ExtractError>;
}

/// How large one track is allowed to get before we refuse to buffer it.
const MAX_AUDIO_BYTES: usize = 32 * 1024 * 1024;

/// How long an error row is allowed to get.
const ERROR_CHARS: usize = 180;

/// Innertube client, built on the first request.
///
/// Building it downloads YouTube's player script. That waits until a track
/// is actually opened.
pub struct InnertubeExtractor {
    runtime: Option<tokio::runtime::Runtime>,
    client: Option<Innertube>,
}

impl InnertubeExtractor {
    pub fn new() -> Self {
        Self {
            runtime: None,
            client: None,
        }
    }

    fn audio_blocking(&mut self, video_id: &str) -> Result<AudioSource, ExtractError> {
        self.ensure_runtime()?;
        if self.client.is_none() {
            let created = self
                .runtime
                .as_ref()
                .ok_or_else(|| ExtractError::new("audio runtime was not started"))?
                .block_on(Innertube::new())
                .map_err(|error| ExtractError::new(error.to_string()))?;
            self.client = Some(created);
        }

        let client = self
            .client
            .clone()
            .ok_or_else(|| ExtractError::new("audio client was not started"))?;
        let runtime = self
            .runtime
            .as_ref()
            .ok_or_else(|| ExtractError::new("audio runtime was not started"))?;
        runtime.block_on(download_audio(client, video_id.to_string()))
    }

    fn ensure_runtime(&mut self) -> Result<(), ExtractError> {
        if self.runtime.is_some() {
            return Ok(());
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_io()
            .enable_time()
            .thread_name("youtube-audio")
            .build()
            .map_err(|error| {
                ExtractError::new(format!("could not start the audio runtime: {error}"))
            })?;
        self.runtime = Some(runtime);
        Ok(())
    }
}

impl Extractor for InnertubeExtractor {
    fn audio(&mut self, video_id: &str) -> Result<AudioSource, ExtractError> {
        let video_id = video_id.trim();
        if video_id.is_empty() {
            return Err(ExtractError::new("missing video id"));
        }
        self.audio_blocking(video_id)
    }
}

async fn download_audio(client: Innertube, video_id: String) -> Result<AudioSource, ExtractError> {
    // The web and YouTube Music players return itag 140 URLs that googlevideo
    // rejects. iOS and TV clients still hand back a URL the same decipher
    // can play. Stop at the first one that actually downloads.
    let filter = FormatFilter {
        format_type: FormatType::AudioOnly,
        quality: QualityPreference::Highest,
        container: Some("mp4".to_string()),
    };
    let mut failures = Vec::new();

    for name in ["IOS", "TV", "ANDROID_VR", "WEB", "YTMUSIC"] {
        let options = GetVideoInfoOptions {
            client: Some(name.to_string()),
            ..GetVideoInfoOptions::default()
        };
        let info = match client.get_basic_info(&video_id, Some(&options)).await {
            Ok(info) => info,
            Err(error) => {
                failures.push(format!("{name}: {error}"));
                continue;
            }
        };
        let format =
            match innertube_rs::endpoints::player::select_format(&info.player_response, &filter) {
                Ok(format) => format,
                Err(error) => {
                    failures.push(format!("{name}: {error}"));
                    continue;
                }
            };
        if !mime_is_decodable(&format.mime_type) {
            failures.push(format!("{name}: mime {}", format.mime_type));
            continue;
        }
        let url = match innertube_rs::endpoints::player::resolve_stream_url(
            format,
            client.player.decipherer.as_ref(),
        ) {
            Ok(url) => url,
            Err(error) => {
                failures.push(format!("{name}: {error}"));
                continue;
            }
        };
        let mime = if format.mime_type.trim().is_empty() {
            "application/octet-stream".to_string()
        } else {
            format.mime_type.clone()
        };
        match fetch_audio(&client, &url, format.itag, &mime).await {
            Ok(bytes) => return Ok(AudioSource { mime, bytes }),
            Err(error) => failures.push(format!("{name}: {error}")),
        }
    }

    Err(ExtractError::new(if failures.is_empty() {
        "no playable audio stream".to_string()
    } else {
        failures.join("; ")
    }))
}

/// A plain GET is often refused. The player sends a range request and a
/// web referer; try those before giving up.
async fn fetch_audio(
    client: &Innertube,
    url: &str,
    itag: u32,
    mime: &str,
) -> Result<Vec<u8>, ExtractError> {
    let referers = ["https://music.youtube.com/", "https://www.youtube.com/"];
    let mut failures = Vec::new();

    for referer in referers {
        let origin = referer.trim_end_matches('/');
        for range in [Some("bytes=0-"), None] {
            let mut request = client
                .session
                .http_client
                .get(url)
                .header("user-agent", innertube_rs::constants::DEFAULT_USER_AGENT)
                .header("referer", referer)
                .header("origin", origin);
            if let Some(range) = range {
                request = request.header("range", range);
            }
            let response = match request.send().await {
                Ok(response) => response,
                Err(error) => {
                    failures.push(format!("did not open: {error}"));
                    continue;
                }
            };
            let status = response.status();
            if !status.is_success() {
                failures.push(format!("HTTP {status}"));
                continue;
            }

            let mut bytes = Vec::new();
            let mut response = response;
            let mut failed = None;
            while let Some(chunk) = response.chunk().await.transpose() {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        failed = Some(ExtractError::new(format!(
                            "the audio stream stopped early: {error}"
                        )));
                        break;
                    }
                };
                if bytes.len().saturating_add(chunk.len()) > MAX_AUDIO_BYTES {
                    return Err(ExtractError::new("the audio stream is larger than 32 MB"));
                }
                bytes.extend_from_slice(&chunk);
            }
            if let Some(error) = failed {
                return Err(error);
            }
            if bytes.is_empty() {
                failures.push("empty body".to_string());
                continue;
            }
            return Ok(bytes);
        }
    }

    Err(ExtractError::new(format!(
        "the audio stream was refused ({mime}, itag {itag}; {})",
        failures.join("; ")
    )))
}

/// AAC in MP4 is what the decoder is built for. Vorbis, MP3, FLAC, and WAV
/// are the other containers rodio is compiled with.
fn mime_is_decodable(mime: &str) -> bool {
    let mime = mime.to_ascii_lowercase();
    ["mp4", "m4a", "aac", "mp3", "wav", "flac", "ogg", "vorbis"]
        .iter()
        .any(|needle| mime.contains(needle))
}

fn brief(message: String) -> String {
    let line = message.lines().next().unwrap_or("unknown error").trim();
    let line = if line.is_empty() {
        "unknown error"
    } else {
        line
    };
    let mut brief: String = line.chars().take(ERROR_CHARS).collect();
    if line.chars().nth(ERROR_CHARS).is_some() {
        brief.push('…');
    }
    brief
}

/// First ten seconds of a real track. Ignored so `cargo test` stays offline.
#[cfg(test)]
mod live {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::Extractor;
    use super::InnertubeExtractor;
    use crate::output::{self, DecodedPrefix};

    /// "Get Lucky" from the recorded search fixture. Long enough that ten
    /// seconds is a prefix, not the whole file.
    const VIDEO_ID: &str = "4D7u5KF7SP8";

    #[test]
    #[ignore = "resolves and decodes a YouTube Music track; cargo test stays offline"]
    fn live_decode_first_ten_seconds() {
        let mut extractor = InnertubeExtractor::new();
        let source = extractor
            .audio(VIDEO_ID)
            .expect("resolve a real audio stream");
        let decoded = output::decode_prefix(&source.bytes, &source.mime, Duration::from_secs(10))
            .expect("decode the first ten seconds");

        let expected = decoded.sample_rate as usize * decoded.channels as usize * 9;
        assert!(
            decoded.samples.len() >= expected,
            "decoded {} samples, wanted at least {expected} for 9s at {} Hz x {} ch",
            decoded.samples.len(),
            decoded.sample_rate,
            decoded.channels
        );
        let loud = decoded
            .samples
            .iter()
            .filter(|sample| sample.abs() > 0.01)
            .count();
        assert!(
            loud > 1000,
            "decoded audio was silent ({loud} loud samples)"
        );

        let path = PathBuf::from("target/live-ten-seconds.wav");
        output::write_wav(&path, &decoded).expect("write the wav");
        let rms = rms(&decoded);
        eprintln!(
            "live decode: id {VIDEO_ID} mime {} bytes {} rate {} channels {} samples {} duration {:.2}s rms {rms:.4} wav {}",
            source.mime,
            source.bytes.len(),
            decoded.sample_rate,
            decoded.channels,
            decoded.samples.len(),
            decoded.samples.len() as f32
                / decoded.sample_rate as f32
                / decoded.channels.max(1) as f32,
            path.display()
        );
    }

    fn rms(decoded: &DecodedPrefix) -> f32 {
        if decoded.samples.is_empty() {
            return 0.0;
        }
        let energy = decoded
            .samples
            .iter()
            .map(|sample| sample * sample)
            .sum::<f32>()
            / decoded.samples.len() as f32;
        energy.sqrt()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_video_id_does_not_open_a_client() {
        let mut extractor = InnertubeExtractor::new();
        let error = extractor.audio("   ").expect_err("blank id");
        assert!(error.to_string().contains("missing video id"));
        assert!(extractor.runtime.is_none());
        assert!(extractor.client.is_none());
    }
}
