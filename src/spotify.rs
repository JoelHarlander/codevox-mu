//! Spotify sign-in and playback through librespot.
//!
//! Web API calls and the PKCE sign-in use CodeVox mU's own client id
//! ([`web_client_id`]). It is a public PKCE client. There is no client secret.
//! The desktop id librespot ships is shared by every librespot app, and Spotify
//! rate-limits that id for Web API calls.
//!
//! librespot 0.8 only completes its client-token and login5 steps for an
//! official Spotify client id. An access token minted for this app's client is
//! rejected, so playback is a second PKCE login with [`desktop_client_id`].
//! Premium sign-in opens the browser twice. A free account never starts that
//! second login.
//!
//! Both refresh tokens, and librespot's reusable credentials, are written under
//! the OS cache directory, mode `0600` where the platform has modes. They are
//! never written into the repo.
//!
//! Playback uses [librespot-playback](https://crates.io/crates/librespot-playback)
//! (MIT). Its rodio backend links an older `alsa-sys` than the YouTube player,
//! so decoded PCM is handed to this app's rodio 0.23 device instead.
//!
//! librespot calls `process::exit` when a session's product info is not
//! Premium. A free account is detected from the Web API profile and never
//! connected. Search still works; the player falls back to YouTube Music.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

use librespot_core::authentication::Credentials;
use librespot_core::cache::Cache;
use librespot_core::session::Session;
use librespot_core::{SessionConfig, SpotifyId, SpotifyUri};
use librespot_oauth::OAuthClientBuilder;
use librespot_playback::audio_backend::{Sink, SinkError, SinkResult};
use librespot_playback::config::PlayerConfig;
use librespot_playback::convert::Converter;
use librespot_playback::decoder::AudioPacket;
use librespot_playback::mixer::NoOpVolume;
use librespot_playback::player::{Player, PlayerEvent};
use librespot_playback::{NUM_CHANNELS, SAMPLE_RATE};

use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, CsrfToken, EndpointNotSet, EndpointSet,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenResponse, TokenUrl,
};

use crate::music::{Error, MusicService, Playlist, Source, Track};
use crate::player::{SpotifyEngine, SpotifyFeed};

/// How long an error row is allowed to get.
const ERROR_CHARS: usize = 160;

/// Local callback. Registered for CodeVox mU's Spotify app, and accepted by
/// librespot's desktop client id. The two logins run one after the other, so
/// they share the port.
const REDIRECT_URI: &str = "http://127.0.0.1:8898/login";

/// Shown in the browser tab after the Web API redirect.
const BROWSER_MESSAGE: &str = "Signed in. You can close this tab and return to CodeVox mU.";

/// Shown after the playback redirect. Premium sign-in opens the browser again
/// for Spotify's desktop client id.
const PLAYBACK_BROWSER_MESSAGE: &str =
    "Playback is signed in. You can close this tab and return to CodeVox mU.";

/// Scopes for the Web API sign-in. The library reads are included so the
/// account is asked once. `streaming` stays on this consent as well.
const SCOPES: &[&str] = &[
    "streaming",
    "user-read-private",
    "user-read-email",
    "user-library-read",
    "playlist-read-private",
    "playlist-read-collaborative",
];

/// The desktop login only mints the token librespot's session accepts.
const PLAYBACK_SCOPES: &[&str] = &["streaming"];

/// CodeVox mU's public PKCE client. Not a secret, and there is no client secret.
const DEFAULT_WEB_CLIENT_ID: &str = "34edc1e59dcb447db61ea7720733cbde";

/// Process environment that overrides the compiled client id.
const RUNTIME_CLIENT_ID_ENV: &str = "CODEVOX_MU_SPOTIFY_CLIENT_ID";

/// One 429 is retried only when Spotify's requested wait fits in this long.
const RATE_LIMIT_RETRY_CAP: Duration = Duration::from_secs(10);

const PROFILE_URL: &str = "https://api.spotify.com/v1/me";
const SEARCH_URL: &str = "https://api.spotify.com/v1/search";
const PLAYLISTS_URL: &str = "https://api.spotify.com/v1/me/playlists";
const SAVED_TRACKS_URL: &str = "https://api.spotify.com/v1/me/tracks";

/// Liked Songs is not a playlist id. The catalogue uses this id, and the
/// window tells it apart from YouTube Music's liked songs by [`Source`].
const LIKED_SONGS_ID: &str = "liked-songs";

/// Spotify's maximum page size for these library endpoints.
const PAGE_LIMIT: &str = "50";

/// User playlists kept from `/v1/me/playlists`. Liked Songs is extra.
const PLAYLIST_CAP: usize = 500;

/// Tracks kept from one playlist or from Liked Songs.
const TRACK_CAP: usize = 1_000;

/// Chunks kept queued in rodio. Each chunk is a fraction of a second.
/// A shorter queue makes skip cut the previous track sooner.
const QUEUED_CHUNKS: usize = 8;

/// What the window shows. No token lives here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccountPhase {
    SignedOut,
    SigningIn,
    /// Premium. `name` is the display name, or the account id.
    SignedIn {
        name: String,
    },
    /// Signed in, and Spotify audio is unavailable. Playback uses YouTube Music.
    NotPremium {
        name: String,
    },
    Failed {
        message: String,
    },
}

/// Colour of the account line. The shell maps these onto the CodeVox tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountTone {
    Muted,
    Normal,
    Warning,
    Error,
}

/// Text and the sign-in button for one [`AccountPhase`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountPresentation {
    pub text: String,
    pub tone: AccountTone,
    /// `true` when the "Sign in to Spotify" control should be pressable.
    pub offer_sign_in: bool,
}

/// Label for the account row. Pure: no account and no network.
pub fn present_account(phase: &AccountPhase) -> AccountPresentation {
    match phase {
        AccountPhase::SignedOut => AccountPresentation {
            text: "Spotify is signed out.".to_string(),
            tone: AccountTone::Muted,
            offer_sign_in: true,
        },
        AccountPhase::SigningIn => AccountPresentation {
            text: "Signing in to Spotify…".to_string(),
            tone: AccountTone::Muted,
            offer_sign_in: false,
        },
        AccountPhase::SignedIn { name } => AccountPresentation {
            text: format!("Signed in as {}", account_name(name)),
            tone: AccountTone::Normal,
            offer_sign_in: false,
        },
        AccountPhase::NotPremium { name } => AccountPresentation {
            text: format!(
                "{} is not Spotify Premium. Tracks play from YouTube Music.",
                account_name(name)
            ),
            tone: AccountTone::Warning,
            offer_sign_in: false,
        },
        AccountPhase::Failed { message } => AccountPresentation {
            text: message.clone(),
            tone: AccountTone::Error,
            offer_sign_in: true,
        },
    }
}

fn account_name(name: &str) -> &str {
    let name = name.trim();
    if name.is_empty() { "Spotify" } else { name }
}

/// Spotify's desktop client id, as librespot selects it for this operating system.
///
/// Playback uses this id. librespot 0.8's client-token and login5 calls fail
/// with an invalid-credential error for any other id, including a token from
/// [`web_client_id`]. This value is public, not a secret.
pub fn desktop_client_id() -> String {
    SessionConfig::default().client_id
}

/// Client id for PKCE sign-in and every Web API request.
///
/// A non-empty [`RUNTIME_CLIENT_ID_ENV`] wins, then the id compiled from
/// `SPOTIFY_CLIENT_ID`, then [`DEFAULT_WEB_CLIENT_ID`].
pub fn web_client_id() -> String {
    resolve_client_id(
        std::env::var(RUNTIME_CLIENT_ID_ENV).ok().as_deref(),
        option_env!("SPOTIFY_CLIENT_ID"),
    )
}

/// Resolution order for [`web_client_id`]. Blank values are skipped.
fn resolve_client_id(runtime: Option<&str>, compiled: Option<&str>) -> String {
    [runtime, compiled]
        .into_iter()
        .find_map(|value| {
            let value = value?.trim();
            (!value.is_empty()).then(|| value.to_string())
        })
        .unwrap_or_else(|| DEFAULT_WEB_CLIENT_ID.to_string())
}

/// librespot session for playback. The client id stays the desktop id even
/// when the Web API token belongs to [`web_client_id`].
fn playback_session_config(device_id: &str) -> SessionConfig {
    SessionConfig {
        device_id: device_id.to_string(),
        client_id: desktop_client_id(),
        ..SessionConfig::default()
    }
}

/// Install ring as rustls's process provider, once.
///
/// librespot's hyper client builds `rustls::ClientConfig` from that provider.
/// This process links rustls with both ring and aws-lc-rs, so rustls refuses
/// to choose and panics on the first playback connection. reqwest does not
/// install the process provider: it passes ring only to its own config. The
/// panic used to happen after `account.json` was already written, and the
/// sign-in thread then died without a reply.
pub fn ensure_playback_tls() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

/// `true` only for a Premium product string from `/v1/me`.
pub fn premium_product(product: &str) -> bool {
    product.trim().eq_ignore_ascii_case("premium")
}

/// A parsed `/v1/me` body. No token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub product: String,
    pub premium: bool,
}

/// Read a `/v1/me` body. Nothing is fetched.
pub fn profile_from_json(body: &str) -> Result<Profile, Error> {
    let value: serde_json::Value = serde_json::from_str(body).map_err(brief)?;
    if let Some(message) = api_error_message(&value) {
        return Err(brief_message(message));
    }
    let product = text_field(&value, "product");
    if product.is_empty() {
        return Err(brief_message(
            "Spotify did not say which plan this account is",
        ));
    }
    let name = {
        let display = text_field(&value, "display_name");
        if !display.is_empty() {
            display
        } else {
            let id = text_field(&value, "id");
            if !id.is_empty() {
                id
            } else {
                "Spotify".to_string()
            }
        }
    };
    Ok(Profile {
        premium: premium_product(&product),
        name,
        product,
    })
}

/// Read a track-search body into tracks. Nothing is fetched.
///
/// Rows without a 22-character Spotify id are skipped. An error payload fails
/// the whole read.
pub fn tracks_from_search_json(body: &str) -> Result<Vec<Track>, Error> {
    let value: serde_json::Value = serde_json::from_str(body).map_err(brief)?;
    if let Some(message) = api_error_message(&value) {
        return Err(brief_message(message));
    }
    let items = value
        .pointer("/tracks/items")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| brief_message("Spotify search did not return tracks"))?;

    let mut tracks = Vec::new();
    for item in items {
        if let Some(track) = track_from_item(item) {
            tracks.push(track);
        }
    }
    Ok(tracks)
}

fn track_from_item(item: &serde_json::Value) -> Option<Track> {
    let id = text_field(item, "id");
    if SpotifyId::from_base62(&id).is_err() {
        return None;
    }
    let title = text_field(item, "name");
    if title.is_empty() {
        return None;
    }
    let artists = item
        .get("artists")
        .and_then(serde_json::Value::as_array)
        .map(|artists| {
            artists
                .iter()
                .filter_map(|artist| {
                    let name = text_field(artist, "name");
                    (!name.is_empty()).then_some(name)
                })
                .collect()
        })
        .unwrap_or_default();
    let album = item
        .pointer("/album/name")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string);
    let duration_ms = item
        .get("duration_ms")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);

    Some(Track {
        id,
        source: Source::Spotify,
        title,
        artists,
        album,
        duration: Duration::from_millis(duration_ms),
    })
}

/// One Web API page. `next` is the absolute URL of the following page.
#[derive(Debug)]
struct ApiPage<T> {
    items: Vec<T>,
    next: Option<String>,
}

