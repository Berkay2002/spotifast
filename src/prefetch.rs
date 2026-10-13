//! Sliding-window audio prefetch: the pure decision logic.
//!
//! Mirrors Spotify's `SlidingWindowPrefetchStrategy`: a window of upcoming
//! tracks that grows when tracks are listened to and shrinks when the user
//! skips or switches context, plus a token bucket that bounds download volume.
//! `Window`, `Bucket` and `Core` do no I/O and read no clock; time is always passed in,
//! so they are unit-testable. `run` is the task that drives them: it follows the
//! player's events and downloads upcoming tracks into librespot's audio cache,
//! which the player then picks up (`AudioFile::open` returns a cached file).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
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
/// A prefetch that received no bytes for this long is stalled and dropped.
pub const STALL_TIMEOUT: Duration = Duration::from_secs(30);
/// After a stall, prefetching stays off at least this long, and until the
/// current track changes, so a dead link is not hammered.
pub const SUSPEND: Duration = Duration::from_secs(10 * 60);
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

    /// Change the capacity; the tokens never grow by it, and shrink if above it.
    pub fn set_cap(&mut self, cap: u64) {
        self.cap = cap as f64;
        self.tokens = self.tokens.min(self.cap);
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

/// A shared token bucket, so engine restarts and reconnects do not refill it.
/// Empty until the first prefetcher fills it in.
pub type SharedBucket = Arc<Mutex<Option<Bucket>>>;

/// Creates the bucket in `shared` (full) the first time; later calls only
/// adjust its capacity, never its tokens.
pub fn init_bucket(shared: &SharedBucket, cap: u64, now: Instant) {
    let mut guard = shared.lock().unwrap_or_else(PoisonError::into_inner);
    match guard.as_mut() {
        Some(bucket) => bucket.set_cap(cap),
        None => *guard = Some(Bucket::new(cap, REFILL_BYTES, REFILL_PERIOD, now)),
    }
}

/// What the player reported, reduced to what the prefetch decisions need.
#[derive(Debug, Clone)]
pub enum Ev {
    /// The queue changed. `current` is `None` when nothing is loaded.
    Queue {
        current: Option<String>,
        upcoming: Vec<String>,
    },
    /// Loading, playing, paused, position or seek: a track is loaded. `uri` is
    /// the track as the queue knows it.
    Playback {
        uri: String,
        ms: u64,
    },
    /// The player's own preload of this track started.
    Preloading(String),
    /// The player loaded a track. `uri` is the track actually played, which
    /// differs from the queued one when Spotify relinked it.
    Track {
        uri: String,
        len_ms: u64,
        file: Option<FileId>,
    },
    End(String),
    Stopped,
}

/// Facts the decision logic reads from the outside world.
pub struct Env<'a> {
    pub cache_on: bool,
    pub metered: bool,
    /// Whether a file is complete in the audio cache.
    pub cached: &'a dyn Fn(FileId) -> bool,
    /// For the download in flight: it finished, and the bytes it holds.
    pub flight_done: bool,
    pub flight_bytes: u64,
}

/// What the I/O shell has to do next.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Wait,
    /// Drop the download in flight (if any).
    Drop,
    /// The download in flight finished.
    Done,
    /// Resolve and start downloading this track.
    Start(String),
}

#[derive(Debug)]
struct Flight {
    uri: String,
    bytes: u64,
    progress_at: Instant,
}

/// The playing track as the player reported it.
#[derive(Debug)]
struct Loaded {
    uri: String,
    /// The queued uri it was played for, once a playback event named it.
    orig: Option<String>,
    len_ms: u64,
    file: Option<FileId>,
}

#[derive(Debug)]
struct Suspension {
    until: Instant,
    /// The current track when it started; prefetching waits for another one.
    track: Option<String>,
}

