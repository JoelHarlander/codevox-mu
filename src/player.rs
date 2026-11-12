//! Play, pause, stop, skip, and return to the previous track in a mixed
//! [`Queue`](crate::queue::Queue).
//!
//! The state machine is synchronous and talks to an [`AudioSink`]. Tests
//! pass a fake sink. The window uses [`Playback`], which owns the machine
//! on a thread named `playback` so stream resolution and the device stay
//! off the UI thread.
//!
//! A Spotify track plays through a [`SpotifyEngine`] when that account can
//! play. Otherwise the title, artist, and duration are matched on YouTube
//! Music and that stream plays, with [`SPOTIFY_FALLBACK_NOTE`]. If nothing
//! matches, the track is skipped and [`SPOTIFY_UNAVAILABLE_NOTE`] is set.
//! A track librespot cannot decrypt does the same match, with
//! [`SPOTIFY_KEY_FALLBACK_NOTE`], and stays on screen with
//! [`SPOTIFY_KEY_UNAVAILABLE_NOTE`] when YouTube Music has no match.
//! Local files are skipped with an error. A failure to resolve a stream or
//! open the device is stored on the snapshot. Nothing here panics for those
//! cases.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::extract::{Extractor, InnertubeExtractor};
use crate::music::{Source, Track};
use crate::output::{AudioSink, RodioSink, SinkError};
use crate::queue::{Queue, Shift};

/// Shown when a Spotify track is heard through its YouTube Music match.
pub const SPOTIFY_FALLBACK_NOTE: &str =
    "Playing the YouTube Music match. Spotify Premium is required for Spotify audio.";

/// Shown when a Spotify track cannot be played and YouTube Music has no match.
pub const SPOTIFY_UNAVAILABLE_NOTE: &str =
    "Spotify Premium is required, and no YouTube Music match was found";

/// Shown when librespot cannot decrypt a track and the YouTube Music match plays.
///
/// Spotify stopped giving audio keys to some Premium accounts. librespot 0.8.0
/// is the newest release, and upstream has no fix. See librespot-org/librespot#1649.
pub const SPOTIFY_KEY_FALLBACK_NOTE: &str =
    "Spotify couldn't play this track, so it's playing from YouTube Music.";

/// Shown when that match does not exist. The Spotify row stays put.
pub const SPOTIFY_KEY_UNAVAILABLE_NOTE: &str =
    "Spotify couldn't play this track, and no YouTube Music match was found.";

const LOCAL_UNAVAILABLE: &str = "Local playback is not available yet";

/// What the transport is doing. `Loading` is only a snapshot value, written
/// while a YouTube stream is still downloading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Loading,
    Playing,
    Paused,
    Stopped,
}

/// The now-playing bar reads this. The playback thread writes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NowPlaying {
    pub title: String,
    pub artist: String,
    pub phase: Phase,
    pub elapsed: Duration,
    pub duration: Duration,
    pub error: Option<String>,
    pub note: Option<String>,
    pub can_play: bool,
    pub can_pause: bool,
    pub can_stop: bool,
    pub can_next: bool,
    /// Previous can restart this track or return to an earlier one.
    pub can_previous: bool,
    /// Service the loaded track came from. `None` when nothing is loaded.
    pub source: Option<Source>,
    /// Source id of the loaded track. Empty when nothing is loaded.
    pub track_id: String,
    /// Album of the loaded track. `None` when the source has none, or nothing is loaded.
    pub album: Option<String>,
    /// The loaded track, when there is one.
    ///
    /// The queue view saves this into a mixed playlist. The title and artist
    /// strings are not enough: playback needs the source id and the artist list.
    pub loaded: Option<Track>,
    /// Tracks that play after this one, front first.
    pub upcoming: Vec<Track>,
}

impl Default for NowPlaying {
    fn default() -> Self {
        Self {
            title: String::new(),
            artist: String::new(),
            phase: Phase::Idle,
            elapsed: Duration::ZERO,
            duration: Duration::ZERO,
            error: None,
            note: None,
            can_play: false,
            can_pause: false,
            can_stop: false,
            can_next: false,
            can_previous: false,
            source: None,
            track_id: String::new(),
            album: None,
            loaded: None,
            upcoming: Vec::new(),
        }
    }
}

/// Position reported by a Spotify engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpotifyFeed {
    pub position: Duration,
    pub finished: bool,
    pub failed: Option<String>,
    /// A `Playing` event arrived in this poll.
    pub playing: bool,
}

/// In-app Spotify audio. The mock used by tests never opens a device.
pub trait SpotifyEngine: Send {
    fn can_play(&self) -> bool;
    fn load(&mut self, track_id: &str) -> Result<(), String>;
    fn play(&mut self);
    fn pause(&mut self);
    fn stop(&mut self);
    fn poll(&mut self) -> SpotifyFeed;
}

/// Finds a YouTube Music track for a Spotify row that cannot play.
pub trait YoutubeMatch: Send {
    /// A YouTube Music recording of the same title, primary artist, and duration.
    fn find(
        &mut self,
        title: &str,
        artists: &[String],
        duration: Duration,
    ) -> Result<Option<Track>, String>;
}

