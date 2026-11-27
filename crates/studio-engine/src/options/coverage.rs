// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use std::collections::{BTreeMap, BTreeSet};

use serde_json::json;
use splot_encode::Preset;
use splot_encode::config::options::ValueShape as CodecShape;
use splot_encode::preset::{CoeffLevel, FrameKind, PresetContext, PresetFieldShape};
use studio_domain::options::ValueShape;

use super::*;
use crate::source::{self, Source};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn request() -> TestResult<OptionRequest> {
    Ok(serde_json::from_value(json!({
        "base":{"output":"ivf","preset":10,"crf":40},
        "values":{}
    }))?)
}

fn descriptors() -> TestResult<Vec<OptionDescriptor>> {
    let mut result = Vec::new();
    loop {
        let first = u32::try_from(result.len())?;
        let (total, mut page) = describe(OPTIONS_SCHEMA_VERSION, first, MAX_OPTIONS_PAGE_ITEMS)
            .unwrap_or_else(|error| unreachable!("{error:?}"));
        assert_ne!(page.len(), 0);
        result.append(&mut page);
        if result.len() == usize::try_from(total)? {
            return Ok(result);
        }
    }
}

fn fixture(dir: &tempfile::TempDir) -> TestResult<Source> {
    let path = dir.path().join("source.y4m");
    let mut bytes = b"YUV4MPEG2 W128 H64 F25:1 Ip A1:1 C420mpeg2\nFRAME\n".to_vec();
    bytes.resize(bytes.len() + 128 * 64 * 3 / 2, 128);
    std::fs::write(&path, bytes)?;
    source::open(&path, source::Selection::default()).map_err(|error| error.to_string().into())
}

fn base(source: &Source, request: &OptionRequest) -> Config {
    config::from_header(&source.header, source.content, request.base)
        .unwrap_or_else(|error| unreachable!("{error:?}"))
}

fn policy(
    source: &Source,
    request: &OptionRequest,
) -> TestResult<BTreeMap<String, EffectiveOption>> {
    let decoded: OptionRequest = serde_json::from_slice(&serde_json::to_vec(request)?)?;
    assert_eq!(&decoded, request);
    let (_, events) = job_policy(base(source, &decoded), &source.facts, &decoded)
        .unwrap_or_else(|error| unreachable!("{error:?}"));
    let mut rows = BTreeMap::new();
    for event in events {
        let Event::JobPolicy { values, .. } = event else {
            unreachable!("Expected a page of effective options.");
        };
        for value in values {
            assert!(rows.insert(value.key.clone(), value).is_none());
        }
    }
    Ok(rows)
}

#[test]
fn descriptors_cover_the_codec_fields_and_exact_writable_surface() -> TestResult {
    let rows = descriptors()?;
    let described: BTreeSet<_> = rows.iter().map(|row| row.key.clone()).collect();
    let authoritative: BTreeSet<_> = OPTIONS
        .iter()
        .map(|field| field.key.to_owned())
        .chain(
            PresetParams::FIELDS
                .iter()
                .map(|name| format!("preset.{name}")),
        )
        .collect();
    assert_eq!(described.len(), rows.len());
    assert_eq!(described, authoritative);

    let writable: BTreeSet<_> = rows
        .iter()
        .filter(|row| row.shape != ValueShape::ReadOnly)
        .map(|row| row.key.clone())
        .collect();
    let implemented: BTreeSet<_> = top_level_examples()?
        .into_keys()
        .chain(
            PresetParams::FIELDS
                .iter()
                .map(|name| format!("preset.{name}")),
        )
        .collect();
    assert_eq!(writable, implemented);
    Ok(())
}

fn top_level_examples() -> serde_json::Result<BTreeMap<String, OptionValue>> {
    serde_json::from_value(json!({
        "rate_control":{"mode":"constant_qp","qp":37},
        "tiles":{"mode":"uniform","cols_log2":1,"rows_log2":0},
        "tile_groups":2,
        "keyint":48,
        "resize_denominator":10,
        "crop":{"left":2,"right":0,"top":2,"bottom":0},
        "threads":{"mode":"fixed","count":2},
        "content":{"timing_info":true},
        "key_frame_metadata":{"icc_profile":vec![0; 128],"itut_t35":[181,0,1],"user_data_unregistered":vec![0; 16]},
        "frame_metadata":{"timecode":true,"temporal_point_info":false,"scan_type":true,"banding_hints":[168,166]},
        "film_grain":true,
        "cbr_padding":true,
        "tune":"ssim",
        "fast_decode":true,
        "multi_frame_header":true
    }))
}

