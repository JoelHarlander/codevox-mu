//! Mixed playlists saved on this device.
//!
//! A playlist is a named list of [`Track`] values. Spotify, YouTube Music,
//! and local tracks share that type, so an import stores the same rows the
//! library already shows. The list is a JSON file in the per-user data
//! directory (the same folder as the log), written by replacing a temporary
//! file so a crash cannot leave a half-written list.
//!
//! Importing the same Spotify or YouTube Music playlist again replaces that
//! saved list's tracks and keeps its id and name. A playlist made in this
//! app has no source, so it is left alone.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::music::{Source, Track};

/// JSON `version` this build writes.
///
/// Version 1 is the same file without an import source. It still loads.
const SCHEMA_VERSION: u32 = 2;

/// Remote playlist ids longer than this are refused.
const ORIGIN_ID_LIMIT: usize = 128;

const FILE_NAME: &str = "playlists.json";

/// Trimmed playlist names longer than this are refused.
const NAME_LIMIT: usize = 80;

/// Where an imported playlist came from.
///
/// The id is the service's playlist id, including Liked Songs. It is not
/// the id of the saved list on this device.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PlaylistOrigin {
    source: Source,
    playlist_id: String,
}

/// One saved playlist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedPlaylist {
    id: String,
    name: String,
    tracks: Vec<Track>,
    origin: Option<PlaylistOrigin>,
}

/// [`Store::import_from`] created a list or replaced the tracks of one
/// that already came from that service playlist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportOutcome {
    Created { id: String },
    Updated { id: String },
}

impl SavedPlaylist {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }
}

/// The playlists file, or an in-memory list when nothing is saved yet.
#[derive(Debug)]
pub struct Store {
    path: Option<PathBuf>,
    playlists: Vec<SavedPlaylist>,
    /// Set when the file on disk must not be replaced.
    ///
    /// A newer schema, or a file this build cannot read, stays as it was.
    /// Mutations fail with this sentence instead of writing.
    blocked: Option<String>,
}

/// Why [`Store::load`] did not return a writable store.
#[derive(Debug)]
pub struct LoadError {
    path: Option<PathBuf>,
    message: String,
}

impl LoadError {
    pub fn message(&self) -> &str {
        &self.message
    }

    /// A store that keeps the file and refuses to write it.
    pub fn into_store(self) -> Store {
        Store {
            path: self.path,
            playlists: Vec::new(),
            blocked: Some(self.message),
        }
    }
}

impl Store {
    /// No file. Mutations stay in memory. Smoke and tests use this.
    pub fn memory() -> Self {
        Self {
            path: None,
            playlists: Vec::new(),
            blocked: None,
        }
    }

    /// Load the playlists file from the per-user data directory.
    pub fn load_default() -> Result<Self, LoadError> {
        let Some(directory) = crate::diag::data_dir() else {
            return Err(LoadError {
                path: None,
                message: "Could not find a folder for saved playlists.".to_string(),
            });
        };
        Self::load(directory.join(FILE_NAME))
    }

