//! Process entry for CodeVox mU.
//!
//! This file only decides how the process was invoked, then hands a
//! [`shell::Mode`] to the window. The shell never reads `argv`, so a bad
//! argument fails before a window exists.

mod shell;

use std::process::exit;

/// Usage errors are distinct from a window that failed to open.
const USAGE_EXIT_CODE: i32 = 2;

fn main() -> iced::Result {
    match invocation(std::env::args()) {
        Ok(mode) => shell::run(mode),
        Err(argument) => {
            eprintln!("codevox-mu: unknown argument: {argument}");
            eprintln!("usage: codevox-mu [--smoke]");
            exit(USAGE_EXIT_CODE);
        }
    }
}

/// Accepts no arguments, or one or more `--smoke` flags.
///
/// The first unknown argument is returned so the caller can show it.
fn invocation(args: impl IntoIterator<Item = impl AsRef<str>>) -> Result<shell::Mode, String> {
    let mut arguments = args.into_iter();
    // The first value is the program name, not an option.
    let _program = arguments.next();
    let mut mode = shell::Mode::Run;

    for argument in arguments {
        let argument = argument.as_ref();
        if argument == "--smoke" {
            mode = shell::Mode::Smoke;
        } else {
            return Err(argument.to_string());
        }
    }

    Ok(mode)
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
}
