// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Jobs: every real operation of both workspaces in one list, each with
//! its own state. A job is an operation, not a file or a document: a
//! queued file waits for its conversion, a document is read by its
//! latest read. Nothing here owns state; the list is derived each time
//! from the queue and the Analyze documents, so it cannot disagree with
//! them.
//!
//! Conversions are the queue items that have an attempt or are queued in
//! the current run. Inspections are the documents' reads: the engine reads
//! one file at a time, in the order asked, so of several pending reads the
//! first runs and the others are queued. Unread, missing and changed
//! documents have no read the engine holds, so no job.

use studio_domain::analysis::{AnalysisFailure, ProblemKind, ScanOutcome};
use studio_domain::job::JobPurpose;
use studio_domain::validation::{BatchOutcome, ValidationCompletion};

use crate::activity::Phase as JobPhase;
use crate::analysis::{Analysis, Document, Phase, ReadEnd};
pub use crate::batch::File as ValidationFile;
use crate::batch::Phase as ValidationPhase;
use crate::export_jobs::{ExportJob, Phase as ExportPhase};
use crate::queue::{Ending, Item, ItemState};
use crate::state::AppState;

/// Where a job is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Waiting for the operation before it.
    Queued,
    /// Under way.
    Running,
    /// Cancel was asked for; it has not stopped yet.
    Cancelling,
    /// Finished.
    Completed,
    /// Some outputs reached their destination before a failure.
    Partial,
    /// A read that ended where the file stopped parsing: what came before
    /// is kept, the rest is not read.
    Stopped,
    /// Stopped on request. A read keeps what it read.
    Cancelled,
    /// Ended with a failure.
    Failed,
    /// The app or the engine stopped while it ran, or, for a read, while
    /// the engine held its document.
    Interrupted,
}

impl State {
    /// Queued, running or cancelling.
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running | Self::Cancelling)
    }
}

/// What a job works on.
#[derive(Debug, Clone, Copy)]
pub enum Work<'a> {
    /// A conversion of a queue item.
    Convert(&'a Item),
    /// A read of the Analyze document at this index.
    Inspect(usize, &'a Document),
    /// A local file in the validation queue.
    Validate(&'a ValidationFile),
    /// An export from Analyze.
    Export(&'a ExportJob),
}

/// One job.
#[derive(Debug, Clone, Copy)]
pub struct Job<'a> {
    /// Where it is.
    pub state: State,
    /// What it works on.
    pub work: Work<'a>,
}

impl Job<'_> {
    /// The purpose of an encoder job, or absence for an Analyze read.
    #[must_use]
    pub fn purpose(&self) -> Option<JobPurpose> {
        let Work::Convert(item) = self.work else {
            return None;
        };
        item.submitted
            .as_ref()
            .map(|submission| submission.job.purpose)
            .or_else(|| item.attempt().map(|attempt| attempt.job.purpose))
    }

    /// The job has a captured request that can be copied to the draft.
    #[must_use]
    pub fn can_clone_settings(&self) -> bool {
        matches!(self.work, Work::Convert(item) if item.ending().is_some())
            && !self.state.is_active()
    }

    /// The job is an ended encode whose request can be checked and replayed.
    #[must_use]
    pub fn can_reencode(&self) -> bool {
        self.can_clone_settings() && self.purpose() == Some(JobPurpose::Encode)
    }

    /// The completed job published a first-pass artifact.
    #[must_use]
    pub fn can_encode_with_pass(&self) -> bool {
        self.purpose() == Some(JobPurpose::FirstPass)
            && matches!(self.work, Work::Convert(item)
                if matches!(item.ending(), Some(Ending::Completed(report)) if report.pass_digest.is_some()))
    }

    /// The job can show its source or report in its workspace.
    #[must_use]
    pub fn can_show(&self) -> bool {
        match self.work {
            Work::Validate(file) => file.can_open_report(),
            Work::Export(file) => file.reveal_path().is_some(),
            Work::Convert(_) | Work::Inspect(..) => true,
        }
    }

    /// The batch worker holds a complete retained report.
    #[must_use]
    pub const fn can_export(&self) -> bool {
        matches!(self.work, Work::Validate(file) if file.archived)
    }

    /// The file validation ended and can be removed from Jobs.
    #[must_use]
    pub const fn can_remove(&self) -> bool {
        matches!(self.work, Work::Validate(_) | Work::Export(_)) && !self.state.is_active()
    }

    /// Cancel applies: it is running or queued.
    #[must_use]
    pub const fn can_cancel(&self) -> bool {
        match self.work {
            // Cancel stops the converting file; a queued one is removed in
            // Encode instead.
            Work::Convert(_) => matches!(self.state, State::Running),
            Work::Export(file) => matches!(
                file.phase,
                ExportPhase::Queued
                    | ExportPhase::Running(
                        studio_domain::export::ExportStage::Checking
                            | studio_domain::export::ExportStage::Decoding
                            | studio_domain::export::ExportStage::Verifying
                    )
            ),
            Work::Inspect(..) | Work::Validate(_) => {
                matches!(self.state, State::Running | State::Queued)
            }
        }
    }
}

/// Every job: conversions in queue order, then reads in document order.
#[must_use]
pub fn list<'a>(state: &'a AppState, analysis: &'a Analysis) -> Vec<Job<'a>> {
    let conversions = state.items().iter().filter_map(|item| {
        conversion(state, item).map(|s| Job {
            state: s,
            work: Work::Convert(item),
        })
    });
    // The smallest pending id starts first. Other scans report their own progress.
    let first_read = analysis
        .documents()
        .iter()
        .filter(|d| matches!(d.phase(), Phase::Opening | Phase::Cancelling))
        .map(Document::id)
        .min();
    let reads = analysis
        .documents()
        .iter()
        .enumerate()
        .filter_map(move |(at, doc)| {
            read(doc, first_read).map(|s| Job {
                state: s,
                work: Work::Inspect(at, doc),
            })
        });
    conversions.chain(reads).collect()
}

pub(crate) fn validation(file: &ValidationFile) -> Job<'_> {
    let state = match &file.phase {
        ValidationPhase::Queued => State::Queued,
        ValidationPhase::Opening | ValidationPhase::Validating => State::Running,
        ValidationPhase::Cancelling => State::Cancelling,
        ValidationPhase::Cancelled => State::Cancelled,
        ValidationPhase::Failed(_) => State::Failed,
        ValidationPhase::Interrupted => State::Interrupted,
        ValidationPhase::Report(report) => match &**report {
            BatchOutcome::Failed {
                code: AnalysisFailure::Cancelled,
                ..
            } => State::Cancelled,
            BatchOutcome::Failed { .. } => State::Failed,
            BatchOutcome::Validated { summary, .. } => match summary.completion {
                ValidationCompletion::EndOfInput if summary.malformed_units == 0 => {
                    State::Completed
                }
                ValidationCompletion::Cancelled => State::Cancelled,
                ValidationCompletion::Io { .. } => State::Failed,
                _ => State::Stopped,
            },
        },
    };
    Job {
        state,
        work: Work::Validate(file),
    }
}