    /// Load `path`. A missing file is an empty store that will create it on save.
    pub fn load(path: PathBuf) -> Result<Self, LoadError> {
        if !path.exists() {
            return Ok(Self {
                path: Some(path),
                playlists: Vec::new(),
                blocked: None,
            });
        }
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                return Err(LoadError {
                    path: Some(path),
                    message: format!(
                        "Could not read saved playlists ({error}), so the file was left unchanged."
                    ),
                });
            }
        };
        let body: FileBody = match serde_json::from_slice(&bytes) {
            Ok(body) => body,
            Err(error) => {
                return Err(LoadError {
                    path: Some(path),
                    message: format!(
                        "Saved playlists could not be read ({}), so the file was left unchanged.",
                        brief(&error.to_string())
                    ),
                });
            }
        };
        if body.version != 1 && body.version != SCHEMA_VERSION {
            return Err(LoadError {
                path: Some(path),
                message: format!(
                    "Saved playlists are version {}. This app reads version {SCHEMA_VERSION}, so the file was left unchanged.",
                    body.version
                ),
            });
        }
        let mut playlists = Vec::with_capacity(body.playlists.len());
        for playlist in body.playlists {
            playlists.push(match playlist.into_saved() {
                Ok(playlist) => playlist,
                Err(message) => {
                    return Err(LoadError {
                        path: Some(path),
                        message,
                    });
                }
            });
        }
        if let Err(message) = validate_ids(&playlists).and_then(|()| validate_origins(&playlists)) {
            return Err(LoadError {
                path: Some(path),
                message,
            });
        }
        Ok(Self {
            path: Some(path),
            playlists,
            blocked: None,
        })
    }

    pub fn playlists(&self) -> &[SavedPlaylist] {
        &self.playlists
    }

    pub fn playlist(&self, id: &str) -> Option<&SavedPlaylist> {
        self.playlists.iter().find(|playlist| playlist.id == id)
    }

    /// Create a playlist and save. The returned id is stable across restarts.
    pub fn create(&mut self, name: &str) -> Result<String, String> {
        self.ensure_writable()?;
        let name = clean_name(name)?;
        let snapshot = self.playlists.clone();
        let id = new_id();
        self.playlists.push(SavedPlaylist {
            id: id.clone(),
            name,
            tracks: Vec::new(),
            origin: None,
        });
        self.save_restoring(snapshot)?;
        Ok(id)
    }

    /// Save `tracks` as the mixed playlist imported from `source` / `playlist_id`.
    ///
    /// The first import uses `name`. A later import of that same service
    /// playlist replaces the tracks and keeps the id and the name, including
    /// a rename made here. Liked Songs on Spotify and liked songs on YouTube
    /// Music are different playlists even though both ids are `liked-songs`.
    pub fn import_from(
        &mut self,
        source: Source,
        playlist_id: &str,
        name: &str,
        tracks: Vec<Track>,
    ) -> Result<ImportOutcome, String> {
        self.ensure_writable()?;
        let origin = clean_origin(source, playlist_id)?;
        if let Some(index) = self
            .playlists
            .iter()
            .position(|playlist| playlist.origin.as_ref() == Some(&origin))
        {
            let snapshot = self.playlists.clone();
            let id = self.playlists[index].id.clone();
            self.playlists[index].tracks = tracks;
            self.save_restoring(snapshot)?;
            return Ok(ImportOutcome::Updated { id });
        }
        let name = import_name(name);
        let snapshot = self.playlists.clone();
        let id = new_id();
        self.playlists.push(SavedPlaylist {
            id: id.clone(),
            name,
            tracks,
            origin: Some(origin),
        });
        self.save_restoring(snapshot)?;
        Ok(ImportOutcome::Created { id })
    }

    pub fn rename(&mut self, id: &str, name: &str) -> Result<(), String> {
        self.ensure_writable()?;
        let name = clean_name(name)?;
        let index = self.index_of(id)?;
        let snapshot = self.playlists.clone();
        self.playlists[index].name = name;
        self.save_restoring(snapshot)
    }

    pub fn delete(&mut self, id: &str) -> Result<(), String> {
        self.ensure_writable()?;
        let index = self.index_of(id)?;
        let snapshot = self.playlists.clone();
        self.playlists.remove(index);
        self.save_restoring(snapshot)
    }

    /// Append `track`. The listener uses this to add one row.
    pub fn add_track(&mut self, id: &str, track: Track) -> Result<(), String> {
        self.ensure_writable()?;
        let index = self.index_of(id)?;
        let snapshot = self.playlists.clone();
        self.playlists[index].tracks.push(track);
        self.save_restoring(snapshot)
    }

    pub fn remove_track(&mut self, id: &str, index: usize) -> Result<(), String> {
        self.ensure_writable()?;
        let playlist = self.index_of(id)?;
        if index >= self.playlists[playlist].tracks.len() {
            return Err("That track is not in the playlist.".to_string());
        }
        let snapshot = self.playlists.clone();
        self.playlists[playlist].tracks.remove(index);
        self.save_restoring(snapshot)
    }

    /// Swap the track at `index` with the one before it.
    ///
    /// The first track and an index past the end stay put.
    pub fn move_earlier(&mut self, id: &str, index: usize) -> Result<(), String> {
        self.swap(id, index, index.wrapping_sub(1), index == 0)
    }

    /// Swap the track at `index` with the one after it.
    ///
    /// The last track and an index past the end stay put.
    pub fn move_later(&mut self, id: &str, index: usize) -> Result<(), String> {
        let Some(playlist) = self.playlist(id) else {
            return Err("That playlist is not saved.".to_string());
        };
        let last = index >= playlist.tracks().len().saturating_sub(1);
        self.swap(id, index, index.saturating_add(1), last)
    }

    fn swap(&mut self, id: &str, index: usize, other: usize, hold: bool) -> Result<(), String> {
        self.ensure_writable()?;
        let playlist = self.index_of(id)?;
        let length = self.playlists[playlist].tracks.len();
        if hold || index >= length || other >= length {
            return Ok(());
        }
        let snapshot = self.playlists.clone();
        self.playlists[playlist].tracks.swap(index, other);
        self.save_restoring(snapshot)
    }

    fn index_of(&self, id: &str) -> Result<usize, String> {
        self.playlists
            .iter()
            .position(|playlist| playlist.id == id)
            .ok_or_else(|| "That playlist is not saved.".to_string())
    }

    fn ensure_writable(&self) -> Result<(), String> {
        if let Some(reason) = &self.blocked {
            return Err(reason.clone());
        }
        Ok(())
    }

    fn save_restoring(&mut self, snapshot: Vec<SavedPlaylist>) -> Result<(), String> {
        match self.save() {
            Ok(()) => Ok(()),
            Err(error) => {
                self.playlists = snapshot;
                Err(error)
            }
        }
    }

    fn save(&self) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let mut stored = Vec::with_capacity(self.playlists.len());
        for playlist in &self.playlists {
            stored.push(StoredPlaylist::from_saved(playlist)?);
        }
        let body = FileBody {
            version: SCHEMA_VERSION,
            playlists: stored,
        };
        let mut bytes = serde_json::to_vec_pretty(&body)
            .map_err(|_| "Could not save playlists.".to_string())?;
        bytes.push(b'\n');
        write_atomic(path, &bytes)
    }
}