/// Read one `/v1/me/playlists` page. Nothing is fetched.
///
/// Rows without a Spotify playlist id are skipped. A blank name becomes
/// "Untitled playlist". An error payload fails the whole page.
fn playlists_from_page_json(body: &str) -> Result<ApiPage<Playlist>, Error> {
    let value = json_value(body)?;
    let items = page_items(&value)?;
    let mut playlists = Vec::new();
    for item in items {
        if let Some(playlist) = playlist_from_item(item) {
            playlists.push(playlist);
        }
    }
    Ok(ApiPage {
        items: playlists,
        next: page_next(&value),
    })
}

/// Read one playlist-items page (`/tracks` or `/items`). Nothing is fetched.
///
/// A row may wrap the recording in `track` or in `item`. Episodes, local
/// files, null entries, and ids the player cannot load are skipped.
fn tracks_from_playlist_page_json(body: &str) -> Result<ApiPage<Track>, Error> {
    tracks_from_collection_page(body)
}

/// Read one `/v1/me/tracks` page. Nothing is fetched.
///
/// Saved tracks use the `track` wrapper. Null and unloadable rows are skipped.
fn tracks_from_saved_page_json(body: &str) -> Result<ApiPage<Track>, Error> {
    tracks_from_collection_page(body)
}

fn tracks_from_collection_page(body: &str) -> Result<ApiPage<Track>, Error> {
    let value = json_value(body)?;
    let items = page_items(&value)?;
    let mut tracks = Vec::new();
    for item in items {
        if let Some(track) = track_from_collection_item(item) {
            tracks.push(track);
        }
    }
    Ok(ApiPage {
        items: tracks,
        next: page_next(&value),
    })
}

fn json_value(body: &str) -> Result<serde_json::Value, Error> {
    let value: serde_json::Value = serde_json::from_str(body).map_err(brief)?;
    if let Some(message) = api_error_message(&value) {
        return Err(brief_message(message));
    }
    Ok(value)
}

fn page_items(value: &serde_json::Value) -> Result<&Vec<serde_json::Value>, Error> {
    value
        .get("items")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| brief_message("Spotify did not return a page"))
}

fn page_next(value: &serde_json::Value) -> Option<String> {
    let next = value.get("next")?.as_str()?.trim();
    if next.is_empty() {
        None
    } else {
        Some(next.to_string())
    }
}

fn playlist_from_item(item: &serde_json::Value) -> Option<Playlist> {
    if !item.is_object() {
        return None;
    }
    let id = text_field(item, "id");
    if !playlist_id_ok(&id) {
        return None;
    }
    let name = text_field(item, "name");
    let name = if name.is_empty() {
        "Untitled playlist".to_string()
    } else {
        name
    };
    Some(Playlist {
        id,
        source: Source::Spotify,
        name,
    })
}

/// A playlist id that can go in a Spotify URL. Base62, no path characters.
fn playlist_id_ok(id: &str) -> bool {
    let len = id.len();
    (1..=64).contains(&len) && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn track_from_collection_item(item: &serde_json::Value) -> Option<Track> {
    if item.get("is_local").and_then(serde_json::Value::as_bool) == Some(true) {
        return None;
    }
    let embedded = item
        .get("item")
        .filter(|value| !value.is_null())
        .or_else(|| item.get("track").filter(|value| !value.is_null()))?;
    let kind = text_field(embedded, "type");
    if !kind.is_empty() && kind != "track" {
        return None;
    }
    track_from_item(embedded)
}

/// Walk `first_url`, then each page's `next`, until the list is finished.
///
/// Stops at `cap` items, when `next` is missing, when `next` is not an
/// `https://api.spotify.com/` URL, or when `next` repeats a URL already
/// requested. The token is only sent to URLs the caller and Spotify named.
fn collect_pages<T, F>(first_url: &str, cap: usize, mut fetch: F) -> Result<Vec<T>, Error>
where
    F: FnMut(&str) -> Result<ApiPage<T>, Error>,
{
    if cap == 0 || first_url.is_empty() {
        return Ok(Vec::new());
    }
    let mut kept = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor = first_url.to_string();
    loop {
        if !seen.insert(cursor.clone()) {
            break;
        }
        let page = fetch(&cursor)?;
        let room = cap.saturating_sub(kept.len());
        if room == 0 {
            break;
        }
        kept.extend(page.items.into_iter().take(room));
        if kept.len() >= cap {
            break;
        }
        let Some(next) = page.next.as_deref().and_then(followable_next) else {
            break;
        };
        cursor = next;
    }
    Ok(kept)
}

/// `next` from Spotify, or nothing. Other hosts are not requested.
fn followable_next(next: &str) -> Option<String> {
    let next = next.trim();
    if next.is_empty() || next.chars().any(char::is_control) || next.contains(' ') {
        return None;
    }
    let rest = next.strip_prefix("https://api.spotify.com/")?;
    if rest.contains('@') {
        return None;
    }
    Some(next.to_string())
}

fn read_pages<T>(
    account: &SpotifyAccount,
    token: &str,
    first_url: &str,
    cap: usize,
    parse: fn(&str) -> Result<ApiPage<T>, Error>,
) -> Result<Vec<T>, Error> {
    collect_pages(first_url, cap, |url| {
        let body = account.http_get(url, token, &[])?;
        parse(&body)
    })
}

fn library_page_url(base: &str) -> String {
    format!("{base}?limit={PAGE_LIMIT}")
}

fn signed_out_library() -> Error {
    brief_message("Spotify is signed out. Sign in to see your library.")
}

fn liked_songs_row() -> Playlist {
    Playlist {
        id: LIKED_SONGS_ID.to_string(),
        source: Source::Spotify,
        name: "Liked Songs".to_string(),
    }
}

/// Spotify's 404 body. `/items` is current; `/tracks` is the older path.
fn replied_not_found(error: &Error) -> bool {
    matches!(error, Error::Unavailable(reason) if reason.contains("replied 404"))
}

/// A Spotify id the player can load. Invalid ids never reach librespot.
pub fn track_uri(id: &str) -> Result<SpotifyUri, String> {
    let id =
        SpotifyId::from_base62(id).map_err(|_| "this Spotify id cannot be played".to_string())?;
    Ok(SpotifyUri::Track { id })
}

fn api_error_message(value: &serde_json::Value) -> Option<String> {
    let message = value
        .pointer("/error/message")
        .and_then(serde_json::Value::as_str)?;
    let status = value
        .pointer("/error/status")
        .and_then(serde_json::Value::as_u64);
    Some(match status {
        Some(429) => rate_limit_message(None),
        Some(status) => format!("Spotify replied {status}: {message}"),
        None => format!("Spotify replied: {message}"),
    })
}

/// What to do with one HTTP 429. `already_retried` stops a second wait.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RateLimitChoice {
    /// Sleep this long, then send the request once more.
    RetryAfter(Duration),
    /// Show this sentence. It does not include the status code.
    GiveUp(String),
}

fn rate_limit_choice(
    retry_after: Option<&str>,
    now: SystemTime,
    already_retried: bool,
) -> RateLimitChoice {
    let delay = retry_after.and_then(|value| parse_retry_after(value, now));
    if !already_retried && let Some(delay) = delay.filter(|delay| *delay <= RATE_LIMIT_RETRY_CAP) {
        return RateLimitChoice::RetryAfter(delay);
    }
    RateLimitChoice::GiveUp(rate_limit_message(delay))
}

/// `Retry-After` as a delay from `now`. Delta-seconds, or an IMF-fixdate.
/// Anything else is `None`, and the caller does not guess a wait.
fn parse_retry_after(header: &str, now: SystemTime) -> Option<Duration> {
    let header = header.trim();
    if header.is_empty() {
        return None;
    }
    if header.bytes().all(|byte| byte.is_ascii_digit()) {
        let seconds: u64 = header.parse().ok()?;
        return Some(Duration::from_secs(seconds));
    }
    let when = http_date(header)?;
    Some(when.duration_since(now).unwrap_or(Duration::ZERO))
}

fn rate_limit_message(wait: Option<Duration>) -> String {
    match wait {
        Some(wait) if wait > RATE_LIMIT_RETRY_CAP => format!(
            "Spotify is limiting requests. Try again in {}.",
            wait_phrase(wait)
        ),
        _ => "Spotify is limiting requests. Wait a moment and try again.".to_string(),
    }
}

fn wait_phrase(wait: Duration) -> String {
    let seconds = wait.as_secs().max(1);
    if seconds < 60 {
        let unit = if seconds == 1 { "second" } else { "seconds" };
        return format!("{seconds} {unit}");
    }
    let minutes = seconds.div_ceil(60);
    let unit = if minutes == 1 { "minute" } else { "minutes" };
    format!("{minutes} {unit}")
}

/// IMF-fixdate, the HTTP-date form Spotify sends (`Sun, 06 Nov 1994 08:49:37 GMT`).
fn http_date(value: &str) -> Option<SystemTime> {
    let mut parts = value.split_whitespace();
    let _weekday = parts.next()?;
    let day: u32 = parts.next()?.parse().ok()?;
    let month = month_number(parts.next()?)?;
    let year: i32 = parts.next()?.parse().ok()?;
    let time = parts.next()?;
    let mut clock = time.split(':');
    let hour: u32 = clock.next()?.parse().ok()?;
    let minute: u32 = clock.next()?.parse().ok()?;
    let second: u32 = clock.next()?.parse().ok()?;
    if clock.next().is_some() {
        return None;
    }
    let zone = parts.next()?;
    if !zone.eq_ignore_ascii_case("GMT") || parts.next().is_some() {
        return None;
    }
    if day == 0 || day > 31 || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let unix = days * 86_400 + i64::from(hour) * 3_600 + i64::from(minute) * 60 + i64::from(second);
    let unix = u64::try_from(unix).ok()?;
    SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(unix))
}

fn month_number(name: &str) -> Option<u32> {
    Some(match name {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    })
}

/// Days since the civil epoch, Howard Hinnant's public-domain algorithm.
fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = (year - era * 400) as u64;
    let shifted = month + if month > 2 { 0 } else { 12 };
    let day_of_year = (153 * (shifted - 3) + 2) / 5 + day - 1;
    let day_of_era =
        year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + u64::from(day_of_year);
    i64::from(era) * 146_097 + day_of_era as i64 - 719_468
}

fn text_field(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Where the refresh token is allowed to live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Desktop {
    Linux,
    Mac,
    Windows,
}

fn this_desktop() -> Desktop {
    if cfg!(windows) {
        Desktop::Windows
    } else if cfg!(target_os = "macos") {
        Desktop::Mac
    } else {
        Desktop::Linux
    }
}

/// Cache directory for a desktop. Relative paths are refused so a token
/// cannot land in the working tree.
fn cache_dir_from(desktop: Desktop, var: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let absolute = |key: &str| {
        let value = var(key).filter(|value| !value.trim().is_empty())?;
        // Judge the env value itself. `Path` rewrites `/` to `\` on Windows,
        // so a later `starts_with('/')` would reject a Linux cache root there.
        is_absolute_for(desktop, &value).then(|| PathBuf::from(value))
    };

    let root = match desktop {
        Desktop::Windows => absolute("LOCALAPPDATA").or_else(|| absolute("APPDATA"))?,
        Desktop::Mac => absolute("HOME")?.join("Library").join("Caches"),
        Desktop::Linux => absolute("XDG_CACHE_HOME")
            .or_else(|| absolute("HOME").map(|home| home.join(".cache")))?,
    };
    Some(root.join("codevox-mu"))
}

/// Absolute for that desktop, not for the machine running the check.
///
/// `Path::is_absolute` follows the host, and on Windows `Path` displays a
/// rooted `/var/cache` path with backslashes. The rules look at the original
/// text so a Linux cache root still counts as absolute in a Windows test.
fn is_absolute_for(desktop: Desktop, path: &str) -> bool {
    match desktop {
        Desktop::Linux | Desktop::Mac => path.starts_with('/'),
        Desktop::Windows => {
            let bytes = path.as_bytes();
            let drive = bytes.len() >= 3
                && bytes[0].is_ascii_alphabetic()
                && bytes[1] == b':'
                && (bytes[2] == b'\\' || bytes[2] == b'/');
            drive || path.starts_with(r"\\") || path.starts_with("//")
        }
    }
}

fn cache_dir() -> Option<PathBuf> {
    cache_dir_from(this_desktop(), |key| std::env::var(key).ok())
}

fn account_path(cache: &Path) -> PathBuf {
    cache.join("account.json")
}

fn librespot_path(cache: &Path) -> PathBuf {
    cache.join("librespot")
}

#[derive(Clone)]
struct SavedAccount {
    device_id: String,
    display_name: String,
    product: String,
    /// Web API refresh token, minted for [`web_client_id`].
    refresh_token: String,
    /// Playback refresh token, minted for [`desktop_client_id`]. Empty when
    /// the account is not Premium.
    playback_refresh_token: String,
}

impl std::fmt::Debug for SavedAccount {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SavedAccount")
            .field("device_id", &self.device_id)
            .field("display_name", &self.display_name)
            .field("product", &self.product)
            .field("refresh_token", &"[redacted]")
            .field("playback_refresh_token", &"[redacted]")
            .finish()
    }
}

