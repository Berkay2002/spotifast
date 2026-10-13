//! The AI DJ's side of the app: it answers the tool calls the DJ's model makes.
//!
//! A call arrives from the tool server's thread already validated (`crate::dj::tools`). It is
//! turned into what a click would do: the same actions, the same optimistic queue rows, the same
//! backend requests, so the DJ can never do anything the listener could not do by hand.
//!
//! Changes to the library wait here for the listener's yes or no (the gate), unless the listener
//! switched that off. The DJ's own turns (a low queue, a schedule) start from `dj_background`.

use super::*;
use crate::api::models::{PlayableItem, SearchResults, Track};
use crate::backend::{DjData, DjRead};
use crate::dj::agent::{self, Moment};
use crate::dj::render;
use crate::dj::session::PendingCall;
use crate::dj::tools::{Call, Kind, RepeatKind, ToolReply, TopKind};
use std::collections::VecDeque;
use std::sync::mpsc::Sender;

/// How long a search or read may take before the model is told it failed.
const SEARCH_WAIT: Duration = Duration::from_secs(25);
/// How often the schedule is looked at.
const SCHEDULE_EVERY: Duration = Duration::from_secs(30);
/// The queue counts as low at this many upcoming songs or fewer.
const QUEUE_LOW: usize = 3;
/// A song left before this many milliseconds counts as skipped.
const SKIP_WITHIN_MS: u32 = 20_000;
/// Two skips this close together say the listener does not like the picks.
const SKIP_STREAK: Duration = Duration::from_secs(120);

type Reply = Sender<Result<ToolReply, String>>;

/// A search call waiting on the backend. A search with playlists in it is two requests (the
/// catalogue and the playlist search are separate operations), merged when both are back.
pub(super) struct DjSearch {
    reply: Reply,
    kinds: Vec<Kind>,
    limit: usize,
    waiting: u8,
    results: SearchResults,
    error: Option<String>,
    started: Instant,
}

/// How a read's data is told back to the model.
enum Shape {
    Playlist { offset: u32 },
    Tracks { title: &'static str, offset: u32 },
    Albums,
    Artists,
    Info,
}

pub(super) struct DjReadJob {
    reply: Reply,
    shape: Shape,
    started: Instant,
}

/// A playlist the DJ asked to create, waiting for Spotify to name it.
pub(super) struct DjCreate {
    reply: Reply,
    uris: Vec<String>,
    started: Instant,
}

/// What the DJ has in flight and what it watches for.
#[derive(Default)]
pub(super) struct DjState {
    reads: HashMap<u64, DjReadJob>,
    pub(super) create: Option<DjCreate>,
    queue_len: Option<usize>,
    track: Option<(String, String, u32, u32)>,
    skips: Vec<Instant>,
    schedule_checked: Option<Instant>,
    due: VecDeque<String>,
}

fn id_of(uri: &str) -> Result<String, String> {
    util::uri_id(uri)
        .map(str::to_string)
        .ok_or_else(|| "That is not a Spotify URI.".to_string())
}

fn reply_ok(text: impl Into<String>, log: Option<String>) -> Result<ToolReply, String> {
    Ok(ToolReply {
        text: text.into(),
        log,
    })
}

fn songs(count: usize) -> String {
    format!("{count} {}", if count == 1 { "song" } else { "songs" })
}

impl App {
    /// Once a frame: applies what the DJ's turn reported and answers its tool calls.
    pub(super) fn handle_dj(&mut self) {
        self.dj.web = self.settings.dj_web;
        self.dj.poll();
        for pending in self.dj.take_calls() {
            if self.settings.dj_enabled {
                self.dj_call(pending);
            } else {
                let _ = pending
                    .reply
                    .send(Err("The DJ is switched off in Spotifast's settings.".into()));
            }
        }
        for summary in self.dj.expire_approvals() {
            self.dj
                .note(format!("No answer, so nothing was changed: {summary}"));
        }
        self.dj_expire_jobs();
        if !self.settings.dj_enabled {
            if self.dj.busy {
                self.dj.shutdown();
            }
            return;
        }
        self.dj_background();
    }

    fn dj_expire_jobs(&mut self) {
        let late: Vec<u64> = self
            .dj_searches
            .iter()
            .filter(|(_, job)| job.started.elapsed() > SEARCH_WAIT)
            .map(|(id, _)| *id)
            .collect();
        for id in late {
            if let Some(job) = self.dj_searches.remove(&id) {
                let _ = job
                    .reply
                    .send(Err("The search timed out. Try again.".into()));
            }
        }
        let late: Vec<u64> = self
            .dj_state
            .reads
            .iter()
            .filter(|(_, job)| job.started.elapsed() > SEARCH_WAIT)
            .map(|(id, _)| *id)
            .collect();
        for id in late {
            if let Some(job) = self.dj_state.reads.remove(&id) {
                let _ = job
                    .reply
                    .send(Err("Spotify took too long to answer. Try again.".into()));
            }
        }
        if self
            .dj_state
            .create
            .as_ref()
            .is_some_and(|job| job.started.elapsed() > SEARCH_WAIT)
            && let Some(job) = self.dj_state.create.take()
        {
            self.playlist_busy = false;
            let _ = job.reply.send(Err(
                    "Spotify took too long to create the playlist. Check the library before trying again."
                        .into(),
                ));
        }
    }

