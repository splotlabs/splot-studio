// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Describe and resolve encoder settings without opening an output.

pub(crate) mod config;
mod descriptor;
mod preset;

use std::ops::Range;

use splot_encode::Config;
use splot_encode::config::options::{OPTIONS, OptionClass, OptionDescription};
use splot_encode::config::{PresetOrigin, ResolvedPreset};
use splot_encode::preset::{PresetField, PresetParams};
use studio_domain::job::SourceFacts;
use studio_domain::options::{
    EffectiveOption, FieldError, MAX_OPTIONS_PAGE_ITEMS, OPTIONS_SCHEMA_VERSION, OptionDescriptor,
    OptionRequest, OptionValue,
};

enum Field {
    Config(&'static OptionDescription),
    Preset(&'static PresetField),
}

fn fields(page: Range<usize>) -> impl Iterator<Item = Field> {
    OPTIONS
        .iter()
        .map(Field::Config)
        .chain(PresetParams::FIELD_DESCRIPTORS.iter().map(Field::Preset))
        .skip(page.start)
        .take(page.len())
}

fn page(schema: u32, first: u32, count: u32) -> Result<(u32, Range<usize>), FieldError> {
    if schema != OPTIONS_SCHEMA_VERSION {
        return Err(FieldError::new(
            "schema",
            format!(
                "Use option schema {OPTIONS_SCHEMA_VERSION}. The request specifies schema {schema}."
            ),
        ));
    }
    let total =
        u32::try_from(OPTIONS.len() + PresetParams::FIELD_DESCRIPTORS.len()).map_err(|_| {
            FieldError::new(
                "count",
                "The number of encoder options exceeds the protocol limit.",
            )
        })?;
    if first >= total || !(1..=MAX_OPTIONS_PAGE_ITEMS).contains(&count) {
        return Err(FieldError::new(
            "page",
            format!(
                "Select a first option below {total} and a count from 1 through {MAX_OPTIONS_PAGE_ITEMS}."
            ),
        ));
    }
    Ok((
        total,
        first as usize..first.saturating_add(count).min(total) as usize,
    ))
}

pub(crate) fn describe(
    schema: u32,
    first: u32,
    count: u32,
) -> Result<(u32, Vec<OptionDescriptor>), FieldError> {
    let (total, range) = page(schema, first, count)?;
    Ok((
        total,
        fields(range)
            .map(|field| match field {
                Field::Config(f) => descriptor::describe(f),
                Field::Preset(f) => preset::describe(f),
            })
            .collect(),
    ))
}

fn apply(config: &mut Config, settings: &OptionRequest) -> Result<(), FieldError> {
    for (key, value) in &settings.values {
        if let Some(name) = key.strip_prefix("preset.") {
            let field = PresetParams::field(name).ok_or_else(|| {
                FieldError::new(key, "The codec does not recognize this preset option.")
            })?;
            config
                .preset_overrides
                .push((name.to_owned(), preset::encode_value(field, value)?));
        } else {
            config::apply(config, key, value)?;
        }
    }
    Ok(())
}

pub(crate) fn resolve(
    schema: u32,
    source: &SourceFacts,
    settings: &OptionRequest,
    first: u32,
    count: u32,
) -> Result<(u32, Vec<EffectiveOption>), FieldError> {
    let (total, range) = page(schema, first, count)?;
    let mut config = config::from_source(source, settings.base)?;
    apply(&mut config, settings)?;
    let resolved = config
        .resolve()
        .map_err(|error| config::error(&config, &error))?;
    let values = fields(range)
        .map(|field| match field {
            Field::Config(field) => {
                let requested = settings.values.get(field.key).cloned();
                let origin = match field.class {
                    OptionClass::Source => "source",
                    OptionClass::FirstPass => "pass workflow",
                    OptionClass::PresetOverrides => "individual preset fields",
                    _ if requested.is_some() => "explicit",
                    _ if matches!(field.key, "rate_control" | "preset") => "simple",
                    _ => "codec default",
                };
                EffectiveOption {
                    key: field.key.into(),
                    requested,
                    value: config::value(&config, field.key).unwrap_or(OptionValue::None),
                    origin: OptionValue::Choice(origin.into()),
                    screen: None,
                    screen_origin: None,
                }
            }
            Field::Preset(field) => {
                let key = format!("preset.{}", field.name);
                let value = |row: &ResolvedPreset| {
                    preset::contexts(field, resolved.resolution, |context| {
                        preset::read(field.value(&row.params, context))
                    })
                };
                let origin = |row: &ResolvedPreset| {
                    preset::contexts(field, resolved.resolution, |context| {
                        let source = match row.origin(field, context) {
                            PresetOrigin::Preset => "preset",
                            PresetOrigin::ScreenContent => "screen content",
                            PresetOrigin::Tune => "tune",
                            PresetOrigin::Explicit => "explicit",
                            PresetOrigin::Lossless => "lossless",
                            PresetOrigin::FastDecode => "fast decode",
                            _ => "codec policy",
                        };
                        OptionValue::Choice(source.into())
                    })
                };
                EffectiveOption {
                    requested: settings.values.get(&key).cloned(),
                    key,
                    value: value(&resolved.normal),
                    origin: origin(&resolved.normal),
                    screen: resolved.screen.as_ref().map(value),
                    screen_origin: resolved.screen.as_ref().map(origin),
                }
            }
        })
        .collect();
    Ok((total, values))
}
