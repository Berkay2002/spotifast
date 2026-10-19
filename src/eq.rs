//! Ten-band equalizer for local playback.
//!
//! Each Winamp band uses a second-order peaking filter. Bands and preamp range
//! from -12 to +12 dB. The UI writes settings behind a mutex; the player reads
//! them once per packet and retunes the filters only after changes, keeping
//! their history so a change mid-song does not click.
//!
//! This stage does not clip boosted samples. `vis::Tapped` limits the signal
//! later, after accounting for output volume.

use std::sync::{Arc, Mutex};

use librespot_playback::{NUM_CHANNELS, SAMPLE_RATE};

/// The centre frequencies, Winamp's, in hertz.
pub const BANDS: [f32; 10] = [
    60.0, 170.0, 310.0, 600.0, 1000.0, 3000.0, 6000.0, 12000.0, 14000.0, 16000.0,
];
/// How far a band goes either way, in decibels.
pub const RANGE_DB: f32 = 12.0;
/// Bands closer to flat than this are skipped rather than run for nothing.
const FLAT: f32 = 0.05;
/// How far the solved gains may go past the sliders while making the
/// combined response meet them; a guard, not a target.
const SOLVED_LIMIT: f64 = 36.0;

/// Equalizer, preamp, balance, and mono settings.
///
/// Balance and mono apply even when the equalizer is off.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EqSettings {
    pub on: bool,
    /// Twelve decibels either way, like the bands.
    pub preamp_db: f32,
    pub bands_db: [f32; 10],
    /// -1 is all left, 1 all right.
    pub balance: f32,
    pub mono: bool,
}

impl Default for EqSettings {
    fn default() -> Self {
        Self {
            on: false,
            preamp_db: 0.0,
            bands_db: [0.0; 10],
            balance: 0.0,
            mono: false,
        }
    }
}

impl EqSettings {
    /// The same settings kept within what the equalizer can do.
    pub fn clamped(mut self) -> Self {
        self.preamp_db = self.preamp_db.clamp(-RANGE_DB, RANGE_DB);
        for band in &mut self.bands_db {
            *band = band.clamp(-RANGE_DB, RANGE_DB);
        }
        self.balance = self.balance.clamp(-1.0, 1.0);
        self
    }

    /// Per-channel balance gains. The quieter side is attenuated.
    pub fn channel_gains(&self) -> [f64; 2] {
        let balance = f64::from(self.balance);
        [(1.0 - balance).min(1.0), (1.0 + balance).min(1.0)]
    }

    /// Filter response used for playback and the curve display.
    pub fn curve(&self) -> Curve {
        Curve {
            preamp_db: self.preamp_db,
            filters: chain(&self.bands_db),
        }
    }

    /// The response at one frequency, in decibels, with the switch on.
    pub fn response_db(&self, hz: f32) -> f32 {
        self.curve().db_at(hz)
    }
}

/// Equalizer response by frequency.
pub struct Curve {
    preamp_db: f32,
    filters: Vec<Biquad>,
}

impl Curve {
    pub fn db_at(&self, hz: f32) -> f32 {
        let mut db = f64::from(self.preamp_db);
        for filter in &self.filters {
            db += filter.gain_db_at(f64::from(hz));
        }
        db as f32
    }
}

/// Band widths in octaves, based on the midpoint between adjacent bands.
///
/// The bands are unevenly spaced. A shared width makes the high bands overlap
/// and overboost presets such as Full Treble. The outer bands extend three
/// octaves below 60 Hz and one octave above 16 kHz, matching Winamp.
fn band_widths() -> [f64; 10] {
    let octaves: Vec<f64> = BANDS.iter().map(|hz| f64::from(*hz).log2()).collect();
    let mut widths = [0.0; 10];
    for (index, width) in widths.iter_mut().enumerate() {
        let below = index
            .checked_sub(1)
            .map_or(octaves[index] - 3.0, |i| octaves[i]);
        let above = octaves
            .get(index + 1)
            .copied()
            .unwrap_or(octaves[index] + 1.0);
        *width = (octaves[index] - below) / 2.0 + (above - octaves[index]) / 2.0;
    }
    widths
}

/// Builds filters whose combined response matches each slider at its centre.
fn chain(bands_db: &[f32; 10]) -> Vec<Biquad> {
    solved_gains(bands_db).map_or_else(Vec::new, |gains| {
        filters_for(&gains, &band_widths())
            .into_iter()
            .map(|(_, filter)| filter)
            .collect()
    })
}