#[test]
fn every_editable_top_level_field_survives_the_job_policy_path() -> TestResult {
    let dir = tempfile::tempdir()?;
    let source = fixture(&dir)?;
    for (key, expected) in top_level_examples()? {
        let mut request = request()?;
        if key == "cbr_padding" {
            request.values.insert(
                "rate_control".into(),
                serde_json::from_value(
                    json!({"mode":"cbr","bitrate_kbps":40,"buffer_ms":600,"initial_ms":300}),
                )?,
            );
        }
        request.values.insert(key.clone(), expected.clone());
        let rows = policy(&source, &request)?;
        let row = &rows[&key];
        assert_eq!(row.requested.as_ref(), Some(&expected), "{key}");
        if key == "content" {
            let OptionValue::Record(content) = &row.value else {
                unreachable!("Expected effective content.");
            };
            assert_eq!(content["timing_info"], OptionValue::Bool(true));
            assert_eq!(content["chroma_sample_position"], choice("left"));
            let OptionValue::Record(origin) = &row.origin else {
                unreachable!("Expected content origins.");
            };
            assert_eq!(origin["timing_info"], choice("explicit"));
            assert_eq!(origin["chroma_sample_position"], choice("source"));
        } else {
            assert_eq!(row.value, expected, "{key}");
            assert_eq!(row.origin, choice("explicit"), "{key}");
        }
    }
    Ok(())
}

fn choice(value: &str) -> OptionValue {
    OptionValue::Choice(value.into())
}

#[test]
fn all_preset_values_survive_serialization_and_codec_application() -> TestResult {
    let dir = tempfile::tempdir()?;
    let source = fixture(&dir)?;
    let mut request = request()?;
    let row = Preset::new(request.base.preset.get())?.params();
    for field in PresetParams::FIELD_DESCRIPTORS {
        let at = |frame| {
            preset::read(field.value(
                row,
                PresetContext {
                    frame,
                    coeff: CoeffLevel::Normal,
                    ..PresetContext::default()
                },
            ))
        };
        let value = match field.shape {
            PresetFieldShape::Scalar(_) => at(FrameKind::Key),
            PresetFieldShape::Contextual(_) => OptionValue::List(
                [
                    FrameKind::Key,
                    FrameKind::Base,
                    FrameKind::Mid,
                    FrameKind::Leaf,
                ]
                .map(at)
                .into(),
            ),
        };
        request
            .values
            .insert(format!("preset.{}", field.name), value);
    }
    let rows = policy(&source, &request)?;
    for (key, requested) in request.values {
        let row = &rows[&key];
        assert_eq!(row.requested.as_ref(), Some(&requested), "{key}");
        match (&row.value, &row.origin, &requested) {
            (
                OptionValue::Record(values),
                OptionValue::Record(origins),
                OptionValue::List(requested),
            ) => {
                assert_eq!(values.len(), 12, "{key}");
                assert_eq!(origins.len(), values.len(), "{key}");
                for (index, frame) in ["key", "base", "mid", "leaf"].iter().enumerate() {
                    for coeff in ["low", "normal", "high"] {
                        let cell = format!("{frame}.{coeff}");
                        assert_eq!(values[&cell], requested[index], "{key}.{cell}");
                        assert_eq!(origins[&cell], choice("explicit"), "{key}.{cell}");
                    }
                }
            }
            (value, origin, requested) => {
                assert_eq!(value, requested, "{key}");
                assert_eq!(origin, &choice("explicit"), "{key}");
            }
        }
    }
    Ok(())
}

#[test]
fn source_and_workflow_fields_refuse_direct_overrides() -> TestResult {
    let dir = tempfile::tempdir()?;
    let source = fixture(&dir)?;
    let request = request()?;
    let config = base(&source, &request);
    for key in [
        "width",
        "height",
        "bit_depth",
        "chroma",
        "frame_rate",
        "first_pass",
        "preset_overrides",
        "preset",
        "output",
    ] {
        let mut edited = request.clone();
        edited.values.insert(key.to_owned(), OptionValue::None);
        assert!(
            matches!(
                job_policy(config.clone(), &source.facts, &edited),
                Err(error) if error.field == key && error.message.contains("read-only")
            ),
            "{key}"
        );
    }

    let Some(content) = OPTIONS.iter().find(|field| field.key == "content") else {
        unreachable!("Expected codec content metadata.");
    };
    let CodecShape::Record(members) = content.shape else {
        unreachable!("Expected content metadata fields.");
    };
    for member in members {
        if member.key == "timing_info" {
            assert_eq!(member.class, OptionClass::Setting);
            continue;
        }
        assert_eq!(member.class, OptionClass::Source, "{}", member.key);
        let mut edited = request.clone();
        edited.values.insert(
            "content".into(),
            OptionValue::Record([(member.key.to_owned(), OptionValue::None)].into()),
        );
        assert!(
            matches!(
                job_policy(config.clone(), &source.facts, &edited),
                Err(error) if error.field == format!("content.{}", member.key)
                    && error.message.contains("read-only")
            ),
            "{}",
            member.key
        );
    }
    Ok(())
}