fn read_saved(path: &Path) -> Option<SavedAccount> {
    let text = fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let refresh_token = text_field(&value, "refresh_token");
    if refresh_token.is_empty() {
        return None;
    }
    Some(SavedAccount {
        device_id: text_field(&value, "device_id"),
        display_name: text_field(&value, "display_name"),
        product: text_field(&value, "product"),
        refresh_token,
        playback_refresh_token: text_field(&value, "playback_refresh_token"),
    })
}

fn write_saved(path: &Path, saved: &SavedAccount) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        restrict_dir(parent)?;
    }
    let body = serde_json::json!({
        "device_id": saved.device_id,
        "display_name": saved.display_name,
        "product": saved.product,
        "refresh_token": saved.refresh_token,
        "playback_refresh_token": saved.playback_refresh_token,
    });
    let text = serde_json::to_string(&body).map_err(|error| error.to_string())?;
    write_private(path, text.as_bytes())
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|error| error.to_string())?;
    file.write_all(bytes).map_err(|error| error.to_string())?;
    Ok(())
}

fn restrict_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn scrub(message: &str, secrets: &[&str]) -> String {
    let mut cleaned = message.to_string();
    for secret in secrets {
        if secret.len() >= 8 {
            cleaned = cleaned.replace(secret, "[redacted]");
        }
    }
    brief_owned(cleaned)
}

fn brief_owned(message: String) -> String {
    let line = message.lines().next().unwrap_or("unknown error").trim();
    let line = if line.is_empty() {
        "unknown error"
    } else {
        line
    };
    let mut brief: String = line.chars().take(ERROR_CHARS).collect();
    if line.chars().count() > ERROR_CHARS {
        brief.push('…');
    }
    brief
}

fn brief(error: impl std::fmt::Display) -> Error {
    brief_message(error.to_string())
}

fn brief_message(message: impl Into<String>) -> Error {
    Error::Unavailable(brief_owned(message.into()))
}

struct Inner {
    epoch: u64,
    access_token: Option<String>,
    access_until: Option<Instant>,
    refresh_token: Option<String>,
    session: Option<Session>,
    premium: bool,
    name: String,
    device_id: String,
    failure: Option<String>,
}

impl Inner {
    fn signed_out() -> Self {
        Self {
            epoch: 0,
            access_token: None,
            access_until: None,
            refresh_token: None,
            session: None,
            premium: false,
            name: String::new(),
            device_id: String::new(),
            failure: None,
        }
    }

    fn phase(&self) -> AccountPhase {
        if let Some(message) = &self.failure {
            return AccountPhase::Failed {
                message: message.clone(),
            };
        }
        if self.refresh_token.is_none() && self.access_token.is_none() {
            return AccountPhase::SignedOut;
        }
        if self.premium && self.session.is_some() {
            AccountPhase::SignedIn {
                name: self.name.clone(),
            }
        } else if self.refresh_token.is_some() && !self.premium {
            AccountPhase::NotPremium {
                name: self.name.clone(),
            }
        } else if self.failure.is_none() && self.refresh_token.is_some() {
            // Premium, but the session is not up yet. The caller is still connecting.
            AccountPhase::SignedIn {
                name: self.name.clone(),
            }
        } else {
            AccountPhase::SignedOut
        }
    }
}

/// Shared Spotify account. Cheap to clone via [`Arc`]. Creating one does not
/// touch the network, the disk, or an audio device.
#[derive(Clone)]
pub struct SpotifyAccount {
    inner: Arc<Mutex<Inner>>,
    runtime: Arc<Mutex<Option<tokio::runtime::Runtime>>>,
    client: Arc<Mutex<Option<reqwest::Client>>>,
}

