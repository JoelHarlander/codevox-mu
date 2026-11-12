//! Catalogue reads shared by every music backend.
//!
//! [`MockService`] answers from a fixed list, so search can run with no
//! network and no account. The unauthenticated YouTube Music search backend
//! is [`crate::youtube_music::YouTubeMusic`]. The play queue is
//! [`crate::queue::Queue`].
//!
//! The window searches and, once an account is signed in, reads that
//! account's playlists. [`MusicService::resolve`] stays on the trait until a
//! caller needs one track by id.

#![cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "resolve stays on the trait until a caller needs one track by id"
    )
)]

use std::fmt;
use std::time::Duration;

/// Where a track or playlist comes from.
///
/// One play queue can hold any mix of these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Spotify,
    YouTubeMusic,
    Local,
}

impl Source {
    /// Short label for a result row.
    pub const fn label(self) -> &'static str {
        match self {
            Source::Spotify => "Spotify",
            Source::YouTubeMusic => "YouTube Music",
            Source::Local => "Local",
        }
    }
}

/// One playable track.
///
/// `id` is unique for a [`Source`], not across sources. The same recording
/// on Spotify and YouTube Music is two tracks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Track {
    pub id: String,
    pub source: Source,
    pub title: String,
    /// Empty when the source names no artist.
    pub artists: Vec<String>,
    /// `None` when the source has no album for this track.
    pub album: Option<String>,
    pub duration: Duration,
}

/// A named list of tracks on one service.
///
/// The tracks themselves come from [`MusicService::playlist_tracks`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Playlist {
    /// Unique for [`Playlist::source`], not across sources.
    pub id: String,
    pub source: Source,
    pub name: String,
}

/// A catalogue call that did not return tracks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// No track or playlist uses this id on this service.
    NotFound,
    /// The service could not be reached, or its reply could not be read.
    ///
    /// The string is a short reason safe to show in the window.
    Unavailable(String),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotFound => formatter.write_str("not found on this service"),
            Error::Unavailable(reason) => formatter.write_str(reason),
        }
    }
}

impl std::error::Error for Error {}

/// Catalogue reads.
///
/// The mock answers from memory. A backend that calls the network blocks
/// the caller, so the window must run that call off the UI thread.
pub trait MusicService {
    /// Tracks that match `query`.
    ///
    /// A blank query matches nothing. The mock matches title, artist, and
    /// album, ignoring case. A network backend uses that service's own
    /// search. Results stay in the service's own order.
    fn search(&self, query: &str) -> Result<Vec<Track>, Error>;

    /// The track this service would play for `id`.
    ///
    /// Ids belong to one service. Another service's id is [`Error::NotFound`].
    fn resolve(&self, id: &str) -> Result<Track, Error>;

    /// Playlists for this account. The mock returns a fixed list.
    fn playlists(&self) -> Result<Vec<Playlist>, Error>;

    /// Tracks in `playlist_id`, in playlist order.
    fn playlist_tracks(&self, playlist_id: &str) -> Result<Vec<Track>, Error>;
}

/// Search several backends and concatenate the hits.
///
/// Order is the iterator's order, then each backend's own order. The first
/// error stops the merge. Duplicates stay; the window uses
/// [`crate::search::merge_results`] to collapse them.
pub fn search_merged<'a, S>(
    services: impl IntoIterator<Item = &'a S>,
    query: &str,
) -> Result<Vec<Track>, Error>
where
    S: MusicService + ?Sized + 'a,
{
    let mut tracks = Vec::new();
    for service in services {
        tracks.extend(service.search(query)?);
    }
    Ok(tracks)
}

/// Spotify mock, then the YouTube Music mock.
///
/// An offline catalogue. The window searches the Spotify mock and the live
/// YouTube Music backend, not this pair.
pub fn mock_services() -> Vec<Box<dyn MusicService>> {
    vec![
        Box::new(MockService::spotify()),
        Box::new(MockService::youtube_music()),
    ]
}

