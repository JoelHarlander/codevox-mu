//! Lyrics for the track that is playing.
//!
//! LRCLIB's signature lookup (`GET /api/get`) is the only source. The query
//! is the track name, artist, album, and duration. Spotify and YouTube are
//! not asked for lyrics. A hit, a plain-only lyric, and a definitive miss
//! are stored under the app cache, one file per track. A network failure is
//! not stored, so the next time that track is loaded it is tried again.
//!
//! The LRC text itself is parsed by [`crate::lrc`].

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use iced::alignment::Vertical;
use iced::widget::operation::{self, AbsoluteOffset};
use iced::widget::{Space, column, container, row, scrollable, text};
use iced::{Element, Fill, Task, Theme};

use crate::lrc::{self, Line};
use crate::music::Source;
use crate::player::NowPlaying;

const ENDPOINT: &str = "https://lrclib.net/api/get";
const USER_AGENT: &str = concat!(
    "CodeVox mU/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/JoelHarlander/codevox-mu)"
);
/// LRCLIB rejects a duration outside this range. Leave it off the query then.
const DURATION_MIN_SECS: u64 = 1;
const DURATION_MAX_SECS: u64 = 3_600;
/// A miss can show up on LRCLIB later. Ask again after a day.
const MISSING_TTL_SECS: u64 = 24 * 60 * 60;
const MAX_BODY: usize = 256 * 1024;
const PANEL_WIDTH: f32 = 280.0;
const BODY_SIZE: f32 = 14.0;
const LINE_SLOT: f32 = 22.0;
const ROW_GAP: f32 = 6.0;
/// Mono body text in the panel width, used only to estimate the scroll offset.
const CHARS_PER_ROW: usize = 24;
const SCROLL_ID: &str = "codevox-lyrics";

/// Quiet copy when LRCLIB has no lyric and when the lookup itself failed.
const NOT_FOUND: &str = "No lyrics found";
const LOOKING: &str = "Looking up lyrics…";

/// Which service track the lyric belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    pub source: Source,
    pub id: String,
}

/// What LRCLIB was asked for. `duration_secs` is omitted when it is outside
/// the range that endpoint accepts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    pub key: Key,
    pub track_name: String,
    pub artist_name: String,
    pub album_name: Option<String>,
    pub duration_secs: Option<u64>,
}

/// One lookup, including where its result should be cached.
#[derive(Debug)]
pub struct Lookup {
    query: Query,
    cache: Option<PathBuf>,
}

/// The worker's reply. `Synced` still carries LRC text; the session parses it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Synced(String),
    Plain(String),
    Missing,
    /// The request did not produce a lyric. Not written to the cache.
    Unavailable,
}

/// Key plus outcome, so a late reply can be dropped.
#[derive(Debug)]
pub struct Loaded {
    pub key: Key,
    pub outcome: Outcome,
}

#[derive(Debug)]
enum Kind {
    Synced(Vec<Line>),
    Plain(String),
    Missing,
}

#[derive(Debug, Default)]
enum Phase {
    #[default]
    Idle,
    Loading,
    Ready(Kind),
}

/// What the window is showing for the current track.
#[derive(Debug, Default)]
pub struct Session {
    key: Option<Key>,
    phase: Phase,
    /// Last line the panel was asked to scroll to. `None` until the first one.
    followed: Option<usize>,
}

enum Step {
    Done(Outcome),
    Retry(Duration),
}

impl Session {
    /// Start a lookup when `now` is a different Spotify or YouTube Music track.
    ///
    /// A cached lyric is applied immediately and no [`Lookup`] is returned.
    /// The same track, already loading or ready, returns `None` so a clock
    /// tick does not ask twice.
    pub fn begin(&mut self, now: &NowPlaying, cache: Option<&Path>) -> Option<Lookup> {
        let Some(query) = query_from_now(now) else {
            self.clear();
            return None;
        };
        if self.key.as_ref() == Some(&query.key) && !matches!(self.phase, Phase::Idle) {
            return None;
        }
        self.key = Some(query.key.clone());
        self.followed = None;
        if let Some(dir) = cache
            && let Some(outcome) = read_cache(dir, &query.key)
        {
            self.phase = Phase::Ready(kind_from(outcome));
            return None;
        }
        self.phase = Phase::Loading;
        Some(Lookup {
            query,
            cache: cache.map(Path::to_path_buf),
        })
    }