    /// The DJ page's buttons.
    pub(super) fn dj_action(&mut self, action: &Action) {
        match action {
            Action::DjSend => {
                let text = std::mem::take(&mut self.dj.input);
                self.dj_ask(&text);
            }
            Action::DjStart => self.dj_ask(crate::dj::START_PROMPT),
            Action::DjCancel => self.dj.cancel(),
            Action::DjCheck => self.dj.refresh_status(),
            Action::DjApprove(id) => self.dj_decide(*id, true),
            Action::DjDeny(id) => self.dj_decide(*id, false),
            Action::DjNewChat => self.dj.new_chat(),
            Action::DjOpenChat(id) => self.dj.open_chat(*id),
            Action::DjDeleteChat(id) => self.dj.delete_chat(*id),
            _ => {}
        }
    }

    fn dj_ask(&mut self, text: &str) {
        let model = self.settings.dj_model.clone();
        let effort = self.settings.dj_effort.clone();
        self.dj.ask(text, &model, &effort);
    }

    /// The listener's answer to a change the DJ asked for.
    fn dj_decide(&mut self, id: u64, approve: bool) {
        let Some(approval) = self.dj.take_approval(id) else {
            return;
        };
        if approve && approval.asked.elapsed() > crate::dj::session::APPROVAL_WAIT {
            // the model was told nothing changed: it must stay true
            self.dj.note(format!(
                "That took too long, so nothing was changed: {}",
                approval.summary
            ));
            let _ = approval.reply.send(Err(
                "The listener did not answer in time, so nothing was changed.".to_string(),
            ));
        } else if approve {
            self.dj_run(approval.call, approval.reply);
        } else {
            self.dj.note(format!("Declined: {}", approval.summary));
            let _ = approval.reply.send(Err(
                "The listener declined this change, so nothing was changed. Do not ask again \
                 unless they ask for it."
                    .to_string(),
            ));
        }
    }

    fn dj_call(&mut self, pending: PendingCall) {
        let PendingCall { call, reply } = pending;
        // What lasts (a note, a scheduled task) is never made by a turn nobody is watching: text
        // from a web page or a playlist name could have steered it.
        if self.dj.unprompted() && matches!(call, Call::Remember { .. } | Call::ScheduleTask { .. })
        {
            let _ = reply.send(Err(
                "Notes and schedules are only saved when the listener asks for them in a message."
                    .to_string(),
            ));
            return;
        }
        // A change names songs the listener can read about: one the DJ has not seen is refused
        // before anything is asked.
        let named = match &call {
            Call::LikeTracks { uris }
            | Call::UnlikeTracks { uris }
            | Call::AddToPlaylist { uris, .. }
            | Call::RemoveFromPlaylist { uris, .. }
            | Call::CreatePlaylist { uris, .. } => Some(uris),
            _ => None,
        };
        if let Some(Err(error)) = named.map(|uris| self.dj_known_tracks(uris)) {
            let _ = reply.send(Err(error));
            return;
        }
        // Notes and schedules are asked about even with the library gate off: they outlive the
        // conversation.
        let lasting = matches!(call, Call::Remember { .. } | Call::ScheduleTask { .. });
        if call.needs_approval() && (self.settings.dj_confirm_writes || lasting) {
            let summary = self.dj_summary(&call);
            self.dj.ask_approval(summary, call, reply);
        } else {
            self.dj_run(call, reply);
        }
    }

    /// A song the DJ has seen: found by search or a read, or on the queue.
    fn dj_track(&self, uri: &str) -> Option<Track> {
        if let Some(track) = util::uri_id(uri).and_then(|id| self.track_cache.get(id)) {
            return Some(track.clone());
        }
        let queue = self.queue.get()?;
        queue
            .currently_playing
            .iter()
            .chain(queue.queue.iter())
            .find_map(|item| match item {
                PlayableItem::Track(track) if track.uri == uri => Some(track.clone()),
                _ => None,
            })
    }

    /// Names for the songs a change touches, as the listener will read them: "A by X; B by Y and
    /// 4 more". Every song in a change is one the DJ has seen (`dj_known_tracks`), so none is
    /// shown as a bare "a song".
    fn dj_names(&self, uris: &[String]) -> String {
        let name = |uri: &String| {
            self.dj_track(uri)
                .map(|track| {
                    let by: Vec<String> = track
                        .artists
                        .iter()
                        .map(|artist| render::clean(&artist.name))
                        .collect();
                    if by.is_empty() {
                        render::clean(&track.name)
                    } else {
                        format!("{} by {}", render::clean(&track.name), by.join(", "))
                    }
                })
                .unwrap_or_else(|| uri.clone())
        };
        let shown: Vec<String> = uris.iter().take(8).map(name).collect();
        let mut out = shown.join("; ");
        if uris.len() > 8 {
            out.push_str(&format!(" and {} more", uris.len() - 8));
        }
        out
    }

