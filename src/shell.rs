//! The native window.
//!
//! One iced window is the whole interface today. Search asks the Spotify
//! mock immediately and YouTube Music on a background thread, then keeps one
//! row per recording. Choosing a row asks the playback thread to play it.
//! The window does not know how a catalogue is fetched or how a stream is
//! decoded.

use std::sync::Arc;

use iced::alignment::{Horizontal, Vertical};
use iced::futures::channel::oneshot;
use iced::widget::{Space, button, column, container, row, scrollable, text, text_input};
use iced::window::{self, Settings};
use iced::{Background, Border, Color, Element, Fill, Shadow, Size, Subscription, Task};

use crate::music::{self, MusicService, Playlist, Source, Track};
use crate::player::{self, Phase};
use crate::search;
use crate::youtube_account::LibrarySession;
use crate::youtube_music::YouTubeMusic;

/// A blocking catalogue call. The iced executor is not a Tokio runtime, so
/// the window runs this on its own thread and waits without blocking the UI.
type BackgroundSearch = Arc<dyn Fn(&str) -> Result<Vec<Track>, music::Error> + Send + Sync>;

/// Window title. This is the product name, not a window-manager default.
const TITLE: &str = "CodeVox mU";

/// Logical pixels. Large enough that a short result list is readable on a laptop.
const WINDOW_SIZE: Size = Size::new(960.0, 640.0);

const PAGE_PADDING: f32 = 28.0;
const PANEL_PADDING: f32 = 24.0;
const PANEL_SPACING: f32 = 14.0;
const PANEL_MAX_WIDTH: f32 = 720.0;
const HEADING_SIZE: f32 = 22.0;
const BODY_SIZE: f32 = 14.0;
const META_SIZE: f32 = 12.0;
const SOURCE_SIZE: f32 = 11.0;
const RESULT_SPACING: f32 = 14.0;
const ROW_SPACING: f32 = 16.0;

/// The one accent mark on the search surface. Short, so it reads as a rule.
const ACCENT_MARK_WIDTH: f32 = 28.0;
const ACCENT_MARK_HEIGHT: f32 = 2.0;

/// `--smoke` exits once this many frames have been drawn.
///
/// One frame proves the window presented. The extra frames prove the loop
/// kept running. Zero would never exit. A frame that arrives after the exit
/// request is not counted: some platforms deliver one more before the
/// window closes.
const SMOKE_FRAME_COUNT_LIMIT: u32 = 3;

const _: () = assert!(SMOKE_FRAME_COUNT_LIMIT > 0);

/// Gives up when smoke is still running this long after the process started.
///
/// Drawing the budget is fast. This only fires when the process never
/// exits, including when the runtime ignores the exit request. It is
/// armed for `--smoke` only, so a normal session is unaffected.
const SMOKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Distinct from a usage error (`2` in `main`). A hang is a failed run.
const SMOKE_TIMEOUT_EXIT_CODE: i32 = 1;

/// How the window should live.
///
/// `Smoke` draws a bounded number of frames and exits.
/// `Run` stays open until the window is closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Run,
    Smoke,
}

