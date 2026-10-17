//! A Mix playlist's stored transition: the base64 protobuf in a playlist
//! item's `automix.auto_transition_recipe` format attribute. It names the
//! pair it was made for, where the overlap starts in each track, how long it
//! lasts, and the preset that shapes it, with optional curve overrides.
//!
//! The recipe is only trusted for the exact pair it was saved for; any
//! mismatch (other tracks, a changed beat analysis, another playback speed)
//! means the caller falls back to an automatic transition.

use base64::Engine as _;
use protobuf::CodedInputStream;

use super::curve::Curve;

pub const ATTRIBUTE: &str = "automix.auto_transition_recipe";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Overlap {
    pub start_a_ms: i32,
    pub start_b_ms: i32,
    pub duration_ms: i32,
    pub speed_a: Option<f32>,
    pub speed_b: Option<f32>,
    pub duration_bars: Option<i32>,
    pub is_beatmatched: bool,
    pub track_a_uri: String,
    pub track_b_uri: String,
    pub playable_a_uri: String,
    pub playable_b_uri: String,
    pub beats_hash_a: String,
    pub beats_hash_b: String,
    pub bpm_a: f32,
    pub bpm_b: f32,
    pub item_speed_a: f64,
    pub item_speed_b: f64,
}

/// Curves that replace the preset's own, per parameter.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Overrides {
    pub volume_out: Option<Curve>,
    pub volume_in: Option<Curve>,
    /// Low, mid, high.
    pub eq_out: [Option<Curve>; 3],
    pub eq_in: [Option<Curve>; 3],
    pub cutoff_out: Option<Curve>,
    pub resonance_out: Option<Curve>,
    pub cutoff_in: Option<Curve>,
    pub resonance_in: Option<Curve>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Recipe {
    pub overlap: Option<Overlap>,
    pub preset_id: i32,
    /// PresetType: 1 fade, 2 cut, 3 beatmatched fade, 4 beatmatched cut.
    pub preset_type: i32,
    pub overrides: Overrides,
    /// False when the stored overlap is only a hint and the preset alone was
    /// chosen; the overlap is then recomputed like an automatic one.
    pub is_overlap_override: bool,
}

impl Recipe {
    /// Decodes the attribute's base64 value.
    pub fn from_attribute(value: &str) -> anyhow::Result<Self> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(value.trim())
            .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(value.trim()))?;
        Self::decode(&bytes)
    }

    pub fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
        let mut recipe = Self::default();
        let mut input = CodedInputStream::from_bytes(bytes);
        while let Some(tag) = input.read_raw_tag_or_eof()? {
            match tag {
                10 => recipe.overlap = Some(overlap(&input.read_bytes()?)?),
                18 => recipe.preset(&input.read_bytes()?)?,
                24 => recipe.is_overlap_override = input.read_bool()?,
                _ => protobuf::rt::skip_field_for_tag(tag, &mut input)?,
            }
        }
        Ok(recipe)
    }

    fn preset(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        let mut input = CodedInputStream::from_bytes(bytes);
        let overrides = &mut self.overrides;
        while let Some(tag) = input.read_raw_tag_or_eof()? {
            match tag {
                8 => self.preset_id = input.read_int32()?,
                16 => self.preset_type = input.read_int32()?,
                50 => overrides.volume_out = Some(Curve::decode(&input.read_bytes()?)?),
                58 => overrides.volume_in = Some(Curve::decode(&input.read_bytes()?)?),
                66 => overrides.eq_out = eq(&input.read_bytes()?)?,
                74 => overrides.eq_in = eq(&input.read_bytes()?)?,
                82 => {
                    (overrides.cutoff_out, overrides.resonance_out) = filter(&input.read_bytes()?)?;
                }
                90 => {
                    (overrides.cutoff_in, overrides.resonance_in) = filter(&input.read_bytes()?)?;
                }
                _ => protobuf::rt::skip_field_for_tag(tag, &mut input)?,
            }
        }
        Ok(())
    }

    /// Whether the recipe still describes this pair: the same tracks, the
    /// same beat analysis when both sides name one, and playback at the
    /// speed it was made for (Spotifast always plays at 1x).
    pub fn matches(
        &self,
        a_uri: &str,
        b_uri: &str,
        beats_hash_a: Option<&str>,
        beats_hash_b: Option<&str>,
    ) -> bool {
        let Some(overlap) = &self.overlap else {
            return false;
        };
        let same_track = |stored: &str, playable: &str, uri: &str| {
            stored == uri || (!playable.is_empty() && playable == uri)
        };
        let same_hash = |stored: &str, current: Option<&str>| {
            stored.is_empty() || current.is_none_or(|current| current == stored)
        };
        let unit_speed = |speed: f64| speed == 0.0 || (speed - 1.0).abs() <= 0.001;
        same_track(&overlap.track_a_uri, &overlap.playable_a_uri, a_uri)
            && same_track(&overlap.track_b_uri, &overlap.playable_b_uri, b_uri)
            && same_hash(&overlap.beats_hash_a, beats_hash_a)
            && same_hash(&overlap.beats_hash_b, beats_hash_b)
            && unit_speed(overlap.item_speed_a)
            && unit_speed(overlap.item_speed_b)
    }
}

