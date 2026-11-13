//! Signed-in YouTube Music: a browser cookie, then the library.
//!
//! ytmapi-rs can sign in with OAuth device codes or with the cookie header
//! from a browser session. Device codes need a Google OAuth client id and
//! secret, which means a Cloud project. A normal listener does not have one.
//! The cookie flow uses the session they already have on music.youtube.com.
//!
//! The cookie goes to the OS keyring. When that backend is missing, it goes
//! to the per-user config directory as a file the user can read and nobody
//! else can. It is never written into the repo, a log line, or an error.
//!
//! A playlist, liked songs, and the library list continue past the first
//! page. ytmapi-rs follows each continuation token. This module stops at
//! [`TRACK_CAP`] tracks or [`PLAYLIST_CAP`] playlists, and it does not
//! request a continuation token it has already used.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use ytmapi_rs::YtMusic;
use ytmapi_rs::auth::BrowserToken;
use ytmapi_rs::common::{PlaylistID, YoutubeID};
use ytmapi_rs::continuations::ParseFromContinuable;
use ytmapi_rs::parse::{LibraryPlaylist, PlaylistItem, TableListSong};
use ytmapi_rs::query::{
    GetLibraryPlaylistsQuery, GetLibrarySongsQuery, GetPlaylistTracksQuery, PostQuery, Query,
};

use crate::music::{Error, Playlist, Source, Track};
use crate::youtube_music::parse_duration;

/// Id the window uses for liked songs. It is not a YouTube playlist id.
pub const LIKED_SONGS: &str = "liked-songs";

const LIKED_SONGS_NAME: &str = "Liked songs";

/// Tracks kept from one playlist or from liked songs.
///
/// The same ceiling Spotify uses. A continuation loop cannot grow past it.
const TRACK_CAP: usize = 1_000;

/// Library playlists kept. Liked songs is added after the cap, as on Spotify.
const PLAYLIST_CAP: usize = 500;
const KEYRING_SERVICE: &str = "codevox-mu";
const KEYRING_USER: &str = "youtube-music";
const COOKIE_FILE: &str = "youtube-music.cookie";
const MAX_COOKIE_CHARS: usize = 16 * 1024;
const ERROR_CHARS: usize = 160;

type Opened = Pin<Box<dyn Future<Output = Result<Arc<dyn Catalogue>, Error>> + Send>>;
type Opener = Arc<dyn Fn(String) -> Opened + Send + Sync>;

/// What the window can do with a YouTube Music account.
///
/// Calls block. The window runs them off the UI thread. A test double stands
/// in for the network.
pub trait LibrarySession: Send + Sync {
    fn signed_in(&self) -> bool;
    fn sign_in(&self, cookie: &str) -> Result<(), Error>;
    /// `Ok(false)` when nothing is stored. `Ok(true)` after the saved cookie
    /// opens a session.
    fn restore(&self) -> Result<bool, Error>;
    fn sign_out(&self) -> Result<(), Error>;
    fn playlists(&self) -> Result<Vec<Playlist>, Error>;
    fn playlist_tracks(&self, playlist_id: &str) -> Result<Vec<Track>, Error>;
}

trait Catalogue: Send + Sync {
    fn playlists(&self) -> Result<Vec<Playlist>, Error>;
    fn tracks(&self, playlist_id: &str) -> Result<Vec<Track>, Error>;
}

trait SecretStore: Send + Sync {
    fn load(&self) -> Result<Option<String>, Error>;
    fn save(&self, secret: &str) -> Result<(), Error>;
    fn delete(&self) -> Result<(), Error>;
}

struct Gate {
    /// Bumped by sign-out so a sign-in still on the network cannot store the
    /// cookie afterwards.
    generation: u64,
    session: Option<Arc<dyn Catalogue>>,
}

/// One YouTube Music sign-in, shared by every clone of the search client.
pub struct Account {
    runtime: Mutex<Option<tokio::runtime::Runtime>>,
    gate: Mutex<Gate>,
    secrets: Arc<dyn SecretStore>,
    opener: Opener,
}

impl Account {
    /// The real keyring, file, and YouTube Music client. Nothing is fetched
    /// until sign-in.
    pub fn live() -> Self {
        Self::from_parts(Arc::new(OsSecrets), live_opener())
    }

    fn from_parts(secrets: Arc<dyn SecretStore>, opener: Opener) -> Self {
        Self {
            runtime: Mutex::new(None),
            gate: Mutex::new(Gate {
                generation: 0,
                session: None,
            }),
            secrets,
            opener,
        }
    }

    fn runtime(&self) -> Result<tokio::runtime::Handle, Error> {
        let mut guard = self
            .runtime
            .lock()
            .map_err(|_| show_error("YouTube Music sign-in state was poisoned"))?;
        if guard.is_none() {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_io()
                .enable_time()
                .thread_name("youtube-music-account")
                .build()
                .map_err(|error| {
                    show_error(format!(
                        "could not start the YouTube Music runtime: {error}"
                    ))
                })?;
            *guard = Some(runtime);
        }
        Ok(guard
            .as_ref()
            .expect("runtime was just stored")
            .handle()
            .clone())
    }
}

impl LibrarySession for Account {
    fn signed_in(&self) -> bool {
        self.gate
            .lock()
            .map(|guard| guard.session.is_some())
            .unwrap_or(false)
    }

