//! The DJ's tools: what the model may ask Spotifast to do, and the checks at that trust boundary.
//!
//! The model's arguments are untrusted input. Every one is parsed here into a [`Call`] before the
//! app sees it: a URI has to be a Spotify track, album, playlist or artist URI with a real-looking
//! id, a list is capped, a limit is clamped. The app then maps a `Call` to what it already does for
//! a click (search, queue, play, skip); nothing here edits the library.

use serde_json::{Value, json};

/// The most songs one `queue_tracks` call may add.
pub const MAX_QUEUE: usize = 20;
/// Search result rows per kind: the default and the ceiling.
pub const SEARCH_DEFAULT: usize = 8;
pub const SEARCH_MAX: usize = 10;
/// Recent plays returned: the default and the ceiling.
pub const RECENT_DEFAULT: usize = 20;
pub const RECENT_MAX: usize = 50;
/// The longest search query passed on.
const MAX_QUERY: usize = 200;
/// Rows of a playlist, album or liked list per call: the default and the ceiling.
pub const ROWS_DEFAULT: usize = 50;
pub const ROWS_MAX: usize = 100;
/// Albums of an artist, songs in `track_info`, top items.
pub const ALBUMS_MAX: usize = 20;
pub const INFO_MAX: usize = 20;
pub const TOP_MAX: usize = 20;
/// Library writes: songs liked at once, songs added to a playlist or a new one at once.
pub const LIKE_MAX: usize = 50;
pub const PLAYLIST_MAX: usize = 100;
/// Text the model supplies for the library or its memory.
const NAME_MAX: usize = 100;
const DESCRIPTION_MAX: usize = 300;
const NOTE_MAX: usize = 200;
const TASK_MAX: usize = 500;

/// One tool as the MCP server lists it.
pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: Value,
}

/// A kind of thing a search can return.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Track,
    Artist,
    Album,
    Playlist,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Track => "track",
            Kind::Artist => "artist",
            Kind::Album => "album",
            Kind::Playlist => "playlist",
        }
    }

    fn parse(s: &str) -> Option<Kind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "track" | "tracks" | "song" | "songs" => Some(Kind::Track),
            "artist" | "artists" => Some(Kind::Artist),
            "album" | "albums" => Some(Kind::Album),
            "playlist" | "playlists" => Some(Kind::Playlist),
            _ => None,
        }
    }
}

/// What the app answers a call with: the text the model reads, and a line for the DJ page's log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolReply {
    pub text: String,
    pub log: Option<String>,
    /// What the page draws under the log line.
    pub card: Option<super::Card>,
}

/// What `top_items` lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopKind {
    Artists,
    Tracks,
}

/// Spotify's three listening windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopRange {
    Short,
    Medium,
    Long,
}

impl TopRange {
    /// The Web API's name for it.
    pub fn api(self) -> &'static str {
        match self {
            TopRange::Short => "short_term",
            TopRange::Medium => "medium_term",
            TopRange::Long => "long_term",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepeatKind {
    Off,
    Track,
    Context,
}

/// How often a scheduled task runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cadence {
    Once,
    Daily,
    Weekdays,
    Weekly,
}

/// When a scheduled task runs, in the listener's local time.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Slot {
    pub cadence: Cadence,
    /// Minutes after local midnight.
    pub minute: u16,
    /// 0 = Monday, for `Weekly`.
    pub weekday: Option<u8>,
    /// `YYYY-MM-DD`, for `Once`.
    pub date: Option<String>,
}

/// A validated tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    Search {
        query: String,
        kinds: Vec<Kind>,
        limit: usize,
    },
    NowPlaying,
    Queue,
    RecentPlays {
        limit: usize,
    },
    LibrarySummary,
    QueueTracks {
        uris: Vec<String>,
    },
    Play {
        uri: String,
    },
    Skip,
    // reads of Spotify's catalogue and the listener's library
    PlaylistTracks {
        uri: String,
        offset: u32,
        limit: usize,
    },
    AlbumTracks {
        uri: String,
    },
    ArtistTopTracks {
        uri: String,
    },
    ArtistAlbums {
        uri: String,
        limit: usize,
    },
    LikedTracks {
        offset: u32,
        limit: usize,
    },
    TrackInfo {
        uris: Vec<String>,
    },
    TopItems {
        kind: TopKind,
        range: TopRange,
        limit: usize,
    },
    // reads of the app's own state
    PlaybackState,
    Now,
    Devices,
    // playback and queue
    Pause,
    Resume,
    Previous,
    Seek {
        seconds: u32,
    },
    SetVolume {
        percent: u8,
    },
    SetShuffle {
        on: bool,
    },
    SetRepeat {
        mode: RepeatKind,
    },
    ClearQueue,
    RemoveFromQueue {
        uris: Vec<String>,
    },
    TransferPlayback {
        device_id: String,
    },
    // changes to the listener's library: these wait for the listener's approval
    LikeTracks {
        uris: Vec<String>,
    },
    UnlikeTracks {
        uris: Vec<String>,
    },
    CreatePlaylist {
        name: String,
        description: String,
        uris: Vec<String>,
        public: bool,
    },
    AddToPlaylist {
        uri: String,
        uris: Vec<String>,
    },
    RemoveFromPlaylist {
        uri: String,
        uris: Vec<String>,
    },
    RenamePlaylist {
        uri: String,
        name: Option<String>,
        description: Option<String>,
    },
    // what the DJ keeps between conversations
    Remember {
        note: String,
    },
    Forget {
        note: String,
    },
    Memory,
    ScheduleTask {
        slot: Slot,
        prompt: String,
    },
    ListSchedules,
    CancelSchedule {
        id: u32,
    },
}

impl Call {
    /// Whether the call changes the listener's library or playlists, or leaves something that
    /// steers later conversations (a note, a scheduled task). With "Ask before changing my library" on, the app asks first
    /// (`crate::app::dj`).
    pub fn needs_approval(&self) -> bool {
        matches!(
            self,
            Call::LikeTracks { .. }
                | Call::UnlikeTracks { .. }
                | Call::CreatePlaylist { .. }
                | Call::AddToPlaylist { .. }
                | Call::RemoveFromPlaylist { .. }
                | Call::RenamePlaylist { .. }
                | Call::ScheduleTask { .. }
                | Call::Remember { .. }
        )
    }
}

