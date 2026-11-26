// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Decoded exports through the worker protocol and the published files.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use splot_encode::PixelFormat;
use splot_encode::y4m::{Y4mHeader, Y4mReader};
use studio_domain::export::{
    ExportArtifactKind, ExportDurability, ExportFormat, ExportLayer, ExportOutcome, ExportRange,
    ExportRequest, ExportSampleFormat, ExportSource, ExportTemp,
};
use studio_domain::job::{ChromaLayout, FailureCode, Rational, WirePath};
use studio_domain::preview::GrainPolicy;
use studio_domain::protocol::{Command, Event};
use studio_engine::inspect::document::Document;

use super::{Live, TestResult, leftovers};

type Answer<T> = Result<T, Box<dyn Error>>;

const REQUEST: u64 = 51;
const IVF_8: &[u8] = include_bytes!("../preview/fixtures/syn-frame-sef-families-64x64.ivf");
const AVM_8: &[u8] = include_bytes!("../preview/fixtures/syn-frame-sef-families-64x64.ivf.yuv");
const IVF_10: &[u8] = include_bytes!("../preview/fixtures/syn-tlayer2-dependent-10bit.ivf");
const AVM_10: &[u8] = include_bytes!("../preview/fixtures/syn-tlayer2-dependent-10bit.ivf.yuv");
const MONO: &[u8] = include_bytes!("../preview/fixtures/syn-luma-palette2-row-mono-16x16-q0.ivf");
const EDGE_422: &[u8] =
    include_bytes!("../preview/fixtures/syn-3frame-padded-filters-422-134x69-10bit-q150.ivf");
const EDGE_444: &[u8] =
    include_bytes!("../preview/fixtures/syn-mrl-padded-444-67x67-10bit-q120.ivf");
const GRAIN: &[u8] =
    include_bytes!("../preview/fixtures/syn-3frame-filmgrain-inter-64x64-10bit.ivf");
const PQ: &[u8] = include_bytes!("../preview/fixtures/syn-pq-420-10bit-64x64.ivf");
const CROPPED: &[u8] = include_bytes!("../preview/fixtures/syn-2seq-crop-intra-64x64.obu");
const DERIVED_SEF: &[u8] = include_bytes!("../preview/fixtures/syn-mlayer2-derived-sef-8bit.obu");
const AVM_DERIVED_SEF: &[u8] =
    include_bytes!("../preview/fixtures/syn-mlayer2-derived-sef-8bit.obu.yuv");

fn inspected_source(path: &Path) -> Answer<ExportSource> {
    let path = WirePath::from_path(path);
    let opened =
        Document::open(&path, &|| false).map_err(|e| format!("{:?}: {}", e.code, e.message))?;
    let summary = opened.summary();
    Ok(ExportSource {
        path,
        fingerprint: summary.fingerprint,
        digest: *summary.digest.clone().ok_or("inspection digest missing")?,
        framing: summary.framing,
    })
}

fn request(
    input: &Path,
    bit_depth: u8,
    chroma: ChromaLayout,
    format: ExportFormat,
) -> Answer<ExportRequest> {
    Ok(ExportRequest {
        source: inspected_source(input)?,
        range: ExportRange::All,
        layer: None,
        grain: GrainPolicy::Skip,
        frame_rate: None,
        expected: ExportSampleFormat { bit_depth, chroma },
        format,
    })
}

fn y4m(path: &Path) -> ExportFormat {
    ExportFormat::Y4m {
        path: WirePath::from_path(path),
    }
}

fn raw(samples: &Path, layout: &Path) -> ExportFormat {
    ExportFormat::Raw {
        samples: WirePath::from_path(samples),
        layout: WirePath::from_path(layout),
    }
}

fn prepare(worker: &mut Live, export: ExportRequest) -> Answer<Event> {
    worker.send(
        REQUEST,
        Command::ExportPrepare {
            export: Box::new(export),
        },
    )?;
    let mut seen = worker.read_until(|event| {
        matches!(
            event,
            Event::ExportPrepared { .. } | Event::ExportFinished { .. } | Event::Error { .. }
        )
    })?;
    let reply = seen.pop().ok_or("no export reply")?;
    assert_eq!(reply.request_id, REQUEST);
    if let Event::ExportPrepared { files } = &reply.body {
        let created = seen.iter().filter_map(|record| match &record.body {
            Event::ExportTempCreated { file } => {
                assert_eq!(record.request_id, REQUEST);
                Some(file)
            }
            _ => None,
        });
        assert!(files.iter().eq(created), "{seen:?}");
    }
    Ok(reply.body)
}

