// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Translate typed option values at the public preset boundary.

use splot_encode::preset::{
    CoeffLevel, FrameKind, PresetContext, PresetField, PresetFieldShape, PresetFieldValue,
    PresetValueKind, ResolutionClass,
};
use studio_domain::options::{FieldError, OptionDescriptor, OptionValue, ValueShape};

const FRAMES: [(&str, FrameKind); 4] = [
    ("key", FrameKind::Key),
    ("base", FrameKind::Base),
    ("mid", FrameKind::Mid),
    ("leaf", FrameKind::Leaf),
];

pub(super) fn describe(field: &PresetField) -> OptionDescriptor {
    let shape = match field.shape {
        PresetFieldShape::Scalar(kind) => shape(field, kind),
        PresetFieldShape::Contextual(kind) => ValueShape::PerFrame {
            value: Box::new(shape(field, kind)),
            frames: FRAMES.iter().map(|(name, _)| (*name).into()).collect(),
        },
    };
    OptionDescriptor {
        key: format!("preset.{}", field.name),
        group: field.group.name().into(),
        help: field.help.into(),
        shape,
        expert: true,
    }
}

fn shape(field: &PresetField, kind: PresetValueKind) -> ValueShape {
    match kind {
        PresetValueKind::Bool => ValueShape::Bool,
        PresetValueKind::I8 | PresetValueKind::U8 | PresetValueKind::U16 => field
            .integer_range
            .map_or(ValueShape::ReadOnly, |r| ValueShape::Integer {
                min: i64::from(r.min),
                max: i64::from(r.max),
            }),
        PresetValueKind::BoolTriple | PresetValueKind::U8Triple => ValueShape::List {
            element: Box::new(if kind == PresetValueKind::BoolTriple {
                ValueShape::Bool
            } else {
                shape(field, PresetValueKind::U8)
            }),
            length: Some(3),
        },
    }
}

pub(super) fn encode_value(field: &PresetField, value: &OptionValue) -> Result<String, FieldError> {
    let encoded = match field.shape {
        PresetFieldShape::Scalar(kind) => encode_scalar(kind, value),
        PresetFieldShape::Contextual(kind) => encode_scalar(kind, value).or_else(|| {
            let OptionValue::List(values) = value else {
                return None;
            };
            if values.len() != FRAMES.len() {
                return None;
            }
            let encoded = values
                .iter()
                .map(|v| encode_scalar(kind, v))
                .collect::<Option<Vec<_>>>()?;
            Some(encoded.join(if kind == PresetValueKind::U8Triple {
                "/"
            } else {
                ","
            }))
        }),
    };
    encoded.ok_or_else(|| FieldError::new(format!("preset.{}", field.name), field.expected))
}

fn encode_scalar(kind: PresetValueKind, value: &OptionValue) -> Option<String> {
    match (kind, value) {
        (PresetValueKind::Bool, OptionValue::Bool(v)) => Some(v.to_string()),
        (
            PresetValueKind::U8 | PresetValueKind::U16 | PresetValueKind::I8,
            OptionValue::Integer(v),
        ) => Some(v.to_string()),
        (PresetValueKind::BoolTriple | PresetValueKind::U8Triple, OptionValue::List(values))
            if values.len() == 3 =>
        {
            let element = if kind == PresetValueKind::BoolTriple {
                PresetValueKind::Bool
            } else {
                PresetValueKind::U8
            };
            Some(
                values
                    .iter()
                    .map(|v| encode_scalar(element, v))
                    .collect::<Option<Vec<_>>>()?
                    .join(","),
            )
        }
        _ => None,
    }
}

pub(super) fn read(value: PresetFieldValue) -> OptionValue {
    match value {
        PresetFieldValue::Bool(v) => OptionValue::Bool(v),
        PresetFieldValue::I8(v) => OptionValue::Integer(i64::from(v)),
        PresetFieldValue::U8(v) => OptionValue::Integer(i64::from(v)),
        PresetFieldValue::U16(v) => OptionValue::Integer(i64::from(v)),
        PresetFieldValue::BoolTriple(v) => OptionValue::List(v.map(OptionValue::Bool).into()),
        PresetFieldValue::U8Triple(v) => {
            OptionValue::List(v.map(|n| OptionValue::Integer(i64::from(n))).into())
        }
    }
}

/// A runtime policy contains one entry per frame kind and coefficient level at the coded size.
pub(super) fn contexts(
    field: &PresetField,
    resolution: ResolutionClass,
    mut value: impl FnMut(PresetContext) -> OptionValue,
) -> OptionValue {
    if matches!(field.shape, PresetFieldShape::Scalar(_)) {
        return value(PresetContext {
            resolution,
            ..Default::default()
        });
    }
    let mut cells = std::collections::BTreeMap::new();
    for (frame_name, frame) in FRAMES {
        for (level_name, coeff) in [
            ("low", CoeffLevel::Low),
            ("normal", CoeffLevel::Normal),
            ("high", CoeffLevel::High),
        ] {
            cells.insert(
                format!("{frame_name}.{level_name}"),
                value(PresetContext {
                    resolution,
                    frame,
                    coeff,
                }),
            );
        }
    }
    OptionValue::Record(cells)
}
