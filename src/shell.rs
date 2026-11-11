//! The native window.
//!
//! One iced window is the whole interface today. A new concern gets its own
//! module when it has real code.

use iced::widget::{Space, column, container, text};
use iced::window::{self, Settings};
use iced::{Element, Fill, Size, Subscription, Task};

/// Window title. This is the product name, not a window-manager default.
const TITLE: &str = "CodeVox mU";

const LABEL: &str = "Placeholder";

/// Logical pixels. Large enough that the placeholder is readable on a laptop.
const WINDOW_SIZE: Size = Size::new(960.0, 640.0);

const PANEL_PADDING: f32 = 24.0;
const PANEL_SPACING: f32 = 14.0;
const HEADING_SIZE: f32 = 22.0;
const BODY_SIZE: f32 = 14.0;

/// The one accent mark on the placeholder. Short, so it reads as a rule.
const ACCENT_MARK_WIDTH: f32 = 28.0;
const ACCENT_MARK_HEIGHT: f32 = 2.0;

/// `--smoke` exits once this many frames have been drawn.
///
/// One frame proves the window presented. The extra frames prove the loop
/// kept running. Zero would never exit.
const SMOKE_FRAME_COUNT_LIMIT: u32 = 3;

const _: () = assert!(SMOKE_FRAME_COUNT_LIMIT > 0);

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
}

struct Shell {
    mode: Mode,
    frame_count: u32,
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

impl Shell {
    fn boot(mode: Mode) -> Self {
        Self {
            mode,
            frame_count: 0,
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Frame => self.on_frame(),
        }
    }

    fn on_frame(&mut self) -> Task<Message> {
        // Frames are only subscribed in smoke mode. A frame in any other
        // mode means the subscription and the mode have drifted.
        assert!(matches!(self.mode, Mode::Smoke));
        assert!(self.frame_count < SMOKE_FRAME_COUNT_LIMIT);

        self.frame_count += 1;

        if self.frame_count < SMOKE_FRAME_COUNT_LIMIT {
            Task::none()
        } else {
            assert_eq!(self.frame_count, SMOKE_FRAME_COUNT_LIMIT);
            eprintln!("codevox-mu smoke: rendered {SMOKE_FRAME_COUNT_LIMIT} frames, exiting");
            iced::exit()
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

    fn view(_shell: &Self) -> Element<'_, Message> {
        let heading = text(TITLE)
            .font(crate::theme::HEADING)
            .size(HEADING_SIZE)
            .color(crate::theme::TEXT_PRIMARY);
        let body = text(LABEL)
            .font(crate::theme::BODY)
            .size(BODY_SIZE)
            .color(crate::theme::TEXT_SECONDARY);
        let accent = container(
            Space::new()
                .width(ACCENT_MARK_WIDTH)
                .height(ACCENT_MARK_HEIGHT),
        )
        .style(accent_mark);

        let panel = container(column![heading, accent, body].spacing(PANEL_SPACING))
            .padding(PANEL_PADDING)
            .style(crate::theme::panel);

        container(panel).center(Fill).into()
    }
}

/// Solid accent. The placeholder paints this once.
fn accent_mark(_theme: &iced::Theme) -> container::Style {
    container::background(crate::theme::ACCENT)
}
