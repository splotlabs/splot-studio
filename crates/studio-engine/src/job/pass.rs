// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Write verified first-pass statistics and load them before final output.

use std::fs::{self, File};
use std::path::Path;
use std::sync::atomic::AtomicBool;

use splot_encode::{Config, FirstPass, RateControlMode, SourceFrame};
use studio_domain::job::{
    Check, CheckKind, CheckOutcome, FailureCode, JobReport, JobSpec, Progress, SourceReader, Stage,
};
use studio_domain::protocol::Event;

use super::{Control, Stop, encoder_error, open_output, options_identity, report_policy, stop_if};
use crate::artifact::{self, Artifact, ArtifactError, Header, MAX_FRAMES};
use crate::output;
use crate::source::Source;

pub(super) fn generate(
    job: &JobSpec,
    source: &mut Source,
    destination: &Path,
    config: &Config,
    policy: Vec<Event>,
    control: &dyn Control,
    report: &mut (dyn FnMut(Event) + Send),
) -> Result<JobReport, Stop> {
    require_vbr(config)?;
    let cancel = control.cancel_flag();
    report(Event::Stage {
        stage: Stage::Checking,
    });
    let header = header(job, source, &policy, cancel)?;
    source.start(cancel)?;
    if source.facts.reader == SourceReader::Ffmpeg
        && source
            .frame_count()
            .is_some_and(|count| count > MAX_FRAMES as u64)
    {
        return Err(limit());
    }
    report_policy(policy, control, report)?;
    let (frames, bits) = analyze(source, config, control, report)?;
    if source.pass_identity(cancel)? != header.source {
        return Err(Stop::failed(
            FailureCode::SourceChanged,
            "The source changed during first pass.",
        ));
    }

    let temp = open_output(destination, source, control, report)?;
    stop_if(cancel)?;
    let digest = artifact::write(temp.as_file(), &header, &bits, cancel).map_err(write_error)?;
    drop(bits);
    report(Event::Stage {
        stage: Stage::Verifying,
    });
    let verified = artifact::read(temp.as_file(), cancel).map_err(read_error)?;
    verified.verify(&header, Some(frames)).map_err(read_error)?;
    if verified.sha256 != digest {
        return Err(invalid(
            "The first-pass artifact changed during verification.",
        ));
    }
    if !control.claim_publish() {
        return Err(Stop::Cancelled);
    }
    report(Event::Stage {
        stage: Stage::Publishing,
    });
    let bytes = output::publish(temp, destination)?;
    Ok(JobReport {
        destination: job.destination.clone(),
        frames,
        bytes,
        checks: vec![check()],
        disclosures: Vec::new(),
        pass_digest: Some(digest),
    })
}

fn analyze(
    source: &mut Source,
    config: &Config,
    control: &dyn Control,
    report: &mut dyn FnMut(Event),
) -> Result<(u64, Vec<u64>), Stop> {
    let cancel = control.cancel_flag();
    let mut pass = FirstPass::new(config).map_err(encoder_error)?;
    report(Event::Stage {
        stage: Stage::FirstPass,
    });
    let mut progress = Progress::default();
    loop {
        stop_if(cancel)?;
        let Some(planes) = source.read_frame()? else {
            break;
        };
        if progress.frames_read == MAX_FRAMES as u64 {
            return Err(limit());
        }
        let frame = SourceFrame::new(config, progress.frames_read, planes)
            .map_err(|error| Stop::failed(FailureCode::InvalidSource, error.to_string()))?;
        pass.send_frame(frame).map_err(encoder_error)?;
        stop_if(cancel)?;
        progress.frames_read += 1;
        report(Event::Progress { progress });
    }
    if progress.frames_read == 0 {
        return Err(Stop::failed(
            FailureCode::InvalidSource,
            "The source has no frames.",
        ));
    }
    report(Event::Stage {
        stage: Stage::Finalizing,
    });
    let bits = pass.finish().map_err(encoder_error)?;
    stop_if(cancel)?;
    if bits.len() as u64 != progress.frames_read {
        return Err(invalid(
            "The encoder did not return costs for every input frame.",
        ));
    }
    // The first-pass API exposes accepted input and final statistics only.
    progress.frames_coded_milli = progress.frames_read * 1000;
    report(Event::Progress { progress });
    Ok((progress.frames_read, bits))
}

