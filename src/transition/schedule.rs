//! Where an overlap goes: a Mix recipe's stored overlap, a downbeat-aligned
//! mix of two tracks at matching tempo, or a crossfade at the end.

use librespot_playback::transition::TransitionPlan;

use super::meta::TrackMix;
use super::preset::Preset;
use super::recipe::Recipe;

/// Longest overlap, and the most of either track it may take.
const MAX_OVERLAP_MS: u32 = 35_000;
const MAX_SHARE: f64 = 0.2;
/// Tempos further apart than this are not beat-mixed (there is no time
/// stretching, so the beats drift apart by this much over the overlap).
pub const MAX_TEMPO_GAP: f64 = 0.03;
/// Automix's crossfade when two tracks cannot be beat-mixed.
const AUTOMIX_FALLBACK_MS: u32 = 6_000;
/// Downbeats this close to the end are part of the fade-out, not music to
/// mix on.
const END_MARGIN_MS: f32 = 250.0;
/// The earliest an overlap may start, before the end of the outgoing track.
/// The next track is preloaded a little before this, so an overlap starting
/// earlier would find nothing to mix.
pub const LATEST_START_BEFORE_END_MS: u32 = 45_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Off,
    Crossfade,
    Automix,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub mode: Mode,
    pub crossfade_ms: u32,
}

pub struct Track<'a> {
    pub duration_ms: u32,
    pub mix: Option<&'a TrackMix>,
}

/// The chosen overlap and the style that shapes it.
#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    pub plan: TransitionPlan,
    pub preset: Preset,
    /// The recipe the overlap came from, whose curve overrides apply.
    pub from_recipe: bool,
}

/// The transition from `a` into `b`, or `None` for plain (gapless) playback.
/// A recipe, already checked to match the pair, wins over the mode.
pub fn choose(config: Config, a: &Track, b: &Track, recipe: Option<&Recipe>) -> Option<Choice> {
    if let Some(recipe) = recipe {
        return from_recipe(config, a, b, recipe);
    }
    match config.mode {
        Mode::Off => None,
        Mode::Crossfade => crossfade(a, b, config.crossfade_ms).map(|plan| Choice {
            plan,
            preset: Preset::Crossfade,
            from_recipe: false,
        }),
        Mode::Automix => automix(a, b),
    }
}

fn from_recipe(config: Config, a: &Track, b: &Track, recipe: &Recipe) -> Option<Choice> {
    let preset = Preset::from_recipe(recipe.preset_id, recipe.preset_type)?;
    let stored = recipe
        .overlap
        .as_ref()
        .filter(|_| recipe.is_overlap_override)
        .and_then(|overlap| {
            let duration = u32::try_from(overlap.duration_ms)
                .ok()
                .filter(|ms| *ms > 0)?;
            let start_a = u32::try_from(overlap.start_a_ms).ok()?;
            // Spotify lets B start before its own beginning; it cannot here.
            let start_b = u32::try_from(overlap.start_b_ms).unwrap_or(0);
            let limit = (f64::from(a.duration_ms.min(b.duration_ms)) * MAX_SHARE) as u32;
            let duration = duration.min(limit).min(MAX_OVERLAP_MS);
            let fits = start_a.checked_add(duration)? <= a.duration_ms
                && start_b.checked_add(duration)? <= b.duration_ms;
            let in_time = a.duration_ms - start_a.min(a.duration_ms) <= LATEST_START_BEFORE_END_MS;
            if !fits || !in_time {
                log::info!(
                    "Mix transition at {start_a} ms does not fit this {} ms track; mixing automatically",
                    a.duration_ms
                );
            }
            (fits && in_time).then_some(TransitionPlan {
                start_a_ms: start_a,
                start_b_ms: start_b,
                duration_ms: duration,
            })
        });
    let plan = match stored {
        Some(plan) => plan,
        None => match automix(a, b) {
            Some(choice) => choice.plan,
            None => crossfade(a, b, config.crossfade_ms.max(AUTOMIX_FALLBACK_MS))?,
        },
    };
    Some(Choice {
        plan,
        preset,
        from_recipe: true,
    })
}

/// A crossfade over the last `length_ms` of `a` and the start of `b`.
pub fn crossfade(a: &Track, b: &Track, length_ms: u32) -> Option<TransitionPlan> {
    let limit = (f64::from(a.duration_ms.min(b.duration_ms)) * MAX_SHARE) as u32;
    let duration = length_ms.min(limit).min(MAX_OVERLAP_MS);
    (duration >= 500).then(|| TransitionPlan {
        start_a_ms: a.duration_ms - duration,
        start_b_ms: 0,
        duration_ms: duration,
    })
}

