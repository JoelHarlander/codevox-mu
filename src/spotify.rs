//! Spotify sign-in and playback through librespot.
//!
//! Sign-in is the OAuth authorisation-code flow with PKCE from
//! [librespot-oauth](https://crates.io/crates/librespot-oauth) (MIT), using the
//! desktop client id librespot already ships. There is no Spotify developer
//! app and no client secret. The refresh token and librespot's reusable
//! credentials are written under the OS cache directory, mode `0600` where
//! the platform has modes. They are never written into the repo.
//!
//! Playback uses [librespot-playback](https://crates.io/crates/librespot-playback)
//! (MIT). Its rodio backend links an older `alsa-sys` than the YouTube player,
//! so decoded PCM is handed to this app's rodio 0.23 device instead.
//!
//! librespot calls `process::exit` when a session's product info is not
//! Premium. A free account is detected from the Web API profile and never
//! connected. Search still works; the player falls back to YouTube Music.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

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

use crate::music::{Error, MusicService, Playlist, Source, Track};
use crate::player::{SpotifyEngine, SpotifyFeed};

/// How long an error row is allowed to get.
const ERROR_CHARS: usize = 160;

/// Local callback librespot's desktop client id accepts.
const REDIRECT_URI: &str = "http://127.0.0.1:8898/login";

/// Shown in the browser tab after Spotify redirects back.
const BROWSER_MESSAGE: &str = "Signed in. You can close this tab and return to CodeVox mU.";

/// Scopes the desktop client id is asked for. Playback needs `streaming`.
/// The profile call needs `user-read-private` (name and Premium) and
/// `user-read-email` as a name fallback.
const SCOPES: &[&str] = &["streaming", "user-read-private", "user-read-email"];

const PROFILE_URL: &str = "https://api.spotify.com/v1/me";
const SEARCH_URL: &str = "https://api.spotify.com/v1/search";

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
/// This is a public client id, not a secret. The PKCE flow does not use a
/// client secret.
pub fn desktop_client_id() -> String {
    SessionConfig::default().client_id
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
        Some(status) => format!("Spotify replied {status}: {message}"),
        None => format!("Spotify replied: {message}"),
    })
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
        let path = PathBuf::from(value);
        is_absolute_for(desktop, &path).then_some(path)
    };

    let root = match desktop {
        Desktop::Windows => absolute("LOCALAPPDATA").or_else(|| absolute("APPDATA"))?,
        Desktop::Mac => absolute("HOME")?.join("Library").join("Caches"),
        Desktop::Linux => absolute("XDG_CACHE_HOME")
            .or_else(|| absolute("HOME").map(|home| home.join(".cache")))?,
    };
    Some(root.join("codevox-mu"))
}