/// Search text for a YouTube Music match: title, then each artist.
pub fn match_query(title: &str, artists: &[String]) -> String {
    let mut parts = Vec::new();
    let title = title.trim();
    if !title.is_empty() {
        parts.push(title.to_string());
    }
    for artist in artists {
        let artist = artist.trim();
        if !artist.is_empty() {
            parts.push(artist.to_string());
        }
    }
    parts.join(" ")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AudioOrigin {
    None,
    Stream,
    Spotify,
}

enum Start {
    Playing,
    /// This track cannot be decoded or resolved. Try the next one.
    Skip,
    /// The device failed. Further tracks will fail the same way.
    Stop,
}

/// Queue plus the sink. `E` resolves YouTube ids. `S` makes sound.
pub struct Player<E, S> {
    queue: Queue,
    phase: Phase,
    duration: Duration,
    error: Option<String>,
    note: Option<String>,
    extractor: E,
    sink: S,
    origin: AudioOrigin,
    spotify_position: Duration,
    spotify: Box<dyn SpotifyEngine>,
    matcher: Box<dyn YoutubeMatch>,
}

impl<E, S> Player<E, S>
where
    E: Extractor,
    S: AudioSink,
{
    pub fn new(
        extractor: E,
        sink: S,
        spotify: Box<dyn SpotifyEngine>,
        matcher: Box<dyn YoutubeMatch>,
    ) -> Self {
        Self {
            queue: Queue::new(),
            phase: Phase::Idle,
            duration: Duration::ZERO,
            error: None,
            note: None,
            extractor,
            sink,
            origin: AudioOrigin::None,
            spotify_position: Duration::ZERO,
            spotify,
            matcher,
        }
    }

    /// Add `track` at the end of the upcoming list.
    pub fn enqueue(&mut self, track: Track) {
        self.queue.enqueue(track);
    }

    /// Insert `track` so it plays immediately after the loaded one.
    pub fn play_next(&mut self, track: Track) {
        self.queue.play_next(track);
    }

    /// Move the upcoming track at `index` one place earlier.
    pub fn move_earlier(&mut self, index: usize) {
        self.queue.move_earlier(index);
    }

    /// Move the upcoming track at `index` one place later.
    pub fn move_later(&mut self, index: usize) {
        self.queue.move_later(index);
    }

    /// Drop the upcoming track at `index`. The loaded track keeps playing.
    pub fn remove_queued(&mut self, index: usize) {
        let _ = self.queue.remove_upcoming(index);
    }

    /// Drop the loaded track and play whatever follows it.
    ///
    /// The dropped track is not remembered, so previous will not return to it.
    pub fn remove_current(&mut self) {
        self.halt_audio();
        self.queue.remove_current();
        self.phase = Phase::Idle;
        self.note = None;
        self.error = None;
        self.advance();
    }

    /// Play `track` now. A YouTube track replaces whatever is audible. A
    /// Spotify track plays in-app when the account can, and otherwise plays
    /// the YouTube Music match. A click that cannot play anything leaves the
    /// current track running and sets a note.
    pub fn play_now(&mut self, track: Track) {
        match track.source {
            Source::Local => {
                self.error = Some(LOCAL_UNAVAILABLE.to_string());
            }
            Source::YouTubeMusic => {
                self.halt_audio();
                self.queue.remember_current();
                self.note = None;
                let _ = self.play_stream(track, None);
            }
            Source::Spotify => self.play_spotify_now(track),
        }
    }

    /// Resume, restart the current track, or start the queue.
    pub fn play(&mut self) {
        match self.phase {
            Phase::Playing | Phase::Loading => {}
            Phase::Paused => {
                if self.origin == AudioOrigin::Spotify {
                    self.spotify.play();
                } else {
                    self.sink.resume();
                }
                self.phase = Phase::Playing;
            }
            Phase::Stopped => {
                let replay_spotify = self
                    .queue
                    .current()
                    .is_some_and(|track| track.source == Source::Spotify);
                if replay_spotify {
                    if let Some(track) = self.queue.current().cloned() {
                        let _ = self.load_spotify(&track);
                    }
                } else {
                    let _ = self.start_current();
                }
            }
            Phase::Idle => self.advance(),
        }
    }

    pub fn pause(&mut self) {
        if self.phase == Phase::Playing {
            if self.origin == AudioOrigin::Spotify {
                self.spotify.pause();
            } else {
                self.sink.pause();
            }
            self.phase = Phase::Paused;
        }
    }

    /// Stop the device and keep the current track so play starts it over.
    pub fn stop(&mut self) {
        if matches!(self.phase, Phase::Playing | Phase::Paused) {
            match self.origin {
                AudioOrigin::Spotify => self.spotify.stop(),
                AudioOrigin::Stream => self.sink.stop(),
                AudioOrigin::None => {}
            }
            self.phase = Phase::Stopped;
        }
    }

    /// Drop the current track and play the next queued one.
    pub fn next(&mut self) {
        self.halt_audio();
        self.queue.forward();
        self.phase = Phase::Idle;
        self.advance();
    }

    /// Play the track heard before this one.
    ///
    /// With no earlier track, the current one starts again from the beginning.
    /// A Spotify entry plays in-app when the account can, and otherwise plays
    /// its YouTube Music match. Local files in the history are skipped.
    pub fn previous(&mut self) {
        self.halt_audio();
        let retreat = self.queue.back();
        if retreat.skipped_local {
            self.error = Some(LOCAL_UNAVAILABLE.to_string());
        }
        match retreat.shift {
            Shift::Idle => {}
            Shift::Restart(_) => self.restart_current(),
            Shift::Play(track) => {
                self.note = None;
                if track.source == Source::Spotify {
                    self.play_spotify_from_history(track);
                } else {
                    let _ = self.play_stream(track, None);
                }
            }
        }
    }

    /// When the sink has finished the current track, play the next one.
    ///
    /// Returns whether the current track changed.
    pub fn poll(&mut self) -> bool {
        if self.origin == AudioOrigin::Spotify
            && matches!(self.phase, Phase::Playing | Phase::Paused)
        {
            let feed = self.spotify.poll();
            self.spotify_position = feed.position;
            if let Some(message) = feed.failed {
                log::info!("spotify playback failed: {message}");
                self.spotify.stop();
                self.origin = AudioOrigin::None;
                self.phase = Phase::Stopped;
                self.error = None;
                if let Some(track) = self.queue.current().cloned()
                    && track.source == Source::Spotify
                {
                    self.play_youtube_match(
                        &track,
                        SPOTIFY_KEY_FALLBACK_NOTE,
                        SPOTIFY_KEY_UNAVAILABLE_NOTE,
                    );
                } else {
                    self.error = Some(format!("Spotify playback failed: {message}"));
                }
                return true;
            }
            if self.phase == Phase::Playing && feed.finished {
                self.queue.forward();
                self.origin = AudioOrigin::None;
                self.phase = Phase::Idle;
                self.advance();
                return true;
            }
            return false;
        }
        if self.phase != Phase::Playing || !self.sink.finished() {
            return false;
        }
        self.queue.forward();
        self.origin = AudioOrigin::None;
        self.phase = Phase::Idle;
        self.advance();
        true
    }

    pub fn view(&self) -> NowPlaying {
        let current = self.queue.current();
        NowPlaying {
            title: current.map(|track| track.title.clone()).unwrap_or_default(),
            artist: current.map(artist_line).unwrap_or_default(),
            album: current.and_then(|track| track.album.clone()),
            phase: self.phase,
            elapsed: self.elapsed(),
            duration: if current.is_some() {
                self.duration
            } else {
                Duration::ZERO
            },
            error: self.error.clone(),
            note: self.note.clone(),
            can_play: self.can_play(),
            can_pause: self.phase == Phase::Playing,
            can_stop: matches!(self.phase, Phase::Playing | Phase::Paused),
            can_next: current.is_some() || !self.queue.is_empty(),
            can_previous: self.queue.can_back(),
            source: current.map(|track| track.source),
            track_id: current.map(|track| track.id.clone()).unwrap_or_default(),
            loaded: current.cloned(),
            upcoming: self.queue.upcoming().to_vec(),
        }
    }

    /// Replace the queue so `tracks[index]` is loaded.
    ///
    /// Tracks before `index` stay reachable with previous. The tracks after
    /// it stay upcoming. [`Self::start_staged`] starts playback.
    fn stage_list(&mut self, tracks: Vec<Track>, index: usize) -> bool {
        self.halt_audio();
        self.note = None;
        self.error = None;
        self.origin = AudioOrigin::None;
        self.queue.start_from(tracks, index);
        self.queue.fill_current();
        if let Some(duration) = self.queue.current().map(|track| track.duration) {
            self.duration = duration;
            self.phase = Phase::Loading;
            true
        } else {
            self.duration = Duration::ZERO;
            self.phase = Phase::Idle;
            false
        }
    }

    /// Play the track [`Self::stage_list`] promoted, then the tracks after it.
    fn start_staged(&mut self) {
        if self.queue.current().is_none() {
            self.phase = Phase::Idle;
            return;
        }
        self.advance();
    }

    /// Play a saved playlist through this queue, starting at `index`.
    ///
    /// The window reaches the same two steps through [`Playback::play_list`],
    /// which can publish a loading snapshot between them. Tests call this.
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "tests play a playlist without the playback thread"
        )
    )]
    pub fn play_from_list(&mut self, tracks: Vec<Track>, index: usize) {
        self.stage_list(tracks, index);
        self.start_staged();
    }

    fn restart_current(&mut self) {
        if self.queue.current().is_none() {
            return;
        }
        if !matches!(self.phase, Phase::Playing | Phase::Paused | Phase::Stopped) {
            return;
        }
        self.halt_audio();
        self.phase = Phase::Stopped;
        let replay_spotify = self
            .queue
            .current()
            .is_some_and(|track| track.source == Source::Spotify);
        if replay_spotify {
            if let Some(track) = self.queue.current().cloned() {
                let _ = self.load_spotify(&track);
            }
        } else {
            let _ = self.start_current();
        }
    }

    /// A Spotify track from the history. Same outcome as clicking the row.
    fn play_spotify_from_history(&mut self, track: Track) {
        if self.spotify.can_play() {
            let _ = self.load_spotify(&track);
            return;
        }
        match self
            .matcher
            .find(&track.title, &track.artists, track.duration)
        {
            Ok(Some(matched)) => {
                let _ = self.play_stream(matched, Some(SPOTIFY_FALLBACK_NOTE.to_string()));
            }
            Ok(None) => {
                self.queue.load(track);
                self.phase = Phase::Stopped;
                self.note = Some(SPOTIFY_UNAVAILABLE_NOTE.to_string());
            }
            Err(message) => {
                self.queue.load(track);
                self.phase = Phase::Stopped;
                self.error = Some(format!("Couldn't find a YouTube Music match: {message}"));
                self.note = Some(SPOTIFY_UNAVAILABLE_NOTE.to_string());
            }
        }
    }

    fn elapsed(&self) -> Duration {
        match (self.phase, self.origin) {
            (Phase::Playing | Phase::Paused, AudioOrigin::Spotify) => self.spotify_position,
            (Phase::Playing | Phase::Paused, AudioOrigin::Stream) => self.sink.elapsed(),
            _ => Duration::ZERO,
        }
    }

    fn can_play(&self) -> bool {
        match self.phase {
            Phase::Playing | Phase::Loading => false,
            Phase::Paused | Phase::Stopped => self.queue.current().is_some(),
            Phase::Idle => !self.queue.is_empty(),
        }
    }

    fn halt_audio(&mut self) {
        match self.origin {
            AudioOrigin::Stream if matches!(self.phase, Phase::Playing | Phase::Paused) => {
                self.sink.stop();
            }
            AudioOrigin::Spotify => self.spotify.stop(),
            AudioOrigin::None | AudioOrigin::Stream => {}
        }
        self.origin = AudioOrigin::None;
    }

    fn start_current(&mut self) -> Start {
        let Some(track) = self.queue.current().cloned() else {
            self.phase = Phase::Idle;
            return Start::Stop;
        };
        if track.source != Source::YouTubeMusic {
            self.phase = Phase::Stopped;
            return Start::Skip;
        }

        let source = match self.extractor.audio(&track.id) {
            Ok(source) => source,
            Err(error) => {
                self.phase = Phase::Stopped;
                self.error = Some(format!("Couldn't open the stream: {error}"));
                return Start::Skip;
            }
        };

        match self.sink.start(source) {
            Ok(()) => {
                self.origin = AudioOrigin::Stream;
                self.phase = Phase::Playing;
                self.error = None;
                Start::Playing
            }
            Err(SinkError::Device(message)) => {
                self.phase = Phase::Stopped;
                self.error = Some(format!("Couldn't open the audio device: {message}"));
                Start::Stop
            }
            Err(SinkError::Decode(message)) => {
                self.phase = Phase::Stopped;
                self.error = Some(format!("Couldn't decode the audio: {message}"));
                Start::Skip
            }
        }
    }

    fn play_youtube_match(&mut self, track: &Track, playing: &str, missing: &str) {
        match self
            .matcher
            .find(&track.title, &track.artists, track.duration)
        {
            Ok(Some(matched)) => {
                let _ = self.play_stream(matched, Some(playing.to_string()));
            }
            Ok(None) => {
                self.note = Some(missing.to_string());
            }
            Err(message) => {
                self.error = Some(format!("Couldn't find a YouTube Music match: {message}"));
                self.note = Some(missing.to_string());
            }
        }
    }

    fn play_stream(&mut self, track: Track, note: Option<String>) -> Start {
        self.duration = track.duration;
        self.queue.load(track);
        self.phase = Phase::Stopped;
        let started = self.start_current();
        match &started {
            Start::Playing => self.note = note,
            Start::Stop | Start::Skip => {
                if note.is_some() {
                    self.note = note;
                }
            }
        }
        started
    }

    /// A clicked Spotify row. Does not stop the current track unless Spotify
    /// or a YouTube Music match can actually start.
    fn play_spotify_now(&mut self, track: Track) {
        if self.spotify.can_play() {
            self.halt_audio();
            self.queue.remember_current();
            self.note = None;
            let _ = self.load_spotify(&track);
            return;
        }
        match self
            .matcher
            .find(&track.title, &track.artists, track.duration)
        {
            Ok(Some(matched)) => {
                self.halt_audio();
                self.queue.remember_current();
                self.note = None;
                let _ = self.play_stream(matched, Some(SPOTIFY_FALLBACK_NOTE.to_string()));
            }
            Ok(None) => {
                self.note = Some(SPOTIFY_UNAVAILABLE_NOTE.to_string());
            }
            Err(message) => {
                self.error = Some(format!("Couldn't find a YouTube Music match: {message}"));
                self.note = Some(SPOTIFY_UNAVAILABLE_NOTE.to_string());
            }
        }
    }

    fn load_spotify(&mut self, track: &Track) -> Start {
        self.duration = track.duration;
        self.queue.load(track.clone());
        self.spotify_position = Duration::ZERO;
        match self.spotify.load(&track.id) {
            Ok(()) => {
                self.origin = AudioOrigin::Spotify;
                self.phase = Phase::Playing;
                self.error = None;
                Start::Playing
            }
            Err(message) => {
                self.origin = AudioOrigin::None;
                self.phase = Phase::Stopped;
                self.error = Some(format!("Couldn't play the Spotify track: {message}"));
                Start::Skip
            }
        }
    }

    fn advance(&mut self) {
        self.queue.fill_current();
        let mut skipped_spotify = false;
        loop {
            let Some(track) = self.queue.current().cloned() else {
                self.phase = Phase::Idle;
                self.origin = AudioOrigin::None;
                if skipped_spotify {
                    self.note = Some(SPOTIFY_UNAVAILABLE_NOTE.to_string());
                }
                return;
            };

            match track.source {
                Source::Local => {
                    self.error = Some(LOCAL_UNAVAILABLE.to_string());
                    self.queue.remove_current();
                }
                Source::Spotify => {
                    if self.spotify.can_play() {
                        match self.load_spotify(&track) {
                            Start::Playing => {
                                self.note =
                                    skipped_spotify.then(|| SPOTIFY_UNAVAILABLE_NOTE.to_string());
                                return;
                            }
                            Start::Stop => return,
                            Start::Skip => {
                                self.queue.remove_current();
                            }
                        }
                    } else {
                        match self
                            .matcher
                            .find(&track.title, &track.artists, track.duration)
                        {
                            Ok(Some(matched)) => {
                                // The Spotify row is replaced by the match. It is
                                // not kept in the history.
                                match self
                                    .play_stream(matched, Some(SPOTIFY_FALLBACK_NOTE.to_string()))
                                {
                                    Start::Playing | Start::Stop => return,
                                    Start::Skip => {
                                        self.queue.remove_current();
                                    }
                                }
                            }
                            Ok(None) => {
                                skipped_spotify = true;
                                self.queue.remove_current();
                            }
                            Err(message) => {
                                self.error =
                                    Some(format!("Couldn't find a YouTube Music match: {message}"));
                                skipped_spotify = true;
                                self.queue.remove_current();
                            }
                        }
                    }
                }
                Source::YouTubeMusic => {
                    let note = skipped_spotify.then(|| SPOTIFY_UNAVAILABLE_NOTE.to_string());
                    match self.play_stream(track, note) {
                        Start::Playing | Start::Stop => return,
                        Start::Skip => {
                            self.queue.remove_current();
                        }
                    }
                }
            }
        }
    }
}

