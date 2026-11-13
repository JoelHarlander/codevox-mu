//! Process entry for CodeVox mU.
//!
//! This file only decides how the process was invoked, then hands a
//! [`shell::Mode`] to the window. The shell never reads `argv`, so a bad
//! argument fails before a window exists.

// Release builds on Windows are GUI programs. A console-subsystem exe opens
// a black console window behind the borderless player. Debug builds stay
// console programs so `cargo run` keeps its output. Tests stay console
// programs so `cargo test --release` can print.
#![cfg_attr(
    all(windows, not(debug_assertions), not(test)),
    windows_subsystem = "windows"
)]

mod diag;
mod extract;
mod lrc;
mod lyrics;
mod media_controls;
mod music;
mod output;
mod player;
mod playlists;
mod queue;
mod search;
mod selftest;
mod shell;
mod spotify;
mod theme;
mod tray;
mod youtube_account;
mod youtube_music;

use std::process::exit;

/// Usage errors are distinct from a window that failed to open.
const USAGE_EXIT_CODE: i32 = 2;

fn main() -> iced::Result {
    attach_parent_console();
    // The log file is truncated here, before sign-in or `--selftest` runs.
    // A missing data directory only skips the file. The window still opens.
    diag::init();
    match invocation(std::env::args()) {
        Ok(Launch::Window(mode)) => shell::run(mode),
        Ok(Launch::SelfTest) => exit(selftest::run()),
        Err(argument) => {
            eprintln!("codevox-mu: unknown argument: {argument}");
            eprintln!("usage: codevox-mu [--smoke | --screenshot <path.png> | --selftest]");
            exit(USAGE_EXIT_CODE);
        }
    }
}

/// A GUI-subsystem process has no console of its own. When a terminal
/// launched this release build, attach to that console and point any
/// missing standard handle at it, so `--smoke`, `--screenshot`, and usage
/// errors still print. Redirected handles (the Windows CI runner, pipes)
/// stay where they are. No parent console means a double-clicked window:
/// attaching fails and this returns without allocating one.
#[cfg(all(windows, not(debug_assertions)))]
fn attach_parent_console() {
    use windows_sys::Win32::Foundation::{GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
        SetStdHandle,
    };

    // SAFETY: documented console-attach sequence for a GUI program that
    // still prints to the terminal it was launched from. The CONOUT$
    // handle is left open for the process lifetime; closing it would
    // dangle the standard handles. Inherited handles are not replaced, so
    // a redirected CI step keeps the pipes it was given.
    unsafe {
        let stdout = GetStdHandle(STD_OUTPUT_HANDLE);
        let stderr = GetStdHandle(STD_ERROR_HANDLE);
        let stdout_missing = stdout.is_null() || stdout == INVALID_HANDLE_VALUE;
        let stderr_missing = stderr.is_null() || stderr == INVALID_HANDLE_VALUE;
        if !stdout_missing && !stderr_missing {
            return;
        }
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return;
        }
        let console = CreateFileW(
            windows_sys::w!("CONOUT$"),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        );
        if console.is_null() || console == INVALID_HANDLE_VALUE {
            return;
        }
        if stdout_missing {
            SetStdHandle(STD_OUTPUT_HANDLE, console);
        }
        if stderr_missing {
            SetStdHandle(STD_ERROR_HANDLE, console);
        }
    }
}

#[cfg(not(all(windows, not(debug_assertions))))]
fn attach_parent_console() {}

/// How the process was started. The window modes stay in the shell.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Launch {
    Window(shell::Mode),
    /// Saved-account checks. No window.
    SelfTest,
}

/// Accepts no arguments, `--smoke`, one `--screenshot` path, or `--selftest`.
///
/// `--screenshot <path.png>` writes the main view there, the Accounts view
/// beside it as `<name>-accounts.png`, and the Library view as
/// `<name>-library.png`. The first unknown argument is returned so the caller
/// can show it.
fn invocation(args: impl IntoIterator<Item = impl AsRef<str>>) -> Result<Launch, String> {
    let mut arguments = args.into_iter();
    // The first value is the program name, not an option.
    let _program = arguments.next();
    let mut mode = shell::Mode::Run;
    let mut saw_smoke = false;
    let mut saw_selftest = false;

    while let Some(argument) = arguments.next() {
        let argument = argument.as_ref();
        if argument == "--smoke" {
            if matches!(mode, shell::Mode::Screenshot { .. }) || saw_selftest {
                return Err(argument.to_string());
            }
            saw_smoke = true;
            mode = shell::Mode::Smoke;
        } else if argument == "--selftest" {
            if saw_smoke || matches!(mode, shell::Mode::Screenshot { .. }) {
                return Err(argument.to_string());
            }
            saw_selftest = true;
        } else if argument == "--screenshot" {
            if saw_smoke || saw_selftest {
                return Err(argument.to_string());
            }
            let Some(path) = arguments.next() else {
                return Err(argument.to_string());
            };
            let path = path.as_ref();
            if path.is_empty() || path.starts_with("--") {
                return Err(path.to_string());
            }
            let main = std::path::PathBuf::from(path);
            let accounts = accounts_screenshot_path(&main);
            let library = library_screenshot_path(&main);
            mode = shell::Mode::Screenshot {
                main,
                accounts,
                library,
            };
        } else {
            return Err(argument.to_string());
        }
    }

    if saw_selftest {
        Ok(Launch::SelfTest)
    } else {
        Ok(Launch::Window(mode))
    }
}

