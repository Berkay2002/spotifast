//! Spotifast's own transition styles. They follow the ideas DJs use (a
//! crossfade, swapping the bass at the middle, sweeping a filter), drawn
//! from scratch over the overlap's normalised time.

use super::curve::Curve;
use super::dsp::{Filter, FilterKind, Shape, TrackShape};
use super::recipe::Overrides;

/// A full cut of a band, in dB.
const KILL_DB: f32 = -26.0;
/// How far Blend lowers mids and highs, in dB.
const DIP_DB: f32 = -12.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    /// Equal-power volume crossfade, nothing else.
    Crossfade,
    /// Crossfade with the bass swapped at the middle.
    Fade,
    /// Bands handed over one at a time: highs first, then mids, then bass.
    Blend,
    /// Bass swap; the outgoing track thins out through a rising high-pass
    /// while the incoming one opens from a low-pass.
    Rise,
    /// Bass swap; the incoming track melts in through a falling high-pass.
    Melt,
    /// Bass swap; the incoming track opens through a resonant low-pass.
    Wave,
    /// A hard cut at the middle.
    Slam,
}

impl Preset {
    /// The style for a Mix recipe's preset id, or its preset type when the
    /// id is one Spotifast does not draw, and a fade when neither is known.
    /// `None` is Spotify's NONE: no transition at all.
    pub fn from_recipe(id: i32, kind: i32) -> Option<Self> {
        Some(match id {
            0 => return None,
            1 => Self::Fade,
            2 => Self::Blend,
            3 => Self::Rise,
            4 => Self::Melt,
            11 => Self::Slam,
            17 => Self::Wave,
            _ => match kind {
                1 | 3 => Self::Fade,
                2 | 4 => Self::Slam,
                _ => Self::Fade,
            },
        })
    }

    pub fn shape(self) -> Shape {
        let mut shape = Shape {
            outgoing: TrackShape::volume(equal_power_out()),
            incoming: TrackShape::volume(equal_power_in()),
        };
        match self {
            Self::Crossfade => {}
            Self::Fade => bass_swap(&mut shape),
            Self::Blend => {
                shape.outgoing.volume = hold_then_fall(0.5);
                shape.incoming.volume = rise_then_hold(0.5);
                shape.incoming.eq = [
                    Some(step(KILL_DB, 0.0, 0.5)),
                    Some(ramp(DIP_DB, 0.0, 0.25, 0.75)),
                    Some(ramp(DIP_DB, 0.0, 0.0, 0.5)),
                ];
                shape.outgoing.eq = [
                    Some(step(0.0, KILL_DB, 0.5)),
                    Some(ramp(0.0, DIP_DB, 0.25, 0.75)),
                    Some(ramp(0.0, DIP_DB, 0.5, 1.0)),
                ];
            }
            Self::Rise => {
                dj_volumes(&mut shape);
                bass_swap(&mut shape);
                shape.outgoing.filter = Some(Filter {
                    kind: FilterKind::HighPass,
                    cutoff: Curve::piece(
                        0.5,
                        1.0,
                        &[
                            (0.0, 20.0),
                            (1.0 / 3.0, 60.0),
                            (2.0 / 3.0, 400.0),
                            (1.0, 1_200.0),
                        ],
                    ),
                    resonance: Some(Curve::linear(&[(0.0, 0.707), (0.5, 0.707), (1.0, 1.2)])),
                });
                shape.incoming.filter = Some(Filter {
                    kind: FilterKind::LowPass,
                    cutoff: Curve::piece(
                        0.0,
                        0.5,
                        &[
                            (0.0, 400.0),
                            (1.0 / 3.0, 1_500.0),
                            (2.0 / 3.0, 9_000.0),
                            (1.0, 20_000.0),
                        ],
                    ),
                    resonance: None,
                });
            }
            Self::Melt => {
                dj_volumes(&mut shape);
                bass_swap(&mut shape);
                shape.incoming.filter = Some(Filter {
                    kind: FilterKind::HighPass,
                    cutoff: Curve::piece(
                        0.0,
                        0.6,
                        &[
                            (0.0, 1_500.0),
                            (1.0 / 3.0, 400.0),
                            (2.0 / 3.0, 60.0),
                            (1.0, 20.0),
                        ],
                    ),
                    resonance: None,
                });
            }
            Self::Wave => {
                dj_volumes(&mut shape);
                bass_swap(&mut shape);
                shape.incoming.filter = Some(Filter {
                    kind: FilterKind::LowPass,
                    cutoff: Curve::piece(
                        0.0,
                        0.75,
                        &[
                            (0.0, 150.0),
                            (1.0 / 3.0, 500.0),
                            (2.0 / 3.0, 4_000.0),
                            (1.0, 20_000.0),
                        ],
                    ),
                    resonance: Some(Curve::linear(&[
                        (0.0, 0.707),
                        (0.4, 2.2),
                        (0.75, 0.707),
                        (1.0, 0.707),
                    ])),
                });
            }
            Self::Slam => {
                shape.outgoing.volume = step(1.0, 0.0, 0.5);
                shape.incoming.volume = step(0.0, 1.0, 0.5);
            }
        }
        shape
    }

