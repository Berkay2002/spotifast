//! What the DJ page draws for a tool's result besides its log line: songs with covers, what left
//! the queue, a playlist, a read. `crate::app::dj` builds them on the UI thread from data the app
//! already holds. They are not saved with the chat: a reopened chat shows the log lines only.

use super::render::clean;
use crate::api::models::{PlayableItem, Track};

/// The rows a card lists before it says "+N more".
pub const CARD_SONGS: usize = 5;
/// The cover size a card asks for, in pixels.
const COVER: u32 = 300;

/// A song as one row or one cover on the page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardSong {
    pub name: String,
    pub artists: String,
    pub cover: Option<String>,
    pub duration_ms: u32,
}

impl CardSong {
    pub fn track(track: &Track) -> Self {
        Self {
            name: clean(&track.name),
            artists: clean(&track.artist_names()),
            cover: track.image(COVER).map(str::to_string),
            duration_ms: track.duration_ms,
        }
    }

    pub fn item(item: &PlayableItem) -> Self {
        Self {
            name: clean(item.name()),
            artists: clean(&item.subtitle()),
            cover: item.image(COVER).map(str::to_string),
            duration_ms: item.duration_ms(),
        }
    }
}

/// A picture of an artist, album or playlist, at the size cards use.
pub fn picture(images: &[crate::api::models::Image]) -> Option<String> {
    crate::api::models::pick_image(images, COVER).map(str::to_string)
}

/// A change the DJ asked for, as the approval card shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub id: u64,
    pub title: String,
    pub detail: String,
    /// Up to four covers for the collage.
    pub covers: Vec<String>,
    /// The label of the button that allows it.
    pub confirm: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Card {
    /// Songs the DJ queued.
    Queued { songs: Vec<CardSong>, total: usize },
    /// Songs that left the queue.
    Removed { songs: Vec<CardSong>, total: usize },
    /// A playlist the DJ made or added to.
    Playlist {
        name: String,
        uri: String,
        created: bool,
        songs: Vec<CardSong>,
        total: usize,
    },
    /// Something the DJ read: a playlist, an album, the liked songs.
    Read {
        what: String,
        name: String,
        detail: String,
        cover: Option<String>,
    },
    /// Artists the DJ read, as their photos.
    Artists {
        what: String,
        photos: Vec<Option<String>>,
    },
    /// What a search found, as covers with their titles.
    Found {
        what: String,
        covers: Vec<(String, Option<String>)>,
    },
}
