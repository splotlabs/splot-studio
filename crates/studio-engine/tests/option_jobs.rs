// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Execute typed settings through jobs and the CLI with the pinned encoder.

mod common;

use std::fs;
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::Path;
use std::process::Command;

use common::{Clip, TestResult, leftovers, run_announced, spec};
use splot_encode::ivf::IvfWriter;
use splot_encode::{
    BitDepth, Config, Encoder, Preset, RateControlMode, SourceFrame, ThreadCount, TileLayout, Tune,
};
use studio_domain::job::{FailureCode, JobSpec, WirePath};
use studio_domain::options::OptionValue;
use studio_domain::protocol::Event;
use studio_domain::settings::OutputKind;
use studio_engine::{job, source};

const BIN: &str = env!("CARGO_BIN_EXE_splot-studio-engine");

fn source_config(path: &Path) -> Result<Config, Box<dyn std::error::Error>> {
    let source = source::open(path, source::Selection::default()).map_err(|err| err.to_string())?;
    let mut config = Config::new(source.header.width, source.header.height);
    config.chroma = source.header.format;
    config.bit_depth = if source.header.bit_depth == 10 {
        BitDepth::Ten
    } else {
        BitDepth::Eight
    };
    config.frame_rate = source.header.frame_rate;
    config.content = source.content;
    Ok(config)
}

fn reference_ivf(clip: Clip, config: Config) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let handle = job::spawn(move || {
        let mut encoder = Encoder::new(config.clone())?;
        for frame in 0..clip.frames {
            let mut planes = clip.samples(frame);
            planes.resize(3, Vec::new());
            encoder.send_frame(SourceFrame::new(&config, frame as u64, planes)?)?;
        }
        encoder.flush()?;
        let mut writer = IvfWriter::new(Vec::new(), &config)?;
        while let Some(packet) = encoder.receive_packet()? {
            writer.write_packet(&packet)?;
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(writer.finish()?)
    })?;
    handle
        .join()
        .map_err(|_| "the reference encoder thread panicked")?
        .map_err(|err| err.to_string().into())
}

fn assert_bytes(path: &Path, expected: &[u8]) -> TestResult {
    assert!(
        fs::read(path)? == expected,
        "{} differs from the same settings applied with the codec API",
        path.display()
    );
    Ok(())
}

#[test]
fn legacy_simple_job_matches_its_original_crf_encode_at_both_depths() -> TestResult {
    for colorspace in ["420", "420p10"] {
        let dir = tempfile::tempdir()?;
        let input = dir.path().join("input.y4m");
        let output = dir.path().join("legacy.ivf");
        let clip = Clip::new(32, 16, colorspace, 3);
        clip.write(&input)?;

        let request = spec(&input, &output, OutputKind::Ivf);
        let mut legacy = serde_json::to_value(&request)?;
        let fields = legacy.as_object_mut().ok_or("expected a job record")?;
        fields.remove("identity");
        fields.remove("pass_artifact");
        fields.insert(
            "settings".into(),
            serde_json::to_value(request.settings.base)?,
        );
        let job: JobSpec = serde_json::from_value(legacy)?;
        assert_eq!(job.settings, request.settings);

        let (_, end, _) = run_announced(job, |_, _| {});
        assert!(matches!(end, Event::Completed { .. }), "{end:?}");
        let mut config = source_config(&input)?;
        config.preset = Preset::new(10)?;
        config.rate_control = RateControlMode::Crf { crf: 40 };
        assert_bytes(&output, &reference_ivf(clip, config)?)?;
    }
    Ok(())
}

#[test]
fn typed_rate_modes_execute_with_the_requested_codec_settings() -> TestResult {
    let dir = tempfile::tempdir()?;
    let input = dir.path().join("input.y4m");
    let clip = Clip::new(32, 16, "420p10", 4);
    clip.write(&input)?;

    for (name, rate, request) in [
        (
            "crf",
            RateControlMode::Crf { crf: 28 },
            r#"{"mode":"crf","crf":28}"#,
        ),
        (
            "constant_qp",
            RateControlMode::ConstantQp { qp: 96 },
            r#"{"mode":"constant_qp","qp":96}"#,
        ),
        (
            "capped_crf",
            RateControlMode::CappedCrf {
                crf: 24,
                max_bitrate_kbps: 40,
                buffer_ms: 600,
                initial_ms: 300,
            },
            r#"{"mode":"capped_crf","crf":24,"max_bitrate_kbps":40,"buffer_ms":600,"initial_ms":300}"#,
        ),
        (
            "vbr",
            RateControlMode::Vbr { bitrate_kbps: 32 },
            r#"{"mode":"vbr","bitrate_kbps":32}"#,
        ),
        (
            "cbr",
            RateControlMode::Cbr {
                bitrate_kbps: 32,
                buffer_ms: 600,
                initial_ms: 300,
            },
            r#"{"mode":"cbr","bitrate_kbps":32,"buffer_ms":600,"initial_ms":300}"#,
        ),
    ] {
        let output = dir.path().join(format!("{name}.ivf"));
        let mut job = spec(&input, &output, OutputKind::Ivf);
        job.identity = Some(job::options_identity());
        job.settings
            .values
            .insert("rate_control".into(), serde_json::from_str(request)?);

        let (_, end, _) = run_announced(job, |_, _| {});
        assert!(matches!(end, Event::Completed { .. }), "{name}: {end:?}");
        let mut config = source_config(&input)?;
        config.preset = Preset::new(10)?;
        config.rate_control = rate;
        assert_bytes(&output, &reference_ivf(clip.clone(), config)?)?;
    }
    Ok(())
}