    fn dj_playlist_name(&self, uri: &str) -> String {
        util::uri_id(uri)
            .and_then(|id| self.library_entry(id))
            .map(|playlist| render::clean(&playlist.name))
            .unwrap_or_else(|| "a playlist".to_string())
    }

    /// What a change would do, in the listener's words, for the approval card.
    fn dj_summary(&self, call: &Call) -> String {
        match call {
            Call::LikeTracks { uris } => {
                format!(
                    "Add {} to Liked Songs: {}",
                    songs(uris.len()),
                    self.dj_names(uris)
                )
            }
            Call::UnlikeTracks { uris } => {
                format!(
                    "Remove {} from Liked Songs: {}",
                    songs(uris.len()),
                    self.dj_names(uris)
                )
            }
            Call::CreatePlaylist {
                name,
                description,
                uris,
                public,
            } => {
                let mut out = format!(
                    "Create the {} playlist \"{}\"",
                    if *public { "public" } else { "private" },
                    render::clean(name),
                );
                if !description.is_empty() {
                    out.push_str(&format!(
                        ", described as \"{}\"",
                        render::clean(description)
                    ));
                }
                out.push_str(&format!(" with {}", songs(uris.len())));
                if !uris.is_empty() {
                    out.push_str(&format!(": {}", self.dj_names(uris)));
                }
                out
            }
            Call::AddToPlaylist { uri, uris } => format!(
                "Add {} to \"{}\": {}",
                songs(uris.len()),
                self.dj_playlist_name(uri),
                self.dj_names(uris)
            ),
            Call::RemoveFromPlaylist { uri, uris } => format!(
                "Remove {} from \"{}\": {}",
                songs(uris.len()),
                self.dj_playlist_name(uri),
                self.dj_names(uris)
            ),
            Call::RenamePlaylist {
                uri,
                name,
                description,
            } => {
                let mut out = format!("Change \"{}\"", self.dj_playlist_name(uri));
                if let Some(name) = name {
                    out.push_str(&format!(", new name \"{}\"", render::clean(name)));
                }
                if let Some(description) = description {
                    out.push_str(&format!(
                        ", new description \"{}\"",
                        render::clean(description)
                    ));
                }
                out
            }
            Call::Remember { note } => {
                format!("Remember for next time: \"{}\"", render::clean(note))
            }
            Call::ScheduleTask { slot, prompt } => format!(
                "Run this by itself {}: \"{}\"",
                agent::slot_text(slot),
                render::clean(prompt)
            ),
            _ => "Make a change".to_string(),
        }
    }

    /// A playlist the listener may edit, as its id and name.
    fn dj_editable(&self, uri: &str) -> Result<(String, String), String> {
        let id = id_of(uri)?;
        let Some(playlist) = self.library_entry(&id) else {
            return Err(
                "That playlist is not in the listener's library (or the library is still loading)."
                    .into(),
            );
        };
        if !self.can_edit_playlist(playlist) {
            return Err(format!(
                "\"{}\" is not the listener's own playlist, so it cannot be changed.",
                render::clean(&playlist.name)
            ));
        }
        Ok((id, playlist.name.clone()))
    }

    /// The songs behind `uris`, from what the DJ has already seen.
    fn dj_known_tracks(&self, uris: &[String]) -> Result<Vec<Track>, String> {
        uris.iter()
            .map(|uri| {
                self.dj_track(uri).ok_or_else(|| {
                    format!("Song {uri} has not been seen yet. Find it with search first.")
                })
            })
            .collect()
    }

