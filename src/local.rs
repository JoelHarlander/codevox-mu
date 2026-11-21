//! Local audio files.
//!
//! The listener picks folders and individual files with the native dialog.
//! A scan reads title, artist, album, and duration with lofty and falls back
//! to the file name. Each track's id is its absolute path, and
//! [`Source::Local`] is the same enum variant the queue and playlists already
//! store. Chosen folders and files live in `local.json` next to saved
//! playlists, and the scan runs off the UI thread.
//!
//! mp3, flac, ogg Vorbis, m4a/aac, and wav go to the rodio sink, which
//! decodes them with symphonia. Opus is not in the symphonia build rodio
//! links, so an Opus file (Ogg, or the WebM Opus YouTube serves) is decoded
//! with the pure-Rust `symphonia-opus` crate and handed to that sink as wav.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use lofty::file::{AudioFile, TaggedFileExt};
use lofty::tag::Accessor;
use serde::{Deserialize, Serialize};

use crate::extract::AudioSource;
use crate::music::{Source, Track};

/// How deep a folder scan will walk. Deeper trees are left unread.
const MAX_DEPTH: u32 = 32;

/// A local file larger than this is refused. The sink buffers the bytes.
const MAX_FILE_BYTES: u64 = 128 * 1024 * 1024;

/// Decoded Opus longer than this is refused. Twelve minutes of stereo at
/// 48 kHz, which is the Opus output rate.
const MAX_OPUS_SAMPLES: usize = 48_000 * 2 * 60 * 12;

const FILE_NAME: &str = "local.json";
const SCHEMA_VERSION: u32 = 1;

const AUDIO_EXTENSIONS: &[&str] = &["mp3", "flac", "ogg", "opus", "m4a", "aac", "wav"];

/// Folders and files the listener asked the app to keep.
///
/// A missing file is an empty store that will create it on the next save.
/// A file this build cannot read is kept and not overwritten.
#[derive(Debug)]
pub struct Store {
    path: Option<PathBuf>,
    folders: Vec<PathBuf>,
    files: Vec<PathBuf>,
    blocked: Option<String>,
}

impl Store {
    /// Nothing is saved. Smoke and tests use this.
    pub fn memory() -> Self {
        Self {
            path: None,
            folders: Vec::new(),
            files: Vec::new(),
            blocked: None,
        }
    }

    /// Load `local.json` from the per-user data directory.
    ///
    /// A missing data directory is an in-memory store that refuses to save,
    /// so a scan is not invented from nowhere.
    pub fn load_default() -> Self {
        let Some(directory) = crate::diag::data_dir() else {
            return Self {
                path: None,
                folders: Vec::new(),
                files: Vec::new(),
                blocked: Some("Could not find a folder for local music.".to_string()),
            };
        };
        Self::load(directory.join(FILE_NAME))
    }