    /// Apply a worker reply. A reply for a track that is no longer current
    /// is ignored.
    pub fn accept(&mut self, key: &Key, outcome: Outcome) -> bool {
        if self.key.as_ref() != Some(key) {
            return false;
        }
        self.followed = None;
        self.phase = Phase::Ready(kind_from(outcome));
        true
    }

    /// Scroll offset for a newly current line, in pixels.
    ///
    /// `None` when the line has not changed, or when the lyric is not synced.
    /// The offset keeps the previous line in view above the current one.
    pub fn follow(&mut self, elapsed: Duration) -> Option<f32> {
        let Phase::Ready(Kind::Synced(lines)) = &self.phase else {
            return None;
        };
        let index = lrc::current_line(lines, elapsed)?;
        if self.followed == Some(index) {
            return None;
        }
        self.followed = Some(index);
        Some(scroll_offset(lines, index))
    }

    /// Offline lines for `--screenshot`. No lookup and no cache.
    pub fn show_synced(&mut self, lrc_text: &str) {
        let lines = lrc::parse(lrc_text);
        self.key = None;
        self.followed = None;
        self.phase = if lines.is_empty() {
            Phase::Ready(Kind::Missing)
        } else {
            Phase::Ready(Kind::Synced(lines))
        };
    }

    fn clear(&mut self) {
        self.key = None;
        self.phase = Phase::Idle;
        self.followed = None;
    }
}

/// The lyrics column, or `None` when nothing is loaded.
pub fn panel<'a, Message: 'a>(
    session: &'a Session,
    elapsed: Duration,
) -> Option<Element<'a, Message>> {
    let body = match &session.phase {
        Phase::Idle => return None,
        Phase::Loading => quiet(LOOKING),
        Phase::Ready(Kind::Missing) => quiet(NOT_FOUND),
        Phase::Ready(Kind::Plain(lyric)) => plain_block(lyric),
        Phase::Ready(Kind::Synced(lines)) => synced_block(lines, elapsed),
    };
    Some(frame(body))
}

/// Scroll the lyrics column to `y`.
pub fn scroll_to<Message>(y: f32) -> Task<Message> {
    operation::scroll_to(scroll_id(), AbsoluteOffset { x: 0.0, y })
}

/// Run `lookup` on a thread named `lyrics`. The iced executor is not Tokio.
pub async fn fetch(lookup: Lookup) -> Loaded {
    let key = lookup.query.key.clone();
    let fallback_key = key.clone();
    let (sender, receiver) = iced::futures::channel::oneshot::channel();
    let started = std::thread::Builder::new()
        .name("lyrics".into())
        .spawn(move || {
            let loaded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| load(lookup)))
                .unwrap_or(Loaded {
                    key: fallback_key,
                    outcome: Outcome::Unavailable,
                });
            let _ = sender.send(loaded);
        });
    if started.is_err() {
        log::error!("lyrics: could not start the lookup thread");
        return Loaded {
            key,
            outcome: Outcome::Unavailable,
        };
    }
    receiver.await.unwrap_or(Loaded {
        key,
        outcome: Outcome::Unavailable,
    })
}

/// Cache directory for lyrics, beside the rest of the app cache.
pub fn disk_cache() -> Option<PathBuf> {
    crate::spotify::cache_dir().map(|root| root.join("lyrics"))
}

fn load(lookup: Lookup) -> Loaded {
    load_with(lookup, retrieve)
}

fn load_with(lookup: Lookup, fetch_outcome: impl FnOnce(&Query) -> Outcome) -> Loaded {
    if let Some(dir) = &lookup.cache
        && let Some(outcome) = read_cache(dir, &lookup.query.key)
    {
        return Loaded {
            key: lookup.query.key,
            outcome,
        };
    }
    let outcome = fetch_outcome(&lookup.query);
    if let Some(dir) = &lookup.cache
        && let Err(error) = write_cache(dir, &lookup.query.key, &outcome)
    {
        log::warn!("lyrics: could not cache the lyric: {error}");
    }
    Loaded {
        key: lookup.query.key,
        outcome,
    }
}

