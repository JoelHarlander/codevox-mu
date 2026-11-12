//! `--selftest` checks a saved account without opening the window.
//!
//! The same lines are printed and written to the log. The process exits
//! 0 only when every line is PASS or SKIP.

use crate::music::MusicService;
use crate::youtube_account::LibrarySession;
use crate::youtube_music::YouTubeMusic;

/// Run the checks and return the process status. 0 means nothing failed.
pub fn run() -> i32 {
    let query = "love";
    let youtube = YouTubeMusic::new();
    let lines = crate::spotify::self_test(query, |query| youtube_search(&youtube, query));
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
