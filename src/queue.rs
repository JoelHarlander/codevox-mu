//! Play order for a mixed session.
//!
//! One [`Queue`] holds the loaded track, what plays after it, and what
//! previous can return to. Spotify and YouTube Music tracks share that
//! order. The window and the OS media keys both call into this type, so
//! next and previous cannot drift from the panel.

use crate::music::{Source, Track};

/// How far back [`Queue::back`] moved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shift {
    /// `track` is now loaded.
    Play(Track),
    /// Nothing earlier. `track` is still loaded, so the caller restarts it.
    Restart(Track),
    /// Nothing is loaded.
    Idle,
}

/// Result of stepping backwards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Retreat {
    pub shift: Shift,
    /// A local file was in the history and was dropped. Local playback is
    /// not available, so previous skips those entries.
    pub skipped_local: bool,
}

/// Loaded track, the tracks after it, and the tracks already heard.
///
/// [`Queue::enqueue`] adds at the back. [`Queue::play_next`] inserts so the
/// track follows the loaded one. [`Queue::forward`] and [`Queue::back`] walk
/// that order from either end.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Queue {
    history: Vec<Track>,
    current: Option<Track>,
    upcoming: Vec<Track>,
}

const HISTORY_LIMIT: usize = 50;

impl Queue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn current(&self) -> Option<&Track> {
        self.current.as_ref()
    }

    /// Tracks that play after the loaded one, front first.
    pub fn upcoming(&self) -> &[Track] {
        &self.upcoming
    }

    /// Nothing is waiting after the loaded track.
    pub fn is_empty(&self) -> bool {
        self.upcoming.is_empty()
    }

    /// Previous can restart the loaded track, or return to an earlier one.
    pub fn can_back(&self) -> bool {
        self.current.is_some() || !self.history.is_empty()
    }

    /// Add `track` at the end of the upcoming list.
    pub fn enqueue(&mut self, track: Track) {
        self.upcoming.push(track);
    }

    /// Insert `track` so it plays immediately after the loaded one.
    ///
    /// With nothing loaded, it becomes the next track to start.
    pub fn play_next(&mut self, track: Track) {
        self.upcoming.insert(0, track);
    }

    /// Swap the upcoming track at `index` with the one before it.
    ///
    /// The first item and an index past the end stay put.
    pub fn move_earlier(&mut self, index: usize) -> bool {
        if index == 0 || index >= self.upcoming.len() {
            return false;
        }
        self.upcoming.swap(index, index - 1);
        true
    }

    /// Swap the upcoming track at `index` with the one after it.
    ///
    /// The last item and an index past the end stay put.
    pub fn move_later(&mut self, index: usize) -> bool {
        if index >= self.upcoming.len().saturating_sub(1) {
            return false;
        }
        self.upcoming.swap(index, index + 1);
        true
    }

    /// Drop the upcoming track at `index`.
    pub fn remove_upcoming(&mut self, index: usize) -> Option<Track> {
        if index >= self.upcoming.len() {
            return None;
        }
        Some(self.upcoming.remove(index))
    }

    /// Make `track` the loaded one without touching history or the upcoming list.
    pub fn load(&mut self, track: Track) {
        self.current = Some(track);
    }

    /// Remember the loaded track, then clear it. Upcoming stays.
    pub fn remember_current(&mut self) {
        if let Some(track) = self.current.take() {
            self.push_history(track);
        }
    }

    /// Drop the loaded track and load the next upcoming one.
    ///
    /// The dropped track is not remembered, so previous will not return to it.
    /// With nothing waiting, the queue is idle.
    pub fn remove_current(&mut self) {
        self.current = None;
        self.promote();
    }

    /// If nothing is loaded, load the next upcoming track.
    pub fn fill_current(&mut self) {
        if self.current.is_none() {
            self.promote();
        }
    }

    /// The loaded track is remembered. The next upcoming track becomes loaded.
    ///
    /// At the end, the loaded track is remembered and nothing stays loaded.
    /// A second call stays there: history is not played again.
    pub fn forward(&mut self) {
        if let Some(track) = self.current.take() {
            self.push_history(track);
        }
        self.promote();
    }

    /// Step to the previous track.
    ///
    /// At the start, the loaded track stays so the caller can restart it.
    /// With nothing loaded and nothing earlier, the result is [`Shift::Idle`].
    /// Otherwise the newest history entry becomes loaded and the displaced
    /// track is inserted at the front of the upcoming list. Local files in
    /// the history are dropped.
    pub fn back(&mut self) -> Retreat {
        let displaced = self.current.take();
        let mut skipped_local = false;
        loop {
            let Some(track) = self.history.pop() else {
                self.current = displaced;
                let shift = match self.current.clone() {
                    Some(track) => Shift::Restart(track),
                    None => Shift::Idle,
                };
                return Retreat {
                    shift,
                    skipped_local,
                };
            };
            if track.source == Source::Local {
                skipped_local = true;
                continue;
            }
            if let Some(current) = displaced {
                self.upcoming.insert(0, current);
            }
            self.current = Some(track.clone());
            return Retreat {
                shift: Shift::Play(track),
                skipped_local,
            };
        }
    }

    fn promote(&mut self) {
        self.current = if self.upcoming.is_empty() {
            None
        } else {
            Some(self.upcoming.remove(0))
        };
    }

    /// Arrange this queue so playback starts at `index`.
    ///
    /// Tracks before `index` become history, so previous walks back through
    /// them. That prefix is not trimmed to the session history cap: previous
    /// should still reach the first track of a playlist. Tracks from `index`
    /// onward wait as upcoming, and nothing is loaded yet. An empty list, or
    /// an index past the end, leaves the queue idle.
    pub fn start_from(&mut self, tracks: Vec<Track>, index: usize) {
        self.history.clear();
        self.current = None;
        self.upcoming.clear();
        if index >= tracks.len() {
            return;
        }
        let mut tracks = tracks;
        self.upcoming = tracks.split_off(index);
        self.history = tracks;
    }

    fn push_history(&mut self, track: Track) {
        self.history.push(track);
        if self.history.len() > HISTORY_LIMIT {
            self.history.remove(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Queue, Shift};
    use crate::music::{Source, Track};

    fn track(source: Source, id: &str) -> Track {
        Track {
            id: id.to_string(),
            source,
            title: id.to_string(),
            artists: vec![id.to_string()],
            album: None,
            duration: Duration::from_secs(30),
        }
    }

    fn yt(id: &str) -> Track {
        track(Source::YouTubeMusic, id)
    }

    fn sp(id: &str) -> Track {
        track(Source::Spotify, id)
    }

    fn upcoming_ids(queue: &Queue) -> Vec<&str> {
        queue
            .upcoming()
            .iter()
            .map(|item| item.id.as_str())
            .collect()
    }

    #[test]
    fn enqueue_keeps_mixed_source_order() {
        let mut queue = Queue::new();
        assert!(queue.is_empty());
        assert!(queue.current().is_none());

        queue.enqueue(sp("sp-1"));
        queue.enqueue(yt("yt-1"));
        queue.enqueue(sp("sp-2"));
        queue.forward();

        assert_eq!(queue.current().map(|item| item.id.as_str()), Some("sp-1"));
        assert_eq!(
            queue.current().map(|item| item.source),
            Some(Source::Spotify)
        );
        queue.forward();
        assert_eq!(
            queue.current().map(|item| item.source),
            Some(Source::YouTubeMusic)
        );
        queue.forward();
        assert_eq!(queue.current().map(|item| item.id.as_str()), Some("sp-2"));
        assert_eq!(
            queue.current().map(|item| item.source),
            Some(Source::Spotify)
        );
        assert!(queue.is_empty());
    }

    #[test]
    fn play_next_inserts_after_the_current_track() {
        let mut queue = Queue::new();
        queue.load(yt("now"));
        queue.enqueue(yt("later"));
        queue.enqueue(sp("after"));
        queue.play_next(sp("soon"));

        assert_eq!(queue.current().map(|item| item.id.as_str()), Some("now"));
        assert_eq!(upcoming_ids(&queue), ["soon", "later", "after"]);

        queue.forward();
        assert_eq!(queue.current().map(|item| item.id.as_str()), Some("soon"));
        assert_eq!(
            queue.current().map(|item| item.source),
            Some(Source::Spotify)
        );
        queue.forward();
        assert_eq!(queue.current().map(|item| item.id.as_str()), Some("later"));
        assert_eq!(
            queue.current().map(|item| item.source),
            Some(Source::YouTubeMusic)
        );
        assert_eq!(upcoming_ids(&queue), ["after"]);

        let mut idle = Queue::new();
        idle.enqueue(yt("a"));
        idle.play_next(sp("b"));
        idle.forward();
        assert_eq!(idle.current().map(|item| item.id.as_str()), Some("b"));
        assert_eq!(
            idle.current().map(|item| item.source),
            Some(Source::Spotify)
        );
        assert_eq!(upcoming_ids(&idle), ["a"]);
    }

    #[test]
    fn removing_the_current_track_drops_it() {
        let mut queue = Queue::new();
        queue.load(yt("now"));
        queue.enqueue(sp("next"));
        queue.enqueue(yt("last"));

        queue.remove_current();
        assert_eq!(queue.current().map(|item| item.id.as_str()), Some("next"));
        assert_eq!(
            queue.current().map(|item| item.source),
            Some(Source::Spotify)
        );
        assert_eq!(upcoming_ids(&queue), ["last"]);

        let retreat = queue.back();
        assert!(matches!(retreat.shift, Shift::Restart(track) if track.id == "next"));
        assert!(!retreat.skipped_local);
        assert_eq!(queue.current().map(|item| item.id.as_str()), Some("next"));

        queue.remove_current();
        assert_eq!(queue.current().map(|item| item.id.as_str()), Some("last"));
        queue.remove_current();
        assert!(queue.current().is_none());
        assert!(queue.is_empty());
        assert!(matches!(queue.back().shift, Shift::Idle));

        queue.load(sp("only"));
        queue.enqueue(yt("kept"));
        assert!(queue.remove_upcoming(0).is_some());
        assert!(queue.remove_upcoming(3).is_none());
        queue.remove_current();
        assert!(queue.current().is_none());
        assert!(queue.is_empty());
    }

    #[test]
    fn forward_and_back_stop_at_both_ends() {
        let mut queue = Queue::new();
        assert!(matches!(queue.back().shift, Shift::Idle));
        queue.forward();
        assert!(queue.current().is_none());

        queue.load(yt("only"));
        let retreat = queue.back();
        assert!(matches!(retreat.shift, Shift::Restart(track) if track.id == "only"));
        assert_eq!(queue.current().map(|item| item.id.as_str()), Some("only"));
        assert!(queue.upcoming().is_empty());

        queue.forward();
        assert!(queue.current().is_none());
        assert!(queue.can_back());
        queue.forward();
        assert!(queue.current().is_none());
        assert!(queue.is_empty());
        assert!(queue.can_back());

        let retreat = queue.back();
        assert!(matches!(retreat.shift, Shift::Play(track) if track.id == "only"));
        assert_eq!(
            queue.current().map(|item| item.source),
            Some(Source::YouTubeMusic)
        );
        let retreat = queue.back();
        assert!(matches!(retreat.shift, Shift::Restart(track) if track.id == "only"));
        assert!(queue.upcoming().is_empty());
    }

    #[test]
    fn forward_and_back_cross_sources() {
        let mut queue = Queue::new();
        queue.enqueue(yt("yt-1"));
        queue.enqueue(sp("sp-1"));
        queue.enqueue(yt("yt-2"));
        queue.forward();
        queue.forward();
        queue.forward();
        assert_eq!(queue.current().map(|item| item.id.as_str()), Some("yt-2"));
        queue.forward();
        assert!(queue.current().is_none());

        assert!(matches!(queue.back().shift, Shift::Play(track) if track.id == "yt-2"));
        assert!(matches!(
            queue.back().shift,
            Shift::Play(track) if track.source == Source::Spotify
        ));
        assert_eq!(upcoming_ids(&queue), ["yt-2"]);
        assert!(matches!(queue.back().shift, Shift::Play(track) if track.id == "yt-1"));
        assert_eq!(upcoming_ids(&queue), ["sp-1", "yt-2"]);
        assert!(matches!(queue.back().shift, Shift::Restart(track) if track.id == "yt-1"));

        queue.forward();
        assert_eq!(
            queue.current().map(|item| item.source),
            Some(Source::Spotify)
        );
        queue.forward();
        assert_eq!(
            queue.current().map(|item| item.source),
            Some(Source::YouTubeMusic)
        );
    }

    #[test]
    fn move_up_and_down_reorder_upcoming_tracks() {
        let mut queue = Queue::new();
        queue.load(yt("now"));
        queue.enqueue(sp("a"));
        queue.enqueue(yt("b"));
        queue.enqueue(sp("c"));

        assert!(!queue.move_earlier(0));
        assert!(!queue.move_later(2));
        assert!(!queue.move_earlier(9));
        assert!(queue.move_earlier(2));
        assert_eq!(upcoming_ids(&queue), ["a", "c", "b"]);
        assert!(queue.move_later(0));
        assert_eq!(upcoming_ids(&queue), ["c", "a", "b"]);
        assert_eq!(queue.current().map(|item| item.id.as_str()), Some("now"));

        assert_eq!(
            queue.remove_upcoming(1).map(|item| item.id),
            Some("a".to_string())
        );
        assert_eq!(upcoming_ids(&queue), ["c", "b"]);
        queue.forward();
        assert_eq!(queue.current().map(|item| item.id.as_str()), Some("c"));
        assert_eq!(
            queue.current().map(|item| item.source),
            Some(Source::Spotify)
        );
        assert_eq!(upcoming_ids(&queue), ["b"]);
    }

    #[test]
    fn local_history_is_skipped_and_then_previous_stops() {
        let mut queue = Queue::new();
        queue.load(track(Source::Local, "desk"));
        queue.remember_current();
        queue.load(yt("now"));

        let retreat = queue.back();
        assert!(retreat.skipped_local);
        assert!(matches!(retreat.shift, Shift::Restart(track) if track.id == "now"));
        assert_eq!(queue.current().map(|item| item.id.as_str()), Some("now"));
        assert!(queue.upcoming().is_empty());
    }

    #[test]
    fn start_from_keeps_the_prefix_in_history_and_the_rest_upcoming() {
        let mut queue = Queue::new();
        queue.enqueue(yt("old"));
        queue.start_from(vec![sp("a"), yt("b"), sp("c")], 1);

        assert!(queue.current().is_none());
        assert_eq!(upcoming_ids(&queue), ["b", "c"]);
        let retreat = queue.back();
        assert!(matches!(retreat.shift, Shift::Play(track) if track.id == "a"));
        assert_eq!(
            queue.current().map(|item| item.source),
            Some(Source::Spotify)
        );
        assert_eq!(upcoming_ids(&queue), ["b", "c"]);

        let mut idle = Queue::new();
        idle.start_from(vec![yt("only")], 2);
        assert!(idle.current().is_none());
        assert!(idle.upcoming().is_empty());
        assert!(matches!(idle.back().shift, Shift::Idle));
    }
}
