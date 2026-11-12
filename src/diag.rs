//! One log file for a launch.
//!
//! The file is `%LOCALAPPDATA%\codevox-mu\codevox-mu.log` on Windows,
//! `~/Library/Application Support/codevox-mu/codevox-mu.log` on macOS, and
//! `$XDG_DATA_HOME/codevox-mu/codevox-mu.log` (or `~/.local/share/codevox-mu/`)
//! on Linux. Each launch truncates it. librespot already logs through the
//! `log` crate, so one logger covers the player and this app.
//!
//! Tokens, authorization codes, and cookie values are stripped before a line
//! is written. Callers should still avoid putting secrets in the message.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Install the file logger. A second call does nothing.
///
/// Missing directories and a relative data root skip the file. The process
/// keeps running, and `--selftest` still prints to stdout.
pub fn init() {
    let Some(path) = log_path() else {
        eprintln!("codevox-mu: no log file, the data directory was not found");
        return;
    };
    let file = match open_log(&path) {
        Ok(file) => file,
        Err(error) => {
            eprintln!("codevox-mu: no log file at {}: {error}", path.display());
            return;
        }
    };
    let logger = Box::new(FileLog {
        file: Mutex::new(file),
    });
    if log::set_boxed_logger(logger).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
        log::info!("log started");
    }
}

/// Append one authorization URL, when `CODEVOX_MU_AUTH_URL_FILE` is set.
///
/// The URL is not a token. It is the page the browser is about to open.
/// The log records that a page was opened and does not store the URL.
pub fn record_auth_url(url: &str) {
    log::info!("spotify auth: authorization page is ready");
    let Some(path) = std::env::var_os("CODEVOX_MU_AUTH_URL_FILE") else {
        return;
    };
    if path.is_empty() {
        return;
    }
    if let Err(error) = append_auth_url(Path::new(&path), url) {
        log::error!("spotify auth: could not record the authorization url: {error}");
    }
}

/// Remove bearer tokens, OAuth codes, refresh tokens, and cookie values.
pub fn redact(message: &str) -> String {
    let mut text = message.to_string();
    redact_bearer(&mut text);
    redact_json_string(&mut text, "refresh_token");
    redact_json_string(&mut text, "access_token");
    redact_json_string(&mut text, "playback_refresh_token");
    redact_json_string(&mut text, "auth_data");
    redact_query(&mut text, "code");
    redact_query(&mut text, "access_token");
    redact_query(&mut text, "refresh_token");
    redact_labeled(&mut text, "SAPISID=");
    text
}

pub(crate) fn append_auth_url(path: &Path, url: &str) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{url}")?;
    file.flush()
}

fn open_log(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    writeln!(
        file,
        "{} INFO codevox_mu: log file truncated for this launch",
        stamp()
    )?;
    file.flush()?;
    Ok(file)
}

fn log_path() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join("codevox-mu.log"))
}

#[derive(Clone, Copy)]
enum Host {
    Linux,
    Mac,
    Windows,
}

fn this_host() -> Host {
    if cfg!(windows) {
        Host::Windows
    } else if cfg!(target_os = "macos") {
        Host::Mac
    } else {
        Host::Linux
    }
}

fn data_dir() -> Option<PathBuf> {
    data_dir_from(this_host(), |key| std::env::var(key).ok())
}

fn data_dir_from(host: Host, var: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let absolute = |key: &str| {
        let value = var(key).filter(|value| !value.trim().is_empty())?;
        is_absolute(host, &value).then(|| PathBuf::from(value))
    };
    let root = match host {
        Host::Windows => absolute("LOCALAPPDATA").or_else(|| absolute("APPDATA"))?,
        Host::Mac => absolute("HOME")?
            .join("Library")
            .join("Application Support"),
        Host::Linux => absolute("XDG_DATA_HOME")
            .or_else(|| absolute("HOME").map(|home| home.join(".local").join("share")))?,
    };
    Some(root.join("codevox-mu"))
}

