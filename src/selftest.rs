//! `--selftest` checks a saved account without opening the window.
//!
//! The same lines are printed and written to the log. The process exits
//! 0 only when every line is PASS or SKIP. YouTube Music audio is checked
//! without a signed-in account.

use std::time::Duration;

use crate::extract::{InnertubeExtractor, SELFTEST_TRACKS};
use crate::music::MusicService;
use crate::output;
use crate::spotify::Check;
use crate::youtube_account::LibrarySession;
use crate::youtube_music::YouTubeMusic;

/// Run the checks and return the process status. 0 means nothing failed.
pub fn run() -> i32 {
    let query = "love";
    let youtube = YouTubeMusic::new();
    let mut lines = crate::spotify::self_test(query, |query| youtube_search(&youtube, query));
    lines.push(youtube_audio());
    println!("codevox-mu selftest");
    log::info!("selftest: started");
    let mut failed = false;
    for line in &lines {
        let text = line.line();
        println!("{text}");
        log::info!("{text}");
        failed |= line.status.failed();
    }
    if failed { 1 } else { 0 }
}

/// Download and decode a few seconds of popular tracks. No YouTube account.
fn youtube_audio() -> Check {
    let mut extractor = InnertubeExtractor::new();
    let mut parts = Vec::new();
    let mut failed = false;
    for (video_id, title) in SELFTEST_TRACKS {
        match listen(&mut extractor, video_id) {
            Ok(detail) => parts.push(format!("\"{title}\": {detail}")),
            Err(error) => {
                failed = true;
                parts.push(format!("\"{title}\": {error}"));
            }
        }
    }
    let detail = parts.join("; ");
    if failed {
        Check::fail("youtube audio", detail)
    } else {
        Check::pass("youtube audio", detail)
    }
}

fn listen(extractor: &mut InnertubeExtractor, video_id: &str) -> Result<String, String> {
    let resolved = extractor
        .open(video_id)
        .map_err(|error| crate::diag::redact(&error.to_string()))?;
    let decoded = output::decode_prefix(
        &resolved.source.bytes,
        &resolved.source.mime,
        Duration::from_secs(5),
    )
    .map_err(|error| crate::diag::redact(&error))?;
    if decoded.loud_samples() < 1000 {
        return Err(format!(
            "{} itag {} decoded silence ({} loud samples)",
            resolved.client,
            resolved.itag,
            decoded.loud_samples()
        ));
    }
    Ok(format!(
        "{} itag {}, {:.1}s {}",
        resolved.client,
        resolved.itag,
        decoded.seconds(),
        resolved.source.mime
    ))
}

fn youtube_search(youtube: &YouTubeMusic, query: &str) -> Result<Option<usize>, String> {
    match youtube.restore() {
        Ok(true) => match youtube.search(query) {
            Ok(tracks) => Ok(Some(tracks.len())),
            Err(error) => Err(crate::diag::redact(&error.to_string())),
        },
        Ok(false) => Ok(None),
        Err(error) => Err(crate::diag::redact(&error.to_string())),
    }
}
