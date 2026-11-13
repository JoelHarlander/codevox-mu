//! Line-timed lyrics in the LRC text format.
//!
//! This module only parses text. It does not fetch lyrics and it does not
//! know which track is playing. A positive `[offset:]` advances every line:
//! the signed millisecond value is subtracted from each timestamp, which is
//! the usual reading of that tag (a positive offset means the words should
//! appear sooner). Enhanced `<mm:ss>` word timings are removed so the line
//! still reads as one phrase.

use std::time::Duration;

/// One lyric line and the position where it becomes current.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub at: Duration,
    pub text: String,
}

/// Timed lines in ascending order.
///
/// Metadata tags are dropped. A line that is not a timestamp is skipped.
/// Several timestamps in front of one phrase become several lines. An empty
/// phrase is kept so a timed gap still moves the highlight.
pub fn parse(lrc: &str) -> Vec<Line> {
    let lrc = lrc.strip_prefix('\u{feff}').unwrap_or(lrc);
    let mut offset_ms = 0i64;
    let mut raw = Vec::new();
    for line in lrc.split('\n') {
        let line = line.trim_end_matches('\r').trim();
        if line.is_empty() {
            continue;
        }
        collect_line(line, &mut offset_ms, &mut raw);
    }
    let mut lines: Vec<Line> = raw
        .into_iter()
        .map(|(stamp_ms, text)| Line {
            at: Duration::from_millis(stamp_ms.saturating_sub(offset_ms).max(0) as u64),
            text,
        })
        .collect();
    lines.sort_by_key(|line| line.at);
    lines
}

/// The line that should be highlighted at `position`.
///
/// That is the line with the latest start at or before `position`. When two
/// lines share that start, the later one is current. Nothing is current
/// before the first timestamp. `lines` does not have to be sorted.
pub fn current_line(lines: &[Line], position: Duration) -> Option<usize> {
    let mut best_at = None;
    let mut best_index = None;
    for (index, line) in lines.iter().enumerate() {
        if line.at > position {
            continue;
        }
        // A tie keeps the later line, matching a sorted lyric with a repeated stamp.
        if best_at.is_none_or(|at: Duration| line.at >= at) {
            best_at = Some(line.at);
            best_index = Some(index);
        }
    }
    best_index
}

fn collect_line(line: &str, offset_ms: &mut i64, raw: &mut Vec<(i64, String)>) {
    let mut rest = line;
    let mut stamps = Vec::new();
    while let Some(stripped) = rest.strip_prefix('[') {
        let Some(end) = stripped.find(']') else {
            break;
        };
        let tag = &stripped[..end];
        if let Some(ms) = parse_stamp(tag) {
            stamps.push(ms);
            rest = &stripped[end + 1..];
            continue;
        }
        if let Some(value) = parse_offset(tag) {
            *offset_ms = value;
            rest = &stripped[end + 1..];
            continue;
        }
        if stamps.is_empty() {
            return;
        }
        break;
    }
    if stamps.is_empty() {
        return;
    }
    let text = strip_word_times(rest.trim()).trim().to_string();
    for stamp in stamps {
        raw.push((stamp, text.clone()));
    }
}

/// `mm:ss`, `mm:ss.frac`, or `hh:mm:ss.frac`. Fraction digits are scaled to
/// milliseconds, so `.5`, `.50`, and `.500` are all 500 ms.
fn parse_stamp(tag: &str) -> Option<i64> {
    let mut parts = tag.split(':');
    let first = parts.next()?;
    let second = parts.next()?;
    let third = parts.next();
    if parts.next().is_some() {
        return None;
    }
    let (minutes, seconds) = if let Some(seconds) = third {
        let hours: i64 = parse_whole(first)?;
        let minutes: i64 = parse_whole(second)?;
        (hours.checked_mul(60)?.checked_add(minutes)?, seconds)
    } else {
        (parse_whole(first)?, second)
    };
    let (secs, frac_ms) = split_seconds(seconds)?;
    minutes
        .checked_mul(60_000)?
        .checked_add(secs.checked_mul(1_000)?)?
        .checked_add(frac_ms)
}

fn parse_whole(text: &str) -> Option<i64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

fn split_seconds(text: &str) -> Option<(i64, i64)> {
    let (whole, frac) = match text.split_once('.') {
        Some((whole, frac)) => (whole, Some(frac)),
        None => (text, None),
    };
    let secs = parse_whole(whole)?;
    let frac_ms = match frac {
        None => 0,
        Some(digits) => {
            let value = parse_whole(digits)?;
            let mut denom = 1i64;
            for _ in 0..digits.len() {
                denom = denom.checked_mul(10)?;
            }
            value.checked_mul(1_000)?.checked_div(denom)?
        }
    };
    Some((secs, frac_ms))
}

