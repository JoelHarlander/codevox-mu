//! Play, pause, stop, skip, and return to the previous track in a mixed
//! [`Queue`](crate::music::Queue).
//!
//! The state machine is synchronous and talks to an [`AudioSink`]. Tests
//! pass a fake sink. The window uses [`Playback`], which owns the machine
//! on a thread named `playback` so stream resolution and the device stay
//! off the UI thread.
//!
//! Spotify tracks are not played. Skipping one sets
//! [`SPOTIFY_NOTE`]. Local files are skipped with an error. A failure to
//! resolve a stream or open the device is stored on the snapshot. Nothing
//! here panics for those cases.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::extract::{Extractor, InnertubeExtractor};
use crate::music::{Queue, Source, Track};
use crate::output::{AudioSink, RodioSink, SinkError};

/// Shown when a Spotify track is passed over.
pub const SPOTIFY_NOTE: &str = "Spotify playback coming next";

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
    /// Service the loaded track came from. `None` when nothing is loaded.
    pub source: Option<Source>,
    /// Source id of the loaded track. Empty when nothing is loaded.
    pub track_id: String,
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
            source: None,
            track_id: String::new(),
        }
    }
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
    /// Tracks already left, newest at the end. Previous pops this.
    history: Vec<Track>,
    current: Option<Track>,
    phase: Phase,
    duration: Duration,
    error: Option<String>,
    note: Option<String>,
    extractor: E,
    sink: S,
}

