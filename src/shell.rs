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

use crate::music::{self, MusicService, Source, Track};
use crate::player::{self, Phase};
use crate::search;
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
    /// The window exists, so OS media controls can attach to it.
    BindMedia(window::Id),
    /// `hwnd` is the native handle on Windows. Other platforms pass `None`.
    MediaBound(Option<isize>),
    Media(crate::media_controls::MediaCommand),
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
    playback: player::Playback,
    /// A clock tick is already waiting. Keeps the 200 ms timer from stacking.
    clock_armed: bool,
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

#[cfg(test)]
fn test_shell(mode: Mode) -> Shell {
    let youtube = music::MockService::youtube_music();
    // Tests never poll the task, and the closure is the mock if they do.
    Shell::with_youtube(mode, Arc::new(move |query| youtube.search(query)))
}

impl Shell {
    fn boot(mode: Mode) -> Self {
        if mode == Mode::Smoke {
            arm_smoke_timeout();
        }

        let youtube = YouTubeMusic::new();
        let mut shell = Self::with_youtube(mode, Arc::new(move |query| youtube.search(query)));
        // Smoke must not open a device or resolve a stream. The dormant
        // handle drops every command.
        if mode == Mode::Run {
            shell.playback = player::Playback::start();
        }
        shell
    }

    fn with_youtube(mode: Mode, youtube: BackgroundSearch) -> Self {
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
            playback: player::Playback::dormant(),
            clock_armed: false,
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
            Message::BindMedia(id) => {
                crate::media_controls::read_handle(id).map(Message::MediaBound)
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

        let results = column(shell.results.iter().map(result_row))
            .spacing(RESULT_SPACING)
            .width(Fill);
        let mut content = column![heading, accent, input].spacing(PANEL_SPACING);
        if let Some(status) = status_line(shell) {
            content = content.push(status);
        }

        let content = content
            .push(scrollable(results).width(Fill).height(Fill))
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

    if shell.query.trim().is_empty() || !shell.results.is_empty() {
        return None;
    }

    Some(
        text("No matches")
            .font(crate::theme::BODY)
            .size(META_SIZE)
            .color(crate::theme::TEXT_SECONDARY)
            .into(),
    )
}

/// Spotify first, then YouTube Music, interleaved by each service's rank.
///
/// YouTube Music is the source a click can play. A duplicate keeps that
/// track, and the other service's id, for a later fallback.
fn search_rows(spotify: &[Track], youtube: &[Track]) -> Vec<search::SearchRow> {
    search::merge_results(&[spotify, youtube], Source::YouTubeMusic)
}

fn result_row(row: &search::SearchRow) -> Element<'_, Message> {
    let play = Message::Play(row.track.clone());
    button(result_body(&row.track))
        .on_press(play)
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