/// Fixed catalogue for one [`Source`].
#[derive(Debug)]
pub struct MockService {
    source: Source,
    tracks: Vec<Track>,
    playlists: Vec<CatalogPlaylist>,
}

#[derive(Debug)]
struct CatalogPlaylist {
    id: String,
    name: String,
    track_ids: Vec<String>,
}

impl MockService {
    pub fn spotify() -> Self {
        let source = Source::Spotify;
        Self::new(
            source,
            vec![
                track(
                    source,
                    "sp-night-drive",
                    "Night Drive",
                    &["Ada Vega"],
                    Some("Low Glow"),
                    192,
                ),
                track(
                    source,
                    "sp-glass-house",
                    "Glass House",
                    &["Ada Vega", "North Line"],
                    Some("Low Glow"),
                    241,
                ),
                track(
                    source,
                    "sp-paper-boats",
                    "Paper Boats",
                    &["Jonah Reed"],
                    Some("Harbour"),
                    164,
                ),
            ],
            vec![
                catalog_playlist(
                    "sp-low-glow",
                    "Low Glow",
                    &["sp-night-drive", "sp-glass-house"],
                ),
                catalog_playlist("sp-harbour", "Harbour", &["sp-paper-boats"]),
            ],
        )
    }

    pub fn youtube_music() -> Self {
        let source = Source::YouTubeMusic;
        Self::new(
            source,
            vec![
                track(
                    source,
                    "yt-night-drive-live",
                    "Night Drive (Live)",
                    &["Ada Vega"],
                    None,
                    220,
                ),
                track(
                    source,
                    "yt-kitchen-radio",
                    "Kitchen Radio",
                    &["North Line"],
                    Some("Weekday"),
                    178,
                ),
                track(
                    source,
                    "yt-paper-planes",
                    "Paper Planes",
                    &["Jonah Reed"],
                    None,
                    185,
                ),
            ],
            vec![
                catalog_playlist("yt-sessions", "Sessions", &["yt-night-drive-live"]),
                catalog_playlist(
                    "yt-weekday",
                    "Weekday",
                    &["yt-kitchen-radio", "yt-paper-planes"],
                ),
            ],
        )
    }

    fn new(source: Source, tracks: Vec<Track>, playlists: Vec<CatalogPlaylist>) -> Self {
        let service = Self {
            source,
            tracks,
            playlists,
        };
        debug_assert!(
            service.tracks.iter().all(|item| item.source == source),
            "mock track source must match the service"
        );
        debug_assert!(
            service.playlists.iter().all(|playlist| {
                playlist
                    .track_ids
                    .iter()
                    .all(|id| service.tracks.iter().any(|item| item.id == *id))
            }),
            "mock playlist references a track the service does not have"
        );
        service
    }
}

impl MusicService for MockService {
    fn search(&self, query: &str) -> Result<Vec<Track>, Error> {
        Ok(self
            .tracks
            .iter()
            .filter(|item| matches_query(item, query))
            .cloned()
            .collect())
    }

    fn resolve(&self, id: &str) -> Result<Track, Error> {
        self.tracks
            .iter()
            .find(|item| item.id == id)
            .cloned()
            .ok_or(Error::NotFound)
    }

    fn playlists(&self) -> Result<Vec<Playlist>, Error> {
        Ok(self
            .playlists
            .iter()
            .map(|playlist| Playlist {
                id: playlist.id.clone(),
                source: self.source,
                name: playlist.name.clone(),
            })
            .collect())
    }

    fn playlist_tracks(&self, playlist_id: &str) -> Result<Vec<Track>, Error> {
        let playlist = self
            .playlists
            .iter()
            .find(|playlist| playlist.id == playlist_id)
            .ok_or(Error::NotFound)?;

        playlist
            .track_ids
            .iter()
            .map(|id| self.resolve(id))
            .collect()
    }
}

