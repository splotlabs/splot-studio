// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The recorded settings of two encode jobs, side by side.
//!
//! An option that a job inherited stays distinct from an option that the job
//! set explicitly to the same value. A value that a job did not record is
//! unknown: it is not equal and not different.

use std::collections::{BTreeMap, BTreeSet};

use super::EncodeProvenance;
use crate::options::{EffectiveOption, OptionValue};
use crate::protocol::OptionsIdentity;
use crate::settings::EncoderSettings;

/// How one option was set for one side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigCell {
    /// The explicit value the job requested. `None` means the job inherited the option.
    pub requested: Option<OptionValue>,
    /// The effective value and its origin. `None` means the job did not record them.
    pub effective: Option<EffectiveOption>,
}

/// Why a row of the configuration comparison is listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConfigDifference {
    /// The effective values differ.
    Effective,
    /// The effective values are equal, but one job set the option explicitly
    /// and the other inherited it, or the origins differ.
    Declaration,
    /// The explicit values differ and an effective value is not recorded.
    Requested,
    /// Only one job recorded the option.
    OneSided,
}

/// One option that the two encode jobs did not set alike.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigRow {
    /// The canonical option key.
    pub key: String,
    /// Side A.
    pub a: ConfigCell,
    /// Side B.
    pub b: ConfigCell,
    /// Why the row is listed.
    pub difference: ConfigDifference,
}

/// The settings of two encode jobs, side by side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPair {
    /// The option schema and codec revision of each job. `None` is unknown.
    pub codec: (Option<OptionsIdentity>, Option<OptionsIdentity>),
    /// The Simple controls and output kind of each job.
    pub base: (EncoderSettings, EncoderSettings),
    /// Whether each job recorded its effective policy.
    pub effective_known: (bool, bool),
    /// The options that differ, in the schema order of side A, then the rest by key.
    pub rows: Vec<ConfigRow>,
    /// The number of compared options with no difference.
    pub same: usize,
}

/// The configuration differences of a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigComparison {
    /// At least one side has no recorded encode job. Nothing is inferred
    /// from its bitstream.
    Unknown {
        /// Side A has recorded settings.
        a: bool,
        /// Side B has recorded settings.
        b: bool,
    },
    /// Both sides have recorded settings.
    Known(Box<ConfigPair>),
}

/// The effective policy of a job by key. Empty when it is not compared.
fn policy(encode: &EncodeProvenance, compared: bool) -> BTreeMap<&str, &EffectiveOption> {
    encode
        .effective
        .iter()
        .filter(|_| compared)
        .map(|row| (row.key.as_str(), row))
        .collect()
}

fn difference(a: &ConfigCell, b: &ConfigCell) -> Option<ConfigDifference> {
    match (&a.effective, &b.effective) {
        (Some(x), Some(y)) if (&x.value, &x.screen) != (&y.value, &y.screen) => {
            Some(ConfigDifference::Effective)
        }
        (Some(x), Some(y)) => (a.requested.is_some() != b.requested.is_some()
            || (&x.origin, &x.screen_origin) != (&y.origin, &y.screen_origin))
            .then_some(ConfigDifference::Declaration),
        (None, None) => (a.requested != b.requested).then_some(ConfigDifference::Requested),
        _ => Some(ConfigDifference::OneSided),
    }
}

/// Compares the recorded settings of two encode jobs.
#[must_use]
pub fn compare_configuration(a: &EncodeProvenance, b: &EncodeProvenance) -> ConfigPair {
    let known = (!a.effective.is_empty(), !b.effective.is_empty());
    // Without both policies only explicit requests are comparable: a missing
    // effective value is unknown, not equal and not different.
    let both = known.0 && known.1;
    let (policy_a, policy_b) = (policy(a, both), policy(b, both));
    // The schema order of side A, then every other key in key order.
    let ordered: Vec<&str> = a
        .effective
        .iter()
        .map(|row| row.key.as_str())
        .filter(|key| policy_a.contains_key(key))
        .collect();
    let rest: BTreeSet<&str> = policy_b
        .keys()
        .copied()
        .chain(a.requested.values.keys().map(String::as_str))
        .chain(b.requested.values.keys().map(String::as_str))
        .filter(|key| !policy_a.contains_key(key))
        .collect();
    let cell =
        |encode: &EncodeProvenance, policy: &BTreeMap<&str, &EffectiveOption>, key| ConfigCell {
            requested: encode.requested.values.get(key).cloned(),
            effective: policy.get(key).copied().cloned(),
        };
    let mut rows = Vec::new();
    let mut same = 0;
    for key in ordered.into_iter().chain(rest) {
        let (left, right) = (cell(a, &policy_a, key), cell(b, &policy_b, key));
        match difference(&left, &right) {
            Some(difference) => rows.push(ConfigRow {
                key: key.to_owned(),
                a: left,
                b: right,
                difference,
            }),
            None => same += 1,
        }
    }
    ConfigPair {
        codec: (a.codec.clone(), b.codec.clone()),
        base: (a.requested.base, b.requested.base),
        effective_known: known,
        rows,
        same,
    }
}