/// The tools, in the order the model sees them.
pub fn registry() -> Vec<Tool> {
    let uri = |what: &str| json!({ "type": "string", "description": what });
    vec![
        Tool {
            name: "search",
            description: "Search Spotify's catalogue. Returns one line per result with its URI. \
                          Use it to find real URIs; never invent one.",
            schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "What to search for, e.g. an artist, a song title or both." },
                    "types": {
                        "type": "array",
                        "items": { "type": "string", "enum": ["track", "artist", "album", "playlist"] },
                        "description": "Kinds of result to return. Default: track."
                    },
                    "limit": { "type": "integer", "minimum": 1, "maximum": SEARCH_MAX, "description": "Results per kind (default 8)." }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "now_playing",
            description: "The song playing right now (title, artists, album, position, URI), or that nothing is playing.",
            schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        },
        Tool {
            name: "queue",
            description: "The song playing now and what is queued after it, in order.",
            schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        },
        Tool {
            name: "recent_plays",
            description: "What the listener played recently, newest first. Use it to avoid repeating songs.",
            schema: json!({
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "minimum": 1, "maximum": RECENT_MAX, "description": "How many plays (default 20)." }
                },
                "additionalProperties": false
            }),
        },
        Tool {
            name: "library_summary",
            description: "The listener's taste as far as it is loaded: top artists, top songs and saved playlists, with URIs.",
            schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        },
        Tool {
            name: "queue_tracks",
            description: "Add songs to the end of the listener's queue, in order. Songs only (spotify:track: URIs), at most 20 per call.",
            schema: json!({
                "type": "object",
                "properties": {
                    "uris": {
                        "type": "array",
                        "items": uri("A spotify:track:<id> URI from search, queue or recent_plays."),
                        "minItems": 1,
                        "maxItems": MAX_QUEUE
                    }
                },
                "required": ["uris"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "play",
            description: "Start playing a song, album, playlist or artist now. This replaces what is playing.",
            schema: json!({
                "type": "object",
                "properties": {
                    "uri": uri("A spotify:track:, spotify:album:, spotify:playlist: or spotify:artist: URI.")
                },
                "required": ["uri"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "skip",
            description: "Skip to the next song.",
            schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        },
        Tool {
            name: "playlist_tracks",
            description: "The songs of a playlist (the listener's own or any public one), with its name, \
                          owner and size. Use offset to read further.",
            schema: object(
                json!({
                    "uri": uri("A spotify:playlist:<id> URI from search or library_summary."),
                    "offset": { "type": "integer", "minimum": 0, "description": "Rows to skip (default 0)." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": ROWS_MAX, "description": "Rows (default 50)." }
                }),
                &["uri"],
            ),
        },
        Tool {
            name: "album_tracks",
            description: "The songs of an album, in order.",
            schema: object(json!({ "uri": uri("A spotify:album:<id> URI.") }), &["uri"]),
        },
        Tool {
            name: "artist_top_tracks",
            description: "An artist's most played songs.",
            schema: object(
                json!({ "uri": uri("A spotify:artist:<id> URI.") }),
                &["uri"],
            ),
        },
        Tool {
            name: "artist_albums",
            description: "An artist's albums and singles, newest first.",
            schema: object(
                json!({
                    "uri": uri("A spotify:artist:<id> URI."),
                    "limit": { "type": "integer", "minimum": 1, "maximum": ALBUMS_MAX, "description": "How many (default 10)." }
                }),
                &["uri"],
            ),
        },
        Tool {
            name: "liked_tracks",
            description: "Songs the listener saved in Liked Songs, most recently liked first. Use offset to \
                          read further back, or to sample across a large library.",
            schema: object(
                json!({
                    "offset": { "type": "integer", "minimum": 0, "description": "Rows to skip (default 0)." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": ROWS_MAX.min(50), "description": "Rows (default 50)." }
                }),
                &[],
            ),
        },
        Tool {
            name: "track_info",
            description: "Details of songs: duration, release date, popularity (0-100), explicit flag, album \
                          and artists.",
            schema: object(
                json!({
                    "uris": {
                        "type": "array",
                        "items": uri("A spotify:track:<id> URI."),
                        "minItems": 1,
                        "maxItems": INFO_MAX
                    }
                }),
                &["uris"],
            ),
        },
        Tool {
            name: "top_items",
            description: "What the listener plays most: top artists or top songs over the last four weeks, \
                          six months or all time.",
            schema: object(
                json!({
                    "kind": { "type": "string", "enum": ["artists", "tracks"] },
                    "range": { "type": "string", "enum": ["short", "medium", "long"], "description": "short = 4 weeks, medium = 6 months, long = all time (default medium)." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": TOP_MAX, "description": "How many (default 20)." }
                }),
                &["kind"],
            ),
        },
        Tool {
            name: "playback_state",
            description: "Whether music is playing or paused, shuffle, repeat, volume and the device.",
            schema: object(json!({}), &[]),
        },
        Tool {
            name: "now",
            description: "The listener's local date, time and weekday. Use it for time-of-day moods and for \
                          scheduling.",
            schema: object(json!({}), &[]),
        },
        Tool {
            name: "devices",
            description: "The Spotify devices the listener can play on, with their ids.",
            schema: object(json!({}), &[]),
        },
        Tool {
            name: "pause",
            description: "Pause playback.",
            schema: object(json!({}), &[]),
        },
        Tool {
            name: "resume",
            description: "Resume playback.",
            schema: object(json!({}), &[]),
        },
        Tool {
            name: "previous",
            description: "Go back to the previous song.",
            schema: object(json!({}), &[]),
        },
        Tool {
            name: "seek",
            description: "Jump to a position in the current song.",
            schema: object(
                json!({ "seconds": { "type": "integer", "minimum": 0, "description": "Seconds from the start." } }),
                &["seconds"],
            ),
        },
        Tool {
            name: "set_volume",
            description: "Set the volume, 0 to 100.",
            schema: object(
                json!({ "percent": { "type": "integer", "minimum": 0, "maximum": 100 } }),
                &["percent"],
            ),
        },
        Tool {
            name: "set_shuffle",
            description: "Turn shuffle on or off.",
            schema: object(json!({ "on": { "type": "boolean" } }), &["on"]),
        },
        Tool {
            name: "set_repeat",
            description: "Set repeat: off, track (the current song) or context (the playlist or album).",
            schema: object(
                json!({ "mode": { "type": "string", "enum": ["off", "track", "context"] } }),
                &["mode"],
            ),
        },
        Tool {
            name: "clear_queue",
            description: "Remove every song the listener queued by hand.",
            schema: object(json!({}), &[]),
        },
        Tool {
            name: "remove_from_queue",
            description: "Remove songs from the queue. Only songs the listener queued can be removed.",
            schema: object(
                json!({
                    "uris": {
                        "type": "array",
                        "items": uri("A spotify:track:<id> URI from queue."),
                        "minItems": 1,
                        "maxItems": MAX_QUEUE
                    }
                }),
                &["uris"],
            ),
        },
        Tool {
            name: "transfer_playback",
            description: "Move playback to another device, by an id from devices.",
            schema: object(
                json!({ "device_id": { "type": "string", "description": "A device id from devices." } }),
                &["device_id"],
            ),
        },
        Tool {
            name: "like_tracks",
            description: "Save songs to the listener's Liked Songs. The listener is asked to approve first; \
                          say what you want and why in your reply, not as a question.",
            schema: object(
                json!({
                    "uris": {
                        "type": "array",
                        "items": uri("A spotify:track:<id> URI."),
                        "minItems": 1,
                        "maxItems": LIKE_MAX
                    }
                }),
                &["uris"],
            ),
        },
        Tool {
            name: "unlike_tracks",
            description: "Remove songs from the listener's Liked Songs. The listener is asked to approve first.",
            schema: object(
                json!({
                    "uris": {
                        "type": "array",
                        "items": uri("A spotify:track:<id> URI."),
                        "minItems": 1,
                        "maxItems": LIKE_MAX
                    }
                }),
                &["uris"],
            ),
        },
        Tool {
            name: "create_playlist",
            description: "Create a playlist in the listener's library and fill it. Private unless public is \
                          true. The listener is asked to approve first.",
            schema: object(
                json!({
                    "name": { "type": "string", "maxLength": NAME_MAX },
                    "description": { "type": "string", "maxLength": DESCRIPTION_MAX },
                    "uris": {
                        "type": "array",
                        "items": uri("A spotify:track:<id> URI."),
                        "maxItems": PLAYLIST_MAX
                    },
                    "public": { "type": "boolean", "description": "Default false." }
                }),
                &["name"],
            ),
        },
        Tool {
            name: "add_to_playlist",
            description: "Add songs to the end of one of the listener's own playlists. It does not \
                          skip songs already there: read playlist_tracks first when that matters. The \
                          listener is asked to approve first.",
            schema: object(
                json!({
                    "uri": uri("A spotify:playlist:<id> URI of a playlist the listener owns."),
                    "uris": {
                        "type": "array",
                        "items": uri("A spotify:track:<id> URI."),
                        "minItems": 1,
                        "maxItems": PLAYLIST_MAX
                    }
                }),
                &["uri", "uris"],
            ),
        },
        Tool {
            name: "remove_from_playlist",
            description: "Remove songs from one of the listener's own playlists. The listener is asked to \
                          approve first.",
            schema: object(
                json!({
                    "uri": uri("A spotify:playlist:<id> URI of a playlist the listener owns."),
                    "uris": {
                        "type": "array",
                        "items": uri("A spotify:track:<id> URI."),
                        "minItems": 1,
                        "maxItems": PLAYLIST_MAX
                    }
                }),
                &["uri", "uris"],
            ),
        },
        Tool {
            name: "rename_playlist",
            description: "Change the name or description of one of the listener's own playlists. The \
                          listener is asked to approve first.",
            schema: object(
                json!({
                    "uri": uri("A spotify:playlist:<id> URI of a playlist the listener owns."),
                    "name": { "type": "string", "maxLength": NAME_MAX },
                    "description": { "type": "string", "maxLength": DESCRIPTION_MAX }
                }),
                &["uri"],
            ),
        },
        Tool {
            name: "remember",
            description: "Keep a short note about the listener's taste (a like, a dislike, \"never play X\", \
                          a mood for a time of day) for future conversations. One fact per note.",
            schema: object(
                json!({ "note": { "type": "string", "maxLength": NOTE_MAX } }),
                &["note"],
            ),
        },
        Tool {
            name: "forget",
            description: "Delete the remembered notes that contain this text.",
            schema: object(
                json!({ "note": { "type": "string", "maxLength": NOTE_MAX, "description": "Text to match." } }),
                &["note"],
            ),
        },
        Tool {
            name: "memory",
            description: "Everything remembered about the listener.",
            schema: object(json!({}), &[]),
        },
        Tool {
            name: "schedule_task",
            description: "Run a request on a schedule while Spotifast is open, e.g. a fresh commute playlist \
                          every weekday at 08:00. The listener is asked to approve it first.",
            schema: object(
                json!({
                    "repeat": { "type": "string", "enum": ["once", "daily", "weekdays", "weekly"] },
                    "time": { "type": "string", "description": "Local time, 24 hour HH:MM." },
                    "weekday": { "type": "string", "enum": ["mon", "tue", "wed", "thu", "fri", "sat", "sun"], "description": "For weekly." },
                    "date": { "type": "string", "description": "YYYY-MM-DD, for once." },
                    "prompt": { "type": "string", "maxLength": TASK_MAX, "description": "What to do, written as the listener would ask it." }
                }),
                &["repeat", "time", "prompt"],
            ),
        },
        Tool {
            name: "list_schedules",
            description: "The scheduled tasks, with their ids.",
            schema: object(json!({}), &[]),
        },
        Tool {
            name: "cancel_schedule",
            description: "Cancel a scheduled task by its id from list_schedules.",
            schema: object(
                json!({ "id": { "type": "integer", "minimum": 1 } }),
                &["id"],
            ),
        },
    ]
}

/// An object schema with no extra properties.
fn object(properties: Value, required: &[&str]) -> Value {
    let mut schema = json!({
        "type": "object",
        "properties": properties,
        "additionalProperties": false
    });
    if !required.is_empty() {
        schema["required"] = json!(required);
    }
    schema
}

/// `uri` if it is `spotify:<kind>:<22 base-62 characters>` with `kind` one of `kinds`.
pub fn valid_uri<'a>(uri: &'a str, kinds: &[&str]) -> Result<&'a str, String> {
    let mut parts = uri.split(':');
    let ok = matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some("spotify"), Some(kind), Some(id), None)
            if kinds.contains(&kind) && id.len() == 22 && id.bytes().all(|b| b.is_ascii_alphanumeric())
    );
    if ok {
        Ok(uri)
    } else {
        // the offending text is the model's own, but it is cut so a long one cannot flood a log
        let shown: String = uri.chars().filter(|c| !c.is_control()).take(60).collect();
        Err(format!(
            "\"{shown}\" is not a usable URI. Use a spotify:{}:<id> URI exactly as search, queue or recent_plays returned it.",
            kinds.join("|")
        ))
    }
}

fn limit(args: &Value, default: usize, max: usize) -> Result<usize, String> {
    match args.get("limit") {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_u64()
            .map(|n| usize::try_from(n).unwrap_or(max).clamp(1, max))
            .ok_or_else(|| "limit must be a whole number.".to_string()),
    }
}

/// A whole number argument, if given.
fn whole(args: &Value, key: &str) -> Result<Option<u64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("{key} must be a whole number, 0 or more.")),
    }
}

