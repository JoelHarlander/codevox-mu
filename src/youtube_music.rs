//! YouTube Music search, and the signed-in library.
//!
//! The HTTP client is [ytmapi-rs](https://crates.io/crates/ytmapi-rs) (MIT),
//! which speaks YouTube Music's InnerTube API. Search does not need an
//! account. The library and playlists do: [`crate::youtube_account`] stores a
//! browser cookie and opens a signed-in client. No API key is stored here;
//! the crate carries the public web client key.
//!
//! [`YouTubeMusic::search`] blocks on the network. The window runs it on a
//! background thread. Parsing is separate, so a recorded response can be
//! tested without a socket.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ytmapi_rs::YtMusic;
use ytmapi_rs::auth::noauth::NoAuthToken;
use ytmapi_rs::common::YoutubeID;
use ytmapi_rs::parse::SearchResultSong;
use ytmapi_rs::query::SearchQuery;
use ytmapi_rs::query::search::{FilteredSearch, SongsFilter};

use crate::music::{Error, MusicService, Playlist, Source, Track};
use crate::player::{YoutubeMatch, match_query};
use crate::youtube_account::{Account, LibrarySession};

/// How long an error row is allowed to get. The API text can be a whole body.
const ERROR_CHARS: usize = 160;

/// Song search against YouTube Music, with no account.
///
/// The client is built on the first search and then reused. Building it
/// fetches a visitor id from the YouTube Music site. That call is not made
/// until something is searched, so `--smoke` stays off the network.
#[derive(Clone)]
pub struct YouTubeMusic {
    inner: Arc<Inner>,
}

struct Inner {
    runtime: Mutex<Option<tokio::runtime::Runtime>>,
    client: Mutex<Option<YtMusic<NoAuthToken>>>,
    account: Account,
}

impl YouTubeMusic {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                runtime: Mutex::new(None),
                client: Mutex::new(None),
                account: Account::live(),
            }),
        }
    }

    /// Read a song-search response body into tracks.
    ///
    /// `body` is the JSON YouTube Music returned. Nothing is fetched.
    pub fn tracks_from_song_search_json(body: &str) -> Result<Vec<Track>, Error> {
        let query = SearchQuery::<FilteredSearch<SongsFilter>>::new_filtered("", SongsFilter);
        let songs: Vec<SearchResultSong> = ytmapi_rs::process_json::<
            SearchQuery<'_, FilteredSearch<SongsFilter>>,
            NoAuthToken,
        >(body.to_string(), &query)
        .map_err(brief)?;
        songs.into_iter().map(track_from_song).collect()
    }
}

impl MusicService for YouTubeMusic {
    fn search(&self, query: &str) -> Result<Vec<Track>, Error> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }

        let runtime = self.handle()?;
        let service = self.clone();
        let query = query.to_string();
        let (sender, receiver) = std::sync::mpsc::channel();
        runtime.spawn(async move {
            let result = service.songs(&query).await;
            let _ = sender.send(result);
        });
        receiver
            .recv()
            .map_err(|_| brief_message("the search task stopped before it replied"))?
    }

    fn resolve(&self, _id: &str) -> Result<Track, Error> {
        // Search does not keep a catalogue, and resolving a video is playback.
        Err(Error::NotFound)
    }

    fn playlists(&self) -> Result<Vec<Playlist>, Error> {
        self.inner.account.playlists()
    }

    fn playlist_tracks(&self, playlist_id: &str) -> Result<Vec<Track>, Error> {
        self.inner.account.playlist_tracks(playlist_id)
    }
}

impl LibrarySession for YouTubeMusic {
    fn signed_in(&self) -> bool {
        self.inner.account.signed_in()
    }

    fn sign_in(&self, cookie: &str) -> Result<(), Error> {
        self.inner.account.sign_in(cookie)
    }

    fn restore(&self) -> Result<bool, Error> {
        self.inner.account.restore()
    }

