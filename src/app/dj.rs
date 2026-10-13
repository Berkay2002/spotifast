//! The AI DJ's side of the app: it answers the tool calls the DJ's model makes.
//!
//! A call arrives from the tool server's thread already validated (`crate::dj::tools`). It is
//! turned into what a click would do: the same actions, the same optimistic queue rows, the same
//! backend requests, so the DJ can never do anything the listener could not do by hand.

use super::*;
use crate::api::models::SearchResults;
use crate::dj::render;
use crate::dj::session::PendingCall;
use crate::dj::tools::{Call, Kind, ToolReply};
use std::sync::mpsc::Sender;

/// How long a search may take before the model is told it failed.
const SEARCH_WAIT: Duration = Duration::from_secs(25);

/// A search call waiting on the backend. A search with playlists in it is two requests (the
/// catalogue and the playlist search are separate operations), merged when both are back.
pub(super) struct DjSearch {
    reply: Sender<Result<ToolReply, String>>,
    kinds: Vec<Kind>,
    limit: usize,
    waiting: u8,
    results: SearchResults,
    error: Option<String>,
    started: Instant,
}

impl App {
    /// Once a frame: applies what the DJ's turn reported and answers its tool calls.
    pub(super) fn handle_dj(&mut self) {
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
        if !self.settings.dj_enabled && self.dj.busy {
            self.dj.shutdown();
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
            _ => {}
        }
    }

    fn dj_ask(&mut self, text: &str) {
        let model = self.settings.dj_model.clone();
        let effort = self.settings.dj_effort.clone();
        self.dj.ask(text, &model, &effort);
    }

    fn dj_call(&mut self, pending: PendingCall) {
        let PendingCall { call, reply } = pending;
        let answer = |text: String, log: Option<String>| Ok(ToolReply { text, log });
        match call {
            Call::Search {
                query,
                kinds,
                limit,
            } => {
                self.dj_search(query, kinds, limit, reply);
                return;
            }
            Call::NowPlaying => {
                let _ = reply.send(answer(
                    render::now_playing(self.now_playing().as_ref()),
                    None,
                ));
            }
            Call::Queue => {
                if self.queue.get().is_none() {
                    // Refreshing is asynchronous: say so rather than claim an empty queue.
                    self.refresh_queue(true);
                    let _ = reply.send(answer(
                        "The queue is still loading. Call queue again in a moment.".into(),
                        None,
                    ));
                    return;
                }
                let text = match self.queue.get() {
                    Some(queue) => {
                        render::queue(queue.currently_playing.as_ref(), &queue.queue, 40)
                    }
                    None => String::new(),
                };
                let _ = reply.send(answer(text, None));
            }
            Call::RecentPlays { limit } => {
                let _ = reply.send(answer(render::recent(&self.recents_view, limit), None));
            }
            Call::LibrarySummary => {
                let none: Vec<crate::api::models::Artist> = Vec::new();
                let artists = self.home.top_artists.get().unwrap_or(&none);
                let songs = self
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
                let _ = reply.send(answer(render::library(artists, songs, lists), None));
            }
            Call::QueueTracks { uris } => {
                let songs: Vec<(String, String)> = uris
                    .iter()
                    .map(|uri| {
                        let name = util::uri_id(uri)
                            .and_then(|id| self.track_cache.get(id))
                            .map(|track| track.name.clone())
                            .unwrap_or_else(|| uri.clone());
                        (uri.clone(), name)
                    })
                    .collect();
                let count = songs.len();
                self.actions.push(Action::QueueMany { songs });
                let noun = if count == 1 { "song" } else { "songs" };
                let _ = reply.send(answer(
                    format!("Queued {count} {noun}."),
                    Some(format!("Queued {count} {noun}")),
                ));
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
                let _ = reply.send(answer(
                    format!("Playing {what}."),
                    Some(format!("Playing {what}")),
                ));
            }
            Call::Skip => {
                self.actions.push(Action::Next);
                let _ = reply.send(answer("Skipped.".into(), Some("Skipped".into())));
            }
        }
    }

    fn dj_search(
        &mut self,
        query: String,
        kinds: Vec<Kind>,
        limit: usize,
        reply: Sender<Result<ToolReply, String>>,
    ) {
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
        let tracks: Vec<crate::api::models::Track> = job
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
}