/// Accounts view for a `--screenshot` path: `main.png` becomes `main-accounts.png`.
fn accounts_screenshot_path(main: &std::path::Path) -> std::path::PathBuf {
    sibling_screenshot(main, "-accounts")
}

/// Library view for a `--screenshot` path: `main.png` becomes `main-library.png`.
fn library_screenshot_path(main: &std::path::Path) -> std::path::PathBuf {
    sibling_screenshot(main, "-library")
}

fn sibling_screenshot(main: &std::path::Path, suffix: &str) -> std::path::PathBuf {
    let mut name = main.file_stem().unwrap_or_default().to_os_string();
    name.push(suffix);
    if let Some(extension) = main.extension() {
        name.push(".");
        name.push(extension);
    } else {
        name.push(".png");
    }
    match main.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(name),
        _ => std::path::PathBuf::from(name),
    }
}

#[cfg(test)]
mod tests {
    use super::{Launch, invocation};
    use crate::shell::Mode;

    #[test]
    fn run_without_arguments() {
        let mode = invocation(["codevox-mu"]).expect("no arguments is run");
        assert_eq!(mode, Launch::Window(Mode::Run));
    }

    #[test]
    fn smoke_flag() {
        let mode = invocation(["codevox-mu", "--smoke"]).expect("--smoke is accepted");
        assert_eq!(mode, Launch::Window(Mode::Smoke));
    }

    #[test]
    fn repeated_smoke_flag_stays_smoke() {
        let mode = invocation(["codevox-mu", "--smoke", "--smoke"])
            .expect("repeating --smoke is still smoke");
        assert_eq!(mode, Launch::Window(Mode::Smoke));
    }

    #[test]
    fn selftest_flag_does_not_open_a_window() {
        let mode = invocation(["codevox-mu", "--selftest"]).expect("--selftest is accepted");
        assert_eq!(mode, Launch::SelfTest);
    }

    #[test]
    fn selftest_cannot_combine_with_smoke_or_screenshot() {
        let error = invocation(["codevox-mu", "--selftest", "--smoke"]).unwrap_err();
        assert_eq!(error, "--smoke");
        let error = invocation(["codevox-mu", "--smoke", "--selftest"]).unwrap_err();
        assert_eq!(error, "--selftest");
        let error = invocation(["codevox-mu", "--selftest", "--screenshot", "a.png"]).unwrap_err();
        assert_eq!(error, "--screenshot");
    }

    #[test]
    fn reject_unknown_argument() {
        let error = invocation(["codevox-mu", "--help"]).unwrap_err();
        assert_eq!(error, "--help");
    }

    #[test]
    fn reject_argument_after_smoke() {
        let error = invocation(["codevox-mu", "--smoke", "extra"]).unwrap_err();
        assert_eq!(error, "extra");
    }

    #[test]
    fn reject_smoke_prefix() {
        let error = invocation(["codevox-mu", "--smoke-now"]).unwrap_err();
        assert_eq!(error, "--smoke-now");
    }

    #[test]
    fn screenshot_flag_names_search_accounts_and_library() {
        let mode = invocation(["codevox-mu", "--screenshot", "/tmp/main.png"])
            .expect("screenshot path is accepted");
        match mode {
            Launch::Window(Mode::Screenshot {
                main,
                accounts,
                library,
            }) => {
                assert_eq!(main, std::path::PathBuf::from("/tmp/main.png"));
                assert_eq!(accounts, std::path::PathBuf::from("/tmp/main-accounts.png"));
                assert_eq!(library, std::path::PathBuf::from("/tmp/main-library.png"));
            }
            other => panic!("expected screenshot mode, got {other:?}"),
        }
    }

    #[test]
    fn screenshot_requires_a_path() {
        let error = invocation(["codevox-mu", "--screenshot"]).unwrap_err();
        assert_eq!(error, "--screenshot");
    }

    #[test]
    fn screenshot_rejects_a_flag_as_the_path() {
        let error = invocation(["codevox-mu", "--screenshot", "--smoke"]).unwrap_err();
        assert_eq!(error, "--smoke");
    }

    #[test]
    fn smoke_and_screenshot_cannot_combine() {
        let error = invocation(["codevox-mu", "--smoke", "--screenshot", "a.png"]).unwrap_err();
        assert_eq!(error, "--screenshot");
    }
}