    fn sign_out(&self) -> Result<(), Error> {
        self.inner.account.sign_out()
    }

    fn playlists(&self) -> Result<Vec<Playlist>, Error> {
        self.inner.account.playlists()
    }

    fn playlist_tracks(&self, playlist_id: &str) -> Result<Vec<Track>, Error> {
        self.inner.account.playlist_tracks(playlist_id)
    }
}

impl YouTubeMusic {
    fn handle(&self) -> Result<tokio::runtime::Handle, Error> {
        let mut guard = self
            .inner
            .runtime
            .lock()
            .map_err(|_| brief_message("search state was poisoned"))?;
        if guard.is_none() {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_io()
                .enable_time()
                .thread_name("youtube-music")
                .build()
                .map_err(|error| {
                    brief_message(format!("could not start the search runtime: {error}"))
                })?;
            *guard = Some(runtime);
        }
        let runtime = guard
            .as_ref()
            .ok_or_else(|| brief_message("search runtime was not started"))?;
        Ok(runtime.handle().clone())
    }

    async fn songs(&self, query: &str) -> Result<Vec<Track>, Error> {
        let client = self.client().await?;
        let query = SearchQuery::<FilteredSearch<SongsFilter>>::new_filtered(query, SongsFilter);
        // Same parser as the fixture test. The client returns a JSON value;
        // writing it back out keeps one read path.
        let json = client
            .json_query::<SearchQuery<'_, FilteredSearch<SongsFilter>>>(&query)
            .await
            .map_err(brief)?;
        let body = serde_json::to_string(&json).map_err(brief)?;
        Self::tracks_from_song_search_json(&body)
    }

    async fn client(&self) -> Result<YtMusic<NoAuthToken>, Error> {
        if let Some(client) = self.cached_client()? {
            return Ok(client);
        }

        let created = YtMusic::new_unauthenticated().await.map_err(brief)?;
        let mut guard = self.lock_client()?;
        if let Some(existing) = guard.clone() {
            return Ok(existing);
        }
        *guard = Some(created.clone());
        Ok(created)
    }

    fn cached_client(&self) -> Result<Option<YtMusic<NoAuthToken>>, Error> {
        Ok(self.lock_client()?.clone())
    }

