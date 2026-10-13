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
    ]
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
