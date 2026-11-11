//! The native window.
//!
//! One iced window is the whole interface today. A new concern gets its own
//! module when it has real code.

use iced::widget::{container, text};
use iced::window::{self, Settings};
use iced::{Element, Fill, Size, Subscription, Task};

/// Window title. This is the product name, not a window-manager default.
const TITLE: &str = "CodeVox mU";

const LABEL: &str = "Placeholder";

/// Logical pixels. Large enough that the placeholder is readable on a laptop.
const WINDOW_SIZE: Size = Size::new(960.0, 640.0);

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
    iced::application(move || Shell::boot(mode), Shell::update, Shell::view)
        .title(TITLE)
        .window(window_settings())
        .subscription(Shell::subscription)
        .run()
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

    fn view(_shell: &Self) -> Element<'_, Message> {
        container(text(LABEL)).center(Fill).into()
    }
}
