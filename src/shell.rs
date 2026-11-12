//! The native window.
//!
//! One borderless iced window is the whole interface. A title bar, a sidebar,
//! the open pane, and a now-playing bar fill that window. Search asks Spotify
//! and YouTube Music on background threads, then keeps one row per recording.
//! Choosing a row asks the playback thread to play it. Spotify and YouTube
//! Music sign-in live on the Accounts pane. The window does not know how a
//! catalogue is fetched or how a stream is decoded.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use iced::alignment::{Horizontal, Vertical};
use iced::futures::channel::oneshot;
use iced::widget::{
    Space, button, column, container, mouse_area, progress_bar, row, scrollable, stack, text,
    text_input,
};
use iced::window::{self, Direction, Settings};
use iced::{Background, Border, Color, Element, Fill, Length, Shadow, Size, Subscription, Task};

use crate::music::{self, MusicService, Playlist, Source, Track};
use crate::player::{self, Phase};
use crate::playlists;
use crate::search;
use crate::spotify::{self, AccountPhase};
use crate::youtube_account::LibrarySession;
use crate::youtube_music::{YouTubeFallback, YouTubeMusic};

/// A blocking catalogue call. The iced executor is not a Tokio runtime, so
/// the window runs this on its own thread and waits without blocking the UI.
type BackgroundSearch = Arc<dyn Fn(&str) -> Result<Vec<Track>, music::Error> + Send + Sync>;

/// Spotify sign-in. Tests pass a closure that does not open a browser.
type SignIn = Arc<dyn Fn() -> AccountPhase + Send + Sync>;

/// Spotify catalogue. Search stays a closure; library reads use the trait.
type Catalogue = Arc<dyn MusicService + Send + Sync>;

/// Window title. This is the product name, not a window-manager default.
const TITLE: &str = "CodeVox mU";

/// Logical pixels. Large enough for the sidebar, a result list, and the bar.
const WINDOW_SIZE: Size = Size::new(1120.0, 740.0);

/// Keeps the title bar, sidebar, and transport from collapsing together.
const MIN_WINDOW_SIZE: Size = Size::new(880.0, 560.0);

const TITLE_BAR_HEIGHT: f32 = 48.0;
const SIDEBAR_WIDTH: f32 = 196.0;
const CONTENT_PADDING: f32 = 20.0;
const HEADING_SIZE: f32 = 18.0;
const BODY_SIZE: f32 = 14.0;
const META_SIZE: f32 = 12.0;
const SOURCE_SIZE: f32 = 11.0;
const RESULT_SPACING: f32 = 8.0;
const ROW_SPACING: f32 = 12.0;
const SECTION_SPACING: f32 = 14.0;

/// Invisible resize strips. Corners sit above the edges so diagonal grabs win.
const RESIZE_EDGE: f32 = 6.0;
const RESIZE_CORNER: f32 = 10.0;

/// Frames to present before a headless screenshot, so the first capture is painted.
const SHOT_FRAME_DELAY: u32 = 3;

/// Brand mark in the title bar. A short vertical accent, not a second line.
const ACCENT_MARK_WIDTH: f32 = 3.0;
const ACCENT_MARK_HEIGHT: f32 = 16.0;

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
/// `Screenshot` writes the main view and the Accounts view, then exits.
/// `Run` stays open until the window is closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    Run,
    Smoke,
    Screenshot {
        main: PathBuf,
        accounts: PathBuf,
        library: PathBuf,
    },
}

/// Which headless capture just finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Capture {
    Main,
    Accounts,
    Library,
}

/// Progress of `--screenshot`. `Idle` is every other mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShotStep {
    Idle,
    Main,
    MainPending,
    Accounts,
    AccountsPending,
    Library,
    LibraryPending,
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
    /// `generation` matches the query that started this search.
    SpotifySearched {
        generation: u64,
        result: Result<Vec<Track>, music::Error>,
    },
    Play(Track),
    TogglePlayback,
    StopPlayback,
    NextTrack,
    PreviousTrack,
    Enqueue(Track),
    PlayNext(Track),
    MoveEarlier(usize),
    MoveLater(usize),
    RemoveQueued(usize),
    RemoveCurrent,
    Tick,
    SignIn,
    AccountChanged {
        epoch: u64,
        phase: AccountPhase,
    },
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
        result: Result<LibraryLoad, music::Error>,
    },
    OpenPlaylist {
        source: Source,
        id: String,
        name: String,
    },
    PlaylistReady {
        epoch: u64,
        result: Result<Vec<Track>, music::Error>,
    },
    ShowSearch,
    ShowQueue,
    ShowMixes,
    MixDraftChanged(String),
    CreateMix,
    OpenMix(String),
    RenameMix,
    DeleteMix,
    AddToMix(Track),
    RemoveMixTrack(usize),
    MoveMixEarlier(usize),
    MoveMixLater(usize),
    PlayMix,
    PlayMixFrom(usize),
    ShowAccounts,
    BackToLibrary,
    DismissNotices,
    /// Press on the custom title bar. Starts an OS window drag.
    DragWindow,
    /// Title-bar double-click, or the maximise / restore button.
    ToggleMaximize,
    MinimizeWindow,
    CloseWindow,
    Resize(Direction),
    /// The window exists. Screenshot mode records the id and does not attach media keys.
    WindowOpened(window::Id),
    WindowResized(window::Id),
    Maximized(bool),
    Captured {
        kind: Capture,
        result: Result<(), String>,
    },
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