fn overlap(bytes: &[u8]) -> anyhow::Result<Overlap> {
    let mut overlap = Overlap::default();
    let mut input = CodedInputStream::from_bytes(bytes);
    while let Some(tag) = input.read_raw_tag_or_eof()? {
        match tag {
            24 => overlap.start_a_ms = input.read_int32()?,
            32 => overlap.start_b_ms = input.read_int32()?,
            40 => overlap.duration_ms = input.read_int32()?,
            53 => overlap.speed_a = Some(input.read_float()?),
            61 => overlap.speed_b = Some(input.read_float()?),
            64 => overlap.duration_bars = Some(input.read_int32()?),
            72 => overlap.is_beatmatched = input.read_bool()?,
            82 => overlap.track_a_uri = input.read_string()?,
            90 => overlap.track_b_uri = input.read_string()?,
            98 => overlap.playable_a_uri = input.read_string()?,
            106 => overlap.playable_b_uri = input.read_string()?,
            114 => overlap.beats_hash_a = input.read_string()?,
            122 => overlap.beats_hash_b = input.read_string()?,
            133 => overlap.bpm_a = input.read_float()?,
            141 => overlap.bpm_b = input.read_float()?,
            145 => overlap.item_speed_a = input.read_double()?,
            153 => overlap.item_speed_b = input.read_double()?,
            _ => protobuf::rt::skip_field_for_tag(tag, &mut input)?,
        }
    }
    Ok(overlap)
}

fn eq(bytes: &[u8]) -> anyhow::Result<[Option<Curve>; 3]> {
    let mut bands = [None, None, None];
    let mut input = CodedInputStream::from_bytes(bytes);
    while let Some(tag) = input.read_raw_tag_or_eof()? {
        match tag {
            10 => bands[0] = Some(Curve::decode(&input.read_bytes()?)?),
            18 => bands[1] = Some(Curve::decode(&input.read_bytes()?)?),
            26 => bands[2] = Some(Curve::decode(&input.read_bytes()?)?),
            _ => protobuf::rt::skip_field_for_tag(tag, &mut input)?,
        }
    }
    Ok(bands)
}

fn filter(bytes: &[u8]) -> anyhow::Result<(Option<Curve>, Option<Curve>)> {
    let (mut cutoff, mut resonance) = (None, None);
    let mut input = CodedInputStream::from_bytes(bytes);
    while let Some(tag) = input.read_raw_tag_or_eof()? {
        match tag {
            10 => cutoff = Some(Curve::decode(&input.read_bytes()?)?),
            18 => resonance = Some(Curve::decode(&input.read_bytes()?)?),
            _ => protobuf::rt::skip_field_for_tag(tag, &mut input)?,
        }
    }
    Ok((cutoff, resonance))
}

#[cfg(test)]
pub(crate) mod tests {
    use protobuf::CodedOutputStream;

    use super::*;