    fn sign_in(&self, cookie: &str) -> Result<(), Error> {
        let cookie = normalize_session_cookie(cookie)?;
        let generation = self
            .gate
            .lock()
            .map_err(|_| show_error("YouTube Music sign-in state was poisoned"))?
            .generation;
        let runtime = self.runtime()?;
        let opener = Arc::clone(&self.opener);
        let (sender, receiver) = std::sync::mpsc::channel();
        let for_open = cookie.clone();
        runtime.spawn(async move {
            let _ = sender.send(opener(for_open).await);
        });
        let opened = receiver
            .recv()
            .map_err(|_| show_error("YouTube Music sign-in stopped before it replied"))?;
        let catalogue = match opened {
            Ok(catalogue) => catalogue,
            Err(error) => return Err(scrub(&cookie, &error.to_string())),
        };

        let mut guard = self
            .gate
            .lock()
            .map_err(|_| show_error("YouTube Music sign-in state was poisoned"))?;
        if guard.generation != generation {
            return Err(show_error("YouTube Music sign-in was cancelled"));
        }
        self.secrets.save(&cookie)?;
        guard.session = Some(catalogue);
        Ok(())
    }

    fn restore(&self) -> Result<bool, Error> {
        let Some(cookie) = self.secrets.load()? else {
            return Ok(false);
        };
        if cookie.is_empty() {
            return Ok(false);
        }
        self.sign_in(&cookie)?;
        Ok(true)
    }

    fn sign_out(&self) -> Result<(), Error> {
        {
            let mut guard = self
                .gate
                .lock()
                .map_err(|_| show_error("YouTube Music sign-in state was poisoned"))?;
            guard.generation = guard.generation.wrapping_add(1);
            guard.session = None;
        }
        self.secrets.delete()
    }

    fn playlists(&self) -> Result<Vec<Playlist>, Error> {
        let session = self.session()?;
        session.playlists()
    }

    fn playlist_tracks(&self, playlist_id: &str) -> Result<Vec<Track>, Error> {
        let session = self.session()?;
        session.tracks(playlist_id)
    }
}

impl Account {
    fn session(&self) -> Result<Arc<dyn Catalogue>, Error> {
        self.gate
            .lock()
            .map_err(|_| show_error("YouTube Music sign-in state was poisoned"))?
            .session
            .clone()
            .ok_or_else(|| show_error("Sign in to YouTube Music to see your library."))
    }
}

fn live_opener() -> Opener {
    Arc::new(|cookie| {
        Box::pin(async move {
            let handle = tokio::runtime::Handle::current();
            match YtMusic::<BrowserToken>::from_cookie(&cookie).await {
                Ok(client) => {
                    let catalogue: Arc<dyn Catalogue> =
                        Arc::new(BrowserCatalogue { client, handle });
                    Ok(catalogue)
                }
                Err(error) => Err(scrub(&cookie, &error.to_string())),
            }
        })
    })
}

struct BrowserCatalogue {
    client: YtMusic<BrowserToken>,
    handle: tokio::runtime::Handle,
}

impl Catalogue for BrowserCatalogue {
    fn playlists(&self) -> Result<Vec<Playlist>, Error> {
        let client = self.client.clone();
        self.handle.block_on(async move {
            let query = GetLibraryPlaylistsQuery;
            let rows = collect_query(&client, &query, PLAYLIST_CAP).await?;
            Ok(with_liked(
                rows.into_iter().filter_map(playlist_from_library).collect(),
            ))
        })
    }

    fn tracks(&self, playlist_id: &str) -> Result<Vec<Track>, Error> {
        let client = self.client.clone();
        let playlist_id = playlist_id.to_string();
        self.handle.block_on(async move {
            if playlist_id == LIKED_SONGS {
                let query = GetLibrarySongsQuery::default();
                let songs = collect_query(&client, &query, TRACK_CAP).await?;
                return tracks_from_parts(
                    songs.into_iter().map(listed_from_library_song).collect(),
                );
            }
            let browse = browse_playlist_id(&playlist_id);
            let query = GetPlaylistTracksQuery::new(PlaylistID::from_raw(browse));
            let items = collect_query(&client, &query, TRACK_CAP).await?;
            let listed = items
                .into_iter()
                .filter_map(listed_from_playlist_item)
                .collect();
            tracks_from_parts(listed)
        })
    }
}

/// One continuation page. `continuation` is the token for the next page.
struct ContinuationPage<T> {
    items: Vec<T>,
    continuation: Option<String>,
}

/// Follow continuation tokens until the list ends, a token repeats, or `cap`
/// items have been kept.
///
/// `fetch(None)` is the first page. A later call gets the previous page's
/// token. The token is trimmed. A blank token ends the walk. A token that
/// was already requested is not requested again.
fn walk_continuations<T, F>(cap: usize, mut fetch: F) -> Vec<T>
where
    F: FnMut(Option<&str>) -> ContinuationPage<T>,
{
    if cap == 0 {
        return Vec::new();
    }
    let mut seen = HashSet::new();
    let mut kept = Vec::new();
    let mut pending: Option<String> = None;
    loop {
        if kept.len() >= cap {
            break;
        }
        let request = pending.as_deref();
        if let Some(token) = request
            && !seen.insert(token.to_string())
        {
            break;
        }
        let page = fetch(request);
        let room = cap - kept.len();
        let available = page.items.len();
        kept.extend(page.items.into_iter().take(room));
        if available > room || kept.len() >= cap {
            break;
        }
        pending = usable_token(page.continuation);
        if pending.is_none() {
            break;
        }
    }
    kept
}

