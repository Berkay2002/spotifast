//! Sliding-window audio prefetch: the pure decision logic.
//!
//! Mirrors Spotify's `SlidingWindowPrefetchStrategy`: a window of upcoming
//! tracks that grows when tracks are listened to and shrinks when the user
//! skips or switches context, plus a token bucket that bounds download volume.
//! No I/O and no clock reads here; time is always passed in, so everything is
//! unit-testable. The download task that uses this comes in a later change.

// The wiring into the player lands in the next change; until then only tests use these.
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Smallest window (tracks ahead).
pub const MIN: usize = 1;
/// Largest window.
pub const MAX: usize = 3;
/// Window growth per fully played track.
pub const INC: usize = 1;
/// How long a track that failed to prefetch is skipped.
pub const FAILED_TTL: Duration = Duration::from_secs(60);
/// Tracks longer than this are not prefetched (they still occupy a slot).
pub const MAX_TRACK_S: u64 = 420;
/// Wait after a queue change before acting.
pub const SETTLE: Duration = Duration::from_secs(5);
/// Retry delay when blocked (bucket empty, priority gate).
pub const RETRY: Duration = Duration::from_secs(10);
/// A prefetch not finished after this long is dropped and marked failed.
pub const INFLIGHT_TIMEOUT: Duration = Duration::from_secs(120);
/// Upper bound of the token bucket.
pub const BUCKET_CAP_MAX: u64 = 100 * 1024 * 1024;
/// Bucket refill: `REFILL_BYTES` per `REFILL_PERIOD`.
pub const REFILL_BYTES: u64 = 100 * 1024 * 1024;
pub const REFILL_PERIOD: Duration = Duration::from_secs(30 * 60);

/// Bucket capacity: 100 MB, or a quarter of the audio cache limit if smaller.
pub fn bucket_cap(cache_limit_bytes: u64) -> u64 {
    BUCKET_CAP_MAX.min(cache_limit_bytes / 4)
}

/// Window state. Tracks are `spotify:track:...` uris.
#[derive(Debug, Clone)]
pub struct Window {
    /// Current window size in tracks, `MIN..=max`.
    pub size: usize,
    max: usize,
    /// Current track and what we know about it.
    item: Option<String>,
    item_len_ms: Option<u64>,
    item_pos_ms: u64,
    /// Upcoming tracks at the last update, at most `max` of them.
    prev: Vec<String>,
}

impl Default for Window {
    fn default() -> Self {
        Self::with_max(MAX)
    }
}

impl Window {
    /// Window that starts at `MIN` and never exceeds `max` (tests use other maxes).
    pub fn with_max(max: usize) -> Self {
        Self {
            size: MIN,
            max: max.max(MIN),
            item: None,
            item_len_ms: None,
            item_pos_ms: 0,
            prev: Vec::new(),
        }
    }

    /// Playback position of `uri`; ignored unless it is the current track.
    pub fn played(&mut self, uri: &str, ms: u64) {
        if self.item.as_deref() == Some(uri) {
            self.item_pos_ms = ms;
        }
    }

    /// Duration of `uri`; ignored unless it is the current track.
    pub fn duration(&mut self, uri: &str, ms: u64) {
        if self.item.as_deref() == Some(uri) {
            self.item_len_ms = Some(ms);
        }
    }

    /// Queue or current track changed. `upcoming` is the queue after `current`.
    pub fn update(&mut self, current: &str, upcoming: &[String]) {
        let mut skipped_early = false;
        if self.item.as_deref() != Some(current) {
            if self.item.is_some() {
                // Unknown length counts as played.
                let played = match self.item_len_ms {
                    None | Some(0) => true,
                    Some(len) => self.item_pos_ms * 100 >= len * 75,
                };
                if played {
                    self.size = (self.size + INC).min(self.max);
                } else {
                    skipped_early = true;
                }
            }
            self.item = Some(current.to_owned());
            self.item_len_ms = None;
            self.item_pos_ms = 0;
        }

        let new: Vec<String> = upcoming.iter().take(self.max).cloned().collect();
        let common = new.iter().filter(|u| self.prev.contains(u)).count();
        let diff = self.prev.len().max(new.len()) - common;
        if diff > 2 * self.max / 3 {
            // Looks like a new context.
            self.size = MIN;
        } else if skipped_early || diff >= 3 {
            // Looks like a skip.
            self.size = (self.size / 2).max(MIN);
        }
        self.prev = new;
    }