    fn dj_run(&mut self, call: Call, reply: Reply) {
        match call {
            Call::Search {
                query,
                kinds,
                limit,
            } => self.dj_search(query, kinds, limit, reply),
            Call::PlaylistTracks { uri, offset, limit } => {
                match id_of(&uri) {
                    Ok(id) => self.dj_read(
                        DjRead::PlaylistTracks {
                            id,
                            offset,
                            limit: limit as u32,
                        },
                        Shape::Playlist { offset },
                        reply,
                    ),
                    Err(error) => {
                        let _ = reply.send(Err(error));
                    }
                };
            }
            Call::AlbumTracks { uri } => match id_of(&uri) {
                Ok(id) => self.dj_read(
                    DjRead::AlbumTracks { id },
                    Shape::Tracks {
                        title: "Album songs",
                        offset: 0,
                    },
                    reply,
                ),
                Err(error) => {
                    let _ = reply.send(Err(error));
                }
            },
            Call::ArtistTopTracks { uri } => match id_of(&uri) {
                Ok(id) => self.dj_read(
                    DjRead::ArtistTop { id },
                    Shape::Tracks {
                        title: "The artist's top songs",
                        offset: 0,
                    },
                    reply,
                ),
                Err(error) => {
                    let _ = reply.send(Err(error));
                }
            },
            Call::ArtistAlbums { uri, limit } => match id_of(&uri) {
                Ok(id) => self.dj_read(
                    DjRead::ArtistAlbums {
                        id,
                        limit: limit as u32,
                    },
                    Shape::Albums,
                    reply,
                ),
                Err(error) => {
                    let _ = reply.send(Err(error));
                }
            },
            Call::LikedTracks { offset, limit } => self.dj_read(
                DjRead::Liked {
                    offset,
                    limit: limit as u32,
                },
                Shape::Tracks {
                    title: "Liked Songs",
                    offset,
                },
                reply,
            ),
            Call::TrackInfo { uris } => {
                let ids: Result<Vec<String>, String> = uris.iter().map(|u| id_of(u)).collect();
                match ids {
                    Ok(ids) => self.dj_read(DjRead::Tracks { ids }, Shape::Info, reply),
                    Err(error) => {
                        let _ = reply.send(Err(error));
                    }
                }
            }
            Call::TopItems { kind, range, limit } => {
                let limit = limit as u32;
                let range = range.api();
                match kind {
                    TopKind::Artists => {
                        self.dj_read(DjRead::TopArtists { range, limit }, Shape::Artists, reply)
                    }
                    TopKind::Tracks => self.dj_read(
                        DjRead::TopTracks { range, limit },
                        Shape::Tracks {
                            title: "The listener's top songs",
                            offset: 0,
                        },
                        reply,
                    ),
                }
            }
            Call::CreatePlaylist {
                name,
                description,
                uris,
                public,
            } => self.dj_create_playlist(name, description, uris, public, reply),
            other => {
                let out = self.dj_sync(other);
                let _ = reply.send(out);
            }
        }
    }