/// `Path::is_absolute` follows the host. Both rules are spelled out so each
/// desktop's path check can be tested on any host: a drive letter is not
/// absolute on Linux, and `/home` is not absolute on Windows.
fn is_absolute_for(desktop: Desktop, path: &Path) -> bool {
    match desktop {
        Desktop::Linux | Desktop::Mac => path.to_string_lossy().starts_with('/'),
        Desktop::Windows => {
            let text = path.to_string_lossy();
            let bytes = text.as_bytes();
            let drive = bytes.len() >= 3
                && bytes[0].is_ascii_alphabetic()
                && bytes[1] == b':'
                && (bytes[2] == b'\\' || bytes[2] == b'/');
            drive || text.starts_with(r"\\") || text.starts_with("//")
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

struct SavedAccount {
    device_id: String,
    display_name: String,
    product: String,
    refresh_token: String,
}

impl std::fmt::Debug for SavedAccount {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SavedAccount")
            .field("device_id", &self.device_id)
            .field("display_name", &self.display_name)
            .field("product", &self.product)
            .field("refresh_token", &"[redacted]")
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
        let epoch = self.snapshot_epoch();
        let Some(cache) = cache_dir() else {
            return self.current_phase();
        };
        let Some(saved) = read_saved(&account_path(&cache)) else {
            return self.current_phase();
        };
        if !self.still(epoch) {
            return self.current_phase();
        }
        self.finish_login(epoch, &saved.refresh_token, &saved.device_id, true)
    }

    /// Browser sign-in. Blocks until the redirect arrives or the flow fails.
    /// Check the cache directory first so a failed save does not follow a
    /// completed login.
    pub fn sign_in(&self) -> AccountPhase {
        if cache_dir().is_none() {
            return AccountPhase::Failed {
                message: "Couldn't find a cache directory for the Spotify sign-in.".to_string(),
            };
        }
        let epoch = self.bump_epoch();
        let client =
            match OAuthClientBuilder::new(&desktop_client_id(), REDIRECT_URI, SCOPES.to_vec())
                .open_in_browser()
                .with_custom_message(BROWSER_MESSAGE)
                .build()
            {
                Ok(client) => client,
                Err(error) => {
                    return self.fail(epoch, scrub(&error.to_string(), &[]));
                }
            };
        let token = match client.get_access_token() {
            Ok(token) => token,
            Err(error) => {
                return self.fail(epoch, scrub(&error.to_string(), &[]));
            }
        };
        if !self.still(epoch) {
            return self.current_phase();
        }
        self.finish_login(epoch, &token.refresh_token, "", false)
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
                return self.fail(epoch, scrub(&error.to_string(), &[refresh_token]));
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
                scrub(&error, &[&refresh_token, &refreshed.access_token]),
            );
        }

        // Free and unknown plans must not reach Session::connect. librespot
        // exits the process when the product info is not Premium.
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
            &refreshed.access_token,
            &refresh_token,
            prefer_saved_credentials,
        ) {
            Ok(session) => {
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
        let cred_dir = librespot_path(cache);
        restrict_dir(&cred_dir)?;
        let store = Cache::new(Some(&cred_dir), None, None, None)
            .map_err(|error| scrub(&error.to_string(), &[access_token, refresh_token]))?;
        restrict_credentials(&cred_dir);
        let cached = prefer_saved_credentials
            .then(|| store.credentials())
            .flatten();
        let credentials = cached.unwrap_or_else(|| Credentials::with_access_token(access_token));
        let config = SessionConfig {
            device_id: device_id.to_string(),
            ..SessionConfig::default()
        };
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
            client
                .get(url)
                .bearer_auth(&access_token)
                .query(&query)
                .send()
                .await
                .map_err(|error| scrub(&error.to_string(), &[&access_token]))?
                .text()
                .await
                .map_err(|error| scrub(&error.to_string(), &[&access_token]))
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

fn oauth_client() -> Result<librespot_oauth::OAuthClient, Error> {
    OAuthClientBuilder::new(&desktop_client_id(), REDIRECT_URI, SCOPES.to_vec())
        .build()
        .map_err(brief)
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
/// and do not use the network. Playlists are not part of this pass.
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
        Ok(Vec::new())
    }

    fn playlist_tracks(&self, _playlist_id: &str) -> Result<Vec<Track>, Error> {
        Err(Error::NotFound)
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
        for event in pending {
            self.note_event(event);
        }
        if disconnected {
            self.failed = Some("Spotify playback stopped".to_string());
        }
        SpotifyFeed {
            position: self.position,
            finished: self.finished,
            failed: self.failed.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;

    const SEARCH_FIXTURE: &str = include_str!("../tests/fixtures/spotify_search.json");
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
        };
        let path = account_path(&dir);
        write_saved(&path, &saved).expect("write");

        let debug = format!("{saved:?}");
        assert!(debug.contains("[redacted]"), "{debug}");
        assert!(!debug.contains("refresh-token-fixture-value"), "{debug}");

        let read = read_saved(&path).expect("read");
        assert_eq!(read.refresh_token, saved.refresh_token);
        assert_eq!(read.display_name, "Ada Vega");

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
        assert!(catalogue.playlists().expect("no library").is_empty());
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
}