fn terminal(worker: &mut Live) -> Answer<ExportOutcome> {
    let mut seen = worker
        .read_until(|event| matches!(event, Event::ExportFinished { .. } | Event::Error { .. }))?;
    let reply = seen.pop().ok_or("no terminal reply")?;
    assert_eq!(reply.request_id, REQUEST);
    match reply.body {
        Event::ExportFinished { outcome } => {
            let published = seen.iter().filter_map(|event| match &event.body {
                Event::ExportPublished { file } => {
                    assert_eq!(event.request_id, REQUEST);
                    Some(file)
                }
                _ => None,
            });
            let files = match &outcome {
                ExportOutcome::Completed { files, .. } | ExportOutcome::Partial { files, .. } => {
                    files.as_slice()
                }
                ExportOutcome::Cancelled | ExportOutcome::Failed { .. } => &[],
            };
            assert!(files.iter().eq(published), "{seen:?}");
            Ok(outcome)
        }
        other => Err(format!("unexpected terminal reply {other:?}").into()),
    }
}

fn close(mut worker: Live) -> TestResult {
    drop(worker.stdin.take());
    assert!(worker.child.wait()?.success());
    Ok(())
}

fn run(export: ExportRequest) -> Answer<ExportOutcome> {
    let mut worker = Live::spawn()?;
    let outcome = match prepare(&mut worker, export)? {
        Event::ExportPrepared { files } => {
            assert!(!files.is_empty());
            worker.send(REQUEST, Command::ExportProceed)?;
            terminal(&mut worker)?
        }
        Event::ExportFinished { outcome } => outcome,
        other => return Err(format!("unexpected prepare reply {other:?}").into()),
    };
    close(worker)?;
    Ok(outcome)
}

fn done(outcome: ExportOutcome, frames: u64, files: usize) {
    assert!(
        matches!(outcome, ExportOutcome::Completed { frames: count, files: ref published }
            if count == frames && published.len() == files),
        "{outcome:?}"
    );
}

fn failed(outcome: ExportOutcome) {
    assert!(
        matches!(outcome, ExportOutcome::Failed { .. }),
        "{outcome:?}"
    );
}

fn refused(outcome: ExportOutcome, code: FailureCode) {
    assert!(
        matches!(outcome, ExportOutcome::Failed { ref failure } if failure.code == code),
        "{outcome:?}"
    );
}

fn input(dir: &Path, name: &str, bytes: &[u8]) -> Answer<PathBuf> {
    let path = dir.join(name);
    fs::write(&path, bytes)?;
    Ok(path)
}

fn sha(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    bytes.iter().fold(String::new(), |mut result, b| {
        let _ = write!(result, "{b:02x}");
        result
    })
}

fn reopen_y4m(path: &Path) -> Answer<(Y4mHeader, u64, String)> {
    let mut reader = Y4mReader::new(std::io::BufReader::new(fs::File::open(path)?))?;
    let header = *reader.header();
    let mut frames = 0;
    let mut hash = Sha256::new();
    while let Some(planes) = reader.read_frame()? {
        frames += 1;
        for sample in planes.into_iter().flatten() {
            if header.bit_depth == 8 {
                hash.update([u8::try_from(sample)?]);
            } else {
                hash.update(sample.to_le_bytes());
            }
        }
    }
    Ok((header, frames, hex(&hash.finalize())))
}

fn check_hashes<'a>(
    path: &Path,
    expected: impl IntoIterator<Item = (usize, &'a [u8])>,
) -> TestResult {
    let report: serde_json::Value = serde_json::from_reader(fs::File::open(path)?)?;
    assert_eq!(report["contract"], "splot.decoded_frame_hash");
    assert_eq!(report["version"], 1);
    assert_eq!(report["algorithm"], "splot-dfh-sha256-v1");
    assert_eq!(report["byte_stream"], "av2-output-samples-v1");
    assert_eq!(report["variant"], "raw_intermediate_output");
    let outputs = report["outputs"]
        .as_array()
        .ok_or("missing output hashes")?;
    let mut expected = expected.into_iter();
    for output in outputs {
        let (ordinal, samples) = expected.next().ok_or("unexpected export hash")?;
        assert_eq!(output["ordinal"], ordinal);
        assert_eq!(output["hash"], sha(samples));
    }
    assert!(expected.next().is_none());
    Ok(())
}