#[derive(Debug, Clone)]
enum Message {
    Frame,
    QueryChanged(String),
    /// `generation` matches the query that started this search.
    YoutubeSearched {
        generation: u64,
        result: Result<Vec<Track>, music::Error>,
    },
    Play(Track),
    TogglePlayback,
    StopPlayback,
    NextTrack,
    Tick,
    ShowSignIn,
    CancelSignIn,
    CookieChanged(String),
    SubmitCookie,
    SignInFinished {
        epoch: u64,
        result: Result<bool, music::Error>,
    },
    SignOut,
    SessionCleared(Result<(), music::Error>),
    ShowLibrary,
    LibraryReady {
        epoch: u64,
        result: Result<Vec<Playlist>, music::Error>,
    },
    OpenPlaylist {
        id: String,
        name: String,
    },
    PlaylistReady {
        epoch: u64,
        result: Result<Vec<Track>, music::Error>,
    },
    ShowSearch,
    BackToLibrary,
    /// The window exists, so OS media controls can attach to it.
    BindMedia(window::Id),
    /// `hwnd` is the native handle on Windows. Other platforms pass `None`.
    MediaBound(Option<isize>),
    Media(crate::media_controls::MediaCommand),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum YtSign {
    Out,
    Busy,
    In,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pane {
    Search,
    Library,
    Playlist,
}

struct Shell {
    mode: Mode,
    frame_count: u32,
    /// Set once smoke has asked the runtime to exit.
    ///
    /// Later frames are ignored. They are not a sign that the budget was
    /// exceeded; the window can still be closing.
    exit_requested: bool,
    query: String,
    results: Vec<search::SearchRow>,
    /// YouTube Music failure for the query now on screen. Spotify rows stay.
    error: Option<String>,
    /// Bumped on every edit so a slow reply cannot overwrite a newer query.
    generation: u64,
    spotify: music::MockService,
    youtube: BackgroundSearch,
    library: Arc<dyn LibrarySession>,
    pane: Pane,
    sign: YtSign,
    cookie_draft: String,
    cookie_open: bool,
    playlists: Vec<Playlist>,
    playlist_tracks: Vec<Track>,
    playlist_name: String,
    library_loading: bool,
    /// Bumped when sign-in or a library fetch starts, so a slow reply cannot
    /// overwrite a newer one.
    epoch: u64,
    restore_started: bool,
    playback: player::Playback,
    /// A clock tick is already waiting. Keeps the 200 ms timer from stacking.
    clock_armed: bool,
    #[cfg(test)]
    last_play: Option<Track>,
}

pub fn run(mode: Mode) -> iced::Result {
    let mut application = iced::application(move || Shell::boot(mode), Shell::update, Shell::view)
        .title(TITLE)
        .theme(Shell::theme)
        .window(window_settings())
        .subscription(Shell::subscription)
        .default_font(crate::theme::FALLBACK);

    for face in crate::theme::FONTS {
        application = application.font(face);
    }

    application.run()
}

/// Size, position, and close behaviour are product choices.
///
/// Decorations, the icon, and blur stay on iced's defaults until there is a
/// reason to override that platform chrome.
fn window_settings() -> Settings {
    Settings {
        size: WINDOW_SIZE,
        position: window::Position::Centered,
        exit_on_close_request: true,
        ..Settings::default()
    }
}

/// What a smoke frame should do. Kept separate from [`Task`] so the
/// late-frame case can be tested without a window.
#[derive(Debug, PartialEq, Eq)]
enum SmokeFrame {
    Continue,
    RequestExit,
    Ignore,
}

/// Ends the process if smoke is still alive after [`SMOKE_TIMEOUT`].
///
/// The thread is detached. A normal exit returns from `main` and the
/// process dies before this wakes up. `Run` mode must not call it.
fn arm_smoke_timeout() {
    std::thread::spawn(|| {
        std::thread::sleep(SMOKE_TIMEOUT);
        eprintln!(
            "codevox-mu smoke: timed out after {}s without exiting",
            SMOKE_TIMEOUT.as_secs()
        );
        let _ = std::io::Write::flush(&mut std::io::stderr());
        std::process::exit(SMOKE_TIMEOUT_EXIT_CODE);
    });
}

#[test]
fn extra_frames_after_exit_are_ignored() {
    let mut shell = test_shell(Mode::Smoke);

    for frame in 1..SMOKE_FRAME_COUNT_LIMIT {
        assert_eq!(shell.smoke_frame(), SmokeFrame::Continue, "frame {frame}");
        assert!(!shell.exit_requested);
    }

    // The frame that meets the budget goes through `on_frame`, which is
    // where a late frame used to abort. Building the exit task must not
    // leave the process.
    let _ = shell.on_frame();
    assert!(shell.exit_requested);
    assert_eq!(shell.frame_count, SMOKE_FRAME_COUNT_LIMIT);

    for _ in 0..4 {
        let _ = shell.on_frame();
    }

    assert_eq!(shell.frame_count, SMOKE_FRAME_COUNT_LIMIT);
    assert!(shell.exit_requested);
}

#[test]
fn search_box_merges_spotify_now_and_youtube_when_it_returns() {
    let mut shell = test_shell(Mode::Run);
    let _ = shell.update(Message::QueryChanged("night".into()));

    assert_eq!(shell.query, "night");
    assert_eq!(shell.generation, 1);
    let sources: Vec<_> = shell.results.iter().map(|row| row.track.source).collect();
    assert_eq!(sources, [Source::Spotify]);
    assert!(shell.error.is_none());

    let youtube = music::MockService::youtube_music()
        .search("night")
        .expect("mock");
    let _ = shell.update(Message::YoutubeSearched {
        generation: 1,
        result: Ok(youtube),
    });
    let sources: Vec<_> = shell.results.iter().map(|row| row.track.source).collect();
    assert_eq!(sources, [Source::Spotify, Source::YouTubeMusic]);
    assert_eq!(shell.results[1].track.title, "Night Drive (Live)");
    let _ = Shell::view(&shell);

    let _ = shell.update(Message::QueryChanged("   ".into()));
    assert!(shell.results.is_empty());
    assert!(shell.error.is_none());
    let _ = Shell::view(&shell);
}

#[test]
fn youtube_failure_keeps_spotify_rows_and_shows_an_error() {
    let mut shell = test_shell(Mode::Run);
    let _ = shell.update(Message::QueryChanged("night".into()));
    let _ = shell.update(Message::YoutubeSearched {
        generation: 1,
        result: Err(music::Error::Unavailable("timed out".into())),
    });

    assert!(
        shell
            .results
            .iter()
            .all(|row| row.track.source == Source::Spotify)
    );
    assert_eq!(
        shell.error.as_deref(),
        Some("YouTube Music search failed: timed out")
    );
    let _ = Shell::view(&shell);
}

#[test]
fn a_stale_youtube_reply_does_not_replace_the_current_query() {
    let mut shell = test_shell(Mode::Run);
    let _ = shell.update(Message::QueryChanged("night".into()));
    let _ = shell.update(Message::QueryChanged("paper".into()));
    assert_eq!(shell.generation, 2);

    let night = music::MockService::youtube_music()
        .search("night")
        .expect("night");
    let _ = shell.update(Message::YoutubeSearched {
        generation: 1,
        result: Ok(night),
    });
    assert!(
        shell
            .results
            .iter()
            .all(|row| row.track.source == Source::Spotify)
    );
    assert_eq!(shell.results[0].track.title, "Paper Boats");

    let paper = music::MockService::youtube_music()
        .search("paper")
        .expect("paper");
    let _ = shell.update(Message::YoutubeSearched {
        generation: 2,
        result: Ok(paper),
    });
    let titles: Vec<_> = shell
        .results
        .iter()
        .map(|row| (row.track.source, row.track.title.as_str()))
        .collect();
    assert_eq!(
        titles,
        [
            (Source::Spotify, "Paper Boats"),
            (Source::YouTubeMusic, "Paper Planes"),
        ]
    );
}

#[test]
fn smoke_play_does_not_start_the_clock() {
    let mut shell = test_shell(Mode::Smoke);
    let track = music::MockService::youtube_music()
        .resolve("yt-kitchen-radio")
        .expect("mock track");
    let _ = shell.update(Message::Play(track));
    assert!(!shell.clock_armed);
    assert_eq!(shell.playback.phase(), Phase::Idle);
    let _ = Shell::view(&shell);
}

#[test]
fn search_box_collapses_a_youtube_duplicate_into_the_playable_row() {
    let mut shell = test_shell(Mode::Run);
    let _ = shell.update(Message::QueryChanged("night".into()));
    let youtube = vec![Track {
        id: "yt-night-drive".to_string(),
        source: Source::YouTubeMusic,
        title: "Night Drive (Official Video)".to_string(),
        artists: vec!["Ada Vega".to_string()],
        album: None,
        duration: std::time::Duration::from_secs(194),
    }];
    let _ = shell.update(Message::YoutubeSearched {
        generation: 1,
        result: Ok(youtube),
    });

    assert_eq!(shell.results.len(), 1);
    assert_eq!(shell.results[0].track.id, "yt-night-drive");
    assert_eq!(shell.results[0].track.source, Source::YouTubeMusic);
    assert_eq!(shell.results[0].also[0].source, Source::Spotify);
    assert_eq!(shell.results[0].also[0].id, "sp-night-drive");
    let _ = Shell::view(&shell);
}

#[test]
fn a_cookie_without_sapisid_stays_signed_out_and_hides_the_paste() {
    let mut shell = test_shell(Mode::Run);
    let _ = shell.update(Message::ShowSignIn);
    assert!(shell.cookie_open);
    shell.cookie_draft = "VISITOR=MARKER-42".into();
    let _ = shell.update(Message::SubmitCookie);
    let error = shell.error.clone().expect("sign-in error");
    assert!(error.contains("SAPISID"), "{error}");
    assert!(!error.contains("MARKER-42"), "{error}");
    assert_eq!(shell.sign, YtSign::Out);
    let _ = Shell::view(&shell);
}

#[test]
fn the_library_stays_closed_until_sign_in() {
    let mut shell = test_shell(Mode::Run);
    let _ = shell.update(Message::ShowLibrary);
    assert_eq!(shell.pane, Pane::Search);
    let error = shell.error.expect("prompt");
    assert!(error.contains("Sign in"), "{error}");
}

#[test]
fn a_playlist_track_plays_through_the_same_message_as_search() {
    let mut shell = test_shell(Mode::Run);
    shell.cookie_draft = "SAPISID=test-value;".into();
    let _ = shell.update(Message::SubmitCookie);
    assert_eq!(shell.sign, YtSign::Busy);
    let epoch = shell.epoch;
    let _ = shell.update(Message::SignInFinished {
        epoch,
        result: Ok(true),
    });
    shell
        .library
        .sign_in("SAPISID=test-value;")
        .expect("the service accepts the same cookie");
    assert_eq!(shell.sign, YtSign::In);
    assert!(shell.cookie_draft.is_empty());

    let _ = shell.update(Message::ShowLibrary);
    assert_eq!(shell.pane, Pane::Library);
    let epoch = shell.epoch;
    let _ = shell.update(Message::LibraryReady {
        epoch,
        result: Ok(vec![Playlist {
            id: "PLnight".into(),
            source: Source::YouTubeMusic,
            name: "Night".into(),
        }]),
    });
    assert_eq!(shell.playlists[0].name, "Night");

    let _ = shell.update(Message::OpenPlaylist {
        id: "PLnight".into(),
        name: "Night".into(),
    });
    let track = Track {
        id: "abcdefghijk".into(),
        source: Source::YouTubeMusic,
        title: "Night Drive".into(),
        artists: vec!["Ada Vega".into()],
        album: None,
        duration: std::time::Duration::from_secs(200),
    };
    let epoch = shell.epoch;
    let _ = shell.update(Message::PlaylistReady {
        epoch,
        result: Ok(vec![track.clone()]),
    });
    assert_eq!(shell.playlist_tracks, vec![track.clone()]);
    assert!(matches!(play_message(&track), Message::Play(played) if played == track));
    let _ = shell.update(play_message(&track));
    assert_eq!(shell.last_play.as_ref(), Some(&track));
    assert_eq!(shell.playback.phase(), Phase::Idle);

    let stale = shell.epoch;
    let _ = shell.update(Message::BackToLibrary);
    assert_eq!(shell.pane, Pane::Library);
    let _ = shell.update(Message::PlaylistReady {
        epoch: stale,
        result: Ok(vec![track]),
    });
    assert!(shell.playlist_tracks.is_empty());
    let _ = Shell::view(&shell);
}

#[test]
fn a_late_sign_in_does_not_undo_sign_out() {
    let mut shell = test_shell(Mode::Run);
    shell.cookie_draft = "SAPISID=test-value;".into();
    let _ = shell.update(Message::SubmitCookie);
    let epoch = shell.epoch;
    let _ = shell.update(Message::SignOut);
    let _ = shell.update(Message::SignInFinished {
        epoch,
        result: Ok(true),
    });
    assert_eq!(shell.sign, YtSign::Out);
    assert_eq!(shell.pane, Pane::Search);
    assert!(shell.playlists.is_empty());
}

#[cfg(test)]
struct FakeLibrary {
    signed_in: std::sync::Mutex<bool>,
    playlists: Vec<Playlist>,
    tracks: Vec<Track>,
}

#[cfg(test)]
impl FakeLibrary {
    fn empty() -> Self {
        Self {
            signed_in: std::sync::Mutex::new(false),
            playlists: Vec::new(),
            tracks: Vec::new(),
        }
    }
}

#[cfg(test)]
impl LibrarySession for FakeLibrary {
    fn signed_in(&self) -> bool {
        *self.signed_in.lock().expect("sign-in")
    }

    fn sign_in(&self, cookie: &str) -> Result<(), music::Error> {
        crate::youtube_account::normalize_session_cookie(cookie)?;
        *self.signed_in.lock().expect("sign-in") = true;
        Ok(())
    }

    fn restore(&self) -> Result<bool, music::Error> {
        Ok(false)
    }

    fn sign_out(&self) -> Result<(), music::Error> {
        *self.signed_in.lock().expect("sign-in") = false;
        Ok(())
    }

    fn playlists(&self) -> Result<Vec<Playlist>, music::Error> {
        if self.signed_in() {
            Ok(self.playlists.clone())
        } else {
            Err(music::Error::Unavailable(
                "Sign in to YouTube Music to see your library.".into(),
            ))
        }
    }

    fn playlist_tracks(&self, playlist_id: &str) -> Result<Vec<Track>, music::Error> {
        if playlist_id == "PLnight" {
            Ok(self.tracks.clone())
        } else {
            Err(music::Error::NotFound)
        }
    }
}

#[cfg(test)]
fn test_shell(mode: Mode) -> Shell {
    let youtube = music::MockService::youtube_music();
    // Tests never poll the task, and the closure is the mock if they do.
    Shell::with_youtube(
        mode,
        Arc::new(move |query| youtube.search(query)),
        Arc::new(FakeLibrary::empty()),
    )
}

impl Shell {
    fn boot(mode: Mode) -> Self {
        if mode == Mode::Smoke {
            arm_smoke_timeout();
        }

        let youtube = YouTubeMusic::new();
        let library: Arc<dyn LibrarySession> = Arc::new(youtube.clone());
        let mut shell =
            Self::with_youtube(mode, Arc::new(move |query| youtube.search(query)), library);
        // Smoke must not open a device or resolve a stream. The dormant
        // handle drops every command.
        if mode == Mode::Run {
            shell.playback = player::Playback::start();
        }
        shell
    }

    fn with_youtube(
        mode: Mode,
        youtube: BackgroundSearch,
        library: Arc<dyn LibrarySession>,
    ) -> Self {
        Self {
            mode,
            frame_count: 0,
            exit_requested: false,
            query: String::new(),
            results: Vec::new(),
            error: None,
            generation: 0,
            spotify: music::MockService::spotify(),
            youtube,
            library,
            pane: Pane::Search,
            sign: YtSign::Out,
            cookie_draft: String::new(),
            cookie_open: false,
            playlists: Vec::new(),
            playlist_tracks: Vec::new(),
            playlist_name: String::new(),
            library_loading: false,
            epoch: 0,
            restore_started: false,
            playback: player::Playback::dormant(),
            clock_armed: false,
            #[cfg(test)]
            last_play: None,
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Frame => self.on_frame(),
            Message::QueryChanged(query) => self.set_query(query),
            Message::YoutubeSearched { generation, result } => {
                self.apply_youtube(generation, result);
                Task::none()
            }
            Message::Play(track) => {
                #[cfg(test)]
                {
                    self.last_play = Some(track.clone());
                }
                self.playback.play_now(track);
                self.arm_clock()
            }
            Message::TogglePlayback => {
                if self.playback.phase() == Phase::Playing {
                    self.playback.pause();
                } else {
                    self.playback.play();
                }
                self.arm_clock()
            }
            Message::StopPlayback => {
                self.playback.stop();
                self.arm_clock()
            }
            Message::NextTrack => {
                self.playback.next();
                self.arm_clock()
            }
            Message::Tick => self.on_tick(),
            Message::ShowSignIn => {
                self.cookie_open = true;
                self.error = None;
                Task::none()
            }
            Message::CancelSignIn => {
                self.cookie_open = false;
                self.cookie_draft.clear();
                Task::none()
            }
            Message::CookieChanged(cookie) => {
                self.cookie_draft = cookie;
                Task::none()
            }
            Message::SubmitCookie => self.submit_cookie(),
            Message::SignInFinished { epoch, result } => {
                self.finish_sign_in(epoch, result);
                Task::none()
            }
            Message::SignOut => self.sign_out(),
            Message::SessionCleared(Err(error)) => {
                self.error = Some(format!("YouTube Music sign-out failed: {error}"));
                Task::none()
            }
            Message::SessionCleared(Ok(())) => Task::none(),
            Message::ShowLibrary => self.show_library(),
            Message::LibraryReady { epoch, result } => {
                self.finish_library(epoch, result);
                Task::none()
            }
            Message::OpenPlaylist { id, name } => self.open_playlist(id, name),
            Message::PlaylistReady { epoch, result } => {
                self.finish_playlist(epoch, result);
                Task::none()
            }
            Message::ShowSearch => {
                self.epoch = self.epoch.wrapping_add(1);
                self.back_to_search();
                Task::none()
            }
            Message::BackToLibrary => {
                self.epoch = self.epoch.wrapping_add(1);
                self.pane = Pane::Library;
                self.playlist_tracks.clear();
                self.playlist_name.clear();
                self.library_loading = false;
                self.error = None;
                Task::none()
            }
            Message::BindMedia(id) => Task::batch([
                crate::media_controls::read_handle(id).map(Message::MediaBound),
                self.restore_saved_sign_in(),
            ]),
            Message::MediaBound(hwnd) => {
                crate::media_controls::attach(hwnd);
                Task::none()
            }
            Message::Media(command) => {
                crate::media_controls::drive(&self.playback, command);
                self.arm_clock()
            }
        }
    }

    /// Redraw while a track is loading or playing. One sleeping thread per
    /// tick, same shape as search, because the window executor is not Tokio.
    fn arm_clock(&mut self) -> Task<Message> {
        if self.mode == Mode::Run {
            crate::media_controls::publish(&self.playback.now());
        }
        if self.mode != Mode::Run || self.clock_armed {
            return Task::none();
        }
        self.clock_armed = true;
        Task::perform(wait(std::time::Duration::from_millis(200)), |_| {
            Message::Tick
        })
    }

    fn on_tick(&mut self) -> Task<Message> {
        self.clock_armed = false;
        crate::media_controls::publish(&self.playback.now());
        match self.playback.phase() {
            Phase::Loading | Phase::Playing => self.arm_clock(),
            Phase::Idle | Phase::Paused | Phase::Stopped => Task::none(),
        }
    }

    fn submit_cookie(&mut self) -> Task<Message> {
        if let Err(error) = crate::youtube_account::normalize_session_cookie(&self.cookie_draft) {
            self.sign = YtSign::Out;
            self.error = Some(format!("YouTube Music sign-in failed: {error}"));
            return Task::none();
        }
        self.sign = YtSign::Busy;
        self.error = None;
        self.epoch = self.epoch.wrapping_add(1);
        let epoch = self.epoch;
        let library = Arc::clone(&self.library);
        let cookie = self.cookie_draft.clone();
        self.off_ui(
            "youtube-music-sign-in",
            move || library.sign_in(&cookie).map(|()| true),
            move |result| Message::SignInFinished { epoch, result },
        )
    }

    fn finish_sign_in(&mut self, epoch: u64, result: Result<bool, music::Error>) {
        if epoch != self.epoch {
            return;
        }
        match result {
            Ok(true) => {
                self.sign = YtSign::In;
                self.cookie_draft.clear();
                self.cookie_open = false;
                self.error = None;
            }
            Ok(false) => {
                if self.sign == YtSign::Busy {
                    self.sign = YtSign::Out;
                }
            }
            Err(error) => {
                self.sign = YtSign::Out;
                self.cookie_open = true;
                self.error = Some(format!("YouTube Music sign-in failed: {error}"));
            }
        }
    }

    fn sign_out(&mut self) -> Task<Message> {
        self.epoch = self.epoch.wrapping_add(1);
        self.sign = YtSign::Out;
        self.back_to_search();
        self.cookie_draft.clear();
        self.cookie_open = false;
        let library = Arc::clone(&self.library);
        self.off_ui(
            "youtube-music-sign-out",
            move || library.sign_out(),
            Message::SessionCleared,
        )
    }

    fn show_library(&mut self) -> Task<Message> {
        if self.sign != YtSign::In || !self.library.signed_in() {
            self.sign = YtSign::Out;
            self.error = Some("Sign in to YouTube Music to see your library.".to_string());
            return Task::none();
        }
        self.pane = Pane::Library;
        self.library_loading = true;
        self.error = None;
        self.epoch = self.epoch.wrapping_add(1);
        let epoch = self.epoch;
        let library = Arc::clone(&self.library);
        self.off_ui(
            "youtube-music-library",
            move || library.playlists(),
            move |result| Message::LibraryReady { epoch, result },
        )
    }

    fn finish_library(&mut self, epoch: u64, result: Result<Vec<Playlist>, music::Error>) {
        if epoch != self.epoch || self.pane != Pane::Library {
            return;
        }
        self.library_loading = false;
        match result {
            Ok(playlists) => {
                self.playlists = playlists;
                self.error = None;
            }
            Err(error) => {
                self.playlists.clear();
                self.error = Some(format!("YouTube Music library failed: {error}"));
            }
        }
    }

    fn open_playlist(&mut self, id: String, name: String) -> Task<Message> {
        self.pane = Pane::Playlist;
        self.playlist_name = name;
        self.playlist_tracks.clear();
        self.library_loading = true;
        self.error = None;
        self.epoch = self.epoch.wrapping_add(1);
        let epoch = self.epoch;
        let library = Arc::clone(&self.library);
        self.off_ui(
            "youtube-music-playlist",
            move || library.playlist_tracks(&id),
            move |result| Message::PlaylistReady { epoch, result },
        )
    }

    fn finish_playlist(&mut self, epoch: u64, result: Result<Vec<Track>, music::Error>) {
        if epoch != self.epoch || self.pane != Pane::Playlist {
            return;
        }
        self.library_loading = false;
        match result {
            Ok(tracks) => {
                self.playlist_tracks = tracks;
                self.error = None;
            }
            Err(error) => {
                self.playlist_tracks.clear();
                self.error = Some(format!("YouTube Music playlist failed: {error}"));
            }
        }
    }

    fn back_to_search(&mut self) {
        self.pane = Pane::Search;
        self.playlists.clear();
        self.playlist_tracks.clear();
        self.playlist_name.clear();
        self.library_loading = false;
    }

    fn restore_saved_sign_in(&mut self) -> Task<Message> {
        if self.mode != Mode::Run || self.restore_started {
            return Task::none();
        }
        self.restore_started = true;
        self.epoch = self.epoch.wrapping_add(1);
        let epoch = self.epoch;
        let library = Arc::clone(&self.library);
        self.off_ui(
            "youtube-music-restore",
            move || library.restore(),
            move |result| Message::SignInFinished { epoch, result },
        )
    }

    fn off_ui<T, F, M>(&self, thread_name: &'static str, work: F, message: M) -> Task<Message>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, music::Error> + Send + 'static,
        M: FnOnce(Result<T, music::Error>) -> Message + Send + 'static,
    {
        Task::perform(
            async move {
                let (sender, receiver) = oneshot::channel();
                let started =
                    std::thread::Builder::new()
                        .name(thread_name.into())
                        .spawn(move || {
                            let _ = sender.send(work());
                        });
                if let Err(error) = started {
                    return Err(music::Error::Unavailable(format!(
                        "could not start the YouTube Music thread: {error}"
                    )));
                }
                receiver.await.unwrap_or_else(|_| {
                    Err(music::Error::Unavailable(
                        "the YouTube Music thread stopped before it replied".into(),
                    ))
                })
            },
            message,
        )
    }

    fn set_query(&mut self, query: String) -> Task<Message> {
        self.query = query;
        self.generation += 1;
        let generation = self.generation;
        self.error = None;

        if self.query.trim().is_empty() {
            self.results.clear();
            return Task::none();
        }

        // Spotify is memory. YouTube blocks, so it rides an iced task.
        // Until that reply lands, the list is the Spotify ranking alone.
        let spotify_tracks = self.spotify.search(&self.query).unwrap_or_default();
        self.results = search_rows(&spotify_tracks, &[]);

        let youtube = Arc::clone(&self.youtube);
        let query = self.query.clone();
        Task::perform(
            async move {
                let (sender, receiver) = oneshot::channel();
                let started = std::thread::Builder::new()
                    .name("youtube-music-search".into())
                    .spawn(move || {
                        let _ = sender.send(youtube(&query));
                    });
                if let Err(error) = started {
                    return Err(music::Error::Unavailable(format!(
                        "could not start the search thread: {error}"
                    )));
                }
                receiver.await.unwrap_or_else(|_| {
                    Err(music::Error::Unavailable(
                        "the search thread stopped before it replied".into(),
                    ))
                })
            },
            move |result| Message::YoutubeSearched { generation, result },
        )
    }

    fn apply_youtube(&mut self, generation: u64, result: Result<Vec<Track>, music::Error>) {
        if generation != self.generation || self.query.trim().is_empty() {
            return;
        }

        let youtube_tracks = match result {
            Ok(tracks) => {
                self.error = None;
                tracks
            }
            Err(error) => {
                self.error = Some(format!("YouTube Music search failed: {error}"));
                Vec::new()
            }
        };

        let spotify_tracks = self.spotify.search(&self.query).unwrap_or_default();
        self.results = search_rows(&spotify_tracks, &youtube_tracks);
    }

    fn on_frame(&mut self) -> Task<Message> {
        // Frames are only subscribed in smoke mode. A frame in any other
        // mode means the subscription and the mode have drifted.
        assert!(matches!(self.mode, Mode::Smoke));

        match self.smoke_frame() {
            SmokeFrame::Continue | SmokeFrame::Ignore => Task::none(),
            SmokeFrame::RequestExit => {
                eprintln!("codevox-mu smoke: rendered {SMOKE_FRAME_COUNT_LIMIT} frames, exiting");
                iced::exit()
            }
        }
    }

    /// One delivered smoke frame.
    ///
    /// The exit request happens once. A frame that shows up afterwards is
    /// ignored, so a slow close cannot trip a frame-budget assertion.
    fn smoke_frame(&mut self) -> SmokeFrame {
        if self.exit_requested {
            return SmokeFrame::Ignore;
        }

        self.frame_count += 1;

        if self.frame_count < SMOKE_FRAME_COUNT_LIMIT {
            SmokeFrame::Continue
        } else {
            assert_eq!(self.frame_count, SMOKE_FRAME_COUNT_LIMIT);
            self.exit_requested = true;
            SmokeFrame::RequestExit
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        match self.mode {
            // Smoke must not open a D-Bus session or a media-controls handle.
            Mode::Smoke => window::frames().map(|_| Message::Frame),
            Mode::Run => Subscription::batch([
                crate::media_controls::events().map(Message::Media),
                window::open_events().map(Message::BindMedia),
            ]),
        }
    }

    fn theme(_shell: &Self) -> iced::Theme {
        crate::theme::theme()
    }

    fn view(shell: &Self) -> Element<'_, Message> {
        let heading = text(TITLE)
            .font(crate::theme::HEADING)
            .size(HEADING_SIZE)
            .color(crate::theme::TEXT_PRIMARY);
        let accent = container(
            Space::new()
                .width(ACCENT_MARK_WIDTH)
                .height(ACCENT_MARK_HEIGHT),
        )
        .style(accent_mark);
        let input = text_input("Search tracks", &shell.query)
            .on_input(Message::QueryChanged)
            .padding([10.0, 12.0])
            .size(BODY_SIZE)
            .font(crate::theme::BODY)
            .style(search_field);

        let list = match shell.pane {
            Pane::Search => column(shell.results.iter().map(result_row)),
            Pane::Library => column(shell.playlists.iter().map(playlist_row)),
            Pane::Playlist => column(shell.playlist_tracks.iter().map(library_track_row)),
        }
        .spacing(RESULT_SPACING)
        .width(Fill);
        let mut content = column![heading, accent, account_controls(shell)].spacing(PANEL_SPACING);
        if shell.pane == Pane::Search {
            content = content.push(input);
        }
        if shell.cookie_open && shell.sign == YtSign::Out {
            content = content.push(cookie_form(shell));
        }
        if shell.pane == Pane::Playlist && !shell.playlist_name.is_empty() {
            content = content.push(
                text(&shell.playlist_name)
                    .font(crate::theme::BODY)
                    .size(BODY_SIZE)
                    .color(crate::theme::TEXT_PRIMARY),
            );
        }
        if let Some(status) = status_line(shell) {
            content = content.push(status);
        }

        let content = content
            .push(scrollable(list).width(Fill).height(Fill))
            .push(now_playing(shell))
            .width(Fill)
            .height(Fill);
        let panel = container(content)
            .padding(PANEL_PADDING)
            .width(Fill)
            .height(Fill)
            .max_width(PANEL_MAX_WIDTH)
            .style(crate::theme::panel);

        container(panel)
            .padding(PAGE_PADDING)
            .width(Fill)
            .height(Fill)
            .align_x(Horizontal::Center)
            .into()
    }
}

fn status_line(shell: &Shell) -> Option<Element<'_, Message>> {
    if let Some(error) = &shell.error {
        return Some(
            text(error)
                .font(crate::theme::BODY)
                .size(META_SIZE)
                .color(crate::theme::DESTRUCTIVE)
                .into(),
        );
    }

    let note = match shell.pane {
        Pane::Search if !shell.query.trim().is_empty() && shell.results.is_empty() => "No matches",
        Pane::Library if !shell.library_loading && shell.playlists.is_empty() => "No playlists",
        Pane::Playlist if !shell.library_loading && shell.playlist_tracks.is_empty() => {
            "This playlist is empty."
        }
        _ => return None,
    };

    Some(
        text(note)
            .font(crate::theme::BODY)
            .size(META_SIZE)
            .color(crate::theme::TEXT_SECONDARY)
            .into(),
    )
}

fn account_controls(shell: &Shell) -> Element<'_, Message> {
    let controls = match (shell.pane, shell.sign, shell.cookie_open) {
        (Pane::Search, YtSign::In, _) => row![
            transport_button("Library", Some(Message::ShowLibrary), true),
            transport_button("Sign out", Some(Message::SignOut), false),
        ],
        (Pane::Search, YtSign::Busy, _) => {
            return text("Signing in to YouTube Music…")
                .font(crate::theme::BODY)
                .size(META_SIZE)
                .color(crate::theme::TEXT_SECONDARY)
                .into();
        }
        (Pane::Search, YtSign::Out, false) => {
            row![transport_button("Sign in", Some(Message::ShowSignIn), true)]
        }
        (Pane::Library, _, _) => row![
            transport_button("Back", Some(Message::ShowSearch), false),
            transport_button("Sign out", Some(Message::SignOut), false),
        ],
        (Pane::Playlist, _, _) => row![
            transport_button("Back", Some(Message::BackToLibrary), false),
            transport_button("Sign out", Some(Message::SignOut), false),
        ],
        (Pane::Search, YtSign::Out, true) => row![],
    };
    controls.spacing(8).into()
}

fn cookie_form(shell: &Shell) -> Element<'_, Message> {
    let help = text(
        "Copy the Cookie header from music.youtube.com while you are signed in. It has to include SAPISID.",
    )
    .font(crate::theme::BODY)
    .size(META_SIZE)
    .color(crate::theme::TEXT_SECONDARY);
    let input = text_input("Cookie header", &shell.cookie_draft)
        .on_input(Message::CookieChanged)
        .secure(true)
        .padding([10.0, 12.0])
        .size(BODY_SIZE)
        .font(crate::theme::BODY)
        .style(search_field);
    let buttons = row![
        transport_button("Continue", Some(Message::SubmitCookie), true),
        transport_button("Cancel", Some(Message::CancelSignIn), false),
    ]
    .spacing(8);
    column![help, input, buttons].spacing(8).into()
}