fn query_from_now(now: &NowPlaying) -> Option<Query> {
    let source = now.source?;
    if !matches!(source, Source::Spotify | Source::YouTubeMusic) {
        return None;
    }
    let track_name = now.title.trim();
    let artist_name = now.artist.trim();
    let id = now.track_id.trim();
    if track_name.is_empty() || artist_name.is_empty() || id.is_empty() {
        return None;
    }
    Some(Query {
        key: Key {
            source,
            id: now.track_id.clone(),
        },
        track_name: track_name.to_string(),
        artist_name: artist_name.to_string(),
        album_name: blank_to_none(now.album.as_deref()),
        duration_secs: duration_secs(now.duration),
    })
}

fn blank_to_none(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// Nearest second, kept only when LRCLIB will accept it.
fn duration_secs(duration: Duration) -> Option<u64> {
    let secs = duration.as_secs_f64().round();
    if !secs.is_finite() || secs < 0.0 {
        return None;
    }
    let secs = secs as u64;
    (DURATION_MIN_SECS..=DURATION_MAX_SECS)
        .contains(&secs)
        .then_some(secs)
}

fn query_pairs(query: &Query) -> Vec<(&'static str, String)> {
    let mut pairs = vec![
        ("track_name", query.track_name.clone()),
        ("artist_name", query.artist_name.clone()),
    ];
    if let Some(album) = &query.album_name {
        pairs.push(("album_name", album.clone()));
    }
    if let Some(duration) = query.duration_secs {
        pairs.push(("duration", duration.to_string()));
    }
    pairs
}

fn kind_from(outcome: Outcome) -> Kind {
    match outcome {
        Outcome::Synced(lrc_text) => {
            let lines = lrc::parse(&lrc_text);
            if lines.is_empty() {
                Kind::Missing
            } else {
                Kind::Synced(lines)
            }
        }
        Outcome::Plain(text) => {
            let text = text.trim().to_string();
            if text.is_empty() {
                Kind::Missing
            } else {
                Kind::Plain(text)
            }
        }
        Outcome::Missing | Outcome::Unavailable => Kind::Missing,
    }
}

fn classify(status: u16, body: &str, retry_after: Option<&str>, retried: bool) -> Step {
    if status == 429 || status == 503 {
        if retried {
            return Step::Done(Outcome::Unavailable);
        }
        return Step::Retry(Duration::from_secs(retry_secs(retry_after)));
    }
    if status == 404 {
        return Step::Done(Outcome::Missing);
    }
    if status != 200 || body.len() > MAX_BODY {
        return Step::Done(Outcome::Unavailable);
    }
    Step::Done(outcome_from_json(body))
}

fn retry_secs(header: Option<&str>) -> u64 {
    header
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(2)
        .clamp(1, 30)
}

fn outcome_from_json(body: &str) -> Outcome {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Outcome::Unavailable;
    };
    if let Some(lrc_text) = string_field(&value, "syncedLyrics")
        && !lrc::parse(&lrc_text).is_empty()
    {
        return Outcome::Synced(lrc_text);
    }
    if let Some(text) = string_field(&value, "plainLyrics") {
        return Outcome::Plain(text);
    }
    Outcome::Missing
}

fn string_field(value: &serde_json::Value, key: &str) -> Option<String> {
    let text = value.get(key)?.as_str()?;
    if text.trim().is_empty() {
        None
    } else {
        Some(text.trim().to_string())
    }
}

fn retrieve(query: &Query) -> Outcome {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            log::warn!("lyrics: could not start HTTP: {error}");
            return Outcome::Unavailable;
        }
    };
    runtime.block_on(async {
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent(USER_AGENT)
            .build()
        {
            Ok(client) => client,
            Err(error) => {
                log::warn!("lyrics: could not build the HTTP client: {error}");
                return Outcome::Unavailable;
            }
        };
        fetch_http(&client, query).await
    })
}