/// A true/false argument, if given.
fn flag(args: &Value, key: &str) -> Result<Option<bool>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_bool()
            .map(Some)
            .ok_or_else(|| format!("{key} must be true or false.")),
    }
}

/// Text from the model: one line, no control characters, trimmed and cut.
fn text(args: &Value, key: &str, max: usize) -> String {
    let raw = args.get(key).and_then(Value::as_str).unwrap_or_default();
    let flat: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    flat.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max)
        .collect()
}

fn one_uri(args: &Value, key: &str, kinds: &[&str]) -> Result<String, String> {
    let u = args.get(key).and_then(Value::as_str).unwrap_or_default();
    valid_uri(u, kinds).map(str::to_string)
}

/// A list of URIs of the given kinds, duplicates dropped, at most `max`.
fn uri_list(
    args: &Value,
    key: &str,
    kinds: &[&str],
    max: usize,
    required: bool,
) -> Result<Vec<String>, String> {
    let list = match args.get(key) {
        None | Some(Value::Null) if !required => return Ok(Vec::new()),
        Some(Value::Array(list)) => list,
        _ => {
            return Err(format!(
                "{key} must be a list of spotify:{}: URIs.",
                kinds.join("|")
            ));
        }
    };
    if list.is_empty() && required {
        return Err(format!("{key} is empty."));
    }
    if list.len() > max {
        return Err(format!(
            "At most {max} per call; send the rest in another call."
        ));
    }
    let mut uris: Vec<String> = Vec::new();
    for v in list {
        let u = valid_uri(v.as_str().unwrap_or_default(), kinds)?;
        if !uris.iter().any(|x| x == u) {
            uris.push(u.to_string());
        }
    }
    Ok(uris)
}