fn usable_token(token: Option<String>) -> Option<String> {
    let token = token?.trim().to_string();
    if token.is_empty() { None } else { Some(token) }
}

/// `true` while another streamed page should be requested.
///
/// Pages and items share `cap`, so a continuation that returns nothing
/// cannot be followed forever.
fn another_stream_page<T>(pages: &[Vec<T>], cap: usize) -> bool {
    cap > 0 && pages.len() < cap && page_len(pages) < cap
}

fn page_len<T>(pages: &[Vec<T>]) -> usize {
    pages.iter().map(Vec::len).sum()
}

/// Read every continuation page ytmapi-rs yields, then apply [`walk_continuations`].
///
/// The crate follows YouTube's continuation token. The tokens used here only
/// link those pages so the cap and the repeated-token stop stay in one place.
async fn collect_query<Q, T>(
    client: &YtMusic<BrowserToken>,
    query: &Q,
    cap: usize,
) -> Result<Vec<T>, Error>
where
    Q: Query<BrowserToken> + PostQuery,
    Q::Output: ParseFromContinuable<Q> + IntoIterator<Item = T>,
{
    let mut stream = std::pin::pin!(client.stream(query));
    let mut pages = Vec::new();
    while another_stream_page(&pages, cap) {
        match stream.next().await {
            None => break,
            Some(Err(error)) => return Err(scrub("", &error.to_string())),
            Some(Ok(page)) => pages.push(page.into_iter().collect()),
        }
    }
    Ok(replay_pages(pages, cap))
}

fn replay_pages<T>(mut pages: Vec<Vec<T>>, cap: usize) -> Vec<T> {
    let count = pages.len();
    if count == 0 {
        return Vec::new();
    }
    let continuations: Vec<Option<String>> = (0..count)
        .map(|index| {
            if index + 1 == count {
                None
            } else {
                Some(format!("p{}", index + 1))
            }
        })
        .collect();
    walk_continuations(cap, |token| {
        let index = match token {
            None => 0,
            Some(token) => token
                .strip_prefix('p')
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|index| *index < count)
                .unwrap_or(count),
        };
        if index >= count {
            return ContinuationPage {
                items: Vec::new(),
                continuation: None,
            };
        }
        ContinuationPage {
            items: std::mem::take(&mut pages[index]),
            continuation: continuations[index].clone(),
        }
    })
}

fn playlist_from_library(row: LibraryPlaylist) -> Option<Playlist> {
    playlist_from_parts(row.playlist_id.get_raw(), &row.title)
}

fn listed_from_library_song(song: TableListSong) -> ListedTrack {
    ListedTrack {
        id: song.video_id.get_raw().to_string(),
        title: song.title,
        artists: song.artists.into_iter().map(|artist| artist.name).collect(),
        album: Some(song.album.name),
        duration: song.duration,
    }
}

fn listed_from_playlist_item(item: PlaylistItem) -> Option<ListedTrack> {
    Some(match item {
        PlaylistItem::Song(song) => ListedTrack {
            id: song.video_id.get_raw().to_string(),
            title: song.title,
            artists: song.artists.into_iter().map(|artist| artist.name).collect(),
            album: Some(song.album.name),
            duration: song.duration,
        },
        PlaylistItem::Video(video) => ListedTrack {
            id: video.video_id.get_raw().to_string(),
            title: video.title,
            artists: vec![video.channel_name],
            album: None,
            duration: video.duration,
        },
        PlaylistItem::UploadSong(song) => ListedTrack {
            id: song.video_id.get_raw().to_string(),
            title: song.title,
            artists: song.artists.into_iter().map(|artist| artist.name).collect(),
            album: song.album.map(|album| album.name),
            duration: song.duration,
        },
        PlaylistItem::Episode(_) => return None,
    })
}

struct ListedTrack {
    id: String,
    title: String,
    artists: Vec<String>,
    album: Option<String>,
    duration: String,
}

/// A library playlist id has to be prefixed before YouTube Music will browse it.
fn browse_playlist_id(id: &str) -> String {
    if id.starts_with("VL") {
        id.to_string()
    } else {
        format!("VL{id}")
    }
}

fn playlist_from_parts(id: &str, title: &str) -> Option<Playlist> {
    let id = id.trim();
    if id.is_empty() || id == LIKED_SONGS {
        return None;
    }
    let title = title.trim();
    let name = if title.is_empty() {
        "Untitled playlist".to_string()
    } else {
        title.to_string()
    };
    Some(Playlist {
        id: id.to_string(),
        source: Source::YouTubeMusic,
        name,
    })
}

fn with_liked(mut playlists: Vec<Playlist>) -> Vec<Playlist> {
    playlists.retain(|playlist| playlist.id != LIKED_SONGS);
    playlists.insert(0, liked_playlist());
    playlists
}

