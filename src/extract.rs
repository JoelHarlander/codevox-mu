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
//! calls it, and so does `--selftest`. Nothing here runs for `--smoke` or
//! for `cargo test`.

use innertube_rs::{FormatFilter, FormatType, GetVideoInfoOptions, Innertube, QualityPreference};

/// Bytes the decoder can read without opening a socket.
#[derive(Clone, Debug)]
pub struct AudioSource {
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// An [`AudioSource`] plus the InnerTube client that actually downloaded it.
#[derive(Clone, Debug)]
pub struct ResolvedAudio {
    pub source: AudioSource,
    pub client: String,
    pub itag: u32,
}

/// A video id that did not become audio.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtractError(String);

impl ExtractError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(brief(message.into()))
    }

    /// A stream failure. [`new`] still shortens other errors to one short line.
    pub fn full(message: impl Into<String>) -> Self {
        Self(one_line(message.into()))
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

    /// Download and, when needed, transcode one track. Reports the client and itag.
    pub fn open(&mut self, video_id: &str) -> Result<ResolvedAudio, ExtractError> {
        let video_id = video_id.trim();
        if video_id.is_empty() {
            return Err(ExtractError::new("missing video id"));
        }
        self.open_blocking(video_id)
    }

    fn open_blocking(&mut self, video_id: &str) -> Result<ResolvedAudio, ExtractError> {
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
        self.open(video_id).map(|resolved| resolved.source)
    }
}

/// Opus in WebM. Used when the AAC URL (usually itag 140) is refused.
const OPUS_ITAG: u32 = 251;

/// TV and visionOS are the clients whose HTTPS URLs do not require a PO
/// token. iOS URLs exist, but googlevideo rejects them unless the request
/// uses the iOS app user agent. Web and YouTube Music are last.
const PLAYBACK_CLIENTS: &[&str] = &["TV", "VISIONOS", "IOS", "ANDROID_VR", "WEB", "YTMUSIC"];

/// Popular tracks `--selftest` and the ignored live test download.
pub(crate) const SELFTEST_TRACKS: &[(&str, &str)] = &[
    ("U9BwWKXjVaI", "Drake - Nice For What"),
    ("4D7u5KF7SP8", "Daft Punk - Get Lucky"),
    ("JGwWNGJdvx8", "Ed Sheeran - Shape of You"),
];

async fn download_audio(
    client: Innertube,
    video_id: String,
) -> Result<ResolvedAudio, ExtractError> {
    let mut failures = Vec::new();

    for name in PLAYBACK_CLIENTS {
        match client_audio(&client, &video_id, name).await {
            Ok(resolved) => return Ok(resolved),
            Err(error) => {
                let detail = logged_detail(&error);
                log::warn!("youtube audio: {name} failed: {detail}");
                failures.push(format!("{name}: {detail}"));
            }
        }
    }

    Err(ExtractError::full(if failures.is_empty() {
        "no playable audio stream".to_string()
    } else {
        failures.join("; ")
    }))
}

async fn client_audio(
    client: &Innertube,
    video_id: &str,
    name: &str,
) -> Result<ResolvedAudio, String> {
    let options = GetVideoInfoOptions {
        client: Some(name.to_string()),
        ..GetVideoInfoOptions::default()
    };
    let info = client
        .get_basic_info(video_id, Some(&options))
        .await
        .map_err(|error| error.to_string())?;
    let targets = audio_targets(&info.player_response);
    if targets.is_empty() {
        return Err("no audio format".to_string());
    }

    let user_agent = user_agent_for(name);
    let mut reasons = Vec::new();
    for format in targets {
        if !mime_is_decodable(&format.mime_type) {
            reasons.push(format!("mime {}", format.mime_type));
            continue;
        }
        let url = match innertube_rs::endpoints::player::resolve_stream_url(
            &format,
            client.player.decipherer.as_ref(),
        ) {
            Ok(url) => url,
            Err(error) => {
                reasons.push(error.to_string());
                continue;
            }
        };
        let mime = if format.mime_type.trim().is_empty() {
            "application/octet-stream".to_string()
        } else {
            format.mime_type.clone()
        };
        let bytes = match fetch_audio(client, &url, format.itag, &mime, user_agent, name).await {
            Ok(bytes) => bytes,
            Err(error) => {
                reasons.push(error.to_string());
                continue;
            }
        };
        let source = match playable_source(mime, bytes) {
            Ok(source) => source,
            Err(error) => {
                reasons.push(error);
                continue;
            }
        };
        log::info!(
            "youtube audio: {name} itag {} ({})",
            format.itag,
            source.mime
        );
        return Ok(ResolvedAudio {
            source,
            client: name.to_string(),
            itag: format.itag,
        });
    }

    Err(if reasons.is_empty() {
        "no playable audio stream".to_string()
    } else {
        reasons.join("; ")
    })
}