    /// The calls the app can answer on the spot.
    fn dj_sync(&mut self, call: Call) -> Result<ToolReply, String> {
        match call {
            Call::NowPlaying => reply_ok(render::now_playing(self.now_playing().as_ref()), None),
            Call::PlaybackState => {
                reply_ok(render::playback_state(self.now_playing().as_ref()), None)
            }
            Call::Now => reply_ok(
                jiff::Zoned::now().strftime("%A %Y-%m-%d %H:%M").to_string(),
                None,
            ),
            Call::Devices => {
                if self.devices.is_empty() {
                    self.actions.push(Action::RefreshDevices);
                    return reply_ok(
                        "The device list is loading. Call devices again in a moment.",
                        None,
                    );
                }
                reply_ok(render::devices(&self.devices), None)
            }
            Call::Queue => {
                if self.queue.get().is_none() {
                    // Refreshing is asynchronous: say so rather than claim an empty queue.
                    self.refresh_queue(true);
                    return reply_ok(
                        "The queue is still loading. Call queue again in a moment.",
                        None,
                    );
                }
                let text = match self.queue.get() {
                    Some(queue) => {
                        render::queue(queue.currently_playing.as_ref(), &queue.queue, 40)
                    }
                    None => String::new(),
                };
                reply_ok(text, None)
            }
            Call::RecentPlays { limit } => {
                reply_ok(render::recent(&self.recents_view, limit), None)
            }
            Call::LibrarySummary => {
                let none: Vec<crate::api::models::Artist> = Vec::new();
                let artists = self.home.top_artists.get().unwrap_or(&none);
                let top = self
                    .home
                    .top_tracks
                    .get()
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let lists = self
                    .library
                    .playlists
                    .get()
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                reply_ok(render::library(artists, top, lists), None)
            }
            Call::QueueTracks { uris } => {
                let songs_to_queue: Vec<(String, String)> = uris
                    .iter()
                    .map(|uri| {
                        let name = util::uri_id(uri)
                            .and_then(|id| self.track_cache.get(id))
                            .map(|track| track.name.clone())
                            .unwrap_or_else(|| uri.clone());
                        (uri.clone(), name)
                    })
                    .collect();
                let count = songs_to_queue.len();
                self.actions.push(Action::QueueMany {
                    songs: songs_to_queue,
                });
                let said = format!("Queued {}", songs(count));
                reply_ok(format!("{said}."), Some(said))
            }
            Call::Play { uri } => {
                let name = util::uri_id(&uri)
                    .and_then(|id| self.track_cache.get(id))
                    .map(|track| track.name.clone());
                let kind = util::uri_kind(&uri).unwrap_or("track").to_string();
                if kind == "track" {
                    self.actions.push(Action::PlayUris {
                        uris: vec![uri],
                        index: 0,
                    });
                } else {
                    self.actions.push(Action::PlayContext {
                        uri,
                        offset_uri: None,
                        offset_index: None,
                    });
                }
                let what = name.map_or_else(|| format!("a {kind}"), |name| render::clean(&name));
                reply_ok(format!("Playing {what}."), Some(format!("Playing {what}")))
            }
            Call::Skip => {
                self.actions.push(Action::Next);
                reply_ok("Skipped.", Some("Skipped".into()))
            }
            Call::Previous => {
                self.actions.push(Action::Previous);
                reply_ok("Went back.", Some("Went back".into()))
            }
            Call::Pause => match self.now_playing() {
                Some(now) if now.playing => {
                    self.actions.push(Action::TogglePlay);
                    reply_ok("Paused.", Some("Paused".into()))
                }
                Some(_) => reply_ok("It is already paused.", None),
                None => reply_ok("Nothing is playing.", None),
            },
            Call::Resume => match self.now_playing() {
                Some(now) if !now.playing => {
                    self.actions.push(Action::TogglePlay);
                    reply_ok("Resumed.", Some("Resumed".into()))
                }
                Some(_) => reply_ok("It is already playing.", None),
                None => Err("Nothing to resume. Start something with play.".into()),
            },
            Call::Seek { seconds } => {
                let Some(now) = self.now_playing().filter(|n| !n.resuming) else {
                    return Err("Nothing is playing.".into());
                };
                let ms = (seconds.saturating_mul(1000)).min(now.duration_ms.saturating_sub(1000));
                self.actions.push(Action::Seek(ms));
                let at = format!("Jumped to {}:{:02}", ms / 60_000, (ms / 1000) % 60);
                reply_ok(format!("{at}."), Some(at))
            }
            Call::SetVolume { percent } => {
                if self.now_playing().is_some_and(|n| !n.can_set_volume) {
                    return Err("The playing device does not take volume changes.".into());
                }
                self.actions.push(Action::SetVolume(percent));
                let said = format!("Volume {percent}%");
                reply_ok(format!("{said}."), Some(said))
            }
            Call::SetShuffle { on } => {
                self.actions.push(Action::SetShuffle(on));
                let said = format!("Shuffle {}", if on { "on" } else { "off" });
                reply_ok(format!("{said}."), Some(said))
            }
            Call::SetRepeat { mode } => {
                let (repeat, word) = match mode {
                    RepeatKind::Off => (crate::player::RepeatMode::Off, "off"),
                    RepeatKind::Track => (crate::player::RepeatMode::Track, "one song"),
                    RepeatKind::Context => (crate::player::RepeatMode::Context, "the list"),
                };
                self.actions.push(Action::SetRepeat(repeat));
                let said = format!("Repeat {word}");
                reply_ok(format!("{said}."), Some(said))
            }
            Call::ClearQueue => {
                if !self.can_clear_queue() {
                    return Err(
                        "Nothing queued by hand to clear, or the queue lives on another device \
                         where Spotify does not let apps clear it."
                            .into(),
                    );
                }
                self.actions.push(Action::ClearQueue);
                reply_ok("Cleared the queue.", Some("Cleared the queue".into()))
            }
            Call::RemoveFromQueue { uris } => {
                if !self.queue_locally_reorderable() {
                    return Err(
                        "Songs can only be taken out of the queue while this computer is the \
                         playing device."
                            .into(),
                    );
                }
                let queued = self.queued_rows_len();
                let rows: Vec<(usize, String)> = match self.queue.get() {
                    Some(queue) => uris
                        .iter()
                        .filter_map(|uri| {
                            queue
                                .queue
                                .iter()
                                .take(queued)
                                .position(|item| item.uri() == uri)
                                .map(|index| (index, uri.clone()))
                        })
                        .collect(),
                    None => Vec::new(),
                };
                if rows.is_empty() {
                    return Err(
                        "None of those songs is in the hand-queued part of the queue.".into(),
                    );
                }
                let count = rows.len();
                self.actions.push(Action::RemoveFromQueue { rows });
                let said = format!("Took {} out of the queue", songs(count));
                reply_ok(format!("{said}."), Some(said))
            }
            Call::TransferPlayback { device_id } => {
                let found = self
                    .devices
                    .iter()
                    .find(|d| d.id.as_deref() == Some(device_id.as_str()))
                    .map(|d| d.name.clone());
                let Some(name) = found else {
                    return Err("Unknown device id. Call devices for the list.".into());
                };
                self.actions.push(Action::Transfer(device_id));
                self.actions.push(Action::RefreshDevices);
                let said = format!("Moved playback to {}", render::clean(&name));
                reply_ok(format!("{said}."), Some(said))
            }
            Call::LikeTracks { uris } => {
                let count = uris.len();
                self.actions
                    .push(Action::SetSavedMany { uris, saved: true });
                let said = format!("Liked {}", songs(count));
                reply_ok(format!("{said}."), Some(said))
            }
            Call::UnlikeTracks { uris } => {
                let count = uris.len();
                self.actions
                    .push(Action::SetSavedMany { uris, saved: false });
                let said = format!("Removed {} from Liked Songs", songs(count));
                reply_ok(format!("{said}."), Some(said))
            }
            Call::AddToPlaylist { uri, uris } => {
                let (id, name) = self.dj_editable(&uri)?;
                let tracks = self.dj_known_tracks(&uris)?;
                let count = tracks.len();
                self.actions.push(Action::ConfirmAddToPlaylist {
                    playlist_id: id,
                    playlist_name: name.clone(),
                    items: tracks.into_iter().map(PlayableItem::Track).collect(),
                    position: None,
                });
                let said = format!("Added {} to {}", songs(count), render::clean(&name));
                reply_ok(format!("{said}."), Some(said))
            }
            Call::RemoveFromPlaylist { uri, uris } => {
                let (id, name) = self.dj_editable(&uri)?;
                let count = uris.len();
                self.actions.push(Action::RemoveFromPlaylist {
                    playlist_id: id,
                    uris,
                });
                let said = format!("Removed {} from {}", songs(count), render::clean(&name));
                reply_ok(format!("{said}."), Some(said))
            }
            Call::RenamePlaylist {
                uri,
                name,
                description,
            } => {
                let (id, current) = self.dj_editable(&uri)?;
                let current_description = self
                    .library_entry(&id)
                    .and_then(|playlist| playlist.description.as_deref())
                    .map(util::strip_html)
                    .unwrap_or_default();
                let label = name.clone().unwrap_or_else(|| current.clone());
                self.actions.push(Action::UpdatePlaylist {
                    id,
                    name: name.unwrap_or(current),
                    description: description.unwrap_or(current_description),
                    public: None,
                });
                let said = format!("Updated {}", render::clean(&label));
                reply_ok(format!("{said}."), Some(said))
            }
            Call::Remember { note } => {
                self.dj.memory.remember(&note)?;
                reply_ok(
                    "Remembered.",
                    Some(format!("Noted: {}", render::clean(&note))),
                )
            }
            Call::Forget { note } => {
                let gone = self.dj.memory.forget(&note);
                if gone == 0 {
                    return reply_ok("No note matched that text.", None);
                }
                reply_ok(
                    format!("Forgot {gone} note(s)."),
                    Some("Forgot a note".into()),
                )
            }
            Call::Memory => reply_ok(render::memory(self.dj.memory.notes()), None),
            Call::ScheduleTask { slot, prompt } => {
                let when = agent::slot_text(&slot);
                let id = self.dj.schedules.add(slot, &prompt)?;
                reply_ok(
                    format!("Scheduled task {id}: {when}."),
                    Some(format!("Scheduled: {when}")),
                )
            }
            Call::ListSchedules => {
                let tasks = self.dj.schedules.tasks();
                if tasks.is_empty() {
                    return reply_ok("Nothing is scheduled.", None);
                }
                reply_ok(
                    tasks
                        .iter()
                        .map(|t| {
                            format!(
                                "- #{} {}: {}",
                                t.id,
                                agent::slot_text(&t.slot),
                                render::clean(&t.prompt)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                    None,
                )
            }
            Call::CancelSchedule { id } => {
                if self.dj.schedules.cancel(id) {
                    reply_ok("Cancelled.", Some("Cancelled a scheduled task".into()))
                } else {
                    Err("There is no scheduled task with that number.".into())
                }
            }
            // answered by `dj_run`
            Call::Search { .. }
            | Call::PlaylistTracks { .. }
            | Call::AlbumTracks { .. }
            | Call::ArtistTopTracks { .. }
            | Call::ArtistAlbums { .. }
            | Call::LikedTracks { .. }
            | Call::TrackInfo { .. }
            | Call::TopItems { .. }
            | Call::CreatePlaylist { .. } => Err("Unsupported here.".into()),
        }
    }

    fn dj_read(&mut self, read: DjRead, shape: Shape, reply: Reply) {
        self.dj_search_serial += 1;
        let serial = self.dj_search_serial;
        self.backend.api(ApiRequest::DjRead { read, serial });
        self.dj_state.reads.insert(
            serial,
            DjReadJob {
                reply,
                shape,
                started: Instant::now(),
            },
        );
    }

    /// A read the DJ asked for is back.
    pub(super) fn dj_read_response(
        &mut self,
        serial: u64,
        result: Result<DjData, crate::api::ApiError>,
    ) {
        let Some(job) = self.dj_state.reads.remove(&serial) else {
            // It timed out and was answered already.
            return;
        };
        let data = match result {
            Ok(data) => data,
            Err(error) => {
                let _ = job.reply.send(Err(format!(
                    "Spotify could not answer: {}",
                    render::clean(&error.to_string())
                )));
                return;
            }
        };
        // Songs the DJ saw have names the queue rows can show at once, and can be added to lists.
        let seen: Vec<Track> = match &data {
            DjData::Tracks { tracks, .. } => tracks.clone(),
            DjData::Playlist { items, .. } => items
                .iter()
                .filter_map(|item| match item.playable() {
                    Some(PlayableItem::Track(track)) => Some(track.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        for track in seen {
            if track.is_local || util::uri_kind(&track.uri) != Some("track") {
                continue;
            }
            if let Some(id) = util::uri_id(&track.uri) {
                let id = id.to_string();
                self.remember_track_recording(&track);
                self.track_cache.insert(id, track);
            }
        }
        let text = match (data, job.shape) {
            (
                DjData::Playlist {
                    name,
                    owner,
                    total,
                    items,
                },
                Shape::Playlist { offset },
            ) => render::playlist_rows(&name, &owner, total, &items, offset),
            (DjData::Tracks { tracks, total }, Shape::Tracks { title, offset }) => {
                render::tracks(title, &tracks, total, offset)
            }
            (DjData::Tracks { tracks, .. }, Shape::Info) => render::track_info(&tracks),
            (DjData::Albums(albums), _) => render::albums(&albums),
            (DjData::Artists(artists), _) => render::artists(&artists),
            _ => "Spotify answered with something unexpected.".to_string(),
        };
        let _ = job.reply.send(Ok(ToolReply { text, log: None }));
    }

    fn dj_create_playlist(
        &mut self,
        name: String,
        description: String,
        uris: Vec<String>,
        public: bool,
        reply: Reply,
    ) {
        if self.playlist_busy || self.dj_state.create.is_some() || self.dialog.is_some() {
            let _ = reply.send(Err(
                "Spotifast is busy with another playlist change. Try again in a moment.".into(),
            ));
            return;
        }
        if let Err(error) = self.dj_known_tracks(&uris) {
            let _ = reply.send(Err(error));
            return;
        }
        self.playlist_busy = true;
        self.backend.api(ApiRequest::CreatePlaylist {
            name,
            public,
            description,
        });
        self.dj_state.create = Some(DjCreate {
            reply,
            uris,
            started: Instant::now(),
        });
    }

    /// Spotify answered the DJ's playlist request: list it, fill it, tell the model.
    pub(super) fn dj_playlist_created(
        &mut self,
        result: Result<crate::api::models::Playlist, crate::api::ApiError>,
    ) {
        let Some(job) = self.dj_state.create.take() else {
            return;
        };
        self.playlist_busy = false;
        match result {
            Ok(playlist) => {
                let settled_by = self.library.playlists_generation + 1;
                self.edit_playlists(PlaylistEdit {
                    uri: playlist.uri.clone(),
                    added: Some(playlist.clone()),
                    settled_by: Some(settled_by),
                });
                self.saved.insert(playlist.uri.clone(), true);
                self.store_library_cache();
                let count = job.uris.len();
                if count > 0 {
                    self.backend.api(ApiRequest::AddToPlaylist {
                        playlist_id: playlist.id.clone(),
                        playlist_name: playlist.name.clone(),
                        uris: job.uris,
                        position: None,
                    });
                }
                let said = format!(
                    "Created {} with {}",
                    render::clean(&playlist.name),
                    songs(count)
                );
                let _ = job.reply.send(Ok(ToolReply {
                    text: format!("{said}. | {}", playlist.uri),
                    log: Some(said),
                }));
            }
            Err(error) => {
                let _ = job.reply.send(Err(format!(
                    "Could not create the playlist: {}",
                    render::clean(&error.to_string())
                )));
            }
        }
    }

    fn dj_search(&mut self, query: String, kinds: Vec<Kind>, limit: usize, reply: Reply) {
        self.dj_search_serial += 1;
        let job = self.dj_search_serial;
        let names = |wanted: bool| -> Vec<String> {
            kinds
                .iter()
                .filter(|kind| (**kind == Kind::Playlist) == wanted)
                .map(|kind| kind.as_str().to_string())
                .collect()
        };
        let catalogue = names(false);
        let playlists = names(true);
        let mut waiting = 0;
        // Serial: the job in the high bits, 0 for the catalogue part and 1 for the playlists.
        if !catalogue.is_empty() {
            waiting += 1;
            self.backend.api(ApiRequest::DjSearch {
                kinds: catalogue,
                query: query.clone(),
                serial: job << 1,
            });
        }
        if !playlists.is_empty() {
            waiting += 1;
            self.backend.api(ApiRequest::DjSearch {
                kinds: playlists,
                query,
                serial: (job << 1) | 1,
            });
        }
        self.dj_searches.insert(
            job,
            DjSearch {
                reply,
                kinds,
                limit,
                waiting,
                results: SearchResults::default(),
                error: None,
                started: Instant::now(),
            },
        );
    }

    /// One part of a DJ search is back; the reply goes out when the last one is.
    pub(super) fn dj_search_response(
        &mut self,
        serial: u64,
        result: Result<SearchResults, crate::api::ApiError>,
    ) {
        let job_id = serial >> 1;
        let Some(job) = self.dj_searches.get_mut(&job_id) else {
            // It timed out and was answered already.
            return;
        };
        job.waiting = job.waiting.saturating_sub(1);
        match result {
            Ok(found) => {
                if serial & 1 == 1 {
                    job.results.playlists = found.playlists;
                } else {
                    job.results.tracks = found.tracks;
                    job.results.artists = found.artists;
                    job.results.albums = found.albums;
                }
            }
            Err(error) => job.error = Some(error.to_string()),
        }
        if job.waiting > 0 {
            return;
        }
        let Some(job) = self.dj_searches.remove(&job_id) else {
            return;
        };
        // Songs the DJ found have names the queue rows can show at once.
        let tracks: Vec<Track> = job
            .results
            .tracks
            .iter()
            .flat_map(|page| page.items.iter().cloned())
            .collect();
        for track in tracks {
            if track.is_local || util::uri_kind(&track.uri) != Some("track") {
                continue;
            }
            if let Some(id) = util::uri_id(&track.uri) {
                let id = id.to_string();
                self.remember_track_recording(&track);
                self.track_cache.insert(id, track);
            }
        }
        let outcome = match job.error {
            Some(error) if job.results == SearchResults::default() => {
                Err(format!("The search failed: {}", render::clean(&error)))
            }
            _ => Ok(ToolReply {
                text: render::search(&job.results, &job.kinds, job.limit),
                log: None,
            }),
        };
        let _ = job.reply.send(outcome);
    }

    // ---- the DJ's own turns --------------------------------------------------------------------

    /// Watches the app for things worth waking the DJ for, and runs due scheduled tasks. A turn of
    /// the DJ's own never starts while the listener's message is being answered.
    fn dj_background(&mut self) {
        if self.user_id().is_none() {
            return;
        }
        let now = Instant::now();
        if self.settings.dj_autopilot {
            self.dj_watch(now);
        } else {
            self.dj.feed.clear();
        }
        if self
            .dj_state
            .schedule_checked
            .is_none_or(|at| now.duration_since(at) >= SCHEDULE_EVERY)
        {
            self.dj_state.schedule_checked = Some(now);
            for task in self.dj.schedules.due(&Moment::now()) {
                self.dj_state.due.push_back(task.prompt);
            }
        }
        if self.dj.busy || !self.dj.ready() {
            return;
        }
        let model = if self.settings.dj_background_model.is_empty() {
            self.settings.dj_model.clone()
        } else {
            self.settings.dj_background_model.clone()
        };
        let effort = self.settings.dj_effort.clone();
        if let Some(prompt) = self.dj_state.due.pop_front() {
            let prompt = format!(
                "[scheduled] The listener scheduled this task. It is not a message from them \
                 right now. Do it now: {prompt}"
            );
            self.dj.ask_unprompted(&prompt, &model, &effort);
        } else if self.settings.dj_autopilot
            && let Some(prompt) = self.dj.feed.take(now, false)
        {
            self.dj.ask_unprompted(&prompt, &model, &effort);
        }
    }

    /// Turns what happened in the app since the last frame into events for the feed.
    fn dj_watch(&mut self, now: Instant) {
        let playing = self.now_playing().filter(|n| !n.resuming && !n.is_episode);
        // the song changed: was the one before left early?
        let current = playing
            .as_ref()
            .map(|n| (n.uri.clone(), n.title.clone(), n.position_ms, n.duration_ms));
        if let Some((uri, title, position, duration)) = current {
            match self.dj_state.track.take() {
                Some((last_uri, last_title, last_position, last_duration)) if last_uri != uri => {
                    if last_position < SKIP_WITHIN_MS && last_duration > 60_000 {
                        self.dj_state.skips.push(now);
                        self.dj_state
                            .skips
                            .retain(|at| now.duration_since(*at) < SKIP_STREAK);
                        let streak = self.dj_state.skips.len() >= 2;
                        self.dj.feed.note(
                            format!(
                                "The listener skipped \"{}\" after {} seconds.{}",
                                render::clean(&last_title),
                                last_position / 1000,
                                if streak {
                                    " That is the second skip in a row: the picks may be off."
                                } else {
                                    ""
                                }
                            ),
                            streak,
                        );
                    } else {
                        self.dj
                            .feed
                            .note(format!("Now playing \"{}\".", render::clean(&title)), false);
                    }
                }
                _ => {}
            }
            self.dj_state.track = Some((uri, title, position, duration));
        } else {
            self.dj_state.track = None;
        }
        // the queue ran low
        let upcoming = self.queue.get().map(|queue| queue.queue.len());
        if let (Some(len), true) = (upcoming, playing.is_some()) {
            let was = self.dj_state.queue_len.replace(len);
            if len <= QUEUE_LOW && was.is_none_or(|was| was > QUEUE_LOW) {
                self.dj.feed.note(
                    format!("The queue is running low: {} upcoming.", songs(len)),
                    true,
                );
            }
        }
    }
}