    /// Load `path`. A missing file is an empty store.
    pub fn load(path: PathBuf) -> Self {
        if !path.exists() {
            return Self {
                path: Some(path),
                folders: Vec::new(),
                files: Vec::new(),
                blocked: None,
            };
        }
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                return blocked(
                    path,
                    format!(
                        "Could not read local folders ({error}), so the file was left unchanged."
                    ),
                );
            }
        };
        let body: FileBody = match serde_json::from_slice(&bytes) {
            Ok(body) => body,
            Err(error) => {
                return blocked(
                    path,
                    format!(
                        "Local folders could not be read ({}), so the file was left unchanged.",
                        brief(&error.to_string())
                    ),
                );
            }
        };
        if body.version != SCHEMA_VERSION {
            return blocked(
                path,
                format!(
                    "Local folders are version {}. This app reads version {SCHEMA_VERSION}, so the file was left unchanged.",
                    body.version
                ),
            );
        }
        Self {
            path: Some(path),
            folders: unique_paths(body.folders),
            files: unique_paths(body.files),
            blocked: None,
        }
    }

    /// Folders first, then individually picked files.
    pub fn paths(&self) -> Vec<PathBuf> {
        self.folders
            .iter()
            .chain(self.files.iter())
            .cloned()
            .collect()
    }

    pub fn has_sources(&self) -> bool {
        !self.folders.is_empty() || !self.files.is_empty()
    }

    pub fn blocked_message(&self) -> Option<&str> {
        self.blocked.as_deref()
    }

    /// Remember `path` if it is a directory. A path already stored is left as it is.
    pub fn add_folder(&mut self, path: PathBuf) -> Result<(), String> {
        self.ensure_writable()?;
        if !path.is_dir() {
            return Err("Choose a folder.".to_string());
        }
        let stable = stable_path(&path)?;
        if self.folders.contains(&stable) {
            return Ok(());
        }
        self.folders.push(stable);
        self.save()
    }

    /// Remember audio files. Other paths are skipped. Every path being
    /// unusable is an error so a cancelled-looking pick is explained.
    pub fn add_files(&mut self, paths: Vec<PathBuf>) -> Result<(), String> {
        self.ensure_writable()?;
        let mut added = false;
        let mut saw_audio = false;
        for path in paths {
            if !path.is_file() || !is_audio(&path) {
                continue;
            }
            saw_audio = true;
            let stable = stable_path(&path)?;
            if self.files.contains(&stable) {
                continue;
            }
            self.files.push(stable);
            added = true;
        }
        if !saw_audio {
            return Err("Choose an mp3, flac, ogg, opus, m4a, aac, or wav file.".to_string());
        }
        if !added {
            return Ok(());
        }
        self.save()
    }

    fn ensure_writable(&self) -> Result<(), String> {
        if let Some(message) = &self.blocked {
            return Err(message.clone());
        }
        Ok(())
    }

    fn save(&self) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let body = FileBody {
            version: SCHEMA_VERSION,
            folders: self.folders.clone(),
            files: self.files.clone(),
        };
        let mut bytes = serde_json::to_vec_pretty(&body)
            .map_err(|_| "Could not save local folders.".to_string())?;
        bytes.push(b'\n');
        write_atomic(path, &bytes)
    }
}

fn blocked(path: PathBuf, message: String) -> Store {
    Store {
        path: Some(path),
        folders: Vec::new(),
        files: Vec::new(),
        blocked: Some(message),
    }
}

fn unique_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut unique = Vec::new();
    for path in paths {
        if path.as_os_str().is_empty() || !seen.insert(path.clone()) {
            continue;
        }
        unique.push(path);
    }
    unique
}

fn stable_path(path: &Path) -> Result<PathBuf, String> {
    let absolute = std::path::absolute(path)
        .map_err(|error| format!("Could not use {}: {error}", path.display()))?;
    Ok(fs::canonicalize(&absolute).unwrap_or(absolute))
}

/// Tracks under `paths`. A path may be a folder or one audio file.
///
/// Unreadable folders are skipped. A file lofty cannot parse still becomes
/// a track named after the file. The list is sorted by title, then path.
pub fn scan(paths: &[PathBuf]) -> Vec<Track> {
    let mut files = Vec::new();
    let mut seen_dirs = HashSet::new();
    let mut seen_files = HashSet::new();
    for path in paths {
        collect(path, &mut files, &mut seen_dirs, &mut seen_files, 0);
    }
    let mut tracks: Vec<Track> = files.iter().map(|path| track_for(path)).collect();
    tracks.sort_by(|left, right| {
        left.title
            .to_lowercase()
            .cmp(&right.title.to_lowercase())
            .then_with(|| left.id.cmp(&right.id))
    });
    tracks
}

/// Tracks whose title, artist, album, or file name contains `query`.
///
/// A blank query matches nothing, matching the other catalogues. Case is ignored.
pub fn matching(tracks: &[Track], query: &str) -> Vec<Track> {
    let query = query.trim();
    if query.is_empty() {
        return Vec::new();
    }
    let query = query.to_lowercase();
    tracks
        .iter()
        .filter(|track| search_text(track).to_lowercase().contains(&query))
        .cloned()
        .collect()
}