impl SpotifyAccount {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::signed_out())),
            runtime: Arc::new(Mutex::new(None)),
            client: Arc::new(Mutex::new(None)),
        }
    }

    /// Load a saved sign-in, if there is one. No file means signed out, and
    /// no network. A saved free account is not connected to librespot.
    pub fn restore(&self) -> AccountPhase {
        log::info!("spotify: restoring a saved sign-in");
        let epoch = self.snapshot_epoch();
        let Some(cache) = cache_dir() else {
            log::info!("spotify: no cache directory, staying signed out");
            return self.current_phase();
        };
        let Some(saved) = read_saved(&account_path(&cache)) else {
            log::info!("spotify: no saved account");
            return self.current_phase();
        };
        if !self.still(epoch) {
            return self.current_phase();
        }
        self.finish_login(
            epoch,
            &saved.refresh_token,
            &saved.playback_refresh_token,
            &saved.device_id,
            true,
        )
    }

    /// Browser sign-in for the Web API. A Premium account then approves
    /// playback with Spotify's desktop client. Blocks until the redirect
    /// arrives or the flow fails. Check the cache directory first so a failed
    /// save does not follow a completed login.
    pub fn sign_in(&self) -> AccountPhase {
        if cache_dir().is_none() {
            return AccountPhase::Failed {
                message: "Couldn't find a cache directory for the Spotify sign-in.".to_string(),
            };
        }
        let epoch = self.bump_epoch();
        log::info!("spotify sign-in: requesting web authorization");
        let token = match interactive_token(&web_client_id(), SCOPES, BROWSER_MESSAGE) {
            Ok(token) => token,
            Err(error) => return self.fail(epoch, error),
        };
        log::info!("spotify sign-in: web authorization finished");
        if !self.still(epoch) {
            return self.current_phase();
        }
        self.finish_login(epoch, &token.refresh_token, "", "", false)
    }

    pub fn can_play(&self) -> bool {
        let guard = self.lock();
        guard.premium && guard.session.is_some()
    }

    pub fn session(&self) -> Option<Session> {
        self.lock().session.clone()
    }

    fn finish_login(
        &self,
        epoch: u64,
        refresh_token: &str,
        playback_refresh_token: &str,
        device_id: &str,
        prefer_saved_credentials: bool,
    ) -> AccountPhase {
        let client = match oauth_client() {
            Ok(client) => client,
            Err(error) => return self.fail(epoch, error.to_string()),
        };
        let refreshed = match client.refresh_token(refresh_token) {
            Ok(token) => token,
            Err(error) => {
                // A token saved before the Web API client changed cannot be
                // refreshed. Ask for a new sign-in instead of the raw reply.
                let message = if prefer_saved_credentials {
                    "Spotify sign-in needs to be renewed.".to_string()
                } else {
                    scrub(&error.to_string(), &[refresh_token])
                };
                return self.fail(epoch, message);
            }
        };
        // Spotify omits the refresh token on some refreshes. Keep the one we have.
        let refresh_token = if refreshed.refresh_token.is_empty() {
            refresh_token.to_string()
        } else {
            refreshed.refresh_token
        };
        let profile = match self.profile_body(&refreshed.access_token) {
            Ok(body) => match profile_from_json(&body) {
                Ok(profile) => profile,
                Err(error) => {
                    return self.fail(epoch, scrub(&error.to_string(), &[&refreshed.access_token]));
                }
            },
            Err(error) => {
                return self.fail(
                    epoch,
                    scrub(
                        &error.to_string(),
                        &[&refreshed.access_token, &refresh_token],
                    ),
                );
            }
        };
        if !self.still(epoch) {
            return self.current_phase();
        }

        // Playback is a different client. Do it before saving, so a cancelled
        // browser does not leave a Premium account with no playback token.
        let (playback_access, playback_refresh) = if profile.premium {
            match playback_credentials(playback_refresh_token, !prefer_saved_credentials) {
                Ok(pair) => pair,
                Err(error) => {
                    return self.fail(
                        epoch,
                        scrub(
                            &error,
                            &[
                                &refresh_token,
                                &refreshed.access_token,
                                playback_refresh_token,
                            ],
                        ),
                    );
                }
            }
        } else {
            (String::new(), String::new())
        };
        if !self.still(epoch) {
            return self.current_phase();
        }

        let device_id = if device_id.is_empty() {
            SessionConfig::default().device_id
        } else {
            device_id.to_string()
        };
        let saved = SavedAccount {
            device_id: device_id.clone(),
            display_name: profile.name.clone(),
            product: profile.product.clone(),
            refresh_token: refresh_token.clone(),
            playback_refresh_token: playback_refresh.clone(),
        };
        let Some(cache) = cache_dir() else {
            return self.fail(
                epoch,
                "Couldn't find a cache directory for the Spotify sign-in.".to_string(),
            );
        };
        if let Err(error) = write_saved(&account_path(&cache), &saved) {
            return self.fail(
                epoch,
                scrub(
                    &error,
                    &[
                        &refresh_token,
                        &refreshed.access_token,
                        &playback_access,
                        &playback_refresh,
                    ],
                ),
            );
        }
        log::info!("spotify sign-in: account saved");

        // Free and unknown plans must not reach Session::connect. librespot
        // exits the process when the product info is not Premium.
        log::info!(
            "spotify sign-in: profile {} ({})",
            profile.name,
            profile.product
        );
        if !profile.premium {
            self.commit(epoch, |inner| {
                inner.access_token = Some(refreshed.access_token.clone());
                inner.access_until = Some(refreshed.expires_at);
                inner.refresh_token = Some(refresh_token.clone());
                inner.session = None;
                inner.premium = false;
                inner.name = profile.name.clone();
                inner.device_id = device_id.clone();
                inner.failure = None;
            });
            return self.current_phase();
        }

        match self.connect(
            &cache,
            &device_id,
            &playback_access,
            &playback_refresh,
            prefer_saved_credentials,
        ) {
            Ok(session) => {
                log::info!("spotify sign-in: playback session connected");
                self.commit(epoch, |inner| {
                    inner.access_token = Some(refreshed.access_token.clone());
                    inner.access_until = Some(refreshed.expires_at);
                    inner.refresh_token = Some(refresh_token.clone());
                    inner.session = Some(session);
                    inner.premium = true;
                    inner.name = profile.name.clone();
                    inner.device_id = device_id;
                    inner.failure = None;
                });
                self.current_phase()
            }
            Err(error) => self.fail(epoch, error),
        }
    }

    fn connect(
        &self,
        cache: &Path,
        device_id: &str,
        access_token: &str,
        refresh_token: &str,
        prefer_saved_credentials: bool,
    ) -> Result<Session, String> {
        // Before Session::connect. The hyper client panics without this.
        ensure_playback_tls();
        log::info!("spotify: connecting the playback session");
        let cred_dir = librespot_path(cache);
        restrict_dir(&cred_dir)?;
        let store = Cache::new(Some(&cred_dir), None, None, None)
            .map_err(|error| scrub(&error.to_string(), &[access_token, refresh_token]))?;
        restrict_credentials(&cred_dir);
        let cached = prefer_saved_credentials
            .then(|| store.credentials())
            .flatten();
        let credentials = if let Some(credentials) = cached {
            credentials
        } else if !access_token.is_empty() {
            Credentials::with_access_token(access_token)
        } else {
            return Err("Spotify playback needs you to sign in again.".to_string());
        };
        let config = playback_session_config(device_id);
        let handle = self.handle().map_err(|error| error.to_string())?;
        let access_token = access_token.to_string();
        let refresh_token = refresh_token.to_string();
        let session = handle.block_on(async move {
            let session = Session::new(config, Some(store));
            session
                .connect(credentials, true)
                .await
                .map_err(|error| scrub(&error.to_string(), &[&access_token, &refresh_token]))?;
            Ok::<Session, String>(session)
        })?;
        restrict_credentials(&cred_dir);
        Ok(session)
    }

    fn profile_body(&self, access_token: &str) -> Result<String, Error> {
        self.http_get(PROFILE_URL, access_token, &[])
    }

    fn http_get(
        &self,
        url: &str,
        access_token: &str,
        query: &[(&str, &str)],
    ) -> Result<String, Error> {
        let handle = self.handle()?;
        let client = self.http_client()?;
        let url = url.to_string();
        let access_token = access_token.to_string();
        let query: Vec<(String, String)> = query
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        let result = handle.block_on(async move {
            let mut retried = false;
            loop {
                let response = client
                    .get(&url)
                    .bearer_auth(&access_token)
                    .query(&query)
                    .send()
                    .await
                    .map_err(|error| scrub(&error.to_string(), &[&access_token]))?;
                let status = response.status();
                log::info!("spotify web api {} {}", status.as_u16(), path_for_log(&url));
                if status != reqwest::StatusCode::TOO_MANY_REQUESTS {
                    return response
                        .text()
                        .await
                        .map_err(|error| scrub(&error.to_string(), &[&access_token]));
                }
                let retry_after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                let _ = response.bytes().await;
                match rate_limit_choice(retry_after.as_deref(), SystemTime::now(), retried) {
                    RateLimitChoice::RetryAfter(delay) => {
                        tokio::time::sleep(delay).await;
                        retried = true;
                    }
                    RateLimitChoice::GiveUp(message) => return Err(message),
                }
            }
        });
        // The async block captured access_token. Rebuild a scrub from the
        // caller's copy: the error strings were already scrubbed inside.
        result.map_err(brief_message)
    }

    fn handle(&self) -> Result<tokio::runtime::Handle, Error> {
        let mut guard = self
            .runtime
            .lock()
            .map_err(|_| brief_message("Spotify state was poisoned"))?;
        if guard.is_none() {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_io()
                .enable_time()
                .thread_name("spotify")
                .build()
                .map_err(|error| brief_message(format!("could not start Spotify: {error}")))?;
            *guard = Some(runtime);
        }
        Ok(guard
            .as_ref()
            .expect("runtime was just set")
            .handle()
            .clone())
    }

    fn http_client(&self) -> Result<reqwest::Client, Error> {
        let mut guard = self
            .client
            .lock()
            .map_err(|_| brief_message("Spotify state was poisoned"))?;
        if guard.is_none() {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .user_agent("codevox-mu")
                .build()
                .map_err(|error| brief_message(format!("could not start Spotify: {error}")))?;
            *guard = Some(client);
        }
        Ok(guard.clone().expect("client was just set"))
    }

    fn bearer(&self) -> Result<Option<String>, Error> {
        let (refresh, current, fresh) = {
            let guard = self.lock();
            let fresh = guard
                .access_until
                .is_some_and(|until| until > Instant::now() + Duration::from_secs(30));
            (
                guard.refresh_token.clone(),
                guard.access_token.clone(),
                fresh,
            )
        };
        let Some(refresh) = refresh else {
            return Ok(None);
        };
        if fresh {
            return Ok(current);
        }
        let client = oauth_client()?;
        let token = client
            .refresh_token(&refresh)
            .map_err(|error| brief_message(scrub(&error.to_string(), &[&refresh])))?;
        let mut guard = self.lock_mut();
        guard.access_token = Some(token.access_token.clone());
        guard.access_until = Some(token.expires_at);
        if !token.refresh_token.is_empty() {
            guard.refresh_token = Some(token.refresh_token);
        }
        Ok(Some(token.access_token))
    }

    fn snapshot_epoch(&self) -> u64 {
        self.lock().epoch
    }

    fn bump_epoch(&self) -> u64 {
        let mut guard = self.lock_mut();
        guard.epoch = guard.epoch.saturating_add(1);
        guard.epoch
    }

    fn still(&self, epoch: u64) -> bool {
        self.lock().epoch == epoch
    }

    fn current_phase(&self) -> AccountPhase {
        self.lock().phase()
    }

    fn commit(&self, epoch: u64, update: impl FnOnce(&mut Inner)) -> bool {
        let mut guard = self.lock_mut();
        if guard.epoch != epoch {
            return false;
        }
        update(&mut guard);
        true
    }

    fn fail(&self, epoch: u64, message: String) -> AccountPhase {
        let message = brief_owned(message);
        log::error!("spotify: {}", crate::diag::redact(&message));
        self.commit(epoch, |inner| {
            inner.session = None;
            inner.premium = false;
            inner.failure = Some(message.clone());
        });
        if self.still(epoch) {
            AccountPhase::Failed { message }
        } else {
            self.current_phase()
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_mut(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.lock()
    }
}

fn path_for_log(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

type WebClient =
    BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;

struct Minted {
    access_token: String,
    refresh_token: String,
}

/// Browser PKCE login. The authorization URL is recorded, then opened.
///
/// The listener lives on this thread and is dropped before the function
/// returns, so the playback login can bind the same port afterwards.
fn interactive_token(
    client_id: &str,
    scopes: &[&str],
    browser_message: &str,
) -> Result<Minted, String> {
    let client = web_client(client_id)?;
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let scopes: Vec<Scope> = scopes
        .iter()
        .copied()
        .map(|scope| Scope::new(scope.to_string()))
        .collect();
    let (url, _csrf) = client
        .authorize_url(CsrfToken::new_random)
        .add_scopes(scopes)
        .set_pkce_challenge(challenge)
        .url();
    crate::diag::record_auth_url(url.as_str());
    // The default browser stays the one that opens. Dropping the handle
    // detaches that thread; it does not cancel the redirect listener.
    let _browser = open::that_in_background(url.as_str());
    log::info!("spotify auth: waiting for the browser redirect on 127.0.0.1:8898");
    let code = listen_for_code(browser_message)?;
    exchange_code(client, code, verifier)
}

fn web_client(client_id: &str) -> Result<WebClient, String> {
    let auth_url = AuthUrl::new("https://accounts.spotify.com/authorize".to_string())
        .map_err(|_| "Spotify's authorization URL was rejected".to_string())?;
    let token_url = TokenUrl::new("https://accounts.spotify.com/api/token".to_string())
        .map_err(|_| "Spotify's token URL was rejected".to_string())?;
    let redirect = RedirectUrl::new(REDIRECT_URI.to_string())
        .map_err(|_| "the Spotify redirect URL was rejected".to_string())?;
    Ok(BasicClient::new(ClientId::new(client_id.to_string()))
        .set_auth_uri(auth_url)
        .set_token_uri(token_url)
        .set_redirect_uri(redirect))
}

fn listen_for_code(message: &str) -> Result<String, String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:8898").map_err(|error| {
        format!("could not listen on 127.0.0.1:8898 for the Spotify redirect ({error})")
    })?;
    let (mut stream, _) = listener
        .accept()
        .map_err(|error| format!("the Spotify redirect closed before it sent a code ({error})"))?;
    let mut reader = std::io::BufReader::new(&mut stream);
    let mut request_line = String::new();
    std::io::BufRead::read_line(&mut reader, &mut request_line)
        .map_err(|_| "could not read the Spotify redirect".to_string())?;
    let target = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| "the Spotify redirect was not an HTTP request".to_string())?;
    let code = auth_code_from_target(target)?;
    let body = message.as_bytes();
    let header = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(header.as_bytes())
        .and_then(|()| stream.write_all(body))
        .and_then(|()| stream.flush())
        .map_err(|_| "could not answer the Spotify redirect".to_string())?;
    drop(stream);
    drop(listener);
    Ok(code)
}

fn auth_code_from_target(target: &str) -> Result<String, String> {
    let query = target.split_once('?').map(|(_, query)| query).unwrap_or("");
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if key == "code" {
            let code = percent_decode(value);
            if code.is_empty() {
                return Err("Spotify did not return an authorization code.".to_string());
            }
            return Ok(code);
        }
    }
    Err("Spotify's redirect did not include an authorization code.".to_string())
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn exchange_code(
    client: WebClient,
    code: String,
    verifier: PkceCodeVerifier,
) -> Result<Minted, String> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("spotify-token".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_io()
                    .enable_time()
                    .build()
                    .map_err(|error| format!("could not start the token exchange: {error}"))?;
                let http = reqwest::Client::builder()
                    .timeout(Duration::from_secs(20))
                    .user_agent("codevox-mu")
                    .build()
                    .map_err(|error| format!("could not start the token exchange: {error}"))?;
                runtime.block_on(async move {
                    client
                        .exchange_code(AuthorizationCode::new(code.clone()))
                        .set_pkce_verifier(verifier)
                        .request_async(&http)
                        .await
                        .map_err(|error| scrub(&error.to_string(), &[code.as_str()]))
                })
            }));
            let _ = sender.send(result);
        });
    if spawned.is_err() {
        return Err("could not start the Spotify token exchange".to_string());
    }
    let result = receiver
        .recv()
        .map_err(|_| "the Spotify token exchange stopped before it finished".to_string())?;
    let token = match result {
        Ok(token) => token?,
        Err(_) => return Err("the Spotify token exchange hit an internal error".to_string()),
    };
    let refresh_token = token
        .refresh_token()
        .map(|token| token.secret().clone())
        .unwrap_or_default();
    let access_token = token.access_token().secret().clone();
    if access_token.is_empty() || refresh_token.is_empty() {
        return Err("Spotify did not return a refresh token.".to_string());
    }
    Ok(Minted {
        access_token,
        refresh_token,
    })
}

fn oauth_client() -> Result<librespot_oauth::OAuthClient, Error> {
    OAuthClientBuilder::new(&web_client_id(), REDIRECT_URI, SCOPES.to_vec())
        .build()
        .map_err(brief)
}

fn playback_oauth_client(open_browser: bool) -> Result<librespot_oauth::OAuthClient, Error> {
    let builder =
        OAuthClientBuilder::new(&desktop_client_id(), REDIRECT_URI, PLAYBACK_SCOPES.to_vec());
    let builder = if open_browser {
        builder
            .open_in_browser()
            .with_custom_message(PLAYBACK_BROWSER_MESSAGE)
    } else {
        builder
    };
    builder.build().map_err(brief)
}

