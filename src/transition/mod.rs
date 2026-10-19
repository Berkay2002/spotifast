//! Mix and Automix transitions: the end of one track overlapped with the
//! start of the next, each shaped by volume, three-band EQ and filter
//! curves.
//!
//! librespot's player decodes the next track alongside the current one
//! once it is preloaded and asks [`Planner`] whether, where and how to
//! overlap them. The planner answers from what it already holds: the
//! settings, Spotify's analysis of both tracks (beat grid, tempo,
//! mixability) and the current playlist's Mix recipes, which [`run`]
//! fetches as the queue moves.

pub mod curve;
pub mod dsp;
pub mod meta;
pub mod preset;
pub mod recipe;
pub mod schedule;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use librespot_core::{Session, SpotifyUri};
use librespot_playback::player::PlayerEvent;
use librespot_playback::transition::{TransitionMix, TransitionPlan, Transitions};
use librespot_protocol::extended_metadata::{BatchedEntityRequest, EntityRequest, ExtensionQuery};
use protobuf::{EnumOrUnknown, Message as _};
use tokio::sync::mpsc::UnboundedReceiver;

pub use meta::TrackMix;
use recipe::Recipe;
pub use schedule::Mode;

/// The analysis a transition uses: tempo and key, the beat grid, and both
/// kinds of mixability.
const KINDS: [i32; 4] = [
    meta::KIND_AUDIO_ATTRIBUTES,
    meta::KIND_BEATS,
    meta::KIND_MIXABILITY,
    meta::KIND_MIXABILITY_TRAIT,
];
/// Tracks whose analysis is kept; the queue only ever needs two.
const KEPT_TRACKS: usize = 32;
/// The next track is preloaded this long before the end when transitions
/// are on, so the longest overlap can start on time.
const TRANSITION_PRELOAD_MS: u32 = schedule::LATEST_START_BEFORE_END_MS + 5_000;
/// A recipe fetch that has not answered, or failed, is asked again after
/// this long, on the next queue update.
const RECIPES_RETRY: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    pub mode: Mode,
    pub crossfade_secs: u32,
    /// Play a Mix playlist's stored transitions, in playlist order, in
    /// place of the mode's own. Off stays off.
    pub mix_recipes: bool,
}

impl Settings {
    fn recipes(&self) -> bool {
        self.mix_recipes && self.mode != Mode::Off
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: Mode::Off,
            crossfade_secs: 6,
            mix_recipes: true,
        }
    }
}