fn search_text(track: &Track) -> String {
    let file_name = Path::new(&track.id)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut parts = vec![track.title.clone(), file_name];
    parts.extend(track.artists.iter().cloned());
    if let Some(album) = &track.album {
        parts.push(album.clone());
    }
    parts.join("\n")
}

/// Open the native folder picker. Cancel returns `None`.
///
/// This blocks the caller. The window runs it on a background thread.
pub fn pick_folder() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Add a music folder")
        .pick_folder()
}

/// Open the native file picker for audio files. Cancel returns `None`.
pub fn pick_files() -> Option<Vec<PathBuf>> {
    let files = rfd::FileDialog::new()
        .set_title("Add audio files")
        .add_filter("Audio", AUDIO_EXTENSIONS)
        .pick_files()?;
    if files.is_empty() { None } else { Some(files) }
}

/// Bytes the existing sink can decode for the file stored as `id`.
pub fn open_file(id: &str) -> Result<AudioSource, String> {
    let path = Path::new(id);
    let meta = fs::metadata(path).map_err(|_| "the file is missing".to_string())?;
    if !meta.is_file() {
        return Err("the file is missing".to_string());
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err("the file is too large to play".to_string());
    }
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    if looks_like_opus(&bytes) {
        match opus_to_wav(&bytes) {
            Ok(wav) => {
                return Ok(AudioSource {
                    mime: "audio/wav".to_string(),
                    bytes: wav,
                });
            }
            Err(error) if extension_is(path, "opus") => return Err(error),
            Err(_) => {}
        }
    }
    Ok(AudioSource {
        mime: mime_of(path).to_string(),
        bytes,
    })
}

fn collect(
    path: &Path,
    files: &mut Vec<PathBuf>,
    seen_dirs: &mut HashSet<PathBuf>,
    seen_files: &mut HashSet<PathBuf>,
    depth: u32,
) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(meta) = fs::metadata(path) else {
        return;
    };
    if meta.is_dir() {
        let key = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if !seen_dirs.insert(key) {
            return;
        }
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            collect(&entry.path(), files, seen_dirs, seen_files, depth + 1);
        }
        return;
    }
    if !meta.is_file() || !is_audio(path) {
        return;
    }
    let key = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if seen_files.insert(key.clone()) {
        files.push(key);
    }
}

fn track_for(path: &Path) -> Track {
    let (title, artists, album, duration) =
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| read_tags(path))) {
            Ok(tags) => tags,
            Err(_) => (file_title(path), Vec::new(), None, Duration::ZERO),
        };
    Track {
        id: path.to_string_lossy().into_owned(),
        source: Source::Local,
        title,
        artists,
        album,
        duration,
    }
}

fn read_tags(path: &Path) -> (String, Vec<String>, Option<String>, Duration) {
    let fallback = file_title(path);
    let Ok(tagged) = lofty::read_from_path(path) else {
        return (fallback, Vec::new(), None, Duration::ZERO);
    };
    let duration = tagged.properties().duration();
    let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) else {
        return (fallback, Vec::new(), None, duration);
    };
    let title = cleaned(tag.title()).unwrap_or(fallback);
    let artists = cleaned(tag.artist()).into_iter().collect();
    let album = cleaned(tag.album());
    (title, artists, album, duration)
}

fn cleaned(value: Option<impl AsRef<str>>) -> Option<String> {
    let text = value?.as_ref().trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

fn file_title(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = stem.trim();
    if stem.is_empty() {
        "Untitled".to_string()
    } else {
        stem.to_string()
    }
}

fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            AUDIO_EXTENSIONS
                .iter()
                .any(|known| ext.eq_ignore_ascii_case(known))
        })
}

fn extension_is(path: &Path, expected: &str) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case(expected))
}

fn mime_of(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "ogg" | "opus" => "audio/ogg",
        "m4a" | "aac" | "mp4" => "audio/mp4",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    }
}