/// Playlists gathered for one Library open, plus a per-service failure.
///
/// One service can fail while the other's rows still show.
#[derive(Clone, Debug)]
struct LibraryLoad {
    playlists: Vec<Playlist>,
    spotify_error: Option<String>,
    youtube_error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pane {
    Search,
    Queue,
    Library,
    Playlist,
    Accounts,
    /// Saved mixed playlists.
    Mixes,
    /// One saved mixed playlist.
    Mix,
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
    spotify_rows: Vec<Track>,
    youtube_rows: Vec<Track>,
    /// YouTube Music failure for the query now on screen. Spotify rows stay.
    error: Option<String>,
    spotify_error: Option<String>,
    spotify_pending: bool,
    youtube_pending: bool,
    /// Bumped on every edit so a slow reply cannot overwrite a newer query.
    generation: u64,
    /// Bumped when sign-in starts, so a slower restore cannot overwrite it.
    epoch: u64,
    phase: AccountPhase,
    spotify: BackgroundSearch,
    youtube: BackgroundSearch,
    sign_in: SignIn,
    library: Arc<dyn LibrarySession>,
    spotify_catalogue: Catalogue,
    pane: Pane,
    sign: YtSign,
    cookie_draft: String,
    cookie_open: bool,
    playlists: Vec<Playlist>,
    playlist_tracks: Vec<Track>,
    playlist_name: String,
    playlist_source: Option<Source>,
    /// Why this playlist has no tracks. Not an error banner.
    playlist_note: Option<String>,
    /// Mixed playlists saved on this device.
    mixes: playlists::Store,
    /// Playlist that Add writes to, and the one open on the Mix pane.
    mix_id: Option<String>,
    mix_draft: String,
    mix_error: Option<String>,
    library_loading: bool,
    /// Bumped when a YouTube Music sign-in or library fetch starts, so a
    /// slower reply cannot overwrite a newer one. Spotify sign-in uses
    /// `epoch` instead, so the two replies cannot cancel each other.
    library_epoch: u64,
    restore_started: bool,
    playback: player::Playback,
    /// A clock tick is already waiting. Keeps the 200 ms timer from stacking.
    clock_armed: bool,
    /// Sample transport for `--screenshot` only. Run and smoke leave this empty.
    preview: Option<player::NowPlaying>,
    /// Playback error the listener dismissed. A new string shows again.
    dismissed_playback_error: Option<String>,
    window_id: Option<window::Id>,
    maximized: bool,
    shot_step: ShotStep,
    screenshot_main: Option<PathBuf>,
    screenshot_accounts: Option<PathBuf>,
    screenshot_library: Option<PathBuf>,
    #[cfg(test)]
    last_play: Option<Track>,
    /// Ids sent to the mixed queue, and the index playback started at.
    #[cfg(test)]
    last_mix_play: Option<(Vec<String>, usize)>,
}

pub fn run(mode: Mode) -> iced::Result {
    let boot_mode = mode.clone();
    let mut application = iced::application(
        move || Shell::boot(boot_mode.clone()),
        Shell::update,
        Shell::view,
    )
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

/// Borderless on every OS. The shell draws the title bar and the resize edges.
///
/// `transparent` stays off: the fill is the token background, not a glass window.
/// macOS traffic lights stay hidden with the decorations. The custom minimise,
/// maximise, and close buttons are the window controls on all three platforms.
fn window_settings() -> Settings {
    Settings {
        size: WINDOW_SIZE,
        min_size: Some(MIN_WINDOW_SIZE),
        position: window::Position::Centered,
        resizable: true,
        decorations: false,
        transparent: false,
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

/// Ends the process if a headless mode is still alive after [`SMOKE_TIMEOUT`].
///
/// The thread is detached. A normal exit returns from `main` and the
/// process dies before this wakes up. `Run` mode must not call it.
fn arm_headless_timeout(label: &'static str) {
    std::thread::spawn(move || {
        std::thread::sleep(SMOKE_TIMEOUT);
        eprintln!(
            "codevox-mu {label}: timed out after {}s without exiting",
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
    assert!(shell.results.is_empty());
    assert!(shell.spotify_pending);
    assert!(shell.error.is_none());

    let spotify = music::MockService::spotify().search("night").expect("mock");
    let _ = shell.update(Message::SpotifySearched {
        generation: 1,
        result: Ok(spotify),
    });
    let sources: Vec<_> = shell.results.iter().map(|row| row.track.source).collect();
    assert_eq!(sources, [Source::Spotify]);

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
fn search_box_collapses_a_youtube_duplicate_into_the_playable_row() {
    let mut shell = test_shell(Mode::Run);
    let _ = shell.update(Message::QueryChanged("night".into()));
    let spotify = music::MockService::spotify().search("night").expect("mock");
    let _ = shell.update(Message::SpotifySearched {
        generation: 1,
        result: Ok(spotify),
    });
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
fn youtube_failure_keeps_spotify_rows_and_shows_an_error() {
    let mut shell = test_shell(Mode::Run);
    let _ = shell.update(Message::QueryChanged("night".into()));
    let spotify = music::MockService::spotify().search("night").expect("mock");
    let _ = shell.update(Message::SpotifySearched {
        generation: 1,
        result: Ok(spotify),
    });
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
    let paper = music::MockService::spotify()
        .search("paper")
        .expect("paper");
    let _ = shell.update(Message::SpotifySearched {
        generation: 2,
        result: Ok(paper),
    });
    assert!(
        shell
            .results
            .iter()
            .all(|row| row.track.source == Source::Spotify)
    );
    assert_eq!(shell.results[0].track.title, "Paper Boats");

    let paper_youtube = music::MockService::youtube_music()
        .search("paper")
        .expect("paper");
    let _ = shell.update(Message::YoutubeSearched {
        generation: 2,
        result: Ok(paper_youtube),
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
fn account_states_render_and_a_stale_restore_is_ignored() {
    let mut shell = test_shell(Mode::Run);
    let signed_out = spotify::present_account(&shell.phase);
    assert!(signed_out.offer_sign_in);
    let _ = Shell::view(&shell);

    let _ = shell.update(Message::AccountChanged {
        epoch: 0,
        phase: AccountPhase::SignedIn {
            name: "Ada Vega".into(),
        },
    });
    assert_eq!(
        shell.phase,
        AccountPhase::SignedIn {
            name: "Ada Vega".into()
        }
    );
    let _ = Shell::view(&shell);

    let _ = shell.update(Message::AccountChanged {
        epoch: 0,
        phase: AccountPhase::NotPremium {
            name: "Jonah".into(),
        },
    });
    assert!(matches!(shell.phase, AccountPhase::NotPremium { .. }));
    let _ = Shell::view(&shell);

    let _ = shell.update(Message::AccountChanged {
        epoch: 0,
        phase: AccountPhase::Failed {
            message: "browser closed".into(),
        },
    });
    shell.epoch = 2;
    let _ = shell.update(Message::AccountChanged {
        epoch: 1,
        phase: AccountPhase::SignedIn {
            name: "Nope".into(),
        },
    });
    assert_eq!(
        shell.phase,
        AccountPhase::Failed {
            message: "browser closed".into()
        }
    );
    let _ = Shell::view(&shell);
}

#[test]
fn a_finished_sign_in_becomes_signed_in_not_stopped() {
    let mut shell = test_shell(Mode::Run);
    let _ = shell.update(Message::SignIn);
    assert!(matches!(shell.phase, AccountPhase::SigningIn));
    let epoch = shell.epoch;
    // The worker returns after the account file is saved. The window must
    // take that phase. A dropped reply used to become "stopped before it finished".
    let phase = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(account_task("spotify-sign-in", || AccountPhase::SignedIn {
            name: "Joel".into(),
        }));
    assert_eq!(
        phase,
        AccountPhase::SignedIn {
            name: "Joel".into()
        }
    );
    let _ = shell.update(Message::AccountChanged { epoch, phase });
    assert_eq!(
        shell.phase,
        AccountPhase::SignedIn {
            name: "Joel".into()
        }
    );
    let shown = spotify::present_account(&shell.phase).text;
    assert_eq!(shown, "Signed in as Joel");
    assert!(!shown.contains("stopped before it finished"), "{shown}");
    assert_eq!(spotify_compact(&shell.phase).0, "signed in");
    let _ = Shell::view(&shell);
}

#[test]
fn a_panicked_sign_in_reports_the_panic_instead_of_a_dropped_reply() {
    let phase = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(account_task("spotify-sign-in", || {
            panic!("boom-marker");
        }));
    let AccountPhase::Failed { message } = phase else {
        panic!("expected the panic text, got {phase:?}");
    };
    assert!(message.contains("boom-marker"), "{message}");
    assert!(!message.contains("stopped before it finished"), "{message}");
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
fn the_library_says_when_both_accounts_are_signed_out() {
    let mut shell = test_shell(Mode::Run);
    let _ = shell.update(Message::ShowLibrary);
    assert_eq!(shell.pane, Pane::Library);
    assert!(!shell.library_loading);
    assert!(shell.playlists.is_empty());
    assert_eq!(
        status_note(&shell).as_deref(),
        Some("Sign in to Spotify or YouTube Music to see your library.")
    );
    assert!(notice_lines(&shell).is_empty());
    let _ = Shell::view(&shell);
}

#[test]
fn a_playlist_track_plays_through_the_same_message_as_search() {
    let mut shell = test_shell(Mode::Run);
    shell.cookie_draft = "SAPISID=test-value;".into();
    let _ = shell.update(Message::SubmitCookie);
    assert_eq!(shell.sign, YtSign::Busy);
    let epoch = shell.library_epoch;
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
    let epoch = shell.library_epoch;
    let _ = shell.update(Message::LibraryReady {
        epoch,
        result: Ok(LibraryLoad {
            playlists: vec![Playlist {
                id: "PLnight".into(),
                source: Source::YouTubeMusic,
                name: "Night".into(),
            }],
            spotify_error: None,
            youtube_error: None,
        }),
    });
    assert_eq!(shell.playlists[0].name, "Night");

    let _ = shell.update(Message::OpenPlaylist {
        source: Source::YouTubeMusic,
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
    let epoch = shell.library_epoch;
    let _ = shell.update(Message::PlaylistReady {
        epoch,
        result: Ok(vec![track.clone()]),
    });
    assert_eq!(shell.playlist_tracks, vec![track.clone()]);
    assert!(matches!(play_message(&track), Message::Play(played) if played == track));
    let _ = shell.update(play_message(&track));
    assert_eq!(shell.last_play.as_ref(), Some(&track));
    assert_eq!(shell.playback.phase(), Phase::Idle);

    let stale = shell.library_epoch;
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
fn library_rows_keep_both_sources_and_spotify_tracks_use_search_playback() {
    let spotify_liked = Playlist {
        id: "liked-songs".into(),
        source: Source::Spotify,
        name: "Liked Songs".into(),
    };
    let glow = Playlist {
        id: "37i9dQZF1DXcBWIGoYBM5M".into(),
        source: Source::Spotify,
        name: "Low Glow".into(),
    };
    let youtube_liked = Playlist {
        id: "liked-songs".into(),
        source: Source::YouTubeMusic,
        name: "Liked songs".into(),
    };
    let weekday = Playlist {
        id: "yt-weekday".into(),
        source: Source::YouTubeMusic,
        name: "Weekday".into(),
    };
    let merged = merge_library(
        Some(Ok(vec![spotify_liked.clone(), glow.clone()])),
        Some(Ok(vec![youtube_liked.clone(), weekday.clone()])),
    );
    assert_eq!(
        merged
            .playlists
            .iter()
            .map(|playlist| (playlist.source, playlist.name.as_str()))
            .collect::<Vec<_>>(),
        [
            (Source::Spotify, "Liked Songs"),
            (Source::Spotify, "Low Glow"),
            (Source::YouTubeMusic, "Liked songs"),
            (Source::YouTubeMusic, "Weekday"),
        ]
    );
    assert!(merged.spotify_error.is_none());
    assert!(merged.youtube_error.is_none());

    let mut shell = test_shell(Mode::Run);
    shell.phase = AccountPhase::SignedIn {
        name: "Ada Vega".into(),
    };
    shell.sign = YtSign::In;
    shell
        .library
        .sign_in("SAPISID=test-value;")
        .expect("youtube session");
    let _ = shell.update(Message::ShowLibrary);
    assert_eq!(shell.pane, Pane::Library);
    assert!(shell.library_loading);
    let epoch = shell.library_epoch;
    let _ = shell.update(Message::LibraryReady {
        epoch,
        result: Ok(merged),
    });
    assert_eq!(shell.playlists.len(), 4);
    let _ = Shell::view(&shell);

    let _ = shell.update(Message::OpenPlaylist {
        source: Source::Spotify,
        id: glow.id.clone(),
        name: glow.name.clone(),
    });
    let track = Track {
        id: "5sWHDYs0csV6RS48xBl0tH".into(),
        source: Source::Spotify,
        title: "Night Drive".into(),
        artists: vec!["Ada Vega".into()],
        album: Some("Low Glow".into()),
        duration: std::time::Duration::from_secs(192),
    };
    let epoch = shell.library_epoch;
    let _ = shell.update(Message::PlaylistReady {
        epoch,
        result: Ok(vec![track.clone()]),
    });
    assert_eq!(shell.playlist_tracks, vec![track.clone()]);
    assert!(matches!(play_message(&track), Message::Play(played) if played == track));
    let _ = shell.update(Message::PlayNext(track.clone()));
    let _ = shell.update(Message::Enqueue(track.clone()));
    assert!(shell.last_play.is_none());
    let _ = shell.update(play_message(&track));
    assert_eq!(shell.last_play.as_ref(), Some(&track));
    assert_eq!(
        shell.last_play.as_ref().map(|played| played.source),
        Some(Source::Spotify)
    );
    let _ = Shell::view(&shell);
}

#[test]
fn an_empty_signed_in_library_says_there_are_no_playlists() {
    let mut shell = test_shell(Mode::Run);
    shell.phase = AccountPhase::SignedIn {
        name: "Ada Vega".into(),
    };
    let _ = shell.update(Message::ShowLibrary);
    let epoch = shell.library_epoch;
    let _ = shell.update(Message::LibraryReady {
        epoch,
        result: Ok(merge_library(Some(Ok(Vec::new())), None)),
    });
    assert!(shell.playlists.is_empty());
    assert_eq!(status_note(&shell).as_deref(), Some("No playlists"));
    assert!(notice_lines(&shell).is_empty());
    let _ = Shell::view(&shell);
}

#[test]
fn a_playlist_spotify_will_not_share_is_a_note() {
    let mut shell = test_shell(Mode::Run);
    shell.pane = Pane::Playlist;
    shell.library_epoch = 4;
    shell.playlist_source = Some(Source::Spotify);
    shell.playlist_name = "Today's Top Hits".into();
    let _ = shell.update(Message::PlaylistReady {
        epoch: 4,
        result: Err(music::Error::Unavailable(
            spotify::PLAYLIST_NOT_SHARED.to_string(),
        )),
    });
    assert!(shell.error.is_none());
    assert!(shell.playlist_tracks.is_empty());
    assert!(notice_lines(&shell).is_empty());
    assert_eq!(
        status_note(&shell).as_deref(),
        Some(spotify::PLAYLIST_NOT_SHARED)
    );
    let _ = Shell::view(&shell);
}

#[test]
fn a_spotify_rate_limit_keeps_the_youtube_library_and_the_sentence() {
    let weekday = Playlist {
        id: "yt-weekday".into(),
        source: Source::YouTubeMusic,
        name: "Weekday".into(),
    };
    let limited = "Spotify is limiting requests. Wait a moment and try again.";
    let merged = merge_library(
        Some(Err(music::Error::Unavailable(limited.to_string()))),
        Some(Ok(vec![weekday.clone()])),
    );
    assert_eq!(merged.playlists, vec![weekday]);
    assert_eq!(merged.spotify_error.as_deref(), Some(limited));
    assert!(merged.youtube_error.is_none());

    let mut shell = test_shell(Mode::Run);
    shell.phase = AccountPhase::NotPremium {
        name: "Jonah".into(),
    };
    shell.sign = YtSign::In;
    shell
        .library
        .sign_in("SAPISID=test-value;")
        .expect("youtube session");
    let _ = shell.update(Message::ShowLibrary);
    let epoch = shell.library_epoch;
    let _ = shell.update(Message::LibraryReady {
        epoch,
        result: Ok(merged),
    });
    assert_eq!(shell.playlists[0].name, "Weekday");
    let lines = notice_lines(&shell);
    assert!(lines.iter().any(|line| line == limited), "{lines:?}");
    assert!(lines.iter().all(|line| !line.contains("429")), "{lines:?}");
    assert_ne!(status_note(&shell).as_deref(), Some("No playlists"));
    let _ = Shell::view(&shell);

    let _ = shell.update(Message::OpenPlaylist {
        source: Source::Spotify,
        id: "liked-songs".into(),
        name: "Liked Songs".into(),
    });
    let epoch = shell.library_epoch;
    let _ = shell.update(Message::PlaylistReady {
        epoch,
        result: Err(music::Error::Unavailable(limited.to_string())),
    });
    assert!(shell.playlist_tracks.is_empty());
    let lines = notice_lines(&shell);
    assert!(lines.iter().any(|line| line == limited), "{lines:?}");
    assert_ne!(
        status_note(&shell).as_deref(),
        Some("This playlist is empty.")
    );
    let _ = Shell::view(&shell);
}

#[test]
fn a_late_sign_in_does_not_undo_sign_out() {
    let mut shell = test_shell(Mode::Run);
    shell.cookie_draft = "SAPISID=test-value;".into();
    let _ = shell.update(Message::SubmitCookie);
    let epoch = shell.library_epoch;
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

/// No keyring and no network. Smoke uses this so the three frames stay offline.
struct ClosedLibrary;

impl LibrarySession for ClosedLibrary {
    fn signed_in(&self) -> bool {
        false
    }

    fn sign_in(&self, _cookie: &str) -> Result<(), music::Error> {
        Err(music::Error::Unavailable(
            "YouTube Music sign-in is not available.".into(),
        ))
    }

    fn restore(&self) -> Result<bool, music::Error> {
        Ok(false)
    }

    fn sign_out(&self) -> Result<(), music::Error> {
        Ok(())
    }

    fn playlists(&self) -> Result<Vec<Playlist>, music::Error> {
        Err(music::Error::Unavailable(
            "Sign in to YouTube Music to see your library.".into(),
        ))
    }

    fn playlist_tracks(&self, _playlist_id: &str) -> Result<Vec<Track>, music::Error> {
        Err(music::Error::NotFound)
    }
}

#[cfg(test)]
fn test_shell(mode: Mode) -> Shell {
    let spotify = music::MockService::spotify();
    let youtube = music::MockService::youtube_music();
    // Tests never poll the task, and the closures are the mocks if they do.
    Shell::with_services(
        mode,
        Arc::new(move |query| spotify.search(query)),
        Arc::new(move |query| youtube.search(query)),
        Arc::new(|| AccountPhase::SignedOut),
        Arc::new(FakeLibrary::empty()),
        Arc::new(music::MockService::spotify()),
    )
}

impl Shell {
    fn boot(mode: Mode) -> (Self, Task<Message>) {
        if mode == Mode::Smoke {
            arm_headless_timeout("smoke");
            // No account restore, no librespot player, no YouTube client.
            // Typing during the three frames would still stay off the network.
            let shell = Self::with_services(
                mode,
                Arc::new(|_| Ok(Vec::new())),
                Arc::new(|_| Ok(Vec::new())),
                Arc::new(|| AccountPhase::SignedOut),
                Arc::new(ClosedLibrary),
                Arc::new(music::MockService::spotify()),
            );
            return (shell, Task::none());
        }

        if matches!(mode, Mode::Screenshot { .. }) {
            arm_headless_timeout("screenshot");
            // Offline catalogue, so the two captures show the layout without a network.
            let mut shell = Self::with_services(
                mode,
                Arc::new(|query| music::MockService::spotify().search(query)),
                Arc::new(|query| music::MockService::youtube_music().search(query)),
                Arc::new(|| AccountPhase::SignedOut),
                Arc::new(ClosedLibrary),
                Arc::new(music::MockService::spotify()),
            );
            shell.seed_screenshot();
            return (shell, Task::none());
        }

        let account = spotify::SpotifyAccount::new();
        let search_account = account.clone();
        let sign_account = account.clone();
        let play_account = account.clone();
        let library_account = account.clone();
        let youtube = YouTubeMusic::new();
        let library: Arc<dyn LibrarySession> = Arc::new(youtube.clone());
        let mut shell = Self::with_services(
            mode,
            Arc::new(move |query| {
                spotify::SpotifyCatalogue::new(search_account.clone()).search(query)
            }),
            Arc::new(move |query| youtube.search(query)),
            Arc::new(move || sign_account.sign_in()),
            library,
            Arc::new(spotify::SpotifyCatalogue::new(library_account)),
        );
        shell.playback = player::Playback::start(
            Box::new(spotify::LibrespotEngine::new(play_account)),
            Box::new(YouTubeFallback::new(YouTubeMusic::new())),
        );
        // Saved playlists outlive this process. Smoke and screenshots stay
        // on the empty in-memory store.
        shell.load_saved_playlists();
        let task = Task::perform(
            account_task("spotify-restore", move || account.restore()),
            |phase| Message::AccountChanged { epoch: 0, phase },
        );
        (shell, task)
    }

    fn with_services(
        mode: Mode,
        spotify: BackgroundSearch,
        youtube: BackgroundSearch,
        sign_in: SignIn,
        library: Arc<dyn LibrarySession>,
        spotify_catalogue: Catalogue,
    ) -> Self {
        let shot_step = match &mode {
            Mode::Screenshot { .. } => ShotStep::Main,
            Mode::Run | Mode::Smoke => ShotStep::Idle,
        };
        let screenshot_main = match &mode {
            Mode::Screenshot { main, .. } => Some(main.clone()),
            Mode::Run | Mode::Smoke => None,
        };
        let screenshot_accounts = match &mode {
            Mode::Screenshot { accounts, .. } => Some(accounts.clone()),
            Mode::Run | Mode::Smoke => None,
        };
        let screenshot_library = match &mode {
            Mode::Screenshot { library, .. } => Some(library.clone()),
            Mode::Run | Mode::Smoke => None,
        };
        Self {
            mode,
            frame_count: 0,
            exit_requested: false,
            query: String::new(),
            results: Vec::new(),
            spotify_rows: Vec::new(),
            youtube_rows: Vec::new(),
            error: None,
            spotify_error: None,
            spotify_pending: false,
            youtube_pending: false,
            generation: 0,
            epoch: 0,
            phase: AccountPhase::SignedOut,
            spotify,
            youtube,
            sign_in,
            library,
            spotify_catalogue,
            pane: Pane::Search,
            sign: YtSign::Out,
            cookie_draft: String::new(),
            cookie_open: false,
            playlists: Vec::new(),
            playlist_tracks: Vec::new(),
            playlist_name: String::new(),
            playlist_source: None,
            playlist_note: None,
            mixes: playlists::Store::memory(),
            mix_id: None,
            mix_draft: String::new(),
            mix_error: None,
            library_loading: false,
            library_epoch: 0,
            restore_started: false,
            playback: player::Playback::dormant(),
            clock_armed: false,
            preview: None,
            dismissed_playback_error: None,
            window_id: None,
            maximized: false,
            shot_step,
            screenshot_main,
            screenshot_accounts,
            screenshot_library,
            #[cfg(test)]
            last_play: None,
            #[cfg(test)]
            last_mix_play: None,
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
            Message::SpotifySearched { generation, result } => {
                self.apply_spotify(generation, result);
                Task::none()
            }
            Message::SignIn => self.start_sign_in(),
            Message::AccountChanged { epoch, phase } => {
                if epoch == self.epoch {
                    log::info!(
                        "{}",
                        crate::diag::redact(&format!("spotify account: {phase:?}"))
                    );
                    self.phase = phase;
                } else {
                    log::info!("spotify account: ignored a stale update");
                }
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
            Message::PreviousTrack => {
                self.playback.previous();
                self.arm_clock()
            }
            Message::Enqueue(track) => {
                self.playback.enqueue(track);
                self.arm_clock()
            }
            Message::PlayNext(track) => {
                self.playback.play_next(track);
                self.arm_clock()
            }
            Message::MoveEarlier(index) => {
                self.playback.move_earlier(index);
                self.arm_clock()
            }
            Message::MoveLater(index) => {
                self.playback.move_later(index);
                self.arm_clock()
            }
            Message::RemoveQueued(index) => {
                self.playback.remove_queued(index);
                self.arm_clock()
            }
            Message::RemoveCurrent => {
                self.playback.remove_current();
                self.arm_clock()
            }
            Message::Tick => self.on_tick(),
            Message::ShowSignIn => {
                self.cookie_open = true;
                self.pane = Pane::Accounts;
                self.error = None;
                Task::none()
            }
            Message::ShowQueue => {
                self.pane = Pane::Queue;
                Task::none()
            }
            Message::ShowMixes => {
                self.mix_draft.clear();
                self.pane = Pane::Mixes;
                Task::none()
            }
            Message::MixDraftChanged(name) => {
                self.mix_draft = name;
                Task::none()
            }
            Message::CreateMix => {
                self.create_saved();
                Task::none()
            }
            Message::OpenMix(id) => {
                self.open_saved(id);
                Task::none()
            }
            Message::RenameMix => {
                self.rename_saved();
                Task::none()
            }
            Message::DeleteMix => {
                self.delete_saved();
                Task::none()
            }
            Message::AddToMix(track) => {
                self.add_saved(track);
                Task::none()
            }
            Message::RemoveMixTrack(index) => {
                self.remove_saved(index);
                Task::none()
            }
            Message::MoveMixEarlier(index) => {
                self.move_saved(index, false);
                Task::none()
            }
            Message::MoveMixLater(index) => {
                self.move_saved(index, true);
                Task::none()
            }
            Message::PlayMix => self.play_saved(0),
            Message::PlayMixFrom(index) => self.play_saved(index),
            Message::ShowAccounts => {
                self.pane = Pane::Accounts;
                if self.sign == YtSign::Out {
                    self.cookie_open = true;
                }
                Task::none()
            }
            Message::DismissNotices => {
                self.dismiss_notices();
                Task::none()
            }
            Message::DragWindow => self.window_action(window::drag),
            Message::ToggleMaximize => self.window_action(|id| {
                window::toggle_maximize(id).chain(window::is_maximized(id).map(Message::Maximized))
            }),
            Message::MinimizeWindow => self.window_action(|id| window::minimize(id, true)),
            Message::CloseWindow => self.window_action(window::close),
            Message::Resize(direction) => {
                self.window_action(move |id| window::drag_resize(id, direction))
            }
            Message::WindowOpened(id) => {
                self.window_id = Some(id);
                Task::none()
            }
            Message::WindowResized(id) => {
                self.window_id = Some(id);
                window::is_maximized(id).map(Message::Maximized)
            }
            Message::Maximized(maximized) => {
                self.maximized = maximized;
                Task::none()
            }
            Message::Captured { kind, result } => self.finish_capture(kind, result),
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
            Message::OpenPlaylist { source, id, name } => self.open_playlist(source, id, name),
            Message::PlaylistReady { epoch, result } => {
                self.finish_playlist(epoch, result);
                Task::none()
            }
            Message::ShowSearch => {
                self.library_epoch = self.library_epoch.wrapping_add(1);
                self.back_to_search();
                Task::none()
            }
            Message::BackToLibrary => {
                self.library_epoch = self.library_epoch.wrapping_add(1);
                self.pane = Pane::Library;
                self.playlist_tracks.clear();
                self.playlist_name.clear();
                self.library_loading = false;
                self.error = None;
                Task::none()
            }
            Message::BindMedia(id) => {
                self.window_id = Some(id);
                Task::batch([
                    crate::media_controls::read_handle(id).map(Message::MediaBound),
                    self.restore_saved_sign_in(),
                ])
            }
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

    fn load_saved_playlists(&mut self) {
        match playlists::Store::load_default() {
            Ok(store) => {
                self.mix_id = store.playlists().first().map(|item| item.id().to_string());
                self.mixes = store;
            }
            Err(error) => {
                self.mix_error = Some(error.message().to_string());
                self.mixes = error.into_store();
            }
        }
    }

    fn selected_mix(&self) -> Option<&playlists::SavedPlaylist> {
        let id = self.mix_id.as_deref()?;
        self.mixes.playlist(id)
    }

    fn create_saved(&mut self) {
        match self.mixes.create(&self.mix_draft) {
            Ok(id) => {
                self.mix_error = None;
                self.mix_id = Some(id);
                self.show_saved();
            }
            Err(error) => self.mix_error = Some(error),
        }
    }

    fn open_saved(&mut self, id: String) {
        if self.mixes.playlist(&id).is_none() {
            return;
        }
        self.mix_id = Some(id);
        self.mix_error = None;
        self.show_saved();
    }

    fn show_saved(&mut self) {
        self.mix_draft = self
            .selected_mix()
            .map(|playlist| playlist.name().to_string())
            .unwrap_or_default();
        self.pane = Pane::Mix;
    }

    fn rename_saved(&mut self) {
        let Some(id) = self.mix_id.clone() else {
            return;
        };
        match self.mixes.rename(&id, &self.mix_draft) {
            Ok(()) => self.mix_error = None,
            Err(error) => self.mix_error = Some(error),
        }
    }

    fn delete_saved(&mut self) {
        let Some(id) = self.mix_id.clone() else {
            return;
        };
        match self.mixes.delete(&id) {
            Ok(()) => {
                self.mix_error = None;
                self.mix_id = self
                    .mixes
                    .playlists()
                    .first()
                    .map(|playlist| playlist.id().to_string());
                self.mix_draft.clear();
                self.pane = Pane::Mixes;
            }
            Err(error) => self.mix_error = Some(error),
        }
    }

    fn add_saved(&mut self, track: Track) {
        let id = match self.mix_target() {
            Ok(id) => id,
            Err(error) => {
                self.mix_error = Some(error);
                return;
            }
        };
        match self.mixes.add_track(&id, track) {
            Ok(()) => self.mix_error = None,
            Err(error) => self.mix_error = Some(error),
        }
    }

    /// The open playlist, otherwise the first saved one.
    ///
    /// With nothing saved, Add creates a playlist named Playlist.
    fn mix_target(&mut self) -> Result<String, String> {
        if let Some(id) = self.mix_id.clone()
            && self.mixes.playlist(&id).is_some()
        {
            return Ok(id);
        }
        if let Some(existing) = self.mixes.playlists().first() {
            let id = existing.id().to_string();
            self.mix_id = Some(id.clone());
            return Ok(id);
        }
        let id = self.mixes.create("Playlist")?;
        self.mix_id = Some(id.clone());
        Ok(id)
    }

    fn remove_saved(&mut self, index: usize) {
        self.edit_saved(|store, id| store.remove_track(&id, index));
    }

    fn move_saved(&mut self, index: usize, later: bool) {
        self.edit_saved(|store, id| {
            if later {
                store.move_later(&id, index)
            } else {
                store.move_earlier(&id, index)
            }
        });
    }

    fn edit_saved(
        &mut self,
        edit: impl FnOnce(&mut playlists::Store, String) -> Result<(), String>,
    ) {
        let Some(id) = self.mix_id.clone() else {
            return;
        };
        match edit(&mut self.mixes, id) {
            Ok(()) => self.mix_error = None,
            Err(error) => self.mix_error = Some(error),
        }
    }

    fn play_saved(&mut self, index: usize) -> Task<Message> {
        let Some(tracks) = self
            .selected_mix()
            .map(|playlist| playlist.tracks().to_vec())
        else {
            return Task::none();
        };
        if index >= tracks.len() {
            return Task::none();
        }
        #[cfg(test)]
        {
            self.last_mix_play =
                Some((tracks.iter().map(|track| track.id.clone()).collect(), index));
        }
        self.playback.play_list(tracks, index);
        self.arm_clock()
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
        self.library_epoch = self.library_epoch.wrapping_add(1);
        let epoch = self.library_epoch;
        let library = Arc::clone(&self.library);
        let cookie = self.cookie_draft.clone();
        self.off_ui(
            "youtube-music-sign-in",
            move || library.sign_in(&cookie).map(|()| true),
            move |result| Message::SignInFinished { epoch, result },
        )
    }

    fn finish_sign_in(&mut self, epoch: u64, result: Result<bool, music::Error>) {
        if epoch != self.library_epoch {
            return;
        }
        match result {
            Ok(true) => {
                log::info!("youtube music: signed in");
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
                let detail = crate::diag::redact(&error.to_string());
                log::error!("youtube music: sign-in failed: {detail}");
                self.sign = YtSign::Out;
                self.cookie_open = true;
                self.error = Some(format!("YouTube Music sign-in failed: {detail}"));
            }
        }
    }

    fn sign_out(&mut self) -> Task<Message> {
        self.library_epoch = self.library_epoch.wrapping_add(1);
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
        if self.sign == YtSign::In && !self.library.signed_in() {
            self.sign = YtSign::Out;
        }
        let spotify_in = spotify_signed_in(&self.phase);
        let youtube_in = youtube_signed_in(self);
        self.pane = Pane::Library;
        self.playlist_tracks.clear();
        self.playlist_name.clear();
        self.playlist_source = None;
        self.error = None;
        self.spotify_error = None;
        self.library_epoch = self.library_epoch.wrapping_add(1);
        if !spotify_in && !youtube_in {
            self.playlists.clear();
            self.library_loading = false;
            return Task::none();
        }
        self.library_loading = true;
        let epoch = self.library_epoch;
        let catalogue = Arc::clone(&self.spotify_catalogue);
        let library = Arc::clone(&self.library);
        self.off_ui(
            "library",
            move || {
                Ok(load_signed_in_libraries(
                    spotify_in,
                    youtube_in,
                    catalogue.as_ref(),
                    library.as_ref(),
                ))
            },
            move |result| Message::LibraryReady { epoch, result },
        )
    }

    fn finish_library(&mut self, epoch: u64, result: Result<LibraryLoad, music::Error>) {
        if epoch != self.library_epoch || self.pane != Pane::Library {
            return;
        }
        self.library_loading = false;
        match result {
            Ok(load) => {
                self.playlists = load.playlists;
                self.spotify_error = load.spotify_error;
                self.error = load.youtube_error;
            }
            Err(error) => {
                self.playlists.clear();
                self.error = Some(format!("Library failed: {error}"));
            }
        }
    }

    fn open_playlist(&mut self, source: Source, id: String, name: String) -> Task<Message> {
        self.pane = Pane::Playlist;
        self.playlist_name = name;
        self.playlist_source = Some(source);
        self.playlist_tracks.clear();
        self.playlist_note = None;
        self.library_loading = true;
        self.error = None;
        self.library_epoch = self.library_epoch.wrapping_add(1);
        let epoch = self.library_epoch;
        match source {
            Source::Spotify => {
                let catalogue = Arc::clone(&self.spotify_catalogue);
                self.off_ui(
                    "spotify-playlist",
                    move || catalogue.playlist_tracks(&id),
                    move |result| Message::PlaylistReady { epoch, result },
                )
            }
            Source::YouTubeMusic => {
                let library = Arc::clone(&self.library);
                self.off_ui(
                    "youtube-music-playlist",
                    move || library.playlist_tracks(&id),
                    move |result| Message::PlaylistReady { epoch, result },
                )
            }
            Source::Local => {
                self.library_loading = false;
                self.error = Some("Local playlists are not available.".to_string());
                Task::none()
            }
        }
    }

    fn finish_playlist(&mut self, epoch: u64, result: Result<Vec<Track>, music::Error>) {
        if epoch != self.library_epoch || self.pane != Pane::Playlist {
            return;
        }
        self.library_loading = false;
        match result {
            Ok(tracks) => {
                self.playlist_tracks = tracks;
                self.playlist_note = None;
                self.error = None;
            }
            Err(error) if spotify::playlist_not_shared(&error) => {
                self.playlist_tracks.clear();
                self.error = None;
                self.playlist_note = Some(spotify::PLAYLIST_NOT_SHARED.to_string());
            }
            Err(error) => {
                self.playlist_tracks.clear();
                self.playlist_note = None;
                self.error = Some(playlist_failure(self.playlist_source, &error));
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
        self.library_epoch = self.library_epoch.wrapping_add(1);
        let epoch = self.library_epoch;
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
                        "could not start {thread_name}: {error}"
                    )));
                }
                receiver.await.unwrap_or_else(|_| {
                    Err(music::Error::Unavailable(format!(
                        "{thread_name} stopped before it replied"
                    )))
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
            self.clear_results();
            return Task::none();
        }

        self.spotify_rows.clear();
        self.youtube_rows.clear();
        self.results.clear();
        self.error = None;
        self.spotify_error = None;
        self.spotify_pending = true;
        self.youtube_pending = true;

        let query = self.query.clone();
        Task::batch([
            search_task(Arc::clone(&self.spotify), query.clone(), generation, true),
            search_task(Arc::clone(&self.youtube), query, generation, false),
        ])
    }

    fn clear_results(&mut self) {
        self.results.clear();
        self.spotify_rows.clear();
        self.youtube_rows.clear();
        self.error = None;
        self.spotify_error = None;
        self.spotify_pending = false;
        self.youtube_pending = false;
    }

    fn start_sign_in(&mut self) -> Task<Message> {
        if matches!(self.phase, AccountPhase::SigningIn) {
            return Task::none();
        }
        self.epoch = self.epoch.saturating_add(1);
        let epoch = self.epoch;
        self.phase = AccountPhase::SigningIn;
        log::info!("spotify sign-in: started");
        let sign_in = Arc::clone(&self.sign_in);
        Task::perform(
            account_task("spotify-sign-in", move || sign_in()),
            move |phase| Message::AccountChanged { epoch, phase },
        )
    }

    fn apply_spotify(&mut self, generation: u64, result: Result<Vec<Track>, music::Error>) {
        if generation != self.generation || self.query.trim().is_empty() {
            return;
        }
        self.spotify_pending = false;
        match result {
            Ok(tracks) => {
                self.spotify_error = None;
                self.spotify_rows = tracks;
            }
            Err(error) => {
                self.spotify_error = Some(spotify_search_notice(&error.to_string()));
                self.spotify_rows.clear();
            }
        }
        self.publish_results();
    }

    fn apply_youtube(&mut self, generation: u64, result: Result<Vec<Track>, music::Error>) {
        if generation != self.generation || self.query.trim().is_empty() {
            return;
        }
        self.youtube_pending = false;

        match result {
            Ok(tracks) => {
                self.error = None;
                self.youtube_rows = tracks;
            }
            Err(error) => {
                self.error = Some(format!("YouTube Music search failed: {error}"));
                self.youtube_rows.clear();
            }
        }
        self.publish_results();
    }

    fn publish_results(&mut self) {
        // YouTube Music is the row a click plays when the same recording
        // is on both services. A Spotify-only hit stays a Spotify row.
        self.results = search::merge_results(
            &[&self.spotify_rows, &self.youtube_rows],
            Source::YouTubeMusic,
        );
    }

    fn on_frame(&mut self) -> Task<Message> {
        match &self.mode {
            Mode::Smoke => match self.smoke_frame() {
                SmokeFrame::Continue | SmokeFrame::Ignore => Task::none(),
                SmokeFrame::RequestExit => {
                    eprintln!(
                        "codevox-mu smoke: rendered {SMOKE_FRAME_COUNT_LIMIT} frames, exiting"
                    );
                    iced::exit()
                }
            },
            Mode::Screenshot { .. } => self.on_screenshot_frame(),
            Mode::Run => Task::none(),
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
        match &self.mode {
            // Smoke must not open a D-Bus session or a media-controls handle.
            Mode::Smoke => window::frames().map(|_| Message::Frame),
            // Screenshot paints, captures search, accounts, and library, then exits.
            // No media keys and no account restore.
            Mode::Screenshot { .. } => Subscription::batch([
                window::frames().map(|_| Message::Frame),
                window::open_events().map(Message::WindowOpened),
            ]),
            Mode::Run => Subscription::batch([
                crate::media_controls::events().map(Message::Media),
                window::open_events().map(Message::BindMedia),
                window::resize_events().map(|(id, _size)| Message::WindowResized(id)),
            ]),
        }
    }

    fn theme(_shell: &Self) -> iced::Theme {
        crate::theme::theme()
    }

    fn displayed_now(&self) -> player::NowPlaying {
        self.preview.clone().unwrap_or_else(|| self.playback.now())
    }

    fn dismiss_notices(&mut self) {
        self.error = None;
        self.spotify_error = None;
        self.mix_error = None;
        if matches!(self.phase, AccountPhase::Failed { .. }) {
            self.phase = AccountPhase::SignedOut;
        }
        if let Some(error) = self.displayed_now().error {
            self.dismissed_playback_error = Some(error);
        }
    }

    fn window_action(
        &self,
        action: impl Fn(window::Id) -> Task<Message> + Send + 'static,
    ) -> Task<Message> {
        if let Some(id) = self.window_id {
            action(id)
        } else {
            window::latest().and_then(action)
        }
    }

    /// Offline rows and a sample transport, so the headless captures show the layout.
    fn seed_screenshot(&mut self) {
        self.query = "night".into();
        self.spotify_rows = music::MockService::spotify()
            .search("night")
            .unwrap_or_default();
        self.youtube_rows = music::MockService::youtube_music()
            .search("night")
            .unwrap_or_default();
        self.publish_results();
        let Some(track) = self
            .youtube_rows
            .first()
            .cloned()
            .or_else(|| self.spotify_rows.first().cloned())
        else {
            return;
        };
        self.playlists = sample_library();
        self.preview = Some(player::NowPlaying {
            title: track.title.clone(),
            artist: track.artists.join(", "),
            phase: Phase::Playing,
            elapsed: std::time::Duration::from_secs(48),
            duration: track.duration,
            error: None,
            note: None,
            can_play: false,
            can_pause: true,
            can_stop: true,
            can_next: true,
            can_previous: true,
            source: Some(track.source),
            track_id: track.id.clone(),
            loaded: Some(track),
            upcoming: Vec::new(),
        });
    }

    fn on_screenshot_frame(&mut self) -> Task<Message> {
        if self.exit_requested {
            return Task::none();
        }
        self.frame_count = self.frame_count.saturating_add(1);
        let Some(id) = self.window_id else {
            return Task::none();
        };
        let ready = matches!(
            self.shot_step,
            ShotStep::Main | ShotStep::Accounts | ShotStep::Library
        ) && self.frame_count >= SHOT_FRAME_DELAY;
        if !ready {
            return Task::none();
        }
        let (kind, path) = match self.shot_step {
            ShotStep::Main => (Capture::Main, self.screenshot_main.clone()),
            ShotStep::Accounts => (Capture::Accounts, self.screenshot_accounts.clone()),
            ShotStep::Library => (Capture::Library, self.screenshot_library.clone()),
            ShotStep::Idle
            | ShotStep::MainPending
            | ShotStep::AccountsPending
            | ShotStep::LibraryPending => {
                return Task::none();
            }
        };
        let Some(path) = path else {
            eprintln!("codevox-mu screenshot: missing output path");
            let _ = std::io::Write::flush(&mut std::io::stderr());
            std::process::exit(1);
        };
        self.shot_step = match kind {
            Capture::Main => ShotStep::MainPending,
            Capture::Accounts => ShotStep::AccountsPending,
            Capture::Library => ShotStep::LibraryPending,
        };
        window::screenshot(id).map(move |shot| {
            let result = write_png(&path, shot.rgba.as_ref(), shot.size.width, shot.size.height);
            Message::Captured { kind, result }
        })
    }

    fn finish_capture(&mut self, kind: Capture, result: Result<(), String>) -> Task<Message> {
        if let Err(error) = result {
            eprintln!("codevox-mu screenshot: {error}");
            let _ = std::io::Write::flush(&mut std::io::stderr());
            std::process::exit(1);
        }
        match kind {
            Capture::Main => {
                if let Some(path) = &self.screenshot_main {
                    eprintln!("codevox-mu screenshot: wrote {}", path.display());
                }
                self.pane = Pane::Accounts;
                self.cookie_open = true;
                self.frame_count = 0;
                self.shot_step = ShotStep::Accounts;
                Task::none()
            }
            Capture::Accounts => {
                if let Some(path) = &self.screenshot_accounts {
                    eprintln!("codevox-mu screenshot: wrote {}", path.display());
                }
                self.pane = Pane::Library;
                self.phase = AccountPhase::SignedIn {
                    name: "Ada Vega".into(),
                };
                self.sign = YtSign::In;
                self.cookie_open = false;
                self.library_loading = false;
                self.frame_count = 0;
                self.shot_step = ShotStep::Library;
                Task::none()
            }
            Capture::Library => {
                if let Some(path) = &self.screenshot_library {
                    eprintln!("codevox-mu screenshot: wrote {}", path.display());
                }
                self.exit_requested = true;
                eprintln!("codevox-mu screenshot: library view saved, exiting");
                iced::exit()
            }
        }
    }

    fn view(shell: &Self) -> Element<'_, Message> {
        let layout = column![
            title_bar(shell),
            horizontal_rule(),
            row![sidebar(shell), vertical_rule(), main_column(shell)]
                .width(Fill)
                .height(Fill),
            horizontal_rule(),
            now_playing(shell),
        ]
        .width(Fill)
        .height(Fill);

        let framed = stack([layout.into(), resize_overlay()])
            .width(Fill)
            .height(Fill);

        container(framed)
            .width(Fill)
            .height(Fill)
            .style(window_background)
            .into()
    }
}

fn title_bar(shell: &Shell) -> Element<'_, Message> {
    let brand = row![
        container(
            Space::new()
                .width(ACCENT_MARK_WIDTH)
                .height(ACCENT_MARK_HEIGHT)
        )
        .style(accent_mark),
        text(TITLE)
            .font(crate::theme::HEADING)
            .size(15.0)
            .color(crate::theme::TEXT_PRIMARY),
    ]
    .spacing(10)
    .align_y(Vertical::Center);

    let drag = mouse_area(
        container(brand)
            .width(Fill)
            .height(Fill)
            .align_y(Vertical::Center),
    )
    .on_press(Message::DragWindow)
    .on_double_click(Message::ToggleMaximize)
    .interaction(iced::mouse::Interaction::Grab);

    let maximize = if shell.maximized { "Restore" } else { "Max" };
    let buttons = row![
        caption_button("Min", Message::MinimizeWindow, false),
        caption_button(maximize, Message::ToggleMaximize, false),
        caption_button("Close", Message::CloseWindow, true),
    ]
    .spacing(6)
    .align_y(Vertical::Center);

    container(
        row![drag, buttons]
            .align_y(Vertical::Center)
            .width(Fill)
            .height(Fill),
    )
    .width(Fill)
    .height(TITLE_BAR_HEIGHT)
    .padding([0.0, 16.0])
    .style(title_bar_style)
    .into()
}

fn sidebar(shell: &Shell) -> Element<'_, Message> {
    let nav = column![
        nav_item(
            "Search",
            Message::ShowSearch,
            matches!(shell.pane, Pane::Search),
        ),
        nav_item(
            "Queue",
            Message::ShowQueue,
            matches!(shell.pane, Pane::Queue),
        ),
        nav_item(
            "Library",
            Message::ShowLibrary,
            matches!(shell.pane, Pane::Library | Pane::Playlist),
        ),
        nav_item(
            "Playlists",
            Message::ShowMixes,
            matches!(shell.pane, Pane::Mixes | Pane::Mix),
        ),
        nav_item(
            "Accounts",
            if shell.sign == YtSign::Out {
                Message::ShowSignIn
            } else {
                Message::ShowAccounts
            },
            matches!(shell.pane, Pane::Accounts),
        ),
    ]
    .spacing(4)
    .width(Fill);

    container(
        column![nav, Space::new().height(Fill), compact_accounts(shell)]
            .spacing(12)
            .width(Fill)
            .height(Fill),
    )
    .padding(12)
    .width(SIDEBAR_WIDTH)
    .height(Fill)
    .style(sidebar_style)
    .into()
}

fn nav_item(label: &'static str, message: Message, selected: bool) -> Element<'static, Message> {
    let mark = container(Space::new().width(2.0).height(16.0))
        .width(2.0)
        .style(move |_theme| {
            if selected {
                container::background(crate::theme::ACCENT)
            } else {
                container::Style::default()
            }
        });
    row![
        mark,
        button(text(label).font(crate::theme::BODY).size(BODY_SIZE))
            .on_press(message)
            .width(Fill)
            .padding([8.0, 10.0])
            .style(move |_theme, status| nav_style(status, selected)),
    ]
    .spacing(8)
    .align_y(Vertical::Center)
    .into()
}

fn compact_accounts(shell: &Shell) -> Element<'_, Message> {
    let (spotify, spotify_color) = spotify_compact(&shell.phase);
    let (youtube, youtube_color) = youtube_compact(shell.sign);
    column![
        compact_line("Spotify", spotify, spotify_color),
        compact_line("YouTube Music", youtube, youtube_color),
    ]
    .spacing(10)
    .width(Fill)
    .into()
}

fn compact_line(
    service: &'static str,
    state: &'static str,
    color: Color,
) -> Element<'static, Message> {
    column![
        text(service)
            .font(crate::theme::BODY)
            .size(SOURCE_SIZE)
            .color(crate::theme::TEXT_SECONDARY),
        text(state)
            .font(crate::theme::BODY)
            .size(SOURCE_SIZE)
            .color(color),
    ]
    .spacing(1)
    .width(Fill)
    .into()
}

fn spotify_compact(phase: &AccountPhase) -> (&'static str, Color) {
    match phase {
        AccountPhase::SignedOut => ("signed out", crate::theme::TEXT_SECONDARY),
        AccountPhase::SigningIn => ("signing in", crate::theme::TEXT_SECONDARY),
        AccountPhase::SignedIn { .. } => ("signed in", crate::theme::TEXT_PRIMARY),
        AccountPhase::NotPremium { .. } => ("not Premium", crate::theme::WARNING),
        AccountPhase::Failed { message } if message == SPOTIFY_RENEW_SIGN_IN => {
            ("sign-in needs to be renewed", crate::theme::DESTRUCTIVE)
        }
        AccountPhase::Failed { .. } => ("sign-in failed", crate::theme::DESTRUCTIVE),
    }
}

/// Saved Web API credentials that no longer refresh. Spotify writes this sentence.
const SPOTIFY_RENEW_SIGN_IN: &str = "Spotify sign-in needs to be renewed.";

/// Search failures that are already a full Spotify sentence stay as written.
///
/// Rate limits from the Web API are one of those sentences. Other failures
/// still get a short prefix so the banner says which search failed.
fn spotify_search_notice(detail: &str) -> String {
    if detail.starts_with("Spotify ") {
        detail.to_string()
    } else {
        format!("Spotify search failed: {detail}")
    }
}

fn spotify_library_notice(detail: &str) -> String {
    if detail.starts_with("Spotify ") {
        detail.to_string()
    } else {
        format!("Spotify library failed: {detail}")
    }
}

fn spotify_signed_in(phase: &AccountPhase) -> bool {
    matches!(
        phase,
        AccountPhase::SignedIn { .. } | AccountPhase::NotPremium { .. }
    )
}

fn youtube_signed_in(shell: &Shell) -> bool {
    shell.sign == YtSign::In && shell.library.signed_in()
}

fn library_accounts_open(shell: &Shell) -> bool {
    spotify_signed_in(&shell.phase) || youtube_signed_in(shell)
}

/// `None` means that service was not signed in, so it was not asked.
fn merge_library(
    spotify: Option<Result<Vec<Playlist>, music::Error>>,
    youtube: Option<Result<Vec<Playlist>, music::Error>>,
) -> LibraryLoad {
    let mut load = LibraryLoad {
        playlists: Vec::new(),
        spotify_error: None,
        youtube_error: None,
    };
    if let Some(result) = spotify {
        match result {
            Ok(rows) => load.playlists.extend(rows),
            Err(error) => load.spotify_error = Some(spotify_library_notice(&error.to_string())),
        }
    }
    if let Some(result) = youtube {
        match result {
            Ok(rows) => load.playlists.extend(rows),
            Err(error) => {
                load.youtube_error = Some(format!("YouTube Music library failed: {error}"));
            }
        }
    }
    load
}

fn load_signed_in_libraries(
    spotify_in: bool,
    youtube_in: bool,
    catalogue: &dyn MusicService,
    library: &dyn LibrarySession,
) -> LibraryLoad {
    let spotify = spotify_in.then(|| catalogue.playlists());
    let youtube = youtube_in.then(|| library.playlists());
    merge_library(spotify, youtube)
}

fn playlist_failure(source: Option<Source>, error: &music::Error) -> String {
    let detail = error.to_string();
    match source {
        Some(Source::Spotify) if detail.starts_with("Spotify ") => detail,
        Some(Source::Spotify) => format!("Spotify playlist failed: {detail}"),
        Some(Source::YouTubeMusic) | None => format!("YouTube Music playlist failed: {detail}"),
        Some(Source::Local) => format!("Playlist failed: {detail}"),
    }
}

fn sample_library() -> Vec<Playlist> {
    vec![
        Playlist {
            id: "liked-songs".into(),
            source: Source::Spotify,
            name: "Liked Songs".into(),
        },
        Playlist {
            id: "sp-low-glow".into(),
            source: Source::Spotify,
            name: "Low Glow".into(),
        },
        Playlist {
            id: "sp-harbour".into(),
            source: Source::Spotify,
            name: "Harbour".into(),
        },
        Playlist {
            id: "liked-songs".into(),
            source: Source::YouTubeMusic,
            name: "Liked songs".into(),
        },
        Playlist {
            id: "yt-weekday".into(),
            source: Source::YouTubeMusic,
            name: "Weekday".into(),
        },
        Playlist {
            id: "yt-sessions".into(),
            source: Source::YouTubeMusic,
            name: "Sessions".into(),
        },
    ]
}

fn youtube_compact(sign: YtSign) -> (&'static str, Color) {
    match sign {
        YtSign::Out => ("signed out", crate::theme::TEXT_SECONDARY),
        YtSign::Busy => ("signing in", crate::theme::TEXT_SECONDARY),
        YtSign::In => ("signed in", crate::theme::TEXT_PRIMARY),
    }
}

fn main_column(shell: &Shell) -> Element<'_, Message> {
    let mut header = column![].spacing(SECTION_SPACING).width(Fill);
    if let Some(banner) = notice_banner_if_any(shell) {
        header = header.push(banner);
    }
    header = header.push(pane_title(shell));
    if shell.pane == Pane::Search {
        header = header.push(search_input(shell));
    }
    if shell.pane == Pane::Mixes {
        header = header.push(create_saved_row(shell));
    }
    if shell.pane == Pane::Mix {
        header = header.push(saved_toolbar(shell));
    }
    if let Some(note) = status_note(shell) {
        header = header.push(status_text(note, crate::theme::TEXT_SECONDARY));
    }

    let body = match shell.pane {
        Pane::Search => scrollable(
            column(shell.results.iter().map(result_row))
                .spacing(RESULT_SPACING)
                .width(Fill),
        )
        .height(Fill)
        .width(Fill)
        .into(),
        Pane::Queue => queue_view(shell),
        Pane::Library => scrollable(
            column(shell.playlists.iter().map(playlist_row))
                .spacing(RESULT_SPACING)
                .width(Fill),
        )
        .height(Fill)
        .width(Fill)
        .into(),
        Pane::Playlist => scrollable(
            column(shell.playlist_tracks.iter().map(library_track_row))
                .spacing(RESULT_SPACING)
                .width(Fill),
        )
        .height(Fill)
        .width(Fill)
        .into(),
        Pane::Accounts => scrollable(accounts_view(shell))
            .height(Fill)
            .width(Fill)
            .into(),
        Pane::Mixes => scrollable(
            column(shell.mixes.playlists().iter().map(saved_row))
                .spacing(RESULT_SPACING)
                .width(Fill),
        )
        .height(Fill)
        .width(Fill)
        .into(),
        Pane::Mix => {
            let tracks = shell
                .selected_mix()
                .map(|playlist| playlist.tracks())
                .unwrap_or(&[]);
            let last = tracks.len().saturating_sub(1);
            scrollable(
                column(
                    tracks
                        .iter()
                        .enumerate()
                        .map(|(index, track)| saved_track_row(index, track, index == last)),
                )
                .spacing(RESULT_SPACING)
                .width(Fill),
            )
            .height(Fill)
            .width(Fill)
            .into()
        }
    };

    container(
        column![header, body]
            .spacing(SECTION_SPACING)
            .width(Fill)
            .height(Fill),
    )
    .padding(CONTENT_PADDING)
    .width(Fill)
    .height(Fill)
    .style(main_surface)
    .into()
}

fn pane_title(shell: &Shell) -> Element<'_, Message> {
    let words = match shell.pane {
        Pane::Search => "Search".to_string(),
        Pane::Queue => "Queue".to_string(),
        Pane::Library => "Library".to_string(),
        Pane::Accounts => "Accounts".to_string(),
        Pane::Mixes => "Playlists".to_string(),
        Pane::Mix => shell
            .selected_mix()
            .map(|playlist| playlist.name().to_string())
            .unwrap_or_else(|| "Playlist".to_string()),
        Pane::Playlist if shell.playlist_name.is_empty() => "Playlist".to_string(),
        Pane::Playlist => shell.playlist_name.clone(),
    };
    let heading = text(words)
        .font(crate::theme::HEADING)
        .size(HEADING_SIZE)
        .color(crate::theme::TEXT_PRIMARY);
    if shell.pane == Pane::Playlist {
        row![
            transport_button("Back", Some(Message::BackToLibrary), false),
            heading,
        ]
        .spacing(12)
        .align_y(Vertical::Center)
        .into()
    } else if shell.pane == Pane::Mix {
        row![
            transport_button("Back", Some(Message::ShowMixes), false),
            heading,
        ]
        .spacing(12)
        .align_y(Vertical::Center)
        .into()
    } else {
        heading.into()
    }
}

fn search_input(shell: &Shell) -> Element<'_, Message> {
    text_input("Search tracks", &shell.query)
        .on_input(Message::QueryChanged)
        .padding([10.0, 12.0])
        .size(BODY_SIZE)
        .font(crate::theme::BODY)
        .style(search_field)
        .into()
}

fn accounts_view(shell: &Shell) -> Element<'_, Message> {
    let presentation = spotify::present_account(&shell.phase);
    let mut spotify_block = column![
        text("Spotify")
            .font(crate::theme::BODY_MEDIUM)
            .size(META_SIZE)
            .color(crate::theme::TEXT_SECONDARY),
        text(presentation.text)
            .font(crate::theme::BODY)
            .size(BODY_SIZE)
            .color(tone_color(presentation.tone)),
    ]
    .spacing(8)
    .width(Fill);
    if presentation.offer_sign_in {
        spotify_block = spotify_block.push(transport_button(
            "Sign in to Spotify",
            Some(Message::SignIn),
            true,
        ));
    }

    let youtube_block = match shell.sign {
        YtSign::In => column![
            section_label("YouTube Music"),
            text("Signed in.")
                .font(crate::theme::BODY)
                .size(BODY_SIZE)
                .color(crate::theme::TEXT_PRIMARY),
            transport_button("Sign out", Some(Message::SignOut), false),
        ]
        .spacing(8)
        .width(Fill),
        YtSign::Busy => column![
            section_label("YouTube Music"),
            text("Signing in to YouTube Music…")
                .font(crate::theme::BODY)
                .size(BODY_SIZE)
                .color(crate::theme::TEXT_SECONDARY),
        ]
        .spacing(8)
        .width(Fill),
        YtSign::Out => column![
            section_label("YouTube Music"),
            text("Signed out.")
                .font(crate::theme::BODY)
                .size(BODY_SIZE)
                .color(crate::theme::TEXT_SECONDARY),
            cookie_form(shell),
        ]
        .spacing(8)
        .width(Fill),
    };

    column![spotify_block, youtube_block]
        .spacing(28)
        .width(Fill)
        .into()
}

fn section_label(label: &'static str) -> Element<'static, Message> {
    text(label)
        .font(crate::theme::BODY_MEDIUM)
        .size(META_SIZE)
        .color(crate::theme::TEXT_SECONDARY)
        .into()
}

fn tone_color(tone: spotify::AccountTone) -> Color {
    match tone {
        spotify::AccountTone::Muted => crate::theme::TEXT_SECONDARY,
        spotify::AccountTone::Normal => crate::theme::TEXT_PRIMARY,
        spotify::AccountTone::Warning => crate::theme::WARNING,
        spotify::AccountTone::Error => crate::theme::DESTRUCTIVE,
    }
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
    column![help, input, buttons].spacing(8).width(Fill).into()
}

fn status_note(shell: &Shell) -> Option<String> {
    if shell.pane == Pane::Playlist
        && !shell.library_loading
        && let Some(note) = &shell.playlist_note
    {
        return Some(note.clone());
    }
    let fixed = match shell.pane {
        Pane::Search if shell.spotify_pending || shell.youtube_pending => Some("Searching…"),
        Pane::Search if !shell.query.trim().is_empty() && shell.results.is_empty() => {
            Some("No matches")
        }
        Pane::Library if shell.library_loading => Some("Loading library…"),
        Pane::Library
            if shell.playlists.is_empty()
                && shell.error.is_none()
                && shell.spotify_error.is_none() =>
        {
            if library_accounts_open(shell) {
                Some("No playlists")
            } else {
                Some("Sign in to Spotify or YouTube Music to see your library.")
            }
        }
        Pane::Playlist if shell.library_loading => Some("Loading playlist…"),
        Pane::Playlist
            if shell.playlist_tracks.is_empty()
                && shell.error.is_none()
                && shell.spotify_error.is_none() =>
        {
            Some("This playlist is empty.")
        }
        Pane::Mixes if shell.mixes.playlists().is_empty() => Some("No playlists yet."),
        Pane::Mix
            if shell
                .selected_mix()
                .is_none_or(|playlist| playlist.tracks().is_empty()) =>
        {
            Some("This playlist is empty.")
        }
        Pane::Search
        | Pane::Queue
        | Pane::Library
        | Pane::Playlist
        | Pane::Accounts
        | Pane::Mixes
        | Pane::Mix => None,
    };
    fixed.map(str::to_string)
}

fn notice_banner_if_any(shell: &Shell) -> Option<Element<'static, Message>> {
    let lines = notice_lines(shell);
    if lines.is_empty() {
        None
    } else {
        Some(notice_banner(lines))
    }
}

fn notice_lines(shell: &Shell) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(error) = &shell.spotify_error {
        push_notice(&mut lines, error.clone());
    }
    if let Some(error) = &shell.error {
        push_notice(&mut lines, error.clone());
    }
    if let Some(error) = &shell.mix_error {
        push_notice(&mut lines, error.clone());
    }
    if let AccountPhase::Failed { message } = &shell.phase {
        push_notice(&mut lines, message.clone());
    }
    if let Some(error) = shell.displayed_now().error
        && shell.dismissed_playback_error.as_ref() != Some(&error)
    {
        push_notice(&mut lines, error);
    }
    lines
}

fn push_notice(lines: &mut Vec<String>, line: String) {
    if !line.is_empty() && !lines.iter().any(|existing| existing == &line) {
        lines.push(line);
    }
}

fn notice_banner(lines: Vec<String>) -> Element<'static, Message> {
    let messages = column(lines.into_iter().map(|line| {
        text(line)
            .font(crate::theme::BODY)
            .size(META_SIZE)
            .color(crate::theme::DESTRUCTIVE)
            .width(Fill)
            .into()
    }))
    .spacing(4)
    .width(Fill);

    container(
        row![
            messages,
            transport_button("Dismiss", Some(Message::DismissNotices), false),
        ]
        .spacing(12)
        .align_y(Vertical::Center),
    )
    .padding([10.0, 12.0])
    .width(Fill)
    .style(notice_style)
    .into()
}

fn status_text(label: impl Into<String>, color: Color) -> Element<'static, Message> {
    text(label.into())
        .font(crate::theme::BODY)
        .size(META_SIZE)
        .color(color)
        .into()
}

fn now_playing(shell: &Shell) -> Element<'_, Message> {
    let now = shell.displayed_now();
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

    let mut title_row = row![
        text(title)
            .font(crate::theme::BODY)
            .size(BODY_SIZE)
            .color(title_color)
    ]
    .spacing(8)
    .align_y(Vertical::Center);
    if let Some(source) = now.source {
        title_row = title_row.push(source_tag(source));
    }

    let mut details = column![title_row].spacing(2).width(Fill);
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

    let (play_label, play_message) = if now.can_pause {
        ("Pause", Some(Message::TogglePlayback))
    } else if now.can_play {
        ("Play", Some(Message::TogglePlayback))
    } else {
        ("Play", None)
    };
    let controls = row![
        transport_button(
            "Prev",
            now.can_previous.then_some(Message::PreviousTrack),
            false,
        ),
        transport_button(play_label, play_message, true),
        transport_button("Next", now.can_next.then_some(Message::NextTrack), false),
        transport_button("Stop", now.can_stop.then_some(Message::StopPlayback), false),
    ]
    .spacing(8)
    .align_y(Vertical::Center);

    let mut bar = column![
        row![details, controls]
            .spacing(ROW_SPACING)
            .align_y(Vertical::Center)
    ]
    .spacing(8)
    .width(Fill);

    if let Some(ratio) = progress_ratio(&now) {
        let time = format!(
            "{} / {}",
            format_duration(now.elapsed),
            format_duration(now.duration)
        );
        bar = bar.push(
            row![
                progress_bar(0.0..=1.0, ratio)
                    .length(Fill)
                    .girth(6.0)
                    .style(transport_progress),
                text(time)
                    .font(crate::theme::BODY)
                    .size(META_SIZE)
                    .color(crate::theme::TEXT_TERTIARY),
            ]
            .spacing(12)
            .align_y(Vertical::Center),
        );
    }

    container(bar)
        .width(Fill)
        .padding([12.0, 16.0])
        .style(transport_bar)
        .into()
}

fn progress_ratio(now: &player::NowPlaying) -> Option<f32> {
    let total = now.duration.as_secs_f32();
    if total <= 0.0 {
        return None;
    }
    Some((now.elapsed.as_secs_f32() / total).clamp(0.0, 1.0))
}

fn queue_view(shell: &Shell) -> Element<'_, Message> {
    let now = shell.displayed_now();
    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    if !now.title.is_empty() {
        rows.push(current_row(&now));
    }
    let last = now.upcoming.len().saturating_sub(1);
    for (index, track) in now.upcoming.iter().enumerate() {
        rows.push(queued_row(index, track, index == last));
    }
    if rows.is_empty() {
        text("Nothing queued")
            .font(crate::theme::BODY)
            .size(META_SIZE)
            .color(crate::theme::TEXT_TERTIARY)
            .into()
    } else {
        scrollable(column(rows).spacing(8).width(Fill))
            .height(Fill)
            .width(Fill)
            .into()
    }
}

fn resize_overlay() -> Element<'static, Message> {
    // Edges first, corners last, so a corner grab wins where the strips meet.
    stack([
        resize_grip(
            Direction::North,
            Fill,
            RESIZE_EDGE,
            Horizontal::Center,
            Vertical::Top,
        ),
        resize_grip(
            Direction::South,
            Fill,
            RESIZE_EDGE,
            Horizontal::Center,
            Vertical::Bottom,
        ),
        resize_grip(
            Direction::West,
            RESIZE_EDGE,
            Fill,
            Horizontal::Left,
            Vertical::Center,
        ),
        resize_grip(
            Direction::East,
            RESIZE_EDGE,
            Fill,
            Horizontal::Right,
            Vertical::Center,
        ),
        resize_grip(
            Direction::NorthWest,
            RESIZE_CORNER,
            RESIZE_CORNER,
            Horizontal::Left,
            Vertical::Top,
        ),
        resize_grip(
            Direction::NorthEast,
            RESIZE_CORNER,
            RESIZE_CORNER,
            Horizontal::Right,
            Vertical::Top,
        ),
        resize_grip(
            Direction::SouthWest,
            RESIZE_CORNER,
            RESIZE_CORNER,
            Horizontal::Left,
            Vertical::Bottom,
        ),
        resize_grip(
            Direction::SouthEast,
            RESIZE_CORNER,
            RESIZE_CORNER,
            Horizontal::Right,
            Vertical::Bottom,
        ),
    ])
    .width(Fill)
    .height(Fill)
    .into()
}

fn resize_grip(
    direction: Direction,
    width: impl Into<Length>,
    height: impl Into<Length>,
    align_x: Horizontal,
    align_y: Vertical,
) -> Element<'static, Message> {
    let cursor = resize_cursor(direction);
    container(
        mouse_area(Space::new().width(width).height(height))
            .on_press(Message::Resize(direction))
            .interaction(cursor),
    )
    .width(Fill)
    .height(Fill)
    .align_x(align_x)
    .align_y(align_y)
    .into()
}

fn resize_cursor(direction: Direction) -> iced::mouse::Interaction {
    match direction {
        Direction::North | Direction::South => iced::mouse::Interaction::ResizingVertically,
        Direction::East | Direction::West => iced::mouse::Interaction::ResizingHorizontally,
        Direction::NorthEast | Direction::SouthWest => {
            iced::mouse::Interaction::ResizingDiagonallyUp
        }
        Direction::NorthWest | Direction::SouthEast => {
            iced::mouse::Interaction::ResizingDiagonallyDown
        }
    }
}

fn horizontal_rule() -> Element<'static, Message> {
    container(Space::new().width(Fill).height(1.0))
        .width(Fill)
        .height(1.0)
        .style(hairline_fill)
        .into()
}

fn vertical_rule() -> Element<'static, Message> {
    container(Space::new().width(1.0).height(Fill))
        .width(1.0)
        .height(Fill)
        .style(hairline_fill)
        .into()
}

fn caption_button(label: &'static str, message: Message, close: bool) -> Element<'static, Message> {
    button(text(label).font(crate::theme::BODY).size(META_SIZE))
        .on_press(message)
        .padding([5.0, 10.0])
        .style(move |_theme, status| caption_style(status, close))
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

fn window_background(_theme: &iced::Theme) -> container::Style {
    container::Style {
        text_color: Some(crate::theme::TEXT_PRIMARY),
        background: Some(crate::theme::BACKGROUND.into()),
        ..container::Style::default()
    }
}

fn title_bar_style(_theme: &iced::Theme) -> container::Style {
    container::Style {
        text_color: Some(crate::theme::TEXT_PRIMARY),
        background: Some(crate::theme::PANEL.into()),
        ..container::Style::default()
    }
}

fn sidebar_style(_theme: &iced::Theme) -> container::Style {
    container::Style {
        text_color: Some(crate::theme::TEXT_PRIMARY),
        background: Some(crate::theme::PANEL.into()),
        ..container::Style::default()
    }
}

fn main_surface(_theme: &iced::Theme) -> container::Style {
    container::Style {
        text_color: Some(crate::theme::TEXT_PRIMARY),
        background: Some(crate::theme::BACKGROUND.into()),
        ..container::Style::default()
    }
}

fn hairline_fill(_theme: &iced::Theme) -> container::Style {
    container::background(crate::theme::HAIRLINE)
}

fn notice_style(_theme: &iced::Theme) -> container::Style {
    container::Style {
        text_color: Some(crate::theme::DESTRUCTIVE),
        background: Some(crate::theme::DARK_LIGHT.into()),
        border: Border {
            color: crate::theme::DESTRUCTIVE,
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

fn transport_bar(_theme: &iced::Theme) -> container::Style {
    container::Style {
        text_color: Some(crate::theme::TEXT_PRIMARY),
        background: Some(crate::theme::DARK_LIGHT.into()),
        ..container::Style::default()
    }
}

fn transport_progress(_theme: &iced::Theme) -> progress_bar::Style {
    progress_bar::Style {
        // The empty track has to read on DARK_LIGHT. DARK_LIGHTER alone disappears.
        background: mix(crate::theme::DARK_LIGHTER, crate::theme::TEXT_PRIMARY, 0.22).into(),
        bar: crate::theme::ACCENT.into(),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: 0.0.into(),
        },
    }
}

fn nav_style(status: button::Status, selected: bool) -> button::Style {
    let (background, text_color) = match (selected, status) {
        (_, button::Status::Disabled) => (None, crate::theme::TEXT_TERTIARY),
        (true, button::Status::Pressed) => {
            (Some(crate::theme::DARK_LIGHT), crate::theme::TEXT_PRIMARY)
        }
        (true, button::Status::Hovered) => (
            Some(mix(
                crate::theme::DARK_LIGHT,
                crate::theme::TEXT_PRIMARY,
                0.08,
            )),
            crate::theme::TEXT_PRIMARY,
        ),
        (true, button::Status::Active) => {
            (Some(crate::theme::DARK_LIGHT), crate::theme::TEXT_PRIMARY)
        }
        (false, button::Status::Pressed) => {
            (Some(crate::theme::DARK_LIGHT), crate::theme::TEXT_PRIMARY)
        }
        (false, button::Status::Hovered) => {
            (Some(crate::theme::DARK_LIGHTER), crate::theme::TEXT_PRIMARY)
        }
        (false, button::Status::Active) => (None, crate::theme::TEXT_SECONDARY),
    };
    flat_button(
        background.map(Background::Color),
        text_color,
        Color::TRANSPARENT,
    )
}

fn caption_style(status: button::Status, close: bool) -> button::Style {
    let (background, text_color) = if close {
        match status {
            button::Status::Disabled => (crate::theme::DARK_LIGHT, crate::theme::TEXT_TERTIARY),
            button::Status::Active => (crate::theme::DARK_LIGHTER, crate::theme::TEXT_PRIMARY),
            button::Status::Hovered => (crate::theme::DESTRUCTIVE, crate::theme::TEXT_PRIMARY),
            button::Status::Pressed => (
                mix(crate::theme::DESTRUCTIVE, crate::theme::BACKGROUND, 0.28),
                crate::theme::TEXT_PRIMARY,
            ),
        }
    } else {
        match status {
            button::Status::Disabled => (crate::theme::DARK_LIGHT, crate::theme::TEXT_TERTIARY),
            button::Status::Active => (crate::theme::DARK_LIGHTER, crate::theme::TEXT_PRIMARY),
            button::Status::Hovered => (
                mix(crate::theme::DARK_LIGHTER, crate::theme::TEXT_PRIMARY, 0.14),
                crate::theme::TEXT_PRIMARY,
            ),
            button::Status::Pressed => (crate::theme::DARK_LIGHT, crate::theme::TEXT_PRIMARY),
        }
    };
    flat_button(
        Some(Background::Color(background)),
        text_color,
        crate::theme::HAIRLINE,
    )
}

fn transport_button_style(status: button::Status, primary: bool) -> button::Style {
    let (background, text_color) = match (primary, status) {
        (_, button::Status::Disabled) => (crate::theme::DARK_LIGHT, crate::theme::TEXT_TERTIARY),
        (true, button::Status::Active) => (crate::theme::ACCENT, crate::theme::ON_ACCENT),
        (true, button::Status::Hovered) => (
            mix(crate::theme::ACCENT, crate::theme::TEXT_PRIMARY, 0.16),
            crate::theme::ON_ACCENT,
        ),
        (true, button::Status::Pressed) => (
            mix(crate::theme::ACCENT, crate::theme::BACKGROUND, 0.22),
            crate::theme::ON_ACCENT,
        ),
        (false, button::Status::Active) => (crate::theme::DARK_LIGHTER, crate::theme::TEXT_PRIMARY),
        (false, button::Status::Hovered) => (
            mix(crate::theme::DARK_LIGHTER, crate::theme::TEXT_PRIMARY, 0.12),
            crate::theme::TEXT_PRIMARY,
        ),
        (false, button::Status::Pressed) => (crate::theme::DARK_LIGHT, crate::theme::TEXT_PRIMARY),
    };
    flat_button(
        Some(Background::Color(background)),
        text_color,
        crate::theme::HAIRLINE,
    )
}

fn result_button(_theme: &iced::Theme, status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered => Some(Background::Color(crate::theme::DARK_LIGHTER)),
        button::Status::Pressed => Some(Background::Color(crate::theme::DARK_LIGHT)),
        button::Status::Active | button::Status::Disabled => None,
    };
    flat_button(background, crate::theme::TEXT_PRIMARY, Color::TRANSPARENT)
}

fn flat_button(background: Option<Background>, text_color: Color, border: Color) -> button::Style {
    button::Style {
        background,
        text_color,
        border: Border {
            color: border,
            width: if border == Color::TRANSPARENT {
                0.0
            } else {
                1.0
            },
            radius: 0.0.into(),
        },
        shadow: Shadow {
            blur_radius: crate::theme::GLOW,
            ..Shadow::default()
        },
        snap: false,
    }
}

fn mix(from: Color, to: Color, amount: f32) -> Color {
    Color {
        r: from.r + (to.r - from.r) * amount,
        g: from.g + (to.g - from.g) * amount,
        b: from.b + (to.b - from.b) * amount,
        a: from.a + (to.a - from.a) * amount,
    }
}

fn write_png(path: &Path, rgba: &[u8], width: u32, height: u32) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err("screenshot was empty".into());
    }
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "screenshot is too large".to_string())?;
    if rgba.len() < expected {
        return Err(format!(
            "screenshot is {} bytes, expected {expected}",
            rgba.len()
        ));
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let file = std::fs::File::create(path).map_err(|error| error.to_string())?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|error| error.to_string())?;
    writer
        .write_image_data(&rgba[..expected])
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[test]
fn the_window_is_borderless_opaque_and_has_a_minimum_size() {
    let settings = window_settings();
    assert!(!settings.decorations);
    assert!(!settings.transparent);
    assert!(settings.resizable);
    let min = settings.min_size.expect("minimum size");
    assert!(min.width >= 800.0 && min.height >= 500.0);
    assert!(settings.size.width >= min.width);
    assert!(settings.size.height >= min.height);
}

#[test]
fn dismissing_a_notice_clears_errors_and_a_failed_sign_in() {
    let mut shell = test_shell(Mode::Run);
    shell.error = Some("YouTube Music search failed: timed out".into());
    shell.spotify_error = Some("Spotify replied 429: API rate limit exceeded".into());
    shell.mix_error = Some("Could not save playlists: denied".into());
    shell.phase = AccountPhase::Failed {
        message: "browser closed".into(),
    };
    assert_eq!(notice_lines(&shell).len(), 4);
    let _ = Shell::view(&shell);
    let _ = shell.update(Message::DismissNotices);
    assert!(shell.error.is_none());
    assert!(shell.spotify_error.is_none());
    assert!(shell.mix_error.is_none());
    assert_eq!(shell.phase, AccountPhase::SignedOut);
    assert!(notice_lines(&shell).is_empty());
    let _ = Shell::view(&shell);
}

#[test]
fn spotify_renewal_and_rate_limit_show_on_accounts_and_in_the_banner() {
    let mut shell = test_shell(Mode::Run);
    let _ = shell.update(Message::AccountChanged {
        epoch: 0,
        phase: AccountPhase::Failed {
            message: SPOTIFY_RENEW_SIGN_IN.to_string(),
        },
    });
    assert_eq!(
        spotify_compact(&shell.phase).0,
        "sign-in needs to be renewed"
    );
    let _ = shell.update(Message::ShowAccounts);
    assert!(
        notice_lines(&shell)
            .iter()
            .any(|line| line == SPOTIFY_RENEW_SIGN_IN)
    );
    let _ = Shell::view(&shell);

    let _ = shell.update(Message::QueryChanged("night".into()));
    let limited = "Spotify is limiting requests. Wait a moment and try again.";
    let _ = shell.update(Message::SpotifySearched {
        generation: shell.generation,
        result: Err(music::Error::Unavailable(limited.to_string())),
    });
    let lines = notice_lines(&shell);
    assert!(
        lines.iter().any(|line| line == limited),
        "banner should show the rate-limit sentence, got {lines:?}"
    );
    assert!(lines.iter().all(|line| !line.contains("429")), "{lines:?}");
    let _ = Shell::view(&shell);
}

#[test]
fn accounts_holds_sign_in_and_the_other_panes_stay_reachable() {
    let mut shell = test_shell(Mode::Run);
    let _ = shell.update(Message::ShowAccounts);
    assert_eq!(shell.pane, Pane::Accounts);
    assert!(shell.cookie_open);
    let _ = Shell::view(&shell);

    let _ = shell.update(Message::ShowSearch);
    assert_eq!(shell.pane, Pane::Search);
    let _ = Shell::view(&shell);

    let _ = shell.update(Message::ShowQueue);
    assert_eq!(shell.pane, Pane::Queue);
    let _ = Shell::view(&shell);

    let _ = shell.update(Message::ShowMixes);
    assert_eq!(shell.pane, Pane::Mixes);
    let _ = Shell::view(&shell);
}

#[test]
fn mixed_playlists_can_be_edited_and_played_from_the_other_panes() {
    let mut shell = test_shell(Mode::Run);
    let spotify = Track {
        id: "sp-night".into(),
        source: Source::Spotify,
        title: "Night Drive".into(),
        artists: vec!["Ada Vega".into()],
        album: Some("Low Glow".into()),
        duration: std::time::Duration::from_secs(192),
    };
    let youtube = Track {
        id: "yt-paper".into(),
        source: Source::YouTubeMusic,
        title: "Paper Boats".into(),
        artists: vec!["Jonah Reed".into()],
        album: None,
        duration: std::time::Duration::from_secs(164),
    };

    let _ = shell.update(Message::MixDraftChanged("   ".into()));
    let _ = shell.update(Message::CreateMix);
    assert!(shell.mixes.playlists().is_empty());
    assert!(
        shell
            .mix_error
            .as_deref()
            .is_some_and(|error| error.contains("Name"))
    );

    let _ = shell.update(Message::AddToMix(spotify.clone()));
    assert_eq!(shell.mixes.playlists().len(), 1);
    assert_eq!(shell.mixes.playlists()[0].name(), "Playlist");
    assert_eq!(
        shell.mixes.playlists()[0].tracks(),
        std::slice::from_ref(&spotify)
    );

    let _ = shell.update(Message::MixDraftChanged("  Night   Drive ".into()));
    let _ = shell.update(Message::CreateMix);
    assert_eq!(shell.pane, Pane::Mix);
    assert!(shell.mix_error.is_none());
    let night = shell
        .mixes
        .playlists()
        .iter()
        .find(|playlist| playlist.name() == "Night Drive")
        .expect("created")
        .id()
        .to_string();
    let _ = shell.update(Message::AddToMix(youtube.clone()));
    let _ = shell.update(Message::AddToMix(spotify.clone()));
    assert_eq!(
        shell.mixes.playlist(&night).expect("night").tracks(),
        &[youtube.clone(), spotify.clone()]
    );
    let _ = Shell::view(&shell);

    let _ = shell.update(Message::MoveMixLater(0));
    assert_eq!(
        shell
            .mixes
            .playlist(&night)
            .expect("night")
            .tracks()
            .iter()
            .map(|track| track.id.as_str())
            .collect::<Vec<_>>(),
        ["sp-night", "yt-paper"]
    );
    let _ = shell.update(Message::PlayMixFrom(1));
    let (ids, index) = shell.last_mix_play.clone().expect("played from a track");
    assert_eq!(ids, ["sp-night", "yt-paper"]);
    assert_eq!(index, 1);
    let _ = shell.update(Message::PlayMix);
    assert_eq!(
        shell.last_mix_play.as_ref().map(|(_ids, index)| *index),
        Some(0)
    );

    shell.results = vec![search::SearchRow {
        track: youtube.clone(),
        also: Vec::new(),
    }];
    shell.pane = Pane::Search;
    let _ = Shell::view(&shell);
    shell.playlist_tracks = vec![spotify.clone()];
    shell.playlist_name = "Harbour".into();
    shell.pane = Pane::Playlist;
    let _ = Shell::view(&shell);
    shell.preview = Some(player::NowPlaying {
        title: spotify.title.clone(),
        artist: spotify.artists.join(", "),
        phase: Phase::Playing,
        elapsed: std::time::Duration::from_secs(4),
        duration: spotify.duration,
        error: None,
        note: None,
        can_play: false,
        can_pause: true,
        can_stop: true,
        can_next: true,
        can_previous: true,
        source: Some(Source::Spotify),
        track_id: spotify.id.clone(),
        loaded: Some(spotify.clone()),
        upcoming: vec![youtube.clone()],
    });
    shell.pane = Pane::Queue;
    let _ = Shell::view(&shell);
    shell.preview = None;

    let _ = shell.update(Message::OpenMix(night.clone()));
    let _ = shell.update(Message::RemoveMixTrack(0));
    assert_eq!(
        shell.mixes.playlist(&night).expect("night").tracks(),
        std::slice::from_ref(&youtube)
    );
    let _ = shell.update(Message::MixDraftChanged("Late".into()));
    let _ = shell.update(Message::RenameMix);
    assert_eq!(shell.mixes.playlist(&night).expect("night").name(), "Late");
    let _ = shell.update(Message::ShowMixes);
    assert_eq!(shell.pane, Pane::Mixes);
    let _ = Shell::view(&shell);
    shell.mix_id = Some(night);
    let _ = shell.update(Message::DeleteMix);
    assert_eq!(shell.pane, Pane::Mixes);
    assert_eq!(shell.mixes.playlists().len(), 1);
    assert_eq!(shell.mixes.playlists()[0].name(), "Playlist");
    let _ = Shell::view(&shell);
}

#[test]
fn screenshot_seed_renders_search_and_accounts() {
    let mut shell = test_shell(Mode::Screenshot {
        main: PathBuf::from("main.png"),
        accounts: PathBuf::from("main-accounts.png"),
        library: PathBuf::from("main-library.png"),
    });
    shell.seed_screenshot();
    assert!(!shell.results.is_empty());
    assert!(shell.preview.is_some());
    assert!(shell.playlists.len() >= 2);
    let _ = Shell::view(&shell);
    shell.pane = Pane::Accounts;
    shell.cookie_open = true;
    let _ = Shell::view(&shell);
    shell.pane = Pane::Library;
    shell.cookie_open = false;
    let _ = Shell::view(&shell);
}

#[test]
fn write_png_creates_a_png_file() {
    let dir = std::env::temp_dir().join(format!("codevox-mu-png-{}", std::process::id()));
    let path = dir.join("pixel.png");
    write_png(&path, &[0xFF, 0x6A, 0x00, 0xFF], 1, 1).expect("png");
    let bytes = std::fs::read(&path).expect("read");
    assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    let _ = std::fs::remove_dir_all(&dir);
}

fn search_task(
    search: BackgroundSearch,
    query: String,
    generation: u64,
    spotify_search: bool,
) -> Task<Message> {
    let thread_name = if spotify_search {
        "spotify-search"
    } else {
        "youtube-music-search"
    };
    Task::perform(
        async move {
            let (sender, receiver) = oneshot::channel();
            let started = std::thread::Builder::new()
                .name(thread_name.into())
                .spawn(move || {
                    let _ = sender.send(search(&query));
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
        move |result| {
            if spotify_search {
                Message::SpotifySearched { generation, result }
            } else {
                Message::YoutubeSearched { generation, result }
            }
        },
    )
}

async fn account_task(
    name: &'static str,
    work: impl FnOnce() -> AccountPhase + Send + 'static,
) -> AccountPhase {
    let (sender, receiver) = oneshot::channel();
    let started = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            // A panic used to drop this sender. The window then reported
            // "stopped before it finished" even when account.json was saved.
            let phase = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)) {
                Ok(phase) => phase,
                Err(payload) => {
                    let message = panic_message(&payload);
                    log::error!("{name} panicked: {message}");
                    AccountPhase::Failed { message }
                }
            };
            let _ = sender.send(phase);
        });
    if started.is_err() {
        return AccountPhase::Failed {
            message: "could not start Spotify sign-in".to_string(),
        };
    }
    receiver.await.unwrap_or(AccountPhase::Failed {
        message: "Spotify sign-in stopped before it finished".to_string(),
    })
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    let text = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("Spotify sign-in hit an internal error");
    let text = crate::diag::redact(text);
    let owned = if text.starts_with("Spotify sign-in") {
        text
    } else {
        format!("Spotify sign-in hit an internal error: {text}")
    };
    // Keep the banner to one line. The log already has the redacted text.
    let mut brief: String = owned.chars().take(160).collect();
    if owned.chars().count() > 160 {
        brief.push('…');
    }
    brief
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
    let play = button(result_body(track))
        .on_press(play_message(track))
        .width(Fill)
        .padding([6.0, 0.0])
        .style(result_button);
    row![
        play,
        transport_button("Play next", Some(Message::PlayNext(track.clone())), false),
        transport_button("Queue", Some(Message::Enqueue(track.clone())), false),
        transport_button("Add", Some(Message::AddToMix(track.clone())), false),
    ]
    .spacing(8)
    .align_y(Vertical::Center)
    .into()
}

fn create_saved_row(shell: &Shell) -> Element<'_, Message> {
    row![
        name_field("Playlist name", &shell.mix_draft),
        transport_button("Create", Some(Message::CreateMix), true),
    ]
    .spacing(8)
    .align_y(Vertical::Center)
    .width(Fill)
    .into()
}

fn saved_toolbar(shell: &Shell) -> Element<'_, Message> {
    let has_tracks = shell
        .selected_mix()
        .is_some_and(|playlist| !playlist.tracks().is_empty());
    row![
        name_field("Playlist name", &shell.mix_draft),
        transport_button("Rename", Some(Message::RenameMix), false),
        transport_button("Play", has_tracks.then_some(Message::PlayMix), true),
        transport_button("Delete", Some(Message::DeleteMix), false),
    ]
    .spacing(8)
    .align_y(Vertical::Center)
    .width(Fill)
    .into()
}

fn name_field<'a>(placeholder: &'static str, value: &'a str) -> Element<'a, Message> {
    text_input(placeholder, value)
        .on_input(Message::MixDraftChanged)
        .padding([10.0, 12.0])
        .size(BODY_SIZE)
        .font(crate::theme::BODY)
        .width(Fill)
        .style(search_field)
        .into()
}

fn saved_row(playlist: &playlists::SavedPlaylist) -> Element<'_, Message> {
    let count = playlist.tracks().len();
    let tracks = if count == 1 {
        "1 track".to_string()
    } else {
        format!("{count} tracks")
    };
    let body = row![
        text(playlist.name())
            .font(crate::theme::BODY)
            .size(BODY_SIZE)
            .color(crate::theme::TEXT_PRIMARY)
            .width(Fill),
        text(tracks)
            .font(crate::theme::BODY)
            .size(META_SIZE)
            .color(crate::theme::TEXT_TERTIARY),
    ]
    .spacing(ROW_SPACING)
    .align_y(Vertical::Center)
    .width(Fill);
    button(body)
        .on_press(Message::OpenMix(playlist.id().to_string()))
        .width(Fill)
        .padding([6.0, 0.0])
        .style(result_button)
        .into()
}