#[test]
fn y4m_reopens_with_avm_samples_and_declared_geometry() -> TestResult {
    let dir = tempfile::tempdir()?;
    let cases = [
        (
            MONO,
            8,
            ChromaLayout::Monochrome,
            (16, 16),
            1,
            "418ecf79e4b2564419678cb25c28cbbf0259bf4602dd54f3e2868529d4df261d",
        ),
        (
            IVF_10,
            10,
            ChromaLayout::Yuv420,
            (64, 64),
            4,
            "2dba95f537ff163df9ff77be59f12418e14dbaa338a86a3e47f57ef8a99cb6ec",
        ),
        (
            EDGE_422,
            10,
            ChromaLayout::Yuv422,
            (134, 69),
            3,
            "88ed1eec0b69461416d840c759eaf37570b283d3c02ed18c482aa1a75a4f1a9f",
        ),
        (
            EDGE_444,
            10,
            ChromaLayout::Yuv444,
            (67, 67),
            1,
            "1d03e5c85f4542facb672568ae893e8c80194f322f9ccac111e128fccd1882c1",
        ),
    ];
    for (at, (bytes, depth, layout, geometry, frames, oracle)) in cases.into_iter().enumerate() {
        let source = input(dir.path(), &format!("in-{at}.ivf"), bytes)?;
        let dest = dir.path().join(format!("out-{at}.y4m"));
        let mut export = request(&source, depth, layout, y4m(&dest))?;
        if frames == 1 {
            export.frame_rate = Some(Rational { num: 25, den: 1 });
        }
        done(run(export)?, frames, 1);
        let (header, actual, hash) = reopen_y4m(&dest)?;
        assert_eq!(hash, oracle, "{layout:?}");
        assert_eq!((header.width, header.height), geometry);
        assert_eq!(header.bit_depth, depth);
        assert_eq!(actual, frames);
        assert_eq!(header.frame_rate, (if frames == 1 { 25 } else { 30 }, 1));
        let format = match layout {
            ChromaLayout::Monochrome => PixelFormat::Monochrome,
            ChromaLayout::Yuv420 => PixelFormat::Yuv420,
            ChromaLayout::Yuv422 => PixelFormat::Yuv422,
            ChromaLayout::Yuv444 => PixelFormat::Yuv444,
        };
        assert_eq!(header.format, format);
        assert_eq!(fs::read(source)?, bytes);
    }
    Ok(())
}

fn ivf_timing(ticks: [u64; 4]) -> Answer<Vec<u8>> {
    let mut bytes = IVF_10.to_vec();
    let mut start = 32;
    for tick in ticks {
        let size = u32::from_le_bytes(bytes[start..][..4].try_into()?);
        bytes[start + 4..start + 12].copy_from_slice(&tick.to_le_bytes());
        start += 12 + usize::try_from(size)?;
    }
    assert_eq!(start, bytes.len());
    Ok(bytes)
}

fn annex_b() -> Answer<Vec<u8>> {
    use splot_core::stream_reader::TemporalUnitReader;

    let mut reader = TemporalUnitReader::new(IVF_10);
    let mut bytes = Vec::new();
    while let Some(unit) = reader.next_temporal_unit()? {
        bytes.extend_from_slice(unit.bytes);
    }
    Ok(bytes)
}