    fn lock_client(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, Option<YtMusic<NoAuthToken>>>, Error> {
        self.inner
            .client
            .lock()
            .map_err(|_| brief_message("search state was poisoned"))
    }
}

fn track_from_song(song: SearchResultSong) -> Result<Track, Error> {
    let id = song.video_id.get_raw().trim().to_string();
    if id.is_empty() {
        return Err(brief_message("song search result had no video id"));
    }

    let artists = artists_from_byline(&song.artist);

    Ok(Track {
        id,
        source: Source::YouTubeMusic,
        title: song.title,
        artists,
        album: song
            .album
            .map(|album| album.name)
            .filter(|name| !name.trim().is_empty()),
        duration: parse_duration(&song.duration)?,
    })
}

/// The song row joins collaborators into one string, with `", "` between
/// names and `" & "` before the last. A name that itself contains those
/// separators is split too.
fn artists_from_byline(byline: &str) -> Vec<String> {
    byline
        .split(" & ")
        .flat_map(|part| part.split(", "))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

pub(crate) fn parse_duration(text: &str) -> Result<Duration, Error> {
    let text = text.trim();
    let mut parts = text.split(':');
    let first = parts.next();
    let second = parts.next();
    let third = parts.next();
    if parts.next().is_some() {
        return Err(brief_message(format!("unreadable duration: {text}")));
    }

    let number = |part: &str| -> Option<u64> {
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        part.parse().ok()
    };

    let seconds = match (first, second, third) {
        (Some(minutes), Some(seconds), None) => {
            let minutes = number(minutes).ok_or_else(|| bad_duration(text))?;
            let seconds = number(seconds).ok_or_else(|| bad_duration(text))?;
            if seconds >= 60 {
                return Err(bad_duration(text));
            }
            minutes.saturating_mul(60).saturating_add(seconds)
        }
        (Some(hours), Some(minutes), Some(seconds)) => {
            let hours = number(hours).ok_or_else(|| bad_duration(text))?;
            let minutes = number(minutes).ok_or_else(|| bad_duration(text))?;
            let seconds = number(seconds).ok_or_else(|| bad_duration(text))?;
            if minutes >= 60 || seconds >= 60 {
                return Err(bad_duration(text));
            }
            hours
                .saturating_mul(3600)
                .saturating_add(minutes.saturating_mul(60))
                .saturating_add(seconds)
        }
        _ => return Err(bad_duration(text)),
    };

    Ok(Duration::from_secs(seconds))
}

fn bad_duration(text: &str) -> Error {
    brief_message(format!("unreadable duration: {text}"))
}

fn brief(error: impl std::fmt::Display) -> Error {
    brief_message(error.to_string())
}

/// Plays a Spotify row that librespot cannot, by searching YouTube Music
/// for the title and artist. The first song is the match.
pub struct YouTubeFallback {
    music: YouTubeMusic,
}

impl YouTubeFallback {
    pub fn new(music: YouTubeMusic) -> Self {
        Self { music }
    }
}

impl YoutubeMatch for YouTubeFallback {
    fn find(&mut self, title: &str, artists: &[String]) -> Result<Option<Track>, String> {
        let query = match_query(title, artists);
        if query.is_empty() {
            return Ok(None);
        }
        match self.music.search(&query) {
            Ok(tracks) => Ok(tracks
                .into_iter()
                .find(|track| track.source == Source::YouTubeMusic)),
            Err(error) => Err(error.to_string()),
        }
    }
}

fn brief_message(message: impl Into<String>) -> Error {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Two song rows trimmed from a live search for "daft punk get lucky"
    /// on 11 Nov 2026. Menus, overlays, and extra thumbnails are removed.
    const SEARCH_FIXTURE: &str = include_str!("../tests/fixtures/youtube_music_search.json");

    #[test]
    fn blank_search_does_not_call_the_network() {
        let service = YouTubeMusic::new();
        assert!(service.search("").expect("blank").is_empty());
        assert!(service.search("   ").expect("blank").is_empty());
        assert!(service.inner.runtime.lock().expect("runtime").is_none());
        assert!(service.inner.client.lock().expect("client").is_none());
    }

    #[test]
    fn signed_out_library_asks_for_sign_in_and_does_not_open_a_runtime() {
        let service = YouTubeMusic::new();
        assert_eq!(service.resolve("dQw4w9WgXcQ"), Err(Error::NotFound));
        assert!(!LibrarySession::signed_in(&service));
        let playlists = LibrarySession::playlists(&service).expect_err("signed out");
        assert!(
            playlists.to_string().to_lowercase().contains("sign in"),
            "{playlists}"
        );
        assert_eq!(
            LibrarySession::playlist_tracks(&service, "list")
                .expect_err("signed out")
                .to_string(),
            playlists.to_string()
        );
        assert!(service.inner.runtime.lock().expect("runtime").is_none());
    }

    #[test]
    fn recorded_song_search_maps_tracks() {
        let tracks =
            YouTubeMusic::tracks_from_song_search_json(SEARCH_FIXTURE).expect("fixture parses");

        assert_eq!(tracks.len(), 2);
        assert!(
            tracks
                .iter()
                .all(|track| track.source == Source::YouTubeMusic)
        );

        assert_eq!(tracks[0].id, "4D7u5KF7SP8");
        assert_eq!(
            tracks[0].title,
            "Get Lucky (feat. Pharrell Williams and Nile Rodgers)"
        );
        assert_eq!(
            tracks[0].artists,
            ["Daft Punk", "Pharrell Williams", "Nile Rodgers"]
        );
        assert_eq!(tracks[0].album.as_deref(), Some("Random Access Memories"));
        assert_eq!(tracks[0].duration, Duration::from_secs(6 * 60 + 10));

        assert_eq!(tracks[1].id, "Rgrt_8mXrK8");
        assert_eq!(
            tracks[1].title,
            "Get Lucky (Radio Edit - feat. Pharrell Williams and Nile Rodgers)"
        );
        assert_eq!(
            tracks[1].artists,
            ["Daft Punk", "Pharrell Williams", "Nile Rodgers"]
        );
        assert_eq!(
            tracks[1].album.as_deref(),
            Some("Get Lucky (Radio Edit - feat. Pharrell Williams and Nile Rodgers)")
        );
        assert_eq!(tracks[1].duration, Duration::from_secs(4 * 60 + 9));
    }

    #[test]
    fn song_without_an_album_leaves_the_album_empty() {
        let body = r#"{
          "contents": {"tabbedSearchResultsRenderer": {"tabs": [{"tabRenderer": {"content": {
            "sectionListRenderer": {"contents": [{"musicShelfRenderer": {"contents": [{
              "musicResponsiveListItemRenderer": {
                "flexColumns": [
                  {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": "Night Drive"}]}}},
                  {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [
                    {"text": "Ada Vega"}, {"text": " • "}, {"text": "3:40"}
                  ]}}},
                  {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": "10 plays"}]}}}
                ],
                "playlistItemData": {"videoId": "abcdefghijk"},
                "thumbnail": {"musicThumbnailRenderer": {"thumbnail": {"thumbnails": [
                  {"url": "https://example.invalid/t.jpg", "width": 60, "height": 60}
                ]}}}
              }
            }]}}]}
          }}}]}}
        }"#;
        let tracks = YouTubeMusic::tracks_from_song_search_json(body).expect("no album");
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].artists, ["Ada Vega"]);
        assert_eq!(tracks[0].album, None);
        assert_eq!(tracks[0].duration, Duration::from_secs(220));
    }

    #[test]
    fn song_search_rejects_an_error_payload() {
        let error = YouTubeMusic::tracks_from_song_search_json(
            r#"{"error":{"code":400,"message":"Precondition check failed."}}"#,
        )
        .expect_err("api error");
        let Error::Unavailable(reason) = error else {
            panic!("an api error is not a missing id");
        };
        assert!(reason.contains("400"), "{reason}");
        assert!(reason.contains("Precondition check failed"), "{reason}");
    }

    #[test]
    fn song_search_rejects_malformed_json() {
        let error = YouTubeMusic::tracks_from_song_search_json("not json").expect_err("malformed");
        assert!(matches!(error, Error::Unavailable(_)));
    }

    #[test]
    fn durations_accept_minutes_and_hours() {
        assert_eq!(
            parse_duration("4:09").expect("minutes"),
            Duration::from_secs(249)
        );
        assert_eq!(
            parse_duration("1:02:03").expect("hours"),
            Duration::from_secs(3723)
        );
        assert_eq!(
            parse_duration(" 0:07 ").expect("padded"),
            Duration::from_secs(7)
        );
        assert!(parse_duration("4:60").is_err());
        assert!(parse_duration("1:02:60").is_err());
        assert!(parse_duration("live").is_err());
        assert!(parse_duration("1:2:3:4").is_err());
    }

    #[test]
    #[ignore = "calls YouTube Music; cargo test stays offline"]
    fn live_search_returns_tracks() {
        let tracks = YouTubeMusic::new()
            .search("daft punk get lucky")
            .expect("live search");
        assert!(!tracks.is_empty());
        for track in &tracks {
            assert_eq!(track.source, Source::YouTubeMusic);
            assert!(!track.id.is_empty());
            assert!(!track.title.is_empty());
            eprintln!(
                "{}\t{}\t{}\t{:?}",
                track.id,
                track.title,
                track.artists.join(", "),
                track.duration
            );
        }
    }
}