fn artist_line(track: &Track) -> String {
    track.artists.join(", ")
}

enum Command {
    PlayNow(Track),
    Play,
    Pause,
    Stop,
    Next,
    Previous,
    Enqueue(Track),
    PlayNext(Track),
    MoveEarlier(usize),
    MoveLater(usize),
    RemoveQueued(usize),
    RemoveCurrent,
    /// Play `tracks` through the queue, starting at `index`.
    PlayList {
        tracks: Vec<Track>,
        index: usize,
    },
}

/// Handle the window keeps. Commands go to the playback thread.
///
/// [`Playback::dormant`] has no thread and opens no device. `--smoke` uses
/// that. [`Playback::start`] is the running app.
pub struct Playback {
    commands: Option<Sender<Command>>,
    snapshot: Arc<Mutex<NowPlaying>>,
}

impl Playback {
    pub fn dormant() -> Self {
        Self {
            commands: None,
            snapshot: Arc::new(Mutex::new(NowPlaying::default())),
        }
    }

    pub fn start(spotify: Box<dyn SpotifyEngine>, matcher: Box<dyn YoutubeMatch>) -> Self {
        let (sender, receiver) = mpsc::channel();
        let snapshot = Arc::new(Mutex::new(NowPlaying::default()));
        let shared = Arc::clone(&snapshot);
        let spawned = std::thread::Builder::new()
            .name("playback".into())
            .spawn(move || worker(receiver, shared, spotify, matcher));
        if let Err(error) = spawned {
            {
                let mut guard = snapshot
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                guard.error = Some(format!("Couldn't start playback: {error}"));
            }
            return Self {
                commands: None,
                snapshot,
            };
        }
        Self {
            commands: Some(sender),
            snapshot,
        }
    }