/// The `schedule_task` arguments as a slot.
fn slot(args: &Value) -> Result<Slot, String> {
    let cadence = match args.get("repeat").and_then(Value::as_str).map(str::trim) {
        Some("once") => Cadence::Once,
        Some("daily") => Cadence::Daily,
        Some("weekdays") => Cadence::Weekdays,
        Some("weekly") => Cadence::Weekly,
        _ => return Err("repeat must be once, daily, weekdays or weekly.".into()),
    };
    let time = args
        .get("time")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let minute = time
        .split_once(':')
        .and_then(|(h, m)| {
            let (h, m) = (h.parse::<u16>().ok()?, m.parse::<u16>().ok()?);
            (h < 24 && m < 60 && time.len() <= 5).then_some(h * 60 + m)
        })
        .ok_or("time must be a local 24 hour time like 08:30.")?;
    let weekday = match cadence {
        Cadence::Weekly => {
            let name = args
                .get("weekday")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase();
            let day = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"]
                .iter()
                .position(|d| name.starts_with(d))
                .ok_or("weekday is required for weekly: mon, tue, wed, thu, fri, sat or sun.")?;
            Some(day as u8)
        }
        _ => None,
    };
    let date = match cadence {
        Cadence::Once => {
            let d = args
                .get("date")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim();
            let b = d.as_bytes();
            let shape = b.len() == 10
                && b[4] == b'-'
                && b[7] == b'-'
                && b.iter()
                    .enumerate()
                    .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit());
            let month = d.get(5..7).and_then(|s| s.parse::<u8>().ok()).unwrap_or(0);
            let day = d.get(8..10).and_then(|s| s.parse::<u8>().ok()).unwrap_or(0);
            if !shape || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
                return Err("date is required for once, as YYYY-MM-DD.".into());
            }
            Some(d.to_string())
        }
        _ => None,
    };
    Ok(Slot {
        cadence,
        minute,
        weekday,
        date,
    })
}