/// One filter for every band of `solved_gains`, with a band at flat left at
/// 0 dB rather than out, so the player can retune them in place.
fn fixed_chain(gains: &[f64; 10]) -> [Biquad; 10] {
    let widths = band_widths();
    std::array::from_fn(|index| {
        let gain = if gains[index].abs() > f64::from(FLAT) {
            gains[index]
        } else {
            0.0
        };
        Biquad::peaking(f64::from(BANDS[index]), widths[index], gain)
    })
}

/// Each band's own gain, so that the combined response matches each slider
/// at its centre, or `None` when every slider is flat. Adjacent filters
/// overlap, so their individual gains are solved together.
fn solved_gains(bands_db: &[f32; 10]) -> Option<[f64; 10]> {
    let widths = band_widths();
    let target: [f64; 10] = bands_db.map(f64::from);
    if target.iter().all(|gain| gain.abs() <= f64::from(FLAT)) {
        return None;
    }
    // How much each band moves every centre, per decibel it is given.
    let mut unit = [[0.0; 10]; 10];
    for (j, (hz, width)) in BANDS.iter().zip(widths).enumerate() {
        let filter = Biquad::peaking(f64::from(*hz), width, 1.0);
        for (i, centre) in BANDS.iter().enumerate() {
            unit[i][j] = filter.gain_db_at(f64::from(*centre));
        }
    }
    let mut gains = target;
    for _ in 0..6 {
        let filters: Vec<(usize, Biquad)> = filters_for(&gains, &widths);
        let mut residual = [0.0; 10];
        for (i, centre) in BANDS.iter().enumerate() {
            let played: f64 = filters
                .iter()
                .map(|(_, filter)| filter.gain_db_at(f64::from(*centre)))
                .sum();
            residual[i] = played - target[i];
        }
        if residual.iter().all(|r| r.abs() < 0.01) {
            break;
        }
        let correction = solve(unit, residual);
        for (gain, step) in gains.iter_mut().zip(correction) {
            *gain = (*gain - step).clamp(-SOLVED_LIMIT, SOLVED_LIMIT);
        }
    }
    Some(gains)
}

/// The bands worth running, with the filter for each.
fn filters_for(gains: &[f64; 10], widths: &[f64; 10]) -> Vec<(usize, Biquad)> {
    BANDS
        .iter()
        .zip(widths)
        .zip(gains)
        .enumerate()
        .filter(|(_, (_, gain))| gain.abs() > f64::from(FLAT))
        .map(|(index, ((hz, width), gain))| (index, Biquad::peaking(f64::from(*hz), *width, *gain)))
        .collect()
}

/// Gaussian elimination with partial pivoting, for the ten-by-ten
/// interaction of the bands.
fn solve(mut matrix: [[f64; 10]; 10], mut rhs: [f64; 10]) -> [f64; 10] {
    let n = 10;
    for column in 0..n {
        let pivot = (column..n)
            .max_by(|a, b| {
                matrix[*a][column]
                    .abs()
                    .total_cmp(&matrix[*b][column].abs())
            })
            .unwrap_or(column);
        matrix.swap(column, pivot);
        rhs.swap(column, pivot);
        let lead = matrix[column][column];
        if lead.abs() < 1e-12 {
            continue;
        }
        let pivot_row = matrix[column];
        let pivot_rhs = rhs[column];
        for row in column + 1..n {
            let factor = matrix[row][column] / lead;
            if factor == 0.0 {
                continue;
            }
            for (cell, above) in matrix[row][column..].iter_mut().zip(&pivot_row[column..]) {
                *cell -= factor * above;
            }
            rhs[row] -= factor * pivot_rhs;
        }
    }
    let mut solution = [0.0; 10];
    for row in (0..n).rev() {
        let mut sum = rhs[row];
        for k in row + 1..n {
            sum -= matrix[row][k] * solution[k];
        }
        let lead = matrix[row][row];
        solution[row] = if lead.abs() < 1e-12 { 0.0 } else { sum / lead };
    }
    solution
}

/// A named set of band gains, as Winamp shipped them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Preset {
    pub name: &'static str,
    pub bands_db: [f32; 10],
}