/// The prefetch decisions, with no I/O and no clock: events and `Instant`s go
/// in, `Action`s come out.
#[derive(Debug)]
pub struct Core {
    pub window: Window,
    /// Failed for now (transient), until `FAILED_TTL` passes.
    failed: HashMap<String, Instant>,
    /// Failed for good this session: never retried.
    permanent: HashSet<String>,
    /// Files of tracks seen so far, to tell whether one is cached.
    files: HashMap<String, FileId>,
    loaded: Option<Loaded>,
    /// A TrackChanged arrived and the next playback event names its queued uri.
    await_orig: bool,
    /// A track is loaded and the player is not stopped.
    active: bool,
    active_uri: Option<String>,
    /// Tracks the player is preloading itself; counted as cached until the
    /// current track changes.
    preloaded: HashSet<String>,
    flight: Option<Flight>,
    /// Do nothing until then (after a queue change, or while blocked).
    hold_until: Instant,
    suspended: Option<Suspension>,
}

impl Core {
    pub fn new(now: Instant) -> Self {
        Self {
            window: Window::default(),
            failed: HashMap::new(),
            permanent: HashSet::new(),
            files: HashMap::new(),
            loaded: None,
            await_orig: false,
            active: false,
            active_uri: None,
            preloaded: HashSet::new(),
            flight: None,
            hold_until: now,
            suspended: None,
        }
    }

    pub fn on_event(&mut self, ev: Ev, now: Instant) {
        match ev {
            Ev::Queue { current: None, .. } => {
                self.active = false;
                self.active_uri = None;
            }
            Ev::Queue {
                current: Some(current),
                upcoming,
            } => {
                let changed = self.window.current() != Some(current.as_str());
                let before = self.window.size;
                self.window.update(&current, &upcoming);
                if changed {
                    self.preloaded.clear();
                }
                self.hold_until = now + SETTLE;
                if self.window.size != before {
                    log::debug!("prefetch: window {before} -> {}", self.window.size);
                }
            }
            Ev::Stopped => self.active = false,
            Ev::End(uri) => {
                // A gapless successor may already be playing.
                if self.active_uri.as_deref() == Some(uri.as_str()) {
                    self.active = false;
                }
            }
            Ev::Playback { uri, ms } => {
                self.active = true;
                if self.await_orig {
                    self.await_orig = false;
                    self.bind_orig(&uri);
                }
                self.window.played(&uri, ms);
                self.active_uri = Some(uri);
            }
            Ev::Track { uri, len_ms, file } => {
                self.window.duration(&uri, len_ms);
                if let Some(id) = file {
                    self.files.insert(uri.clone(), id);
                }
                self.loaded = Some(Loaded {
                    uri,
                    orig: None,
                    len_ms,
                    file,
                });
                self.await_orig = true;
            }
            Ev::Preloading(uri) => {
                self.preloaded.insert(uri);
            }
        }
    }

    /// The track queued as `orig` is the one just loaded (it may have been
    /// relinked to another uri).
    fn bind_orig(&mut self, orig: &str) {
        let Some(loaded) = &mut self.loaded else {
            return;
        };
        loaded.orig = Some(orig.to_owned());
        if loaded.uri != orig {
            self.window.duration(orig, loaded.len_ms);
            if let Some(id) = loaded.file {
                self.files.insert(orig.to_owned(), id);
            }
        }
    }

    /// Whether the download in flight is pointless now (its track is playing,
    /// left the window, or the player is preloading it). Clears it if so.
    pub fn cancel_stale(&mut self) -> bool {
        let stale = self.flight.as_ref().is_some_and(|f| {
            self.window.current() == Some(f.uri.as_str())
                || !self.window.in_window(&f.uri)
                || self.preloaded.contains(&f.uri)
        });
        if stale {
            self.flight = None;
        }
        stale
    }

    fn drop_flight(&mut self) -> Action {
        if self.flight.take().is_some() {
            Action::Drop
        } else {
            Action::Wait
        }
    }

