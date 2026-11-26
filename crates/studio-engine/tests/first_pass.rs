// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Run first-pass and final-pass jobs through the real engine.

mod common;

use std::error::Error;
use std::fs;
use std::num::NonZeroUsize;
use std::path::Path;
use std::process::{Command as Process, Stdio};
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;

use common::{Clip, TestResult, decode, fingerprint, leftovers, run_job, spec};
use splot_encode::ivf::IvfWriter;
use splot_encode::{
    BitDepth, Config, Encoder, FirstPass, Preset, RateControlMode, SourceFrame, ThreadCount,
};
use studio_domain::job::{
    CheckKind, CheckOutcome, FailureCode, JobPurpose, JobReport, JobSpec, Stage, WirePath,
};
use studio_domain::options::OptionValue;
use studio_domain::protocol::{Command, Envelope, Event, read_message, write_message};
use studio_domain::settings::OutputKind;
use studio_engine::{job, source};

const FRAMES: usize = 6;
const BITRATE: u32 = 32;

fn first_pass(input: &Path, artifact: &Path, output: OutputKind) -> JobSpec {
    let mut job = spec(input, artifact, output);
    job.purpose = JobPurpose::FirstPass;
    job.settings.values.insert(
        "rate_control".into(),
        OptionValue::Record(
            [
                ("mode".into(), OptionValue::Choice("vbr".into())),
                (
                    "bitrate_kbps".into(),
                    OptionValue::Integer(i64::from(BITRATE)),
                ),
            ]
            .into(),
        ),
    );
    job.settings.values.insert(
        "threads".into(),
        OptionValue::Record(
            [
                ("mode".into(), OptionValue::Choice("fixed".into())),
                ("count".into(), OptionValue::Integer(1)),
            ]
            .into(),
        ),
    );
    job
}

fn final_pass(first: &JobSpec, destination: &Path, digest: String) -> JobSpec {
    let mut job = first.clone();
    job.purpose = JobPurpose::Encode;
    job.destination = WirePath::from_path(destination);
    job.pass_artifact = Some(first.destination.clone());
    job.pass_digest = Some(digest);
    job
}

fn completed(end: Event) -> Result<JobReport, Box<dyn Error>> {
    match end {
        Event::Completed { report } => Ok(report),
        other => Err(format!("The job did not complete. {other:?}").into()),
    }
}

fn failed(end: &Event, code: FailureCode) {
    assert!(
        matches!(end, Event::Failed { failure } if failure.code == code),
        "{end:?}"
    );
}

fn has_check(report: &JobReport, kind: CheckKind) -> bool {
    report
        .checks
        .iter()
        .any(|check| check.kind == kind && check.outcome == CheckOutcome::Passed)
}

fn collect(first: &JobSpec) -> Result<String, Box<dyn Error>> {
    let (events, end) = run_job(first.clone(), |_, _| {});
    let report = completed(end)?;
    let path = first.destination.to_path_buf()?;
    assert_eq!(report.destination, first.destination);
    assert_eq!(report.frames, FRAMES as u64);
    assert_eq!(report.bytes, fs::metadata(path)?.len());
    assert!(has_check(&report, CheckKind::PassArtifact));
    let stages = events
        .iter()
        .filter_map(|event| match event {
            Event::Stage { stage } => Some(*stage),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        stages,
        [
            Stage::Checking,
            Stage::FirstPass,
            Stage::Finalizing,
            Stage::Verifying,
            Stage::Publishing
        ]
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Progress { progress }
        if progress.frames_read == FRAMES as u64 && progress.frames_encoded == 0))
    );
    let digest = report.pass_digest.ok_or("The report has no pass digest.")?;
    assert_eq!(digest.len(), 64);
    assert!(
        digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    );
    Ok(digest)
}

/// Produce the expected IVF through the codec API with the same source and settings.
fn codec_ivf(input: &Path, clip: Clip) -> Result<Vec<u8>, Box<dyn Error>> {
    let source =
        source::open(input, source::Selection::default()).map_err(|err| err.to_string())?;
    let mut config = Config::new(source.header.width, source.header.height);
    config.chroma = source.header.format;
    config.bit_depth = if source.header.bit_depth == 10 {
        BitDepth::Ten
    } else {
        BitDepth::Eight
    };
    config.frame_rate = source.header.frame_rate;
    config.content = source.content;
    config.preset = Preset::new(10)?;
    config.rate_control = RateControlMode::Vbr {
        bitrate_kbps: BITRATE,
    };
    config.threads = ThreadCount::Fixed(NonZeroUsize::MIN);
    let encode = job::spawn(move || {
        let frame = |index| {
            let planes = clip
                .samples(index)
                .try_into()
                .map_err(|_| "Expected three planes.")?;
            Ok::<_, Box<dyn Error + Send + Sync>>(SourceFrame::new(&config, index as u64, planes)?)
        };
        let mut first = FirstPass::new(&config)?;
        for index in 0..clip.frames {
            first.send_frame(frame(index)?)?;
        }
        let bits = first.finish()?;
        assert_eq!(bits.len(), clip.frames);
        let mut final_config = config.clone();
        final_config.first_pass = bits;
        let mut encoder = Encoder::new(final_config.clone())?;
        for index in 0..clip.frames {
            encoder.send_frame(frame(index)?)?;
        }
        encoder.flush()?;
        let mut ivf = IvfWriter::new(Vec::new(), &final_config)?;
        while let Some(packet) = encoder.receive_packet()? {
            ivf.write_packet(&packet)?;
        }
        Ok::<_, Box<dyn Error + Send + Sync>>(ivf.finish()?)
    })?;
    encode
        .join()
        .map_err(|_| "The reference codec thread panicked.")?
        .map_err(|err| err.to_string().into())
}

