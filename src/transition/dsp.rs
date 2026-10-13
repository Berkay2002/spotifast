//! The sound of an overlap: each track through a three-band isolator, a
//! resonant low- or high-pass filter and a volume, all following curves over
//! the overlap's normalised time, then summed. Runs on the player thread for
//! every packet of an overlap, so nothing here allocates.

use librespot_playback::transition::TransitionMix;

use super::curve::Curve;
use crate::eq::Biquad;

/// Frames between curve evaluations and filter retunes.
const BLOCK: usize = 32;
/// Isolator crossovers, Linkwitz-Riley fourth order.
const LOW_HZ: f64 = 200.0;
const HIGH_HZ: f64 = 2_500.0;
const BUTTERWORTH_Q: f64 = std::f64::consts::FRAC_1_SQRT_2;
/// A low-pass this open, or a high-pass this low, is no filter at all.
const LOWPASS_OPEN_HZ: f32 = 19_000.0;
const HIGHPASS_OPEN_HZ: f32 = 25.0;
/// Gains slew towards their curve with this time constant, so a step in a
/// curve (a bass kill, a cut) lands without a click.
const SLEW_SECONDS: f64 = 0.005;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterKind {
    LowPass,
    HighPass,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Filter {
    pub kind: FilterKind,
    /// Cutoff in Hz.
    pub cutoff: Curve,
    /// Quality; Butterworth when absent.
    pub resonance: Option<Curve>,
}

/// How one track is shaped through the overlap.
#[derive(Clone, Debug, PartialEq)]
pub struct TrackShape {
    /// Linear gain.
    pub volume: Curve,
    /// Low, mid and high band gains in dB; absent is 0 dB.
    pub eq: [Option<Curve>; 3],
    pub filter: Option<Filter>,
}

impl TrackShape {
    pub fn volume(volume: Curve) -> Self {
        Self {
            volume,
            eq: [None, None, None],
            filter: None,
        }
    }
}

/// Both tracks' shapes for one transition.
#[derive(Clone, Debug, PartialEq)]
pub struct Shape {
    pub outgoing: TrackShape,
    pub incoming: TrackShape,
}

pub struct Mixer {
    outgoing: Lane,
    incoming: Lane,
}

impl Mixer {
    pub fn new(shape: Shape) -> Self {
        Self {
            outgoing: Lane::new(shape.outgoing),
            incoming: Lane::new(shape.incoming),
        }
    }

    /// Sums `b` into `a`, both interleaved stereo, `position` frames into an
    /// overlap of `length` frames.
    pub fn process(&mut self, a: &mut [f64], b: &[f64], position: u64, length: u64) {
        let length = length.max(1) as f64;
        for (index, (a, b)) in a.chunks_mut(BLOCK * 2).zip(b.chunks(BLOCK * 2)).enumerate() {
            let t = ((position + (index * BLOCK) as u64) as f64 / length) as f32;
            self.outgoing.retarget(t);
            self.incoming.retarget(t);
            for (a, b) in a
                .as_chunks_mut::<2>()
                .0
                .iter_mut()
                .zip(b.as_chunks::<2>().0)
            {
                let (al, ar) = self.outgoing.frame(a[0], a[1]);
                let (bl, br) = self.incoming.frame(b[0], b[1]);
                a[0] = al + bl;
                a[1] = ar + br;
            }
        }
    }
}

impl TransitionMix for Mixer {
    fn mix(&mut self, a: &mut [f64], b: &[f64], position: u64, length: u64) {
        self.process(a, b, position, length);
    }
}

/// Splits into three bands that sum back to an all-pass of the input:
/// low below `LOW_HZ`, high above `HIGH_HZ`, mid between.
#[derive(Clone, Copy)]
struct Isolator {
    low: [Biquad; 2],
    rest: [Biquad; 2],
    mid: [Biquad; 2],
    high: [Biquad; 2],
}

impl Isolator {
    fn new() -> Self {
        let lp = |hz| Biquad::lowpass(hz, BUTTERWORTH_Q);
        let hp = |hz| Biquad::highpass(hz, BUTTERWORTH_Q);
        Self {
            low: [lp(LOW_HZ); 2],
            rest: [hp(LOW_HZ); 2],
            mid: [lp(HIGH_HZ); 2],
            high: [hp(HIGH_HZ); 2],
        }
    }

    #[inline]
    fn run(&mut self, x: f64, gains: &[f64; 3]) -> f64 {
        let low = cascade(&mut self.low, x);
        let rest = cascade(&mut self.rest, x);
        let mid = cascade(&mut self.mid, rest);
        let high = cascade(&mut self.high, rest);
        low * gains[0] + mid * gains[1] + high * gains[2]
    }
}

#[inline]
fn cascade([first, second]: &mut [Biquad; 2], x: f64) -> f64 {
    second.run(first.run(x))
}

/// Follows its target with the slew time constant.
#[derive(Clone, Copy)]
struct Slewed {
    value: f64,
    target: f64,
}

impl Slewed {
    fn new(value: f64) -> Self {
        Self {
            value,
            target: value,
        }
    }

    #[inline]
    fn next(&mut self, step: f64) -> f64 {
        self.value += (self.target - self.value) * step;
        self.value
    }
}

struct Lane {
    shape: TrackShape,
    step: f64,
    volume: Slewed,
    eq: [Slewed; 3],
    gains: [f64; 3],
    /// How much of the isolated signal is heard rather than the dry one;
    /// the isolator's phase differs from the dry signal, so it is faded in
    /// and out rather than switched.
    eq_wet: Slewed,
    isolators: [Isolator; 2],
    filter_wet: Slewed,
    filters: [Biquad; 2],
}

impl Lane {
    fn new(shape: TrackShape) -> Self {
        let step = 1.0 - (-1.0 / (SLEW_SECONDS * f64::from(librespot_playback::SAMPLE_RATE))).exp();
        let mut lane = Self {
            shape,
            step,
            volume: Slewed::new(0.0),
            eq: [Slewed::new(1.0); 3],
            gains: [1.0; 3],
            eq_wet: Slewed::new(0.0),
            isolators: [Isolator::new(); 2],
            filter_wet: Slewed::new(0.0),
            filters: [Biquad::default(); 2],
        };
        // Start exactly on the curves rather than slewing up to them.
        lane.retarget(0.0);
        lane.volume.value = lane.volume.target;
        for band in &mut lane.eq {
            band.value = band.target;
        }
        lane.eq_wet.value = lane.eq_wet.target;
        lane.filter_wet.value = lane.filter_wet.target;
        lane
    }

    fn retarget(&mut self, t: f32) {
        self.volume.target = f64::from(self.shape.volume.eval(t).clamp(0.0, 2.0));
        let mut flat = true;
        for (band, curve) in self.eq.iter_mut().zip(&self.shape.eq) {
            let db = curve
                .as_ref()
                .map_or(0.0, |curve| curve.eval(t).clamp(-60.0, 12.0));
            flat &= db.abs() < 0.01;
            band.target = 10f64.powf(f64::from(db) / 20.0);
        }
        self.eq_wet.target = if flat { 0.0 } else { 1.0 };
        self.filter_wet.target = 0.0;
        if let Some(filter) = &self.shape.filter {
            let cutoff = filter.cutoff.eval(t).clamp(20.0, 20_000.0);
            let q = filter
                .resonance
                .as_ref()
                .map_or(BUTTERWORTH_Q, |q| f64::from(q.eval(t).clamp(0.3, 12.0)));
            let (open, tuned) = match filter.kind {
                FilterKind::LowPass => (
                    cutoff >= LOWPASS_OPEN_HZ,
                    Biquad::lowpass(f64::from(cutoff), q),
                ),
                FilterKind::HighPass => (
                    cutoff <= HIGHPASS_OPEN_HZ,
                    Biquad::highpass(f64::from(cutoff), q),
                ),
            };
            for filter in &mut self.filters {
                filter.retune(tuned);
            }
            if !open {
                self.filter_wet.target = 1.0;
            }
        }
    }

    #[inline]
    fn frame(&mut self, left: f64, right: f64) -> (f64, f64) {
        let step = self.step;
        for (gain, band) in self.gains.iter_mut().zip(&mut self.eq) {
            *gain = band.next(step);
        }
        let eq_wet = self.eq_wet.next(step);
        let filter_wet = self.filter_wet.next(step);
        let volume = self.volume.next(step);
        let mut out = [left, right];
        for (channel, sample) in out.iter_mut().enumerate() {
            // The filters always run so they are settled when they are heard.
            let isolated = self.isolators[channel].run(*sample, &self.gains);
            *sample += (isolated - *sample) * eq_wet;
            let filtered = self.filters[channel].run(*sample);
            *sample += (filtered - *sample) * filter_wet;
            *sample *= volume;
        }
        (out[0], out[1])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 44_100.0;

    fn tone(hz: f64, frames: usize) -> Vec<f64> {
        (0..frames)
            .flat_map(|i| {
                let s = (std::f64::consts::TAU * hz * i as f64 / RATE).sin() * 0.5;
                [s, s]
            })
            .collect()
    }

    fn rms(samples: &[f64]) -> f64 {
        (samples.iter().map(|s| s * s).sum::<f64>() / samples.len() as f64).sqrt()
    }

    fn silent_b(len: usize) -> Vec<f64> {
        vec![0.0; len]
    }

    fn run(shape: TrackShape, input: &[f64]) -> Vec<f64> {
        let mut mixer = Mixer::new(Shape {
            outgoing: shape,
            incoming: TrackShape::volume(Curve::constant(0.0)),
        });
        let mut a = input.to_vec();
        let frames = (a.len() / 2) as u64;
        mixer.process(&mut a, &silent_b(input.len()), 0, frames);
        a
    }

    #[test]
    fn neutral_shape_passes_the_signal_unchanged() {
        let input = tone(440.0, 4410);
        let out = run(TrackShape::volume(Curve::constant(1.0)), &input);
        assert!(input.iter().zip(&out).all(|(a, b)| (a - b).abs() < 1e-12));
    }

    #[test]
    fn a_bass_kill_removes_low_tones_and_keeps_high_ones() {
        let mut shape = TrackShape::volume(Curve::constant(1.0));
        shape.eq[0] = Some(Curve::constant(-26.0));
        let low = run(shape.clone(), &tone(60.0, 44_100));
        let high = run(shape, &tone(5_000.0, 44_100));
        // Skip the first 0.2 s while the filters settle.
        let settled = 2 * 8_820;
        let low_ratio = rms(&low[settled..]) / rms(&tone(60.0, 44_100)[settled..]);
        let high_ratio = rms(&high[settled..]) / rms(&tone(5_000.0, 44_100)[settled..]);
        assert!(low_ratio < 0.1, "low kept {low_ratio}");
        assert!((high_ratio - 1.0).abs() < 0.05, "high changed {high_ratio}");
    }

    #[test]
    fn a_closed_lowpass_removes_highs() {
        let mut shape = TrackShape::volume(Curve::constant(1.0));
        shape.filter = Some(Filter {
            kind: FilterKind::LowPass,
            cutoff: Curve::constant(300.0),
            resonance: None,
        });
        let out = run(shape, &tone(6_000.0, 22_050));
        assert!(rms(&out[8_820..]) < 0.01);
    }

    #[test]
    fn a_crossfade_sums_both_tracks_by_their_volumes() {
        let mut mixer = Mixer::new(Shape {
            outgoing: TrackShape::volume(Curve::linear(&[(0.0, 1.0), (1.0, 0.0)])),
            incoming: TrackShape::volume(Curve::linear(&[(0.0, 0.0), (1.0, 1.0)])),
        });
        let frames = 44_100;
        let mut a = vec![1.0; frames * 2];
        let b = vec![0.5; frames * 2];
        mixer.process(&mut a, &b, 0, frames as u64);
        assert!((a[0] - 1.0).abs() < 1e-3);
        // Halfway: 0.5 * 1.0 + 0.5 * 0.5, a little behind for the slew.
        let mid = a[frames];
        assert!((mid - 0.75).abs() < 0.01, "{mid}");
        assert!((a[a.len() - 1] - 0.5).abs() < 0.01);
    }

    #[test]
    fn a_step_in_volume_ramps_instead_of_clicking() {
        let mut mixer = Mixer::new(Shape {
            outgoing: TrackShape::volume(Curve::linear(&[(0.0, 1.0), (0.5, 1.0), (0.5, 0.0)])),
            incoming: TrackShape::volume(Curve::constant(0.0)),
        });
        let frames = 4_410;
        let mut a = vec![1.0; frames * 2];
        mixer.process(&mut a, &vec![0.0; frames * 2], 0, frames as u64);
        let jumps = a
            .as_chunks::<2>()
            .0
            .iter()
            .map(|f| f[0])
            .collect::<Vec<_>>()
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f64::max);
        assert!(jumps < 0.01, "largest step {jumps}");
    }
}