    /// What to do now. Called about once a second and again after a `Start`
    /// that did not begin a download.
    pub fn tick(&mut self, env: &Env, now: Instant) -> Action {
        if !env.cache_on || env.metered || !self.active {
            return self.drop_flight();
        }
        if self.cancel_stale() {
            return Action::Drop;
        }
        if let Some(mut f) = self.flight.take() {
            if env.flight_done {
                return Action::Done;
            }
            if env.flight_bytes > f.bytes {
                f.bytes = env.flight_bytes;
                f.progress_at = now;
            } else if now.saturating_duration_since(f.progress_at) >= STALL_TIMEOUT {
                // A slow link that still moves is left alone; a stalled one
                // would restart the same download forever, so stop for a while.
                self.failed(&f.uri, false, now);
                self.suspended = Some(Suspension {
                    until: now + SUSPEND,
                    track: self.window.current().map(str::to_owned),
                });
                log::debug!("prefetch: stalled on {}, suspended", f.uri);
                return Action::Drop;
            }
            self.flight = Some(f);
            return Action::Wait;
        }
        if let Some(s) = &self.suspended {
            if now >= s.until && self.window.current() != s.track.as_deref() {
                self.suspended = None;
            } else {
                return Action::Wait;
            }
        }
        if now < self.hold_until || !self.current_cached(env) {
            return Action::Wait;
        }
        let cached = |uri: &str| {
            self.permanent.contains(uri)
                || self.preloaded.contains(uri)
                || self.files.get(uri).is_some_and(|id| (env.cached)(*id))
        };
        match self.window.next(cached, &self.failed, now) {
            Some((_, uri)) => Action::Start(uri),
            None => Action::Wait,
        }
    }

    /// Priority gate: the current track's file must be in the cache first.
    fn current_cached(&self, env: &Env) -> bool {
        let Some(current) = self.window.current() else {
            return false;
        };
        if !is_track(current) {
            return true;
        }
        match &self.loaded {
            Some(l) if l.uri == current || l.orig.as_deref() == Some(current) => {
                l.file.is_none_or(|id| (env.cached)(id))
            }
            _ => false, // its TrackChanged has not arrived yet
        }
    }

    /// The shell began downloading `uri`.
    pub fn started(&mut self, uri: &str, now: Instant) {
        self.flight = Some(Flight {
            uri: uri.to_owned(),
            bytes: 0,
            progress_at: now,
        });
    }

    /// The file `uri` would play is known.
    pub fn resolved(&mut self, uri: &str, file: FileId) {
        self.files.insert(uri.to_owned(), file);
    }

    /// `uri` could not be prefetched: for now, or `permanent` for the session.
    pub fn failed(&mut self, uri: &str, permanent: bool, now: Instant) {
        if permanent {
            self.permanent.insert(uri.to_owned());
        } else {
            self.failed.insert(uri.to_owned(), now);
            // The map only ever needs the last minute.
            self.failed
                .retain(|_, at| now.saturating_duration_since(*at) < FAILED_TTL);
        }
    }

    /// Blocked (budget empty): look again later.
    pub fn blocked(&mut self, now: Instant) {
        self.hold_until = now + RETRY;
    }
}

fn is_track(uri: &str) -> bool {
    uri.starts_with("spotify:track:")
}

/// How long resolving a track's metadata or opening its download may take.
const OPEN_TIMEOUT: Duration = Duration::from_secs(15);

/// A download in progress. Dropping it cancels the download.
struct InFlight {
    id: SpotifyId,
    uri: String,
    file_id: FileId,
    path: PathBuf,
    _file: AudioFile,
    ctl: StreamLoaderController,
}

/// The I/O shell around `Core`.
struct Prefetcher {
    session: Session,
    bitrate: Bitrate,
    metered: Arc<AtomicBool>,
    bucket: SharedBucket,
    core: Core,
    inflight: Option<InFlight>,
}