/// Spotify first, then YouTube Music, interleaved by each service's rank.
///
/// YouTube Music is the source a click can play. A duplicate keeps that
/// track, and the other service's id, for a later fallback.
fn search_rows(spotify: &[Track], youtube: &[Track]) -> Vec<search::SearchRow> {
    search::merge_results(&[spotify, youtube], Source::YouTubeMusic)
}

fn play_message(track: &Track) -> Message {
    Message::Play(track.clone())
}

fn result_row(row: &search::SearchRow) -> Element<'_, Message> {
    track_button(&row.track)
}

fn library_track_row(track: &Track) -> Element<'_, Message> {
    track_button(track)
}

fn track_button(track: &Track) -> Element<'_, Message> {
    button(result_body(track))
        .on_press(play_message(track))
        .width(Fill)
        .padding([6.0, 0.0])
        .style(result_button)
        .into()
}

fn playlist_row(playlist: &Playlist) -> Element<'_, Message> {
    button(
        text(&playlist.name)
            .font(crate::theme::BODY)
            .size(BODY_SIZE)
            .color(crate::theme::TEXT_PRIMARY),
    )
    .on_press(Message::OpenPlaylist {
        id: playlist.id.clone(),
        name: playlist.name.clone(),
    })
    .width(Fill)
    .padding([6.0, 0.0])
    .style(result_button)
    .into()
}

