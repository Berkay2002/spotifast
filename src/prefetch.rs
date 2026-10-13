//! Sliding-window audio prefetch: the pure decision logic.
//!
//! Mirrors Spotify's `SlidingWindowPrefetchStrategy`: a window of upcoming
//! tracks that grows when tracks are listened to and shrinks when the user
//! skips or switches context, plus a token bucket that bounds download volume.
//! `Window` and `Bucket` do no I/O and read no clock; time is always passed in,
//! so they are unit-testable. `run` is the task that drives them: it follows the
//! player's events and downloads upcoming tracks into librespot's audio cache,
//! which the player then picks up (`AudioFile::open` returns a cached file).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use librespot_audio::{AudioFile, StreamLoaderController};
use librespot_core::{FileId, Session, SpotifyId, SpotifyUri};
use librespot_metadata::audio::AudioItem;
use librespot_playback::config::Bitrate;
use librespot_playback::player::{PlayerEvent, select_file, stream_data_rate};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::timeout;

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
    /// A duration that arrived before its track became current (the player's
    /// TrackChanged can beat the SetQueue that makes the track current).
    pending_len: Option<(String, u64)>,
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
            pending_len: None,
        }
    }

    /// Playback position of `uri`; ignored unless it is the current track.
    pub fn played(&mut self, uri: &str, ms: u64) {
        if self.item.as_deref() == Some(uri) {
            self.item_pos_ms = ms;
        }
    }

    /// Duration of `uri`. Kept for later when it is not the current track yet.
    pub fn duration(&mut self, uri: &str, ms: u64) {
        if self.item.as_deref() == Some(uri) {
            self.item_len_ms = Some(ms);
        } else {
            self.pending_len = Some((uri.to_owned(), ms));
        }
    }

    pub fn current(&self) -> Option<&str> {
        self.item.as_deref()
    }

    /// Whether `uri` is one of the tracks the window currently covers.
    pub fn in_window(&self, uri: &str) -> bool {
        self.prev.iter().take(self.size).any(|u| u == uri)
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
            self.item_len_ms = match self.pending_len.take() {
                Some((uri, ms)) if uri == current => Some(ms),
                other => {
                    self.pending_len = other;
                    None
                }
            };
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

    /// Whether `need` bytes could ever be taken (it is within the capacity).
    pub fn fits(&self, need: u64) -> bool {
        need as f64 <= self.cap
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

/// How long resolving a track's metadata or opening its download may take.
const OPEN_TIMEOUT: Duration = Duration::from_secs(15);

/// A download in progress. Dropping it cancels the download.
struct InFlight {
    uri: String,
    id: SpotifyId,
    file_id: FileId,
    path: PathBuf,
    started: Instant,
    _file: AudioFile,
    _ctl: StreamLoaderController,
}

struct Prefetcher {
    session: Session,
    bitrate: Bitrate,
    metered: Arc<AtomicBool>,
    window: Window,
    bucket: Bucket,
    failed: HashMap<String, Instant>,
    /// Files of tracks seen so far, to tell whether one is cached.
    files: HashMap<String, FileId>,
    /// The current track and the file the player will use for it, from its
    /// TrackChanged (`None` file: nothing playable, so nothing to wait for).
    current_file: Option<(String, Option<FileId>)>,
    has_current: bool,
    inflight: Option<InFlight>,
    /// Do nothing until then (after a queue change, or while blocked).
    hold_until: Instant,
}

/// Follows the player and keeps the audio cache filled with the tracks about
/// to play. Runs until the event channel closes. `cache_limit` is the audio
/// cache size in bytes.
pub async fn run(
    session: Session,
    mut events: UnboundedReceiver<PlayerEvent>,
    bitrate: Bitrate,
    metered: Arc<AtomicBool>,
    cache_limit: u64,
) {
    let now = Instant::now();
    let mut p = Prefetcher {
        session,
        bitrate,
        metered,
        window: Window::default(),
        bucket: Bucket::new(bucket_cap(cache_limit), REFILL_BYTES, REFILL_PERIOD, now),
        failed: HashMap::new(),
        files: HashMap::new(),
        current_file: None,
        has_current: false,
        inflight: None,
        hold_until: now,
    };
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(event) => p.on_event(event),
                None => return,
            },
            _ = tick.tick() => p.step().await,
        }
    }
}

fn is_track(uri: &str) -> bool {
    uri.starts_with("spotify:track:")
}