fn validate_origins(playlists: &[SavedPlaylist]) -> Result<(), String> {
    let mut seen = HashSet::new();
    for playlist in playlists {
        let Some(origin) = &playlist.origin else {
            continue;
        };
        let key = format!("{}:{}", source_key(origin.source), origin.playlist_id);
        if !seen.insert(key) {
            return Err(
                "Saved playlists could not be read, so the file was left unchanged.".to_string(),
            );
        }
    }
    Ok(())
}

fn source_key(source: Source) -> &'static str {
    match source {
        Source::Spotify => "spotify",
        Source::YouTubeMusic => "youtube_music",
        Source::Local => "local",
    }
}

fn validate_ids(playlists: &[SavedPlaylist]) -> Result<(), String> {
    let mut seen = HashSet::new();
    for playlist in playlists {
        if playlist.id.is_empty() || !seen.insert(playlist.id.clone()) {
            return Err(
                "Saved playlists could not be read, so the file was left unchanged.".to_string(),
            );
        }
    }
    Ok(())
}

fn clean_name(name: &str) -> Result<String, String> {
    let name = collapsed(name);
    if name.is_empty() {
        return Err("Name the playlist first.".to_string());
    }
    if name.chars().count() > NAME_LIMIT {
        return Err("Use a shorter playlist name.".to_string());
    }
    Ok(name)
}

/// A service title, shortened to the name limit. A blank title gets a stand-in
/// so a long YouTube Music name can still be imported.
fn import_name(name: &str) -> String {
    let name = collapsed(name);
    let name: String = if name.is_empty() {
        "Untitled playlist".to_string()
    } else {
        name.chars().take(NAME_LIMIT).collect()
    };
    let name = collapsed(&name);
    if name.is_empty() {
        "Untitled playlist".to_string()
    } else {
        name
    }
}

fn collapsed(name: &str) -> String {
    name.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clean_origin(source: Source, playlist_id: &str) -> Result<PlaylistOrigin, String> {
    if !matches!(source, Source::Spotify | Source::YouTubeMusic) {
        return Err("That playlist cannot be imported.".to_string());
    }
    let playlist_id = playlist_id.trim();
    if playlist_id.is_empty()
        || playlist_id.chars().count() > ORIGIN_ID_LIMIT
        || playlist_id.chars().any(char::is_control)
    {
        return Err("That playlist cannot be imported.".to_string());
    }
    Ok(PlaylistOrigin {
        source,
        playlist_id: playlist_id.to_string(),
    })
}

fn new_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}-{count:x}")
}

