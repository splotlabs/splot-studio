// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The simple controls as views of [`EncoderSettings`]: a Quality step is a
//! rate factor and a Priority step is a speed preset. There is no separate
//! "simple" state: a raw value that matches no step reads as Custom.
//!
//! Every value below is provisional, chosen for STUDIO-02 without measured
//! size/quality curves for the pinned codec; revisit them with evidence.

use studio_domain::settings::{Crf, SpeedPreset};

/// A Quality step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Quality {
    /// Smallest files of the four.
    Compact,
    /// A middle ground.
    Good,
    /// The codec's default rate factor.
    High,
    /// Closest to the source of the four.
    Highest,
}

impl Quality {
    /// Every step, left to right.
    pub const ALL: [Self; 4] = [Self::Compact, Self::Good, Self::High, Self::Highest];

    /// Provisional rate factor for each step, in [`Self::ALL`] order. `High`
    /// is `splot_encode::Config::DEFAULT_CRF` at the pinned revision.
    pub const PROVISIONAL_CRF: [u8; 4] = [36, 28, 20, 12];

    /// The rate factor this step selects.
    #[must_use]
    pub const fn crf(self) -> Crf {
        CRF_STEPS[self as usize]
    }

    /// The step a rate factor corresponds to; `None` is Custom.
    #[must_use]
    pub fn of(crf: Crf) -> Option<Self> {
        Self::ALL.into_iter().find(|q| q.crf() == crf)
    }

    /// The label on the control.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Compact => "Compact",
            Self::Good => "Good",
            Self::High => "High",
            Self::Highest => "Highest",
        }
    }
}

/// A Priority step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Priority {
    /// A faster preset.
    Fast,
    /// The codec's default preset.
    Balanced,
    /// A slower preset that spends more time on compression.
    SmallestFile,
}

impl Priority {
    /// Every step, left to right.
    pub const ALL: [Self; 3] = [Self::Fast, Self::Balanced, Self::SmallestFile];

    /// Provisional speed preset for each step, in [`Self::ALL`] order.
    /// `Balanced` is `splot_encode::Preset::default()` at the pinned revision.
    pub const PROVISIONAL_PRESET: [u8; 3] = [8, 6, 3];

    /// The speed preset this step selects.
    #[must_use]
    pub const fn preset(self) -> SpeedPreset {
        PRESET_STEPS[self as usize]
    }

    /// The step a speed preset corresponds to; `None` is Custom.
    #[must_use]
    pub fn of(preset: SpeedPreset) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.preset() == preset)
    }

    /// The label on the control.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Fast => "Fast",
            Self::Balanced => "Balanced",
            Self::SmallestFile => "Smallest file",
        }
    }

    /// One line describing the step.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Fast => "Encodes sooner, slightly larger file.",
            Self::Balanced => "The default trade-off.",
            Self::SmallestFile => "Takes longer, squeezes every megabyte.",
        }
    }
}

// The tables as typed values. These are const items, so the `unreachable!`
// arms run in const evaluation: an out-of-range edit is a compile error.
const CRF_STEPS: [Crf; 4] = {
    let t = Quality::PROVISIONAL_CRF;
    [crf(t[0]), crf(t[1]), crf(t[2]), crf(t[3])]
};

const PRESET_STEPS: [SpeedPreset; 3] = {
    let t = Priority::PROVISIONAL_PRESET;
    [preset(t[0]), preset(t[1]), preset(t[2])]
};

const fn crf(value: u8) -> Crf {
    match Crf::new(value) {
        Ok(crf) => crf,
        Err(_) => unreachable!(),
    }
}

const fn preset(value: u8) -> SpeedPreset {
    match SpeedPreset::new(value) {
        Ok(preset) => preset,
        Err(_) => unreachable!(),
    }
}

/// Index of `item` in `all`, as the UI's selected index; `-1` is Custom.
#[must_use]
pub fn index<T: PartialEq + Copy>(all: &[T], item: Option<T>) -> i32 {
    item.and_then(|item| all.iter().position(|&x| x == item))
        .and_then(|i| i32::try_from(i).ok())
        .unwrap_or(-1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::INITIAL_SETTINGS;

    fn crf(v: u8) -> Crf {
        Crf::new(v).unwrap_or_else(|e| unreachable!("{e}"))
    }

    fn preset(v: u8) -> SpeedPreset {
        SpeedPreset::new(v).unwrap_or_else(|e| unreachable!("{e}"))
    }

    #[test]
    fn steps_map_to_the_provisional_values() {
        let crfs: Vec<u8> = Quality::ALL.iter().map(|q| q.crf().get()).collect();
        assert_eq!(crfs, [36, 28, 20, 12]);
        let presets: Vec<u8> = Priority::ALL.iter().map(|p| p.preset().get()).collect();
        assert_eq!(presets, [8, 6, 3]);
    }

    #[test]
    fn values_map_back_to_their_step_or_custom() {
        for q in Quality::ALL {
            assert_eq!(Quality::of(q.crf()), Some(q));
        }
        for p in Priority::ALL {
            assert_eq!(Priority::of(p.preset()), Some(p));
        }
        assert_eq!(Quality::of(crf(21)), None);
        assert_eq!(Quality::of(crf(1)), None);
        for raw in [0, 1, 2, 9, 10] {
            assert_eq!(Priority::of(preset(raw)), None, "preset {raw}");
        }
    }

    #[test]
    fn the_initial_settings_are_high_and_balanced() {
        assert_eq!(Quality::of(INITIAL_SETTINGS.crf), Some(Quality::High));
        assert_eq!(
            Priority::of(INITIAL_SETTINGS.preset),
            Some(Priority::Balanced)
        );
    }

    #[test]
    fn quality_steps_trade_size_for_fidelity_in_order() {
        // Left to right is "smaller file" to "closer to the source": the rate
        // factor falls strictly.
        assert!(Quality::PROVISIONAL_CRF.windows(2).all(|w| w[0] > w[1]));
        // Left to right is faster to slower: the preset falls strictly.
        assert!(Priority::PROVISIONAL_PRESET.windows(2).all(|w| w[0] > w[1]));
    }

    #[test]
    fn custom_has_no_index() {
        assert_eq!(index(&Quality::ALL, Some(Quality::High)), 2);
        assert_eq!(index(&Quality::ALL, None), -1);
    }
}
