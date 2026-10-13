//! Per-track mixing analysis from Spotify's extended metadata: the beat grid,
//! tempo and key, and whether Spotify considers the track mixable. Each
//! payload is a small protobuf read field by field with the codec the crate
//! already uses; unknown fields are skipped.

use protobuf::CodedInputStream;

/// Extended-metadata kinds this module reads.
pub const KIND_AUDIO_ATTRIBUTES: i32 = 61;
pub const KIND_BEATS: i32 = 217;
pub const KIND_MIXABILITY: i32 = 219;
pub const KIND_MIXABILITY_TRAIT: i32 = 235;

/// One beat of the grid. Times are seconds from the start of the track.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Beat {
    pub time: f32,
    pub duration: f32,
    /// Position in the bar, 1 for a downbeat.
    pub value: u32,
}

impl Beat {
    pub fn is_downbeat(&self) -> bool {
        self.value == 1
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Beats {
    pub beats_per_bar: u32,
    pub beats: Vec<Beat>,
    pub hash: String,
}

impl Beats {
    pub fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
        let mut beats = Self::default();
        let mut input = CodedInputStream::from_bytes(bytes);
        while let Some(tag) = input.read_raw_tag_or_eof()? {
            match tag {
                8 => beats.beats_per_bar = input.read_uint32()?,
                18 => beats.beats.push(beat(&input.read_bytes()?)?),
                26 => beats.hash = input.read_string()?,
                _ => protobuf::rt::skip_field_for_tag(tag, &mut input)?,
            }
        }
        Ok(beats)
    }

    pub fn downbeats(&self) -> impl Iterator<Item = f32> + '_ {
        self.beats
            .iter()
            .filter(|beat| beat.is_downbeat())
            .map(|beat| beat.time)
    }

    /// Tempo from the median beat length, which ignores the odd missed or
    /// doubled beat.
    pub fn bpm(&self) -> Option<f64> {
        let mut lengths: Vec<f32> = self
            .beats
            .windows(2)
            .map(|pair| pair[1].time - pair[0].time)
            .filter(|length| *length > 0.0)
            .collect();
        if lengths.is_empty() {
            return None;
        }
        lengths.sort_by(f32::total_cmp);
        Some(60.0 / f64::from(lengths[lengths.len() / 2]))
    }
}

fn beat(bytes: &[u8]) -> anyhow::Result<Beat> {
    let mut beat = Beat {
        time: 0.0,
        duration: 0.0,
        value: 0,
    };
    let mut input = CodedInputStream::from_bytes(bytes);
    while let Some(tag) = input.read_raw_tag_or_eof()? {
        match tag {
            13 => beat.time = input.read_float()?,
            21 => beat.duration = input.read_float()?,
            24 => beat.value = input.read_uint32()?,
            _ => protobuf::rt::skip_field_for_tag(tag, &mut input)?,
        }
    }
    Ok(beat)
}

/// What a mixing decision needs to know about one track.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TrackMix {
    pub bpm: Option<f64>,
    /// Key name as Spotify writes it, with the Camelot code when given.
    pub key: Option<String>,
    pub camelot: Option<String>,
    /// MIXABILITY: Spotify's analysis says the track can be beat-mixed.
    pub mixable: Option<bool>,
    pub beatmatchability: Option<f64>,
    /// MIXABILITY_TRAIT: the rights side allows mixing at all.
    pub allow_mixing: Option<bool>,
    pub beats: Option<Beats>,
}

impl TrackMix {
    /// Folds one extended-metadata payload of `kind` into the record.
    pub fn apply(&mut self, kind: i32, bytes: &[u8]) -> anyhow::Result<()> {
        match kind {
            KIND_AUDIO_ATTRIBUTES => self.audio_attributes(bytes)?,
            KIND_BEATS => self.beats = Some(Beats::decode(bytes)?),
            KIND_MIXABILITY => {
                let mut input = CodedInputStream::from_bytes(bytes);
                while let Some(tag) = input.read_raw_tag_or_eof()? {
                    match tag {
                        8 => self.mixable = Some(input.read_bool()?),
                        17 => self.beatmatchability = Some(input.read_double()?),
                        _ => protobuf::rt::skip_field_for_tag(tag, &mut input)?,
                    }
                }
                // An empty message is proto3's `mixable: false`.
                self.mixable.get_or_insert(false);
            }
            KIND_MIXABILITY_TRAIT => {
                let mut input = CodedInputStream::from_bytes(bytes);
                while let Some(tag) = input.read_raw_tag_or_eof()? {
                    match tag {
                        8 => self.allow_mixing = Some(input.read_bool()?),
                        _ => protobuf::rt::skip_field_for_tag(tag, &mut input)?,
                    }
                }
                self.allow_mixing.get_or_insert(false);
            }
            _ => {}
        }
        Ok(())
    }

    fn audio_attributes(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        let mut input = CodedInputStream::from_bytes(bytes);
        while let Some(tag) = input.read_raw_tag_or_eof()? {
            match tag {
                9 => self.bpm = Some(input.read_double()?).filter(|bpm| *bpm > 0.0),
                18 => {
                    let key = input.read_bytes()?;
                    let mut input = CodedInputStream::from_bytes(&key);
                    while let Some(tag) = input.read_raw_tag_or_eof()? {
                        match tag {
                            10 => self.key = Some(input.read_string()?),
                            26 => {
                                let camelot = input.read_bytes()?;
                                let mut input = CodedInputStream::from_bytes(&camelot);
                                while let Some(tag) = input.read_raw_tag_or_eof()? {
                                    match tag {
                                        10 => self.camelot = Some(input.read_string()?),
                                        _ => protobuf::rt::skip_field_for_tag(tag, &mut input)?,
                                    }
                                }
                            }
                            _ => protobuf::rt::skip_field_for_tag(tag, &mut input)?,
                        }
                    }
                }
                _ => protobuf::rt::skip_field_for_tag(tag, &mut input)?,
            }
        }
        Ok(())
    }

    /// The tempo, from the attributes or else the beat grid.
    pub fn tempo(&self) -> Option<f64> {
        self.bpm
            .or_else(|| self.beats.as_ref().and_then(Beats::bpm))
    }

    /// Spotify lets this track be beat-mixed: analysed as mixable and not
    /// excluded by its rights.
    pub fn can_beatmix(&self) -> bool {
        self.mixable == Some(true) && self.allow_mixing != Some(false)
    }
}