/// `(access token, refresh token)` for the desktop playback login.
///
/// A saved refresh is used when Spotify still accepts it. Restore never opens
/// a browser. Without a token, the session uses librespot's cached credentials.
/// A fresh Premium sign-in opens the browser.
fn playback_credentials(
    refresh_token: &str,
    interactive: bool,
) -> Result<(String, String), String> {
    if !refresh_token.is_empty() {
        if let Some(refreshed) = refreshed_playback(refresh_token) {
            return Ok(refreshed);
        }
        if !interactive {
            return Ok((String::new(), refresh_token.to_string()));
        }
    }
    if !interactive {
        return Ok((String::new(), String::new()));
    }
    log::info!("spotify sign-in: requesting playback authorization");
    let token = interactive_token(
        &desktop_client_id(),
        PLAYBACK_SCOPES,
        PLAYBACK_BROWSER_MESSAGE,
    )
    .map_err(|error| scrub(&error, &[refresh_token]))?;
    if token.access_token.is_empty() {
        return Err("Spotify did not return a playback token.".to_string());
    }
    log::info!("spotify sign-in: playback authorization finished");
    Ok((token.access_token, token.refresh_token))
}

fn refreshed_playback(refresh_token: &str) -> Option<(String, String)> {
    let client = playback_oauth_client(false).ok()?;
    let token = client.refresh_token(refresh_token).ok()?;
    let refresh = if token.refresh_token.is_empty() {
        refresh_token.to_string()
    } else {
        token.refresh_token
    };
    Some((token.access_token, refresh))
}

fn restrict_credentials(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("credentials.json");
        if path.is_file() {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
}

/// Catalogue backed by the signed-in account. Signed out searches are empty
/// and do not use the network. Library reads use the same token, client, and
/// 429 handling as search. Signed out library reads do not use the network.
pub struct SpotifyCatalogue {
    account: SpotifyAccount,
}

impl SpotifyCatalogue {
    pub fn new(account: SpotifyAccount) -> Self {
        Self { account }
    }
}

impl MusicService for SpotifyCatalogue {
    fn search(&self, query: &str) -> Result<Vec<Track>, Error> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let Some(token) = self.account.bearer()? else {
            return Ok(Vec::new());
        };
        let body = self.account.http_get(
            SEARCH_URL,
            &token,
            &[("type", "track"), ("q", query), ("limit", "10")],
        )?;
        tracks_from_search_json(&body)
    }

    fn resolve(&self, _id: &str) -> Result<Track, Error> {
        Err(Error::NotFound)
    }

    fn playlists(&self) -> Result<Vec<Playlist>, Error> {
        let Some(token) = self.account.bearer()? else {
            return Err(signed_out_library());
        };
        let url = library_page_url(PLAYLISTS_URL);
        let mut rows = vec![liked_songs_row()];
        for playlist in read_pages(
            &self.account,
            &token,
            &url,
            PLAYLIST_CAP,
            playlists_from_page_json,
        )? {
            if playlist.id != LIKED_SONGS_ID {
                rows.push(playlist);
            }
        }
        Ok(rows)
    }

    fn playlist_tracks(&self, playlist_id: &str) -> Result<Vec<Track>, Error> {
        if playlist_id != LIKED_SONGS_ID && !playlist_id_ok(playlist_id) {
            return Err(Error::NotFound);
        }
        let Some(token) = self.account.bearer()? else {
            return Err(signed_out_library());
        };
        if playlist_id == LIKED_SONGS_ID {
            let url = library_page_url(SAVED_TRACKS_URL);
            return read_pages(
                &self.account,
                &token,
                &url,
                TRACK_CAP,
                tracks_from_saved_page_json,
            );
        }
        let items = library_page_url(&format!(
            "https://api.spotify.com/v1/playlists/{playlist_id}/items"
        ));
        match read_pages(
            &self.account,
            &token,
            &items,
            TRACK_CAP,
            tracks_from_playlist_page_json,
        ) {
            Err(error) if replied_not_found(&error) => {
                let tracks = library_page_url(&format!(
                    "https://api.spotify.com/v1/playlists/{playlist_id}/tracks"
                ));
                read_pages(
                    &self.account,
                    &token,
                    &tracks,
                    TRACK_CAP,
                    tracks_from_playlist_page_json,
                )
            }
            other => other,
        }
    }
}

/// librespot player plus the rodio device. The device opens on the first
/// [`SpotifyEngine::load`], never at construction, so `--smoke` stays silent.
pub struct LibrespotEngine {
    account: SpotifyAccount,
    player: Option<Arc<Player>>,
    events: Option<librespot_playback::player::PlayerEventChannel>,
    position: Duration,
    finished: bool,
    failed: Option<String>,
    stop_requested: bool,
    /// Last position written to the log, so a 200 ms tick does not fill it.
    logged_position_ms: Option<u32>,
}

impl LibrespotEngine {
    pub fn new(account: SpotifyAccount) -> Self {
        Self {
            account,
            player: None,
            events: None,
            position: Duration::ZERO,
            finished: false,
            failed: None,
            stop_requested: false,
            logged_position_ms: None,
        }
    }

    fn ensure_player(&mut self, session: Session) -> Result<(), String> {
        if let Some(player) = &self.player {
            player.set_session(session);
            return Ok(());
        }

        let gate = Arc::new(DeviceGate::new());
        let report = Arc::clone(&gate);
        // A new track should cut the previous one. Gapless would keep the old sink running.
        let config = PlayerConfig {
            gapless: false,
            position_update_interval: Some(Duration::from_millis(200)),
            ..PlayerConfig::default()
        };
        let player = Player::new(config, session, Box::new(NoOpVolume), move || {
            let sink = match PcmSink::open() {
                Ok(sink) => {
                    report.finish(Ok(()));
                    sink
                }
                Err(error) => {
                    report.finish(Err(error));
                    PcmSink::closed()
                }
            };
            Box::new(sink) as Box<dyn Sink>
        });
        gate.wait()?;
        self.events = Some(player.get_player_event_channel());
        self.player = Some(player);
        Ok(())
    }

    fn note_event(&mut self, event: PlayerEvent) {
        match &event {
            PlayerEvent::Playing { position_ms, .. } => {
                log::info!("spotify playback: playing at {position_ms}ms");
            }
            PlayerEvent::Paused { position_ms, .. } => {
                log::info!("spotify playback: paused at {position_ms}ms");
            }
            PlayerEvent::Stopped { .. } => {
                log::info!("spotify playback: stopped");
            }
            PlayerEvent::EndOfTrack { .. } => {
                log::info!("spotify playback: end of track");
            }
            PlayerEvent::Unavailable { .. } => {
                log::error!("spotify playback: track unavailable");
            }
            PlayerEvent::PositionChanged { position_ms, .. } => {
                let log_it = self
                    .logged_position_ms
                    .is_none_or(|previous| position_ms.saturating_sub(previous) >= 1_000);
                if log_it {
                    self.logged_position_ms = Some(*position_ms);
                    log::info!("spotify playback: position {position_ms}ms");
                }
            }
            _ => {}
        }
        match event {
            PlayerEvent::Playing { position_ms, .. }
            | PlayerEvent::Paused { position_ms, .. }
            | PlayerEvent::PositionChanged { position_ms, .. }
            | PlayerEvent::PositionCorrection { position_ms, .. }
            | PlayerEvent::Seeked { position_ms, .. }
            | PlayerEvent::Loading { position_ms, .. } => {
                self.position = Duration::from_millis(u64::from(position_ms));
            }
            PlayerEvent::EndOfTrack { .. } => {
                self.finished = true;
            }
            PlayerEvent::Unavailable { .. } => {
                self.failed = Some("Spotify could not load this track".to_string());
            }
            PlayerEvent::Stopped { .. } => {
                if self.stop_requested {
                    self.stop_requested = false;
                } else {
                    self.finished = true;
                }
            }
            _ => {}
        }
    }
}

impl SpotifyEngine for LibrespotEngine {
    fn can_play(&self) -> bool {
        self.account.can_play()
    }

    fn load(&mut self, track_id: &str) -> Result<(), String> {
        let session = self
            .account
            .session()
            .ok_or_else(|| "Spotify is not connected".to_string())?;
        let uri = track_uri(track_id)?;
        self.ensure_player(session)?;
        self.finished = false;
        self.failed = None;
        self.stop_requested = false;
        self.position = Duration::ZERO;
        if let Some(player) = &self.player {
            player.load(uri, true, 0);
        }
        Ok(())
    }

    fn play(&mut self) {
        self.finished = false;
        if let Some(player) = &self.player {
            player.play();
        }
    }

    fn pause(&mut self) {
        if let Some(player) = &self.player {
            player.pause();
        }
    }

    fn stop(&mut self) {
        self.stop_requested = true;
        self.finished = false;
        if let Some(player) = &self.player {
            player.stop();
        }
    }

    fn poll(&mut self) -> SpotifyFeed {
        let mut pending = Vec::new();
        let mut disconnected = false;
        if let Some(events) = self.events.as_mut() {
            loop {
                match events.try_recv() {
                    Ok(event) => pending.push(event),
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                }
            }
        }
        let mut playing = false;
        for event in pending {
            if matches!(event, PlayerEvent::Playing { .. }) {
                playing = true;
            }
            self.note_event(event);
        }
        if disconnected {
            log::error!("spotify playback: event channel closed");
            self.failed = Some("Spotify playback stopped".to_string());
        }
        SpotifyFeed {
            position: self.position,
            finished: self.finished,
            failed: self.failed.clone(),
            playing,
        }
    }
}

struct DeviceGate {
    state: Mutex<Option<Result<(), String>>>,
    wait: Condvar,
}

impl DeviceGate {
    fn new() -> Self {
        Self {
            state: Mutex::new(None),
            wait: Condvar::new(),
        }
    }

    fn finish(&self, result: Result<(), String>) {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = Some(result);
        self.wait.notify_one();
    }

    fn wait(&self) -> Result<(), String> {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (guard, timed_out) = self
            .wait
            .wait_timeout_while(guard, Duration::from_secs(5), |state| state.is_none())
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if timed_out.timed_out() && guard.is_none() {
            return Err("Couldn't open the audio device".to_string());
        }
        match guard.clone() {
            Some(Ok(())) => Ok(()),
            Some(Err(error)) => Err(format!("Couldn't open the audio device: {error}")),
            None => Err("Couldn't open the audio device".to_string()),
        }
    }
}

/// PCM from librespot, played by rodio 0.23.
struct PcmSink {
    /// Kept so the stream stays open. The player borrows the mixer.
    #[allow(dead_code, reason = "the device must outlive the player")]
    output: Option<rodio::MixerDeviceSink>,
    player: Option<rodio::Player>,
}

impl PcmSink {
    fn open() -> Result<Self, String> {
        let mut output =
            rodio::DeviceSinkBuilder::open_default_sink().map_err(|error| error.to_string())?;
        output.log_on_drop(false);
        let player = rodio::Player::connect_new(output.mixer());
        Ok(Self {
            output: Some(output),
            player: Some(player),
        })
    }

    fn closed() -> Self {
        Self {
            output: None,
            player: None,
        }
    }
}

impl Sink for PcmSink {
    fn start(&mut self) -> SinkResult<()> {
        if let Some(player) = &self.player {
            player.play();
        }
        Ok(())
    }

    fn stop(&mut self) -> SinkResult<()> {
        // librespot exits the process when stop returns an error.
        if let Some(player) = &self.player {
            player.clear();
        }
        Ok(())
    }

    fn write(&mut self, packet: AudioPacket, converter: &mut Converter) -> SinkResult<()> {
        let samples = packet
            .samples()
            .map_err(|error| SinkError::OnWrite(error.to_string()))?;
        let floats = converter.f64_to_f32(samples);
        let channels = std::num::NonZero::new(u16::from(NUM_CHANNELS))
            .ok_or_else(|| SinkError::InvalidParams("no channels".into()))?;
        let rate = std::num::NonZero::new(SAMPLE_RATE)
            .ok_or_else(|| SinkError::InvalidParams("no sample rate".into()))?;
        let source = rodio::buffer::SamplesBuffer::new(channels, rate, floats);
        let player = self
            .player
            .as_ref()
            .ok_or_else(|| SinkError::NotConnected("audio device is not open".into()))?;
        player.append(source);
        while player.len() > QUEUED_CHUNKS {
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }
}

/// One `--selftest` line. `detail` never contains a token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub status: CheckStatus,
    pub detail: String,
}