fn looks_like_opus(bytes: &[u8]) -> bool {
    let window = bytes.len().min(65_536);
    bytes[..window].windows(8).any(|chunk| chunk == b"OpusHead")
}

/// Decode an Ogg or WebM Opus file to 16-bit wav the rodio sink already plays.
pub(crate) fn opus_to_wav(bytes: &[u8]) -> Result<Vec<u8>, String> {
    use symphonia::core::codecs::audio::AudioDecoderOptions;
    use symphonia::core::codecs::registry::CodecRegistry;
    use symphonia::core::common::Tier;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia_opus::OpusDecoder;

    let mut codecs = CodecRegistry::new();
    symphonia::default::register_enabled_codecs(&mut codecs);
    codecs.register_audio_decoder_at_tier::<OpusDecoder>(Tier::Fallback);

    let cursor = Cursor::new(bytes.to_vec());
    let source = MediaSourceStream::new(Box::new(cursor), Default::default());
    let mut format = symphonia::default::get_probe()
        .probe(
            &Hint::new(),
            source,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|error| format!("could not read the Opus file ({error})"))?;

    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| "the Opus file has no audio".to_string())?;
    let track_id = track.id;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|params| params.audio())
        .cloned()
        .ok_or_else(|| "the Opus file has no audio".to_string())?;
    let mut decoder = codecs
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .map_err(|error| format!("could not decode Opus ({error})"))?;

    let mut pcm = Vec::new();
    let mut rate = 0u32;
    let mut channels = 0u16;
    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(error) => return Err(format!("could not read the Opus file ({error})")),
        };
        if packet.track_id != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(_) => continue,
        };
        if decoded.frames() == 0 {
            continue;
        }
        let packet_rate = decoded.spec().rate();
        let packet_channels = u16::try_from(decoded.num_planes()).unwrap_or(0);
        if packet_rate == 0 || packet_channels == 0 {
            continue;
        }
        if rate == 0 {
            rate = packet_rate;
            channels = packet_channels;
        } else if packet_rate != rate || packet_channels != channels {
            return Err("the Opus file changes format mid-stream".to_string());
        }
        let count = decoded.samples_interleaved();
        if pcm.len().saturating_add(count) > MAX_OPUS_SAMPLES {
            return Err("the file is too long to play".to_string());
        }
        let mut floats = vec![0.0f32; count];
        decoded.copy_to_slice_interleaved(&mut floats);
        for sample in floats {
            let clamped = sample.clamp(-1.0, 1.0);
            pcm.push((clamped * f32::from(i16::MAX)) as i16);
        }
    }
    if pcm.is_empty() || rate == 0 || channels == 0 {
        return Err("the Opus file has no audio".to_string());
    }
    Ok(pcm_wav(rate, channels, &pcm))
}