fn parse_offset(tag: &str) -> Option<i64> {
    let rest = strip_prefix_ignore_ascii(tag, "offset:")?.trim();
    if rest.is_empty() {
        return None;
    }
    rest.parse().ok()
}

fn strip_prefix_ignore_ascii<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let prefix_len = prefix.len();
    if text.len() >= prefix_len && text[..prefix_len].eq_ignore_ascii_case(prefix) {
        Some(&text[prefix_len..])
    } else {
        None
    }
}

fn strip_word_times(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        if let Some(end) = after.find('>')
            && parse_stamp(&after[..end]).is_some()
        {
            rest = &after[end + 1..];
            continue;
        }
        out.push('<');
        rest = after;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: u64, millis: u64) -> Duration {
        Duration::from_millis(seconds * 1_000 + millis)
    }

    #[test]
    fn timestamps_keep_the_phrase_and_scale_the_fraction() {
        let lines =
            parse("[00:12.34] Streetlights\n[00:15.5] Lane\n[01:02] Home\n[00:18.345] Radio\n");
        assert_eq!(
            lines,
            vec![
                Line {
                    at: at(12, 340),
                    text: "Streetlights".into(),
                },
                Line {
                    at: at(15, 500),
                    text: "Lane".into(),
                },
                Line {
                    at: at(18, 345),
                    text: "Radio".into(),
                },
                Line {
                    at: at(62, 0),
                    text: "Home".into(),
                },
            ]
        );
    }

    #[test]
    fn one_phrase_with_two_timestamps_becomes_two_lines() {
        let lines = parse("[00:12.00][00:24.50] Chorus\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].at, at(12, 0));
        assert_eq!(lines[1].at, at(24, 500));
        assert_eq!(lines[0].text, "Chorus");
        assert_eq!(lines[1].text, "Chorus");
    }

    #[test]
    fn a_positive_offset_advances_every_line_and_clamps_at_zero() {
        let lines = parse("[offset:+1000]\n[00:02.00] Hi\n[00:00.40] Early\n");
        assert_eq!(lines[0].text, "Early");
        assert_eq!(lines[0].at, Duration::ZERO);
        assert_eq!(lines[1].at, at(1, 0));

        let delayed = parse("[OFFSET:-1500]\n[00:01.00] Late\n");
        assert_eq!(delayed[0].at, at(2, 500));
    }

    #[test]
    fn metadata_is_ignored_and_word_timings_are_removed() {
        let lines = parse(
            "[ar:Ada Vega]\n[ti:Night Drive]\n[al:Low Glow]\n[00:12.00]<00:12.10>Keep <00:12.40>the window\nnot a lyric\n[00:20.00]\n",
        );
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "Keep the window");
        assert_eq!(lines[1].text, "");
        assert_eq!(lines[1].at, at(20, 0));
    }

    #[test]
    fn hours_crlf_and_a_bom_parse() {
        let lines = parse("\u{feff}[01:02:03.25] Far\r\n[00:01.00] Near\r\n");
        assert_eq!(lines[0].text, "Near");
        assert_eq!(lines[0].at, at(1, 0));
        assert_eq!(lines[1].at, at(3_723, 250));
    }

    #[test]
    fn a_broken_stamp_does_not_drop_the_other_lines() {
        let lines = parse("[00:01.00] Kept\n[aa:bb] Dropped\n[:01] Dropped\n[00:02.00] Still\n");
        assert_eq!(
            lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            vec!["Kept", "Still"]
        );
    }

    #[test]
    fn current_line_is_the_last_one_that_has_started() {
        let lines = parse("[00:10.00] A\n[00:20.00] B\n[00:20.00] C\n[00:30.00] D\n");
        assert_eq!(current_line(&lines, at(9, 999)), None);
        assert_eq!(current_line(&lines, at(10, 0)), Some(0));
        assert_eq!(current_line(&lines, at(19, 0)), Some(0));
        assert_eq!(current_line(&lines, at(20, 0)), Some(2));
        assert_eq!(current_line(&lines, at(45, 0)), Some(3));
        assert_eq!(current_line(&[], at(0, 0)), None);
    }

    #[test]
    fn current_line_does_not_require_sorted_input() {
        let lines = vec![
            Line {
                at: at(30, 0),
                text: "Later".into(),
            },
            Line {
                at: at(5, 0),
                text: "Earlier".into(),
            },
        ];
        assert_eq!(current_line(&lines, at(10, 0)), Some(1));
        assert_eq!(current_line(&lines, at(30, 0)), Some(0));
    }
}