    /// The preset's shape with a recipe's own curves in place of its.
    pub fn shape_with(self, overrides: &Overrides) -> Shape {
        let mut shape = self.shape();
        apply(
            &mut shape.outgoing,
            overrides.volume_out.as_ref(),
            &overrides.eq_out,
            overrides.cutoff_out.as_ref(),
            overrides.resonance_out.as_ref(),
        );
        apply(
            &mut shape.incoming,
            overrides.volume_in.as_ref(),
            &overrides.eq_in,
            overrides.cutoff_in.as_ref(),
            overrides.resonance_in.as_ref(),
        );
        shape
    }
}

fn apply(
    track: &mut TrackShape,
    volume: Option<&Curve>,
    eq: &[Option<Curve>; 3],
    cutoff: Option<&Curve>,
    resonance: Option<&Curve>,
) {
    if let Some(volume) = volume {
        track.volume = volume.clone();
    }
    for (band, curve) in track.eq.iter_mut().zip(eq) {
        if let Some(curve) = curve {
            *band = Some(curve.clone());
        }
    }
    if let Some(cutoff) = cutoff {
        // A recipe names the cutoff, not the filter type: keep the preset's
        // type, or else take a curve that reaches the top for a low-pass.
        let kind = track
            .filter
            .as_ref()
            .map(|filter| filter.kind)
            .unwrap_or_else(|| {
                let top = (0..=20)
                    .map(|i| cutoff.eval(i as f32 / 20.0))
                    .fold(0.0, f32::max);
                if top >= 10_000.0 {
                    FilterKind::LowPass
                } else {
                    FilterKind::HighPass
                }
            });
        track.filter = Some(Filter {
            kind,
            cutoff: cutoff.clone(),
            resonance: resonance.cloned(),
        });
    } else if let (Some(filter), Some(resonance)) = (&mut track.filter, resonance) {
        filter.resonance = Some(resonance.clone());
    }
}

/// cos and sin quarter waves, so the summed power stays level.
fn equal_power_out() -> Curve {
    Curve::linear(&quarter_wave(|x| (x * std::f32::consts::FRAC_PI_2).cos()))
}

fn equal_power_in() -> Curve {
    Curve::linear(&quarter_wave(|x| (x * std::f32::consts::FRAC_PI_2).sin()))
}

fn quarter_wave(f: impl Fn(f32) -> f32) -> [(f32, f32); 9] {
    std::array::from_fn(|i| {
        let x = i as f32 / 8.0;
        (x, f(x))
    })
}

/// The outgoing track holds full volume until `at`, then fades out.
fn hold_then_fall(at: f32) -> Curve {
    Curve::piece(0.0, at, &[(0.0, 1.0), (1.0, 1.0)]).then(
        at,
        1.0,
        &[(0.0, 1.0), (1.0 / 3.0, 0.9), (2.0 / 3.0, 0.5), (1.0, 0.0)],
    )
}

/// The incoming track fades in until `at`, then holds full volume.
fn rise_then_hold(at: f32) -> Curve {
    Curve::piece(
        0.0,
        at,
        &[(0.0, 0.0), (1.0 / 3.0, 0.5), (2.0 / 3.0, 0.9), (1.0, 1.0)],
    )
    .then(at, 1.0, &[(0.0, 1.0), (1.0, 1.0)])
}

/// A DJ's volumes: the incoming track comes up over the first quarter, the
/// outgoing one goes down over the last.
fn dj_volumes(shape: &mut Shape) {
    shape.outgoing.volume = hold_then_fall(0.75);
    shape.incoming.volume = rise_then_hold(0.25);
}

