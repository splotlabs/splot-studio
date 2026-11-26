// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Encoder options that travel between the editor and the engine.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::settings::EncoderSettings;

/// The version of the option keys and their wire values.
pub const OPTIONS_SCHEMA_VERSION: u32 = 1;

/// The maximum number of descriptors or resolved options in one reply.
pub const MAX_OPTIONS_PAGE_ITEMS: u32 = 4;

/// One option value. A missing key in the request inherits its value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OptionValue {
    /// A switch.
    Bool(bool),
    /// A whole number.
    Integer(i64),
    /// A named choice.
    Choice(String),
    /// Values in the order specified by the descriptor.
    List(Vec<Self>),
    /// Named components of a compound option.
    Record(BTreeMap<String, Self>),
    /// An explicit empty value for an optional field.
    None,
}

/// The Simple controls and the user's explicit overrides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptionRequest {
    /// Output, base preset and Simple rate factor.
    pub base: EncoderSettings,
    /// Explicit overrides indexed by canonical key.
    pub values: BTreeMap<String, OptionValue>,
}

/// The structure of an editable value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ValueShape {
    /// A true or false value.
    Bool,
    /// The inclusive numeric bounds.
    Integer {
        /// Smallest accepted value.
        min: i64,
        /// Largest accepted value.
        max: i64,
    },
    /// An unsigned integer with a fixed or dependent maximum.
    Unsigned {
        /// The inclusive minimum.
        min: u64,
        /// The inclusive maximum.
        max: UnsignedLimit,
        /// The unit of the value, when applicable.
        unit: Option<String>,
    },
    /// One of the listed names.
    Choice {
        /// Names that the codec accepts.
        values: Vec<String>,
    },
    /// A positive ratio.
    Ratio,
    /// A list of unsigned byte values.
    Bytes {
        /// The largest payload the engine accepts in one field.
        max_length: u32,
    },
    /// A list with an element shape and an optional fixed length.
    List {
        /// Shape of each list item.
        element: Box<Self>,
        /// Number of items for a fixed-length list.
        length: Option<u32>,
    },
    /// A group of named values.
    Record {
        /// Members indexed by key.
        fields: BTreeMap<String, OptionMember>,
    },
    /// A record whose `mode` member selects the other members.
    Modes {
        /// Alternatives indexed by the accepted mode value.
        modes: BTreeMap<String, OptionMode>,
    },
    /// An explicit empty value or the stated shape.
    Optional {
        /// Shape when the user supplies a value.
        value: Box<Self>,
    },
    /// One value for all frames or one value per named frame kind.
    PerFrame {
        /// Shape of a frame override.
        value: Box<Self>,
        /// Frame kinds in input order.
        frames: Vec<String>,
    },
    /// Information supplied by the source, codec or pass workflow.
    ReadOnly,
}

/// An inclusive upper bound for an unsigned value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UnsignedLimit {
    /// The same limit for every source and setting.
    Fixed {
        /// The maximum value.
        value: u64,
    },
    /// The source bit depth determines the limit.
    PerBitDepth {
        /// The maximum for 8-bit sources.
        eight: u64,
        /// The maximum for 10-bit sources.
        ten: u64,
    },
    /// Another member of the same record determines the limit.
    Field {
        /// The key of that member.
        key: String,
    },
}

/// A member of a compound value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptionMember {
    /// The member's value shape.
    pub shape: ValueShape,
    /// The member's description.
    pub help: String,
    /// Conditions that affect availability or validation.
    pub applicability: String,
    /// The reason the member is read-only, when applicable.
    pub read_only: Option<String>,
}

/// The members and description of one mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptionMode {
    /// The mode's description.
    pub help: String,
    /// The remaining members of the record, indexed by key.
    pub fields: BTreeMap<String, OptionMember>,
}

/// Information the engine supplies for one option.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptionDescriptor {
    /// Stable, searchable option key.
    pub key: String,
    /// Group of related controls.
    pub group: String,
    /// The user-visible field description and applicability conditions.
    pub help: String,
    /// The accepted input structure.
    pub shape: ValueShape,
    /// Expert controls require explicit disclosure.
    pub expert: bool,
}

/// A resolved value and its origin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveOption {
    /// Canonical option key.
    pub key: String,
    /// Explicit override, if supplied.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_value"
    )]
    pub requested: Option<OptionValue>,
    /// Value or per-context values for ordinary content.
    pub value: OptionValue,
    /// Possible frame-coder value for screen content. Sequence setup and planning use the normal row.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_value"
    )]
    pub screen: Option<OptionValue>,
    /// How the encoder selected the ordinary value.
    pub origin: OptionValue,
    /// How the encoder selected the screen-content value.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_value"
    )]
    pub screen_origin: Option<OptionValue>,
}

fn present_value<'de, D: serde::Deserializer<'de>>(
    input: D,
) -> Result<Option<OptionValue>, D::Error> {
    OptionValue::deserialize(input).map(Some)
}

/// A validation error for one input field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldError {
    /// Canonical field key.
    pub field: String,
    /// Reason and any applicable limit.
    pub message: String,
}

impl FieldError {
    /// Identifies the input that requires correction.
    pub fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_empty_value_survives_the_protocol() -> Result<(), serde_json::Error> {
        for requested in [
            None,
            Some(OptionValue::None),
            Some(OptionValue::Bool(false)),
            Some(OptionValue::Integer(0)),
        ] {
            let row = EffectiveOption {
                key: "resize_denominator".into(),
                requested,
                value: OptionValue::None,
                origin: OptionValue::Choice("explicit".into()),
                screen: None,
                screen_origin: None,
            };
            let encoded = serde_json::to_vec(&row)?;
            assert_eq!(serde_json::from_slice::<EffectiveOption>(&encoded)?, row);
        }
        Ok(())
    }
}