fn brief(text: &str) -> String {
    let mut brief: String = text.chars().take(120).collect();
    if text.chars().count() > 120 {
        brief.push('…');
    }
    brief
}

fn duration_ms(duration: Duration) -> Result<u64, String> {
    u64::try_from(duration.as_millis()).map_err(|_| "That track is too long to save.".to_string())
}

fn duration_from_ms(milliseconds: u64) -> Duration {
    Duration::new(
        milliseconds / 1000,
        ((milliseconds % 1000) * 1_000_000) as u32,
    )
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(save_error)?;
    }
    let temporary = temporary_path(path);
    let write_result = write_then_replace(&temporary, path, bytes);
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result
}

fn write_then_replace(temporary: &Path, path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(temporary)
        .map_err(save_error)?;
    file.write_all(bytes).map_err(save_error)?;
    file.sync_all().map_err(save_error)?;
    replace_file(temporary, path).map_err(save_error)?;
    sync_parent(path);
    Ok(())
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .unwrap_or(std::ffi::OsStr::new(FILE_NAME))
        .to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

fn save_error(error: std::io::Error) -> String {
    format!("Could not save playlists: {error}")
}

/// The file is fully written and flushed before the name changes, so readers
/// see the previous list or the new one, not a partial write.
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        replace_file_windows(from, to)
    }
    #[cfg(not(windows))]
    {
        fs::rename(from, to)
    }
}