    pub fn play_now(&self, track: Track) {
        self.send(Command::PlayNow(track));
    }

    pub fn play(&self) {
        self.send(Command::Play);
    }

    pub fn pause(&self) {
        self.send(Command::Pause);
    }

    pub fn stop(&self) {
        self.send(Command::Stop);
    }

    pub fn next(&self) {
        self.send(Command::Next);
    }

    pub fn previous(&self) {
        self.send(Command::Previous);
    }

    pub fn enqueue(&self, track: Track) {
        self.send(Command::Enqueue(track));
    }

    pub fn play_next(&self, track: Track) {
        self.send(Command::PlayNext(track));
    }

    pub fn move_earlier(&self, index: usize) {
        self.send(Command::MoveEarlier(index));
    }

    pub fn move_later(&self, index: usize) {
        self.send(Command::MoveLater(index));
    }

    pub fn remove_queued(&self, index: usize) {
        self.send(Command::RemoveQueued(index));
    }

    pub fn remove_current(&self) {
        self.send(Command::RemoveCurrent);
    }

    /// Play `tracks` through the mixed queue, starting at `index`.
    pub fn play_list(&self, tracks: Vec<Track>, index: usize) {
        self.send(Command::PlayList { tracks, index });
    }

    pub fn now(&self) -> NowPlaying {
        self.snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn phase(&self) -> Phase {
        self.now().phase
    }

    fn send(&self, command: Command) {
        let Some(commands) = &self.commands else {
            return;
        };
        if commands.send(command).is_err() {
            let mut guard = self
                .snapshot
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if guard.error.is_none() {
                guard.error = Some("Playback stopped unexpectedly".to_string());
            }
        }
    }
}

fn worker(
    receiver: Receiver<Command>,
    snapshot: Arc<Mutex<NowPlaying>>,
    spotify: Box<dyn SpotifyEngine>,
    matcher: Box<dyn YoutubeMatch>,
) {
    let shared = Arc::clone(&snapshot);
    let result = catch_unwind(AssertUnwindSafe(move || {
        worker_loop(receiver, shared, spotify, matcher);
    }));
    if result.is_err() {
        let mut guard = snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.phase = Phase::Idle;
        guard.can_pause = false;
        guard.can_stop = false;
        guard.error = Some("Playback stopped unexpectedly".to_string());
    }
}

fn worker_loop(
    receiver: Receiver<Command>,
    snapshot: Arc<Mutex<NowPlaying>>,
    spotify: Box<dyn SpotifyEngine>,
    matcher: Box<dyn YoutubeMatch>,
) {
    let mut player = Player::new(
        InnertubeExtractor::new(),
        RodioSink::new(),
        spotify,
        matcher,
    );
    loop {
        match receiver.recv_timeout(Duration::from_millis(150)) {
            Ok(command) => {
                dispatch(&mut player, &snapshot, command);
                publish(&snapshot, &player);
            }
            Err(RecvTimeoutError::Timeout) => {
                let changed = player.poll();
                if changed || matches!(player.phase, Phase::Playing | Phase::Paused) {
                    publish(&snapshot, &player);
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn dispatch(
    player: &mut Player<InnertubeExtractor, RodioSink>,
    snapshot: &Mutex<NowPlaying>,
    command: Command,
) {
    match command {
        Command::PlayNow(track) => {
            if matches!(track.source, Source::YouTubeMusic | Source::Spotify) {
                write_snapshot(snapshot, loading_now(&track, &player.queue));
            }
            player.play_now(track);
        }
        Command::Play => player.play(),
        Command::Pause => player.pause(),
        Command::Stop => player.stop(),
        Command::Next => player.next(),
        Command::Previous => player.previous(),
        Command::Enqueue(track) => player.enqueue(track),
        Command::PlayNext(track) => player.play_next(track),
        Command::MoveEarlier(index) => player.move_earlier(index),
        Command::MoveLater(index) => player.move_later(index),
        Command::RemoveQueued(index) => player.remove_queued(index),
        Command::RemoveCurrent => player.remove_current(),
        Command::PlayList { tracks, index } => {
            if player.stage_list(tracks, index) {
                let mut now = player.view();
                now.can_play = false;
                now.can_pause = false;
                now.can_stop = true;
                write_snapshot(snapshot, now);
            }
            player.start_staged();
        }
    }
}

fn loading_now(track: &Track, queue: &Queue) -> NowPlaying {
    NowPlaying {
        title: track.title.clone(),
        artist: artist_line(track),
        phase: Phase::Loading,
        elapsed: Duration::ZERO,
        duration: track.duration,
        error: None,
        note: None,
        can_play: false,
        can_pause: false,
        can_stop: true,
        can_next: queue.current().is_some() || !queue.is_empty(),
        can_previous: queue.can_back(),
        source: Some(track.source),
        track_id: track.id.clone(),
        album: track.album.clone(),
        loaded: Some(track.clone()),
        upcoming: queue.upcoming().to_vec(),
    }
}

fn publish(snapshot: &Mutex<NowPlaying>, player: &Player<InnertubeExtractor, RodioSink>) {
    write_snapshot(snapshot, player.view());
}

fn write_snapshot(snapshot: &Mutex<NowPlaying>, now: NowPlaying) {
    let mut guard = snapshot
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = now;
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    use crate::extract::{AudioSource, ExtractError};

    struct FakeExtractor {
        fail: HashSet<String>,
        calls: Vec<String>,
    }

    impl Extractor for FakeExtractor {
        fn audio(&mut self, video_id: &str) -> Result<AudioSource, ExtractError> {
            self.calls.push(video_id.to_string());
            if self.fail.contains(video_id) {
                Err(ExtractError::new("stream unavailable"))
            } else {
                Ok(AudioSource {
                    mime: "audio/mp4".to_string(),
                    bytes: Vec::new(),
                })
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Fail {
        None,
        Device,
        Decode,
    }

    struct FakeSink {
        starts: u32,
        pauses: u32,
        resumes: u32,
        stops: u32,
        playing: bool,
        paused: bool,
        finished: bool,
        position: Duration,
        fail: Fail,
    }

    impl FakeSink {
        fn new(fail: Fail) -> Self {
            Self {
                starts: 0,
                pauses: 0,
                resumes: 0,
                stops: 0,
                playing: false,
                paused: false,
                finished: false,
                position: Duration::ZERO,
                fail,
            }
        }
    }

    impl AudioSink for FakeSink {
        fn start(&mut self, _source: AudioSource) -> Result<(), SinkError> {
            match self.fail {
                Fail::Device => {
                    return Err(SinkError::Device("no output device".into()));
                }
                Fail::Decode => {
                    return Err(SinkError::Decode("unrecognised format".into()));
                }
                Fail::None => {}
            }
            self.starts += 1;
            self.playing = true;
            self.paused = false;
            self.finished = false;
            self.position = Duration::ZERO;
            Ok(())
        }

        fn pause(&mut self) {
            if self.playing && !self.paused {
                self.pauses += 1;
                self.paused = true;
            }
        }

        fn resume(&mut self) {
            if self.paused {
                self.resumes += 1;
                self.paused = false;
            }
        }

        fn stop(&mut self) {
            self.stops += 1;
            self.playing = false;
            self.paused = false;
            self.finished = false;
        }

        fn elapsed(&self) -> Duration {
            if self.playing || self.paused {
                self.position
            } else {
                Duration::ZERO
            }
        }

        fn finished(&self) -> bool {
            self.finished && self.playing && !self.paused
        }
    }

    fn track(source: Source, id: &str, title: &str, artist: &str) -> Track {
        Track {
            id: id.to_string(),
            source,
            title: title.to_string(),
            artists: vec![artist.to_string()],
            album: None,
            duration: Duration::from_secs(180),
        }
    }

    fn yt(id: &str, title: &str) -> Track {
        track(Source::YouTubeMusic, id, title, "Ada Vega")
    }

    fn spotify(id: &str, title: &str) -> Track {
        track(Source::Spotify, id, title, "Ada Vega")
    }

    struct FakeSpotify {
        allow: bool,
        loads: Vec<String>,
        fail_load: bool,
        plays: u32,
        pauses: u32,
        stops: u32,
        position: Duration,
        finished: bool,
        failed: Option<String>,
        /// The next poll reports this, once. Models an audio key refusal.
        fail_playback: bool,
    }

    impl FakeSpotify {
        fn denied() -> Self {
            Self {
                allow: false,
                loads: Vec::new(),
                fail_load: false,
                plays: 0,
                pauses: 0,
                stops: 0,
                position: Duration::ZERO,
                finished: false,
                failed: None,
                fail_playback: false,
            }
        }

        fn premium() -> Self {
            let mut engine = Self::denied();
            engine.allow = true;
            engine
        }

        fn key_refused() -> Self {
            let mut engine = Self::premium();
            engine.fail_playback = true;
            engine
        }
    }

    impl SpotifyEngine for FakeSpotify {
        fn can_play(&self) -> bool {
            self.allow
        }

        fn load(&mut self, track_id: &str) -> Result<(), String> {
            self.loads.push(track_id.to_string());
            if self.fail_load {
                return Err("track refused".into());
            }
            self.finished = false;
            self.failed = None;
            self.position = Duration::ZERO;
            Ok(())
        }

        fn play(&mut self) {
            self.plays += 1;
            self.finished = false;
        }

        fn pause(&mut self) {
            self.pauses += 1;
        }

        fn stop(&mut self) {
            self.stops += 1;
            self.finished = false;
        }

        fn poll(&mut self) -> SpotifyFeed {
            SpotifyFeed {
                position: self.position,
                finished: self.finished,
                failed: self
                    .fail_playback
                    .then(|| "audio key error".to_string())
                    .or_else(|| self.failed.clone()),
                playing: false,
            }
        }
    }

    struct FakeMatch {
        track: Option<Track>,
        error: Option<String>,
        queries: Vec<String>,
    }

    impl FakeMatch {
        fn none() -> Self {
            Self {
                track: None,
                error: None,
                queries: Vec::new(),
            }
        }
    }

    struct SharedSpotify(Arc<Mutex<FakeSpotify>>);

    impl SpotifyEngine for SharedSpotify {
        fn can_play(&self) -> bool {
            self.0.lock().expect("spotify").can_play()
        }

        fn load(&mut self, track_id: &str) -> Result<(), String> {
            self.0.lock().expect("spotify").load(track_id)
        }

        fn play(&mut self) {
            self.0.lock().expect("spotify").play();
        }

        fn pause(&mut self) {
            self.0.lock().expect("spotify").pause();
        }

        fn stop(&mut self) {
            self.0.lock().expect("spotify").stop();
        }

        fn poll(&mut self) -> SpotifyFeed {
            self.0.lock().expect("spotify").poll()
        }
    }

    impl YoutubeMatch for FakeMatch {
        fn find(
            &mut self,
            title: &str,
            artists: &[String],
            _duration: Duration,
        ) -> Result<Option<Track>, String> {
            self.queries.push(match_query(title, artists));
            if let Some(error) = &self.error {
                return Err(error.clone());
            }
            Ok(self.track.clone())
        }
    }

    fn machine(fail_ids: &[&str], fail: Fail) -> Player<FakeExtractor, FakeSink> {
        Player::new(
            FakeExtractor {
                fail: fail_ids.iter().map(|id| (*id).to_string()).collect(),
                calls: Vec::new(),
            },
            FakeSink::new(fail),
            Box::new(FakeSpotify::denied()),
            Box::new(FakeMatch::none()),
        )
    }

    fn machine_with(spotify: FakeSpotify, matcher: FakeMatch) -> Player<FakeExtractor, FakeSink> {
        Player::new(
            FakeExtractor {
                fail: HashSet::new(),
                calls: Vec::new(),
            },
            FakeSink::new(Fail::None),
            Box::new(spotify),
            Box::new(matcher),
        )
    }

    #[test]
    fn play_skips_spotify_then_plays_youtube_in_queue_order() {
        let mut player = machine(&[], Fail::None);
        player.enqueue(spotify("sp-1", "Night Drive"));
        player.enqueue(yt("yt-1", "Kitchen Radio"));
        player.enqueue(yt("yt-2", "Paper Planes"));

        player.play();

        let now = player.view();
        assert_eq!(now.phase, Phase::Playing);
        assert_eq!(now.title, "Kitchen Radio");
        assert_eq!(now.artist, "Ada Vega");
        assert_eq!(now.note.as_deref(), Some(SPOTIFY_UNAVAILABLE_NOTE));
        assert!(now.error.is_none());
        assert_eq!(player.extractor.calls, ["yt-1"]);
        assert_eq!(player.sink.starts, 1);
        assert!(now.can_pause);
        assert!(now.can_next);
        assert!(!now.can_play);

        player.next();
        let now = player.view();
        assert_eq!(now.phase, Phase::Playing);
        assert_eq!(now.title, "Paper Planes");
        assert_eq!(player.extractor.calls, ["yt-1", "yt-2"]);
        assert!(now.note.is_none());
    }

    #[test]
    fn spotify_only_queue_sets_the_note_and_plays_nothing() {
        let mut player = machine(&[], Fail::None);
        player.enqueue(spotify("sp-1", "Night Drive"));
        player.enqueue(spotify("sp-2", "Glass House"));

        player.play();

        let now = player.view();
        assert_eq!(now.phase, Phase::Idle);
        assert!(now.title.is_empty());
        assert_eq!(now.note.as_deref(), Some(SPOTIFY_UNAVAILABLE_NOTE));
        assert!(player.extractor.calls.is_empty());
        assert_eq!(player.sink.starts, 0);
        assert!(!now.can_play);
    }

    #[test]
    fn pause_then_play_resumes_without_resolving_again() {
        let mut player = machine(&[], Fail::None);
        player.enqueue(yt("yt-1", "Kitchen Radio"));
        player.play();
        player.sink.position = Duration::from_secs(12);

        player.pause();
        let paused = player.view();
        assert_eq!(paused.phase, Phase::Paused);
        assert_eq!(paused.elapsed, Duration::from_secs(12));
        assert!(paused.can_play);
        assert!(paused.can_stop);
        assert!(!paused.can_pause);

        player.play();
        let resumed = player.view();
        assert_eq!(resumed.phase, Phase::Playing);
        assert_eq!(resumed.elapsed, Duration::from_secs(12));
        assert_eq!(resumed.album, None);
        assert_eq!(player.extractor.calls, ["yt-1"]);
        assert_eq!(player.sink.starts, 1);
        assert_eq!(player.sink.pauses, 1);
        assert_eq!(player.sink.resumes, 1);
    }

    #[test]
    fn stop_then_play_restarts_the_current_track() {
        let mut player = machine(&[], Fail::None);
        player.enqueue(yt("yt-1", "Kitchen Radio"));
        player.play();
        player.sink.position = Duration::from_secs(12);

        player.stop();
        let stopped = player.view();
        assert_eq!(stopped.phase, Phase::Stopped);
        assert_eq!(stopped.title, "Kitchen Radio");
        assert_eq!(stopped.elapsed, Duration::ZERO);
        assert!(stopped.can_play);
        assert!(!stopped.can_stop);

        player.play();
        assert_eq!(player.view().phase, Phase::Playing);
        assert_eq!(player.extractor.calls, ["yt-1", "yt-1"]);
        assert_eq!(player.sink.starts, 2);
        assert_eq!(player.sink.stops, 1);
    }

    #[test]
    fn next_on_the_last_track_goes_idle() {
        let mut player = machine(&[], Fail::None);
        player.enqueue(yt("yt-1", "Kitchen Radio"));
        player.play();

        player.next();

        let now = player.view();
        assert_eq!(now.phase, Phase::Idle);
        assert!(now.title.is_empty());
        assert_eq!(player.sink.stops, 1);
        assert!(!now.can_next);
    }

    #[test]
    fn next_skips_a_spotify_track_between_youtube_tracks() {
        let mut player = machine(&[], Fail::None);
        player.enqueue(yt("yt-1", "Kitchen Radio"));
        player.enqueue(spotify("sp-1", "Night Drive"));
        player.enqueue(yt("yt-2", "Paper Planes"));
        player.play();
        assert!(player.view().note.is_none());

        player.next();

        let now = player.view();
        assert_eq!(now.title, "Paper Planes");
        assert_eq!(now.phase, Phase::Playing);
        assert_eq!(now.note.as_deref(), Some(SPOTIFY_UNAVAILABLE_NOTE));
        assert_eq!(player.extractor.calls, ["yt-1", "yt-2"]);
    }

    #[test]
    fn play_now_replaces_the_current_track() {
        let mut player = machine(&[], Fail::None);
        player.enqueue(yt("yt-1", "Kitchen Radio"));
        player.enqueue(yt("yt-queued", "Still Queued"));
        player.play();

        player.play_now(yt("yt-2", "Paper Planes"));

        let now = player.view();
        assert_eq!(now.title, "Paper Planes");
        assert_eq!(now.phase, Phase::Playing);
        assert_eq!(player.extractor.calls, ["yt-1", "yt-2"]);
        assert_eq!(player.sink.stops, 1);

        player.next();
        assert_eq!(player.view().title, "Still Queued");
    }

    #[test]
    fn play_now_spotify_sets_the_note_and_leaves_playback() {
        let mut player = machine(&[], Fail::None);
        player.play_now(yt("yt-1", "Kitchen Radio"));
        player.sink.position = Duration::from_secs(4);

        player.play_now(spotify("sp-1", "Night Drive"));

        let now = player.view();
        assert_eq!(now.phase, Phase::Playing);
        assert_eq!(now.title, "Kitchen Radio");
        assert_eq!(now.elapsed, Duration::from_secs(4));
        assert_eq!(now.note.as_deref(), Some(SPOTIFY_UNAVAILABLE_NOTE));
        assert_eq!(player.sink.starts, 1);
        assert_eq!(player.sink.stops, 0);
        assert_eq!(player.extractor.calls, ["yt-1"]);
    }

    #[test]
    fn stream_error_is_shown_and_does_not_panic() {
        let mut player = machine(&["yt-bad"], Fail::None);
        player.play_now(yt("yt-bad", "Missing"));

        let now = player.view();
        assert_eq!(now.phase, Phase::Stopped);
        assert_eq!(now.title, "Missing");
        let error = now.error.expect("stream error");
        assert!(error.contains("Couldn't open the stream"), "{error}");
        assert!(error.contains("stream unavailable"), "{error}");
        assert_eq!(player.sink.starts, 0);
    }

    #[test]
    fn a_failed_stream_in_the_queue_is_skipped_for_the_next_track() {
        let mut player = machine(&["yt-bad"], Fail::None);
        player.enqueue(yt("yt-bad", "Missing"));
        player.enqueue(yt("yt-ok", "Kitchen Radio"));

        player.play();

        let now = player.view();
        assert_eq!(now.phase, Phase::Playing);
        assert_eq!(now.title, "Kitchen Radio");
        assert!(now.error.is_none());
        assert_eq!(player.extractor.calls, ["yt-bad", "yt-ok"]);
    }

    #[test]
    fn device_error_is_shown_and_does_not_try_the_next_track() {
        let mut player = machine(&[], Fail::Device);
        player.enqueue(yt("yt-1", "Kitchen Radio"));
        player.enqueue(yt("yt-2", "Paper Planes"));

        player.play();

        let now = player.view();
        assert_eq!(now.phase, Phase::Stopped);
        assert_eq!(now.title, "Kitchen Radio");
        let error = now.error.expect("device error");
        assert!(error.contains("Couldn't open the audio device"), "{error}");
        assert!(error.contains("no output device"), "{error}");
        assert_eq!(player.extractor.calls, ["yt-1"]);
        assert_eq!(player.sink.starts, 0);
    }

    #[test]
    fn decode_error_is_shown_for_the_selected_track() {
        let mut player = machine(&[], Fail::Decode);
        player.play_now(yt("yt-1", "Kitchen Radio"));

        let now = player.view();
        assert_eq!(now.phase, Phase::Stopped);
        assert_eq!(now.title, "Kitchen Radio");
        let error = now.error.expect("decode error");
        assert!(error.contains("Couldn't decode the audio"), "{error}");
        assert!(error.contains("unrecognised format"), "{error}");
    }

    #[test]
    fn local_tracks_in_the_queue_are_skipped() {
        let mut player = machine(&[], Fail::None);
        player.enqueue(track(Source::Local, "desk", "Desk", "Room"));
        player.enqueue(yt("yt-1", "Kitchen Radio"));

        player.play();

        assert_eq!(player.view().title, "Kitchen Radio");
        assert_eq!(player.view().phase, Phase::Playing);
        assert!(player.view().error.is_none());
    }

    #[test]
    fn a_local_track_alone_reports_an_error() {
        let mut player = machine(&[], Fail::None);
        player.play_now(track(Source::Local, "desk", "Desk", "Room"));

        let now = player.view();
        assert_eq!(now.phase, Phase::Idle);
        assert_eq!(
            now.error.as_deref(),
            Some("Local playback is not available yet")
        );
        assert_eq!(player.sink.starts, 0);
    }

    #[test]
    fn finished_track_advances_to_the_next_one() {
        let mut player = machine(&[], Fail::None);
        player.enqueue(yt("yt-1", "Kitchen Radio"));
        player.enqueue(yt("yt-2", "Paper Planes"));
        player.play();
        assert!(!player.poll());

        player.sink.finished = true;
        assert!(player.poll());

        let now = player.view();
        assert_eq!(now.phase, Phase::Playing);
        assert_eq!(now.title, "Paper Planes");
        assert_eq!(player.extractor.calls, ["yt-1", "yt-2"]);
    }

    #[test]
    fn idle_pause_and_stop_do_nothing() {
        let mut player = machine(&[], Fail::None);
        player.pause();
        player.stop();
        player.play();
        player.next();

        let now = player.view();
        assert_eq!(now.phase, Phase::Idle);
        assert_eq!(player.sink.starts, 0);
        assert_eq!(player.sink.stops, 0);
        assert!(now.error.is_none());
        assert!(now.note.is_none());
    }

    #[test]
    fn play_while_already_playing_does_not_restart() {
        let mut player = machine(&[], Fail::None);
        player.enqueue(yt("yt-1", "Kitchen Radio"));
        player.play();
        player.play();

        assert_eq!(player.sink.starts, 1);
        assert_eq!(player.extractor.calls, ["yt-1"]);
        assert_eq!(player.view().phase, Phase::Playing);
    }

    #[test]
    fn match_query_joins_title_and_artists() {
        assert_eq!(
            match_query(" Night Drive ", &["Ada Vega".into(), "  ".into()]),
            "Night Drive Ada Vega"
        );
        assert_eq!(match_query("   ", &[String::new()]), "");
    }

    #[test]
    fn premium_playback_replaces_the_current_track() {
        let state = Arc::new(Mutex::new(FakeSpotify::premium()));
        let mut player = Player::new(
            FakeExtractor {
                fail: HashSet::new(),
                calls: Vec::new(),
            },
            FakeSink::new(Fail::None),
            Box::new(SharedSpotify(Arc::clone(&state))),
            Box::new(FakeMatch::none()),
        );
        player.play_now(yt("yt-1", "Kitchen Radio"));
        player.sink.position = Duration::from_secs(4);

        player.play_now(spotify("5sWHDYs0csV6RS48xBl0tH", "Night Drive"));

        let now = player.view();
        assert_eq!(now.phase, Phase::Playing);
        assert_eq!(now.title, "Night Drive");
        assert_eq!(now.elapsed, Duration::ZERO);
        assert!(now.note.is_none());
        assert!(now.error.is_none());
        assert_eq!(player.sink.stops, 1);
        assert_eq!(player.extractor.calls, ["yt-1"]);
        assert_eq!(
            state.lock().expect("spotify").loads,
            ["5sWHDYs0csV6RS48xBl0tH"]
        );

        state.lock().expect("spotify").position = Duration::from_secs(9);
        assert!(!player.poll());
        assert_eq!(player.view().elapsed, Duration::from_secs(9));

        player.pause();
        assert_eq!(player.view().phase, Phase::Paused);
        assert_eq!(state.lock().expect("spotify").pauses, 1);
        player.play();
        assert_eq!(player.view().phase, Phase::Playing);
        assert_eq!(state.lock().expect("spotify").plays, 1);
        assert_eq!(state.lock().expect("spotify").loads.len(), 1);

        player.stop();
        player.play();
        assert_eq!(state.lock().expect("spotify").loads.len(), 2);
        assert_eq!(state.lock().expect("spotify").stops, 1);
    }

    #[test]
    fn a_refused_audio_key_plays_the_youtube_match() {
        let mut matcher = FakeMatch::none();
        matcher.track = Some(yt("yt-match", "Night Drive (Live)"));
        let mut player = machine_with(FakeSpotify::key_refused(), matcher);

        player.play_now(spotify("5sWHDYs0csV6RS48xBl0tH", "Night Drive"));
        assert_eq!(player.view().phase, Phase::Playing);
        assert!(player.poll());

        let now = player.view();
        assert_eq!(now.phase, Phase::Playing);
        assert_eq!(now.title, "Night Drive (Live)");
        assert_eq!(now.source, Some(Source::YouTubeMusic));
        assert_eq!(now.note.as_deref(), Some(SPOTIFY_KEY_FALLBACK_NOTE));
        assert!(now.error.is_none(), "{:?}", now.error);
        assert_eq!(player.extractor.calls, ["yt-match"]);
    }

    #[test]
    fn a_refused_audio_key_without_a_match_stays_on_the_spotify_track() {
        let mut player = machine_with(FakeSpotify::key_refused(), FakeMatch::none());
        player.enqueue(yt("yt-next", "Something Else"));
        player.play_now(spotify("5sWHDYs0csV6RS48xBl0tH", "Night Drive"));
        assert!(player.poll());

        let now = player.view();
        assert_eq!(now.phase, Phase::Stopped);
        assert_eq!(now.title, "Night Drive");
        assert_eq!(now.source, Some(Source::Spotify));
        assert_eq!(now.note.as_deref(), Some(SPOTIFY_KEY_UNAVAILABLE_NOTE));
        assert!(now.error.is_none(), "{:?}", now.error);
        assert!(player.extractor.calls.is_empty());
    }

    #[test]
    fn a_free_account_plays_the_youtube_match() {
        let mut matcher = FakeMatch::none();
        matcher.track = Some(yt("yt-match", "Night Drive (Live)"));
        let mut player = machine_with(FakeSpotify::denied(), matcher);

        player.play_now(spotify("sp-1", "Night Drive"));

        let now = player.view();
        assert_eq!(now.phase, Phase::Playing);
        assert_eq!(now.title, "Night Drive (Live)");
        assert_eq!(now.note.as_deref(), Some(SPOTIFY_FALLBACK_NOTE));
        assert_eq!(player.extractor.calls, ["yt-match"]);
        assert_eq!(player.sink.starts, 1);
    }

    #[test]
    fn premium_queue_plays_spotify_then_youtube() {
        let state = Arc::new(Mutex::new(FakeSpotify::premium()));
        let mut player = Player::new(
            FakeExtractor {
                fail: HashSet::new(),
                calls: Vec::new(),
            },
            FakeSink::new(Fail::None),
            Box::new(SharedSpotify(Arc::clone(&state))),
            Box::new(FakeMatch::none()),
        );
        player.enqueue(spotify("5sWHDYs0csV6RS48xBl0tH", "Night Drive"));
        player.enqueue(yt("yt-1", "Kitchen Radio"));

        player.play();
        assert_eq!(player.view().title, "Night Drive");
        assert_eq!(player.view().phase, Phase::Playing);
        assert!(player.view().note.is_none());
        assert!(player.extractor.calls.is_empty());

        state.lock().expect("spotify").finished = true;
        assert!(player.poll());
        assert_eq!(player.view().title, "Kitchen Radio");
        assert_eq!(player.view().phase, Phase::Playing);
        assert_eq!(player.extractor.calls, ["yt-1"]);
    }

    #[test]
    fn a_refused_spotify_track_is_reported_without_panicking() {
        let mut engine = FakeSpotify::premium();
        engine.fail_load = true;
        let mut player = machine_with(engine, FakeMatch::none());

        player.play_now(spotify("5sWHDYs0csV6RS48xBl0tH", "Night Drive"));

        let now = player.view();
        assert_eq!(now.phase, Phase::Stopped);
        assert_eq!(now.title, "Night Drive");
        let error = now.error.expect("load error");
        assert!(error.contains("Couldn't play the Spotify track"), "{error}");
        assert!(error.contains("track refused"), "{error}");
        assert_eq!(player.sink.starts, 0);
    }

    #[test]
    fn previous_returns_to_the_track_that_was_playing() {
        let mut player = machine(&[], Fail::None);
        player.enqueue(yt("yt-1", "Kitchen Radio"));
        player.enqueue(yt("yt-2", "Paper Planes"));
        player.play();
        player.next();
        assert_eq!(player.view().title, "Paper Planes");

        player.previous();

        let now = player.view();
        assert_eq!(now.phase, Phase::Playing);
        assert_eq!(now.title, "Kitchen Radio");
        assert_eq!(now.source, Some(Source::YouTubeMusic));
        assert_eq!(now.track_id, "yt-1");

        player.next();
        assert_eq!(player.view().title, "Paper Planes");
    }

    #[test]
    fn previous_with_no_history_restarts_the_current_track() {
        let mut player = machine(&[], Fail::None);
        player.play_now(yt("yt-1", "Kitchen Radio"));
        assert_eq!(player.sink.starts, 1);

        player.previous();

        assert_eq!(player.view().title, "Kitchen Radio");
        assert_eq!(player.view().phase, Phase::Playing);
        assert_eq!(player.sink.starts, 2);
        assert_eq!(player.extractor.calls, ["yt-1", "yt-1"]);
    }

    #[test]
    fn previous_while_idle_does_nothing() {
        let mut player = machine(&[], Fail::None);
        player.previous();

        assert_eq!(player.view().phase, Phase::Idle);
        assert_eq!(player.sink.starts, 0);
        assert_eq!(player.sink.stops, 0);
    }

    #[test]
    fn previous_reloads_a_spotify_track_and_returns_to_it() {
        let state = Arc::new(Mutex::new(FakeSpotify::premium()));
        let mut player = Player::new(
            FakeExtractor {
                fail: HashSet::new(),
                calls: Vec::new(),
            },
            FakeSink::new(Fail::None),
            Box::new(SharedSpotify(Arc::clone(&state))),
            Box::new(FakeMatch::none()),
        );
        player.play_now(yt("yt-1", "Kitchen Radio"));
        player.play_now(spotify("5sWHDYs0csV6RS48xBl0tH", "Night Drive"));
        assert_eq!(player.view().source, Some(Source::Spotify));
        assert_eq!(player.view().track_id, "5sWHDYs0csV6RS48xBl0tH");

        player.previous();

        let now = player.view();
        assert_eq!(now.phase, Phase::Playing);
        assert_eq!(now.title, "Kitchen Radio");
        assert_eq!(now.source, Some(Source::YouTubeMusic));
        assert_eq!(player.extractor.calls, ["yt-1", "yt-1"]);

        player.next();
        assert_eq!(player.view().title, "Night Drive");
        assert_eq!(player.view().source, Some(Source::Spotify));
        assert_eq!(
            state.lock().expect("spotify").loads,
            ["5sWHDYs0csV6RS48xBl0tH", "5sWHDYs0csV6RS48xBl0tH"]
        );

        player.previous();
        assert_eq!(player.view().title, "Kitchen Radio");
        player.next();
        assert_eq!(player.view().title, "Night Drive");
    }

    #[test]
    fn dormant_playback_accepts_commands_without_audio() {
        let playback = Playback::dormant();
        playback.play_now(yt("yt-1", "Kitchen Radio"));
        playback.play();
        playback.pause();
        playback.stop();
        playback.next();
        playback.previous();
        playback.enqueue(yt("yt-2", "Paper Planes"));
        playback.play_next(yt("yt-3", "Later"));
        playback.move_earlier(0);
        playback.move_later(0);
        playback.remove_queued(0);
        playback.remove_current();
        playback.play_list(vec![yt("yt-4", "From here")], 0);

        let now = playback.now();
        assert_eq!(now.phase, Phase::Idle);
        assert!(now.error.is_none());
        assert!(now.note.is_none());
    }

    #[test]
    fn play_next_and_remove_current_follow_the_queue() {
        let mut player = machine(&[], Fail::None);
        player.play_now(yt("yt-1", "Kitchen Radio"));
        player.enqueue(yt("yt-3", "Later"));
        player.play_next(yt("yt-2", "Paper Planes"));

        let now = player.view();
        let upcoming: Vec<_> = now.upcoming.iter().map(|track| track.id.as_str()).collect();
        assert_eq!(upcoming, ["yt-2", "yt-3"]);

        player.next();
        assert_eq!(player.view().title, "Paper Planes");
        player.previous();
        assert_eq!(player.view().title, "Kitchen Radio");
        player.next();
        assert_eq!(player.view().title, "Paper Planes");

        player.remove_current();
        assert_eq!(player.view().title, "Later");
        assert!(
            player
                .view()
                .upcoming
                .iter()
                .all(|track| track.id != "yt-2")
        );
        player.previous();
        assert_eq!(player.view().title, "Kitchen Radio");
    }

    #[test]
    fn play_from_list_starts_in_the_mixed_queue() {
        let mut player = machine_with(FakeSpotify::premium(), FakeMatch::none());
        player.play_from_list(Vec::new(), 0);
        assert_eq!(player.view().phase, Phase::Idle);
        player.play_from_list(vec![yt("yt-only", "Only")], 3);
        assert!(player.view().loaded.is_none());
        assert!(player.view().upcoming.is_empty());

        player.play_from_list(
            vec![
                yt("yt-a", "Kitchen Radio"),
                spotify("sp-b", "Night Drive"),
                yt("yt-c", "Paper Planes"),
            ],
            1,
        );
        let now = player.view();
        assert_eq!(now.phase, Phase::Playing);
        assert_eq!(now.track_id, "sp-b");
        assert_eq!(now.source, Some(Source::Spotify));
        assert_eq!(
            now.loaded.as_ref().map(|track| track.id.as_str()),
            Some("sp-b")
        );
        let upcoming: Vec<_> = now.upcoming.iter().map(|track| track.id.as_str()).collect();
        assert_eq!(upcoming, ["yt-c"]);

        player.previous();
        let now = player.view();
        assert_eq!(now.track_id, "yt-a");
        assert_eq!(
            now.loaded.as_ref().map(|track| track.source),
            Some(Source::YouTubeMusic)
        );
        let upcoming: Vec<_> = now.upcoming.iter().map(|track| track.id.as_str()).collect();
        assert_eq!(upcoming, ["sp-b", "yt-c"]);

        player.next();
        assert_eq!(player.view().track_id, "sp-b");
        assert_eq!(player.view().source, Some(Source::Spotify));
    }
}