/// Follows the player and keeps the audio cache filled with the tracks about
/// to play. Runs until the event channel closes. `cache_limit` is the audio
/// cache size in bytes; `bucket` outlives this task so a restart of the engine
/// does not refill it.
pub async fn run(
    session: Session,
    mut events: UnboundedReceiver<PlayerEvent>,
    bitrate: Bitrate,
    metered: Arc<AtomicBool>,
    bucket: SharedBucket,
    cache_limit: u64,
) {
    let now = Instant::now();
    init_bucket(&bucket, bucket_cap(cache_limit), now);
    let mut p = Prefetcher {
        session,
        bitrate,
        metered,
        bucket,
        core: Core::new(now),
        inflight: None,
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

/// The player event as the decision logic sees it, `None` if it is irrelevant.
fn convert(event: PlayerEvent, bitrate: Bitrate) -> Option<Ev> {
    Some(match event {
        PlayerEvent::SetQueue {
            current_track,
            next_tracks,
            ..
        } => Ev::Queue {
            current: current_track.map(|t| t.uri),
            upcoming: next_tracks.into_iter().map(|t| t.uri).collect(),
        },
        PlayerEvent::Stopped { .. } => Ev::Stopped,
        PlayerEvent::EndOfTrack { track_id, .. } => Ev::End(track_id.to_uri()),
        PlayerEvent::Preloading { track_id } => Ev::Preloading(track_id.to_uri()),
        PlayerEvent::Loading {
            track_id,
            position_ms,
            ..
        }
        | PlayerEvent::PositionChanged {
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
        } => Ev::Playback {
            uri: track_id.to_uri(),
            ms: u64::from(position_ms),
        },
        PlayerEvent::TrackChanged { audio_item } => Ev::Track {
            file: select_file(&audio_item.files, bitrate).map(|(_, id)| id),
            len_ms: u64::from(audio_item.duration_ms),
            uri: audio_item.uri,
        },
        _ => return None,
    })
}

fn cache_path(session: &Session, file: FileId) -> Option<PathBuf> {
    session.cache()?.file_path(file)
}

fn file_cached(session: &Session, file: FileId) -> bool {
    cache_path(session, file).is_some_and(|p| p.exists())
}

impl Prefetcher {
    fn on_event(&mut self, event: PlayerEvent) {
        let Some(ev) = convert(event, self.bitrate) else {
            return;
        };
        self.core.on_event(ev, Instant::now());
        if self.core.cancel_stale() {
            self.drop_inflight("track is current, left the window or is preloading");
        }
    }

    fn drop_inflight(&mut self, why: &str) {
        if let Some(f) = self.inflight.take() {
            log::debug!("prefetch: cancel {} ({why})", f.uri);
        }
    }

    fn cache_path(&self, file: FileId) -> Option<PathBuf> {
        cache_path(&self.session, file)
    }

    fn cached(&self, file: FileId) -> bool {
        file_cached(&self.session, file)
    }

    async fn step(&mut self) {
        // One resolve per window slot per step; failed and cached tracks are
        // skipped by the core.
        for _ in 0..=MAX {
            let now = Instant::now();
            let (flight_done, flight_bytes) = self.inflight.as_ref().map_or((false, 0), |f| {
                (f.path.exists(), f.ctl.downloaded_len() as u64)
            });
            let action = {
                let session = &self.session;
                let cached = |id: FileId| file_cached(session, id);
                let env = Env {
                    cache_on: cache_path(session, FileId::from_raw(&[0; 20])).is_some(),
                    metered: self.metered.load(Ordering::Relaxed),
                    cached: &cached,
                    flight_done,
                    flight_bytes,
                };
                self.core.tick(&env, now)
            };
            match action {
                Action::Wait => return,
                Action::Drop => return self.drop_inflight("off, metered, stalled or stale"),
                Action::Done => {
                    let Some(f) = self.inflight.take() else {
                        return;
                    };
                    log::debug!("prefetch: done {}", f.uri);
                    // Warm the audio key so the track starts without a round trip.
                    let session = self.session.clone();
                    tokio::spawn(async move {
                        if let Err(e) = session.audio_key().request(f.id, f.file_id).await {
                            log::debug!("prefetch: key for {} failed: {e}", f.uri);
                        }
                    });
                    return;
                }
                Action::Start(uri) => {
                    if !self.start(&uri, now).await {
                        return;
                    }
                }
            }
        }
    }

    fn fail(&mut self, uri: &str, why: &str, permanent: bool, now: Instant) {
        log::debug!("prefetch: failed {uri} ({why})");
        self.core.failed(uri, permanent, now);
    }

    /// Resolves and starts downloading `uri`. Returns whether to try the next
    /// candidate now (`false`: started, or blocked until later).
    async fn start(&mut self, uri: &str, now: Instant) -> bool {
        let Ok(SpotifyUri::Track { id }) = SpotifyUri::from_uri(uri) else {
            self.fail(uri, "not a track", true, now);
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
                self.fail(uri, &format!("metadata: {e}"), false, now);
                return true;
            }
            Err(_) => {
                self.fail(uri, "metadata timeout", false, now);
                return true;
            }
        };
        if let Err(reason) = &item.availability {
            self.fail(uri, &format!("unavailable: {reason:?}"), true, now);
            return true;
        }
        if u64::from(item.duration_ms) > MAX_TRACK_S * 1000 {
            self.fail(uri, "longer than 420 s", true, now);
            return true;
        }
        let Some((format, file_id)) = select_file(&item.files, self.bitrate) else {
            self.fail(uri, "no playable file", true, now);
            return true;
        };
        self.core.resolved(uri, file_id);
        if self.cached(file_id) {
            return true; // the core skips it now
        }
        let rate = stream_data_rate(format).unwrap_or(40 * 1024);
        let need = u64::from(item.duration_ms) * rate as u64 / 1000;
        let budget = {
            let mut guard = self.bucket.lock().unwrap_or_else(PoisonError::into_inner);
            let bucket = guard.get_or_insert_with(|| {
                Bucket::new(BUCKET_CAP_MAX, REFILL_BYTES, REFILL_PERIOD, now)
            });
            bucket.fits(need).then(|| bucket.take(need, now))
        };
        match budget {
            None => {
                self.fail(uri, "larger than the budget", true, now);
                return true;
            }
            Some(false) => {
                log::debug!("prefetch: budget empty, retry in {RETRY:?}");
                self.core.blocked(now);
                return false;
            }
            Some(true) => {}
        }
        let opened = timeout(OPEN_TIMEOUT, AudioFile::open(&self.session, file_id, rate)).await;
        let file = match opened {
            Ok(Ok(file)) => file,
            Ok(Err(e)) => {
                self.fail(uri, &format!("open: {e}"), false, now);
                return true;
            }
            Err(_) => {
                self.fail(uri, "open timeout", false, now);
                return true;
            }
        };
        let (Some(path), Ok(ctl)) = (
            self.cache_path(file_id),
            file.get_stream_loader_controller(),
        ) else {
            self.fail(uri, "no cache path or controller", false, now);
            return true;
        };
        // Without this only the first 64 KiB is fetched.
        ctl.set_stream_mode();
        log::debug!("prefetch: start {uri} ({need} bytes)");
        self.core.started(uri, now);
        self.inflight = Some(InFlight {
            id,
            uri: uri.to_owned(),
            file_id,
            path,
            _file: file,
            ctl,
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

    // ---- Core: the decision state machine, driven by fake events and Instants.

    fn fid(n: u8) -> FileId {
        FileId::from_raw(&[n; 20])
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    /// File ids in `done` are complete in the cache.
    fn env<'a>(done: &'a dyn Fn(FileId) -> bool, flight_done: bool, flight_bytes: u64) -> Env<'a> {
        Env {
            cache_on: true,
            metered: false,
            cached: done,
            flight_done,
            flight_bytes,
        }
    }

    fn queue(core: &mut Core, cur: &str, upcoming: &[&str], now: Instant) {
        core.on_event(
            Ev::Queue {
                current: Some(uri(cur)),
                upcoming: uris(upcoming),
            },
            now,
        );
    }

    fn track(core: &mut Core, played: &str, orig: &str, file: u8, now: Instant) {
        core.on_event(
            Ev::Track {
                uri: uri(played),
                len_ms: 200_000,
                file: Some(fid(file)),
            },
            now,
        );
        playback(core, orig, now);
    }

    fn playback(core: &mut Core, name: &str, now: Instant) {
        core.on_event(
            Ev::Playback {
                uri: uri(name),
                ms: 1_000,
            },
            now,
        );
    }

    /// Playing "a" (file 1, fully cached) with b, c, d queued and a window of 3,
    /// at the moment the settle delay has passed. Returns the core and that time.
    fn playing() -> (Core, Instant) {
        let t0 = Instant::now();
        let mut core = Core::new(t0);
        queue(&mut core, "a", &["b", "c", "d"], t0);
        core.window.size = 3;
        track(&mut core, "a", "a", 1, t0);
        (core, t0 + SETTLE)
    }

    const A_CACHED: &dyn Fn(FileId) -> bool = &|id| id == FileId::from_raw(&[1; 20]);

    #[test]
    fn gate_opens_once_the_current_track_is_cached() {
        let (mut core, t) = playing();
        assert_eq!(core.tick(&env(&|_| false, false, 0), t), Action::Wait);
        assert_eq!(
            core.tick(&env(A_CACHED, false, 0), t),
            Action::Start(uri("b"))
        );
    }

    #[test]
    fn settles_after_a_queue_change() {
        let (mut core, t) = playing();
        assert_eq!(
            core.tick(&env(A_CACHED, false, 0), t - Duration::from_millis(1)),
            Action::Wait
        );
    }

    #[test]
    fn cancels_when_the_track_becomes_current_or_leaves_the_window() {
        let (mut core, t) = playing();
        core.started(&uri("b"), t);
        assert!(!core.cancel_stale());
        // The queue moves on: b is now playing.
        queue(&mut core, "b", &["c", "d", "e"], t);
        assert!(core.cancel_stale());
        assert!(!core.cancel_stale()); // already cleared

        // Window shrinks below the download's slot.
        let (mut core, t) = playing();
        core.started(&uri("d"), t);
        core.window.size = 1;
        assert_eq!(core.tick(&env(A_CACHED, false, 0), t), Action::Drop);
    }

    #[test]
    fn metered_or_cache_off_drops_the_download() {
        let (mut core, t) = playing();
        core.started(&uri("b"), t);
        let mut e = env(A_CACHED, false, 0);
        e.metered = true;
        assert_eq!(core.tick(&e, t), Action::Drop);
        assert_eq!(core.tick(&e, t), Action::Wait);
        core.started(&uri("b"), t);
        let mut e = env(A_CACHED, false, 0);
        e.cache_on = false;
        assert_eq!(core.tick(&e, t), Action::Drop);
    }

    #[test]
    fn slow_but_moving_download_is_never_dropped() {
        let (mut core, t) = playing();
        core.started(&uri("b"), t);
        // 10 minutes at 80 KB/s: far past the old fixed 120 s limit.
        for s in (1..600).step_by(5) {
            let bytes = s * 80_000;
            assert_eq!(
                core.tick(&env(A_CACHED, false, bytes), t + secs(s)),
                Action::Wait,
                "second {s}"
            );
        }
        assert_eq!(
            core.tick(&env(A_CACHED, true, 1), t + secs(600)),
            Action::Done
        );
    }

    #[test]
    fn stalled_download_is_dropped_after_30s_without_bytes() {
        let (mut core, t) = playing();
        core.started(&uri("b"), t);
        assert_eq!(
            core.tick(&env(A_CACHED, false, 500), t + secs(10)),
            Action::Wait
        );
        // Last bytes arrived at +10 s.
        assert_eq!(
            core.tick(&env(A_CACHED, false, 500), t + secs(39)),
            Action::Wait
        );
        assert_eq!(
            core.tick(&env(A_CACHED, false, 500), t + secs(40)),
            Action::Drop
        );
    }

    #[test]
    fn stall_suspends_prefetching_until_track_changes_and_10_minutes() {
        let (mut core, t) = playing();
        core.started(&uri("b"), t);
        let stalled = t + STALL_TIMEOUT;
        assert_eq!(core.tick(&env(A_CACHED, false, 0), stalled), Action::Drop);

        // Same track, even after the 60 s failure expiry and past 10 minutes:
        // still suspended.
        for s in [1, 61, 700, 5000] {
            assert_eq!(
                core.tick(&env(A_CACHED, false, 0), stalled + secs(s)),
                Action::Wait,
                "+{s}s"
            );
        }

        // A new track within the 10 minutes: still suspended.
        let (mut core, t) = playing();
        core.started(&uri("b"), t);
        let stalled = t + STALL_TIMEOUT;
        assert_eq!(core.tick(&env(A_CACHED, false, 0), stalled), Action::Drop);
        let t2 = stalled + secs(60);
        queue(&mut core, "b", &["c", "d", "e"], t2);
        track(&mut core, "b", "b", 2, t2);
        let all = |_: FileId| true;
        assert_eq!(core.tick(&env(&all, false, 0), t2 + SETTLE), Action::Wait);

        // Track changed and 10 minutes passed: prefetching resumes.
        let t3 = stalled + SUSPEND;
        let b_cached = |id: FileId| id == fid(2);
        assert_eq!(
            core.tick(&env(&b_cached, false, 0), t3),
            Action::Start(uri("c"))
        );
    }

    #[test]
    fn ten_minutes_alone_do_not_lift_the_suspension() {
        let (mut core, t) = playing();
        core.started(&uri("b"), t);
        let stalled = t + STALL_TIMEOUT;
        core.tick(&env(A_CACHED, false, 0), stalled);
        // Track changes right away, but before the 10 minutes are over.
        queue(&mut core, "b", &["c", "d", "e"], stalled);
        track(&mut core, "b", "b", 2, stalled);
        let b_cached = |id: FileId| id == fid(2);
        assert_eq!(
            core.tick(&env(&b_cached, false, 0), stalled + SUSPEND - secs(1)),
            Action::Wait
        );
        assert_eq!(
            core.tick(&env(&b_cached, false, 0), stalled + SUSPEND),
            Action::Start(uri("c"))
        );
    }

    #[test]
    fn preloading_drops_the_download_and_counts_as_cached() {
        let (mut core, t) = playing();
        core.started(&uri("b"), t);
        core.on_event(Ev::Preloading(uri("b")), t);
        assert!(core.cancel_stale());
        // b is skipped for the next candidate: c.
        assert_eq!(
            core.tick(&env(A_CACHED, false, 0), t),
            Action::Start(uri("c"))
        );

        // Another uri's preload does not touch a download of b.
        let (mut core, t) = playing();
        core.started(&uri("b"), t);
        core.on_event(Ev::Preloading(uri("c")), t);
        assert!(!core.cancel_stale());
    }

    #[test]
    fn preloaded_marker_ends_when_the_current_track_changes() {
        let (mut core, t) = playing();
        core.window.size = 1;
        core.on_event(Ev::Preloading(uri("b")), t);
        // b counts as cached, and the window of 1 has no other slot.
        assert_eq!(core.tick(&env(A_CACHED, false, 0), t), Action::Wait);
        // b becomes current, then a new upcoming list: preload marker is gone,
        // so a later b would be fetched again.
        queue(&mut core, "b", &["c", "d", "e"], t);
        track(&mut core, "b", "b", 2, t);
        queue(&mut core, "x", &["b", "c", "d"], t);
        track(&mut core, "x", "x", 3, t);
        let x_cached = |id: FileId| id == fid(3);
        assert_eq!(
            core.tick(&env(&x_cached, false, 0), t + SETTLE),
            Action::Start(uri("b"))
        );
    }

    #[test]
    fn bucket_survives_a_restart() {
        let t0 = Instant::now();
        let shared = SharedBucket::default();
        init_bucket(&shared, 100, t0);
        assert!(shared.lock().unwrap().as_mut().unwrap().take(100, t0));
        // The engine restarts: a new prefetcher initialises the same bucket.
        init_bucket(&shared, 100, t0);
        assert!(!shared.lock().unwrap().as_mut().unwrap().take(50, t0));
        // A smaller cache limit shrinks it without refilling.
        init_bucket(&shared, 10, t0);
        assert!(!shared.lock().unwrap().as_mut().unwrap().take(50, t0));
    }

    #[test]
    fn relinked_track_opens_the_gate() {
        let t0 = Instant::now();
        let t = t0 + SETTLE;
        // Not bound yet: no playback event named the queued uri.
        let mut unbound = Core::new(t0);
        queue(&mut unbound, "orig", &["b", "c", "d"], t0);
        unbound.on_event(
            Ev::Track {
                uri: uri("alt"),
                len_ms: 200_000,
                file: Some(fid(1)),
            },
            t0,
        );
        assert_eq!(unbound.tick(&env(A_CACHED, false, 0), t), Action::Wait);

        let mut core = Core::new(t0);
        queue(&mut core, "orig", &["b", "c", "d"], t0);
        track(&mut core, "alt", "orig", 1, t0);
        assert_eq!(
            core.tick(&env(A_CACHED, false, 0), t),
            Action::Start(uri("b"))
        );
        // Still waits for the file of the relinked track.
        assert_eq!(core.tick(&env(&|_| false, false, 0), t), Action::Wait);
        // Its duration reached the queued uri, so the window grows on a full play.
        core.on_event(
            Ev::Playback {
                uri: uri("orig"),
                ms: 190_000,
            },
            t,
        );
        queue(&mut core, "b", &["c", "d", "e"], t);
        assert_eq!(core.window.size, 2);
    }

    #[test]
    fn permanent_failures_are_not_retried_but_transient_ones_are() {
        let (mut core, t) = playing();
        core.failed(&uri("b"), true, t);
        core.failed(&uri("c"), false, t);
        let e = env(A_CACHED, false, 0);
        // c is blocked for 60 s, b for the whole session.
        assert_eq!(core.tick(&e, t), Action::Start(uri("d")));
        core.failed(&uri("d"), true, t);
        assert_eq!(core.tick(&e, t + FAILED_TTL - secs(1)), Action::Wait);
        assert_eq!(core.tick(&e, t + FAILED_TTL), Action::Start(uri("c")));
        assert_eq!(core.tick(&e, t + secs(24 * 3600)), Action::Start(uri("c")));
        core.failed(&uri("c"), true, t + secs(24 * 3600));
        assert_eq!(core.tick(&e, t + secs(24 * 3600)), Action::Wait);
    }

    #[test]
    fn queue_while_stopped_prefetches_nothing() {
        let t0 = Instant::now();
        let mut core = Core::new(t0);
        queue(&mut core, "a", &["b", "c", "d"], t0);
        core.on_event(
            Ev::Track {
                uri: uri("a"),
                len_ms: 1,
                file: None,
            },
            t0,
        );
        // Nothing is playing: not even a track with no file to wait for starts.
        assert_eq!(
            core.tick(&env(&|_| false, false, 0), t0 + SETTLE),
            Action::Wait
        );
        // Playback begins.
        playback(&mut core, "a", t0);
        assert_eq!(
            core.tick(&env(&|_| false, false, 0), t0 + SETTLE),
            Action::Start(uri("b"))
        );
    }

    #[test]
    fn stop_drops_the_download_and_goes_idle() {
        let (mut core, t) = playing();
        core.started(&uri("b"), t);
        core.on_event(Ev::Stopped, t);
        assert_eq!(core.tick(&env(A_CACHED, false, 0), t), Action::Drop);
        assert_eq!(core.tick(&env(A_CACHED, false, 0), t), Action::Wait);
        // Playing again resumes.
        playback(&mut core, "a", t);
        assert_eq!(
            core.tick(&env(A_CACHED, false, 0), t),
            Action::Start(uri("b"))
        );
    }

    #[test]
    fn paused_with_a_loaded_track_still_prefetches() {
        let (mut core, t) = playing();
        playback(&mut core, "a", t); // what Paused reports
        assert_eq!(
            core.tick(&env(A_CACHED, false, 0), t),
            Action::Start(uri("b"))
        );
    }

    #[test]
    fn end_of_track_with_no_next_goes_idle() {
        let (mut core, t) = playing();
        core.started(&uri("b"), t);
        core.on_event(Ev::End(uri("a")), t);
        assert_eq!(core.tick(&env(A_CACHED, false, 0), t), Action::Drop);

        // A gapless successor that already plays is not stopped by the old end event.
        let (mut core, t) = playing();
        playback(&mut core, "b", t);
        core.on_event(Ev::End(uri("a")), t);
        assert!(core.active);
    }

    #[test]
    fn no_current_track_goes_idle() {
        let (mut core, t) = playing();
        core.on_event(
            Ev::Queue {
                current: None,
                upcoming: vec![],
            },
            t,
        );
        assert_eq!(core.tick(&env(A_CACHED, false, 0), t), Action::Wait);
    }
}