fn liked_playlist() -> Playlist {
    Playlist {
        id: LIKED_SONGS.to_string(),
        source: Source::YouTubeMusic,
        name: LIKED_SONGS_NAME.to_string(),
    }
}

fn tracks_from_parts(items: Vec<ListedTrack>) -> Result<Vec<Track>, Error> {
    let mut tracks = Vec::new();
    let mut last_error = None;
    for item in items {
        match track_from_parts(item) {
            Ok(track) => tracks.push(track),
            Err(error) => last_error = Some(error),
        }
    }
    if tracks.is_empty()
        && let Some(error) = last_error
    {
        return Err(error);
    }
    Ok(tracks)
}

fn track_from_parts(item: ListedTrack) -> Result<Track, Error> {
    let id = item.id.trim().to_string();
    if id.is_empty() {
        return Err(show_error("a library track had no video id"));
    }
    let album = item.album.and_then(|name| {
        let name = name.trim().to_string();
        if name.is_empty() { None } else { Some(name) }
    });
    let artists = item
        .artists
        .into_iter()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect();
    Ok(Track {
        id,
        source: Source::YouTubeMusic,
        title: item.title,
        artists,
        album,
        duration: parse_duration(&item.duration)?,
    })
}

/// Accept a Cookie header that contains a SAPISID value.
///
/// ytmapi-rs reads that value only up to the next semicolon, so a header
/// that ends on SAPISID gets one.
pub fn normalize_session_cookie(raw: &str) -> Result<String, Error> {
    let raw = raw.trim();
    if raw.is_empty() || raw.chars().count() > MAX_COOKIE_CHARS {
        return Err(cookie_rejected());
    }
    let Some((_, after)) = raw.split_once("SAPISID=") else {
        return Err(cookie_rejected());
    };
    let value = after.split([';', ' ', '\n', '\r']).next().unwrap_or("");
    if value.is_empty() {
        return Err(cookie_rejected());
    }
    let mut cookie = raw.to_string();
    if !cookie.ends_with(';') {
        cookie.push(';');
    }
    Ok(cookie)
}

fn cookie_rejected() -> Error {
    show_error("Paste the YouTube Music cookie header, including SAPISID.")
}

fn scrub(secret: &str, message: &str) -> Error {
    let mut text = message.to_string();
    if !secret.is_empty() {
        text = text.replace(secret, "[redacted]");
    }
    text = scrub_sapisid(&text);
    show_error(text)
}

fn scrub_sapisid(message: &str) -> String {
    let mut out = String::new();
    let mut rest = message;
    while let Some(index) = rest.find("SAPISID=") {
        out.push_str(&rest[..index]);
        out.push_str("SAPISID=[redacted]");
        rest = &rest[index + "SAPISID=".len()..];
        if let Some(end) = rest.find([';', ' ', '\n', '\r']) {
            rest = &rest[end..];
        } else {
            rest = "";
        }
    }
    out.push_str(rest);
    out
}

fn show_error(message: impl Into<String>) -> Error {
    let message = message.into();
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
    Error::Unavailable(brief)
}

#[derive(Clone, Copy)]
enum Host {
    Linux,
    Macos,
    Windows,
}

fn config_dir_from(
    host: Host,
    xdg: Option<&OsStr>,
    home: Option<&OsStr>,
    appdata: Option<&OsStr>,
) -> Result<PathBuf, Error> {
    if let Some(xdg) = xdg.filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(xdg).join("codevox-mu"));
    }
    match host {
        Host::Windows => appdata
            .filter(|value| !value.is_empty())
            .map(|dir| PathBuf::from(dir).join("codevox-mu"))
            .ok_or_else(|| show_error("could not find a per-user config directory")),
        Host::Macos => home
            .filter(|value| !value.is_empty())
            .map(|dir| {
                PathBuf::from(dir)
                    .join("Library")
                    .join("Application Support")
                    .join("codevox-mu")
            })
            .ok_or_else(|| show_error("could not find a per-user config directory")),
        Host::Linux => home
            .filter(|value| !value.is_empty())
            .map(|dir| PathBuf::from(dir).join(".config").join("codevox-mu"))
            .ok_or_else(|| show_error("could not find a per-user config directory")),
    }
}

fn config_dir() -> Result<PathBuf, Error> {
    let host = if cfg!(windows) {
        Host::Windows
    } else if cfg!(target_os = "macos") {
        Host::Macos
    } else {
        Host::Linux
    };
    config_dir_from(
        host,
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("APPDATA").as_deref(),
    )
}

/// Keyring wins. A missing or broken keyring falls through to the file.
fn preferred_cookie(keyring: Result<Option<String>, ()>, file: Option<String>) -> Option<String> {
    if let Ok(Some(value)) = keyring
        && !value.is_empty()
    {
        return Some(value);
    }
    file.filter(|value| !value.is_empty())
}

struct OsSecrets;

impl SecretStore for OsSecrets {
    fn load(&self) -> Result<Option<String>, Error> {
        let keyring = match keyring_load() {
            Ok(value) => Ok(value),
            Err(_) => Err(()),
        };
        if let Ok(Some(value)) = &keyring
            && !value.is_empty()
        {
            return Ok(Some(value.clone()));
        }
        let dir = config_dir()?;
        let file = read_cookie_file(&dir)?;
        Ok(preferred_cookie(keyring, file))
    }