fn saved_track_row(index: usize, track: &Track, last: bool) -> Element<'_, Message> {
    let play = button(result_body(track))
        .on_press(Message::PlayMixFrom(index))
        .width(Fill)
        .padding([6.0, 0.0])
        .style(result_button);
    let up = (index > 0).then_some(Message::MoveMixEarlier(index));
    let down = (!last).then_some(Message::MoveMixLater(index));
    row![
        play,
        transport_button("Up", up, false),
        transport_button("Down", down, false),
        transport_button("Remove", Some(Message::RemoveMixTrack(index)), false),
    ]
    .spacing(8)
    .align_y(Vertical::Center)
    .into()
}

fn playlist_row(playlist: &Playlist) -> Element<'_, Message> {
    let body = row![
        text(&playlist.name)
            .font(crate::theme::BODY)
            .size(BODY_SIZE)
            .color(crate::theme::TEXT_PRIMARY)
            .width(Fill),
        source_tag(playlist.source),
    ]
    .spacing(ROW_SPACING)
    .align_y(Vertical::Center)
    .width(Fill);
    button(body)
        .on_press(Message::OpenPlaylist {
            source: playlist.source,
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

fn current_row(now: &player::NowPlaying) -> Element<'static, Message> {
    let mut details = column![
        text(now.title.clone())
            .font(crate::theme::BODY)
            .size(BODY_SIZE)
            .color(crate::theme::TEXT_PRIMARY)
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

    let mut line = row![
        text("Now")
            .font(crate::theme::BODY)
            .size(SOURCE_SIZE)
            .color(crate::theme::ACCENT),
        details,
    ]
    .spacing(ROW_SPACING)
    .align_y(Vertical::Center)
    .width(Fill);
    if let Some(source) = now.source {
        line = line.push(source_tag(source));
    }

    row![
        line,
        transport_button("Add", now.loaded.clone().map(Message::AddToMix), false,),
        transport_button("Remove", Some(Message::RemoveCurrent), false),
    ]
    .spacing(8)
    .align_y(Vertical::Center)
    .into()
}

fn queued_row(index: usize, track: &Track, last: bool) -> Element<'static, Message> {
    let up = (index > 0).then_some(Message::MoveEarlier(index));
    let down = (!last).then_some(Message::MoveLater(index));
    row![
        queue_track_line(track),
        transport_button("Up", up, false),
        transport_button("Down", down, false),
        transport_button("Add", Some(Message::AddToMix(track.clone())), false),
        transport_button("Remove", Some(Message::RemoveQueued(index)), false),
    ]
    .spacing(8)
    .align_y(Vertical::Center)
    .into()
}

fn queue_track_line(track: &Track) -> Element<'static, Message> {
    let mut details = column![
        text(track.title.clone())
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

    row![details, source_tag(track.source)]
        .spacing(ROW_SPACING)
        .align_y(Vertical::Center)
        .width(Fill)
        .into()
}

fn source_tag(source: Source) -> Element<'static, Message> {
    let (_, foreground) = source_colors(source);
    container(
        text(source.label())
            .font(crate::theme::BODY)
            .size(SOURCE_SIZE)
            .color(foreground),
    )
    .padding([3.0, 7.0])
    .style(source_chip(source))
    .into()
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