impl Prefetcher {
    fn on_event(&mut self, event: PlayerEvent) {
        match event {
            PlayerEvent::SetQueue {
                current_track,
                next_tracks,
                ..
            } => {
                let Some(current) = current_track else {
                    self.has_current = false;
                    self.drop_inflight("cancel", "no current track");
                    return;
                };
                let before = self.window.size;
                let upcoming: Vec<String> = next_tracks.into_iter().map(|t| t.uri).collect();
                self.window.update(&current.uri, &upcoming);
                self.has_current = true;
                self.hold_until = Instant::now() + SETTLE;
                if self.window.size != before {
                    log::debug!("prefetch: window {before} -> {}", self.window.size);
                }
            }
            PlayerEvent::Stopped { .. } => self.has_current = false,
            PlayerEvent::PositionChanged {
                track_id,
                position_ms,
                ..
            }
            | PlayerEvent::Seeked {
                track_id,
                position_ms,
                ..
            }
            | PlayerEvent::Paused {
                track_id,
                position_ms,
                ..
            }
            | PlayerEvent::Playing {
                track_id,
                position_ms,
                ..
            } => {
                let uri = track_id.to_uri();
                if self.window.current() == Some(uri.as_str()) {
                    self.has_current = true;
                }
                self.window.played(&uri, u64::from(position_ms));
            }
            PlayerEvent::TrackChanged { audio_item } => {
                self.window
                    .duration(&audio_item.uri, u64::from(audio_item.duration_ms));
                let file = select_file(&audio_item.files, self.bitrate).map(|(_, id)| id);
                if let Some(id) = file {
                    self.files.insert(audio_item.uri.clone(), id);
                }
                self.current_file = Some((audio_item.uri, file));
            }
            _ => return,
        }
        // A prefetch for the track that just became current, or that left the
        // window, is pointless.
        let stale = self.inflight.as_ref().is_some_and(|f| {
            self.window.current() == Some(f.uri.as_str()) || !self.window.in_window(&f.uri)
        });
        if stale {
            self.drop_inflight("cancel", "track is current or left the window");
        }
    }

    fn drop_inflight(&mut self, what: &str, why: &str) {
        if let Some(f) = self.inflight.take() {
            log::debug!("prefetch: {what} {} ({why})", f.uri);
        }
    }

    fn fail(&mut self, uri: &str, why: &str) {
        log::debug!("prefetch: failed {uri} ({why})");
        self.failed.insert(uri.to_owned(), Instant::now());
        // The map only ever needs the last minute.
        self.failed.retain(|_, at| at.elapsed() < FAILED_TTL);
    }

    fn cache_path(&self, file: FileId) -> Option<PathBuf> {
        self.session.cache()?.file_path(file)
    }

    fn cached(&self, file: FileId) -> bool {
        self.cache_path(file).is_some_and(|p| p.exists())
    }

    /// The audio cache is on (a path exists for any file id).
    fn cache_on(&self) -> bool {
        self.cache_path(FileId::from_raw(&[0; 20])).is_some()
    }

    async fn step(&mut self) {
        if !self.cache_on() || self.metered.load(Ordering::Relaxed) || !self.has_current {
            self.drop_inflight("cancel", "off, metered or nothing playing");
            return;
        }
        if let Some(f) = &self.inflight {
            if f.path.exists() {
                let (uri, id, file_id) = (f.uri.clone(), f.id, f.file_id);
                let secs = f.started.elapsed().as_secs_f32();
                self.inflight = None;
                log::debug!("prefetch: done {uri} in {secs:.1}s");
                // Warm the audio key so the track starts without a round trip.
                let session = self.session.clone();
                tokio::spawn(async move {
                    if let Err(e) = session.audio_key().request(id, file_id).await {
                        log::debug!("prefetch: key for {uri} failed: {e}");
                    }
                });
            } else if f.started.elapsed() >= INFLIGHT_TIMEOUT {
                let uri = f.uri.clone();
                self.inflight = None;
                self.fail(&uri, "timeout");
            }
            return;
        }
        if Instant::now() < self.hold_until || !self.current_cached() {
            return;
        }
        // One resolve per window slot per step; failed and cached tracks are
        // skipped by `next`.
        for _ in 0..=MAX {
            let now = Instant::now();
            let cached = |uri: &str| self.files.get(uri).is_some_and(|id| self.cached(*id));
            let Some((_, uri)) = self.window.next(cached, &self.failed, now) else {
                return;
            };
            if !self.start(&uri, now).await {
                return;
            }
        }
    }

