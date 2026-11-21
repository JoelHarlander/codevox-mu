//! `--selftest` checks a saved account without opening the window.
//!
//! The same lines are printed and written to the log. The process exits
//! 0 only when every line is PASS or SKIP. YouTube Music audio is checked
//! without a signed-in account.

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::extract::{InnertubeExtractor, SELFTEST_TRACKS};
use crate::music::MusicService;
use crate::output::{AudioSink, RodioSink, SharedOutput};
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
    let client = resolved.client.clone();
    let itag = resolved.itag;
    let mime = resolved.source.mime.clone();
    // The same sink the window uses, including the shared device. Decoding
    // into a buffer hid a stream that never reached the speakers.
    let output = Arc::new(SharedOutput::new());
    let mut sink = RodioSink::new(output);
    sink.start(resolved.source)
        .map_err(|error| crate::diag::redact(&error.to_string()))?;
    let heard = wait_until_heard(&sink);
    sink.stop();
    let position = heard?;
    Ok(format!(
        "{client} itag {itag}, played {}ms {mime}",
        position.as_millis()
    ))
}

/// Position comes from the device callback. If that callback never runs, it
/// stays at zero and this fails instead of reporting a successful decode.
fn wait_until_heard(sink: &RodioSink) -> Result<Duration, String> {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(4) {
        let position = sink.elapsed();
        if position >= Duration::from_millis(250) {
            return Ok(position);
        }
        if sink.finished() {
            return Err(format!("playback ended after {}ms", position.as_millis()));
        }
        thread::sleep(Duration::from_millis(40));
    }
    Err(format!(
        "output position did not advance (last {}ms)",
        sink.elapsed().as_millis()
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