/// Highest AAC in MP4 first. Itag 251 is the fallback when that URL is refused.
fn audio_targets(
    player: &innertube_rs::models::video::PlayerResponse,
) -> Vec<innertube_rs::models::format::StreamingFormat> {
    let filter = FormatFilter {
        format_type: FormatType::AudioOnly,
        quality: QualityPreference::Highest,
        container: Some("mp4".to_string()),
    };
    let mut targets = Vec::new();
    if let Ok(format) = innertube_rs::endpoints::player::select_format(player, &filter) {
        targets.push(format.clone());
    }
    let Some(streaming) = player.streaming_data.as_ref() else {
        return targets;
    };
    for format in streaming
        .adaptive_formats
        .iter()
        .chain(streaming.formats.iter())
    {
        if format.itag == OPUS_ITAG && targets.iter().all(|chosen| chosen.itag != OPUS_ITAG) {
            targets.push(format.clone());
            break;
        }
    }
    targets
}

fn user_agent_for(client: &str) -> &'static str {
    use innertube_rs::constants::clients as names;
    match client {
        "IOS" => names::IOS_USER_AGENT,
        "ANDROID_VR" => names::ANDROID_VR_USER_AGENT,
        "TV" => names::TV_USER_AGENT,
        "VISIONOS" => names::VISIONOS_USER_AGENT,
        "ANDROID" => names::ANDROID_USER_AGENT,
        "MWEB" => names::MWEB_USER_AGENT,
        _ => names::WEB_USER_AGENT,
    }
}

/// WebM Opus is not a rodio container. It becomes wav before the sink sees it.
fn playable_source(mime: String, bytes: Vec<u8>) -> Result<AudioSource, String> {
    let lower = mime.to_ascii_lowercase();
    if lower.contains("webm") || lower.contains("opus") {
        let wav = crate::local::opus_to_wav(&bytes)?;
        return Ok(AudioSource {
            mime: "audio/wav".to_string(),
            bytes: wav,
        });
    }
    Ok(AudioSource { mime, bytes })
}

