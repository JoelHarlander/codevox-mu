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
}

struct Shell {
    mode: Mode,
    frame_count: u32,
    /// Set once smoke has asked the runtime to exit.
    ///
    /// Later frames are ignored. They are not a sign that the budget was
    /// exceeded; the window can still be closing.
    exit_requested: bool,
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

impl Shell {
    fn boot(mode: Mode) -> Self {
        if mode == Mode::Smoke {
            arm_smoke_timeout();
        }

        Self {
            mode,
            frame_count: 0,
            exit_requested: false,
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
