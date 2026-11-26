// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Describe and resolve encoder settings without opening an output.

pub(crate) mod config;
mod descriptor;
mod preset;

use std::ops::Range;

use splot_encode::Config;
use splot_encode::config::options::{OPTIONS, OptionClass, OptionDescription};
use splot_encode::config::{PresetOrigin, ResolvedConfig, ResolvedPreset};
use splot_encode::preset::{PresetField, PresetParams};
use studio_domain::job::SourceFacts;
use studio_domain::options::{
    EffectiveOption, FieldError, MAX_OPTIONS_PAGE_ITEMS, OPTIONS_SCHEMA_VERSION, OptionDescriptor,
    OptionRequest, OptionValue,
};
use studio_domain::protocol::{Envelope, Event, write_message};

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
    let config = config::from_source(source, settings.base)?;
    let (config, resolved) = prepare(config, settings)?;
    Ok((
        total,
        effective(&config, &resolved, source, settings, range),
    ))
}

fn prepare(
    mut config: Config,
    settings: &OptionRequest,
) -> Result<(Config, ResolvedConfig), FieldError> {
    apply(&mut config, settings)?;
    let resolved = config
        .resolve()
        .map_err(|error| config::error(&config, &error))?;
    Ok((config, resolved))
}

pub(crate) fn job_policy(
    config: Config,
    source: &SourceFacts,
    settings: &OptionRequest,
) -> Result<(Config, Vec<Event>), FieldError> {
    let (config, resolved) = prepare(config, settings)?;
    let schema = OPTIONS_SCHEMA_VERSION;
    let (total, _) = page(schema, 0, 1)?;
    let mut first = 0;
    let mut events = Vec::new();
    while first < total {
        let mut count = (total - first).min(MAX_OPTIONS_PAGE_ITEMS);
        loop {
            let (_, range) = page(schema, first, count)?;
            let values = effective(&config, &resolved, source, settings, range);
            let field = values
                .first()
                .map_or_else(|| "policy".into(), |value| value.key.clone());
            let envelope = Envelope::new(
                u64::MAX,
                Event::JobPolicy {
                    first,
                    total,
                    values,
                },
            );
            if write_message(&mut std::io::sink(), &envelope).is_ok() {
                events.push(envelope.body);
                first += count;
                break;
            }
            if count == 1 {
                return Err(FieldError::new(
                    field,
                    "The option exceeds the job policy message limit.",
                ));
            }
            count /= 2;
        }
    }
    Ok((config, events))
}

fn effective(
    config: &Config,
    resolved: &ResolvedConfig,
    source: &SourceFacts,
    settings: &OptionRequest,
    range: Range<usize>,
) -> Vec<EffectiveOption> {
    fields(range)
        .map(|field| match field {
            Field::Config(field) => {
                let requested = settings.values.get(field.key).cloned();
                let label = match field.class {
                    OptionClass::Source => "source",
                    OptionClass::FirstPass => "pass workflow",
                    OptionClass::PresetOverrides => "individual preset fields",
                    _ if requested.is_some() => "explicit",
                    _ if matches!(field.key, "rate_control" | "preset") => "simple",
                    _ => "codec default",
                };
                let origin = if field.class == OptionClass::Mixed {
                    config::content_origin(source, requested.as_ref())
                } else {
                    OptionValue::Choice(label.into())
                };
                EffectiveOption {
                    key: field.key.into(),
                    requested,
                    value: config::value(config, field.key).unwrap_or(OptionValue::None),
                    origin,
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
        .collect()
}
