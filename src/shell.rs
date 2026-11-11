//! The native window.
//!
//! One iced window is the whole interface today. A new concern gets its own
//! module when it has real code.

use std::io::{self, Write};
use std::thread;
use std::time::Duration;

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
const SMOKE_TIMEOUT: Duration = Duration::from_secs(60);

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

    fn view(_shell: &Self) -> Element<'_, Message> {
        container(text(LABEL)).center(Fill).into()
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
    thread::spawn(|| {
        thread::sleep(SMOKE_TIMEOUT);
        eprintln!(
            "codevox-mu smoke: timed out after {}s without exiting",
            SMOKE_TIMEOUT.as_secs()
        );
        let _ = io::stderr().flush();
        std::process::exit(SMOKE_TIMEOUT_EXIT_CODE);
    });
}

#[cfg(test)]
mod tests {
    use super::{Mode, SMOKE_FRAME_COUNT_LIMIT, Shell, SmokeFrame};

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
}
