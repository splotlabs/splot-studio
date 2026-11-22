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
//! first runs and the others are queued. Unread and missing documents have
//! no read, so no job.

use studio_domain::analysis::{ProblemKind, ScanOutcome};

use crate::activity::Phase as JobPhase;
use crate::analysis::{Analysis, Document, Phase, ReadEnd};
use crate::queue::{Item, ItemState};
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
    /// Cancel applies: it is running or queued.
    #[must_use]
    pub const fn can_cancel(&self) -> bool {
        match self.work {
            // Cancel stops the converting file; a queued one is removed in
            // Encode instead.
            Work::Convert(_) => matches!(self.state, State::Running),
            Work::Inspect(..) => matches!(self.state, State::Running | State::Queued),
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
    // The engine reads in the order asked, and ids only increase.
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
        Phase::Opening if Some(doc.id()) == first_read => State::Running,
        Phase::Opening => State::Queued,
        Phase::Cancelling => State::Cancelling,
        Phase::Open(summary)
        | Phase::Changed {
            read: Some(summary),
            ..
        } => match &summary.outcome {
            ScanOutcome::Stopped { problem } => match problem.kind {
                ProblemKind::Cancelled => State::Cancelled,
                ProblemKind::Io | ProblemKind::LimitReached => State::Failed,
                // The file itself ends the read: it was read as far as it goes.
                ProblemKind::Malformed | ProblemKind::Unsupported => State::Completed,
            },
            ScanOutcome::Complete | ScanOutcome::Damaged { .. } => State::Completed,
        },
        Phase::Failed { by, .. } => match by {
            ReadEnd::Failure => State::Failed,
            ReadEnd::Cancel => State::Cancelled,
            ReadEnd::EngineStopped => State::Interrupted,
        },
        Phase::Unread | Phase::Missing | Phase::Changed { read: None, .. } => return None,
    })
}