#[cfg(windows)]
fn replace_file_windows(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let from_wide = wide(from);
    let to_wide = wide(to);
    // SAFETY: both pointers are NUL-terminated paths that live until the
    // call returns. REPLACE_EXISTING swaps the name onto the completed temp
    // file. WRITE_THROUGH asks the filesystem to flush that metadata.
    let replaced = unsafe {
        MoveFileExW(
            from_wide.as_ptr(),
            to_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if replaced == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn sync_parent(path: &Path) {
    let Some(parent) = path.parent() else {
        return;
    };
    if parent.as_os_str().is_empty() {
        return;
    }
    if let Ok(directory) = File::open(parent) {
        let _ = directory.sync_all();
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileBody {
    version: u32,
    playlists: Vec<StoredPlaylist>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredPlaylist {
    id: String,
    name: String,
    tracks: Vec<StoredTrack>,
    /// Absent on playlists made in this app, and on version 1 files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin: Option<StoredOrigin>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredOrigin {
    source: StoredSource,
    playlist_id: String,
}

impl StoredPlaylist {
    fn from_saved(playlist: &SavedPlaylist) -> Result<Self, String> {
        let mut tracks = Vec::with_capacity(playlist.tracks.len());
        for track in &playlist.tracks {
            tracks.push(StoredTrack::from_track(track)?);
        }
        Ok(Self {
            id: playlist.id.clone(),
            name: playlist.name.clone(),
            tracks,
            origin: playlist.origin.clone().map(StoredOrigin::from_origin),
        })
    }

    fn into_saved(self) -> Result<SavedPlaylist, String> {
        if self.id.is_empty() {
            return Err(
                "Saved playlists could not be read, so the file was left unchanged.".to_string(),
            );
        }
        let mut tracks = Vec::with_capacity(self.tracks.len());
        for track in self.tracks {
            tracks.push(track.into_track());
        }
        let origin = match self.origin {
            Some(origin) => Some(origin.into_origin()?),
            None => None,
        };
        Ok(SavedPlaylist {
            id: self.id,
            name: self.name,
            tracks,
            origin,
        })
    }
}

impl StoredOrigin {
    fn from_origin(origin: PlaylistOrigin) -> Self {
        Self {
            source: StoredSource::from(origin.source),
            playlist_id: origin.playlist_id,
        }
    }

    fn into_origin(self) -> Result<PlaylistOrigin, String> {
        let source = match self.source {
            StoredSource::Spotify => Source::Spotify,
            StoredSource::YoutubeMusic => Source::YouTubeMusic,
            StoredSource::Local => {
                return Err(
                    "Saved playlists could not be read, so the file was left unchanged."
                        .to_string(),
                );
            }
        };
        clean_origin(source, &self.playlist_id)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredTrack {
    id: String,
    source: StoredSource,
    title: String,
    artists: Vec<String>,
    album: Option<String>,
    duration_ms: u64,
}

impl StoredTrack {
    fn from_track(track: &Track) -> Result<Self, String> {
        Ok(Self {
            id: track.id.clone(),
            source: StoredSource::from(track.source),
            title: track.title.clone(),
            artists: track.artists.clone(),
            album: track.album.clone(),
            duration_ms: duration_ms(track.duration)?,
        })
    }

    fn into_track(self) -> Track {
        Track {
            id: self.id,
            source: Source::from(self.source),
            title: self.title,
            artists: self.artists,
            album: self.album,
            duration: duration_from_ms(self.duration_ms),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum StoredSource {
    Spotify,
    YoutubeMusic,
    Local,
}

impl From<Source> for StoredSource {
    fn from(source: Source) -> Self {
        match source {
            Source::Spotify => Self::Spotify,
            Source::YouTubeMusic => Self::YoutubeMusic,
            Source::Local => Self::Local,
        }
    }
}

impl From<StoredSource> for Source {
    fn from(source: StoredSource) -> Self {
        match source {
            StoredSource::Spotify => Source::Spotify,
            StoredSource::YoutubeMusic => Source::YouTubeMusic,
            StoredSource::Local => Source::Local,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "codevox-mu-playlists-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn track(source: Source, id: &str, album: Option<&str>) -> Track {
        Track {
            id: id.to_string(),
            source,
            title: id.to_string(),
            artists: vec!["Ada Vega".to_string(), "North Line".to_string()],
            album: album.map(str::to_string),
            duration: Duration::from_secs(192),
        }
    }

    fn ids(store: &Store, playlist: &str) -> Vec<String> {
        store
            .playlist(playlist)
            .expect("playlist")
            .tracks()
            .iter()
            .map(|item| item.id.clone())
            .collect()
    }

    #[test]
    fn save_and_load_round_trip_keeps_mixed_tracks() {
        let dir = temp_dir("round");
        let path = dir.join(FILE_NAME);
        let mut store = Store::load(path.clone()).expect("missing file is empty");
        assert!(store.create("   ").is_err());
        assert!(!path.exists());

        let id = store.create("  Night   Drive ").expect("create");
        store.rename(&id, "Late Drive").expect("rename");
        store
            .add_track(&id, track(Source::Spotify, "sp-1", Some("Low Glow")))
            .expect("spotify");
        store
            .add_track(&id, track(Source::YouTubeMusic, "yt-1", None))
            .expect("youtube");
        store
            .add_track(&id, track(Source::Local, "local-1", Some("Desk")))
            .expect("local");

        let text = fs::read_to_string(&path).expect("file");
        assert!(text.contains("\"version\": 2"), "{text}");
        assert!(text.contains("\"spotify\""), "{text}");
        assert!(text.contains("\"youtube_music\""), "{text}");
        assert!(text.contains("\"local\""), "{text}");
        assert!(text.contains("\"duration_ms\": 192000"), "{text}");
        assert!(!dir.join("playlists.json.tmp").exists());

        let loaded = Store::load(path).expect("reload");
        let playlist = loaded.playlist(&id).expect("saved");
        assert_eq!(playlist.name(), "Late Drive");
        assert_eq!(playlist.tracks().len(), 3);
        assert_eq!(playlist.tracks()[0].source, Source::Spotify);
        assert_eq!(playlist.tracks()[0].album.as_deref(), Some("Low Glow"));
        assert_eq!(playlist.tracks()[0].artists, ["Ada Vega", "North Line"]);
        assert_eq!(playlist.tracks()[0].duration, Duration::from_secs(192));
        assert_eq!(playlist.tracks()[1].source, Source::YouTubeMusic);
        assert_eq!(playlist.tracks()[1].album, None);
        assert_eq!(playlist.tracks()[2].source, Source::Local);
        assert_eq!(playlist.tracks()[2].id, "local-1");

        let mut again = loaded;
        again.delete(&id).expect("delete");
        let empty = Store::load(dir.join(FILE_NAME)).expect("empty file");
        assert!(empty.playlists().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reorder_survives_reload() {
        let dir = temp_dir("reorder");
        let path = dir.join(FILE_NAME);
        let mut store = Store::load(path.clone()).expect("load");
        let id = store.create("Night").expect("create");
        for source_id in ["a", "b", "c"] {
            store
                .add_track(&id, track(Source::Spotify, source_id, None))
                .expect("add");
        }
        assert!(store.move_earlier(&id, 0).is_ok());
        assert_eq!(ids(&store, &id), ["a", "b", "c"]);
        store.move_later(&id, 0).expect("later");
        store.move_earlier(&id, 2).expect("earlier");
        assert_eq!(ids(&store, &id), ["b", "c", "a"]);

        let mut loaded = Store::load(path).expect("reload");
        assert_eq!(ids(&loaded, &id), ["b", "c", "a"]);
        assert!(loaded.move_later(&id, 9).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_survives_reload() {
        let dir = temp_dir("remove");
        let path = dir.join(FILE_NAME);
        let mut store = Store::load(path.clone()).expect("load");
        let id = store.create("Night").expect("create");
        store
            .add_track(&id, track(Source::YouTubeMusic, "yt", None))
            .expect("yt");
        store
            .add_track(&id, track(Source::Spotify, "sp", None))
            .expect("sp");
        assert!(store.remove_track(&id, 4).is_err());
        store.remove_track(&id, 0).expect("remove");

        let loaded = Store::load(path).expect("reload");
        assert_eq!(ids(&loaded, &id), ["sp"]);
        assert_eq!(
            loaded.playlist(&id).expect("playlist").tracks()[0].source,
            Source::Spotify
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_schema_this_app_does_not_read_is_left_unchanged() {
        let dir = temp_dir("schema");
        let newer = "{\n  \"version\": 3,\n  \"playlists\": []\n}\n";
        let path = dir.join(FILE_NAME);
        fs::write(&path, newer).expect("write");
        let error = Store::load(path.clone()).expect_err("newer version");
        assert!(error.message().contains("version 3"), "{}", error.message());
        assert!(error.message().contains("left unchanged"));
        let mut refused = error.into_store();
        let save_error = refused.create("Nope").expect_err("refused");
        assert!(save_error.contains("left unchanged"), "{save_error}");
        assert_eq!(fs::read(&path).expect("bytes"), newer.as_bytes());

        let extra = "{\"version\":1,\"playlists\":[],\"extra\":true}\n";
        let extra_path = dir.join("extra.json");
        fs::write(&extra_path, extra).expect("write extra");
        let error = Store::load(extra_path.clone()).expect_err("unknown field");
        assert!(error.message().contains("left unchanged"));
        assert_eq!(fs::read(&extra_path).expect("bytes"), extra.as_bytes());

        let unknown_source = r#"{
  "version": 1,
  "playlists": [{
    "id": "p1",
    "name": "Imported",
    "tracks": [{
      "id": "x",
      "source": "bandcamp",
      "title": "Later",
      "artists": [],
      "album": null,
      "duration_ms": 1000
    }]
  }]
}
"#;
        let source_path = dir.join("source.json");
        fs::write(&source_path, unknown_source).expect("write source");
        assert!(Store::load(source_path.clone()).is_err());
        assert_eq!(
            fs::read(&source_path).expect("bytes"),
            unknown_source.as_bytes()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_version_one_file_still_loads_and_import_writes_version_two() {
        let dir = temp_dir("v1");
        let path = dir.join(FILE_NAME);
        let older = r#"{
  "version": 1,
  "playlists": [{
    "id": "p1",
    "name": "Night",
    "tracks": []
  }]
}
"#;
        fs::write(&path, older).expect("write");
        let mut store = Store::load(path.clone()).expect("version 1");
        assert_eq!(store.playlist("p1").expect("old").name(), "Night");
        assert!(store.playlist("p1").expect("old").origin.is_none());
        store
            .import_from(Source::Spotify, "liked-songs", "Liked Songs", Vec::new())
            .expect("import");
        let text = fs::read_to_string(&path).expect("rewritten");
        assert!(text.contains("\"version\": 2"), "{text}");
        assert!(text.contains("\"id\": \"p1\""), "{text}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn importing_the_same_source_replaces_tracks_and_keeps_the_name() {
        let dir = temp_dir("import");
        let path = dir.join(FILE_NAME);
        let mut store = Store::load(path.clone()).expect("load");
        let night = track(Source::Spotify, "sp-1", Some("Low Glow"));
        let paper = track(Source::YouTubeMusic, "yt-1", None);
        assert!(
            store
                .import_from(Source::Local, "desk", "Local", vec![night.clone()])
                .is_err()
        );
        assert!(store.playlists().is_empty());
        assert!(
            store
                .import_from(Source::Spotify, "  ", "Liked", Vec::new())
                .is_err()
        );

        let created = store
            .import_from(
                Source::Spotify,
                " liked-songs ",
                "  Liked   Songs ",
                vec![night.clone()],
            )
            .expect("create");
        let ImportOutcome::Created { id } = created else {
            panic!("first import creates");
        };
        store.rename(&id, "Kept").expect("rename");
        let updated = store
            .import_from(
                Source::Spotify,
                "liked-songs",
                "A New Spotify Name",
                vec![paper.clone()],
            )
            .expect("update");
        assert_eq!(updated, ImportOutcome::Updated { id: id.clone() });
        let saved = store.playlist(&id).expect("same list");
        assert_eq!(saved.name(), "Kept");
        assert_eq!(saved.tracks(), std::slice::from_ref(&paper));
        assert_eq!(
            saved
                .origin
                .as_ref()
                .map(|origin| origin.playlist_id.as_str()),
            Some("liked-songs")
        );

        let youtube = store
            .import_from(
                Source::YouTubeMusic,
                "liked-songs",
                "Liked songs",
                vec![night.clone()],
            )
            .expect("youtube");
        let ImportOutcome::Created { id: youtube_id } = youtube else {
            panic!("youtube liked songs is a different list");
        };
        assert_ne!(youtube_id, id);
        assert_eq!(store.playlists().len(), 2);

        let long = "N".repeat(NAME_LIMIT + 40);
        let long_id = store
            .import_from(Source::Spotify, "pl-long", &long, Vec::new())
            .expect("long name");
        let ImportOutcome::Created { id: long_id } = long_id else {
            panic!("long name still creates");
        };
        assert_eq!(
            store
                .playlist(&long_id)
                .expect("long")
                .name()
                .chars()
                .count(),
            NAME_LIMIT
        );
        let blank = store
            .import_from(Source::YouTubeMusic, "PLblank", "   ", Vec::new())
            .expect("blank");
        let ImportOutcome::Created { id: blank_id } = blank else {
            panic!("blank title");
        };
        assert_eq!(
            store.playlist(&blank_id).expect("blank").name(),
            "Untitled playlist"
        );

        let text = fs::read_to_string(&path).expect("file");
        assert!(text.contains("\"version\": 2"), "{text}");
        assert!(text.contains("\"playlist_id\": \"liked-songs\""), "{text}");
        assert!(text.contains("\"spotify\""), "{text}");
        assert!(text.contains("\"youtube_music\""), "{text}");

        let loaded = Store::load(path).expect("reload");
        assert_eq!(loaded.playlist(&id).expect("kept").name(), "Kept");
        assert_eq!(
            loaded.playlist(&id).expect("kept").tracks(),
            std::slice::from_ref(&paper)
        );
        assert_eq!(loaded.playlists().len(), 4);

        let duplicate = r#"{
  "version": 2,
  "playlists": [
    {
      "id": "a",
      "name": "One",
      "tracks": [],
      "origin": { "source": "spotify", "playlist_id": "liked-songs" }
    },
    {
      "id": "b",
      "name": "Two",
      "tracks": [],
      "origin": { "source": "spotify", "playlist_id": "liked-songs" }
    }
  ]
}
"#;
        let duplicate_path = dir.join("duplicate.json");
        fs::write(&duplicate_path, duplicate).expect("write duplicate");
        let error = Store::load(duplicate_path.clone()).expect_err("duplicate origin");
        assert!(
            error.message().contains("left unchanged"),
            "{}",
            error.message()
        );
        assert_eq!(
            fs::read(&duplicate_path).expect("bytes"),
            duplicate.as_bytes()
        );

        let local_origin = r#"{
  "version": 2,
  "playlists": [{
    "id": "c",
    "name": "Desk",
    "tracks": [],
    "origin": { "source": "local", "playlist_id": "desk" }
  }]
}
"#;
        let local_path = dir.join("local.json");
        fs::write(&local_path, local_origin).expect("write local");
        assert!(Store::load(local_path.clone()).is_err());
        assert_eq!(
            fs::read(&local_path).expect("bytes"),
            local_origin.as_bytes()
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