/// Winamp's presets, in its order.
/// How many of `PRESETS` are Winamp's own, in its order; what follows
/// are scenario presets of this app's, shown behind a separator.
pub const WINAMP_PRESET_COUNT: usize = 18;

pub const PRESETS: &[Preset] = &[
    Preset {
        name: "Flat",
        bands_db: [0.0; 10],
    },
    Preset {
        name: "Classical",
        bands_db: [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, -7.2, -7.2, -7.2, -9.6],
    },
    Preset {
        name: "Club",
        bands_db: [0.0, 0.0, 8.0, 5.6, 5.6, 5.6, 3.2, 0.0, 0.0, 0.0],
    },
    Preset {
        name: "Dance",
        bands_db: [9.6, 7.2, 2.4, 0.0, 0.0, -5.6, -7.2, -7.2, 0.0, 0.0],
    },
    Preset {
        name: "Full Bass",
        bands_db: [-8.0, 9.6, 9.6, 5.6, 1.6, -4.0, -8.0, -10.4, -11.2, -11.2],
    },
    Preset {
        name: "Full Bass & Treble",
        bands_db: [7.2, 5.6, 0.0, -7.2, -4.8, 1.6, 8.0, 11.2, 12.0, 12.0],
    },
    Preset {
        name: "Full Treble",
        bands_db: [-9.6, -9.6, -9.6, -4.0, 2.4, 11.2, 12.0, 12.0, 12.0, 12.0],
    },
    Preset {
        name: "Laptop Speakers / Headphones",
        bands_db: [4.8, 11.2, 5.6, -3.2, -2.4, 1.6, 4.8, 9.6, 12.0, 12.0],
    },
    Preset {
        name: "Large Hall",
        bands_db: [10.4, 10.4, 5.6, 5.6, 0.0, -4.8, -4.8, -4.8, 0.0, 0.0],
    },
    Preset {
        name: "Live",
        bands_db: [-4.8, 0.0, 4.0, 5.6, 5.6, 5.6, 4.0, 2.4, 2.4, 2.4],
    },
    Preset {
        name: "Party",
        bands_db: [7.2, 7.2, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 7.2, 7.2],
    },
    Preset {
        name: "Pop",
        bands_db: [-1.6, 4.8, 7.2, 8.0, 5.6, 0.0, -2.4, -2.4, -1.6, -1.6],
    },
    Preset {
        name: "Reggae",
        bands_db: [0.0, 0.0, 0.0, -5.6, 0.0, 6.4, 6.4, 0.0, 0.0, 0.0],
    },
    Preset {
        name: "Rock",
        bands_db: [8.0, 4.8, -5.6, -8.0, -3.2, 4.0, 8.8, 11.2, 11.2, 11.2],
    },
    Preset {
        name: "Ska",
        bands_db: [-2.4, -4.8, -4.0, 0.0, 4.0, 5.6, 8.8, 9.6, 11.2, 9.6],
    },
    Preset {
        name: "Soft",
        bands_db: [4.8, 1.6, 0.0, -2.4, 0.0, 4.0, 8.0, 9.6, 11.2, 12.0],
    },
    Preset {
        name: "Soft Rock",
        bands_db: [4.0, 4.0, 2.4, 0.0, -4.0, -5.6, -3.2, 0.0, 2.4, 8.8],
    },
    Preset {
        name: "Techno",
        bands_db: [8.0, 5.6, 0.0, -5.6, -4.8, 0.0, 8.0, 9.6, 9.6, 8.8],
    },
    Preset {
        name: "Bass Booster",
        bands_db: [8.8, 7.2, 5.6, 3.2, 0.8, 0.0, 0.0, 0.0, 0.0, 0.0],
    },
    Preset {
        name: "Bass Reducer",
        bands_db: [-8.8, -7.2, -5.6, -3.2, -0.8, 0.0, 0.0, 0.0, 0.0, 0.0],
    },
    Preset {
        name: "Treble Booster",
        bands_db: [0.0, 0.0, 0.0, 0.0, 0.0, 0.8, 3.2, 5.6, 7.2, 8.8],
    },
    Preset {
        name: "Vocal Booster",
        bands_db: [-2.4, -4.8, -4.8, 1.6, 5.6, 5.6, 4.0, 1.6, 0.0, -2.4],
    },
    Preset {
        name: "Small Speakers",
        bands_db: [-8.0, -6.4, -4.0, -1.6, 1.6, 3.2, 4.8, 5.6, 5.6, 5.6],
    },
    Preset {
        name: "Spoken Word",
        bands_db: [-3.2, -0.8, 0.0, 0.8, 4.0, 5.6, 4.8, 2.4, 0.8, 0.0],
    },
    Preset {
        name: "Loudness",
        bands_db: [9.6, 6.4, 0.0, 0.0, -2.4, 0.0, -1.6, 0.0, 8.0, 1.6],
    },
    Preset {
        name: "Night Listening",
        bands_db: [-4.8, -3.2, -1.6, 0.8, 2.4, 3.2, 2.4, 0.8, -1.6, -3.2],
    },
];