    pub(crate) const A: &str = "spotify:track:0000000000000000000000";
    pub(crate) const B: &str = "spotify:track:1111111111111111111111";

    fn message(write: impl FnOnce(&mut CodedOutputStream)) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut output = CodedOutputStream::vec(&mut bytes);
        write(&mut output);
        output.flush().unwrap();
        drop(output);
        bytes
    }

    /// A Rise (preset 3) from 180 s into A to 20 s into B over 16 s, with a
    /// linear volume override on the outgoing track. Anonymised: the URIs
    /// and hashes are made up.
    pub(crate) fn fixture() -> String {
        let point = |x: f64, y: f64| {
            message(|out| {
                out.write_double(1, x).unwrap();
                out.write_double(2, y).unwrap();
            })
        };
        let curve = message(|out| {
            let piece = message(|out| {
                out.write_bytes(1, &point(0.0, 1.0)).unwrap();
                out.write_bytes(1, &point(1.0, 0.0)).unwrap();
                out.write_double(2, 0.0).unwrap();
                out.write_double(3, 1.0).unwrap();
            });
            out.write_bytes(1, &piece).unwrap();
        });
        let overlap = message(|out| {
            out.write_int32(3, 180_000).unwrap();
            out.write_int32(4, 20_000).unwrap();
            out.write_int32(5, 16_000).unwrap();
            out.write_int32(8, 8).unwrap();
            out.write_bool(9, true).unwrap();
            out.write_string(10, A).unwrap();
            out.write_string(11, B).unwrap();
            out.write_string(14, "hash-a").unwrap();
            out.write_string(15, "hash-b").unwrap();
            out.write_float(16, 120.0).unwrap();
            out.write_float(17, 121.0).unwrap();
            out.write_double(18, 1.0).unwrap();
            // An unknown field is skipped.
            out.write_string(30, "ignored").unwrap();
        });
        let preset = message(|out| {
            out.write_int32(1, 3).unwrap();
            out.write_int32(2, 3).unwrap();
            out.write_bytes(6, &curve).unwrap();
        });
        let recipe = message(|out| {
            out.write_bytes(1, &overlap).unwrap();
            out.write_bytes(2, &preset).unwrap();
            out.write_bool(3, true).unwrap();
        });
        base64::engine::general_purpose::STANDARD.encode(recipe)
    }

    #[test]
    fn decodes_a_stored_transition() {
        let recipe = Recipe::from_attribute(&fixture()).unwrap();
        let overlap = recipe.overlap.as_ref().unwrap();
        assert_eq!(
            (overlap.start_a_ms, overlap.start_b_ms, overlap.duration_ms),
            (180_000, 20_000, 16_000)
        );
        assert_eq!(overlap.duration_bars, Some(8));
        assert!(overlap.is_beatmatched);
        assert_eq!(
            (overlap.track_a_uri.as_str(), overlap.track_b_uri.as_str()),
            (A, B)
        );
        assert_eq!((overlap.bpm_a, overlap.bpm_b), (120.0, 121.0));
        assert_eq!((recipe.preset_id, recipe.preset_type), (3, 3));
        assert!(recipe.is_overlap_override);
        let volume = recipe.overrides.volume_out.as_ref().unwrap();
        assert!((volume.eval(0.25) - 0.75).abs() < 1e-6);
        assert!(recipe.overrides.volume_in.is_none());
    }

    #[test]
    fn a_recipe_only_matches_its_own_pair_and_analysis() {
        let recipe = Recipe::from_attribute(&fixture()).unwrap();
        assert!(recipe.matches(A, B, Some("hash-a"), Some("hash-b")));
        assert!(recipe.matches(A, B, None, None));
        assert!(!recipe.matches(B, A, None, None));
        assert!(!recipe.matches(A, B, Some("reanalysed"), Some("hash-b")));
        let mut faster = recipe.clone();
        faster.overlap.as_mut().unwrap().item_speed_a = 1.1;
        assert!(!faster.matches(A, B, None, None));
        assert!(Recipe::from_attribute("not base64!").is_err());
    }
}