async fn fetch_http(client: &reqwest::Client, query: &Query) -> Outcome {
    let mut retried = false;
    loop {
        let response = match client.get(ENDPOINT).query(&query_pairs(query)).send().await {
            Ok(response) => response,
            Err(error) => {
                log::warn!("lyrics: request failed: {error}");
                return Outcome::Unavailable;
            }
        };
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let body = match response.text().await {
            Ok(body) => body,
            Err(error) => {
                log::warn!("lyrics: could not read the reply: {error}");
                return Outcome::Unavailable;
            }
        };
        match classify(status, &body, retry_after.as_deref(), retried) {
            Step::Done(outcome) => {
                log::info!(
                    "lyrics: HTTP {status} for {} / {}",
                    query.artist_name,
                    query.track_name
                );
                return outcome;
            }
            Step::Retry(delay) => {
                // Wait out Retry-After, plus up to 30% so retries do not line up.
                let jitter_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|elapsed| {
                        u64::from(elapsed.subsec_millis())
                            % delay.as_secs().saturating_mul(300).max(1)
                    })
                    .unwrap_or(0);
                log::info!("lyrics: HTTP {status}, waiting {}s", delay.as_secs());
                retried = true;
                tokio::time::sleep(delay + Duration::from_millis(jitter_ms)).await;
            }
        }
    }
}

fn cache_body(outcome: &Outcome) -> Option<serde_json::Value> {
    let (kind, body) = match outcome {
        Outcome::Synced(lrc_text) => ("synced", lrc_text.as_str()),
        Outcome::Plain(text) => ("plain", text.as_str()),
        Outcome::Missing => ("missing", ""),
        Outcome::Unavailable => return None,
    };
    Some(serde_json::json!({
        "kind": kind,
        "body": body,
        "stored_unix": unix_now(),
    }))
}

fn read_cache(dir: &Path, key: &Key) -> Option<Outcome> {
    let bytes = std::fs::read(cache_path(dir, key)).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let kind = value.get("kind")?.as_str()?;
    let stored = value.get("stored_unix")?.as_u64()?;
    let body = value.get("body")?.as_str().unwrap_or("");
    match kind {
        "synced" if !lrc::parse(body).is_empty() => Some(Outcome::Synced(body.to_string())),
        "plain" if !body.trim().is_empty() => Some(Outcome::Plain(body.to_string())),
        "missing" if fresh(stored, unix_now(), true) => Some(Outcome::Missing),
        _ => None,
    }
}

fn write_cache(dir: &Path, key: &Key, outcome: &Outcome) -> Result<(), String> {
    let Some(body) = cache_body(outcome) else {
        return Ok(());
    };
    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    let bytes = serde_json::to_vec(&body).map_err(|error| error.to_string())?;
    std::fs::write(cache_path(dir, key), bytes).map_err(|error| error.to_string())
}

fn fresh(stored: u64, now: u64, missing: bool) -> bool {
    if !missing {
        return true;
    }
    now.saturating_sub(stored) < MISSING_TTL_SECS
}

fn cache_path(dir: &Path, key: &Key) -> PathBuf {
    dir.join(cache_name(key))
}

fn cache_name(key: &Key) -> String {
    let prefix = match key.source {
        Source::Spotify => "spotify",
        Source::YouTubeMusic => "youtube",
        Source::Local => "local",
    };
    let mut safe = String::new();
    let mut changed = false;
    for ch in key.id.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            safe.push(ch);
        } else {
            changed = true;
            safe.push('_');
        }
    }
    if safe.is_empty() {
        safe.push_str("track");
        changed = true;
    }
    if safe.len() > 80 {
        safe.truncate(80);
        changed = true;
    }
    if changed {
        format!("{prefix}-{safe}-{:08x}.json", fnv(&key.id))
    } else {
        format!("{prefix}-{safe}.json")
    }
}

fn fnv(text: &str) -> u32 {
    let mut hash = 2_166_136_261u32;
    for byte in text.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(16_777_619);
    }
    hash
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn text_height(lyric: &str) -> f32 {
    let chars = lyric.chars().count().max(1);
    let rows = chars.div_ceil(CHARS_PER_ROW);
    rows as f32 * LINE_SLOT
}

fn scroll_offset(lines: &[Line], index: usize) -> f32 {
    let pin = index.saturating_sub(1);
    let mut y = 0.0;
    for line in lines.iter().take(pin) {
        y += text_height(&line.text);
    }
    y + pin as f32 * ROW_GAP
}

fn scroll_id() -> iced::widget::Id {
    iced::widget::Id::new(SCROLL_ID)
}