/// From `from` to `to` at `at`.
fn step(from: f32, to: f32, at: f32) -> Curve {
    Curve::linear(&[(0.0, from), (at, from), (at, to), (1.0, to)])
}

/// From `from` to `to` in a straight line between `start` and `end`.
fn ramp(from: f32, to: f32, start: f32, end: f32) -> Curve {
    Curve::linear(&[(0.0, from), (start, from), (end, to), (1.0, to)])
}

fn bass_swap(shape: &mut Shape) {
    shape.outgoing.eq[0] = Some(step(0.0, KILL_DB, 0.5));
    shape.incoming.eq[0] = Some(step(KILL_DB, 0.0, 0.5));
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Preset; 7] = [
        Preset::Crossfade,
        Preset::Fade,
        Preset::Blend,
        Preset::Rise,
        Preset::Melt,
        Preset::Wave,
        Preset::Slam,
    ];

    #[test]
    fn every_preset_starts_on_the_outgoing_track_and_ends_on_the_incoming_one() {
        for preset in ALL {
            let shape = preset.shape();
            assert!(
                (shape.outgoing.volume.eval(0.0) - 1.0).abs() < 1e-4,
                "{preset:?}"
            );
            assert!(shape.incoming.volume.eval(0.0).abs() < 1e-4, "{preset:?}");
            assert!(shape.outgoing.volume.eval(1.0).abs() < 1e-4, "{preset:?}");
            assert!(
                (shape.incoming.volume.eval(1.0) - 1.0).abs() < 1e-4,
                "{preset:?}"
            );
            // The incoming track ends untouched, as it plays on alone.
            for band in shape.incoming.eq.iter().flatten() {
                assert!(band.eval(1.0).abs() < 1e-4, "{preset:?}");
            }
            if let Some(filter) = &shape.incoming.filter {
                let end = filter.cutoff.eval(1.0);
                match filter.kind {
                    FilterKind::LowPass => assert!(end >= 19_000.0, "{preset:?}"),
                    FilterKind::HighPass => assert!(end <= 25.0, "{preset:?}"),
                }
            }
        }
    }

    #[test]
    fn crossfade_keeps_the_summed_power_level() {
        let shape = Preset::Crossfade.shape();
        for i in 0..=20 {
            let t = i as f32 / 20.0;
            let power =
                shape.outgoing.volume.eval(t).powi(2) + shape.incoming.volume.eval(t).powi(2);
            assert!((power - 1.0).abs() < 0.03, "{t}: {power}");
        }
    }

    #[test]
    fn bass_swaps_at_the_middle_so_only_one_bass_plays() {
        for preset in [
            Preset::Fade,
            Preset::Rise,
            Preset::Melt,
            Preset::Wave,
            Preset::Blend,
        ] {
            let shape = preset.shape();
            let out = shape.outgoing.eq[0].as_ref().unwrap();
            let incoming = shape.incoming.eq[0].as_ref().unwrap();
            assert_eq!(out.eval(0.49), 0.0, "{preset:?}");
            assert_eq!(incoming.eval(0.49), KILL_DB, "{preset:?}");
            assert_eq!(out.eval(0.51), KILL_DB, "{preset:?}");
            assert_eq!(incoming.eval(0.51), 0.0, "{preset:?}");
        }
    }

    #[test]
    fn recipe_ids_map_to_styles() {
        assert_eq!(Preset::from_recipe(0, 1), None);
        assert_eq!(Preset::from_recipe(3, 3), Some(Preset::Rise));
        assert_eq!(Preset::from_recipe(17, 3), Some(Preset::Wave));
        assert_eq!(Preset::from_recipe(99, 2), Some(Preset::Slam));
        assert_eq!(Preset::from_recipe(99, 0), Some(Preset::Fade));
    }

    #[test]
    fn a_recipe_curve_replaces_the_presets() {
        let overrides = Overrides {
            volume_in: Some(Curve::constant(0.5)),
            ..Overrides::default()
        };
        let shape = Preset::Fade.shape_with(&overrides);
        assert_eq!(shape.incoming.volume.eval(0.3), 0.5);
        assert_eq!(shape.outgoing.volume, Preset::Fade.shape().outgoing.volume);
    }
}