#[test]
fn y4m_distinguishes_cadence_from_ivf_ticks_and_unknown_timing() -> TestResult {
    let dir = tempfile::tempdir()?;
    let source = input(dir.path(), "fixed.ivf", &ivf_timing([3, 5, 7, 9])?)?;
    let dest = dir.path().join("fixed.y4m");
    let mut export = request(&source, 10, ChromaLayout::Yuv420, y4m(&dest))?;
    export.range = ExportRange::Inclusive { first: 1, last: 2 };
    done(run(export)?, 2, 1);
    let (header, frames, hash) = reopen_y4m(&dest)?;
    assert_eq!(header.frame_rate, (15, 1));
    assert_eq!(frames, 2);
    assert_eq!(hash, sha(&AVM_10[12_288..36_864]));

    let source = input(dir.path(), "variable.ivf", &ivf_timing([0, 1, 3, 4])?)?;
    let dest = dir.path().join("variable.y4m");
    let mut export = request(&source, 10, ChromaLayout::Yuv420, y4m(&dest))?;
    export.frame_rate = Some(Rational { num: 30, den: 1 });
    failed(run(export)?);
    assert!(!dest.exists());
    assert!(
        leftovers(
            dir.path(),
            &[
                &source,
                &dir.path().join("fixed.ivf"),
                &dir.path().join("fixed.y4m")
            ]
        )
        .is_empty()
    );

    let source = input(dir.path(), "unknown.obu", &annex_b()?)?;
    let dest = dir.path().join("assumed.y4m");
    let mut export = request(&source, 10, ChromaLayout::Yuv420, y4m(&dest))?;
    failed(run(export.clone())?);
    assert!(!dest.exists());
    export.frame_rate = Some(Rational {
        num: 24_000,
        den: 1001,
    });
    done(run(export)?, 4, 1);
    let (header, frames, hash) = reopen_y4m(&dest)?;
    assert_eq!(header.frame_rate, (24_000, 1001));
    assert_eq!((frames, hash), (4, sha(AVM_10)));
    Ok(())
}

#[test]
fn raw_ranges_follow_avm_output_order_without_extra_decode_prerequisites() -> TestResult {
    let dir = tempfile::tempdir()?;
    let cases = [
        ("reordered.ivf", IVF_8.to_vec(), 8, 6144, AVM_8, 2, None),
        (
            "unknown.obu",
            annex_b()?,
            10,
            12_288,
            AVM_10,
            2,
            Some("unknown"),
        ),
        (
            "variable.ivf",
            ivf_timing([0, 1, 3, 4])?,
            10,
            12_288,
            AVM_10,
            3,
            Some("variable"),
        ),
    ];
    for (at, (name, bytes, depth, frame_bytes, reference, last, timing)) in
        cases.into_iter().enumerate()
    {
        let source = input(dir.path(), name, &bytes)?;
        let samples = dir.path().join(format!("out-{at}.yuv"));
        let layout = dir.path().join(format!("out-{at}.json"));
        let mut export = request(&source, depth, ChromaLayout::Yuv420, raw(&samples, &layout))?;
        export.range = ExportRange::Inclusive { first: 1, last };
        done(run(export)?, last, 2);
        assert_eq!(
            fs::read(samples)?,
            reference[frame_bytes..frame_bytes * (usize::try_from(last)? + 1)]
        );
        let layout: serde_json::Value = serde_json::from_reader(fs::File::open(layout)?)?;
        assert_eq!(layout["sample_layout"], "av2-output-samples-v1");
        assert_eq!(layout["source_digest"]["bytes"], bytes.len());
        assert_eq!(layout["source_digest"]["sha256"], sha(&bytes));
        assert_eq!(layout["sample_format"]["bit_depth"], depth);
        assert_eq!(layout["sample_format"]["chroma"], "yuv420");
        assert_eq!(layout["frames"], last);
        assert_eq!(
            (layout["width"].as_u64(), layout["height"].as_u64()),
            (Some(64), Some(64))
        );
        assert_eq!(layout["frame_bytes"], frame_bytes);
        assert_eq!(layout["storage_bits"], if depth == 8 { 8 } else { 16 });
        assert_eq!(
            layout["endianness"],
            if depth == 8 { "none" } else { "little" }
        );
        assert_eq!(layout["padding"], "none");
        let planes = layout["planes"].as_array().ok_or("missing export planes")?;
        assert_eq!(planes.len(), 3);
        for (plane, name) in planes.iter().zip(["y", "u", "v"]) {
            let width = if name == "y" { 64 } else { 32 };
            assert_eq!(plane["name"], name);
            assert_eq!(plane["width"], width);
            assert_eq!(plane["height"], width);
            assert_eq!(plane["row_bytes"], width * if depth == 8 { 1 } else { 2 });
        }
        if let Some(timing) = timing {
            assert_eq!(layout["timing"]["kind"], timing);
        }
        let outputs = layout["outputs"]
            .as_array()
            .ok_or("missing output identities")?;
        assert_eq!(u64::try_from(outputs.len())?, last);
        for (output, ordinal) in outputs.iter().zip(1..=last) {
            assert_eq!(output["ordinal"], ordinal);
            match timing {
                Some("variable") => {
                    let ivf = &output["timing"]["ivf"];
                    assert_eq!(ivf["pts"], [0, 1, 3, 4][usize::try_from(ordinal)?]);
                    assert_eq!(ivf["time_base"], serde_json::json!({"num": 1, "den": 30}));
                }
                Some("unknown") => assert!(output["timing"]["ivf"].is_null()),
                _ => {}
            }
        }
        assert_eq!(fs::read(source)?, bytes);
    }
    Ok(())
}