/// PASS keeps the process successful. SKIP is an optional step. FAIL exits non-zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckStatus {
    Pass,
    Fail,
    Skip,
}

impl CheckStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::Skip => "SKIP",
        }
    }

    pub fn failed(self) -> bool {
        matches!(self, Self::Fail)
    }
}

impl Check {
    fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: CheckStatus::Pass,
            detail: detail.into(),
        }
    }

    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: CheckStatus::Fail,
            detail: detail.into(),
        }
    }

    fn skip(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: CheckStatus::Skip,
            detail: detail.into(),
        }
    }

    pub fn line(&self) -> String {
        format!("{} {}: {}", self.status.label(), self.name, self.detail)
    }
}

/// Saved-account checks. No window. `youtube` returns `Ok(None)` when that
/// service is signed out, which is reported as SKIP.
pub fn self_test(
    query: &str,
    youtube: impl FnMut(&str) -> Result<Option<usize>, String>,
) -> Vec<Check> {
    let saved = cache_dir().and_then(|cache| read_saved(&account_path(&cache)));
    self_test_with(saved, query, youtube)
}

fn self_test_with(
    saved: Option<SavedAccount>,
    query: &str,
    mut youtube: impl FnMut(&str) -> Result<Option<usize>, String>,
) -> Vec<Check> {
    const AFTER_REFRESH: [&str; 5] = [
        "profile",
        "playlists",
        "liked songs",
        "playlist tracks",
        "spotify search",
    ];
    let mut lines = Vec::new();
    let Some(saved) = saved else {
        for name in [
            "refresh",
            "profile",
            "playlists",
            "liked songs",
            "playlist tracks",
            "spotify search",
        ] {
            lines.push(Check::fail(name, "not signed in"));
        }
        lines.push(youtube_check(query, &mut youtube));
        lines.push(Check::fail("playback", "not signed in"));
        return lines;
    };

    let account = SpotifyAccount::new();
    let refreshed = match oauth_client().and_then(|client| {
        client
            .refresh_token(&saved.refresh_token)
            .map_err(|error| brief_message(scrub(&error.to_string(), &[&saved.refresh_token])))
    }) {
        Ok(token) => token,
        Err(error) => {
            lines.push(Check::fail("refresh", error.to_string()));
            for name in AFTER_REFRESH {
                lines.push(Check::fail(name, "refresh failed"));
            }
            lines.push(youtube_check(query, &mut youtube));
            lines.push(Check::fail("playback", "refresh failed"));
            return lines;
        }
    };
    lines.push(Check::pass("refresh", "web token refreshed"));
    remember_rotated_refresh(&saved, &refreshed.refresh_token);

    let profile = match account
        .http_get(PROFILE_URL, &refreshed.access_token, &[])
        .and_then(|body| profile_from_json(&body))
    {
        Ok(profile) => profile,
        Err(error) => {
            lines.push(Check::fail("profile", error.to_string()));
            for name in &AFTER_REFRESH[1..] {
                lines.push(Check::fail(name, "profile failed"));
            }
            lines.push(youtube_check(query, &mut youtube));
            lines.push(Check::fail("playback", "profile failed"));
            return lines;
        }
    };
    lines.push(Check::pass(
        "profile",
        format!("{} ({})", profile.name, profile.product),
    ));

    let access = refreshed.access_token.as_str();
    let mut first_playlist = None;
    match account
        .http_get(&library_page_url(PLAYLISTS_URL), access, &[])
        .and_then(|body| playlist_sample(&body))
    {
        Ok(sample) => {
            first_playlist = sample.first;
            lines.push(Check::pass(
                "playlists",
                format!(
                    "{} playlists, {} on the first page",
                    sample.total, sample.shown
                ),
            ));
        }
        Err(error) => lines.push(Check::fail("playlists", error.to_string())),
    }

    match account
        .http_get(&library_page_url(SAVED_TRACKS_URL), access, &[])
        .and_then(|body| track_sample(&body))
    {
        Ok(sample) => lines.push(Check::pass(
            "liked songs",
            format!(
                "{} liked songs, {} on the first page",
                sample.total, sample.shown
            ),
        )),
        Err(error) => lines.push(Check::fail("liked songs", error.to_string())),
    }

    match first_playlist {
        Some((id, name)) => {
            let items =
                library_page_url(&format!("https://api.spotify.com/v1/playlists/{id}/items"));
            let body = account.http_get(&items, access, &[]).or_else(|error| {
                if replied_not_found(&error) {
                    let tracks = library_page_url(&format!(
                        "https://api.spotify.com/v1/playlists/{id}/tracks"
                    ));
                    account.http_get(&tracks, access, &[])
                } else {
                    Err(error)
                }
            });
            match body.and_then(|body| track_sample(&body)) {
                Ok(sample) => lines.push(Check::pass(
                    "playlist tracks",
                    format!(
                        "{name}: {} tracks, {} on the first page",
                        sample.total, sample.shown
                    ),
                )),
                Err(error) => lines.push(Check::fail("playlist tracks", error.to_string())),
            }
        }
        None => lines.push(Check::skip(
            "playlist tracks",
            "no playlist on the first page",
        )),
    }

    let mut play_id = None;
    match account
        .http_get(
            SEARCH_URL,
            access,
            &[("type", "track"), ("q", query), ("limit", "10")],
        )
        .and_then(|body| tracks_from_search_json(&body))
    {
        Ok(tracks) if tracks.is_empty() => {
            lines.push(Check::fail(
                "spotify search",
                format!("no tracks for \"{query}\""),
            ));
        }
        Ok(tracks) => {
            play_id = Some(tracks[0].id.clone());
            lines.push(Check::pass(
                "spotify search",
                format!(
                    "{} tracks for \"{query}\", first \"{}\"",
                    tracks.len(),
                    tracks[0].title
                ),
            ));
        }
        Err(error) => lines.push(Check::fail("spotify search", error.to_string())),
    }

    lines.push(youtube_check(query, &mut youtube));
    lines.push(playback_check(&account, &profile, play_id.as_deref()));
    lines
}

fn remember_rotated_refresh(saved: &SavedAccount, refresh_token: &str) {
    if refresh_token.is_empty() || refresh_token == saved.refresh_token {
        return;
    }
    let mut updated = saved.clone();
    updated.refresh_token = refresh_token.to_string();
    let Some(cache) = cache_dir() else {
        return;
    };
    if let Err(error) = write_saved(&account_path(&cache), &updated) {
        log::error!(
            "selftest: could not store the rotated refresh token: {}",
            crate::diag::redact(&error)
        );
    } else {
        log::info!("selftest: stored a rotated refresh token");
    }
}

fn playback_check(account: &SpotifyAccount, profile: &Profile, track_id: Option<&str>) -> Check {
    if !profile.premium {
        return Check::skip("playback", "account is not Premium");
    }
    let Some(track_id) = track_id else {
        return Check::fail("playback", "no Spotify search result to play");
    };
    match account.restore() {
        AccountPhase::NotPremium { .. } => Check::skip("playback", "account is not Premium"),
        AccountPhase::Failed { message } => Check::fail("playback", message),
        AccountPhase::SignedOut | AccountPhase::SigningIn => {
            Check::fail("playback", "not signed in")
        }
        AccountPhase::SignedIn { .. } => play_for(account, track_id),
    }
}