fn result_body(track: &Track) -> Element<'_, Message> {
    let mut details = column![
        text(&track.title)
            .font(crate::theme::BODY)
            .size(BODY_SIZE)
            .color(crate::theme::TEXT_PRIMARY)
    ]
    .spacing(2)
    .width(Fill);

    let subtitle = subtitle(track);
    if !subtitle.is_empty() {
        details = details.push(
            text(subtitle)
                .font(crate::theme::BODY)
                .size(META_SIZE)
                .color(crate::theme::TEXT_SECONDARY),
        );
    }

    let (_, foreground) = source_colors(track.source);
    let tag = container(
        text(track.source.label())
            .font(crate::theme::BODY)
            .size(SOURCE_SIZE)
            .color(foreground),
    )
    .padding([3.0, 7.0])
    .style(source_chip(track.source));

    row![
        details,
        text(format_duration(track.duration))
            .font(crate::theme::BODY)
            .size(META_SIZE)
            .color(crate::theme::TEXT_TERTIARY),
        tag,
    ]
    .spacing(ROW_SPACING)
    .align_y(Vertical::Center)
    .width(Fill)
    .into()
}

fn now_playing(shell: &Shell) -> Element<'_, Message> {
    let now = shell.playback.now();
    let idle = now.title.is_empty();
    let title = if idle {
        "Nothing playing".to_string()
    } else {
        now.title.clone()
    };
    let title_color = if idle {
        crate::theme::TEXT_SECONDARY
    } else {
        crate::theme::TEXT_PRIMARY
    };

    let mut details = column![
        text(title)
            .font(crate::theme::BODY)
            .size(BODY_SIZE)
            .color(title_color)
    ]
    .spacing(2)
    .width(Fill);

    if !now.artist.is_empty() {
        details = details.push(
            text(now.artist.clone())
                .font(crate::theme::BODY)
                .size(META_SIZE)
                .color(crate::theme::TEXT_SECONDARY),
        );
    }
    if let Some(note) = now.note.clone() {
        details = details.push(
            text(note)
                .font(crate::theme::BODY)
                .size(META_SIZE)
                .color(crate::theme::WARNING),
        );
    }
    if let Some(error) = now.error.clone() {
        details = details.push(
            text(error)
                .font(crate::theme::BODY)
                .size(META_SIZE)
                .color(crate::theme::DESTRUCTIVE),
        );
    }

    let time = match now.phase {
        Phase::Loading => "Loading".to_string(),
        Phase::Idle if idle => String::new(),
        _ => format!(
            "{} / {}",
            format_duration(now.elapsed),
            format_duration(now.duration)
        ),
    };

    let (play_label, play_message) = if now.can_pause {
        ("Pause", Some(Message::TogglePlayback))
    } else if now.can_play {
        ("Play", Some(Message::TogglePlayback))
    } else {
        ("Play", None)
    };

    let controls = row![
        text(time)
            .font(crate::theme::BODY)
            .size(META_SIZE)
            .color(crate::theme::TEXT_TERTIARY),
        transport_button(play_label, play_message, true),
        transport_button("Next", now.can_next.then_some(Message::NextTrack), false,),
        transport_button("Stop", now.can_stop.then_some(Message::StopPlayback), false,),
    ]
    .spacing(8)
    .align_y(Vertical::Center);

    container(
        row![details, controls]
            .spacing(ROW_SPACING)
            .align_y(Vertical::Center),
    )
    .width(Fill)
    .padding([10.0, 12.0])
    .style(transport_bar)
    .into()
}

