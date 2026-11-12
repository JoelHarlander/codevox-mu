//! One search list from several catalogues.
//!
//! Each service ranks its own hits. [`merge_results`] interleaves those
//! rankings and collapses two rows when [`same_recording`] says they are the
//! same recording on different services. The kept track is the one the
//! listener can play. The other service's id stays on the row so a later
//! fallback can play it without searching again.

use std::time::Duration;

use crate::music::{Source, Track};

/// How far apart two uploads of one recording can be.
///
/// A video upload often runs a couple of seconds longer than the album
/// track. A live cut or another edit sits further away than this.
const DURATION_TOLERANCE: Duration = Duration::from_secs(3);

/// Trailing upload labels stripped from a title, longest first.
///
/// Live, acoustic, and remix stay. Those name a different recording, not
/// another upload of the same one.
const TITLE_SUFFIXES: &[&[&str]] = &[
    &["official", "music", "video"],
    &["official", "lyric", "video"],
    &["official", "lyrics", "video"],
    &["official", "audio"],
    &["official", "video"],
    &["official", "lyrics"],
    &["lyric", "video"],
    &["lyrics", "video"],
    &["music", "video"],
    &["audio"],
    &["lyrics"],
];

/// One row in a merged search list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchRow {
    /// The track to show and to play.
    pub track: Track,
    /// Ids for this recording on the services collapsed into [`Self::track`].
    ///
    /// Earliest in the interleaved order first. Empty when this is the only
    /// copy.
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "a later playback fallback reads the other service's id"
        )
    )]
    pub also: Vec<AlternateId>,
}

/// A source and the id that source uses for a recording.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlternateId {
    pub source: Source,
    pub id: String,
}

/// Whether `left` and `right` are the same recording on two services.
///
/// A free Spotify account plays the matched YouTube Music track, so playback
/// can call this on a Spotify track and a YouTube candidate without going
/// through [`merge_results`].
///
/// The tracks match when they come from different sources, the normalised
/// titles are equal, the normalised primary artists are equal, and the
/// durations differ by at most three seconds. The primary artist is the
/// first named artist. Normalising folds case, drops apostrophes, and treats
/// other punctuation as a word break. A title also loses a trailing upload
/// label: official or lyric video, audio, lyrics, or a remaster mark
/// (`Remastered`, `Remaster`, with an optional four-digit year).
///
/// The same source never matches, so one service's own ranking stays intact.
/// Live, acoustic, and remix titles stay distinct. A featuring credit left
/// on only one title also stays distinct: that clause can name a different
/// mix, and it often sits in front of one of those words.
pub fn same_recording(left: &Track, right: &Track) -> bool {
    if left.source == right.source {
        return false;
    }

    let title = normalize_title(&left.title);
    if title.is_empty() || title != normalize_title(&right.title) {
        return false;
    }

    let artist = normalize_artist(primary_artist(&left.artists));
    if artist.is_empty() || artist != normalize_artist(primary_artist(&right.artists)) {
        return false;
    }

    left.duration.abs_diff(right.duration) <= DURATION_TOLERANCE
}

/// Interleave `lists` by rank and collapse the same recording.
///
/// `lists` is one service per entry, each already in that service's order.
/// Rank 0 of every service comes out before rank 1, and the order inside
/// `lists` is the order within a rank. A hit that [`same_recording`] matches
/// to an earlier row joins that row instead of taking a new place.
///
/// [`SearchRow::track`] is the copy whose source is `playable`, when either
/// side is. The other id is appended to [`SearchRow::also`]. Two tracks from
/// the same source are never collapsed.
pub fn merge_results(lists: &[&[Track]], playable: Source) -> Vec<SearchRow> {
    let mut rows = Vec::new();
    let ranks = lists.iter().map(|list| list.len()).max().unwrap_or(0);
    for rank in 0..ranks {
        for list in lists {
            if let Some(track) = list.get(rank) {
                place(&mut rows, track.clone(), playable);
            }
        }
    }
    rows
}

fn place(rows: &mut Vec<SearchRow>, track: Track, playable: Source) {
    if let Some(row) = rows.iter_mut().find(|row| row_matches(row, &track)) {
        absorb(row, track, playable);
        return;
    }

    rows.push(SearchRow {
        track,
        also: Vec::new(),
    });
}

fn row_matches(row: &SearchRow, track: &Track) -> bool {
    if row.track.source == track.source || row.also.iter().any(|other| other.source == track.source)
    {
        return false;
    }
    same_recording(&row.track, track)
}

fn absorb(row: &mut SearchRow, incoming: Track, playable: Source) {
    let incoming_plays = incoming.source == playable && row.track.source != playable;
    let displaced = if incoming_plays {
        std::mem::replace(&mut row.track, incoming)
    } else {
        incoming
    };
    row.also.push(AlternateId {
        source: displaced.source,
        id: displaced.id,
    });
}

fn normalize_title(title: &str) -> String {
    let mut words = fold_words(title);
    while let Some(stripped) = strip_title_suffix(&words) {
        words = stripped;
    }
    words.join(" ")
}

fn normalize_artist(name: &str) -> String {
    fold_words(name).join(" ")
}