fn track(
    source: Source,
    id: &str,
    title: &str,
    artists: &[&str],
    album: Option<&str>,
    duration_secs: u64,
) -> Track {
    Track {
        id: id.to_string(),
        source,
        title: title.to_string(),
        artists: artists.iter().copied().map(str::to_string).collect(),
        album: album.map(str::to_string),
        duration: Duration::from_secs(duration_secs),
    }
}

fn catalog_playlist(id: &str, name: &str, track_ids: &[&str]) -> CatalogPlaylist {
    CatalogPlaylist {
        id: id.to_string(),
        name: name.to_string(),
        track_ids: track_ids.iter().copied().map(str::to_string).collect(),
    }
}

fn matches_query(item: &Track, query: &str) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return false;
    }

    let query = query.to_lowercase();
    search_fields(item).any(|field| field.to_lowercase().contains(&query))
}

fn search_fields(item: &Track) -> impl Iterator<Item = &str> {
    std::iter::once(item.title.as_str())
        .chain(item.artists.iter().map(String::as_str))
        .chain(item.album.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn search_both(query: &str) -> Vec<Track> {
        let services = mock_services();
        search_merged(services.iter().map(|service| service.as_ref()), query).expect("mock search")
    }

    #[test]
    fn merged_search_returns_both_sources_in_service_order() {
        let results = search_both("night");
        let tagged: Vec<_> = results
            .iter()
            .map(|item| (item.source, item.title.as_str(), item.album.as_deref()))
            .collect();

        assert_eq!(
            tagged,
            [
                (Source::Spotify, "Night Drive", Some("Low Glow")),
                (Source::YouTubeMusic, "Night Drive (Live)", None),
            ]
        );
    }

    #[test]
    fn search_matches_text_fields_and_ignores_blank_queries() {
        assert!(search_both("").is_empty());
        assert!(search_both("   ").is_empty());
        assert!(search_both("sp-night-drive").is_empty());

        let harbour = search_both("HARBOUR");
        assert_eq!(
            harbour
                .iter()
                .map(|item| item.title.as_str())
                .collect::<Vec<_>>(),
            ["Paper Boats"]
        );

        let north_line = search_both("north line");
        let north_line: Vec<_> = north_line
            .iter()
            .map(|item| (item.source, item.title.as_str()))
            .collect();
        assert_eq!(
            north_line,
            [
                (Source::Spotify, "Glass House"),
                (Source::YouTubeMusic, "Kitchen Radio"),
            ]
        );
    }

    #[test]
    fn resolve_stays_on_one_service() {
        let spotify = MockService::spotify();
        let track = spotify.resolve("sp-night-drive").expect("known track");

        assert_eq!(track.source, Source::Spotify);
        assert_eq!(track.artists, ["Ada Vega"]);
        assert_eq!(track.duration, Duration::from_secs(192));
        assert_eq!(spotify.resolve("yt-kitchen-radio"), Err(Error::NotFound));
        assert_eq!(spotify.playlist_tracks("missing"), Err(Error::NotFound));
    }

    #[test]
    fn playlist_tracks_follow_stored_order() {
        let youtube = MockService::youtube_music();
        let playlists = youtube.playlists().expect("playlists");

        assert!(
            playlists
                .iter()
                .all(|playlist| playlist.source == Source::YouTubeMusic)
        );
        assert_eq!(
            playlists
                .iter()
                .map(|playlist| playlist.name.as_str())
                .collect::<Vec<_>>(),
            ["Sessions", "Weekday"]
        );

        let weekday = youtube.playlist_tracks("yt-weekday").expect("weekday");
        let ids: Vec<_> = weekday.iter().map(|item| item.id.as_str()).collect();
        assert_eq!(ids, ["yt-kitchen-radio", "yt-paper-planes"]);
        assert!(
            youtube
                .playlist_tracks("yt-weekday")
                .expect("weekday")
                .iter()
                .all(|item| item.source == Source::YouTubeMusic)
        );
    }
}