fn transport_button(
    label: &'static str,
    message: Option<Message>,
    primary: bool,
) -> Element<'static, Message> {
    button(text(label).font(crate::theme::BODY).size(META_SIZE))
        .on_press_maybe(message)
        .padding([6.0, 10.0])
        .style(move |_theme, status| transport_button_style(status, primary))
        .into()
}

fn transport_bar(_theme: &iced::Theme) -> container::Style {
    container::Style {
        text_color: Some(crate::theme::TEXT_PRIMARY),
        background: Some(crate::theme::DARK_LIGHT.into()),
        border: Border {
            color: crate::theme::HAIRLINE,
            width: 1.0,
            radius: 0.0.into(),
        },
        shadow: Shadow {
            blur_radius: crate::theme::GLOW,
            ..Shadow::default()
        },
        ..container::Style::default()
    }
}

fn transport_button_style(status: button::Status, primary: bool) -> button::Style {
    let (background, text_color) = if primary {
        (crate::theme::ACCENT, crate::theme::ON_ACCENT)
    } else {
        (crate::theme::DARK_LIGHTER, crate::theme::TEXT_PRIMARY)
    };
    let (background, text_color) = match status {
        button::Status::Disabled => (crate::theme::DARK_LIGHT, crate::theme::TEXT_TERTIARY),
        button::Status::Hovered | button::Status::Pressed if !primary => {
            (crate::theme::DARK_LIGHTER, crate::theme::TEXT_PRIMARY)
        }
        button::Status::Hovered | button::Status::Pressed => (background, text_color),
        button::Status::Active => (background, text_color),
    };

    button::Style {
        background: Some(Background::Color(background)),
        text_color,
        border: Border {
            color: crate::theme::HAIRLINE,
            width: 1.0,
            radius: 0.0.into(),
        },
        shadow: Shadow {
            blur_radius: crate::theme::GLOW,
            ..Shadow::default()
        },
        snap: false,
    }
}