fn primary_artist(artists: &[String]) -> &str {
    artists
        .iter()
        .find(|name| !name.trim().is_empty())
        .map(String::as_str)
        .unwrap_or("")
}

fn strip_title_suffix(words: &[String]) -> Option<Vec<String>> {
    for phrase in TITLE_SUFFIXES {
        if let Some(rest) = strip_phrase(words, phrase) {
            return Some(rest);
        }
    }
    strip_remaster(words)
}

fn strip_phrase(words: &[String], phrase: &[&str]) -> Option<Vec<String>> {
    if phrase.is_empty() || words.len() <= phrase.len() {
        return None;
    }
    let start = words.len() - phrase.len();
    let matches = words[start..]
        .iter()
        .zip(phrase)
        .all(|(word, expected)| word == expected);
    if matches {
        Some(words[..start].to_vec())
    } else {
        None
    }
}

fn strip_remaster(words: &[String]) -> Option<Vec<String>> {
    let count = words.len();
    if count >= 3 && is_year(&words[count - 1]) && is_remaster(&words[count - 2]) {
        return Some(words[..count - 2].to_vec());
    }
    if count >= 3 && is_remaster(&words[count - 1]) && is_year(&words[count - 2]) {
        return Some(words[..count - 2].to_vec());
    }
    if count >= 2 && is_remaster(&words[count - 1]) {
        return Some(words[..count - 1].to_vec());
    }
    None
}

fn is_remaster(word: &str) -> bool {
    word == "remaster" || word == "remastered"
}

fn is_year(word: &str) -> bool {
    word.len() == 4 && word.bytes().all(|byte| byte.is_ascii_digit())
}