pub(super) fn load(
    job: &JobSpec,
    source: &mut Source,
    config: &Config,
    policy: &[Event],
    cancel: &AtomicBool,
) -> Result<Option<Artifact>, Stop> {
    let Some(path) = &job.pass_artifact else {
        return Ok(None);
    };
    require_vbr(config)?;
    let digest = job
        .pass_digest
        .as_deref()
        .ok_or_else(|| invalid("Select a verified artifact."))?;
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(invalid(
            "Use the lowercase SHA-256 digest from the first-pass job.",
        ));
    }
    let path = path
        .to_path_buf()
        .map_err(|error| invalid(error.to_string()))?;
    if !fs::metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
        return Err(invalid(
            "The first-pass artifact must be a readable regular file.",
        ));
    }
    let file = File::open(path).map_err(|error| invalid(error.to_string()))?;
    let artifact = artifact::read(&file, cancel).map_err(read_error)?;
    if artifact.sha256 != digest {
        return Err(invalid(
            "The first-pass artifact digest does not match the selected job.",
        ));
    }
    let expected = header(job, source, policy, cancel)?;
    let frame_count = match source.facts.reader {
        SourceReader::Y4m => {
            check_y4m_count(source, artifact.bits.len() as u64, cancel)?;
            None
        }
        SourceReader::Ffmpeg => Some(
            source
                .frame_count()
                .ok_or_else(|| invalid("The source scan did not verify the frame count."))?,
        ),
    };
    artifact
        .verify(&expected, frame_count)
        .map_err(read_error)?;
    Ok(Some(artifact))
}

fn check_y4m_count(source: &mut Source, count: u64, cancel: &AtomicBool) -> Result<(), Stop> {
    let mut actual = 0;
    loop {
        stop_if(cancel)?;
        if source.read_frame()?.is_none() {
            break;
        }
        actual += 1;
        if actual > count {
            return Err(invalid(
                "The artifact has fewer frame entries than the source.",
            ));
        }
    }
    if actual != count {
        return Err(invalid(
            "The artifact has more frame entries than the source.",
        ));
    }
    source.position_at(0, 0)?;
    source.unchanged()
}

fn header(
    job: &JobSpec,
    source: &Source,
    policy: &[Event],
    cancel: &AtomicBool,
) -> Result<Header, Stop> {
    Ok(Header {
        source: source.pass_identity(cancel)?,
        identity: options_identity(),
        settings: job.settings.clone(),
        policy: policy
            .iter()
            .filter_map(|event| match event {
                Event::JobPolicy { values, .. } => Some(values.iter()),
                _ => None,
            })
            .flatten()
            .cloned()
            .collect(),
    })
}

fn require_vbr(config: &Config) -> Result<(), Stop> {
    if matches!(config.rate_control, RateControlMode::Vbr { .. }) {
        Ok(())
    } else {
        Err(invalid("Select VBR to use a first-pass artifact."))
    }
}

fn limit() -> Stop {
    invalid(format!(
        "A first-pass job supports at most {MAX_FRAMES} frames."
    ))
}

pub(super) fn invalid(message: impl std::fmt::Display) -> Stop {
    Stop::failed(
        FailureCode::InvalidSettings,
        format!("pass_artifact: {message}"),
    )
}

pub(super) const fn check() -> Check {
    Check {
        kind: CheckKind::PassArtifact,
        outcome: CheckOutcome::Passed,
    }
}

fn read_error(error: ArtifactError) -> Stop {
    match error {
        ArtifactError::Cancelled => Stop::Cancelled,
        error => invalid(error),
    }
}

fn write_error(error: ArtifactError) -> Stop {
    match error {
        ArtifactError::Cancelled => Stop::Cancelled,
        error => output::output(format!("Cannot write the first-pass artifact. {error}")),
    }
}
