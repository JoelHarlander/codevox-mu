//! Process entry for CodeVox mU.
//!
//! This file only decides how the process was invoked, then hands a
//! [`shell::Mode`] to the window. The shell never reads `argv`, so a bad
//! argument fails before a window exists.

mod extract;
mod media_controls;
mod music;
mod output;
mod player;
mod queue;
mod search;
mod shell;
mod spotify;
mod theme;
mod youtube_account;
mod youtube_music;

use std::process::exit;

/// Usage errors are distinct from a window that failed to open.
const USAGE_EXIT_CODE: i32 = 2;

fn main() -> iced::Result {
    match invocation(std::env::args()) {
        Ok(mode) => shell::run(mode),
        Err(argument) => {
            eprintln!("codevox-mu: unknown argument: {argument}");
            eprintln!("usage: codevox-mu [--smoke | --screenshot <path.png>]");
            exit(USAGE_EXIT_CODE);
        }
    }
}

/// Accepts no arguments, one or more `--smoke` flags, or one `--screenshot` path.
///
/// `--screenshot <path.png>` writes the main view there, the Accounts view
/// beside it as `<name>-accounts.png`, and the Library view as
/// `<name>-library.png`. The first unknown argument is returned so the caller
/// can show it.
fn invocation(args: impl IntoIterator<Item = impl AsRef<str>>) -> Result<shell::Mode, String> {
    let mut arguments = args.into_iter();
    // The first value is the program name, not an option.
    let _program = arguments.next();
    let mut mode = shell::Mode::Run;
    let mut saw_smoke = false;

    while let Some(argument) = arguments.next() {
        let argument = argument.as_ref();
        if argument == "--smoke" {
            if matches!(mode, shell::Mode::Screenshot { .. }) {
                return Err(argument.to_string());
            }
            saw_smoke = true;
            mode = shell::Mode::Smoke;
        } else if argument == "--screenshot" {
            if saw_smoke {
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

    Ok(mode)
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
    use super::invocation;
    use crate::shell::Mode;

    #[test]
    fn run_without_arguments() {
        let mode = invocation(["codevox-mu"]).expect("no arguments is run");
        assert_eq!(mode, Mode::Run);
    }

    #[test]
    fn smoke_flag() {
        let mode = invocation(["codevox-mu", "--smoke"]).expect("--smoke is accepted");
        assert_eq!(mode, Mode::Smoke);
    }

    #[test]
    fn repeated_smoke_flag_stays_smoke() {
        let mode = invocation(["codevox-mu", "--smoke", "--smoke"])
            .expect("repeating --smoke is still smoke");
        assert_eq!(mode, Mode::Smoke);
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
            Mode::Screenshot {
                main,
                accounts,
                library,
            } => {
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