    /// First upcoming track that is neither cached nor recently failed, with its
    /// index. `None` when there is none or it lies beyond the window.
    pub fn next(
        &self,
        is_cached: impl Fn(&str) -> bool,
        failed: &HashMap<String, Instant>,
        now: Instant,
    ) -> Option<(usize, String)> {
        let (idx, uri) = self.prev.iter().enumerate().find(|(_, uri)| {
            let recently_failed = failed
                .get(uri.as_str())
                .is_some_and(|&at| now.saturating_duration_since(at) < FAILED_TTL);
            !is_cached(uri) && !recently_failed
        })?;
        (idx < self.size).then(|| (idx, uri.clone()))
    }
}

/// Token bucket in bytes; starts full.
#[derive(Debug, Clone)]
pub struct Bucket {
    tokens: f64,
    cap: f64,
    /// Tokens added per second.
    rate: f64,
    last: Instant,
}

impl Bucket {
    pub fn new(cap: u64, refill: u64, period: Duration, now: Instant) -> Self {
        Self {
            tokens: cap as f64,
            cap: cap as f64,
            rate: refill as f64 / period.as_secs_f64(),
            last: now,
        }
    }

    /// Refill up to `now`, then spend `need` bytes if available.
    pub fn take(&mut self, need: u64, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = self.last.max(now);
        self.tokens = (self.tokens + elapsed * self.rate).min(self.cap);
        let need = need as f64;
        if need > self.tokens {
            return false;
        }
        self.tokens -= need;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uris(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| format!("spotify:track:{n}")).collect()
    }

    fn uri(n: &str) -> String {
        format!("spotify:track:{n}")
    }

    /// Play `cur` to `pct` percent of a 200 s track, then the queue moves to `next_cur`.
    fn listen(w: &mut Window, cur: &str, pct: u64, next_cur: &str, upcoming: &[&str]) {
        w.duration(&uri(cur), 200_000);
        w.played(&uri(cur), 2_000 * pct);
        w.update(&uri(next_cur), &uris(upcoming));
    }

    #[test]
    fn grows_and_stops_at_max() {
        let mut w = Window::default();
        assert_eq!(w.size, MIN);
        w.update(&uri("a"), &uris(&["b", "c", "d"]));
        assert_eq!(w.size, 1);
        listen(&mut w, "a", 100, "b", &["c", "d", "e"]);
        assert_eq!(w.size, 2);
        listen(&mut w, "b", 75, "c", &["d", "e", "f"]);
        assert_eq!(w.size, 3);
        listen(&mut w, "c", 100, "d", &["e", "f", "g"]);
        assert_eq!(w.size, MAX);
    }

    #[test]
    fn unknown_length_counts_as_played() {
        let mut w = Window::default();
        w.update(&uri("a"), &uris(&["b", "c", "d"]));
        w.update(&uri("b"), &uris(&["c", "d", "e"]));
        assert_eq!(w.size, 2);
    }

    #[test]
    fn early_skip_halves_with_min_floor() {
        let mut w = Window::default();
        w.update(&uri("a"), &uris(&["b", "c", "d"]));
        listen(&mut w, "a", 100, "b", &["c", "d", "e"]);
        listen(&mut w, "b", 100, "c", &["d", "e", "f"]);
        assert_eq!(w.size, 3);
        listen(&mut w, "c", 10, "d", &["e", "f", "g"]);
        assert_eq!(w.size, 1); // 3 / 2 = 1
        listen(&mut w, "d", 10, "e", &["f", "g", "h"]);
        assert_eq!(w.size, MIN);
        // 74% is still an early skip.
        let mut w = Window::default();
        w.update(&uri("a"), &uris(&["b", "c", "d"]));
        w.size = 3;
        listen(&mut w, "a", 74, "b", &["c", "d", "e"]);
        assert_eq!(w.size, 1);
    }

    #[test]
    fn new_context_resets() {
        let mut w = Window::default();
        w.update(&uri("a"), &uris(&["b", "c", "d"]));
        listen(&mut w, "a", 100, "b", &["c", "d", "e"]);
        listen(&mut w, "b", 100, "c", &["d", "e", "f"]);
        assert_eq!(w.size, 3);
        // Fully played, but the whole upcoming list changed: diff 3 > 2.
        listen(&mut w, "c", 100, "x", &["y", "z", "w"]);
        assert_eq!(w.size, MIN);
    }