fn is_absolute(host: Host, path: &str) -> bool {
    match host {
        Host::Linux | Host::Mac => path.starts_with('/'),
        Host::Windows => {
            let bytes = path.as_bytes();
            let drive = bytes.len() >= 3
                && bytes[0].is_ascii_alphabetic()
                && bytes[1] == b':'
                && (bytes[2] == b'\\' || bytes[2] == b'/');
            drive || path.starts_with(r"\\") || path.starts_with("//")
        }
    }
}

struct FileLog {
    file: Mutex<File>,
}

impl log::Log for FileLog {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let message = redact(&record.args().to_string());
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = writeln!(
            file,
            "{} {} {}: {message}",
            stamp(),
            record.level(),
            record.target()
        );
        let _ = file.flush();
    }

    fn flush(&self) {
        if let Ok(mut file) = self.file.lock() {
            let _ = file.flush();
        }
    }
}

fn stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = tod / 3_600;
    let minute = (tod % 3_600) / 60;
    let second = tod % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Howard Hinnant's civil-from-days, proleptic Gregorian.
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (year as i32, month as u32, day as u32)
}

fn redact_bearer(text: &mut String) {
    redact_labeled(text, "Bearer ");
}

fn redact_labeled(text: &mut String, label: &str) {
    let mut start = 0;
    while let Some(found) = text[start..].find(label) {
        let value = start + found + label.len();
        let end = text[value..]
            .find(|ch: char| ch.is_whitespace() || ch == ';' || ch == '"' || ch == ',')
            .map_or(text.len(), |offset| value + offset);
        if end - value >= 8 {
            text.replace_range(value..end, "[redacted]");
            start = value + "[redacted]".len();
        } else {
            start = value;
        }
        if start >= text.len() {
            break;
        }
    }
}

fn redact_json_string(text: &mut String, key: &str) {
    let pattern = format!("\"{key}\"");
    let mut start = 0;
    while let Some(found) = text[start..].find(&pattern) {
        let after_key = start + found + pattern.len();
        let Some(colon) = text[after_key..].find(':') else {
            break;
        };
        let after_colon = after_key + colon + 1;
        let Some(open) = text[after_colon..].find('"') else {
            break;
        };
        let value = after_colon + open + 1;
        let mut end = value;
        let bytes = text.as_bytes();
        while end < bytes.len() {
            if bytes[end] == b'\\' {
                end = (end + 2).min(bytes.len());
                continue;
            }
            if bytes[end] == b'"' {
                break;
            }
            end += 1;
        }
        if end - value >= 8 && end <= bytes.len() {
            text.replace_range(value..end, "[redacted]");
            start = value + "[redacted]".len();
        } else {
            start = end.saturating_add(1);
        }
        if start >= text.len() {
            break;
        }
    }
}