fn result_button(_theme: &iced::Theme, status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered | button::Status::Pressed => {
            Some(Background::Color(crate::theme::DARK_LIGHTER))
        }
        button::Status::Active | button::Status::Disabled => None,
    };
    button::Style {
        background,
        text_color: crate::theme::TEXT_PRIMARY,
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: 0.0.into(),
        },
        shadow: Shadow {
            blur_radius: crate::theme::GLOW,
            ..Shadow::default()
        },
        snap: false,
    }
}

async fn wait(duration: std::time::Duration) {
    let (sender, receiver) = oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("playback-clock".into())
        .spawn(move || {
            std::thread::sleep(duration);
            let _ = sender.send(());
        });
    if spawned.is_err() {
        return;
    }
    let _ = receiver.await;
}

fn subtitle(track: &Track) -> String {
    let artists = track.artists.join(", ");
    match (artists.is_empty(), track.album.as_deref()) {
        (true, None) => String::new(),
        (true, Some(album)) => album.to_string(),
        (false, None) => artists,
        (false, Some(album)) => format!("{artists} - {album}"),
    }
}

fn format_duration(duration: std::time::Duration) -> String {
    let seconds = duration.as_secs();
    let minutes = seconds / 60;
    let remainder = seconds % 60;
    format!("{minutes}:{remainder:02}")
}