pub(crate) fn export(file: &ExportJob) -> Job<'_> {
    use studio_domain::export::ExportOutcome;
    let state = match &file.phase {
        ExportPhase::Queued => State::Queued,
        ExportPhase::Running(_) => State::Running,
        ExportPhase::Cancelling => State::Cancelling,
        ExportPhase::Finished(ExportOutcome::Completed { .. }) => State::Completed,
        ExportPhase::Finished(ExportOutcome::Partial { .. }) => State::Partial,
        ExportPhase::Finished(ExportOutcome::Failed { .. }) => State::Failed,
        ExportPhase::Finished(ExportOutcome::Cancelled) => State::Cancelled,
        ExportPhase::Interrupted(_) if !file.published.is_empty() => State::Partial,
        ExportPhase::Interrupted(_) => State::Interrupted,
    };
    Job {
        state,
        work: Work::Export(file),
    }
}

fn conversion(state: &AppState, item: &Item) -> Option<State> {
    Some(match item.state() {
        ItemState::Running | ItemState::Publishing => {
            match item.activity().map(crate::activity::Activity::phase) {
                Some(JobPhase::Cancelling { .. }) => State::Cancelling,
                _ => State::Running,
            }
        }
        ItemState::Waiting if state.queued(item) => State::Queued,
        ItemState::Completed => State::Completed,
        ItemState::Cancelled => State::Cancelled,
        ItemState::Failed | ItemState::Lost => State::Failed,
        ItemState::Interrupted => State::Interrupted,
        ItemState::Probing | ItemState::Waiting | ItemState::Unreadable => return None,
    })
}

fn read(doc: &Document, first_read: Option<u64>) -> Option<State> {
    Some(match doc.phase() {
        Phase::Opening if Some(doc.id()) == first_read || doc.summary().is_some() => State::Running,
        Phase::Opening => State::Queued,
        Phase::Cancelling => State::Cancelling,
        Phase::Open(summary) => match &summary.outcome {
            ScanOutcome::Stopped { problem } => match problem.kind {
                ProblemKind::Cancelled => State::Cancelled,
                ProblemKind::Io | ProblemKind::LimitReached => State::Failed,
                // The file itself ends the read: kept up to there, not whole.
                ProblemKind::Malformed | ProblemKind::Unsupported => State::Stopped,
            },
            ScanOutcome::Complete | ScanOutcome::Damaged { .. } => State::Completed,
            ScanOutcome::Scanning => State::Running,
        },
        Phase::Failed { by, .. } => match by {
            ReadEnd::Failure => State::Failed,
            ReadEnd::Cancel => State::Cancelled,
            ReadEnd::EngineStopped => State::Interrupted,
        },
        // No read, or none held: a changed file waits to be read as new.
        Phase::Unread | Phase::Missing | Phase::Changed { .. } => return None,
    })
}