/// Beat-mixed when both tracks allow it and their tempos match, else a
/// short crossfade.
pub fn automix(a: &Track, b: &Track) -> Option<Choice> {
    if let (Some(a_mix), Some(b_mix)) = (a.mix, b.mix)
        && let Some(plan) = beatmatched(a_mix, a.duration_ms, b_mix, b.duration_ms)
    {
        return Some(Choice {
            plan,
            preset: Preset::Rise,
            from_recipe: false,
        });
    }
    // TODO: detect beats locally when Spotify has no grid for a track.
    crossfade(a, b, AUTOMIX_FALLBACK_MS).map(|plan| Choice {
        plan,
        preset: Preset::Crossfade,
        from_recipe: false,
    })
}

/// How far apart two tempos are as a fraction, allowing for one grid being
/// counted at half or double time.
pub fn tempo_gap(a: f64, b: f64) -> f64 {
    [0.5, 1.0, 2.0]
        .into_iter()
        .map(|factor| (b * factor / a - 1.0).abs())
        .fold(f64::INFINITY, f64::min)
}

/// An 8-bar overlap (4 when 8 is too long) ending on `a`'s last downbeat,
/// with the middle downbeats of both tracks on top of each other. Without
/// time stretching the beats drift apart by the tempo gap, so aligning the
/// middle, where the bass swaps, halves the drift at either edge.
pub fn beatmatched(a: &TrackMix, a_ms: u32, b: &TrackMix, b_ms: u32) -> Option<TransitionPlan> {
    if !a.can_beatmix() || !b.can_beatmix() {
        return None;
    }
    if tempo_gap(a.tempo()?, b.tempo()?) >= MAX_TEMPO_GAP {
        return None;
    }
    let ms = |seconds: f32| seconds * 1000.0;
    let a_downbeats: Vec<f32> = a.beats.as_ref()?.downbeats().map(ms).collect();
    let b_downbeats: Vec<f32> = b.beats.as_ref()?.downbeats().map(ms).collect();
    let last = a_downbeats
        .iter()
        .rposition(|time| *time <= a_ms as f32 - END_MARGIN_MS)?;
    let limit = (MAX_OVERLAP_MS as f32)
        .min(a_ms as f32 * MAX_SHARE as f32)
        .min(b_ms as f32 * MAX_SHARE as f32);
    for bars in [8, 4] {
        let Some(first) = last.checked_sub(bars) else {
            continue;
        };
        let start_a = a_downbeats[first];
        let duration = a_downbeats[last] - start_a;
        if duration > limit || duration <= 0.0 {
            continue;
        }
        let into_middle = a_downbeats[first + bars / 2] - start_a;
        // B's earliest downbeat that can sit at A's middle downbeat.
        let Some(start_b) = b_downbeats
            .iter()
            .skip(bars / 2)
            .map(|middle| middle - into_middle)
            .find(|start| *start >= 0.0)
        else {
            continue;
        };
        if start_b + duration > b_ms as f32 {
            continue;
        }
        return Some(TransitionPlan {
            start_a_ms: start_a.round() as u32,
            start_b_ms: start_b.round() as u32,
            duration_ms: duration.round() as u32,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transition::meta::{Beat, Beats};

    /// A steady grid at `bpm` from `offset` seconds, four beats a bar.
    fn grid(bpm: f32, offset: f32, seconds: f32) -> TrackMix {
        let beat = 60.0 / bpm;
        let count = ((seconds - offset) / beat) as usize;
        TrackMix {
            mixable: Some(true),
            allow_mixing: Some(true),
            beats: Some(Beats {
                beats_per_bar: 4,
                beats: (0..count)
                    .map(|i| Beat {
                        time: offset + i as f32 * beat,
                        duration: 0.0,
                        value: (i % 4) as u32 + 1,
                    })
                    .collect(),
                hash: "h".into(),
            }),
            ..TrackMix::default()
        }
    }

    fn track(ms: u32, mix: Option<&TrackMix>) -> Track<'_> {
        Track {
            duration_ms: ms,
            mix,
        }
    }

    #[test]
    fn matching_tempos_get_an_eight_bar_mix_aligned_on_the_middle_downbeat() {
        let a = grid(120.0, 0.3, 200.0);
        let b = grid(121.0, 1.1, 180.0);
        let plan = beatmatched(&a, 200_000, &b, 180_000).unwrap();
        // 8 bars at 120 bpm.
        assert!((plan.duration_ms as i32 - 16_000).abs() <= 1);
        // The overlap starts and ends on A's downbeats.
        let bar = 2_000.0;
        assert!(((plan.start_a_ms as f32 - 300.0) / bar).fract().abs() < 0.001);
        assert!(plan.start_a_ms + plan.duration_ms <= 200_000 - 250);
        // B's fifth downbeat (4 bars in) sits on A's middle downbeat.
        let b_bar = 4.0 * 60.0 / 121.0 * 1000.0;
        let b_middle = 1_100.0 + 4.0 * b_bar;
        assert!(
            (plan.start_b_ms as f32 + 8_000.0 - b_middle).abs() < 2.0,
            "{plan:?}"
        );
    }

    #[test]
    fn a_tempo_gap_of_three_percent_or_more_is_not_beat_mixed() {
        let a = grid(120.0, 0.0, 200.0);
        assert!(beatmatched(&a, 200_000, &grid(123.7, 0.0, 200.0), 200_000).is_none());
        assert!(beatmatched(&a, 200_000, &grid(122.0, 0.0, 200.0), 200_000).is_some());
        // A grid counted at double time still matches.
        assert!(beatmatched(&a, 200_000, &grid(240.0, 0.0, 200.0), 200_000).is_some());
        assert!((tempo_gap(120.0, 60.5) - 0.0083).abs() < 0.001);
    }

    #[test]
    fn unmixable_tracks_get_a_crossfade_instead() {
        let a = grid(120.0, 0.0, 200.0);
        let mut b = grid(120.0, 0.0, 200.0);
        b.mixable = Some(false);
        let choice = automix(&track(200_000, Some(&a)), &track(200_000, Some(&b))).unwrap();
        assert_eq!(choice.preset, Preset::Crossfade);
        assert_eq!(
            choice.plan,
            TransitionPlan {
                start_a_ms: 194_000,
                start_b_ms: 0,
                duration_ms: 6_000
            }
        );
        // Rights that forbid mixing count as unmixable.
        b.mixable = Some(true);
        b.allow_mixing = Some(false);
        assert_eq!(
            automix(&track(200_000, Some(&a)), &track(200_000, Some(&b)))
                .unwrap()
                .preset,
            Preset::Crossfade
        );
    }

    #[test]
    fn slow_tracks_fall_back_to_four_bars_and_short_ones_to_a_crossfade() {
        // 8 bars at 60 bpm is 32 s, over 20 % of a 150 s track.
        let a = grid(60.0, 0.0, 150.0);
        let plan = beatmatched(&a, 150_000, &grid(60.0, 0.0, 150.0), 150_000).unwrap();
        assert_eq!(plan.duration_ms, 16_000);
        assert!(beatmatched(&a, 150_000, &grid(60.0, 0.0, 40.0), 40_000).is_none());
    }

    #[test]
    fn crossfades_take_at_most_a_fifth_of_either_track() {
        let plan = crossfade(&track(20_000, None), &track(200_000, None), 12_000).unwrap();
        assert_eq!(plan.duration_ms, 4_000);
        assert_eq!(plan.start_a_ms, 16_000);
        assert!(crossfade(&track(1_000, None), &track(200_000, None), 12_000).is_none());
    }

    #[test]
    fn a_stored_overlap_wins_and_preset_none_means_no_transition() {
        let config = Config {
            mode: Mode::Off,
            crossfade_ms: 6_000,
        };
        let mut recipe = Recipe {
            overlap: Some(crate::transition::recipe::Overlap {
                start_a_ms: 180_000,
                start_b_ms: 2_000,
                duration_ms: 10_000,
                ..Default::default()
            }),
            preset_id: 17,
            preset_type: 3,
            is_overlap_override: true,
            ..Recipe::default()
        };
        let a = track(200_000, None);
        let b = track(200_000, None);
        let choice = choose(config, &a, &b, Some(&recipe)).unwrap();
        assert_eq!(choice.preset, Preset::Wave);
        assert_eq!(
            choice.plan,
            TransitionPlan {
                start_a_ms: 180_000,
                start_b_ms: 2_000,
                duration_ms: 10_000
            }
        );
        // An overlap that runs past A's end is recomputed.
        recipe.overlap.as_mut().unwrap().start_a_ms = 195_000;
        assert_eq!(
            choose(config, &a, &b, Some(&recipe))
                .unwrap()
                .plan
                .start_a_ms,
            194_000
        );
        // One starting before the next track is preloaded is too.
        recipe.overlap.as_mut().unwrap().start_a_ms = 150_000;
        assert_eq!(
            choose(config, &a, &b, Some(&recipe))
                .unwrap()
                .plan
                .start_a_ms,
            194_000
        );
        // A long one is shortened to the share of either track it may take.
        recipe.overlap.as_mut().unwrap().start_a_ms = 160_000;
        recipe.overlap.as_mut().unwrap().duration_ms = 60_000;
        assert_eq!(
            choose(config, &a, &b, Some(&recipe))
                .unwrap()
                .plan
                .duration_ms,
            35_000
        );
        recipe.preset_id = 0;
        assert!(choose(config, &a, &b, Some(&recipe)).is_none());
        // With no recipe, Off is off.
        assert!(choose(config, &a, &b, None).is_none());
    }
}