fn source_colors(source: Source) -> (Color, Color) {
    match source {
        Source::Spotify => (crate::theme::ACCENT, crate::theme::ON_ACCENT),
        Source::YouTubeMusic => (crate::theme::DARK_LIGHTER, crate::theme::TEXT_PRIMARY),
        Source::Local => (crate::theme::DARK_LIGHT, crate::theme::TEXT_SECONDARY),
    }
}

fn source_chip(source: Source) -> impl Fn(&iced::Theme) -> container::Style {
    let (background, foreground) = source_colors(source);
    let border_color = match source {
        Source::Spotify => background,
        Source::YouTubeMusic | Source::Local => crate::theme::HAIRLINE,
    };

    move |_theme: &iced::Theme| container::Style {
        text_color: Some(foreground),
        background: Some(background.into()),
        border: Border {
            color: border_color,
            width: 1.0,
            radius: 0.0.into(),
        },
        ..container::Style::default()
    }
}

/// Solid accent. The search surface paints this once, under the title.
fn accent_mark(_theme: &iced::Theme) -> container::Style {
    container::background(crate::theme::ACCENT)
}

fn search_field(_theme: &iced::Theme, status: text_input::Status) -> text_input::Style {
    let border_color = match status {
        text_input::Status::Focused { .. } => crate::theme::ACCENT,
        text_input::Status::Hovered => crate::theme::TEXT_SECONDARY,
        text_input::Status::Active | text_input::Status::Disabled => crate::theme::HAIRLINE,
    };

    text_input::Style {
        background: Background::Color(crate::theme::BACKGROUND),
        border: Border {
            color: border_color,
            width: 1.0,
            radius: 0.0.into(),
        },
        icon: crate::theme::TEXT_SECONDARY,
        placeholder: crate::theme::TEXT_TERTIARY,
        value: crate::theme::TEXT_PRIMARY,
        selection: crate::theme::ACCENT_DIM,
    }
}