#[test]
fn cli_options_reach_the_encoder_and_change_the_output() -> TestResult {
    let dir = tempfile::tempdir()?;
    let input = dir.path().join("input.y4m");
    let simple = dir.path().join("simple.ivf");
    let advanced = dir.path().join("advanced.ivf");
    let clip = Clip::new(256, 32, "420", 3);
    clip.write(&input)?;

    let run = |output: &Path, options: &[&str]| -> TestResult {
        let result = Command::new(BIN)
            .arg("encode")
            .arg(&input)
            .arg(output)
            .args(options)
            .output()?;
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        Ok(())
    };

    run(&simple, &[])?;
    let mut config = source_config(&input)?;
    config.preset = Preset::new(6)?;
    config.rate_control = RateControlMode::Crf { crf: 20 };
    assert_bytes(&simple, &reference_ivf(clip.clone(), config.clone())?)?;

    run(
        &advanced,
        &[
            "--option",
            "tile_groups",
            "2",
            "--option",
            "tiles",
            r#"{"mode":"uniform","cols_log2":1,"rows_log2":0}"#,
            "--option",
            "threads",
            r#"{"mode":"fixed","count":2}"#,
            "--option",
            "keyint",
            "2",
            "--option",
            "film_grain",
            "true",
            "--option",
            "tune",
            r#""vq""#,
            "--option",
            "preset.min_block_log2",
            "[3,4,4,4]",
            "--option",
            "preset.rdoq",
            "false",
        ],
    )?;
    config.tiles = TileLayout::Uniform {
        cols_log2: 1,
        rows_log2: 0,
    };
    config.tile_groups = NonZeroU32::new(2).ok_or("invalid tile group count")?;
    config.threads = ThreadCount::Fixed(NonZeroUsize::new(2).ok_or("invalid thread count")?);
    config.keyint = NonZeroU32::new(2).ok_or("invalid keyframe interval")?;
    config.film_grain = true;
    config.tune = Tune::Vq;
    config.preset_overrides = vec![
        ("min_block_log2".into(), "3,4,4,4".into()),
        ("rdoq".into(), "false".into()),
    ];
    assert_bytes(&advanced, &reference_ivf(clip, config)?)?;
    assert_ne!(fs::read(simple)?, fs::read(advanced)?);
    Ok(())
}

#[test]
fn invalid_options_and_stale_identity_fail_before_output_creation() -> TestResult {
    let dir = tempfile::tempdir()?;
    let input = dir.path().join("input.y4m");
    let output = dir.path().join("out.ivf");
    Clip::new(32, 16, "420", 1).write(&input)?;
    let plain = spec(&input, &output, OutputKind::Ivf);
    let mut invalid = plain.clone();
    invalid.identity = Some(job::options_identity());
    invalid.settings.values.insert(
        "preset.bitrate_mini_gop_size".into(),
        OptionValue::Integer(10),
    );

    let mut stale_schema = plain.clone();
    let mut identity = job::options_identity();
    identity.schema += 1;
    stale_schema.identity = Some(identity);

    let mut stale_codec = plain.clone();
    let mut identity = job::options_identity();
    identity.codec_revision = "0".repeat(40);
    stale_codec.identity = Some(identity);

    let mut pass = plain;
    pass.identity = Some(job::options_identity());
    pass.pass_artifact = Some(WirePath::from_path(&dir.path().join("first-pass.bin")));

    for (job, field) in [
        (invalid, "preset.bitrate_mini_gop_size"),
        (stale_schema, "identity"),
        (stale_codec, "identity"),
        (pass, "pass_artifact"),
    ] {
        let (_, end, working) = run_announced(job, |_, _| {});
        let Event::Failed { failure } = end else {
            return Err(format!("expected a settings failure: {end:?}").into());
        };
        assert_eq!(failure.code, FailureCode::InvalidSettings);
        assert!(failure.message.starts_with(field), "{failure:?}");
        assert!(working.is_none());
        assert!(leftovers(dir.path(), &[&input]).is_empty());
    }
    Ok(())
}
