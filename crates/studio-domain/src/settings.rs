// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Requested encoder settings, as the user chooses them.
//!
//! Ranges mirror `splot-encode` at the pinned revision (`Preset::MAX`,
//! `Crf::MAX`); the engine validates again against the codec itself. This is a
//! request DTO, not a copy of the codec's `Config`.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A value outside the range the pinned encoder accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SettingsError {
    /// The speed preset exceeds [`SpeedPreset::MAX`].
    #[error("speed preset {0} is outside 0..={max}", max = SpeedPreset::MAX)]
    Preset(u8),
    /// The rate factor is outside [`Crf::MIN`]`..=`[`Crf::MAX`].
    #[error("rate factor {0} is outside {min}..={max}", min = Crf::MIN, max = Crf::MAX)]
    Crf(u8),
}

/// The file format of an encode. Both are video-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputKind {
    /// AV2 in IVF (`AV02` fourcc), a lightweight video-only wrapper.
    Ivf,
    /// AV2 Annex B length-delimited OBU stream, an elementary stream.
    Av2AnnexB,
}

impl OutputKind {
    /// Every output kind, in display order.
    pub const ALL: [Self; 2] = [Self::Ivf, Self::Av2AnnexB];

    /// The file extension, without the dot.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Ivf => "ivf",
            Self::Av2AnnexB => "obu",
        }
    }

    /// The exact format name shown to users.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ivf => "AV2 IVF",
            Self::Av2AnnexB => "AV2 Annex B length-delimited OBU stream",
        }
    }
}

/// An encoder speed preset, `0..=10`; higher is faster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct SpeedPreset(u8);

impl SpeedPreset {
    /// The fastest preset.
    pub const MAX: u8 = 10;

    /// Creates a preset.
    ///
    /// # Errors
    /// Returns [`SettingsError::Preset`] when `value` exceeds [`Self::MAX`].
    pub const fn new(value: u8) -> Result<Self, SettingsError> {
        if value <= Self::MAX {
            Ok(Self(value))
        } else {
            Err(SettingsError::Preset(value))
        }
    }

    /// Returns the preset number.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl TryFrom<u8> for SpeedPreset {
    type Error = SettingsError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<SpeedPreset> for u8 {
    fn from(preset: SpeedPreset) -> Self {
        preset.0
    }
}

/// A constant-quality rate factor, `1..=63`; higher means fewer bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct Crf(u8);

impl Crf {
    /// The highest-quality rate factor.
    pub const MIN: u8 = 1;
    /// The lowest-quality rate factor.
    pub const MAX: u8 = 63;

    /// Creates a rate factor.
    ///
    /// # Errors
    /// Returns [`SettingsError::Crf`] when `value` is outside
    /// [`Self::MIN`]`..=`[`Self::MAX`].
    pub const fn new(value: u8) -> Result<Self, SettingsError> {
        if value >= Self::MIN && value <= Self::MAX {
            Ok(Self(value))
        } else {
            Err(SettingsError::Crf(value))
        }
    }

    /// Returns the rate factor.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl TryFrom<u8> for Crf {
    type Error = SettingsError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<Crf> for u8 {
    fn from(crf: Crf) -> Self {
        crf.0
    }
}

/// What the user asks the encoder for. Source-derived values (dimensions, bit
/// depth, frame rate) are not settings and arrive with the job in STUDIO-02.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EncoderSettings {
    /// The output file format.
    pub output: OutputKind,
    /// The speed preset.
    pub preset: SpeedPreset,
    /// The constant-quality rate factor.
    pub crf: Crf,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_match_the_pinned_encoder() {
        assert!(SpeedPreset::new(SpeedPreset::MAX).is_ok());
        assert_eq!(SpeedPreset::new(11), Err(SettingsError::Preset(11)));
        assert_eq!(Crf::new(0), Err(SettingsError::Crf(0)));
        assert!(Crf::new(Crf::MIN).is_ok() && Crf::new(Crf::MAX).is_ok());
        assert_eq!(Crf::new(64), Err(SettingsError::Crf(64)));
    }

    #[test]
    fn deserializing_rejects_out_of_range_values() {
        let json = r#"{"output":"av2_annex_b","preset":11,"crf":20}"#;
        assert!(serde_json::from_str::<EncoderSettings>(json).is_err());
        let json = r#"{"output":"av2_annex_b","preset":10,"crf":20}"#;
        let settings: EncoderSettings = serde_json::from_str(json).unwrap_or_else(|e| {
            unreachable!("valid settings must parse: {e}");
        });
        assert_eq!(settings.output.extension(), "obu");
    }
}
