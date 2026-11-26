// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Display text for Jobs. Counts are the engine's; an operation that
//! reports no progress shows none, never an invented share.

use studio_domain::analysis::{DocumentSummary, ScanOutcome};
use studio_domain::export::{ExportFormat, ExportOutcome, ExportStage};
use studio_domain::validation::{BatchOutcome, ValidationCompletion};

use super::{Tone, analysis::plural, bytes, count, file_name, queue::is_first_pass};
use crate::activity::Phase as JobPhase;
use crate::analysis::Phase;
use crate::batch::{File as ValidationFile, Phase as ValidationPhase};
use crate::export_jobs::{ExportJob, Phase as ExportPhase};
use crate::jobs::{Job, State, Work};
use crate::queue::{Ending, Item};

/// One job, in words.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    /// "Convert", "First pass", or "Inspect".
    pub kind: &'static str,
    /// The file's name.
    pub name: String,
    /// The state.
    pub state: &'static str,
    /// How the state reads.
    pub tone: Tone,
    /// What is true now: counts, or why it ended.
    pub detail: String,
    /// The share done, only when the engine's counts give one.
    pub fraction: Option<f32>,
}

/// `job` in words.
#[must_use]
pub fn line(job: &Job<'_>) -> Line {
    let (kind, name, detail, fraction) = match job.work {
        Work::Convert(item) => (
            if is_first_pass(item) {
                "First pass"
            } else {
                "Convert"
            },
            file_name(item.source()),
            conversion(item, job.state),
            item.activity()
                .and_then(crate::activity::Activity::fraction),
        ),
        Work::Inspect(_, doc) => ("Inspect", file_name(doc.path()), read(doc, job.state), None),
        Work::Validate(file) => ("Validate", file_name(&file.path), validation(file), None),
        Work::Export(file) => (
            match file.request.format {
                ExportFormat::Y4m { .. } => "Export Y4M",
                ExportFormat::Raw { .. } => "Export raw",
                ExportFormat::Hashes { .. } => "Frame hashes",
            },
            file.request
                .source
                .path
                .to_path_buf()
                .ok()
                .as_deref()
                .map_or_else(|| "Export source".to_owned(), file_name),
            export_detail(file),
            None,
        ),
    };
    Line {
        kind,
        name,
        state: state_word(job.state),
        tone: match (job.work, job.state) {
            (Work::Validate(file), State::Completed) => match &file.phase {
                ValidationPhase::Report(report) => match &**report {
                    BatchOutcome::Validated { summary, .. } if summary.policy_failed() => {
                        Tone::Error
                    }
                    BatchOutcome::Validated { summary, .. } if summary.omitted != 0 => {
                        Tone::Warning
                    }
                    _ => Tone::Neutral,
                },
                _ => Tone::Neutral,
            },
            _ => tone(job.state),
        },
        detail,
        fraction: fraction.filter(|_| job.state == State::Running),
    }
}

/// A state in one word.
#[must_use]
pub const fn state_word(state: State) -> &'static str {
    match state {
        State::Queued => "Queued",
        State::Running => "Running",
        State::Cancelling => "Cancelling",
        State::Completed => "Completed",
        State::Partial => "Partial",
        State::Stopped => "Stopped",
        State::Cancelled => "Cancelled",
        State::Failed => "Failed",
        State::Interrupted => "Interrupted",
    }
}

const fn tone(state: State) -> Tone {
    match state {
        State::Queued | State::Running | State::Cancelled => Tone::Neutral,
        State::Cancelling | State::Stopped | State::Interrupted | State::Partial => Tone::Warning,
        State::Completed => Tone::Success,
        State::Failed => Tone::Error,
    }
}