#[test]
fn both_depths_and_formats_use_the_verified_pass() -> TestResult {
    for colorspace in ["420", "420p10"] {
        let dir = tempfile::tempdir()?;
        let clip = Clip::new(64, 48, colorspace, FRAMES);
        let input = dir.path().join("source.y4m");
        clip.write(&input)?;
        let expected_ivf = codec_ivf(&input, clip.clone())?;
        let ivf = dir.path().join("final.ivf");
        let mut kept = vec![input.as_path()];
        let destinations = [ivf.clone(), dir.path().join("final.obu")];
        let artifacts = [dir.path().join("ivf.pass"), dir.path().join("obu.pass")];
        for ((kind, output), artifact) in [OutputKind::Ivf, OutputKind::Av2AnnexB]
            .into_iter()
            .zip(&destinations)
            .zip(&artifacts)
        {
            let first = first_pass(&input, artifact, kind);
            let digest = collect(&first)?;
            let before = fs::read(artifact)?;
            let (_, end) = run_job(final_pass(&first, output, digest.clone()), |_, _| {});
            let report = completed(end)?;
            assert_eq!(report.destination.to_path_buf()?, *output);
            assert_eq!(report.pass_digest.as_deref(), Some(digest.as_str()));
            assert_eq!(report.frames, FRAMES as u64);
            assert_eq!(report.bytes, fs::metadata(output)?.len());
            assert!(has_check(&report, CheckKind::PassArtifact));
            assert!(has_check(&report, CheckKind::Structure));
            assert!(has_check(&report, CheckKind::SplotDecode));
            assert_eq!(fs::read(artifact)?, before);
            if kind == OutputKind::Ivf {
                assert!(
                    fs::read(output)? == expected_ivf,
                    "{colorspace} differs from the codec's two-pass output."
                );
            } else {
                assert!(
                    decode(output)? == decode(&ivf)?,
                    "{colorspace} IVF and OBU decode to different samples."
                );
            }
            kept.extend([output.as_path(), artifact.as_path()]);
        }
        assert!(leftovers(dir.path(), &kept).is_empty());
        assert!(fs::read(input)? == clip.bytes());
    }
    Ok(())
}

fn refused(job: JobSpec, code: FailureCode, field: Option<&str>) {
    let destination = job.destination.to_path_buf().unwrap_or_default();
    let (events, end) = run_job(job, |_, _| {});
    failed(&end, code);
    if let Some(field) = field {
        assert!(
            matches!(&end, Event::Failed { failure } if failure.message.starts_with(field)),
            "{end:?}"
        );
    }
    assert!(
        !events.iter().any(|e| matches!(e, Event::Working { .. })),
        "{events:?}"
    );
    assert!(!destination.exists());
}

/// Make an artifact with one fewer cost and a valid checksum for its bytes.
fn missing_last_cost(original: &[u8]) -> Result<(Vec<u8>, String), Box<dyn Error>> {
    use std::fmt::Write as _;

    use sha2::{Digest, Sha256};

    assert_eq!(original.get(..8), Some(&b"SPLT1ST\x01"[..]));
    let frames = u32::from_le_bytes(original[12..16].try_into()?);
    assert_eq!(frames, FRAMES as u32);
    let mut incomplete = original[..original.len() - 32 - 8].to_vec();
    incomplete[12..16].copy_from_slice(&(frames - 1).to_le_bytes());
    let bytes = Sha256::digest(&incomplete);
    incomplete.extend_from_slice(&bytes);
    let mut digest = String::new();
    for byte in bytes {
        write!(digest, "{byte:02x}")?;
    }
    Ok((incomplete, digest))
}