/// A plain GET is often refused. The player sends a range request and the
/// same user agent that asked for the URL; try the other referer before giving up.
async fn fetch_audio(
    client: &Innertube,
    url: &str,
    itag: u32,
    mime: &str,
    user_agent: &str,
    name: &str,
) -> Result<Vec<u8>, ExtractError> {
    let referers = referers_for(name);
    let mut failures = Vec::new();

    for referer in referers {
        let origin = referer.trim_end_matches('/');
        for range in [Some("bytes=0-"), None] {
            let mut request = client
                .session
                .http_client
                .get(url)
                .header("user-agent", user_agent)
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
                        failed = Some(ExtractError::full(format!(
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

    Err(ExtractError::full(format!(
        "the audio stream was refused ({mime}, itag {itag}; {})",
        summarize_attempts(&failures)
    )))
}

fn referers_for(client: &str) -> [&'static str; 2] {
    if client.eq_ignore_ascii_case("YTMUSIC") {
        ["https://music.youtube.com/", "https://www.youtube.com/"]
    } else {
        ["https://www.youtube.com/", "https://music.youtube.com/"]
    }
}

fn summarize_attempts(failures: &[String]) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for failure in failures {
        if let Some(found) = counts.iter_mut().find(|(text, _)| *text == failure) {
            found.1 += 1;
        } else {
            counts.push((failure, 1));
        }
    }
    counts
        .into_iter()
        .map(|(text, count)| {
            if count == 1 {
                text.to_string()
            } else {
                format!("{text} x{count}")
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// AAC in MP4 is what the decoder is built for. WebM Opus (itag 251) is
/// transcoded to wav. Vorbis, MP3, FLAC, and WAV are the other containers.
fn mime_is_decodable(mime: &str) -> bool {
    let mime = mime.to_ascii_lowercase();
    [
        "mp4", "m4a", "aac", "mp3", "wav", "flac", "ogg", "vorbis", "opus", "webm",
    ]
    .iter()
    .any(|needle| mime.contains(needle))
}

fn logged_detail(message: &str) -> String {
    crate::diag::redact(&strip_urls(&one_line(message.to_string())))
}

fn strip_urls(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = find_url(rest) {
        out.push_str(&rest[..start]);
        out.push_str("[url]");
        let after = &rest[start..];
        let end = after.find(char::is_whitespace).unwrap_or(after.len());
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

fn find_url(text: &str) -> Option<usize> {
    let https = text.find("https://");
    let http = text.find("http://");
    match (https, http) {
        (Some(https), Some(http)) => Some(https.min(http)),
        (Some(index), None) | (None, Some(index)) => Some(index),
        (None, None) => None,
    }
}

fn one_line(message: String) -> String {
    let line = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.is_empty() {
        "unknown error".to_string()
    } else {
        line
    }
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

/// First ten seconds of a few real tracks. Ignored so `cargo test` stays offline.
#[cfg(test)]
mod live {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::InnertubeExtractor;
    use super::SELFTEST_TRACKS;
    use crate::output::{self, DecodedPrefix};

    #[test]
    #[ignore = "resolves and decodes YouTube Music tracks; cargo test stays offline"]
    fn live_decode_first_ten_seconds() {
        let mut extractor = InnertubeExtractor::new();
        for (video_id, title) in SELFTEST_TRACKS {
            let resolved = extractor
                .open(video_id)
                .unwrap_or_else(|error| panic!("{title}: {error}"));
            let decoded = output::decode_prefix(
                &resolved.source.bytes,
                &resolved.source.mime,
                Duration::from_secs(10),
            )
            .unwrap_or_else(|error| panic!("{title}: {error}"));

            let expected = decoded.sample_rate as usize * decoded.channels as usize * 9;
            assert!(
                decoded.samples.len() >= expected,
                "{title}: decoded {} samples, wanted at least {expected}",
                decoded.samples.len()
            );
            assert!(
                decoded.loud_samples() > 1000,
                "{title}: decoded audio was silent ({} loud samples)",
                decoded.loud_samples()
            );

            let path = PathBuf::from(format!("target/live-{video_id}.wav"));
            output::write_wav(&path, &decoded).expect("write the wav");
            eprintln!(
                "live decode: \"{title}\" id {video_id} client {} itag {} mime {} bytes {} duration {:.2}s rms {:.4} wav {}",
                resolved.client,
                resolved.itag,
                resolved.source.mime,
                resolved.source.bytes.len(),
                decoded.seconds(),
                rms(&decoded),
                path.display()
            );
        }
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

    #[test]
    fn stream_failures_keep_every_client_and_other_errors_stay_short() {
        let long = format!("IOS: {}", "x".repeat(400));
        let shortened = ExtractError::new(long.clone());
        assert!(shortened.to_string().chars().count() <= ERROR_CHARS + 1);
        assert!(shortened.to_string().ends_with('…'));
        let full = ExtractError::full(long);
        assert!(full.to_string().chars().count() > ERROR_CHARS);
        assert!(!full.to_string().ends_with('…'));
    }

    #[test]
    fn ios_streams_use_the_ios_app_user_agent() {
        let ios = user_agent_for("IOS");
        assert!(ios.starts_with("com.google.ios.youtube/"), "{ios}");
        assert_ne!(ios, innertube_rs::constants::DEFAULT_USER_AGENT);
        assert!(user_agent_for("ANDROID_VR").contains("youtube.vr"));
        assert_eq!(
            user_agent_for("WEB"),
            innertube_rs::constants::clients::WEB_USER_AGENT
        );
        assert_eq!(PLAYBACK_CLIENTS[0], "TV");
        assert!(PLAYBACK_CLIENTS.iter().position(|name| *name == "IOS") > Some(0));
    }

    #[test]
    fn repeated_refusals_collapse_and_urls_are_not_logged() {
        let summary = summarize_attempts(&[
            "HTTP 403 Forbidden".to_string(),
            "HTTP 403 Forbidden".to_string(),
            "HTTP 403 Forbidden".to_string(),
            "HTTP 403 Forbidden".to_string(),
        ]);
        assert_eq!(summary, "HTTP 403 Forbidden x4");
        let cleaned = logged_detail(
            "Network request failed: https://rr.googlevideo.com/videoplayback?sig=supersecretvalue&id=1",
        );
        assert!(!cleaned.contains("supersecretvalue"), "{cleaned}");
        assert!(cleaned.contains("[url]"), "{cleaned}");
        assert!(cleaned.contains("Network request failed"), "{cleaned}");
    }

    #[test]
    fn opus_and_aac_mimes_are_both_decodable() {
        assert!(mime_is_decodable("audio/mp4; codecs=\"mp4a.40.2\""));
        assert!(mime_is_decodable("audio/webm; codecs=\"opus\""));
        assert!(!mime_is_decodable("image/jpeg"));
    }
}