/// The Jobs entry: its label and its description, from how many jobs are
/// under way.
#[must_use]
pub fn entry(jobs: &[Job<'_>]) -> (String, String) {
    let active = jobs.iter().filter(|j| j.state.is_active()).count();
    let ended = jobs.len() - active;
    let label = if active == 0 {
        "Jobs".to_owned()
    } else {
        format!("Jobs · {active}")
    };
    let description = format!(
        "{active} running or queued, {ended} ended. Opens encode, inspect, validation and export jobs."
    );
    (label, description)
}

fn export_detail(file: &ExportJob) -> String {
    let state = match &file.phase {
        ExportPhase::Queued => "Waiting for the shared worker.".to_owned(),
        ExportPhase::Running(ExportStage::Checking) => {
            "Checking the source and output paths.".to_owned()
        }
        ExportPhase::Running(ExportStage::Decoding) => file.progress.as_ref().map_or_else(
            || "Decoding frames.".to_owned(),
            |p| {
                format!(
                    "Decoded {} outputs. Wrote {} selected frames and {}.",
                    count(p.decoded),
                    count(p.exported),
                    bytes(p.bytes_written)
                )
            },
        ),
        ExportPhase::Running(ExportStage::Verifying) => {
            "Checking the source and staged outputs.".to_owned()
        }
        ExportPhase::Running(ExportStage::Publishing) => {
            "Publishing the verified outputs.".to_owned()
        }
        ExportPhase::Cancelling => "Stopping the export.".to_owned(),
        ExportPhase::Finished(ExportOutcome::Completed { frames, .. }) => {
            format!("Exported {}.", plural(*frames, "frame", "frames"))
        }
        ExportPhase::Finished(
            ExportOutcome::Partial { failure, .. } | ExportOutcome::Failed { failure },
        ) => failure.message.clone(),
        ExportPhase::Finished(ExportOutcome::Cancelled) => {
            "The export stopped before publication.".to_owned()
        }
        ExportPhase::Interrupted(message) => message.clone(),
    };
    let published = &file.published;
    let mut lines = vec![state];
    for (kind, path) in file.request.format.destinations() {
        let Ok(path) = path.to_path_buf() else {
            continue;
        };
        let status = match published.iter().find(|item| item.kind == kind) {
            Some(item) if item.durability == studio_domain::export::ExportDurability::Confirmed => {
                "Published"
            }
            Some(_) => "Published, sync unconfirmed",
            None if file.publication_possible
                && matches!(file.phase, ExportPhase::Interrupted(_)) =>
            {
                "Publication unconfirmed"
            }
            None if !file.phase.active() => "No publication recorded",
            None => "Destination",
        };
        lines.push(format!("{status} · {}", path.display()));
    }
    lines.extend(file.cleanup.iter().cloned());
    lines.join("\n")
}

fn conversion(item: &Item, state: State) -> String {
    let activity = item.activity();
    let first_pass = is_first_pass(item);
    match (state, item.ending()) {
        (State::Queued, _) if first_pass => "First pass queued.".into(),
        (State::Queued, _) => "Converts after the files before it in this run.".to_owned(),
        (State::Running, _) if first_pass => {
            activity.map_or_else(|| "Starting first pass.".into(), super::progress_text)
        }
        (State::Running, _) => match activity {
            Some(a) if a.before_first_frame() => "Starting; no frame encoded yet.".to_owned(),
            Some(a) => format!(
                "{} · {} written",
                super::progress_text(a),
                bytes(a.progress().bytes_written)
            ),
            None => "Converting.".to_owned(),
        },
        (State::Cancelling, _) => match activity.map(crate::activity::Activity::phase) {
            Some(JobPhase::Cancelling { killing: true, .. }) => {
                "The worker did not stop in time and is being terminated.".to_owned()
            }
            _ => "Stopping at the next safe point. Nothing is published.".to_owned(),
        },
        (_, Some(Ending::Completed(report))) if first_pass => {
            format!("{} frames analyzed · verified", count(report.frames))
        }
        (_, Some(Ending::Completed(report))) => format!(
            "{} frames · {} · verified",
            count(report.frames),
            bytes(report.bytes)
        ),
        (_, Some(Ending::Cancelled)) => "Stopped on request. Nothing was published.".to_owned(),
        (_, Some(Ending::Failed(failure))) => {
            format!(
                "{}. Nothing was published.",
                failure.message.trim_end_matches('.')
            )
        }
        (_, Some(Ending::Lost { why, .. })) => format!("{why} Nothing was published."),
        (_, Some(Ending::Interrupted(_))) if first_pass => {
            "First pass interrupted. Retry starts it again.".into()
        }
        (_, Some(Ending::Interrupted(_))) => {
            "The app closed or stopped while it converted. It does not resume; Retry converts \
             it again."
                .to_owned()
        }
        (_, None) => String::new(),
    }
}

fn read(doc: &crate::analysis::Document, state: State) -> String {
    match (state, doc.phase()) {
        (State::Queued, _) => "Waiting to scan this file.".to_owned(),
        (State::Running, _) => doc
            .summary()
            .map_or_else(|| "Reading the OBU envelopes.".to_owned(), found),
        (State::Cancelling, _) => "Stopping; what was read is kept.".to_owned(),
        (_, Phase::Open(summary)) => found(summary),
        (_, Phase::Failed { message, .. }) => message.clone(),
        (
            _,
            Phase::Unread
            | Phase::Missing
            | Phase::Changed { .. }
            | Phase::Opening
            | Phase::Cancelling,
        ) => String::new(),
    }
}

/// The elements found or retained by the scan.
fn found(summary: &DocumentSummary) -> String {
    let elements = plural(summary.elements, "element", "elements");
    match &summary.outcome {
        ScanOutcome::Scanning => format!("{elements} found so far."),
        ScanOutcome::Complete => format!("{elements}; every byte parsed."),
        ScanOutcome::Damaged { .. } => format!(
            "{elements}; read to the end, {} unparsed.",
            count(summary.unparsed)
        ),
        ScanOutcome::Stopped { problem } => format!(
            "{elements} kept; stopped at byte {} ({}).",
            count(problem.offset),
            super::analysis::problem_kind(problem.kind)
        ),
    }
}

fn validation(file: &ValidationFile) -> String {
    match &file.phase {
        ValidationPhase::Queued => "Waiting for validation.".into(),
        ValidationPhase::Opening => "Reading the AV2 stream.".into(),
        ValidationPhase::Validating => "Checking AV2 syntax and stream structure.".into(),
        ValidationPhase::Cancelling => {
            "Stopping validation. Earlier results remain available.".into()
        }
        ValidationPhase::Cancelled => "Validation stopped before this file started.".into(),
        ValidationPhase::Interrupted => {
            "The worker stopped before completing this validation.".into()
        }
        ValidationPhase::Failed(message) => message.clone(),
        ValidationPhase::Report(report) => {
            let mut detail = validation_result(report);
            if !file.archived {
                detail.push_str(" The worker stopped. Its detailed report is unavailable.");
            } else if matches!(&**report, BatchOutcome::Validated { .. }) && !file.can_open_report()
            {
                detail.push_str(
                    " Studio read only a source prefix. Export the report to inspect it.",
                );
            }
            detail
        }
    }
}

fn validation_result(report: &BatchOutcome) -> String {
    match report {
        BatchOutcome::Failed { message, .. } => message.clone(),
        BatchOutcome::Validated { summary, .. } => {
            let scope = match &summary.completion {
                ValidationCompletion::EndOfInput if summary.malformed_units > 0 => {
                    "Reached the end of input with incomplete framing coverage."
                }
                ValidationCompletion::EndOfInput => "Reached the end of input.",
                ValidationCompletion::Cancelled => "Stopped on request.",
                ValidationCompletion::MalformedInput => "Malformed input stopped validation.",
                ValidationCompletion::Io { .. } => "A read error stopped validation.",
                ValidationCompletion::UnitTooLarge { .. } => {
                    "An input unit exceeded the size limit."
                }
                ValidationCompletion::ObuLimit { .. } => "Reached the limit on the number of OBUs.",
                ValidationCompletion::Unsupported => {
                    "The worker could not describe why validation stopped."
                }
            };
            let policy = if summary.policy_failed() {
                "Findings fail the requested policy."
            } else if matches!(summary.completion, ValidationCompletion::EndOfInput)
                && summary.malformed_units == 0
            {
                "Completed checks pass the requested policy."
            } else {
                "Checks are incomplete."
            };
            format!(
                "{scope} {policy} {}, {}.",
                plural(summary.errors, "error", "errors"),
                plural(summary.warnings, "warning", "warnings"),
            )
        }
    }
}