    fn save(&self, secret: &str) -> Result<(), Error> {
        if keyring_save(secret).is_ok() {
            if let Ok(dir) = config_dir() {
                let _ = delete_cookie_file(&dir);
            }
            return Ok(());
        }
        let dir = config_dir()?;
        write_cookie_file(&dir, secret)
    }

    fn delete(&self) -> Result<(), Error> {
        let keyring_deleted = keyring_delete().is_ok();
        let file_deleted = match config_dir() {
            Ok(dir) => delete_cookie_file(&dir).is_ok(),
            Err(_) => false,
        };
        if keyring_deleted || file_deleted {
            Ok(())
        } else {
            Err(show_error(
                "could not remove the saved YouTube Music sign-in",
            ))
        }
    }
}

fn keyring_entry() -> Result<keyring::Entry, keyring::Error> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
}

fn keyring_load() -> Result<Option<String>, keyring::Error> {
    let entry = keyring_entry()?;
    match entry.get_password() {
        Ok(value) => Ok(Some(value)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(error),
    }
}

fn keyring_save(secret: &str) -> Result<(), keyring::Error> {
    keyring_entry()?.set_password(secret)
}

fn keyring_delete() -> Result<(), keyring::Error> {
    match keyring_entry()?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(error),
    }
}

fn write_cookie_file(dir: &Path, secret: &str) -> Result<(), Error> {
    fs::create_dir_all(dir).map_err(|_| show_error("could not create the config directory"))?;
    lock_down_dir(dir)?;
    let path = dir.join(COOKIE_FILE);
    let mut file = open_cookie_file(&path)?;
    file.write_all(secret.as_bytes())
        .map_err(|_| show_error("could not store the YouTube Music sign-in"))?;
    lock_down_file(&path)?;
    Ok(())
}

fn read_cookie_file(dir: &Path) -> Result<Option<String>, Error> {
    let path = dir.join(COOKIE_FILE);
    match fs::read_to_string(&path) {
        Ok(value) => Ok(Some(value).filter(|cookie| !cookie.is_empty())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(show_error("could not read the saved YouTube Music sign-in")),
    }
}

fn delete_cookie_file(dir: &Path) -> Result<(), Error> {
    let path = dir.join(COOKIE_FILE);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(show_error(
            "could not remove the saved YouTube Music sign-in",
        )),
    }
}

fn open_cookie_file(path: &Path) -> Result<std::fs::File, Error> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|_| show_error("could not store the YouTube Music sign-in"))
}

fn lock_down_dir(dir: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .map_err(|_| show_error("could not protect the config directory"))?;
    }
    let _ = dir;
    Ok(())
}

fn lock_down_file(path: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|_| show_error("could not protect the YouTube Music sign-in"))?;
    }
    let _ = path;
    Ok(())
}

#[cfg(test)]
struct FixedCatalogue {
    playlists: Vec<Playlist>,
    tracks: Vec<(String, Vec<Track>)>,
}

#[cfg(test)]
impl Catalogue for FixedCatalogue {
    fn playlists(&self) -> Result<Vec<Playlist>, Error> {
        Ok(with_liked(self.playlists.clone()))
    }