#[test]
fn annex_b_same_unit_outputs_require_raw_or_hashes_unless_the_layer_is_narrowed() -> TestResult {
    let dir = tempfile::tempdir()?;
    let source = input(dir.path(), "layers.obu", DERIVED_SEF)?;
    let dest = dir.path().join("one-layer.y4m");
    let mut export = request(&source, 8, ChromaLayout::Yuv420, y4m(&dest))?;
    export.frame_rate = Some(Rational { num: 24, den: 1 });

    // The two outputs in each temporal unit share one presentation time.
    failed(run(export.clone())?);
    assert!(!dest.exists());
    export.layer = Some(ExportLayer {
        extended: 0,
        embedded: 1,
        temporal: 0,
    });
    done(run(export)?, 1, 1);
    let (header, frames, hash) = reopen_y4m(&dest)?;
    assert_eq!(header.frame_rate, (24, 1));
    assert_eq!((frames, hash), (1, sha(&AVM_DERIVED_SEF[6144..12_288])));

    let samples = dir.path().join("all-outputs.yuv");
    let layout = dir.path().join("all-outputs.json");
    let export = request(&source, 8, ChromaLayout::Yuv420, raw(&samples, &layout))?;
    done(run(export)?, 4, 2);
    assert_eq!(fs::read(samples)?, AVM_DERIVED_SEF);
    let layout: serde_json::Value = serde_json::from_reader(fs::File::open(layout)?)?;
    assert_eq!(layout["timing"]["kind"], "variable");
    assert_eq!(layout["outputs"][2]["unit_offset"], 783);
    assert_eq!(layout["outputs"][3]["unit_offset"], 783);

    let hashes = dir.path().join("hashes.json");
    let export = request(
        &source,
        8,
        ChromaLayout::Yuv420,
        ExportFormat::Hashes {
            path: WirePath::from_path(&hashes),
        },
    )?;
    done(run(export)?, 4, 1);
    check_hashes(&hashes, AVM_DERIVED_SEF.chunks_exact(6144).enumerate())?;
    Ok(())
}

#[test]
fn the_single_frame_export_keeps_the_selected_grain_variant() -> TestResult {
    let dir = tempfile::tempdir()?;
    let source = input(dir.path(), "grain.ivf", GRAIN)?;
    let variants = [
        (
            GrainPolicy::Skip,
            "9d75d085ff8fb770fc48f3b625faaec74fd196ab7270ca803ef5d197a5133789",
        ),
        (
            GrainPolicy::Apply,
            "533eb0c6add3b30161b88e568181e1fe07999b31a0b6fb2f0ccaff9aebcdfedf",
        ),
    ];
    for (at, (grain, oracle)) in variants.into_iter().enumerate() {
        let samples = dir.path().join(format!("frame-{at}.yuv"));
        let layout = dir.path().join(format!("frame-{at}.json"));
        let mut export = request(&source, 10, ChromaLayout::Yuv420, raw(&samples, &layout))?;
        export.range = ExportRange::Inclusive { first: 2, last: 2 };
        export.grain = grain;
        done(run(export)?, 1, 2);
        let samples = fs::read(samples)?;
        assert_eq!(samples.len(), 12_288);
        assert_eq!(sha(&samples), oracle);
        let record: serde_json::Value = serde_json::from_reader(fs::File::open(layout)?)?;
        assert_eq!(record["grain"], serde_json::to_value(grain)?);
    }
    assert_eq!(fs::read(source)?, GRAIN);
    Ok(())
}