/// The settings as the window and the player's thread share them.
pub type SharedEq = Arc<Mutex<EqSettings>>;

pub fn shared() -> SharedEq {
    Arc::new(Mutex::new(EqSettings::default()))
}

/// A second-order section in direct form I, one per band and channel.
/// Direct form I also keeps a filter whose coefficients move every few
/// samples (the transitions' sweeps) free of zipper noise.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl Biquad {
    /// A peaking filter after the Audio EQ Cookbook, `width` octaves wide
    /// between its half-gain points and normalised by `a0`. The width is
    /// taken the cookbook's digital way, with its `w0 / sin(w0)` term: near
    /// the top of the band the plain analog Q would have come out three
    /// times too narrow, which is what rippled the treble.
    fn peaking(hz: f64, width: f64, gain_db: f64) -> Self {
        let a = 10f64.powf(gain_db / 40.0);
        let w0 = std::f64::consts::TAU * hz / f64::from(SAMPLE_RATE);
        let (sin, cos) = w0.sin_cos();
        let alpha = sin * ((std::f64::consts::LN_2 / 2.0) * width * w0 / sin).sinh();
        let a0 = 1.0 + alpha / a;
        Self {
            b0: (1.0 + alpha * a) / a0,
            b1: (-2.0 * cos) / a0,
            b2: (1.0 - alpha * a) / a0,
            a1: (-2.0 * cos) / a0,
            a2: (1.0 - alpha / a) / a0,
            ..Self::default()
        }
    }

    /// The filter's gain at a frequency, in decibels, from its transfer
    /// function on the unit circle: what is played, including the bend
    /// the bilinear transform puts near the top of the band.
    fn gain_db_at(&self, hz: f64) -> f64 {
        let w = std::f64::consts::TAU * hz / f64::from(SAMPLE_RATE);
        let (sin1, cos1) = w.sin_cos();
        let (sin2, cos2) = (2.0 * w).sin_cos();
        let numerator = (self.b0 + self.b1 * cos1 + self.b2 * cos2).powi(2)
            + (self.b1 * sin1 + self.b2 * sin2).powi(2);
        let denominator = (1.0 + self.a1 * cos1 + self.a2 * cos2).powi(2)
            + (self.a1 * sin1 + self.a2 * sin2).powi(2);
        10.0 * (numerator / denominator).log10()
    }

    /// A cookbook low-pass at `hz` with quality `q`.
    pub(crate) fn lowpass(hz: f64, q: f64) -> Self {
        let w0 = std::f64::consts::TAU * hz / f64::from(SAMPLE_RATE);
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let a0 = 1.0 + alpha;
        Self {
            b0: (1.0 - cos) / 2.0 / a0,
            b1: (1.0 - cos) / a0,
            b2: (1.0 - cos) / 2.0 / a0,
            a1: (-2.0 * cos) / a0,
            a2: (1.0 - alpha) / a0,
            ..Self::default()
        }
    }

    /// A cookbook high-pass at `hz` with quality `q`.
    pub(crate) fn highpass(hz: f64, q: f64) -> Self {
        let w0 = std::f64::consts::TAU * hz / f64::from(SAMPLE_RATE);
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let a0 = 1.0 + alpha;
        Self {
            b0: (1.0 + cos) / 2.0 / a0,
            b1: -(1.0 + cos) / a0,
            b2: (1.0 + cos) / 2.0 / a0,
            a1: (-2.0 * cos) / a0,
            a2: (1.0 - alpha) / a0,
            ..Self::default()
        }
    }

    /// Takes `other`'s coefficients and keeps this filter's history.
    pub(crate) fn retune(&mut self, other: Self) {
        self.b0 = other.b0;
        self.b1 = other.b1;
        self.b2 = other.b2;
        self.a1 = other.a1;
        self.a2 = other.a2;
    }

    #[inline]
    pub(crate) fn run(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// The filters on the player's thread, retuned when the settings change.
pub struct Processor {
    shared: SharedEq,
    applied: EqSettings,
    /// One chain per channel, one filter per band. A change retunes them
    /// and keeps their history: fresh filters would restart from silence
    /// partway through the waveform, and that step is a click.
    chains: Vec<[Biquad; 10]>,
    /// Every band is flat, so only the preamp shapes.
    flat: bool,
    gain: f64,
}

impl Processor {
    pub fn new(shared: SharedEq) -> Self {
        // Nothing has played yet, so there is nothing to fade from.
        let applied = shared
            .lock()
            .map(|settings| settings.clamped())
            .unwrap_or_default();
        let mut processor = Self {
            shared,
            applied,
            chains: vec![fixed_chain(&[0.0; 10]); NUM_CHANNELS as usize],
            flat: true,
            gain: 1.0,
        };
        processor.retune();
        processor
    }

    fn retune(&mut self) {
        let settings = self.applied;
        self.gain = 10f64.powf(f64::from(settings.preamp_db) / 20.0);
        let gains = solved_gains(&settings.bands_db);
        self.flat = gains.is_none();
        let tuned = fixed_chain(&gains.unwrap_or([0.0; 10]));
        for chain in &mut self.chains {
            for (filter, tuned) in chain.iter_mut().zip(tuned) {
                filter.retune(tuned);
            }
        }
    }

    fn shaping(&self) -> bool {
        self.applied.on && !(self.flat && self.gain == 1.0)
    }

    /// Discards the previous signal without changing the filter settings.
    pub fn reset(&mut self) {
        for filter in self.chains.iter_mut().flatten() {
            filter.x1 = 0.0;
            filter.x2 = 0.0;
            filter.y1 = 0.0;
            filter.y2 = 0.0;
        }
    }

    /// Runs interleaved stereo samples through the equalizer, in place.
    pub fn process(&mut self, samples: &mut [f64]) {
        let wanted = self
            .shared
            .lock()
            .map(|settings| settings.clamped())
            .unwrap_or(self.applied);
        let was_shaping = self.shaping();
        let old_gain = self.gain;
        if wanted != self.applied {
            self.applied = wanted;
            self.retune();
        }
        let shaping = self.shaping();
        if shaping && !was_shaping {
            // The filters sat idle; what they last heard is long gone.
            self.reset();
        }
        let gains = self.applied.channel_gains();
        let placing = self.applied.mono || gains != [1.0, 1.0];
        if !shaping && !was_shaping && !placing {
            return;
        }
        let channels = self.chains.len();
        let frames = (samples.len() / channels).max(1) as f64;
        let (from, to) = (
            f64::from(u8::from(was_shaping)),
            f64::from(u8::from(shaping)),
        );
        for (index, frame) in samples.chunks_exact_mut(channels).enumerate() {
            if shaping || was_shaping {
                // Switching on or off and a preamp change fade over the
                // packet instead of stepping at its start.
                let along = (index + 1) as f64 / frames;
                let wet = from + (to - from) * along;
                let gain = old_gain + (self.gain - old_gain) * along;
                for (sample, chain) in frame.iter_mut().zip(self.chains.iter_mut()) {
                    let mut y = *sample * gain;
                    for filter in chain.iter_mut() {
                        y = filter.run(y);
                    }
                    *sample = *sample * (1.0 - wet) + y * wet;
                }
            }
            if self.applied.mono && frame.len() == 2 {
                let middle = (frame[0] + frame[1]) / 2.0;
                frame[0] = middle;
                frame[1] = middle;
            }
            for (sample, gain) in frame.iter_mut().zip(gains) {
                // No ceiling here. These are floats with room to spare, and
                // the one ceiling in the chain sits at the end, past the
                // volume: a boost that would clip at full volume is fine
                // three notches down, and holding it back here would take
                // that away for good. See `vis::Tapped`.
                *sample *= gain;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f32, frames: usize) -> Vec<f64> {
        (0..frames)
            .flat_map(|i| {
                let t = i as f64 / f64::from(SAMPLE_RATE);
                let sample = 0.25 * (std::f64::consts::TAU * f64::from(hz) * t).sin();
                [sample, sample]
            })
            .collect()
    }

    fn rms(samples: &[f64]) -> f64 {
        (samples.iter().map(|s| s * s).sum::<f64>() / samples.len() as f64).sqrt()
    }

    #[test]
    fn a_boosted_band_makes_its_tone_louder_and_leaves_others_alone() {
        let shared = shared();
        shared.lock().unwrap().on = true;
        shared.lock().unwrap().bands_db[4] = 12.0; // 1 kHz
        let mut processor = Processor::new(shared);
        let mut at_band = tone(1000.0, 8192);
        let before = rms(&at_band[4096..]);
        processor.process(&mut at_band);
        let after = rms(&at_band[4096..]);
        let gain_db = 20.0 * (after / before).log10();
        assert!((gain_db - 12.0).abs() < 1.0, "1 kHz gained {gain_db:.1} dB");

        let mut far = tone(60.0, 8192);
        let before = rms(&far[4096..]);
        processor.process(&mut far);
        let after = rms(&far[4096..]);
        let gain_db = 20.0 * (after / before).log10();
        assert!(gain_db.abs() < 1.0, "60 Hz moved {gain_db:.1} dB");
    }

    #[test]
    fn off_or_flat_changes_nothing_and_the_preamp_scales() {
        let shared = shared();
        let mut processor = Processor::new(shared.clone());
        let original = tone(440.0, 1024);
        let mut samples = original.clone();
        processor.process(&mut samples);
        assert_eq!(samples, original);

        shared.lock().unwrap().on = true;
        let mut samples = original.clone();
        processor.process(&mut samples);
        assert_eq!(samples, original);

        shared.lock().unwrap().preamp_db = 6.0;
        // A change fades in over one packet.
        processor.process(&mut original.clone());
        let mut samples = original.clone();
        processor.process(&mut samples);
        let ratio = rms(&samples) / rms(&original);
        assert!((20.0 * ratio.log10() - 6.0).abs() < 0.1);

        shared.lock().unwrap().preamp_db = -6.0;
        processor.process(&mut original.clone());
        let mut samples = original.clone();
        processor.process(&mut samples);
        let ratio = rms(&samples) / rms(&original);
        assert!((20.0 * ratio.log10() + 6.0).abs() < 0.1);
    }

    #[test]
    fn the_drawn_response_peaks_at_the_band_and_adds_the_preamp() {
        let mut settings = EqSettings::default();
        settings.bands_db[7] = 6.0; // 12 kHz
        settings.preamp_db = -3.0;
        assert!((settings.response_db(12_000.0) - 3.0).abs() < 0.1);
        assert!((settings.response_db(100.0) + 3.0).abs() < 0.2);
        let clamped = EqSettings {
            preamp_db: 20.0,
            bands_db: [20.0; 10],
            ..settings
        }
        .clamped();
        assert_eq!(clamped.preamp_db, RANGE_DB);
        assert!(clamped.bands_db.iter().all(|band| *band == RANGE_DB));
    }

    #[test]
    fn balance_turns_one_side_down_and_mono_makes_the_sides_the_same() {
        let shared = shared();
        let mut processor = Processor::new(shared.clone());
        let mut samples = vec![0.5, -0.25, 0.5, -0.25];
        shared.lock().unwrap().balance = 0.5;
        processor.process(&mut samples);
        assert_eq!(samples, vec![0.25, -0.25, 0.25, -0.25]);

        shared.lock().unwrap().balance = 0.0;
        shared.lock().unwrap().mono = true;
        let mut samples = vec![0.5, -0.25];
        processor.process(&mut samples);
        assert_eq!(samples, vec![0.125, 0.125]);
        assert_eq!(
            EqSettings {
                balance: 3.0,
                ..EqSettings::default()
            }
            .clamped()
            .balance,
            1.0
        );
    }

    /// Every preset, played, meets its sliders at every band's centre,
    /// which the top three bands, a fifth of an octave apart, did not.
    #[test]
    fn what_is_played_meets_the_sliders_at_every_band() {
        for preset in PRESETS {
            let settings = EqSettings {
                on: true,
                bands_db: preset.bands_db,
                ..EqSettings::default()
            };
            let curve = settings.curve();
            for (hz, wanted) in BANDS.iter().zip(preset.bands_db) {
                let got = curve.db_at(*hz);
                assert!(
                    (got - wanted).abs() < 0.3,
                    "{} at {hz} Hz plays {got:.1} dB for {wanted:.1}",
                    preset.name
                );
            }
        }
    }

    /// One slider moves its own band and leaves the neighbours' centres
    /// alone, even at the top where they sit close together.
    #[test]
    fn a_slider_leaves_its_neighbours_centres_alone() {
        let mut settings = EqSettings::default();
        settings.bands_db[8] = 12.0; // 14 kHz
        let curve = settings.curve();
        assert!((curve.db_at(14_000.0) - 12.0).abs() < 0.3);
        assert!(
            curve.db_at(12_000.0).abs() < 0.3,
            "12 kHz moved {:.1}",
            curve.db_at(12_000.0)
        );
        assert!(
            curve.db_at(16_000.0).abs() < 0.3,
            "16 kHz moved {:.1}",
            curve.db_at(16_000.0)
        );
        assert!(curve.db_at(1_000.0).abs() < 0.1);
        // Between two boosted neighbours the response does not sag away.
        settings.bands_db[7] = 12.0; // 12 kHz too
        let curve = settings.curve();
        assert!(
            curve.db_at(13_000.0) > 9.0,
            "13 kHz sags to {:.1}",
            curve.db_at(13_000.0)
        );
    }

    /// The outer sliders reach past their centres: the sub-bass follows
    /// the 60 Hz slider and the air follows the 16 kHz one, as they did.
    #[test]
    fn the_outer_sliders_reach_the_ends() {
        let mut settings = EqSettings::default();
        settings.bands_db[0] = 12.0;
        let curve = settings.curve();
        assert!(
            curve.db_at(30.0) > 6.0,
            "30 Hz gets {:.1}",
            curve.db_at(30.0)
        );
        assert!(curve.db_at(170.0).abs() < 0.3);
        let mut settings = EqSettings::default();
        settings.bands_db[9] = 12.0;
        let curve = settings.curve();
        // A peaking filter is back at 0 dB by the sample rate's top, so
        // half the slider is what 19 kHz can hold.
        assert!(
            curve.db_at(19_000.0) > 5.0,
            "19 kHz gets {:.1}",
            curve.db_at(19_000.0)
        );
        assert!(curve.db_at(14_000.0).abs() < 0.3);
    }

    /// Moving a slider, the preamp or a preset while a tone plays, or
    /// switching the equalizer off and on, never steps the waveform: the
    /// filters keep their history and the switch and preamp fade over a
    /// packet.
    #[test]
    fn changing_the_settings_mid_tone_does_not_click() {
        let shared = shared();
        {
            let mut settings = shared.lock().unwrap();
            settings.on = true;
            settings.bands_db[0] = 6.0;
        }
        let mut processor = Processor::new(shared.clone());
        let packet = 1024;
        let signal = tone(60.0, packet * 40);
        let mut played: Vec<f64> = Vec::new();
        for (index, chunk) in signal.chunks(packet * 2).enumerate() {
            {
                let mut settings = shared.lock().unwrap();
                match index {
                    10 => settings.bands_db[0] = 9.0,
                    14 => settings.bands_db[1] = 0.6,
                    18 => settings.bands_db = PRESETS[13].bands_db,
                    22 => settings.preamp_db = -6.0,
                    26 => settings.on = false,
                    30 => settings.on = true,
                    34 => settings.bands_db = [0.0; 10],
                    _ => {}
                }
            }
            let mut samples = chunk.to_vec();
            processor.process(&mut samples);
            played.extend(samples.iter().step_by(2));
        }
        // A 60 Hz tone at the boost moves at most about 0.01 a sample.
        let worst = played
            .windows(2)
            .skip(packet * 4)
            .map(|pair| (pair[1] - pair[0]).abs())
            .fold(0.0, f64::max);
        assert!(worst < 0.02, "a step of {worst:.4} between samples");
    }

    #[test]
    fn every_preset_stays_within_the_range() {
        assert_eq!(PRESETS[0].name, "Flat");
        for preset in PRESETS {
            assert!(
                preset.bands_db.iter().all(|band| band.abs() <= RANGE_DB),
                "{} leaves the range",
                preset.name
            );
        }
    }
}