impl<E, S> Player<E, S>
where
    E: Extractor,
    S: AudioSink,
{
    pub fn new(extractor: E, sink: S) -> Self {
        Self {
            queue: Queue::new(),
            history: Vec::new(),
            current: None,
            phase: Phase::Idle,
            duration: Duration::ZERO,
            error: None,
            note: None,
            extractor,
            sink,
        }
    }

    /// Add a track behind whatever is current. The window plays a click
    /// immediately; tests use this to build a mixed queue.
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "tests fill the queue; the window plays the clicked track directly"
        )
    )]
    pub fn enqueue(&mut self, track: Track) {
        self.queue.enqueue(track);
    }

    /// Play `track` now. A YouTube track replaces whatever is audible. A
    /// Spotify track does not interrupt; it only sets [`SPOTIFY_NOTE`].
    pub fn play_now(&mut self, track: Track) {
        match track.source {
            Source::Spotify => {
                self.note = Some(SPOTIFY_NOTE.to_string());
            }
            Source::Local => {
                self.error = Some(LOCAL_UNAVAILABLE.to_string());
            }
            Source::YouTubeMusic => {
                self.halt_audio();
                if let Some(previous) = self.current.take() {
                    self.remember(previous);
                }
                self.note = None;
                self.duration = track.duration;
                self.current = Some(track);
                self.phase = Phase::Stopped;
                let _ = self.start_current();
            }
        }
    }

    /// Resume, restart the current track, or start the queue.
    pub fn play(&mut self) {
        match self.phase {
            Phase::Playing | Phase::Loading => {}
            Phase::Paused => {
                self.sink.resume();
                self.phase = Phase::Playing;
            }
            Phase::Stopped => {
                let _ = self.start_current();
            }
            Phase::Idle => self.advance(),
        }
    }

    pub fn pause(&mut self) {
        if self.phase == Phase::Playing {
            self.sink.pause();
            self.phase = Phase::Paused;
        }
    }

    /// Stop the device and keep the current track so play starts it over.
    pub fn stop(&mut self) {
        if matches!(self.phase, Phase::Playing | Phase::Paused) {
            self.sink.stop();
            self.phase = Phase::Stopped;
        }
    }

    /// Drop the current track and play the next queued one.
    pub fn next(&mut self) {
        self.halt_audio();
        if let Some(track) = self.current.take() {
            self.remember(track);
        }
        self.phase = Phase::Idle;
        self.advance();
    }

    /// Play the track heard before this one.
    ///
    /// With no earlier track, the current one starts again from the beginning.
    /// Spotify and local entries in the history are skipped, same as next.
    pub fn previous(&mut self) {
        self.halt_audio();
        let displaced = self.current.take();
        let mut skipped_spotify = false;
        while let Some(track) = self.history.pop() {
            match track.source {
                Source::YouTubeMusic => {
                    if let Some(current) = displaced {
                        self.queue.prepend(current);
                    }
                    self.note = skipped_spotify.then(|| SPOTIFY_NOTE.to_string());
                    self.duration = track.duration;
                    self.current = Some(track);
                    self.phase = Phase::Stopped;
                    let _ = self.start_current();
                    return;
                }
                Source::Spotify => skipped_spotify = true,
                Source::Local => {
                    self.error = Some(LOCAL_UNAVAILABLE.to_string());
                }
            }
        }

        self.current = displaced;
        if skipped_spotify {
            self.note = Some(SPOTIFY_NOTE.to_string());
        }
        self.restart_current();
    }

    /// When the sink has finished the current track, play the next one.
    ///
    /// Returns whether the current track changed.
    pub fn poll(&mut self) -> bool {
        if self.phase != Phase::Playing || !self.sink.finished() {
            return false;
        }
        if let Some(track) = self.current.take() {
            self.remember(track);
        }
        self.phase = Phase::Idle;
        self.advance();
        true
    }

    pub fn view(&self) -> NowPlaying {
        let title = self
            .current
            .as_ref()
            .map(|track| track.title.clone())
            .unwrap_or_default();
        let artist = self.current.as_ref().map(artist_line).unwrap_or_default();
        let source = self.current.as_ref().map(|track| track.source);
        let track_id = self
            .current
            .as_ref()
            .map(|track| track.id.clone())
            .unwrap_or_default();
        NowPlaying {
            title,
            artist,
            phase: self.phase,
            elapsed: self.elapsed(),
            duration: if self.current.is_some() {
                self.duration
            } else {
                Duration::ZERO
            },
            error: self.error.clone(),
            note: self.note.clone(),
            can_play: self.can_play(),
            can_pause: self.phase == Phase::Playing,
            can_stop: matches!(self.phase, Phase::Playing | Phase::Paused),
            can_next: self.current.is_some() || !self.queue.is_empty(),
            source,
            track_id,
        }
    }

    fn remember(&mut self, track: Track) {
        self.history.push(track);
        const LIMIT: usize = 50;
        if self.history.len() > LIMIT {
            self.history.remove(0);
        }
    }

    fn restart_current(&mut self) {
        if self.current.is_none() {
            return;
        }
        if !matches!(self.phase, Phase::Playing | Phase::Paused | Phase::Stopped) {
            return;
        }
        self.halt_audio();
        self.phase = Phase::Stopped;
        let _ = self.start_current();
    }

    fn elapsed(&self) -> Duration {
        match self.phase {
            Phase::Playing | Phase::Paused => self.sink.elapsed(),
            Phase::Idle | Phase::Loading | Phase::Stopped => Duration::ZERO,
        }
    }

    fn can_play(&self) -> bool {
        match self.phase {
            Phase::Playing | Phase::Loading => false,
            Phase::Paused | Phase::Stopped => self.current.is_some(),
            Phase::Idle => !self.queue.is_empty(),
        }
    }

    fn halt_audio(&mut self) {
        if matches!(self.phase, Phase::Playing | Phase::Paused) {
            self.sink.stop();
        }
    }

    fn start_current(&mut self) -> Start {
        let Some(track) = self.current.clone() else {
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

    fn advance(&mut self) {
        let mut skipped_spotify = false;
        loop {
            let Some(track) = self.queue.next() else {
                self.current = None;
                self.phase = Phase::Idle;
                if skipped_spotify {
                    self.note = Some(SPOTIFY_NOTE.to_string());
                }
                return;
            };

            match track.source {
                Source::Spotify => {
                    skipped_spotify = true;
                }
                Source::Local => {
                    self.error = Some(LOCAL_UNAVAILABLE.to_string());
                }
                Source::YouTubeMusic => {
                    self.duration = track.duration;
                    self.current = Some(track);
                    self.phase = Phase::Stopped;
                    match self.start_current() {
                        Start::Playing => {
                            self.note = skipped_spotify.then(|| SPOTIFY_NOTE.to_string());
                            return;
                        }
                        Start::Stop => {
                            if skipped_spotify {
                                self.note = Some(SPOTIFY_NOTE.to_string());
                            }
                            return;
                        }
                        Start::Skip => {
                            self.current = None;
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

    pub fn start() -> Self {
        let (sender, receiver) = mpsc::channel();
        let snapshot = Arc::new(Mutex::new(NowPlaying::default()));
        let shared = Arc::clone(&snapshot);
        let spawned = std::thread::Builder::new()
            .name("playback".into())
            .spawn(move || worker(receiver, shared));
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

fn worker(receiver: Receiver<Command>, snapshot: Arc<Mutex<NowPlaying>>) {
    let shared = Arc::clone(&snapshot);
    let result = catch_unwind(AssertUnwindSafe(move || {
        worker_loop(receiver, shared);
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

fn worker_loop(receiver: Receiver<Command>, snapshot: Arc<Mutex<NowPlaying>>) {
    let mut player = Player::new(InnertubeExtractor::new(), RodioSink::new());
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
            if track.source == Source::YouTubeMusic {
                let can_next = !player.queue.is_empty();
                write_snapshot(snapshot, loading_now(&track, can_next));
            }
            player.play_now(track);
        }
        Command::Play => player.play(),
        Command::Pause => player.pause(),
        Command::Stop => player.stop(),
        Command::Next => player.next(),
        Command::Previous => player.previous(),
    }
}

fn loading_now(track: &Track, can_next: bool) -> NowPlaying {
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
        can_next,
        source: Some(track.source),
        track_id: track.id.clone(),
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

    fn machine(fail_ids: &[&str], fail: Fail) -> Player<FakeExtractor, FakeSink> {
        Player::new(
            FakeExtractor {
                fail: fail_ids.iter().map(|id| (*id).to_string()).collect(),
                calls: Vec::new(),
            },
            FakeSink::new(fail),
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
        assert_eq!(now.note.as_deref(), Some(SPOTIFY_NOTE));
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
        assert_eq!(now.note.as_deref(), Some(SPOTIFY_NOTE));
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
        assert_eq!(now.note.as_deref(), Some(SPOTIFY_NOTE));
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
        assert_eq!(now.note.as_deref(), Some(SPOTIFY_NOTE));
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
    fn dormant_playback_accepts_commands_without_audio() {
        let playback = Playback::dormant();
        playback.play_now(yt("yt-1", "Kitchen Radio"));
        playback.play();
        playback.pause();
        playback.stop();
        playback.next();
        playback.previous();

        let now = playback.now();
        assert_eq!(now.phase, Phase::Idle);
        assert!(now.error.is_none());
        assert!(now.note.is_none());
    }
}
