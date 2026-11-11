//! The native window.
//!
//! One iced window is the whole interface today. Search goes through
//! [`crate::music`]; the window does not know how a backend is implemented.

use iced::alignment::{Horizontal, Vertical};
use iced::widget::{Space, column, container, row, scrollable, text, text_input};
use iced::window::{self, Settings};
use iced::{Background, Border, Color, Element, Fill, Size, Subscription, Task};

use crate::music::{self, MusicService, Source, Track};

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
    results: Vec<Track>,
    services: Vec<Box<dyn MusicService>>,
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
    let mut shell = Shell {
        mode: Mode::Smoke,
        frame_count: 0,
        exit_requested: false,
        query: String::new(),
        results: Vec::new(),
        services: Vec::new(),
    };

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
fn search_box_merges_mock_sources() {
    let mut shell = Shell::boot(Mode::Run);
    let _ = shell.update(Message::QueryChanged("night".into()));

    assert_eq!(shell.query, "night");
    let sources: Vec<_> = shell.results.iter().map(|track| track.source).collect();
    assert_eq!(sources, [Source::Spotify, Source::YouTubeMusic]);
    let _ = Shell::view(&shell);

    let _ = shell.update(Message::QueryChanged("   ".into()));
    assert!(shell.results.is_empty());
    let _ = Shell::view(&shell);
}

impl Shell {
    fn boot(mode: Mode) -> Self {
        if mode == Mode::Smoke {
            arm_smoke_timeout();
        }

        Self {
            mode,
            frame_count: 0,
            exit_requested: false,
            query: String::new(),
            results: Vec::new(),
            services: music::mock_services(),
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Frame => self.on_frame(),
            Message::QueryChanged(query) => {
                self.set_query(query);
                Task::none()
            }
        }
    }

    fn set_query(&mut self, query: String) {
        self.query = query;
        // The mock does not fail a search. A later backend might; drop the
        // rows rather than leave the previous query on screen.
        self.results = music::search_merged(
            self.services.iter().map(|service| service.as_ref()),
            &self.query,
        )
        .unwrap_or_default();
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
            Mode::Smoke => window::frames().map(|_| Message::Frame),
            Mode::Run => Subscription::none(),
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

fn result_row(track: &Track) -> Element<'_, Message> {
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