/// A tool call's arguments, checked. The error is written for the model.
pub fn parse(name: &str, args: &Value) -> Result<Call, String> {
    match name {
        "search" => {
            let query = args
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let query: String = query
                .chars()
                .filter(|c| !c.is_control())
                .take(MAX_QUERY)
                .collect();
            let query = query.trim().to_string();
            if query.is_empty() {
                return Err("query is required.".into());
            }
            let mut kinds: Vec<Kind> = Vec::new();
            let raw: Vec<&str> = match args.get("types") {
                None | Some(Value::Null) => vec!["track"],
                Some(Value::String(s)) => s.split(',').collect(),
                Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
                Some(_) => {
                    return Err("types must be a list of: track, artist, album, playlist.".into());
                }
            };
            for t in raw {
                let kind = Kind::parse(t).ok_or_else(|| {
                    format!(
                        "\"{}\" is not a kind of result. Use: track, artist, album, playlist.",
                        t.chars().take(20).collect::<String>()
                    )
                })?;
                if !kinds.contains(&kind) {
                    kinds.push(kind);
                }
            }
            if kinds.is_empty() {
                kinds.push(Kind::Track);
            }
            Ok(Call::Search {
                query,
                kinds,
                limit: limit(args, SEARCH_DEFAULT, SEARCH_MAX)?,
            })
        }
        "now_playing" => Ok(Call::NowPlaying),
        "queue" => Ok(Call::Queue),
        "recent_plays" => Ok(Call::RecentPlays {
            limit: limit(args, RECENT_DEFAULT, RECENT_MAX)?,
        }),
        "library_summary" => Ok(Call::LibrarySummary),
        "queue_tracks" => {
            let Some(list) = args.get("uris").and_then(Value::as_array) else {
                return Err("uris must be a list of spotify:track: URIs.".into());
            };
            if list.is_empty() {
                return Err("uris is empty.".into());
            }
            if list.len() > MAX_QUEUE {
                return Err(format!(
                    "At most {MAX_QUEUE} songs per call; send the rest in another call."
                ));
            }
            let mut uris: Vec<String> = Vec::new();
            for v in list {
                let u = valid_uri(v.as_str().unwrap_or_default(), &["track"])?;
                if !uris.iter().any(|x| x == u) {
                    uris.push(u.to_string());
                }
            }
            Ok(Call::QueueTracks { uris })
        }
        "play" => {
            let u = args.get("uri").and_then(Value::as_str).unwrap_or_default();
            Ok(Call::Play {
                uri: valid_uri(u, &["track", "album", "playlist", "artist"])?.to_string(),
            })
        }
        "skip" => Ok(Call::Skip),
        "playlist_tracks" => Ok(Call::PlaylistTracks {
            uri: one_uri(args, "uri", &["playlist"])?,
            offset: u32::try_from(whole(args, "offset")?.unwrap_or(0).min(100_000)).unwrap_or(0),
            limit: limit(args, ROWS_DEFAULT, ROWS_MAX)?,
        }),
        "album_tracks" => Ok(Call::AlbumTracks {
            uri: one_uri(args, "uri", &["album"])?,
        }),
        "artist_top_tracks" => Ok(Call::ArtistTopTracks {
            uri: one_uri(args, "uri", &["artist"])?,
        }),
        "artist_albums" => Ok(Call::ArtistAlbums {
            uri: one_uri(args, "uri", &["artist"])?,
            limit: limit(args, 10, ALBUMS_MAX)?,
        }),
        "liked_tracks" => Ok(Call::LikedTracks {
            offset: u32::try_from(whole(args, "offset")?.unwrap_or(0).min(100_000)).unwrap_or(0),
            limit: limit(args, 50, 50)?,
        }),
        "track_info" => Ok(Call::TrackInfo {
            uris: uri_list(args, "uris", &["track"], INFO_MAX, true)?,
        }),
        "top_items" => {
            let kind = match args.get("kind").and_then(Value::as_str).map(str::trim) {
                Some("artists" | "artist") => TopKind::Artists,
                Some("tracks" | "track" | "songs") => TopKind::Tracks,
                _ => return Err("kind must be artists or tracks.".into()),
            };
            let range = match args.get("range") {
                None | Some(Value::Null) => TopRange::Medium,
                Some(v) => match v.as_str().map(str::trim) {
                    Some("short" | "short_term") => TopRange::Short,
                    Some("medium" | "medium_term") => TopRange::Medium,
                    Some("long" | "long_term") => TopRange::Long,
                    _ => return Err("range must be short, medium or long.".into()),
                },
            };
            Ok(Call::TopItems {
                kind,
                range,
                limit: limit(args, TOP_MAX, TOP_MAX)?,
            })
        }
        "playback_state" => Ok(Call::PlaybackState),
        "now" => Ok(Call::Now),
        "devices" => Ok(Call::Devices),
        "pause" => Ok(Call::Pause),
        "resume" => Ok(Call::Resume),
        "previous" => Ok(Call::Previous),
        "seek" => {
            let seconds = whole(args, "seconds")?.ok_or("seconds is required.")?;
            // a day is more than any song; the player clamps to the song's own length
            Ok(Call::Seek {
                seconds: u32::try_from(seconds.min(86_400)).unwrap_or(0),
            })
        }
        "set_volume" => {
            let percent = whole(args, "percent")?.ok_or("percent is required.")?;
            Ok(Call::SetVolume {
                percent: u8::try_from(percent.min(100)).unwrap_or(100),
            })
        }
        "set_shuffle" => Ok(Call::SetShuffle {
            on: flag(args, "on")?.ok_or("on is required (true or false).")?,
        }),
        "set_repeat" => Ok(Call::SetRepeat {
            mode: match args.get("mode").and_then(Value::as_str).map(str::trim) {
                Some("off") => RepeatKind::Off,
                Some("track") => RepeatKind::Track,
                Some("context") => RepeatKind::Context,
                _ => return Err("mode must be off, track or context.".into()),
            },
        }),
        "clear_queue" => Ok(Call::ClearQueue),
        "remove_from_queue" => Ok(Call::RemoveFromQueue {
            uris: uri_list(args, "uris", &["track"], MAX_QUEUE, true)?,
        }),
        "transfer_playback" => {
            let id = args
                .get("device_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim();
            let plain = !id.is_empty()
                && id.len() <= 64
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
            if !plain {
                return Err("device_id must be an id exactly as devices returned it.".into());
            }
            Ok(Call::TransferPlayback {
                device_id: id.to_string(),
            })
        }
        "like_tracks" => Ok(Call::LikeTracks {
            uris: uri_list(args, "uris", &["track"], LIKE_MAX, true)?,
        }),
        "unlike_tracks" => Ok(Call::UnlikeTracks {
            uris: uri_list(args, "uris", &["track"], LIKE_MAX, true)?,
        }),
        "create_playlist" => {
            let name = text(args, "name", NAME_MAX);
            if name.is_empty() {
                return Err("name is required.".into());
            }
            Ok(Call::CreatePlaylist {
                name,
                description: text(args, "description", DESCRIPTION_MAX),
                uris: uri_list(args, "uris", &["track"], PLAYLIST_MAX, false)?,
                public: flag(args, "public")?.unwrap_or(false),
            })
        }
        "add_to_playlist" => Ok(Call::AddToPlaylist {
            uri: one_uri(args, "uri", &["playlist"])?,
            uris: uri_list(args, "uris", &["track"], PLAYLIST_MAX, true)?,
        }),
        "remove_from_playlist" => Ok(Call::RemoveFromPlaylist {
            uri: one_uri(args, "uri", &["playlist"])?,
            uris: uri_list(args, "uris", &["track"], PLAYLIST_MAX, true)?,
        }),
        "rename_playlist" => {
            let name = text(args, "name", NAME_MAX);
            let description = args
                .get("description")
                .and_then(Value::as_str)
                .map(|_| text(args, "description", DESCRIPTION_MAX));
            if name.is_empty() && description.is_none() {
                return Err("Give a new name or a new description.".into());
            }
            Ok(Call::RenamePlaylist {
                uri: one_uri(args, "uri", &["playlist"])?,
                name: (!name.is_empty()).then_some(name),
                description,
            })
        }
        "remember" => {
            let note = text(args, "note", NOTE_MAX);
            if note.is_empty() {
                return Err("note is required.".into());
            }
            Ok(Call::Remember { note })
        }
        "forget" => {
            let note = text(args, "note", NOTE_MAX);
            if note.is_empty() {
                return Err("note is required.".into());
            }
            Ok(Call::Forget { note })
        }
        "memory" => Ok(Call::Memory),
        "schedule_task" => {
            let prompt = text(args, "prompt", TASK_MAX);
            if prompt.is_empty() {
                return Err("prompt is required.".into());
            }
            Ok(Call::ScheduleTask {
                slot: slot(args)?,
                prompt,
            })
        }
        "list_schedules" => Ok(Call::ListSchedules),
        "cancel_schedule" => {
            let id = whole(args, "id")?.ok_or("id is required.")?;
            Ok(Call::CancelSchedule {
                id: u32::try_from(id).map_err(|_| "id is not a schedule id.".to_string())?,
            })
        }
        other => Err(format!(
            "There is no tool called {}.",
            other
                .chars()
                .filter(|c| !c.is_control())
                .take(40)
                .collect::<String>()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "spotify:track:4uLU6hMCjMI75M1A2tKUQC";

    #[test]
    fn uris_are_checked_by_kind_and_shape() {
        assert_eq!(valid_uri(T, &["track"]), Ok(T));
        assert!(valid_uri(T, &["album"]).is_err());
        assert!(valid_uri("spotify:track:short", &["track"]).is_err());
        assert!(valid_uri("spotify:track:4uLU6hMCjMI75M1A2tKUQC:extra", &["track"]).is_err());
        assert!(valid_uri("spotify:track:4uLU6hMCjMI75M1A2tKU!C", &["track"]).is_err());
        assert!(
            valid_uri(
                "https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC",
                &["track"]
            )
            .is_err()
        );
        assert!(
            valid_uri(
                "spotify:user:x:playlist:4uLU6hMCjMI75M1A2tKUQC",
                &["playlist"]
            )
            .is_err()
        );
        assert!(
            valid_uri(
                "spotify:episode:4uLU6hMCjMI75M1A2tKUQC",
                &["track", "album"]
            )
            .is_err()
        );
        assert!(valid_uri("", &["track"]).is_err());
    }

    #[test]
    fn a_bad_uri_is_reported_without_control_characters() {
        let e = valid_uri("spotify:track:\n\u{1b}[31mevil\0", &["track"]).unwrap_err();
        assert!(!e.chars().any(|c| c.is_control()), "{e:?}");
    }

    #[test]
    fn search_takes_defaults_and_clamps() {
        assert_eq!(
            parse("search", &json!({ "query": "  daft punk  " })),
            Ok(Call::Search {
                query: "daft punk".into(),
                kinds: vec![Kind::Track],
                limit: SEARCH_DEFAULT
            })
        );
        assert_eq!(
            parse(
                "search",
                &json!({ "query": "x", "types": ["album", "Artists", "album"], "limit": 500 })
            ),
            Ok(Call::Search {
                query: "x".into(),
                kinds: vec![Kind::Album, Kind::Artist],
                limit: SEARCH_MAX
            })
        );
        assert_eq!(
            parse(
                "search",
                &json!({ "query": "x", "types": "track,playlist", "limit": 0 })
            ),
            Ok(Call::Search {
                query: "x".into(),
                kinds: vec![Kind::Track, Kind::Playlist],
                limit: 1
            })
        );
        assert!(parse("search", &json!({})).is_err());
        assert!(parse("search", &json!({ "query": "   " })).is_err());
        assert!(parse("search", &json!({ "query": "x", "types": ["show"] })).is_err());
        assert!(parse("search", &json!({ "query": "x", "limit": "many" })).is_err());
        // control characters never reach the search request
        let Ok(Call::Search { query, .. }) = parse("search", &json!({ "query": "a\r\nb" })) else {
            panic!()
        };
        assert_eq!(query, "ab");
    }

    #[test]
    fn a_query_is_cut_to_a_sane_length() {
        let long = "x".repeat(5000);
        let Ok(Call::Search { query, .. }) = parse("search", &json!({ "query": long })) else {
            panic!()
        };
        assert_eq!(query.len(), MAX_QUERY);
    }

    #[test]
    fn queue_tracks_validates_every_uri_and_caps_the_list() {
        assert_eq!(
            parse("queue_tracks", &json!({ "uris": [T, T] })),
            Ok(Call::QueueTracks {
                uris: vec![T.into()]
            })
        );
        assert!(parse("queue_tracks", &json!({ "uris": [] })).is_err());
        assert!(parse("queue_tracks", &json!({})).is_err());
        assert!(
            parse(
                "queue_tracks",
                &json!({ "uris": [T, "spotify:album:4uLU6hMCjMI75M1A2tKUQC"] })
            )
            .is_err()
        );
        assert!(parse("queue_tracks", &json!({ "uris": [T, 5] })).is_err());
        let many: Vec<String> = (0..=MAX_QUEUE).map(|_| T.to_string()).collect();
        assert!(
            parse("queue_tracks", &json!({ "uris": many }))
                .unwrap_err()
                .contains("At most")
        );
        let ids: Vec<String> = (0..MAX_QUEUE)
            .map(|i| format!("spotify:track:{i:022}"))
            .collect();
        let Ok(Call::QueueTracks { uris }) = parse("queue_tracks", &json!({ "uris": ids })) else {
            panic!()
        };
        assert_eq!(uris.len(), MAX_QUEUE);
    }

    #[test]
    fn play_accepts_a_context_and_nothing_else() {
        for kind in ["track", "album", "playlist", "artist"] {
            let uri = format!("spotify:{kind}:4uLU6hMCjMI75M1A2tKUQC");
            assert_eq!(
                parse("play", &json!({ "uri": uri })),
                Ok(Call::Play { uri })
            );
        }
        assert!(
            parse(
                "play",
                &json!({ "uri": "spotify:show:4uLU6hMCjMI75M1A2tKUQC" })
            )
            .is_err()
        );
        assert!(
            parse(
                "play",
                &json!({ "uri": "spotify:station:track:4uLU6hMCjMI75M1A2tKUQC" })
            )
            .is_err()
        );
        assert!(parse("play", &json!({})).is_err());
    }

    #[test]
    fn simple_tools_and_unknown_names() {
        assert_eq!(parse("skip", &json!({})), Ok(Call::Skip));
        assert_eq!(parse("now_playing", &json!({})), Ok(Call::NowPlaying));
        assert_eq!(parse("queue", &json!({})), Ok(Call::Queue));
        assert_eq!(
            parse("library_summary", &json!({})),
            Ok(Call::LibrarySummary)
        );
        assert_eq!(
            parse("recent_plays", &json!({})),
            Ok(Call::RecentPlays {
                limit: RECENT_DEFAULT
            })
        );
        assert_eq!(
            parse("recent_plays", &json!({ "limit": 9999 })),
            Ok(Call::RecentPlays { limit: RECENT_MAX })
        );
        assert!(parse("delete_playlist", &json!({})).is_err());
    }

    const P: &str = "spotify:playlist:4uLU6hMCjMI75M1A2tKUQC";

    #[test]
    fn reads_take_defaults_and_caps() {
        assert_eq!(
            parse("playlist_tracks", &json!({ "uri": P })),
            Ok(Call::PlaylistTracks {
                uri: P.into(),
                offset: 0,
                limit: ROWS_DEFAULT
            })
        );
        assert_eq!(
            parse(
                "playlist_tracks",
                &json!({ "uri": P, "offset": 150, "limit": 9999 })
            ),
            Ok(Call::PlaylistTracks {
                uri: P.into(),
                offset: 150,
                limit: ROWS_MAX
            })
        );
        // a playlist call takes a playlist, never a track or a user path
        assert!(parse("playlist_tracks", &json!({ "uri": T })).is_err());
        assert!(parse("playlist_tracks", &json!({ "uri": P, "offset": -1 })).is_err());
        assert!(parse("album_tracks", &json!({ "uri": P })).is_err());
        assert_eq!(
            parse("liked_tracks", &json!({ "limit": 500 })),
            Ok(Call::LikedTracks {
                offset: 0,
                limit: 50
            })
        );
        assert_eq!(
            parse("top_items", &json!({ "kind": "artists" })),
            Ok(Call::TopItems {
                kind: TopKind::Artists,
                range: TopRange::Medium,
                limit: TOP_MAX
            })
        );
        assert_eq!(
            parse(
                "top_items",
                &json!({ "kind": "tracks", "range": "long", "limit": 5 })
            ),
            Ok(Call::TopItems {
                kind: TopKind::Tracks,
                range: TopRange::Long,
                limit: 5
            })
        );
        assert!(parse("top_items", &json!({ "kind": "podcasts" })).is_err());
        assert!(parse("top_items", &json!({ "kind": "tracks", "range": "year" })).is_err());
        let many: Vec<String> = (0..=INFO_MAX)
            .map(|i| format!("spotify:track:{i:022}"))
            .collect();
        assert!(
            parse("track_info", &json!({ "uris": many }))
                .unwrap_err()
                .contains("At most")
        );
    }

    #[test]
    fn playback_controls_are_checked_and_clamped() {
        assert_eq!(
            parse("seek", &json!({ "seconds": 90 })),
            Ok(Call::Seek { seconds: 90 })
        );
        assert_eq!(
            parse("seek", &json!({ "seconds": 10u64.pow(12) })),
            Ok(Call::Seek { seconds: 86_400 })
        );
        assert!(parse("seek", &json!({})).is_err());
        assert!(parse("seek", &json!({ "seconds": -3 })).is_err());
        assert_eq!(
            parse("set_volume", &json!({ "percent": 400 })),
            Ok(Call::SetVolume { percent: 100 })
        );
        assert_eq!(
            parse("set_shuffle", &json!({ "on": true })),
            Ok(Call::SetShuffle { on: true })
        );
        assert!(parse("set_shuffle", &json!({ "on": "yes" })).is_err());
        assert_eq!(
            parse("set_repeat", &json!({ "mode": "context" })),
            Ok(Call::SetRepeat {
                mode: RepeatKind::Context
            })
        );
        assert!(parse("set_repeat", &json!({ "mode": "all" })).is_err());
        assert_eq!(
            parse("transfer_playback", &json!({ "device_id": "abc123_-" })),
            Ok(Call::TransferPlayback {
                device_id: "abc123_-".into()
            })
        );
        for bad in ["", "a b", "a/b", "../x", &"x".repeat(65)] {
            assert!(
                parse("transfer_playback", &json!({ "device_id": bad })).is_err(),
                "{bad:?}"
            );
        }
        assert_eq!(
            parse("remove_from_queue", &json!({ "uris": [T] })),
            Ok(Call::RemoveFromQueue {
                uris: vec![T.into()]
            })
        );
        assert!(parse("remove_from_queue", &json!({ "uris": [] })).is_err());
    }

    #[test]
    fn library_writes_are_typed_capped_and_flagged_for_approval() {
        let call = parse(
            "create_playlist",
            &json!({ "name": "  Night\n drive ", "uris": [T, T], "public": true }),
        )
        .unwrap();
        assert_eq!(
            call,
            Call::CreatePlaylist {
                name: "Night drive".into(),
                description: String::new(),
                uris: vec![T.into()],
                public: true
            }
        );
        assert!(call.needs_approval());
        // private unless asked, and an empty playlist is allowed
        let Ok(Call::CreatePlaylist { public, uris, .. }) =
            parse("create_playlist", &json!({ "name": "x" }))
        else {
            panic!()
        };
        assert!(!public && uris.is_empty());
        assert!(parse("create_playlist", &json!({ "name": "   " })).is_err());
        let long = "n".repeat(500);
        let Ok(Call::CreatePlaylist { name, .. }) =
            parse("create_playlist", &json!({ "name": long }))
        else {
            panic!()
        };
        assert_eq!(name.len(), NAME_MAX);
        let many: Vec<String> = (0..=PLAYLIST_MAX)
            .map(|i| format!("spotify:track:{i:022}"))
            .collect();
        assert!(parse("create_playlist", &json!({ "name": "x", "uris": many })).is_err());
        assert!(
            parse("add_to_playlist", &json!({ "uri": P, "uris": [T] }))
                .unwrap()
                .needs_approval()
        );
        assert!(parse("add_to_playlist", &json!({ "uri": T, "uris": [T] })).is_err());
        assert!(parse("add_to_playlist", &json!({ "uri": P, "uris": [P] })).is_err());
        assert!(parse("remove_from_playlist", &json!({ "uri": P, "uris": [] })).is_err());
        assert_eq!(
            parse("rename_playlist", &json!({ "uri": P, "name": "New" })),
            Ok(Call::RenamePlaylist {
                uri: P.into(),
                name: Some("New".into()),
                description: None
            })
        );
        assert!(parse("rename_playlist", &json!({ "uri": P })).is_err());
        let fifty: Vec<String> = (0..=LIKE_MAX)
            .map(|i| format!("spotify:track:{i:022}"))
            .collect();
        assert!(parse("like_tracks", &json!({ "uris": fifty })).is_err());
        // reads and playback never wait for approval
        for read in [
            Call::Skip,
            Call::Queue,
            Call::Pause,
            Call::ClearQueue,
            Call::Memory,
        ] {
            assert!(!read.needs_approval());
        }
        assert!(
            Call::LikeTracks { uris: vec![] }.needs_approval()
                && Call::UnlikeTracks { uris: vec![] }.needs_approval()
        );
    }

    #[test]
    fn notes_and_schedules_are_parsed() {
        assert_eq!(
            parse("remember", &json!({ "note": "never\nplay Nickelback" })),
            Ok(Call::Remember {
                note: "never play Nickelback".into()
            })
        );
        assert!(parse("remember", &json!({ "note": " " })).is_err());
        let Ok(Call::ScheduleTask { slot, prompt }) = parse(
            "schedule_task",
            &json!({ "repeat": "weekly", "time": "08:30", "weekday": "Monday", "prompt": "commute mix" }),
        ) else {
            panic!()
        };
        assert_eq!(
            slot,
            Slot {
                cadence: Cadence::Weekly,
                minute: 8 * 60 + 30,
                weekday: Some(0),
                date: None
            }
        );
        assert_eq!(prompt, "commute mix");
        assert!(
            parse(
                "schedule_task",
                &json!({ "repeat": "daily", "time": "24:00", "prompt": "x" })
            )
            .is_err()
        );
        assert!(
            parse(
                "schedule_task",
                &json!({ "repeat": "weekly", "time": "08:00", "prompt": "x" })
            )
            .is_err()
        );
        assert!(
            parse(
                "schedule_task",
                &json!({ "repeat": "once", "time": "08:00", "date": "tomorrow", "prompt": "x" })
            )
            .is_err()
        );
        assert!(
            parse(
                "schedule_task",
                &json!({ "repeat": "once", "time": "23:59", "date": "2026-10-14", "prompt": "x" })
            )
            .unwrap()
            .needs_approval()
        );
        assert_eq!(
            parse("cancel_schedule", &json!({ "id": 3 })),
            Ok(Call::CancelSchedule { id: 3 })
        );
        assert!(parse("cancel_schedule", &json!({ "id": -1 })).is_err());
    }

    #[test]
    fn what_outlives_the_conversation_needs_approval_and_forgetting_does_not() {
        let note = parse("remember", &json!({ "note": "no country" })).unwrap();
        assert!(note.needs_approval());
        assert!(
            !parse("forget", &json!({ "note": "country" }))
                .unwrap()
                .needs_approval()
        );
        assert!(!Call::Skip.needs_approval());
    }

    #[test]
    fn the_registry_has_no_duplicates_and_no_library_deletion() {
        let tools = registry();
        let mut names: Vec<&str> = tools.iter().map(|t| t.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len());
        for banned in ["delete_playlist", "follow_artist", "unfollow_artist"] {
            assert!(!names.contains(&banned));
        }
    }

    #[test]
    fn every_listed_tool_parses_and_has_an_object_schema() {
        for t in registry() {
            assert_eq!(t.schema["type"], "object", "{}", t.name);
            assert!(!t.description.is_empty());
            // an unknown name is the only way parse refuses a tool with no required arguments
            let r = parse(t.name, &json!({}));
            assert!(
                r.is_ok() || t.schema.get("required").is_some(),
                "{} -> {r:?}",
                t.name
            );
        }
    }
}