fn redact_query(text: &mut String, key: &str) {
    let pattern = format!("{key}=");
    let mut start = 0;
    while let Some(found) = text[start..].find(&pattern) {
        let value = start + found + pattern.len();
        let end = text[value..]
            .find(|ch: char| ch == '&' || ch.is_whitespace() || ch == '"' || ch == ',')
            .map_or(text.len(), |offset| value + offset);
        if end - value >= 8 {
            text.replace_range(value..end, "[redacted]");
            start = value + "[redacted]".len();
        } else {
            start = end.saturating_add(1);
        }
        if start >= text.len() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_dir_is_the_platform_directory_and_stays_absolute() {
        let windows = data_dir_from(Host::Windows, |key| {
            (key == "LOCALAPPDATA").then(|| r"C:\Users\ada\AppData\Local".to_string())
        });
        assert_eq!(
            windows,
            Some(PathBuf::from(r"C:\Users\ada\AppData\Local").join("codevox-mu"))
        );

        let roaming = data_dir_from(Host::Windows, |key| {
            (key == "APPDATA").then(|| r"D:\AppData\Roaming".to_string())
        });
        assert_eq!(
            roaming,
            Some(PathBuf::from(r"D:\AppData\Roaming").join("codevox-mu"))
        );
        assert_eq!(
            data_dir_from(Host::Windows, |key| {
                (key == "LOCALAPPDATA").then(|| r"Users\ada".to_string())
            }),
            None
        );

        let mac = data_dir_from(Host::Mac, |key| {
            (key == "HOME").then(|| "/Users/ada".to_string())
        });
        assert_eq!(
            mac,
            Some(PathBuf::from(
                "/Users/ada/Library/Application Support/codevox-mu"
            ))
        );

        let linux = data_dir_from(Host::Linux, |key| match key {
            "XDG_DATA_HOME" => Some("/var/lib/ada".to_string()),
            "HOME" => Some("/home/ada".to_string()),
            _ => None,
        });
        assert_eq!(linux, Some(PathBuf::from("/var/lib/ada/codevox-mu")));

        let from_home = data_dir_from(Host::Linux, |key| {
            (key == "HOME").then(|| "/home/ada".to_string())
        });
        assert_eq!(
            from_home,
            Some(PathBuf::from("/home/ada/.local/share/codevox-mu"))
        );
        assert_eq!(
            data_dir_from(Host::Linux, |key| {
                (key == "HOME").then(|| "relative".to_string())
            }),
            None
        );
    }

    #[test]
    fn redact_strips_tokens_codes_and_cookies() {
        let raw = concat!(
            "Authorization: Bearer access-token-value-1234 ",
            r#"{"refresh_token":"refresh-token-fixture-value","playback_refresh_token":"playback-token-fixture-value"} "#,
            "callback?code=authorization-code-value&access_token=another-access-token ",
            "cookie SAPISID=sapisid-cookie-value; other=1"
        );
        let cleaned = redact(raw);
        assert!(!cleaned.contains("access-token-value-1234"), "{cleaned}");
        assert!(
            !cleaned.contains("refresh-token-fixture-value"),
            "{cleaned}"
        );
        assert!(
            !cleaned.contains("playback-token-fixture-value"),
            "{cleaned}"
        );
        assert!(!cleaned.contains("authorization-code-value"), "{cleaned}");
        assert!(!cleaned.contains("another-access-token"), "{cleaned}");
        assert!(!cleaned.contains("sapisid-cookie-value"), "{cleaned}");
        assert!(cleaned.contains("[redacted]"), "{cleaned}");
        assert!(cleaned.contains("callback?"), "{cleaned}");
    }

    #[test]
    fn a_new_launch_truncates_the_previous_log() {
        let dir = std::env::temp_dir().join(format!("codevox-mu-log-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("codevox-mu.log");
        fs::create_dir_all(&dir).expect("dir");
        fs::write(&path, "refresh-token-fixture-value\n").expect("seed");
        open_log(&path).expect("truncate");
        let text = fs::read_to_string(&path).expect("read");
        assert!(!text.contains("refresh-token-fixture-value"), "{text}");
        assert!(text.contains("truncated"), "{text}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn auth_urls_append_one_per_line() {
        let dir = std::env::temp_dir().join(format!("codevox-mu-auth-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("urls.txt");
        append_auth_url(&path, "http://127.0.0.1:8898/login?one=1").expect("first");
        append_auth_url(&path, "http://127.0.0.1:8898/login?two=2").expect("second");
        let text = fs::read_to_string(&path).expect("read");
        assert_eq!(
            text,
            "http://127.0.0.1:8898/login?one=1\nhttp://127.0.0.1:8898/login?two=2\n"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stamp_is_utc_and_sortable() {
        let stamp = stamp();
        assert_eq!(stamp.len(), 20, "{stamp}");
        assert!(stamp.ends_with('Z'), "{stamp}");
        assert!(stamp.contains('T'), "{stamp}");
    }
}