fn frame<'a, Message: 'a>(body: Element<'a, Message>) -> Element<'a, Message> {
    container(
        column![
            text("Lyrics")
                .font(crate::theme::HEADING)
                .size(15.0)
                .color(crate::theme::TEXT_PRIMARY),
            body,
        ]
        .spacing(12)
        .width(Fill)
        .height(Fill),
    )
    .padding([16.0, 16.0])
    .width(PANEL_WIDTH)
    .height(Fill)
    .style(lyrics_surface)
    .into()
}

fn quiet<'a, Message: 'a>(label: &'static str) -> Element<'a, Message> {
    text(label)
        .font(crate::theme::BODY)
        .size(BODY_SIZE)
        .color(crate::theme::TEXT_TERTIARY)
        .into()
}

fn plain_block<'a, Message: 'a>(lyric: &'a str) -> Element<'a, Message> {
    scrollable(
        text(lyric)
            .font(crate::theme::BODY)
            .size(BODY_SIZE)
            .color(crate::theme::TEXT_SECONDARY)
            .width(Fill),
    )
    .height(Fill)
    .width(Fill)
    .into()
}

fn synced_block<'a, Message: 'a>(lines: &'a [Line], elapsed: Duration) -> Element<'a, Message> {
    let current = lrc::current_line(lines, elapsed);
    let rows = lines
        .iter()
        .enumerate()
        .map(|(index, line)| lyric_row(line, current == Some(index)));
    scrollable(column(rows).spacing(ROW_GAP).width(Fill))
        .id(scroll_id())
        .height(Fill)
        .width(Fill)
        .into()
}

fn lyric_row<'a, Message: 'a>(line: &'a Line, current: bool) -> Element<'a, Message> {
    let color = if current {
        crate::theme::TEXT_PRIMARY
    } else {
        crate::theme::TEXT_TERTIARY
    };
    let font = if current {
        crate::theme::BODY_MEDIUM
    } else {
        crate::theme::BODY
    };
    let mark = container(Space::new().width(2).height(14))
        .width(2)
        .style(move |_theme: &Theme| {
            if current {
                container::background(crate::theme::ACCENT)
            } else {
                container::Style::default()
            }
        });
    let words = if line.text.is_empty() {
        " "
    } else {
        line.text.as_str()
    };
    row![
        mark,
        text(words)
            .font(font)
            .size(BODY_SIZE)
            .color(color)
            .width(Fill),
    ]
    .spacing(8)
    .align_y(Vertical::Center)
    .into()
}

fn lyrics_surface(_theme: &Theme) -> container::Style {
    container::Style {
        text_color: Some(crate::theme::TEXT_PRIMARY),
        background: Some(crate::theme::PANEL.into()),
        ..container::Style::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now_track(source: Source, album: Option<&str>, millis: u64) -> NowPlaying {
        NowPlaying {
            title: "Night Drive".into(),
            artist: "Ada Vega".into(),
            album: album.map(str::to_string),
            phase: crate::player::Phase::Playing,
            elapsed: Duration::from_secs(12),
            duration: Duration::from_millis(millis),
            error: None,
            note: None,
            can_play: false,
            can_pause: true,
            can_stop: true,
            can_next: false,
            can_previous: false,
            source: Some(source),
            track_id: "track-1".into(),
            loaded: None,
            upcoming: Vec::new(),
        }
    }

    fn sample_lines() -> Vec<Line> {
        lrc::parse(
            "[00:01.00] First\n[00:05.00] Second line that is quite long indeed\n[00:10.00] Third\n",
        )
    }

    #[test]
    fn the_signature_query_uses_name_artist_album_and_rounded_duration() {
        let query = query_from_now(&now_track(Source::Spotify, Some("  Low Glow "), 233_600))
            .expect("spotify track");
        assert_eq!(
            query_pairs(&query),
            vec![
                ("track_name", "Night Drive".into()),
                ("artist_name", "Ada Vega".into()),
                ("album_name", "Low Glow".into()),
                ("duration", "234".into()),
            ]
        );
        assert_eq!(query.key.source, Source::Spotify);
        assert_eq!(query.key.id, "track-1");
    }

    #[test]
    fn a_missing_album_and_an_unusable_duration_are_left_off_the_query() {
        let no_album = query_from_now(&now_track(Source::YouTubeMusic, None, 0)).expect("youtube");
        assert_eq!(
            query_pairs(&no_album)
                .into_iter()
                .map(|(key, _)| key)
                .collect::<Vec<_>>(),
            vec!["track_name", "artist_name"]
        );
        assert_eq!(duration_secs(Duration::from_secs(3_601)), None);
        assert_eq!(duration_secs(Duration::from_secs(3_600)), Some(3_600));
        assert_eq!(duration_secs(Duration::from_millis(1_500)), Some(2));
    }

    #[test]
    fn local_and_untitled_tracks_are_not_looked_up() {
        assert!(query_from_now(&now_track(Source::Local, None, 1_000)).is_none());
        let mut blank = now_track(Source::Spotify, None, 1_000);
        blank.artist.clear();
        assert!(query_from_now(&blank).is_none());
        blank.artist = "Ada Vega".into();
        blank.title.clear();
        assert!(query_from_now(&blank).is_none());
    }

    #[test]
    fn json_picks_synced_then_plain_then_a_miss() {
        let synced = outcome_from_json(
            r#"{"syncedLyrics":"[00:01.00] Hi\n","plainLyrics":"Hi\n","instrumental":false}"#,
        );
        assert_eq!(synced, Outcome::Synced("[00:01.00] Hi".into()));
        let plain = outcome_from_json(r#"{"syncedLyrics":null,"plainLyrics":"Just words"}"#);
        assert_eq!(plain, Outcome::Plain("Just words".into()));
        let missing =
            outcome_from_json(r#"{"syncedLyrics":null,"plainLyrics":null,"instrumental":true}"#);
        assert_eq!(missing, Outcome::Missing);
        let broken_sync =
            outcome_from_json(r#"{"syncedLyrics":"not lrc","plainLyrics":"Fallback"}"#);
        assert_eq!(broken_sync, Outcome::Plain("Fallback".into()));
    }

    #[test]
    fn status_codes_miss_retry_once_and_otherwise_give_up() {
        assert!(matches!(
            classify(404, "", None, false),
            Step::Done(Outcome::Missing)
        ));
        assert!(matches!(
            classify(429, "", Some("9"), false),
            Step::Retry(delay) if delay == Duration::from_secs(9)
        ));
        assert!(matches!(
            classify(503, "", None, true),
            Step::Done(Outcome::Unavailable)
        ));
        assert!(matches!(
            classify(500, "", None, false),
            Step::Done(Outcome::Unavailable)
        ));
        assert!(matches!(
            classify(200, "not-json", None, false),
            Step::Done(Outcome::Unavailable)
        ));
        assert_eq!(retry_secs(None), 2);
        assert_eq!(retry_secs(Some("0")), 1);
        assert_eq!(retry_secs(Some("120")), 30);
        assert_eq!(retry_secs(Some("soon")), 2);
    }

    #[test]
    fn a_hit_is_cached_per_track_and_a_miss_expires() {
        let dir = std::env::temp_dir().join(format!("codevox-mu-lyrics-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let query =
            query_from_now(&now_track(Source::Spotify, Some("Low Glow"), 180_000)).expect("query");
        let lookup = Lookup {
            query: query.clone(),
            cache: Some(dir.clone()),
        };
        let mut calls = 0;
        let loaded = load_with(lookup, |_| {
            calls += 1;
            Outcome::Synced("[00:01.00] Hi".into())
        });
        assert_eq!(calls, 1);
        assert_eq!(loaded.outcome, Outcome::Synced("[00:01.00] Hi".into()));
        let again = Lookup {
            query: query.clone(),
            cache: Some(dir.clone()),
        };
        let cached = load_with(again, |_| {
            calls += 1;
            Outcome::Unavailable
        });
        assert_eq!(calls, 1);
        assert_eq!(cached.outcome, Outcome::Synced("[00:01.00] Hi".into()));

        let name = cache_name(&query.key);
        assert_eq!(
            Path::new(&name).file_name().and_then(|file| file.to_str()),
            Some(name.as_str())
        );
        assert!(!name.contains(".."));

        let odd = Key {
            source: Source::YouTubeMusic,
            id: "../nope".into(),
        };
        let odd_name = cache_name(&odd);
        assert_eq!(Path::new(&odd_name).components().count(), 1);
        assert!(!odd_name.contains('/'));
        assert!(!odd_name.contains(".."));

        assert!(!fresh(0, unix_now(), true));
        let path = cache_path(&dir, &query.key);
        std::fs::write(&path, r#"{"kind":"missing","body":"","stored_unix":0}"#).expect("stale");
        assert_eq!(read_cache(&dir, &query.key), None);
        std::fs::write(
            &path,
            format!(
                r#"{{"kind":"missing","body":"","stored_unix":{}}}"#,
                unix_now()
            ),
        )
        .expect("fresh miss");
        assert_eq!(read_cache(&dir, &query.key), Some(Outcome::Missing));
        // A found lyric is kept even when the timestamp is old.
        std::fs::write(
            &path,
            r#"{"kind":"synced","body":"[00:02.00] Kept","stored_unix":0}"#,
        )
        .expect("old hit");
        assert_eq!(
            read_cache(&dir, &query.key),
            Some(Outcome::Synced("[00:02.00] Kept".into()))
        );
        // A failed lookup must not replace a lyric that is already stored.
        write_cache(&dir, &query.key, &Outcome::Unavailable).expect("skip");
        assert_eq!(
            read_cache(&dir, &query.key),
            Some(Outcome::Synced("[00:02.00] Kept".into()))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_session_follows_the_current_line_and_drops_a_stale_reply() {
        let mut session = Session::default();
        let playing = now_track(Source::YouTubeMusic, None, 180_000);
        let lookup = session.begin(&playing, None).expect("fetch");
        assert!(session.begin(&playing, None).is_none());
        assert!(matches!(session.phase, Phase::Loading));

        let other = Key {
            source: Source::Spotify,
            id: "other".into(),
        };
        assert!(!session.accept(&other, Outcome::Plain("nope".into())));
        assert!(matches!(session.phase, Phase::Loading));

        assert!(session.accept(
            &lookup.query.key,
            Outcome::Synced("[00:01.00] First\n[00:05.00] Second\n[00:10.00] Third".into()),
        ));
        assert_eq!(session.follow(Duration::from_secs(0)), None);
        assert_eq!(session.follow(Duration::from_secs(1)), Some(0.0));
        assert_eq!(session.follow(Duration::from_millis(4_000)), None);
        // The second line is pinned with the first still in view, so the offset stays 0.
        assert_eq!(session.follow(Duration::from_secs(5)), Some(0.0));
        assert_eq!(session.follow(Duration::from_secs(6)), None);
        assert!(session.follow(Duration::from_secs(10)).unwrap_or(0.0) > 0.0);

        assert!(session.accept(&lookup.query.key, Outcome::Plain("one\ntwo".into())));
        assert!(session.follow(Duration::from_secs(3)).is_none());
        assert!(session.accept(&lookup.query.key, Outcome::Missing));
        assert!(matches!(session.phase, Phase::Ready(Kind::Missing)));

        session.clear();
        assert!(session.begin(&NowPlaying::default(), None).is_none());
        assert!(matches!(session.phase, Phase::Idle));
    }

    #[test]
    fn scroll_offset_uses_earlier_lines_and_grows_for_a_long_line() {
        let lines = sample_lines();
        assert_eq!(scroll_offset(&lines, 0), 0.0);
        assert_eq!(scroll_offset(&lines, 1), 0.0);
        let short = text_height("First");
        assert_eq!(scroll_offset(&lines, 2), short + ROW_GAP);
        assert!(text_height(&lines[1].text) > short);
    }

    #[test]
    fn the_panel_builds_for_synced_plain_and_the_quiet_miss() {
        let mut session = Session::default();
        assert!(panel::<()>(&session, Duration::ZERO).is_none());
        session.phase = Phase::Loading;
        assert!(panel::<()>(&session, Duration::ZERO).is_some());
        session.phase = Phase::Ready(Kind::Missing);
        assert!(panel::<()>(&session, Duration::ZERO).is_some());
        session.phase = Phase::Ready(Kind::Plain("words".into()));
        assert!(panel::<()>(&session, Duration::ZERO).is_some());
        session.phase = Phase::Ready(Kind::Synced(sample_lines()));
        assert!(panel::<()>(&session, Duration::from_secs(6)).is_some());
        assert_eq!(NOT_FOUND, "No lyrics found");
    }
}