fn play_for(account: &SpotifyAccount, track_id: &str) -> Check {
    let mut engine = LibrespotEngine::new(account.clone());
    if let Err(error) = engine.load(track_id) {
        return Check::fail("playback", error);
    }
    engine.play();
    let started = Instant::now();
    let mut saw_playing = false;
    let mut min_position = None;
    let mut max_position = Duration::ZERO;
    let mut failure = None;
    while started.elapsed() < Duration::from_secs(8) {
        let feed = engine.poll();
        if let Some(error) = feed.failed.clone() {
            failure = Some(error);
            break;
        }
        saw_playing |= feed.playing;
        min_position = Some(min_position.map_or(feed.position, |current: Duration| {
            current.min(feed.position)
        }));
        if feed.position > max_position {
            max_position = feed.position;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    engine.stop();
    if let Some(error) = failure {
        return Check::fail("playback", error);
    }
    let advanced = min_position.is_some_and(|min| max_position > min);
    if saw_playing && advanced {
        Check::pass(
            "playback",
            format!(
                "Playing, position advanced {}ms to {}ms",
                min_position.unwrap_or_default().as_millis(),
                max_position.as_millis()
            ),
        )
    } else if !saw_playing {
        Check::fail("playback", "no Playing event")
    } else {
        Check::fail(
            "playback",
            format!(
                "position did not advance (last {}ms)",
                max_position.as_millis()
            ),
        )
    }
}

fn youtube_check(
    query: &str,
    youtube: &mut impl FnMut(&str) -> Result<Option<usize>, String>,
) -> Check {
    match youtube(query) {
        Ok(None) => Check::skip("youtube music search", "not signed in"),
        Ok(Some(count)) => Check::pass(
            "youtube music search",
            format!("{count} tracks for \"{query}\""),
        ),
        Err(error) => Check::fail("youtube music search", crate::diag::redact(&error)),
    }
}

struct PageSample {
    total: usize,
    shown: usize,
    first: Option<(String, String)>,
}

fn playlist_sample(body: &str) -> Result<PageSample, Error> {
    let page = playlists_from_page_json(body)?;
    let value: serde_json::Value = serde_json::from_str(body).map_err(brief)?;
    let first = page
        .items
        .first()
        .map(|playlist| (playlist.id.clone(), playlist.name.clone()));
    Ok(PageSample {
        total: page_total(&value, page.items.len()),
        shown: page.items.len(),
        first,
    })
}

fn track_sample(body: &str) -> Result<PageSample, Error> {
    let page = tracks_from_collection_page(body)?;
    let value: serde_json::Value = serde_json::from_str(body).map_err(brief)?;
    let first = page
        .items
        .first()
        .map(|track| (track.id.clone(), track.title.clone()));
    Ok(PageSample {
        total: page_total(&value, page.items.len()),
        shown: page.items.len(),
        first,
    })
}

fn page_total(value: &serde_json::Value, fallback: usize) -> usize {
    value
        .get("total")
        .and_then(serde_json::Value::as_u64)
        .map(|total| usize::try_from(total).unwrap_or(fallback))
        .unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEARCH_FIXTURE: &str = include_str!("../tests/fixtures/spotify_search.json");
    const PLAYLISTS_FIXTURE: &str = include_str!("../tests/fixtures/spotify_playlists_page.json");
    const PLAYLISTS_NEXT_FIXTURE: &str =
        include_str!("../tests/fixtures/spotify_playlists_page_2.json");
    const PLAYLIST_TRACKS_FIXTURE: &str =
        include_str!("../tests/fixtures/spotify_playlist_tracks_page.json");
    const SAVED_TRACKS_FIXTURE: &str =
        include_str!("../tests/fixtures/spotify_saved_tracks_page.json");
    const PREMIUM_FIXTURE: &str = include_str!("../tests/fixtures/spotify_profile_premium.json");
    const FREE_FIXTURE: &str = include_str!("../tests/fixtures/spotify_profile_free.json");

    /// Spotify's published desktop client id. Public, not a secret.
    const DESKTOP_CLIENT_ID: &str = "65b708073fc0480ea92a077233ca87bd";

    #[test]
    fn desktop_client_id_is_the_librespot_desktop_id() {
        let id = desktop_client_id();
        assert_eq!(id, DESKTOP_CLIENT_ID);
        assert!(id.chars().all(|char| char.is_ascii_hexdigit()));
    }

    #[test]
    fn premium_plan_is_the_only_one_that_may_connect() {
        assert!(premium_product("premium"));
        assert!(premium_product("Premium"));
        assert!(!premium_product("free"));
        assert!(!premium_product("open"));
        assert!(!premium_product(""));
        assert!(!premium_product("unknown"));
    }

    #[test]
    fn profile_json_reads_premium_and_free_fixtures() {
        let premium = profile_from_json(PREMIUM_FIXTURE).expect("premium profile");
        assert_eq!(premium.name, "Ada Vega");
        assert!(premium.premium);
        assert_eq!(premium.product, "premium");

        let free = profile_from_json(FREE_FIXTURE).expect("free profile");
        assert_eq!(free.name, "Jonah Reed");
        assert!(!free.premium);

        let fallback =
            profile_from_json(r#"{"display_name":"  ","id":"adavega","product":"premium"}"#)
                .expect("id fallback");
        assert_eq!(fallback.name, "adavega");
    }

    #[test]
    fn profile_json_rejects_an_error_payload() {
        let error =
            profile_from_json(r#"{"error":{"status":401,"message":"The access token expired"}}"#)
                .expect_err("api error");
        let Error::Unavailable(reason) = error else {
            panic!("profile errors are unavailable");
        };
        assert!(reason.contains("401"), "{reason}");
        assert!(reason.contains("expired"), "{reason}");
    }

    #[test]
    fn search_json_maps_tracks_and_skips_a_bad_id() {
        let tracks = tracks_from_search_json(SEARCH_FIXTURE).expect("fixture");
        assert_eq!(tracks.len(), 2);
        assert!(tracks.iter().all(|track| track.source == Source::Spotify));
        assert_eq!(tracks[0].id, "5sWHDYs0csV6RS48xBl0tH");
        assert_eq!(tracks[0].title, "Night Drive");
        assert_eq!(tracks[0].artists, ["Ada Vega"]);
        assert_eq!(tracks[0].album.as_deref(), Some("Low Glow"));
        assert_eq!(tracks[0].duration, Duration::from_secs(192));
        assert_eq!(tracks[1].album, None);
        assert!(track_uri(&tracks[0].id).is_ok());

        let mixed = r#"{"tracks":{"items":[
            {"id":"sp-night-drive","name":"Skipped","duration_ms":1000,"artists":[{"name":"Ada"}]},
            {"id":"5sWHDYs0csV6RS48xBl0tH","name":"Night Drive","duration_ms":1000,"artists":[]}
        ]}}"#;
        let tracks = tracks_from_search_json(mixed).expect("mixed");
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].title, "Night Drive");
        assert!(track_uri("sp-night-drive").is_err());
    }

    #[test]
    fn search_json_rejects_malformed_and_error_payloads() {
        assert!(matches!(
            tracks_from_search_json("not json"),
            Err(Error::Unavailable(_))
        ));
        let error =
            tracks_from_search_json(r#"{"error":{"status":400,"message":"No search query"}}"#)
                .expect_err("api");
        let Error::Unavailable(reason) = error else {
            panic!("search errors are unavailable");
        };
        assert!(reason.contains("No search query"), "{reason}");
    }

    #[test]
    fn account_lines_cover_every_state() {
        let signed_out = present_account(&AccountPhase::SignedOut);
        assert!(signed_out.offer_sign_in);
        assert_eq!(signed_out.tone, AccountTone::Muted);
        assert!(signed_out.text.contains("signed out"));

        let signing = present_account(&AccountPhase::SigningIn);
        assert!(!signing.offer_sign_in);
        assert!(signing.text.contains("Signing in"));

        let signed_in = present_account(&AccountPhase::SignedIn {
            name: "Ada Vega".into(),
        });
        assert_eq!(signed_in.text, "Signed in as Ada Vega");
        assert_eq!(signed_in.tone, AccountTone::Normal);
        assert!(!signed_in.offer_sign_in);

        let free = present_account(&AccountPhase::NotPremium {
            name: "Jonah".into(),
        });
        assert_eq!(free.tone, AccountTone::Warning);
        assert!(free.text.contains("not Spotify Premium"));
        assert!(free.text.contains("YouTube Music"));
        assert!(free.text.contains("Jonah"));

        let failed = present_account(&AccountPhase::Failed {
            message: "browser closed".into(),
        });
        assert_eq!(failed.tone, AccountTone::Error);
        assert!(failed.offer_sign_in);
        assert_eq!(failed.text, "browser closed");
    }

    #[test]
    fn cache_dir_stays_absolute_and_off_the_working_tree() {
        let home = "/home/ada";
        let linux = cache_dir_from(Desktop::Linux, |key| match key {
            "HOME" => Some(home.to_string()),
            "XDG_CACHE_HOME" => Some("/var/cache/ada".to_string()),
            _ => None,
        });
        assert_eq!(linux, Some(PathBuf::from("/var/cache/ada/codevox-mu")));

        let from_home = cache_dir_from(Desktop::Linux, |key| {
            (key == "HOME").then(|| home.to_string())
        });
        assert_eq!(
            from_home,
            Some(PathBuf::from("/home/ada/.cache/codevox-mu"))
        );

        let relative = cache_dir_from(Desktop::Linux, |key| match key {
            "HOME" => Some("relative-home".to_string()),
            "XDG_CACHE_HOME" => Some("relative-cache".to_string()),
            _ => None,
        });
        assert_eq!(relative, None);

        let mac = cache_dir_from(Desktop::Mac, |key| {
            (key == "HOME").then(|| "/Users/ada".to_string())
        });
        assert_eq!(
            mac,
            Some(PathBuf::from("/Users/ada/Library/Caches/codevox-mu"))
        );

        let windows = cache_dir_from(Desktop::Windows, |key| {
            (key == "LOCALAPPDATA").then(|| r"C:\Users\ada\AppData\Local".to_string())
        });
        assert_eq!(
            windows,
            Some(PathBuf::from(r"C:\Users\ada\AppData\Local").join("codevox-mu"))
        );

        let appdata = cache_dir_from(Desktop::Windows, |key| {
            (key == "APPDATA").then(|| r"D:\AppData\Roaming".to_string())
        });
        assert_eq!(
            appdata,
            Some(PathBuf::from(r"D:\AppData\Roaming").join("codevox-mu"))
        );
        assert_eq!(cache_dir_from(Desktop::Windows, |_| None), None);
        let relative_windows = cache_dir_from(Desktop::Windows, |key| {
            (key == "LOCALAPPDATA").then(|| r"Users\ada\AppData\Local".to_string())
        });
        assert_eq!(relative_windows, None);
    }

    #[test]
    fn saved_account_file_redacts_the_token_and_is_private() {
        let dir =
            std::env::temp_dir().join(format!("codevox-mu-spotify-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let saved = SavedAccount {
            device_id: "device-1".to_string(),
            display_name: "Ada Vega".to_string(),
            product: "premium".to_string(),
            refresh_token: "refresh-token-fixture-value".to_string(),
            playback_refresh_token: "playback-token-fixture-value".to_string(),
        };
        let path = account_path(&dir);
        write_saved(&path, &saved).expect("write");

        let debug = format!("{saved:?}");
        assert!(debug.contains("[redacted]"), "{debug}");
        assert!(!debug.contains("refresh-token-fixture-value"), "{debug}");
        assert!(!debug.contains("playback-token-fixture-value"), "{debug}");

        let read = read_saved(&path).expect("read");
        assert_eq!(read.refresh_token, saved.refresh_token);
        assert_eq!(read.playback_refresh_token, saved.playback_refresh_token);
        assert_eq!(read.display_name, "Ada Vega");

        let legacy = dir.join("legacy.json");
        fs::write(
            &legacy,
            r#"{"device_id":"d","display_name":"Ada","product":"premium","refresh_token":"refresh-token-fixture-value"}"#,
        )
        .expect("legacy");
        let legacy_read = read_saved(&legacy).expect("legacy read");
        assert_eq!(legacy_read.refresh_token, saved.refresh_token);
        assert_eq!(legacy_read.playback_refresh_token, "");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "account file mode is {mode:o}");
            let dir_mode = fs::metadata(&dir).expect("dir").permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o700, "cache dir mode is {dir_mode:o}");
        }

        let scrubbed = scrub(
            "token refresh-token-fixture-value failed",
            &["refresh-token-fixture-value"],
        );
        assert!(
            !scrubbed.contains("refresh-token-fixture-value"),
            "{scrubbed}"
        );
        assert!(scrubbed.contains("[redacted]"), "{scrubbed}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn blank_search_does_not_start_a_runtime() {
        let catalogue = SpotifyCatalogue::new(SpotifyAccount::new());
        assert!(catalogue.search("").expect("blank").is_empty());
        assert!(catalogue.search("   ").expect("blank").is_empty());
        assert!(catalogue.account.runtime.lock().expect("runtime").is_none());
        assert!(!catalogue.account.can_play());
        let playlists = catalogue.playlists();
        assert!(
            matches!(playlists, Err(Error::Unavailable(ref reason)) if reason.contains("signed out")),
            "{playlists:?}"
        );
        assert!(catalogue.account.runtime.lock().expect("runtime").is_none());
        let liked = catalogue.playlist_tracks(LIKED_SONGS_ID);
        assert!(
            matches!(liked, Err(Error::Unavailable(ref reason)) if reason.contains("signed out")),
            "{liked:?}"
        );
        assert_eq!(catalogue.playlist_tracks("../etc"), Err(Error::NotFound));
        assert!(catalogue.account.runtime.lock().expect("runtime").is_none());
        assert_eq!(
            catalogue.resolve("5sWHDYs0csV6RS48xBl0tH"),
            Err(Error::NotFound)
        );
    }

    #[test]
    fn signed_out_search_does_not_call_the_network() {
        let catalogue = SpotifyCatalogue::new(SpotifyAccount::new());
        assert!(
            catalogue
                .search("night drive")
                .expect("signed out")
                .is_empty()
        );
        assert!(catalogue.account.runtime.lock().expect("runtime").is_none());
        assert!(catalogue.account.client.lock().expect("client").is_none());
    }

    #[test]
    fn client_id_prefers_runtime_then_compiled_then_the_built_in_app() {
        let compiled = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let runtime = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        assert_eq!(resolve_client_id(Some(runtime), Some(compiled)), runtime);
        assert_eq!(
            resolve_client_id(Some("   "), Some(compiled)),
            compiled,
            "a blank runtime override falls through"
        );
        assert_eq!(
            resolve_client_id(None, Some("  aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa ")),
            compiled,
            "the compiled id is trimmed"
        );
        assert_eq!(
            resolve_client_id(Some(""), Some(" ")),
            "34edc1e59dcb447db61ea7720733cbde",
            "blank overrides fall through to the CodeVox mU app"
        );
        assert_eq!(
            resolve_client_id(None, None),
            "34edc1e59dcb447db61ea7720733cbde"
        );
        assert_ne!(resolve_client_id(None, None), desktop_client_id());

        let id = web_client_id();
        assert_ne!(id, desktop_client_id());
        assert!(!id.trim().is_empty());
    }

    #[test]
    fn playback_session_uses_the_desktop_client_id() {
        let config = playback_session_config("device-42");
        assert_eq!(config.device_id, "device-42");
        assert_eq!(config.client_id, desktop_client_id());
        assert_ne!(config.client_id, DEFAULT_WEB_CLIENT_ID);
    }

    #[test]
    fn web_sign_in_includes_the_library_scopes() {
        for scope in [
            "streaming",
            "user-read-private",
            "user-read-email",
            "user-library-read",
            "playlist-read-private",
            "playlist-read-collaborative",
        ] {
            assert!(SCOPES.contains(&scope), "missing {scope}");
        }
    }

    #[test]
    fn retry_after_retries_once_inside_ten_seconds_and_explains_the_rest() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_445_412_480);
        let rfc = http_date("Sun, 06 Nov 1994 08:49:37 GMT").expect("rfc date");
        assert_eq!(
            rfc.duration_since(SystemTime::UNIX_EPOCH).expect("rfc"),
            Duration::from_secs(784_111_777)
        );

        assert_eq!(parse_retry_after("0", now), Some(Duration::ZERO));
        assert_eq!(
            parse_retry_after("  10  ", now),
            Some(Duration::from_secs(10))
        );
        assert_eq!(parse_retry_after("11", now), Some(Duration::from_secs(11)));
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:28:04 GMT", now),
            Some(Duration::from_secs(4))
        );
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:28:30 GMT", now),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            parse_retry_after("Thu, 01 Jan 1970 00:00:00 GMT", now),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after("", now), None);
        assert_eq!(parse_retry_after("soon", now), None);
        assert_eq!(parse_retry_after("1.5", now), None);
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:28:00 UTC", now),
            None
        );

        assert_eq!(
            rate_limit_choice(Some("10"), now, false),
            RateLimitChoice::RetryAfter(Duration::from_secs(10))
        );
        assert_eq!(
            rate_limit_choice(Some("0"), now, false),
            RateLimitChoice::RetryAfter(Duration::ZERO)
        );
        assert_eq!(
            rate_limit_choice(Some("Wed, 21 Oct 2015 07:28:04 GMT"), now, false),
            RateLimitChoice::RetryAfter(Duration::from_secs(4))
        );

        match rate_limit_choice(Some("45"), now, false) {
            RateLimitChoice::GiveUp(message) => {
                assert!(message.contains("45 seconds"), "{message}");
                assert_human_rate_limit(&message);
            }
            other => panic!("45s is past the cap: {other:?}"),
        }
        match rate_limit_choice(Some("90"), now, false) {
            RateLimitChoice::GiveUp(message) => {
                assert!(message.contains("2 minutes"), "{message}");
                assert_human_rate_limit(&message);
            }
            other => panic!("90s is past the cap: {other:?}"),
        }
        match rate_limit_choice(Some("1"), now, true) {
            RateLimitChoice::GiveUp(message) => {
                assert!(message.contains("Wait a moment"), "{message}");
                assert_human_rate_limit(&message);
            }
            other => panic!("a second 429 is not retried: {other:?}"),
        }
        match rate_limit_choice(None, now, false) {
            RateLimitChoice::GiveUp(message) => {
                assert!(message.contains("Wait a moment"), "{message}");
                assert_human_rate_limit(&message);
            }
            other => panic!("a missing Retry-After is not a guessed wait: {other:?}"),
        }
    }

    fn assert_human_rate_limit(message: &str) {
        assert!(!message.contains("429"), "{message}");
        assert!(!message.contains("API rate limit exceeded"), "{message}");
    }

    #[test]
    fn a_rate_limit_body_is_a_sentence_without_the_status() {
        let body = r#"{"error":{"status":429,"message":"API rate limit exceeded"}}"#;
        for error in [
            profile_from_json(body).expect_err("profile"),
            tracks_from_search_json(body).expect_err("search"),
            playlists_from_page_json(body).expect_err("playlists"),
            tracks_from_playlist_page_json(body).expect_err("playlist tracks"),
            tracks_from_saved_page_json(body).expect_err("saved tracks"),
        ] {
            let Error::Unavailable(reason) = error else {
                panic!("rate limits are unavailable");
            };
            assert!(reason.contains("limiting requests"), "{reason}");
            assert_human_rate_limit(&reason);
        }
    }

    #[test]
    fn playlist_pages_keep_named_lists_and_skip_bad_rows() {
        let page = playlists_from_page_json(PLAYLISTS_FIXTURE).expect("playlists");
        assert_eq!(
            page.next.as_deref(),
            Some("https://api.spotify.com/v1/me/playlists?offset=50&limit=50")
        );
        assert_eq!(page.items.len(), 2);
        assert!(page.items.iter().all(|row| row.source == Source::Spotify));
        assert_eq!(page.items[0].id, "37i9dQZF1DXcBWIGoYBM5M");
        assert_eq!(page.items[0].name, "Low Glow");
        assert_eq!(page.items[1].name, "Untitled playlist");

        assert!(matches!(
            playlists_from_page_json("not json"),
            Err(Error::Unavailable(_))
        ));
        assert!(matches!(
            playlists_from_page_json(r#"{"href":"https://api.spotify.com/v1/me/playlists"}"#),
            Err(Error::Unavailable(_))
        ));
    }

    #[test]
    fn playlist_track_pages_keep_tracks_from_either_wrapper() {
        let page = tracks_from_playlist_page_json(PLAYLIST_TRACKS_FIXTURE).expect("tracks");
        assert_eq!(page.next, None);
        assert_eq!(page.items.len(), 2);
        assert!(
            page.items
                .iter()
                .all(|track| track.source == Source::Spotify)
        );
        assert_eq!(page.items[0].id, "5sWHDYs0csV6RS48xBl0tH");
        assert_eq!(page.items[0].title, "Night Drive");
        assert_eq!(page.items[0].artists, ["Ada Vega"]);
        assert_eq!(page.items[0].album.as_deref(), Some("Low Glow"));
        assert_eq!(page.items[0].duration, Duration::from_secs(192));
        assert_eq!(page.items[1].title, "Glass House");
        assert_eq!(page.items[1].artists, ["Ada Vega", "North Line"]);
        assert_eq!(page.items[1].duration, Duration::from_secs(241));
        assert!(page.items.iter().all(|track| track.title != "A Podcast"));
        assert!(
            page.items
                .iter()
                .all(|track| track.title != "On This Computer")
        );
        assert!(page.items.iter().all(|track| track.title != "Skipped"));
    }

    #[test]
    fn saved_track_pages_keep_liked_songs_and_skip_nulls() {
        let page = tracks_from_saved_page_json(SAVED_TRACKS_FIXTURE).expect("saved");
        assert_eq!(
            page.next.as_deref(),
            Some("https://api.spotify.com/v1/me/tracks?offset=50&limit=50")
        );
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].source, Source::Spotify);
        assert_eq!(page.items[0].title, "Night Drive");
        assert_eq!(page.items[0].album.as_deref(), Some("Low Glow"));
        assert_eq!(page.items[0].duration, Duration::from_secs(192));
    }

    fn first_playlist_page() -> &'static str {
        "https://api.spotify.com/v1/me/playlists?limit=50"
    }

    #[test]
    fn library_pages_follow_next_in_order_and_stop() {
        let first = first_playlist_page();
        let second = "https://api.spotify.com/v1/me/playlists?offset=50&limit=50";
        let mut calls = Vec::new();
        let playlists = collect_pages(first, PLAYLIST_CAP, |url| {
            calls.push(url.to_string());
            let body = if url == first {
                PLAYLISTS_FIXTURE
            } else if url == second {
                PLAYLISTS_NEXT_FIXTURE
            } else {
                panic!("unexpected page {url}");
            };
            playlists_from_page_json(body)
        })
        .expect("pages");
        assert_eq!(calls, [first, second]);
        assert_eq!(
            playlists
                .iter()
                .map(|playlist| playlist.name.as_str())
                .collect::<Vec<_>>(),
            ["Low Glow", "Untitled playlist", "Harbour"]
        );
    }

    #[test]
    fn library_pages_stop_at_the_cap_without_another_request() {
        let first = first_playlist_page();
        let mut calls = Vec::new();
        let one = collect_pages(first, 1, |url| {
            calls.push(url.to_string());
            playlists_from_page_json(if url == first {
                PLAYLISTS_FIXTURE
            } else {
                panic!("the cap stops before the next page");
            })
        })
        .expect("cap");
        assert_eq!(calls, [first]);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].name, "Low Glow");

        let mut fetched = false;
        let empty = collect_pages(first, 0, |_url| {
            fetched = true;
            playlists_from_page_json(PLAYLISTS_FIXTURE)
        })
        .expect("zero cap");
        assert!(empty.is_empty());
        assert!(!fetched);
    }

    #[test]
    fn library_pages_do_not_follow_a_foreign_or_repeated_next_url() {
        let first = first_playlist_page();
        let mut hostile_calls = Vec::new();
        let kept = collect_pages(first, PLAYLIST_CAP, |url| {
            hostile_calls.push(url.to_string());
            if url == first {
                let mut page = playlists_from_page_json(PLAYLISTS_FIXTURE).expect("first");
                page.next = Some("https://evil.example/v1/me/playlists".to_string());
                Ok(page)
            } else {
                panic!("a non-Spotify next URL is not requested");
            }
        })
        .expect("hostile");
        assert_eq!(hostile_calls, [first]);
        assert_eq!(kept.len(), 2);

        let mut repeated = 0;
        let stopped = collect_pages(first, PLAYLIST_CAP, |url| {
            repeated += 1;
            assert_eq!(url, first);
            let mut page = playlists_from_page_json(PLAYLISTS_FIXTURE).expect("loop");
            page.next = Some(first.to_string());
            Ok(page)
        })
        .expect("repeat");
        assert_eq!(repeated, 1);
        assert_eq!(stopped.len(), 2);
    }

    #[test]
    fn a_rate_limited_library_page_fails_the_walk_and_404_is_the_tracks_fallback() {
        let failed = collect_pages(first_playlist_page(), PLAYLIST_CAP, |_url| {
            playlists_from_page_json(
                r#"{"error":{"status":429,"message":"API rate limit exceeded"}}"#,
            )
        });
        let Err(Error::Unavailable(reason)) = failed else {
            panic!("a rate-limited page fails the walk");
        };
        assert!(reason.contains("limiting requests"), "{reason}");
        assert_human_rate_limit(&reason);

        let missing = brief_message("Spotify replied 404: Not found");
        assert!(replied_not_found(&missing));
        let limited = brief_message("Spotify is limiting requests. Wait a moment and try again.");
        assert!(!replied_not_found(&limited));
    }

    #[test]
    fn playback_tls_provider_lets_the_session_client_build() {
        // librespot builds this config on the playback connection. With both
        // rustls crypto features linked and no process provider, it panics
        // after the account file has already been written.
        ensure_playback_tls();
        let _config = rustls::ClientConfig::builder();
    }

    #[test]
    fn a_missing_account_is_not_signed_in() {
        let lines = self_test_with(None, "love", |_| Ok(None));
        assert!(
            lines
                .iter()
                .any(|line| line.name == "refresh" && line.status == CheckStatus::Fail),
            "{lines:?}"
        );
        assert!(lines.iter().all(|line| {
            if line.name == "youtube music search" {
                line.status == CheckStatus::Skip && line.detail.contains("not signed in")
            } else {
                line.status == CheckStatus::Fail && line.detail.contains("not signed in")
            }
        }));
        let rendered = lines.iter().map(Check::line).collect::<Vec<_>>().join("\n");
        assert!(
            rendered.contains("FAIL refresh: not signed in"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("stopped before it finished"),
            "{rendered}"
        );
    }

    #[test]
    fn the_redirect_target_yields_the_authorization_code() {
        let code =
            auth_code_from_target("/login?state=abc&code=authorization-code-value").expect("code");
        assert_eq!(code, "authorization-code-value");
        assert!(auth_code_from_target("/login?error=access_denied").is_err());
    }
}
