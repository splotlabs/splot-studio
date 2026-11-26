// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Display text for Jobs. Counts are the engine's; an operation that
//! reports no progress shows none, never an invented share.

use studio_domain::analysis::{DocumentSummary, ScanOutcome};

use super::{Tone, bytes, count, file_name, queue::is_first_pass};
use crate::activity::Phase as JobPhase;
use crate::analysis::Phase;
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
    };
    Line {
        kind,
        name,
        state: state_word(job.state),
        tone: tone(job.state),
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
        State::Stopped => "Stopped",
        State::Cancelled => "Cancelled",
        State::Failed => "Failed",
        State::Interrupted => "Interrupted",
    }
}

const fn tone(state: State) -> Tone {
    match state {
        State::Queued | State::Running | State::Cancelled => Tone::Neutral,
        State::Cancelling | State::Stopped | State::Interrupted => Tone::Warning,
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
        "{active} running or queued, {ended} ended. Opens the list of encode jobs and stream reads."
    );
    (label, description)
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
        (State::Queued, _) => {
            "Waits for the read before it; the engine reads one file at a time.".to_owned()
        }
        (State::Running, _) => {
            "Reading the OBU envelopes. This read reports no progress until it ends.".to_owned()
        }
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

/// What a finished read kept.
fn found(summary: &DocumentSummary) -> String {
    let elements = format!("{} elements", count(summary.elements));
    match &summary.outcome {
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