    #[test]
    fn large_diff_halves_with_max_6() {
        let mut w = Window::with_max(6);
        w.update(&uri("a"), &uris(&["b", "c", "d", "e", "f", "g"]));
        w.size = 4;
        // Same current, 3 of 6 upcoming replaced: diff 3 <= 2*6/3 = 4, so halve.
        w.update(&uri("a"), &uris(&["b", "c", "d", "x", "y", "z"]));
        assert_eq!(w.size, 2);
        // diff 5 > 4: reset.
        w.size = 4;
        w.update(&uri("a"), &uris(&["q", "r", "s", "t", "u", "z"]));
        assert_eq!(w.size, MIN);
        // diff 2: nothing happens.
        w.size = 4;
        w.update(&uri("a"), &uris(&["q", "r", "s", "t", "m", "n"]));
        assert_eq!(w.size, 4);
        // Growth honours the custom max.
        for i in 0..8 {
            let (cur, nxt) = (format!("c{i}"), format!("c{}", i + 1));
            w.update(&uri(&cur), &uris(&["q", "r", "s", "t", "m", "n"]));
            w.update(&uri(&nxt), &uris(&["q", "r", "s", "t", "m", "n"]));
        }
        assert_eq!(w.size, 6);
    }

    #[test]
    fn events_for_other_uri_ignored() {
        let mut w = Window::default();
        w.update(&uri("a"), &uris(&["b", "c", "d"]));
        w.duration(&uri("a"), 200_000);
        w.played(&uri("a"), 10_000);
        // Stale events for another track must not make "a" look played.
        w.played(&uri("zzz"), 200_000);
        w.duration(&uri("zzz"), 1);
        w.update(&uri("b"), &uris(&["c", "d", "e"]));
        assert_eq!(w.size, 1); // skipped early at 5%, not grown
        // Events before the first update (no current item) are ignored too.
        let mut w = Window::default();
        w.played(&uri("a"), 5);
        assert_eq!(w.item_pos_ms, 0);
    }

    fn window_at(size: usize) -> Window {
        let mut w = Window::default();
        w.update(&uri("a"), &uris(&["b", "c", "d"]));
        w.size = size;
        w
    }

    #[test]
    fn next_respects_window_and_cache() {
        let now = Instant::now();
        let failed = HashMap::new();
        let none_cached = |_: &str| false;
        assert_eq!(
            window_at(1).next(none_cached, &failed, now),
            Some((0, uri("b")))
        );

        // First is cached, but the next uncached one (idx 1) is outside a window of 1.
        let b_cached = |u: &str| u == uri("b");
        assert_eq!(window_at(1).next(b_cached, &failed, now), None);
        assert_eq!(
            window_at(2).next(b_cached, &failed, now),
            Some((1, uri("c")))
        );

        let all = |_: &str| true;
        assert_eq!(window_at(3).next(all, &failed, now), None);
    }

    #[test]
    fn failure_expires_at_exactly_60s() {
        let t0 = Instant::now();
        let mut failed = HashMap::new();
        failed.insert(uri("b"), t0);
        let w = window_at(3);
        let none_cached = |_: &str| false;
        let just_before = t0 + FAILED_TTL - Duration::from_millis(1);
        assert_eq!(
            w.next(none_cached, &failed, just_before),
            Some((1, uri("c")))
        );
        assert_eq!(
            w.next(none_cached, &failed, t0 + FAILED_TTL),
            Some((0, uri("b")))
        );
        // Failed track at idx 0 with window 1: skipping it lands outside the window.
        let w = window_at(1);
        assert_eq!(w.next(none_cached, &failed, just_before), None);
    }

    fn bucket(t0: Instant) -> Bucket {
        // 100 bytes capacity, 10 bytes per second.
        Bucket::new(100, 600, Duration::from_secs(60), t0)
    }

    #[test]
    fn bucket_refuses_when_empty() {
        let t0 = Instant::now();
        let mut b = bucket(t0);
        assert!(b.take(100, t0));
        assert!(!b.take(1, t0));
        assert!(!b.take(101, t0 + Duration::from_secs(1000))); // above cap never works
    }

    #[test]
    fn bucket_refills() {
        let t0 = Instant::now();
        let mut b = bucket(t0);
        assert!(b.take(100, t0));
        assert!(!b.take(50, t0 + Duration::from_secs(4))); // 40 tokens
        assert!(b.take(50, t0 + Duration::from_secs(5))); // 50 tokens
        assert!(!b.take(1, t0 + Duration::from_secs(5)));
    }

    #[test]
    fn bucket_never_exceeds_cap() {
        let t0 = Instant::now();
        let mut b = bucket(t0);
        assert!(b.take(100, t0));
        let later = t0 + Duration::from_secs(10_000);
        assert!(b.take(100, later));
        assert!(!b.take(1, later));
    }

    #[test]
    fn cap_is_quarter_of_cache_or_100mb() {
        assert_eq!(bucket_cap(40 * 1024 * 1024), 10 * 1024 * 1024);
        assert_eq!(bucket_cap(10 * 1024 * 1024 * 1024), BUCKET_CAP_MAX);
        assert_eq!(bucket_cap(0), 0);
    }
}