    /// Priority gate: the current track's file must be in the cache first.
    fn current_cached(&self) -> bool {
        let Some(current) = self.window.current() else {
            return false;
        };
        if !is_track(current) {
            return true;
        }
        match &self.current_file {
            Some((uri, file)) if uri == current => file.is_none_or(|id| self.cached(id)),
            _ => false, // its TrackChanged has not arrived yet
        }
    }

    /// Resolves and starts downloading `uri`. Returns whether to try the next
    /// candidate now (`false`: started, or blocked until later).
    async fn start(&mut self, uri: &str, now: Instant) -> bool {
        let Ok(SpotifyUri::Track { id }) = SpotifyUri::from_uri(uri) else {
            self.fail(uri, "not a track");
            return true;
        };
        let spotify_uri = SpotifyUri::Track { id };
        let item = match timeout(
            OPEN_TIMEOUT,
            AudioItem::get_file(&self.session, spotify_uri),
        )
        .await
        {
            Ok(Ok(item)) => item,
            Ok(Err(e)) => {
                self.fail(uri, &format!("metadata: {e}"));
                return true;
            }
            Err(_) => {
                self.fail(uri, "metadata timeout");
                return true;
            }
        };
        if let Err(reason) = &item.availability {
            self.fail(uri, &format!("unavailable: {reason:?}"));
            return true;
        }
        if u64::from(item.duration_ms) > MAX_TRACK_S * 1000 {
            self.fail(uri, "longer than 420 s");
            return true;
        }
        let Some((format, file_id)) = select_file(&item.files, self.bitrate) else {
            self.fail(uri, "no playable file");
            return true;
        };
        self.files.insert(uri.to_owned(), file_id);
        if self.cached(file_id) {
            return true; // `next` skips it now
        }
        let rate = stream_data_rate(format).unwrap_or(40 * 1024);
        let need = u64::from(item.duration_ms) * rate as u64 / 1000;
        if !self.bucket.fits(need) {
            self.fail(uri, "larger than the budget");
            return true;
        }
        if !self.bucket.take(need, now) {
            log::debug!("prefetch: budget empty, retry in {RETRY:?}");
            self.hold_until = now + RETRY;
            return false;
        }
        let opened = timeout(OPEN_TIMEOUT, AudioFile::open(&self.session, file_id, rate)).await;
        let file = match opened {
            Ok(Ok(file)) => file,
            Ok(Err(e)) => {
                self.fail(uri, &format!("open: {e}"));
                return true;
            }
            Err(_) => {
                self.fail(uri, "open timeout");
                return true;
            }
        };
        let (Some(path), Ok(ctl)) = (
            self.cache_path(file_id),
            file.get_stream_loader_controller(),
        ) else {
            self.fail(uri, "no cache path or controller");
            return true;
        };
        // Without this only the first 64 KiB is fetched.
        ctl.set_stream_mode();
        log::debug!("prefetch: start {uri} ({need} bytes)");
        self.inflight = Some(InFlight {
            uri: uri.to_owned(),
            id,
            file_id,
            path,
            started: now,
            _file: file,
            _ctl: ctl,
        });
        false
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

    #[test]
    fn duration_before_set_queue_is_applied() {
        let mut w = Window::default();
        w.update(&uri("a"), &uris(&["b", "c", "d"]));
        // TrackChanged for "b" arrives before the SetQueue that makes "b" current.
        w.duration(&uri("b"), 200_000);
        w.update(&uri("b"), &uris(&["c", "d", "e"]));
        assert_eq!(w.size, 2); // "a" had no length: counted as played
        w.played(&uri("b"), 10_000);
        w.update(&uri("c"), &uris(&["d", "e", "f"]));
        // "b" is 5% played of a known 200 s: an early skip, not a play.
        assert_eq!(w.size, 1);
    }

    #[test]
    fn current_and_in_window() {
        let mut w = Window::default();
        assert_eq!(w.current(), None);
        w.update(&uri("a"), &uris(&["b", "c", "d"]));
        assert_eq!(w.current(), Some(uri("a").as_str()));
        assert!(w.in_window(&uri("b")));
        assert!(!w.in_window(&uri("c"))); // size 1
        w.size = 3;
        assert!(w.in_window(&uri("d")));
        assert!(!w.in_window(&uri("a")));
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
    fn bucket_fits_only_within_cap() {
        let b = bucket(Instant::now());
        assert!(b.fits(100));
        assert!(!b.fits(101));
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