    fn tracks(&self, playlist_id: &str) -> Result<Vec<Track>, Error> {
        self.tracks
            .iter()
            .find(|(id, _)| id == playlist_id)
            .map(|(_, tracks)| tracks.clone())
            .ok_or(Error::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const MARKER: &str = "MARKER-42";

    fn sample_track(id: &str, title: &str) -> Track {
        Track {
            id: id.to_string(),
            source: Source::YouTubeMusic,
            title: title.to_string(),
            artists: vec!["Ada Vega".to_string()],
            album: Some("Low Glow".to_string()),
            duration: Duration::from_secs(200),
        }
    }

    struct MemorySecrets {
        value: Mutex<Option<String>>,
    }

    impl MemorySecrets {
        fn empty() -> Arc<Self> {
            Arc::new(Self {
                value: Mutex::new(None),
            })
        }
    }

    impl SecretStore for MemorySecrets {
        fn load(&self) -> Result<Option<String>, Error> {
            Ok(self.value.lock().expect("memory").clone())
        }

        fn save(&self, secret: &str) -> Result<(), Error> {
            *self.value.lock().expect("memory") = Some(secret.to_string());
            Ok(())
        }

        fn delete(&self) -> Result<(), Error> {
            *self.value.lock().expect("memory") = None;
            Ok(())
        }
    }

    fn fixed_opener(playlists: Vec<Playlist>, tracks: Vec<(String, Vec<Track>)>) -> Opener {
        Arc::new(move |_cookie| {
            let playlists = playlists.clone();
            let tracks = tracks.clone();
            Box::pin(async move {
                let catalogue: Arc<dyn Catalogue> = Arc::new(FixedCatalogue { playlists, tracks });
                Ok(catalogue)
            })
        })
    }

    #[test]
    fn a_cookie_without_sapisid_is_rejected_and_the_text_stays_out_of_the_error() {
        let error = normalize_session_cookie(&format!("VISITOR={MARKER}")).expect_err("missing");
        let text = error.to_string();
        assert!(text.contains("SAPISID"), "{text}");
        assert!(!text.contains(MARKER), "{text}");
        assert!(normalize_session_cookie("").is_err());
        assert!(normalize_session_cookie("SAPISID=").is_err());
        assert!(normalize_session_cookie("SAPISID=; HSID=a").is_err());
    }

    #[test]
    fn a_long_paste_is_rejected_without_quoting_it() {
        let body = format!("SAPISID={MARKER}{}", "x".repeat(MAX_COOKIE_CHARS));
        let error = normalize_session_cookie(&body).expect_err("too long");
        assert!(!error.to_string().contains(MARKER));
    }

    #[test]
    fn a_sapisid_cookie_is_kept_and_gains_a_trailing_semicolon() {
        let cookie = normalize_session_cookie(" HSID=a; SAPISID=abc ").expect("cookie");
        assert_eq!(cookie, "HSID=a; SAPISID=abc;");
        let already = normalize_session_cookie("SAPISID=abc;").expect("cookie");
        assert_eq!(already, "SAPISID=abc;");
    }

    #[test]
    fn an_error_that_echoes_the_cookie_is_scrubbed() {
        let secret = "HSID=a; SAPISID=abc;";
        let error = scrub(secret, "rejected HSID=a; SAPISID=abc; by the server");
        let text = error.to_string();
        assert!(!text.contains("abc"), "{text}");
        assert!(text.contains("[redacted]"), "{text}");
    }

    #[test]
    fn the_keyring_cookie_wins_and_a_broken_keyring_uses_the_file() {
        assert_eq!(
            preferred_cookie(Ok(Some("key".into())), Some("file".into())).as_deref(),
            Some("key")
        );
        assert_eq!(
            preferred_cookie(Err(()), Some("file".into())).as_deref(),
            Some("file")
        );
        assert_eq!(
            preferred_cookie(Ok(None), Some("file".into())).as_deref(),
            Some("file")
        );
        assert_eq!(preferred_cookie(Ok(Some(String::new())), None), None);
        assert_eq!(preferred_cookie(Err(()), None), None);
    }

    #[test]
    fn config_dirs_follow_the_host_and_xdg_wins() {
        let xdg = OsStr::new("/tmp/xdg");
        let home = OsStr::new("/Users/ada");
        let appdata = OsStr::new("C:\\Users\\ada\\AppData\\Roaming");
        assert_eq!(
            config_dir_from(Host::Linux, Some(xdg), Some(home), None).expect("xdg"),
            PathBuf::from("/tmp/xdg/codevox-mu")
        );
        assert_eq!(
            config_dir_from(Host::Macos, None, Some(home), None).expect("mac"),
            PathBuf::from("/Users/ada/Library/Application Support/codevox-mu")
        );
        assert_eq!(
            config_dir_from(Host::Windows, None, None, Some(appdata)).expect("windows"),
            PathBuf::from(appdata).join("codevox-mu")
        );
        assert_eq!(
            config_dir_from(Host::Linux, None, Some(home), None).expect("linux"),
            PathBuf::from("/Users/ada/.config/codevox-mu")
        );
        assert!(config_dir_from(Host::Linux, None, None, None).is_err());
    }

    #[test]
    fn the_cookie_file_is_private_and_sign_out_removes_it() {
        let dir = std::env::temp_dir().join(format!(
            "codevox-mu-ytm-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let secret = "SAPISID=test-value;";
        write_cookie_file(&dir, secret).expect("write");
        assert_eq!(
            read_cookie_file(&dir).expect("read").as_deref(),
            Some(secret)
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file_mode = fs::metadata(dir.join(COOKIE_FILE))
                .expect("file")
                .permissions()
                .mode()
                & 0o777;
            let dir_mode = fs::metadata(&dir).expect("dir").permissions().mode() & 0o777;
            assert_eq!(file_mode, 0o600, "cookie file mode {file_mode:o}");
            assert_eq!(dir_mode, 0o700, "config dir mode {dir_mode:o}");
        }
        write_cookie_file(&dir, "SAPISID=replaced;").expect("replace");
        assert_eq!(
            read_cookie_file(&dir).expect("read").as_deref(),
            Some("SAPISID=replaced;")
        );
        delete_cookie_file(&dir).expect("delete");
        assert_eq!(read_cookie_file(&dir).expect("gone"), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn library_rows_start_with_liked_songs_and_skip_blank_ids() {
        let rows = with_liked(
            ["", "  ", LIKED_SONGS, "PLnight"]
                .into_iter()
                .filter_map(|id| {
                    playlist_from_parts(id, if id == "PLnight" { "Night" } else { "" })
                })
                .collect(),
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, LIKED_SONGS);
        assert_eq!(rows[0].name, "Liked songs");
        assert_eq!(rows[0].source, Source::YouTubeMusic);
        assert_eq!(rows[1].id, "PLnight");
        assert_eq!(rows[1].name, "Night");
        assert_eq!(
            playlist_from_parts("PLblank", "  ").expect("untitled").name,
            "Untitled playlist"
        );
    }

    #[test]
    fn playlist_browse_ids_gain_the_vl_prefix_once() {
        assert_eq!(browse_playlist_id("PLnight"), "VLPLnight");
        assert_eq!(browse_playlist_id("VLPLnight"), "VLPLnight");
    }

    #[test]
    fn a_library_track_keeps_the_video_id_the_player_already_uses() {
        let track = track_from_parts(ListedTrack {
            id: " abcdefghijk ".into(),
            title: "Night Drive".into(),
            artists: vec!["Ada Vega".into(), " ".into()],
            album: Some("  ".into()),
            duration: "3:20".into(),
        })
        .expect("track");
        assert_eq!(track.id, "abcdefghijk");
        assert_eq!(track.source, Source::YouTubeMusic);
        assert_eq!(track.artists, ["Ada Vega"]);
        assert_eq!(track.album, None);
        assert_eq!(track.duration, Duration::from_secs(200));
        assert!(
            track_from_parts(ListedTrack {
                id: " ".into(),
                title: "x".into(),
                artists: Vec::new(),
                album: None,
                duration: "1:00".into(),
            })
            .is_err()
        );
        let mixed = tracks_from_parts(vec![
            ListedTrack {
                id: "abcdefghijk".into(),
                title: "Night Drive".into(),
                artists: vec!["Ada Vega".into()],
                album: None,
                duration: "3:20".into(),
            },
            ListedTrack {
                id: "bad".into(),
                title: "Live".into(),
                artists: Vec::new(),
                album: None,
                duration: "live".into(),
            },
        ])
        .expect("keeps the readable row");
        assert_eq!(mixed.len(), 1);
        assert_eq!(mixed[0].id, "abcdefghijk");
    }

    #[test]
    fn sign_in_stores_the_cookie_and_lists_playlists_and_tracks() {
        let secrets = MemorySecrets::empty();
        let night = Playlist {
            id: "PLnight".into(),
            source: Source::YouTubeMusic,
            name: "Night".into(),
        };
        let track = sample_track("abcdefghijk", "Night Drive");
        let account = Account::from_parts(
            Arc::clone(&secrets) as Arc<dyn SecretStore>,
            fixed_opener(
                vec![night.clone()],
                vec![
                    (LIKED_SONGS.to_string(), vec![track.clone()]),
                    ("PLnight".into(), vec![track.clone()]),
                ],
            ),
        );
        assert!(
            account
                .sign_in(&format!("VISITOR={MARKER}"))
                .expect_err("no sapisid")
                .to_string()
                .contains("SAPISID")
        );
        assert!(!account.signed_in());
        assert!(secrets.load().expect("load").is_none());
        assert!(account.runtime.lock().expect("runtime").is_none());

        account.sign_in("SAPISID=test-value").expect("sign in");
        assert!(account.signed_in());
        assert_eq!(
            secrets.load().expect("load").as_deref(),
            Some("SAPISID=test-value;")
        );
        let playlists = account.playlists().expect("playlists");
        assert_eq!(playlists[0].name, "Liked songs");
        assert_eq!(playlists[1], night);
        let tracks = account.playlist_tracks("PLnight").expect("tracks");
        assert_eq!(tracks, vec![track.clone()]);
        assert_eq!(
            account.playlist_tracks(LIKED_SONGS).expect("liked"),
            vec![track]
        );
        assert_eq!(account.playlist_tracks("missing"), Err(Error::NotFound));

        account.sign_out().expect("sign out");
        assert!(!account.signed_in());
        assert!(secrets.load().expect("load").is_none());
        assert!(account.playlists().is_err());
    }

    #[test]
    fn a_failed_sign_in_does_not_store_the_cookie_and_hides_it() {
        let secrets = MemorySecrets::empty();
        let opener: Opener = Arc::new(|cookie| {
            Box::pin(async move { Err(show_error(format!("server said {cookie}"))) })
        });
        let account = Account::from_parts(Arc::clone(&secrets) as Arc<dyn SecretStore>, opener);
        let error = account
            .sign_in("SAPISID=test-value")
            .expect_err("exchange failed");
        assert!(!error.to_string().contains("test-value"), "{error}");
        assert!(!account.signed_in());
        assert!(secrets.load().expect("load").is_none());
    }

    #[test]
    fn sign_out_during_sign_in_does_not_keep_the_cookie() {
        let secrets = MemorySecrets::empty();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
        let entered_tx = Arc::new(Mutex::new(Some(entered_tx)));
        let gate_rx = Arc::new(Mutex::new(Some(gate_rx)));
        let opener: Opener = Arc::new(move |_cookie| {
            let entered_tx = Arc::clone(&entered_tx);
            let gate_rx = Arc::clone(&gate_rx);
            Box::pin(async move {
                entered_tx
                    .lock()
                    .expect("entered")
                    .take()
                    .expect("once")
                    .send(())
                    .expect("entered");
                gate_rx
                    .lock()
                    .expect("gate")
                    .take()
                    .expect("once")
                    .recv()
                    .expect("release");
                let catalogue: Arc<dyn Catalogue> = Arc::new(FixedCatalogue {
                    playlists: Vec::new(),
                    tracks: Vec::new(),
                });
                Ok(catalogue)
            })
        });
        let account = Arc::new(Account::from_parts(
            Arc::clone(&secrets) as Arc<dyn SecretStore>,
            opener,
        ));
        let signing_in = {
            let account = Arc::clone(&account);
            std::thread::spawn(move || account.sign_in("SAPISID=test-value"))
        };
        entered_rx.recv().expect("started");
        account.sign_out().expect("cancel");
        gate_tx.send(()).expect("release");
        let error = signing_in.join().expect("thread").expect_err("cancelled");
        assert!(error.to_string().contains("cancelled"), "{error}");
        assert!(!account.signed_in());
        assert!(secrets.load().expect("load").is_none());
    }

    const PLAYLIST_PAGES: &str =
        include_str!("../tests/fixtures/youtube_music_playlist_pages.json");

    #[derive(serde::Deserialize)]
    struct FixtureFile {
        ordered: Vec<FixturePage>,
        repeated_token: Vec<FixturePage>,
    }

    #[derive(serde::Deserialize)]
    struct FixturePage {
        token: Option<String>,
        continuation: Option<String>,
        tracks: Vec<FixtureTrack>,
    }

    #[derive(serde::Deserialize)]
    struct FixtureTrack {
        id: String,
        title: String,
        artists: Vec<String>,
        album: Option<String>,
        duration: String,
    }

    fn listed_fixture(track: &FixtureTrack) -> ListedTrack {
        ListedTrack {
            id: track.id.clone(),
            title: track.title.clone(),
            artists: track.artists.clone(),
            album: track.album.clone(),
            duration: track.duration.clone(),
        }
    }

    fn walk_fixture(pages: &[FixturePage], cap: usize) -> (Vec<Track>, usize) {
        let mut calls = 0;
        let listed = walk_continuations(cap, |token| {
            calls += 1;
            if calls > pages.len() + 1 {
                return ContinuationPage {
                    items: Vec::new(),
                    continuation: None,
                };
            }
            let page = pages
                .iter()
                .find(|page| page.token.as_deref() == token)
                .expect("fixture page");
            ContinuationPage {
                items: page.tracks.iter().map(listed_fixture).collect(),
                continuation: page.continuation.clone(),
            }
        });
        (tracks_from_parts(listed).expect("tracks"), calls)
    }

    #[test]
    fn playlist_pages_follow_continuations_in_order_and_stop() {
        let fixture: FixtureFile = serde_json::from_str(PLAYLIST_PAGES).expect("fixture");
        let (tracks, calls) = walk_fixture(&fixture.ordered, TRACK_CAP);
        assert_eq!(calls, 3);
        assert_eq!(
            tracks
                .iter()
                .map(|track| track.id.as_str())
                .collect::<Vec<_>>(),
            ["aaaaaaaaaaa", "bbbbbbbbbbb", "ddddddddddd"]
        );
        assert!(
            tracks
                .iter()
                .all(|track| track.source == Source::YouTubeMusic)
        );
        assert_eq!(tracks[0].title, "Night Drive");
        assert_eq!(tracks[0].artists, ["Ada Vega"]);
        assert_eq!(tracks[0].album.as_deref(), Some("Low Glow"));
        assert_eq!(tracks[0].duration, Duration::from_secs(192));
        assert_eq!(tracks[1].artists, ["Ada Vega", "North Line"]);
        assert_eq!(tracks[1].duration, Duration::from_secs(241));
        assert_eq!(tracks[2].title, "Paper Boats");
        assert_eq!(tracks[2].album, None);
        assert_eq!(tracks[2].duration, Duration::from_secs(164));

        let (capped, calls) = walk_fixture(&fixture.ordered, 1);
        assert_eq!(calls, 1, "the cap stops before the next continuation");
        assert_eq!(capped.len(), 1);
        assert_eq!(capped[0].id, "aaaaaaaaaaa");

        let (repeated, calls) = walk_fixture(&fixture.repeated_token, TRACK_CAP);
        assert_eq!(calls, 2, "a repeated continuation is not requested again");
        assert_eq!(
            repeated
                .iter()
                .map(|track| track.id.as_str())
                .collect::<Vec<_>>(),
            ["aaaaaaaaaaa", "bbbbbbbbbbb"]
        );
    }

    #[test]
    fn streamed_pages_stop_at_the_cap_without_another_page() {
        let mut pages = Vec::new();
        assert!(another_stream_page(&pages, 2));
        pages.push(vec!["a"]);
        assert!(another_stream_page(&pages, 2));
        pages.push(vec!["b", "c"]);
        assert!(
            !another_stream_page(&pages, 2),
            "two items fill the cap, so the next page is not requested"
        );
        assert_eq!(replay_pages(pages, 2), ["a", "b"]);

        let full = vec![vec!["a", "b"]];
        assert!(!another_stream_page(&full, 2));
        assert_eq!(replay_pages(full, 2), ["a", "b"]);
        assert!(replay_pages(Vec::<Vec<&str>>::new(), 10).is_empty());
        assert!(
            walk_continuations(0, |_| ContinuationPage {
                items: vec!["a"],
                continuation: Some("next".into()),
            })
            .is_empty()
        );
    }

    #[test]
    fn a_blank_continuation_ends_the_walk() {
        let tracks = walk_continuations(10, |token| {
            if token.is_none() {
                ContinuationPage {
                    items: vec!["first".to_string()],
                    continuation: Some("   ".into()),
                }
            } else {
                panic!("a blank token is not requested");
            }
        });
        assert_eq!(tracks, ["first"]);
    }
}