#[test]
fn hashes_keep_absolute_ordinals_through_the_layer_filter() -> TestResult {
    let dir = tempfile::tempdir()?;
    let source = input(dir.path(), "layer.ivf", IVF_10)?;
    let dest = dir.path().join("hashes.json");
    let mut export = request(
        &source,
        10,
        ChromaLayout::Yuv420,
        ExportFormat::Hashes {
            path: WirePath::from_path(&dest),
        },
    )?;
    export.range = ExportRange::Inclusive { first: 1, last: 3 };
    export.layer = Some(ExportLayer {
        extended: 0,
        embedded: 0,
        temporal: 1,
    });
    done(run(export.clone())?, 2, 1);
    check_hashes(
        &dest,
        [1, 3].map(|ordinal| {
            let start = ordinal * 12_288;
            (ordinal, &AVM_10[start..start + 12_288])
        }),
    )?;

    let dest = dir.path().join("applied-grain-hashes.json");
    export.format = ExportFormat::Hashes {
        path: WirePath::from_path(&dest),
    };
    export.grain = GrainPolicy::Apply;
    failed(run(export)?);
    assert!(!dest.exists());
    Ok(())
}

#[test]
fn y4m_refuses_pq_and_changing_geometry_without_publishing() -> TestResult {
    let dir = tempfile::tempdir()?;
    for (name, bytes, depth) in [("pq.ivf", PQ, 10), ("crop.obu", CROPPED, 8)] {
        let source = input(dir.path(), name, bytes)?;
        let dest = dir.path().join("out.y4m");
        let mut export = request(&source, depth, ChromaLayout::Yuv420, y4m(&dest))?;
        export.frame_rate = Some(Rational { num: 24, den: 1 });
        failed(run(export)?);
        assert!(!dest.exists());
        assert_eq!(fs::read(source)?, bytes);
    }
    Ok(())
}

#[test]
fn raw_pq_retains_native_samples_and_per_output_color() -> TestResult {
    let dir = tempfile::tempdir()?;
    let source = input(dir.path(), "pq.ivf", PQ)?;
    let samples = dir.path().join("pq.yuv");
    let layout = dir.path().join("pq.json");
    let export = request(&source, 10, ChromaLayout::Yuv420, raw(&samples, &layout))?;
    done(run(export)?, 1, 2);
    assert_eq!(
        sha(&fs::read(samples)?),
        "d28d3ac800fa3af64ccbfba31bf7e38d16d871a3791ec57a8f4ab732723323a2"
    );
    let layout: serde_json::Value = serde_json::from_reader(fs::File::open(layout)?)?;
    let color = &layout["outputs"][0]["color"];
    assert_eq!(color["primaries"], 9);
    assert_eq!(color["transfer"], 16);
    assert_eq!(color["matrix"], 9);
    assert_eq!(color["full_range"], false);
    Ok(())
}

#[test]
fn existing_files_and_source_aliases_are_never_overwritten() -> TestResult {
    let dir = tempfile::tempdir()?;
    let source = input(dir.path(), "source.ivf", IVF_10)?;
    let existing = input(dir.path(), "protected.y4m", b"owner data")?;
    let export = request(&source, 10, ChromaLayout::Yuv420, y4m(&existing))?;
    refused(run(export)?, FailureCode::DestinationExists);
    assert_eq!(fs::read(&existing)?, b"owner data");

    let alias = dir.path().join("alias.y4m");
    fs::hard_link(&source, &alias)?;
    let export = request(&source, 10, ChromaLayout::Yuv420, y4m(&alias))?;
    refused(run(export)?, FailureCode::SourceIsDestination);
    assert_eq!(fs::read(&source)?, IVF_10);
    assert_eq!(fs::read(&alias)?, IVF_10);
    assert!(leftovers(dir.path(), &[&source, &existing, &alias]).is_empty());
    Ok(())
}

#[test]
fn a_different_source_with_the_same_file_metadata_fails_before_publication() -> TestResult {
    let dir = tempfile::tempdir()?;
    let source = input(dir.path(), "source.ivf", IVF_10)?;
    let output = dir.path().join("out.y4m");
    let export = request(&source, 10, ChromaLayout::Yuv420, y4m(&output))?;
    let modified = fs::metadata(&source)?.modified()?;
    let changed = ivf_timing([3, 5, 7, 9])?;
    fs::write(&source, &changed)?;
    fs::File::options()
        .write(true)
        .open(&source)?
        .set_times(fs::FileTimes::new().set_modified(modified))?;
    assert_eq!(
        inspected_source(&source)?.fingerprint,
        export.source.fingerprint
    );
    refused(run(export)?, FailureCode::SourceChanged);
    assert_eq!(fs::read(&source)?, changed);
    assert!(leftovers(dir.path(), &[&source]).is_empty());
    Ok(())
}

