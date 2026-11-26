// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use std::collections::BTreeMap;

use splot_encode::BitDepth;
use splot_encode::config::options::{
    Limit, OptionClass, OptionDescription, ValueShape as CodecShape,
};
use splot_encode::rate_control::max_qindex;
use studio_domain::options::{
    OptionDescriptor, OptionMember, OptionMode, UnsignedLimit, ValueShape,
};

use super::config::MAX_METADATA_BYTES;

pub(super) fn describe(field: &OptionDescription) -> OptionDescriptor {
    let read_only = read_only(field);
    let mut help = [
        field.help,
        field.applicability,
        read_only.unwrap_or_default(),
    ]
    .into_iter()
    .filter(|text| !text.is_empty())
    .collect::<Vec<_>>()
    .join(" ");
    if field.key == "key_frame_metadata" {
        use std::fmt::Write as _;
        let _ = write!(
            help,
            " The total payload limit is {MAX_METADATA_BYTES} bytes."
        );
    }
    OptionDescriptor {
        key: field.key.to_owned(),
        group: if field.class == OptionClass::Source {
            "Source"
        } else {
            "Encoder"
        }
        .to_owned(),
        help,
        shape: if read_only.is_some() {
            ValueShape::ReadOnly
        } else {
            shape(field.shape)
        },
        expert: read_only.is_none(),
    }
}

fn read_only(field: &OptionDescription) -> Option<&'static str> {
    match field.class {
        OptionClass::Source => Some("Studio takes this value from the source."),
        OptionClass::FirstPass => Some("A verified first-pass job supplies these values."),
        OptionClass::PresetOverrides => Some("Set each override in its own preset option."),
        OptionClass::Setting if field.key == "preset" => {
            Some("Select the preset in Simple settings.")
        }
        OptionClass::Setting | OptionClass::Mixed => None,
    }
}

fn members(fields: &[OptionDescription]) -> BTreeMap<String, OptionMember> {
    fields
        .iter()
        .map(|field| {
            (
                field.key.to_owned(),
                OptionMember {
                    shape: shape(field.shape),
                    help: field.help.to_owned(),
                    applicability: field.applicability.to_owned(),
                    read_only: read_only(field).map(str::to_owned),
                },
            )
        })
        .collect()
}

fn shape(encoder: CodecShape) -> ValueShape {
    match encoder {
        CodecShape::Boolean => ValueShape::Bool,
        CodecShape::Integer { min, max, unit } => ValueShape::Unsigned {
            min,
            max: match max {
                Limit::Fixed(value) => UnsignedLimit::Fixed {
                    value: value.min(i64::MAX.unsigned_abs()),
                },
                Limit::QuantizerIndex => UnsignedLimit::PerBitDepth {
                    eight: u64::from(max_qindex(BitDepth::Eight)),
                    ten: u64::from(max_qindex(BitDepth::Ten)),
                },
                Limit::Field(key) => UnsignedLimit::Field {
                    key: key.to_owned(),
                },
            },
            unit: unit.map(str::to_owned),
        },
        CodecShape::Ratio => ValueShape::Ratio,
        CodecShape::Bytes => ValueShape::Bytes {
            max_length: MAX_METADATA_BYTES,
        },
        CodecShape::Optional(inner) => ValueShape::Optional {
            value: Box::new(shape(*inner)),
        },
        CodecShape::List(inner) => ValueShape::List {
            element: Box::new(shape(*inner)),
            length: None,
        },
        CodecShape::Record(fields) => ValueShape::Record {
            fields: members(fields),
        },
        CodecShape::Choice(variants)
            if variants.iter().all(|variant| variant.fields.is_empty()) =>
        {
            ValueShape::Choice {
                values: variants
                    .iter()
                    .map(|variant| variant.key.to_owned())
                    .collect(),
            }
        }
        CodecShape::Choice(variants) => ValueShape::Modes {
            modes: variants
                .iter()
                .map(|variant| {
                    (
                        variant.key.to_owned(),
                        OptionMode {
                            help: variant.help.to_owned(),
                            fields: members(variant.fields),
                        },
                    )
                })
                .collect(),
        },
        CodecShape::Overrides => ValueShape::ReadOnly,
    }
}