/// Spotify's analysis of tracks, by URI. `uris` may be any length; the
/// request is one round trip. Tracks Spotify has nothing for come back with
/// an empty [`TrackMix`].
pub async fn analyse(
    session: &Session,
    uris: &[String],
) -> anyhow::Result<HashMap<String, TrackMix>> {
    let mut request = BatchedEntityRequest::new();
    for uri in uris {
        request.entity_request.push(EntityRequest {
            entity_uri: uri.clone(),
            query: KINDS
                .iter()
                .map(|kind| ExtensionQuery {
                    extension_kind: EnumOrUnknown::from_i32(*kind),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        });
    }
    let response = session.spclient().get_extended_metadata(request).await?;
    let mut tracks: HashMap<String, TrackMix> = uris
        .iter()
        .map(|uri| (uri.clone(), TrackMix::default()))
        .collect();
    for array in &response.extended_metadata {
        let kind = array.extension_kind.value();
        for data in &array.extension_data {
            let (Some(track), Some(payload)) = (
                tracks.get_mut(&data.entity_uri),
                data.extension_data.as_ref(),
            ) else {
                continue;
            };
            if let Err(error) = track.apply(kind, &payload.value) {
                log::debug!(
                    "unreadable mixing analysis {kind} for {}: {error}",
                    data.entity_uri
                );
            }
        }
    }
    Ok(tracks)
}

/// The Mix recipes stored in a playlist's items.
pub async fn mix_recipes(session: &Session, playlist_uri: &str) -> anyhow::Result<Vec<Recipe>> {
    let SpotifyUri::Playlist { id, .. } = SpotifyUri::from_uri(playlist_uri)? else {
        anyhow::bail!("not a playlist: {playlist_uri}");
    };
    let bytes = session.spclient().get_playlist(&id).await?;
    let list =
        librespot_protocol::playlist4_external::SelectedListContent::parse_from_bytes(&bytes)?;
    let mut recipes = Vec::new();
    for item in list.contents.iter().flat_map(|contents| &contents.items) {
        for attribute in &item.attributes.format_attributes {
            if attribute.key() != recipe::ATTRIBUTE {
                continue;
            }
            match Recipe::from_attribute(attribute.value()) {
                Ok(recipe) if recipe.overlap.is_some() => recipes.push(recipe),
                Ok(_) => {}
                Err(error) => log::debug!("unreadable Mix recipe: {error}"),
            }
        }
    }
    Ok(recipes)
}

fn beat_hash(mix: Option<&TrackMix>) -> Option<&str> {
    mix.and_then(|mix| mix.beats.as_ref())
        .map(|beats| beats.hash.as_str())
        .filter(|hash| !hash.is_empty())
}

enum Analysis {
    Pending,
    Ready(TrackMix),
    Missing,
}

/// The last pair the player asked about, so asking again for every packet
/// costs a comparison.
struct Planned {
    a: SpotifyUri,
    b: SpotifyUri,
    a_ms: u32,
    b_ms: u32,
    settings: Settings,
    generation: u64,
    choice: Option<(TransitionPlan, dsp::Shape)>,
}

#[derive(Default)]
struct State {
    tracks: HashMap<String, Analysis>,
    /// The playing playlist's recipes, by the pair each was made for.
    context: String,
    recipes: HashMap<(String, String), Recipe>,
    /// Whether the playing context's recipes arrived, and when they were
    /// last asked for, so turning recipes on or a failed fetch asks again.
    recipes_loaded: bool,
    recipes_asked: Option<Instant>,
    /// Bumped whenever analysis or recipes arrive, so a plan made without
    /// them is made again.
    generation: u64,
    planned: Option<Planned>,
}

pub struct Planner {
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

impl std::fmt::Debug for Planner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Planner")
            .field("settings", &self.settings())
            .finish_non_exhaustive()
    }
}

impl Planner {
    pub fn new(settings: Settings) -> Arc<Self> {
        Arc::new(Self {
            settings: Mutex::new(settings),
            state: Mutex::default(),
        })
    }

    pub fn settings(&self) -> Settings {
        *self.settings.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn set_settings(&self, settings: Settings) {
        *self.settings.lock().unwrap_or_else(PoisonError::into_inner) = settings;
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Marks the tracks not analysed yet as pending and returns them.
    fn claim(&self, uris: &[String]) -> Vec<String> {
        let mut state = self.state();
        if state.tracks.len() > KEPT_TRACKS {
            state.tracks.retain(|uri, _| uris.contains(uri));
        }
        uris.iter()
            .filter(|uri| uri.starts_with("spotify:track:") && !state.tracks.contains_key(*uri))
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .inspect(|uri| {
                state.tracks.insert(uri.clone(), Analysis::Pending);
            })
            .collect()
    }

    fn analysed(&self, uris: &[String], result: anyhow::Result<HashMap<String, TrackMix>>) {
        let mut state = self.state();
        match result {
            Ok(mut tracks) => {
                for uri in uris {
                    let analysis = tracks
                        .remove(uri)
                        .map_or(Analysis::Missing, Analysis::Ready);
                    state.tracks.insert(uri.clone(), analysis);
                }
            }
            Err(error) => {
                log::info!("no mixing analysis: {error:#}");
                for uri in uris {
                    state.tracks.insert(uri.clone(), Analysis::Missing);
                }
            }
        }
        state.generation += 1;
    }

    /// Notes the playing context; returns whether its recipes should be
    /// fetched now: wanted, not loaded, and not asked for recently.
    fn enter_context(&self, context: &str) -> bool {
        let mut state = self.state();
        if state.context != context {
            state.context = context.to_string();
            state.recipes.clear();
            state.recipes_loaded = false;
            state.recipes_asked = None;
            state.generation += 1;
        }
        let fetch = self.settings().recipes()
            && context.starts_with("spotify:playlist:")
            && !state.recipes_loaded
            && state
                .recipes_asked
                .is_none_or(|asked| asked.elapsed() >= RECIPES_RETRY);
        if fetch {
            state.recipes_asked = Some(Instant::now());
        }
        fetch
    }

    fn found_recipes(&self, context: &str, recipes: Vec<Recipe>) {
        let mut state = self.state();
        if state.context != context {
            return;
        }
        log::info!("{} Mix transitions in the playing playlist", recipes.len());
        state.recipes_loaded = true;
        state.recipes = recipes
            .into_iter()
            .filter_map(|recipe| {
                let overlap = recipe.overlap.as_ref()?;
                Some((
                    (overlap.track_a_uri.clone(), overlap.track_b_uri.clone()),
                    recipe,
                ))
            })
            .collect();
        state.generation += 1;
    }

    fn choose(
        state: &State,
        settings: Settings,
        a: &SpotifyUri,
        a_ms: u32,
        b: &SpotifyUri,
        b_ms: u32,
    ) -> Option<(TransitionPlan, dsp::Shape)> {
        let (a_uri, b_uri) = (a.to_uri(), b.to_uri());
        let mix = |uri: &str| match state.tracks.get(uri) {
            Some(Analysis::Ready(mix)) => Some(mix),
            _ => None,
        };
        let (a_mix, b_mix) = (mix(&a_uri), mix(&b_uri));
        let recipe = settings
            .recipes()
            .then(|| state.recipes.get(&(a_uri.clone(), b_uri.clone())))
            .flatten()
            .filter(|recipe| recipe.matches(&a_uri, &b_uri, beat_hash(a_mix), beat_hash(b_mix)));
        if recipe.is_none()
            && settings.recipes()
            && state.recipes.contains_key(&(a_uri.clone(), b_uri.clone()))
        {
            log::info!("Mix transition recipe mismatch for this pair; falling back to the setting");
        }
        let config = schedule::Config {
            mode: settings.mode,
            crossfade_ms: settings.crossfade_secs.saturating_mul(1000),
        };
        let choice = schedule::choose(
            config,
            &schedule::Track {
                duration_ms: a_ms,
                mix: a_mix,
            },
            &schedule::Track {
                duration_ms: b_ms,
                mix: b_mix,
            },
            recipe,
        )?;
        let shape = match (choice.from_recipe, recipe) {
            (true, Some(recipe)) => choice.preset.shape_with(&recipe.overrides),
            _ => choice.preset.shape(),
        };
        log::info!("transition planned: {:?} {:?}", choice.preset, choice.plan);
        Some((choice.plan, shape))
    }
}

impl Transitions for Planner {
    fn preload_before_end_ms(&self) -> u32 {
        if self.settings().mode == Mode::Off {
            0
        } else {
            TRANSITION_PRELOAD_MS
        }
    }

    fn plan(&self, a: &SpotifyUri, a_ms: u32, b: &SpotifyUri, b_ms: u32) -> Option<TransitionPlan> {
        let settings = self.settings();
        let mut state = self.state();
        let generation = state.generation;
        if let Some(planned) = &state.planned
            && planned.a == *a
            && planned.b == *b
            && planned.a_ms == a_ms
            && planned.b_ms == b_ms
            && planned.settings == settings
            && planned.generation == generation
        {
            return planned.choice.as_ref().map(|(plan, _)| *plan);
        }
        let choice = Self::choose(&state, settings, a, a_ms, b, b_ms);
        let plan = choice.as_ref().map(|(plan, _)| *plan);
        state.planned = Some(Planned {
            a: a.clone(),
            b: b.clone(),
            a_ms,
            b_ms,
            settings,
            generation,
            choice,
        });
        plan
    }

    fn begin(
        &self,
        a: &SpotifyUri,
        b: &SpotifyUri,
        plan: &TransitionPlan,
    ) -> Box<dyn TransitionMix> {
        let state = self.state();
        let shape = state
            .planned
            .as_ref()
            .filter(|planned| planned.a == *a && planned.b == *b)
            .and_then(|planned| planned.choice.as_ref())
            .filter(|(planned, _)| planned == plan)
            .map(|(_, shape)| shape.clone())
            .unwrap_or_else(|| preset::Preset::Crossfade.shape());
        Box::new(dsp::Mixer::new(shape))
    }
}

/// Keeps the planner's analysis and recipes current as the queue moves.
pub async fn run(
    session: Session,
    mut events: UnboundedReceiver<PlayerEvent>,
    planner: Arc<Planner>,
) {
    while let Some(event) = events.recv().await {
        // Off asks Spotify for nothing.
        if planner.settings().mode == Mode::Off {
            continue;
        }
        let mut wanted = Vec::new();
        match event {
            PlayerEvent::SetQueue {
                context_uri,
                current_track,
                next_tracks,
                ..
            } => {
                wanted.extend(current_track.map(|track| track.uri));
                wanted.extend(next_tracks.into_iter().next().map(|track| track.uri));
                if planner.enter_context(&context_uri) {
                    let (session, planner) = (session.clone(), Arc::clone(&planner));
                    tokio::spawn(async move {
                        match mix_recipes(&session, &context_uri).await {
                            Ok(recipes) => planner.found_recipes(&context_uri, recipes),
                            Err(error) => log::info!("no Mix recipes: {error:#}"),
                        }
                    });
                }
            }
            PlayerEvent::Preloading { track_id } => wanted.push(track_id.to_uri()),
            PlayerEvent::TrackChanged { audio_item } => wanted.push(audio_item.track_id.to_uri()),
            _ => continue,
        }
        let claimed = planner.claim(&wanted);
        if claimed.is_empty() {
            continue;
        }
        let (session, planner) = (session.clone(), Arc::clone(&planner));
        tokio::spawn(async move {
            let result = analyse(&session, &claimed).await;
            planner.analysed(&claimed, result);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::recipe::tests::{A, B, fixture};
    use super::*;

    fn uri(uri: &str) -> SpotifyUri {
        SpotifyUri::from_uri(uri).unwrap()
    }

    fn planner(mode: Mode, mix_recipes: bool) -> Arc<Planner> {
        Planner::new(Settings {
            mode,
            crossfade_secs: 6,
            mix_recipes,
        })
    }

    fn with_recipe(planner: &Planner) {
        assert!(planner.enter_context("spotify:playlist:2222222222222222222222"));
        planner.found_recipes(
            "spotify:playlist:2222222222222222222222",
            vec![Recipe::from_attribute(&fixture()).unwrap()],
        );
    }

    #[test]
    fn off_plays_gaplessly_and_crossfade_overlaps_the_ends() {
        let (a, b) = (uri(A), uri(B));
        let off = planner(Mode::Off, true);
        assert_eq!(off.plan(&a, 200_000, &b, 200_000), None);
        assert_eq!(off.preload_before_end_ms(), 0);
        let crossfade = planner(Mode::Crossfade, true);
        let plan = crossfade.plan(&a, 200_000, &b, 200_000).unwrap();
        assert_eq!(
            (plan.start_a_ms, plan.start_b_ms, plan.duration_ms),
            (194_000, 0, 6_000)
        );
        assert_eq!(crossfade.preload_before_end_ms(), TRANSITION_PRELOAD_MS);
    }

    #[test]
    fn recipes_are_fetched_once_turned_on_and_again_after_a_failed_fetch() {
        const P: &str = "spotify:playlist:2222222222222222222222";
        let planner = planner(Mode::Crossfade, false);
        assert!(!planner.enter_context(P));
        planner.set_settings(Settings {
            mix_recipes: true,
            ..planner.settings()
        });
        // Turned on in the same playlist, the next queue update fetches.
        assert!(planner.enter_context(P));
        // A fetch in flight is not repeated...
        assert!(!planner.enter_context(P));
        // ...but one that failed is asked again after a while.
        planner.state().recipes_asked = Instant::now().checked_sub(RECIPES_RETRY);
        assert!(planner.enter_context(P));
        planner.found_recipes(P, Vec::new());
        planner.state().recipes_asked = Instant::now().checked_sub(RECIPES_RETRY);
        assert!(!planner.enter_context(P));
        assert!(!planner.enter_context("spotify:album:3333333333333333333333"));
        assert!(planner.enter_context(P));
    }

    #[test]
    fn a_stored_recipe_wins_in_playlist_order_but_off_stays_off() {
        let (a, b) = (uri(A), uri(B));
        let off = planner(Mode::Off, true);
        assert!(!off.enter_context("spotify:playlist:2222222222222222222222"));
        off.found_recipes(
            "spotify:playlist:2222222222222222222222",
            vec![Recipe::from_attribute(&fixture()).unwrap()],
        );
        assert_eq!(off.preload_before_end_ms(), 0);
        assert_eq!(off.plan(&a, 220_000, &b, 240_000), None);
        let planner = planner(Mode::Crossfade, true);
        with_recipe(&planner);
        let plan = planner.plan(&a, 220_000, &b, 240_000).unwrap();
        assert_eq!(
            (plan.start_a_ms, plan.start_b_ms, plan.duration_ms),
            (180_000, 20_000, 16_000)
        );
        // Played the other way round, the recipe does not apply.
        assert_eq!(
            planner.plan(&b, 220_000, &a, 240_000).unwrap().duration_ms,
            6_000
        );
        // Turning recipes off falls back to the mode.
        planner.set_settings(Settings {
            mix_recipes: false,
            ..planner.settings()
        });
        assert_eq!(
            planner.plan(&a, 220_000, &b, 240_000).unwrap().duration_ms,
            6_000
        );
    }

    #[test]
    fn a_reanalysed_track_drops_its_recipe() {
        let (a, b) = (uri(A), uri(B));
        let planner = planner(Mode::Crossfade, true);
        with_recipe(&planner);
        let changed = TrackMix {
            beats: Some(meta::Beats {
                beats_per_bar: 4,
                beats: Vec::new(),
                hash: "reanalysed".into(),
            }),
            ..TrackMix::default()
        };
        planner.analysed(
            &[A.to_string()],
            Ok(HashMap::from([(A.to_string(), changed)])),
        );
        let plan = planner.plan(&a, 220_000, &b, 240_000).unwrap();
        assert_eq!(plan.duration_ms, 6_000, "falls back to the crossfade");
    }

    #[test]
    fn asking_again_reuses_the_plan_until_something_changes() {
        let (a, b) = (uri(A), uri(B));
        let planner = planner(Mode::Crossfade, true);
        let first = planner.plan(&a, 200_000, &b, 200_000);
        let generation = planner.state().generation;
        assert_eq!(planner.plan(&a, 200_000, &b, 200_000), first);
        assert_eq!(planner.state().generation, generation);
        planner.set_settings(Settings {
            crossfade_secs: 10,
            ..planner.settings()
        });
        assert_eq!(
            planner.plan(&a, 200_000, &b, 200_000).unwrap().duration_ms,
            10_000
        );
        let mix = planner.begin(&a, &b, &planner.plan(&a, 200_000, &b, 200_000).unwrap());
        drop(mix);
    }

    #[test]
    fn only_new_tracks_are_claimed_for_analysis() {
        let planner = planner(Mode::Automix, true);
        let wanted = [
            A.to_string(),
            B.to_string(),
            "spotify:episode:3333333333333333333333".to_string(),
        ];
        assert_eq!(planner.claim(&wanted), vec![A.to_string(), B.to_string()]);
        assert!(planner.claim(&wanted).is_empty());
    }
}