fn fold_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if is_apostrophe(ch) {
            continue;
        }
        if ch.is_alphanumeric() {
            current.extend(ch.to_lowercase());
            continue;
        }
        if !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn is_apostrophe(ch: char) -> bool {
    matches!(ch, '\'' | '\u{2019}' | '\u{2018}' | '\u{02BC}')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(source: Source, id: &str, title: &str, artist: &str, seconds: u64) -> Track {
        Track {
            id: id.to_string(),
            source,
            title: title.to_string(),
            artists: vec![artist.to_string()],
            album: None,
            duration: Duration::from_secs(seconds),
        }
    }

    fn ids(rows: &[SearchRow]) -> Vec<&str> {
        rows.iter().map(|row| row.track.id.as_str()).collect()
    }

    #[test]
    fn upload_labels_match_the_album_track() {
        let album = track(Source::Spotify, "sp", "Night Drive", "Ada Vega", 192);
        let titles = [
            "Night Drive (Official Video)",
            "night drive [Official Music Video]",
            "Night Drive (Audio)",
            "Night Drive (Official Audio)",
            "Night Drive - Remastered 2011",
            "Night Drive (Remastered)",
            "Night Drive (2011 Remaster)",
            "NIGHT DRIVE (Official Video) - Remastered 2011",
        ];

        for title in titles {
            let upload = track(Source::YouTubeMusic, "yt", title, "Ada Vega", 194);
            assert!(same_recording(&album, &upload), "{title}");
            assert!(same_recording(&upload, &album), "{title}");
        }
    }

    #[test]
    fn case_and_punctuation_fold_for_the_title_and_the_primary_artist() {
        let spotify = track(Source::Spotify, "sp", "Don't Stop!", "Guns N' Roses", 250);
        let youtube = track(Source::YouTubeMusic, "yt", "dont stop", "guns n roses", 250);
        assert!(same_recording(&spotify, &youtube));
    }

    #[test]
    fn only_the_primary_artist_has_to_match() {
        let spotify = track(Source::Spotify, "sp", "Glass House", "Ada Vega", 241);
        let mut youtube = track(Source::YouTubeMusic, "yt", "Glass House", "Ada Vega", 241);
        youtube.artists.push("North Line".to_string());
        assert!(same_recording(&spotify, &youtube));

        youtube.artists = vec!["North Line".to_string(), "Ada Vega".to_string()];
        assert!(!same_recording(&spotify, &youtube));
    }

    #[test]
    fn a_different_recording_stays_separate() {
        let album = track(Source::Spotify, "sp", "Night Drive", "Ada Vega", 192);
        let others = [
            track(
                Source::YouTubeMusic,
                "live",
                "Night Drive (Live)",
                "Ada Vega",
                192,
            ),
            track(
                Source::YouTubeMusic,
                "acoustic",
                "Night Drive (Acoustic)",
                "Ada Vega",
                190,
            ),
            track(
                Source::YouTubeMusic,
                "remix",
                "Night Drive (Remix)",
                "Ada Vega",
                192,
            ),
            track(
                Source::YouTubeMusic,
                "cover",
                "Night Drive",
                "Jonah Reed",
                192,
            ),
            track(
                Source::YouTubeMusic,
                "featured",
                "Night Drive (feat. North Line)",
                "Ada Vega",
                192,
            ),
            track(Source::YouTubeMusic, "long", "Night Drive", "Ada Vega", 196),
        ];

        for other in &others {
            assert!(!same_recording(&album, other), "{}", other.id);
        }
    }

    #[test]
    fn three_seconds_is_still_the_same_recording() {
        let album = track(Source::Spotify, "sp", "Paper Boats", "Jonah Reed", 180);
        let mut close = track(Source::YouTubeMusic, "yt", "Paper Boats", "Jonah Reed", 180);
        close.duration = album.duration + DURATION_TOLERANCE;
        assert!(same_recording(&album, &close));

        close.duration += Duration::from_millis(1);
        assert!(!same_recording(&album, &close));
    }

    #[test]
    fn the_same_source_is_never_one_recording() {
        let album = track(Source::Spotify, "sp-a", "Night Drive", "Ada Vega", 192);
        let copy = track(Source::Spotify, "sp-b", "Night Drive", "Ada Vega", 192);
        assert!(!same_recording(&album, &copy));

        let first = vec![album];
        let second = vec![copy];
        let rows = merge_results(&[&first, &second], Source::YouTubeMusic);
        assert_eq!(ids(&rows), ["sp-a", "sp-b"]);
        assert!(rows.iter().all(|row| row.also.is_empty()));
    }

    #[test]
    fn rows_interleave_by_rank_and_a_duplicate_keeps_the_earlier_place() {
        let spotify = vec![
            track(Source::Spotify, "sp-alpha", "Alpha", "Ada Vega", 100),
            track(Source::Spotify, "sp-beta", "Beta", "Ada Vega", 110),
            track(Source::Spotify, "sp-gamma", "Gamma", "Ada Vega", 120),
        ];
        let youtube = vec![
            track(Source::YouTubeMusic, "yt-delta", "Delta", "Ada Vega", 130),
            track(
                Source::YouTubeMusic,
                "yt-beta",
                "Beta (Official Video)",
                "Ada Vega",
                111,
            ),
            track(
                Source::YouTubeMusic,
                "yt-epsilon",
                "Epsilon",
                "Ada Vega",
                140,
            ),
        ];

        let rows = merge_results(&[&spotify, &youtube], Source::YouTubeMusic);
        assert_eq!(
            ids(&rows),
            ["sp-alpha", "yt-delta", "yt-beta", "sp-gamma", "yt-epsilon"]
        );
        assert_eq!(rows[2].track.source, Source::YouTubeMusic);
        assert_eq!(rows[2].track.title, "Beta (Official Video)");
        assert_eq!(
            rows[2].also,
            [AlternateId {
                source: Source::Spotify,
                id: "sp-beta".to_string(),
            }]
        );
        assert!(rows[0].also.is_empty());
    }

    #[test]
    fn the_playable_copy_replaces_an_earlier_duplicate_in_place() {
        let spotify = vec![
            track(Source::Spotify, "sp-night", "Night Drive", "Ada Vega", 192),
            track(
                Source::Spotify,
                "sp-later",
                "Paper Boats",
                "Jonah Reed",
                164,
            ),
        ];
        let youtube = vec![
            track(
                Source::YouTubeMusic,
                "yt-other",
                "Kitchen Radio",
                "North Line",
                178,
            ),
            track(
                Source::YouTubeMusic,
                "yt-night",
                "Night Drive (Official Video)",
                "Ada Vega",
                193,
            ),
        ];

        let rows = merge_results(&[&spotify, &youtube], Source::YouTubeMusic);
        assert_eq!(ids(&rows), ["yt-night", "yt-other", "sp-later"]);
        assert_eq!(rows[0].track.source, Source::YouTubeMusic);
        assert_eq!(
            rows[0].also,
            [AlternateId {
                source: Source::Spotify,
                id: "sp-night".to_string(),
            }]
        );
    }

    #[test]
    fn a_playable_row_keeps_its_place_when_it_ranked_first() {
        let youtube = vec![track(
            Source::YouTubeMusic,
            "yt-night",
            "Night Drive (Audio)",
            "Ada Vega",
            192,
        )];
        let spotify = vec![track(
            Source::Spotify,
            "sp-night",
            "Night Drive",
            "Ada Vega",
            190,
        )];

        let rows = merge_results(&[&youtube, &spotify], Source::YouTubeMusic);
        assert_eq!(ids(&rows), ["yt-night"]);
        assert_eq!(rows[0].track.title, "Night Drive (Audio)");
        assert_eq!(rows[0].also[0].source, Source::Spotify);
        assert_eq!(rows[0].also[0].id, "sp-night");
    }

    #[test]
    fn an_empty_service_leaves_the_other_order_alone() {
        let spotify = vec![
            track(Source::Spotify, "sp-a", "Alpha", "Ada Vega", 100),
            track(Source::Spotify, "sp-b", "Beta", "Ada Vega", 110),
        ];
        let youtube: Vec<Track> = Vec::new();

        assert!(merge_results(&[], Source::YouTubeMusic).is_empty());
        let rows = merge_results(&[&spotify, &youtube], Source::YouTubeMusic);
        assert_eq!(ids(&rows), ["sp-a", "sp-b"]);
        assert!(rows.iter().all(|row| row.also.is_empty()));
    }
}