fn prepared(worker: &mut Live, export: ExportRequest) -> Answer<Vec<ExportTemp>> {
    let destinations = export.format.clone();
    match prepare(worker, export)? {
        Event::ExportPrepared { files } => {
            assert_eq!(files.len(), destinations.destinations().count());
            for file in &files {
                let partial = file.partial.to_path_buf()?;
                let destination = file.destination.to_path_buf()?;
                assert_eq!(partial.parent(), destination.parent());
                assert!(partial.is_file());
                assert!(!destination.exists());
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt as _;

                    let identity = file.identity.ok_or("missing temporary file identity")?;
                    let metadata = fs::metadata(&partial)?;
                    assert_eq!(
                        (identity.device, identity.inode),
                        (metadata.dev(), metadata.ino())
                    );
                    let parent = file.parent_identity.ok_or("missing parent identity")?;
                    let metadata = fs::metadata(partial.parent().ok_or("missing parent")?)?;
                    assert_eq!(
                        (parent.device, parent.inode),
                        (metadata.dev(), metadata.ino())
                    );
                }
                assert!(
                    destinations
                        .destinations()
                        .any(|(kind, dest)| kind == file.kind && *dest == file.destination)
                );
            }
            Ok(files)
        }
        other => Err(format!("unexpected prepare reply {other:?}").into()),
    }
}

#[test]
fn loss_of_worker_before_proceed_names_only_unpublished_temporaries() -> TestResult {
    let dir = tempfile::tempdir()?;
    let source = input(dir.path(), "source.ivf", IVF_10)?;
    let samples = dir.path().join("out.yuv");
    let layout = dir.path().join("out.json");
    let export = request(&source, 10, ChromaLayout::Yuv420, raw(&samples, &layout))?;
    let mut worker = Live::spawn()?;
    let files = prepared(&mut worker, export.clone())?;
    worker.child.kill()?;
    assert!(!worker.child.wait()?.success());
    assert!(!samples.exists());
    assert!(!layout.exists());
    assert_eq!(fs::read(&source)?, IVF_10);
    let mut announced = files
        .into_iter()
        .map(|file| file.partial.to_path_buf())
        .collect::<Result<Vec<_>, _>>()?;
    announced.sort();
    assert_eq!(leftovers(dir.path(), &[&source]), announced);
    for partial in announced {
        fs::remove_file(partial)?;
    }

    let mut worker = Live::spawn()?;
    prepared(&mut worker, export)?;
    worker.send(REQUEST + 1, Command::Cancel)?;
    assert_eq!(terminal(&mut worker)?, ExportOutcome::Cancelled);
    close(worker)?;
    assert!(leftovers(dir.path(), &[&source]).is_empty());
    Ok(())
}

#[test]
fn a_publication_collision_reports_the_exact_partial_batch() -> TestResult {
    let dir = tempfile::tempdir()?;
    let source = input(dir.path(), "source.ivf", IVF_10)?;
    let samples = dir.path().join("out.yuv");
    let layout = dir.path().join("out.json");
    let export = request(&source, 10, ChromaLayout::Yuv420, raw(&samples, &layout))?;
    let mut worker = Live::spawn()?;
    prepared(&mut worker, export)?;
    fs::write(&samples, b"owner data")?;
    worker.send(REQUEST, Command::ExportProceed)?;
    let outcome = terminal(&mut worker)?;
    let ExportOutcome::Partial {
        frames,
        files,
        failure,
    } = outcome
    else {
        return Err(format!("expected a partial batch, got {outcome:?}").into());
    };
    assert_eq!(frames, 4);
    assert_eq!(failure.code, FailureCode::DestinationExists);
    let [file] = files.as_slice() else {
        return Err(format!("unexpected published artifacts {files:?}").into());
    };
    assert_eq!(file.kind, ExportArtifactKind::Layout);
    assert_eq!(file.durability, ExportDurability::Confirmed);
    assert_eq!(file.path.to_path_buf()?, layout);
    assert_eq!(file.bytes, fs::metadata(&layout)?.len());
    assert_eq!(fs::read(&samples)?, b"owner data");
    assert_eq!(fs::read(&source)?, IVF_10);
    close(worker)?;
    assert!(leftovers(dir.path(), &[&source, &samples, &layout]).is_empty());
    Ok(())
}