#[test]
fn wrong_or_incomplete_passes_fail_before_opening_output() -> TestResult {
    let dir = tempfile::tempdir()?;
    let input = dir.path().join("source.y4m");
    let clip = Clip::new(64, 48, "420", FRAMES);
    clip.write(&input)?;
    let path = dir.path().join("statistics.pass");
    let output = dir.path().join("final.ivf");
    let first = first_pass(&input, &path, OutputKind::Ivf);
    let final_job = final_pass(&first, &output, collect(&first)?);
    let original = fs::read(&path)?;
    let pass_error = |job| refused(job, FailureCode::InvalidSettings, Some("pass_artifact:"));

    // A different file can pass its own source fingerprint check.
    let other = dir.path().join("other.y4m");
    let mut samples = clip.bytes();
    samples[clip.header().len() + b"FRAME\n".len()] ^= 1;
    fs::write(&other, samples)?;
    let mut wrong = final_job.clone();
    wrong.source = WirePath::from_path(&other);
    wrong.fingerprint = fingerprint(&other);
    pass_error(wrong);

    let mut wrong = final_job.clone();
    wrong
        .settings
        .values
        .insert("keyint".into(), OptionValue::Integer(3));
    pass_error(wrong);

    let mut stale = final_job.clone();
    stale
        .identity
        .as_mut()
        .ok_or("The job has no codec identity.")?
        .schema += 1;
    refused(stale, FailureCode::InvalidSettings, Some("identity:"));

    let mut unfrozen = final_job.clone();
    unfrozen.pass_digest = None;
    pass_error(unfrozen);

    fs::write(&path, &original[..original.len() - 1])?;
    pass_error(final_job.clone());

    let mut damaged = original.clone();
    damaged[16] ^= 1;
    fs::write(&path, damaged)?;
    pass_error(final_job.clone());

    let (incomplete, digest) = missing_last_cost(&original)?;
    fs::write(&path, incomplete)?;
    let mut incomplete = final_job.clone();
    incomplete.pass_digest = Some(digest);
    pass_error(incomplete);
    fs::write(&path, original)?;

    Clip::new(64, 48, "420", FRAMES - 1).write(&input)?;
    refused(final_job, FailureCode::SourceChanged, None);
    assert!(leftovers(dir.path(), &[&input, &path, &other]).is_empty());
    Ok(())
}

#[test]
fn cancelled_and_failed_passes_publish_no_artifact() -> TestResult {
    let dir = tempfile::tempdir()?;
    let input = dir.path().join("source.y4m");
    Clip::new(64, 48, "420", FRAMES).write(&input)?;
    let artifact = dir.path().join("statistics.pass");
    let first = first_pass(&input, &artifact, OutputKind::Ivf);
    let (events, end) = run_job(first.clone(), |event, cancel| {
        if matches!(event, Event::Working { .. }) {
            cancel.store(true, Ordering::Relaxed);
        }
    });
    assert!(matches!(end, Event::Cancelled), "{end:?}");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Working { .. }))
    );
    assert!(leftovers(dir.path(), &[&input]).is_empty());

    // A file in the destination's parent path makes creation fail on every host.
    let blocked = first_pass(&input, &input.join("statistics.pass"), OutputKind::Ivf);
    let (_, end) = run_job(blocked, |_, _| {});
    failed(&end, FailureCode::Output);
    assert!(leftovers(dir.path(), &[&input]).is_empty());

    // Make another file claim the name after the worker verified the statistics.
    let racer = artifact.clone();
    let (_, end) = run_job(first, move |event, _| {
        if matches!(
            event,
            Event::Stage {
                stage: Stage::Publishing
            }
        ) {
            fs::write(&racer, b"Keep this file.").unwrap_or_else(|err| unreachable!("{err}"));
        }
    });
    failed(&end, FailureCode::DestinationExists);
    assert_eq!(fs::read(&artifact)?, b"Keep this file.");
    assert!(leftovers(dir.path(), &[&input, &artifact]).is_empty());
    Ok(())
}

#[test]
fn worker_death_during_first_pass_cannot_publish_an_artifact() -> TestResult {
    let dir = tempfile::tempdir()?;
    let input = dir.path().join("source.y4m");
    Clip::new(160, 96, "420", 400).write(&input)?;
    let artifact = dir.path().join("statistics.pass");
    let mut child = Process::new(env!("CARGO_BIN_EXE_splot-studio-engine"))
        .arg("worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let ready = (|| -> TestResult {
        let mut stdout = child
            .stdout
            .take()
            .ok_or("The worker has no output pipe.")?;
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(Some(reply)) = read_message::<Event>(&mut stdout) {
                if sender.send(reply).is_err() {
                    break;
                }
            }
        });
        let job = first_pass(&input, &artifact, OutputKind::Ivf);
        let pipe = child
            .stdin
            .as_mut()
            .ok_or("The worker has no input pipe.")?;
        write_message(pipe, &Envelope::new(1, Command::Start { job, live: None }))?;
        loop {
            let reply = receiver.recv_timeout(Duration::from_secs(30))?;
            if reply.request_id != 1 {
                return Err(format!("Unexpected request ID {}.", reply.request_id).into());
            }
            match reply.body {
                Event::Progress { progress } if progress.frames_read > 0 => return Ok(()),
                Event::Completed { .. }
                | Event::Cancelled
                | Event::Failed { .. }
                | Event::Error { .. } => {
                    return Err(format!(
                        "First pass stopped before reading any frames. {:?}",
                        reply.body
                    )
                    .into());
                }
                _ => {}
            }
        }
    })();
    let killed = child.kill();
    let status = child.wait()?;
    ready?;
    killed?;
    assert!(!status.success());
    assert!(leftovers(dir.path(), &[&input]).is_empty());
    assert!(!artifact.exists());
    Ok(())
}