fn pcm_wav(sample_rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
    let channels = channels.max(1);
    let data_len = samples.len().saturating_mul(2);
    let data_len = u32::try_from(data_len).unwrap_or(u32::MAX);
    let mut bytes = Vec::with_capacity(44 + samples.len().saturating_mul(2));
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36u32.saturating_add(data_len)).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&channels.to_le_bytes());
    bytes.extend_from_slice(&sample_rate.to_le_bytes());
    let byte_rate = sample_rate.saturating_mul(u32::from(channels) * 2);
    bytes.extend_from_slice(&byte_rate.to_le_bytes());
    bytes.extend_from_slice(&channels.saturating_mul(2).to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

fn brief(text: &str) -> String {
    let mut brief: String = text.chars().take(120).collect();
    if text.chars().count() > 120 {
        brief.push('…');
    }
    brief
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not save local folders: {error}"))?;
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
        .map_err(|error| format!("Could not save local folders: {error}"))?;
    file.write_all(bytes)
        .map_err(|error| format!("Could not save local folders: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("Could not save local folders: {error}"))?;
    replace_file(temporary, path)
        .map_err(|error| format!("Could not save local folders: {error}"))?;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && let Ok(directory) = File::open(parent)
    {
        let _ = directory.sync_all();
    }
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

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileBody {
    version: u32,
    folders: Vec<PathBuf>,
    files: Vec<PathBuf>,
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use lofty::config::WriteOptions;
    use lofty::tag::{Accessor, Tag, TagExt, TagType};

    use super::*;
    use crate::music::Source;

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "codevox-mu-local-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn write_wav(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        let mut samples = Vec::with_capacity(8_000);
        for index in 0..8_000 {
            samples.push((index % 16) as i16 * 1000);
        }
        fs::write(path, pcm_wav(8_000, 1, &samples)).expect("wav");
    }

    fn write_tags(path: &Path, title: &str, artist: &str, album: &str) {
        let mut tag = Tag::new(TagType::Id3v2);
        tag.set_title(title.to_string());
        tag.set_artist(artist.to_string());
        tag.set_album(album.to_string());
        tag.save_to_path(path, WriteOptions::default())
            .expect("write tags");
    }

    fn fixture_dir() -> PathBuf {
        let dir = temp_dir("scan");
        let tagged = dir.join("ignored-name.wav");
        write_wav(&tagged);
        write_tags(&tagged, "Night Drive", "Ada Vega", "Low Glow");
        write_wav(&dir.join("Desk Tape.WAV"));
        write_wav(&dir.join("nested").join("Room Tone.wav"));
        fs::write(dir.join("clip.MP3"), b"not audio, just a name").expect("mp3");
        fs::write(dir.join("notes.txt"), b"skip me").expect("txt");
        fs::write(dir.join("cover.jpg"), b"skip me").expect("jpg");
        dir
    }

    fn by_title<'a>(tracks: &'a [Track], title: &str) -> &'a Track {
        tracks
            .iter()
            .find(|track| track.title == title)
            .unwrap_or_else(|| panic!("missing {title}"))
    }

    #[test]
    fn scan_reads_tags_and_falls_back_to_the_file_name() {
        let dir = fixture_dir();
        let tracks = scan(std::slice::from_ref(&dir));

        let titles: Vec<_> = tracks.iter().map(|track| track.title.as_str()).collect();
        assert_eq!(titles, ["clip", "Desk Tape", "Night Drive", "Room Tone"]);
        assert!(tracks.iter().all(|track| track.source == Source::Local));

        let night = by_title(&tracks, "Night Drive");
        assert_eq!(night.artists, ["Ada Vega"]);
        assert_eq!(night.album.as_deref(), Some("Low Glow"));
        assert_eq!(night.duration, Duration::from_secs(1));
        assert!(night.id.ends_with("ignored-name.wav"), "{}", night.id);
        assert!(Path::new(&night.id).is_absolute());

        let desk = by_title(&tracks, "Desk Tape");
        assert!(desk.artists.is_empty());
        assert_eq!(desk.album, None);
        assert_eq!(desk.duration, Duration::from_secs(1));
        assert!(desk.id.ends_with("Desk Tape.WAV"), "{}", desk.id);

        let room = by_title(&tracks, "Room Tone");
        assert!(room.id.contains("nested"), "{}", room.id);
        assert!(room.artists.is_empty());

        let clip = by_title(&tracks, "clip");
        assert_eq!(clip.duration, Duration::ZERO);
        assert!(clip.artists.is_empty());
        assert_eq!(clip.album, None);

        let found = matching(&tracks, "ada");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].title, "Night Drive");
        assert!(
            matching(&tracks, "desk")
                .iter()
                .any(|track| track.title == "Desk Tape")
        );
        assert!(matching(&tracks, "  ").is_empty());

        let opened = open_file(&desk.id).expect("wav opens");
        assert_eq!(opened.mime, "audio/wav");
        assert!(opened.bytes.starts_with(b"RIFF"));
        let decoded =
            crate::output::decode_prefix(&opened.bytes, &opened.mime, Duration::from_secs(1))
                .expect("existing decoder reads the local wav");
        assert!(decoded.samples.iter().any(|sample| sample.abs() > 0.01));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn folders_and_files_round_trip_and_rescan() {
        let root = temp_dir("store");
        let music = root.join("music");
        fs::create_dir_all(&music).expect("music");
        write_wav(&music.join("Desk Tape.wav"));
        let loose = root.join("clip.mp3");
        fs::write(&loose, b"name only").expect("loose");
        let path = root.join(FILE_NAME);

        let mut store = Store::load(path.clone());
        store.add_folder(music.clone()).expect("folder");
        store.add_folder(music.clone()).expect("again");
        store.add_files(vec![loose.clone()]).expect("file");
        assert!(store.add_files(vec![root.join("notes.txt")]).is_err());

        let text = fs::read_to_string(&path).expect("saved");
        assert!(text.contains("\"version\": 1"), "{text}");
        assert!(!root.join("local.json.tmp").exists());

        let loaded = Store::load(path);
        assert_eq!(loaded.folders.len(), 1);
        assert_eq!(loaded.files.len(), 1);
        assert!(loaded.folders[0].ends_with("music"));
        assert!(loaded.files[0].ends_with("clip.mp3"));

        let tracks = scan(&loaded.paths());
        let titles: Vec<_> = tracks.iter().map(|track| track.title.as_str()).collect();
        assert_eq!(titles, ["clip", "Desk Tape"]);
        assert!(tracks.iter().all(|track| track.source == Source::Local));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_scanned_local_track_round_trips_through_a_mixed_playlist() {
        let dir = fixture_dir();
        let tracks = scan(std::slice::from_ref(&dir));
        let night = by_title(&tracks, "Night Drive").clone();
        let desk = by_title(&tracks, "Desk Tape").clone();
        let spotify = Track {
            id: "sp-night-drive".to_string(),
            source: Source::Spotify,
            title: "Night Drive".to_string(),
            artists: vec!["Ada Vega".to_string()],
            album: Some("Low Glow".to_string()),
            duration: Duration::from_secs(192),
        };
        let youtube = Track {
            id: "yt-kitchen".to_string(),
            source: Source::YouTubeMusic,
            title: "Kitchen Radio".to_string(),
            artists: vec!["North Line".to_string()],
            album: None,
            duration: Duration::from_secs(178),
        };

        let store_dir = temp_dir("playlist");
        let path = store_dir.join("playlists.json");
        let mut store = crate::playlists::Store::load(path.clone()).expect("empty");
        let id = store.create("Late Drive").expect("create");
        store.add_track(&id, spotify.clone()).expect("spotify");
        store.add_track(&id, night.clone()).expect("local");
        store.add_track(&id, youtube.clone()).expect("youtube");
        store.add_track(&id, desk.clone()).expect("fallback");

        let loaded = crate::playlists::Store::load(path).expect("reload");
        let saved = loaded.playlist(&id).expect("playlist");
        assert_eq!(saved.tracks().len(), 4);
        assert_eq!(saved.tracks()[0].source, Source::Spotify);
        assert_eq!(saved.tracks()[1], night);
        assert_eq!(saved.tracks()[1].source, Source::Local);
        assert_eq!(saved.tracks()[2], youtube);
        assert_eq!(saved.tracks()[3], desk);
        assert!(saved.tracks()[3].artists.is_empty());
        assert_eq!(saved.tracks()[3].title, "Desk Tape");

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&store_dir);
    }

    #[test]
    fn a_broken_opus_file_is_refused_without_panicking() {
        let dir = temp_dir("opus");
        let path = dir.join("silence.opus");
        fs::write(&path, b"OpusHead is not a full file").expect("opus");
        let error = open_file(&path.to_string_lossy()).expect_err("refused");
        assert!(error.contains("Opus") || error.contains("audio"), "{error}");
        let _ = fs::remove_dir_all(&dir);
    }
}
