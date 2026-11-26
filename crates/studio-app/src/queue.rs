// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! One file in the queue: its durable [`ItemState`], the table that is the
//! only way to change it, and what the last attempt to convert it left.
//!
//! ```text
//! Probing ──Probed──> Waiting ──Start──> Running ──PublishIntent──> Publishing ──Complete──> Completed
//!    │  ^               │  ^                │                          │
//!    │  └──Reprobe──────┘  │                ├─Cancel / Fail / Lose / Interrupt─┤
//!    └─ProbeFailed─> Unreadable            v                          v
//!                     (Reprobe)      Cancelled · Failed · Lost · Interrupted ──Retry──> Probing
//! ```
//!
//! Retry goes back through Probing: the file is read again before it waits.
//! `Interrupt` is how a job that was converting when the app closed or died
//! is recorded; nothing ever resumes a bitstream. Any item that is not
//! Running or Publishing can be removed.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use studio_domain::job::{Failure, JobReport, JobSpec, Omissions, SourceFacts, SourceFingerprint};
use studio_domain::options::EffectiveOption;
use studio_domain::settings::EncoderSettings;

use crate::activity::Activity;
use crate::source::ProbeFailure;

/// A queue item's identity: monotonic and never reused, also across restarts.
pub type ItemId = u64;

/// Where a file is, durably.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemState {
    /// The engine is reading (or about to read) the file's header.
    Probing,
    /// Read and ready; the next run may start it.
    Waiting,
    /// The engine cannot read it, or could not be asked.
    Unreadable,
    /// Its job is running.
    Running,
    /// The engine reported that it is moving the verified file into place.
    Publishing,
    /// The verified file is at its destination.
    Completed,
    /// Stopped on request; nothing was published.
    Cancelled,
    /// The engine reported a failure; nothing was published.
    Failed,
    /// The worker went away without a terminal event.
    Lost,
    /// The app closed or stopped while it was converting.
    Interrupted,
}

impl ItemState {
    /// Every state, for exhaustive tests.
    pub const ALL: [Self; 10] = [
        Self::Probing,
        Self::Waiting,
        Self::Unreadable,
        Self::Running,
        Self::Publishing,
        Self::Completed,
        Self::Cancelled,
        Self::Failed,
        Self::Lost,
        Self::Interrupted,
    ];

    /// A job owns the file: it cannot be removed, edited or started again.
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Running | Self::Publishing)
    }

    /// Retry is allowed.
    #[must_use]
    pub const fn can_retry(self) -> bool {
        next(self, Transition::Retry).is_some()
    }

    /// An attempt ended, one way or another.
    #[must_use]
    pub const fn has_ended(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Cancelled | Self::Failed | Self::Lost | Self::Interrupted
        )
    }
}

/// Something that moves an item between states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Transition {
    /// The probe answered with facts.
    Probed,
    /// The probe answered with a failure.
    ProbeFailed,
    /// Read the file again: another video track, or a changed file.
    Reprobe,
    /// The run started its job.
    Start,
    /// The engine reported `Stage::Publishing`.
    PublishIntent,
    /// The engine reported `Completed`.
    Complete,
    /// The engine reported `Cancelled`.
    Cancel,
    /// The engine reported `Failed`.
    Fail,
    /// The worker ended without a terminal event.
    Lose,
    /// The app closed, or was found to have stopped, while converting.
    Interrupt,
    /// The user asked to convert it again.
    Retry,
}

impl Transition {
    /// Every transition, for exhaustive tests.
    pub const ALL: [Self; 11] = [
        Self::Probed,
        Self::ProbeFailed,
        Self::Reprobe,
        Self::Start,
        Self::PublishIntent,
        Self::Complete,
        Self::Cancel,
        Self::Fail,
        Self::Lose,
        Self::Interrupt,
        Self::Retry,
    ];
}

/// The transition table. `None` is an illegal transition.
#[must_use]
pub const fn next(from: ItemState, transition: Transition) -> Option<ItemState> {
    use ItemState as S;
    use Transition as T;
    match (from, transition) {
        (S::Probing, T::Probed) => Some(S::Waiting),
        (S::Probing, T::ProbeFailed) => Some(S::Unreadable),
        (S::Waiting, T::Start) => Some(S::Running),
        (S::Running, T::PublishIntent) => Some(S::Publishing),
        (S::Publishing, T::Complete) => Some(S::Completed),
        (S::Running | S::Publishing, T::Cancel) => Some(S::Cancelled),
        (S::Running | S::Publishing, T::Fail) => Some(S::Failed),
        (S::Running | S::Publishing, T::Lose) => Some(S::Lost),
        (S::Running | S::Publishing, T::Interrupt) => Some(S::Interrupted),
        // Read again: another track or a changed file, or a retry.
        (S::Probing | S::Waiting | S::Unreadable, T::Reprobe)
        | (S::Cancelled | S::Failed | S::Lost | S::Interrupted, T::Retry) => Some(S::Probing),
        _ => None,
    }
}

/// A transition the table does not allow; the item is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IllegalTransition {
    /// The state the item was in.
    pub from: ItemState,
    /// What was attempted.
    pub transition: Transition,
}

/// Where an item's output goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    /// The file to create.
    pub path: PathBuf,
    /// The user picked it; otherwise it is the default next to the source
    /// and follows the output kind's extension.
    pub chosen: bool,
    /// Something existed at `path` when last checked.
    pub exists: bool,
}

/// The user's confirmation that a video-only output leaves out what the
/// probe found. It holds only while the file (fingerprint), the video stream,
/// what is left out and every setting (the output kind included) are the
/// ones it was given for; any change discards it. It belongs to one item and
/// is never moved to another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Acknowledgement {
    /// The file that was looked at.
    pub fingerprint: SourceFingerprint,
    /// The video stream that is kept.
    pub video_stream: u32,
    /// What the output leaves out.
    pub omissions: Omissions,
    /// The settings it was given for.
    pub settings: EncoderSettings,
}

impl Acknowledgement {
    /// The acknowledgement a confirmation records for `facts` and `settings`.
    #[must_use]
    pub const fn new(facts: &SourceFacts, settings: EncoderSettings) -> Self {
        Self {
            fingerprint: facts.fingerprint,
            video_stream: facts.video_stream,
            omissions: facts.omissions,
            settings,
        }
    }

    /// Whether it still covers a job made from `facts` and `settings`.
    #[must_use]
    pub fn holds_for(&self, facts: &SourceFacts, settings: EncoderSettings) -> bool {
        *self == Self::new(facts, settings)
    }
}

/// What became of a job's temporary file once its worker was gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartialFate {
    /// The engine never announced one.
    None,
    /// Announced and not dealt with yet: removed on the next chance.
    Pending(PathBuf),
    /// Announced, and no longer there (the worker removed it).
    Gone,
    /// Announced, and removed by the app.
    Removed(PathBuf),
    /// Announced, and left alone, with why.
    Kept {
        /// The path.
        path: PathBuf,
        /// Why it was not removed.
        reason: String,
    },
}

impl PartialFate {
    /// The fate of a just-ended job that announced `partial`.
    #[must_use]
    pub fn pending(partial: Option<&Path>) -> Self {
        partial.map_or(Self::None, |p| Self::Pending(p.to_path_buf()))
    }
}

/// How the app came to stop while a job ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// The window was closed or Quit chosen; the worker was told to stop.
    AppClosed,
    /// The journal still said converting when the app started: the app or
    /// the computer stopped without closing.
    AppStopped,
}

/// What is known about an interrupted job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interruption {
    /// The engine had reported that it was publishing the file.
    pub publishing: bool,
    /// How the app stopped.
    pub cause: Cause,
    /// Something was at the destination when the app started again.
    pub destination_present: bool,
    /// Its temporary file.
    pub partial: PartialFate,
}

/// How an attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ending {
    /// The verified file is at its destination.
    Completed(JobReport),
    /// Stopped on request.
    Cancelled,
    /// The engine reported a failure.
    Failed(Failure),
    /// The worker went away without a terminal event.
    Lost {
        /// What happened, for the user.
        why: String,
        /// The engine had reported that it was publishing the file.
        publishing: bool,
        /// Its temporary file.
        partial: PartialFate,
    },
    /// The app closed or stopped while it ran.
    Interrupted(Interruption),
}

impl Ending {
    /// The temporary file's fate, for the endings that can leave one.
    #[must_use]
    pub const fn partial(&self) -> Option<&PartialFate> {
        match self {
            Self::Lost { partial, .. } | Self::Interrupted(Interruption { partial, .. }) => {
                Some(partial)
            }
            Self::Completed(_) | Self::Cancelled | Self::Failed(_) => None,
        }
    }

    /// Replaces the temporary file's fate where the ending has one.
    pub fn set_partial(&mut self, fate: PartialFate) {
        match self {
            Self::Lost { partial, .. } | Self::Interrupted(Interruption { partial, .. }) => {
                *partial = fate;
            }
            Self::Completed(_) | Self::Cancelled | Self::Failed(_) => {}
        }
    }
}

/// An immutable job and the effective policy returned by preflight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Submission {
    /// The source, destination, and requested settings.
    pub job: JobSpec,
    /// The effective policy. Empty until preflight finishes.
    pub policy: Vec<EffectiveOption>,
}

/// One attempt to convert an item: the immutable job it started with and
/// what came of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    /// The job exactly as it was sent, settings included.
    pub job: JobSpec,
    /// The effective policy captured before the job starts.
    pub policy: Vec<EffectiveOption>,
    /// The worker request it ran under; `0` for an attempt restored from
    /// the journal, which has no worker.
    pub request: u64,
    /// The live state while this session ran it; `None` once restored.
    pub activity: Option<Activity>,
    /// The temporary file the engine announced with `Working`.
    pub partial: Option<PathBuf>,
    /// How it ended; `None` while it runs.
    pub ending: Option<Ending>,
}

/// One file in the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub(crate) id: ItemId,
    pub(crate) source: PathBuf,
    pub(crate) size: Option<u64>,
    pub(crate) state: ItemState,
    /// The video stream asked for; `None` lets the engine pick.
    pub(crate) stream: Option<u32>,
    /// The last successful probe. While the file is read again it stays,
    /// for display only; a job is built from it only in [`ItemState::Waiting`].
    pub(crate) facts: Option<SourceFacts>,
    /// Why the last probe failed, in [`ItemState::Unreadable`].
    pub(crate) probe_failure: Option<ProbeFailure>,
    /// The probe request in flight for it; `None` while it waits its turn.
    pub(crate) probe_request: Option<u64>,
    pub(crate) destination: Destination,
    pub(crate) ack: Option<Acknowledgement>,
    pub(crate) submitted: Option<Submission>,
    /// The current or last attempt. A retry keeps the last one visible until
    /// the next one starts.
    pub(crate) attempt: Option<Attempt>,
}

impl Item {
    /// A file just added: it is read first.
    #[must_use]
    pub const fn new(id: ItemId, source: PathBuf, destination: PathBuf) -> Self {
        Self {
            id,
            source,
            size: None,
            state: ItemState::Probing,
            stream: None,
            facts: None,
            probe_failure: None,
            probe_request: None,
            destination: Destination {
                path: destination,
                chosen: false,
                exists: false,
            },
            ack: None,
            submitted: None,
            attempt: None,
        }
    }

    /// Applies one transition from the table.
    ///
    /// # Errors
    /// [`IllegalTransition`] when the table does not allow it; nothing changes.
    pub const fn transition(&mut self, transition: Transition) -> Result<(), IllegalTransition> {
        match next(self.state, transition) {
            Some(state) => {
                self.state = state;
                Ok(())
            }
            None => Err(IllegalTransition {
                from: self.state,
                transition,
            }),
        }
    }

    /// Its identity.
    #[must_use]
    pub const fn id(&self) -> ItemId {
        self.id
    }

    /// The source file.
    #[must_use]
    pub fn source(&self) -> &Path {
        &self.source
    }

    /// The source file's size, when it could be read.
    #[must_use]
    pub const fn size(&self) -> Option<u64> {
        self.size
    }

    /// Its durable state.
    #[must_use]
    pub const fn state(&self) -> ItemState {
        self.state
    }

    /// The last facts the engine read, also while reading again.
    #[must_use]
    pub const fn facts(&self) -> Option<&SourceFacts> {
        self.facts.as_ref()
    }

    /// Why it cannot be read, when it cannot.
    #[must_use]
    pub const fn probe_failure(&self) -> Option<&ProbeFailure> {
        self.probe_failure.as_ref()
    }

    /// The video stream described, or asked for while reading.
    #[must_use]
    pub fn video_stream(&self) -> Option<u32> {
        match self.state {
            ItemState::Probing | ItemState::Unreadable => self.stream,
            _ => None,
        }
        .or_else(|| self.facts.as_ref().map(|f| f.video_stream))
    }

    /// Where its output goes.
    #[must_use]
    pub const fn destination(&self) -> &Destination {
        &self.destination
    }

    /// The confirmation recorded for it, if any.
    #[must_use]
    pub const fn acknowledgement(&self) -> Option<&Acknowledgement> {
        self.ack.as_ref()
    }

    /// The current or last attempt.
    #[must_use]
    pub const fn attempt(&self) -> Option<&Attempt> {
        self.attempt.as_ref()
    }

    /// The live job state, while this session runs or ran it.
    #[must_use]
    pub fn activity(&self) -> Option<&Activity> {
        self.attempt.as_ref().and_then(|a| a.activity.as_ref())
    }

    /// How the last attempt ended.
    #[must_use]
    pub fn ending(&self) -> Option<&Ending> {
        self.attempt.as_ref().and_then(|a| a.ending.as_ref())
    }

    /// A video-only output of it leaves something out.
    #[must_use]
    pub fn leaves_out_something(&self) -> bool {
        self.facts.as_ref().is_some_and(|f| f.omissions.any())
    }

    /// Its recorded acknowledgement covers a job with `settings` now.
    #[must_use]
    pub fn acknowledged_for(&self, settings: EncoderSettings) -> bool {
        match (&self.ack, &self.facts) {
            (Some(ack), Some(facts)) => ack.holds_for(facts, settings),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ItemState as S;
    use Transition as T;

    /// Every legal edge, written out independently of [`next`].
    const LEGAL: [(ItemState, Transition, ItemState); 20] = [
        (S::Probing, T::Probed, S::Waiting),
        (S::Probing, T::ProbeFailed, S::Unreadable),
        (S::Probing, T::Reprobe, S::Probing),
        (S::Waiting, T::Reprobe, S::Probing),
        (S::Unreadable, T::Reprobe, S::Probing),
        (S::Waiting, T::Start, S::Running),
        (S::Running, T::PublishIntent, S::Publishing),
        (S::Publishing, T::Complete, S::Completed),
        (S::Running, T::Cancel, S::Cancelled),
        (S::Publishing, T::Cancel, S::Cancelled),
        (S::Running, T::Fail, S::Failed),
        (S::Publishing, T::Fail, S::Failed),
        (S::Running, T::Lose, S::Lost),
        (S::Publishing, T::Lose, S::Lost),
        (S::Running, T::Interrupt, S::Interrupted),
        (S::Publishing, T::Interrupt, S::Interrupted),
        (S::Cancelled, T::Retry, S::Probing),
        (S::Failed, T::Retry, S::Probing),
        (S::Lost, T::Retry, S::Probing),
        (S::Interrupted, T::Retry, S::Probing),
    ];

    #[test]
    fn every_pair_of_state_and_transition_matches_the_table() {
        let mut legal = 0;
        for from in ItemState::ALL {
            for transition in Transition::ALL {
                let expected = LEGAL
                    .iter()
                    .find(|(f, t, _)| *f == from && *t == transition)
                    .map(|(_, _, to)| *to);
                assert_eq!(
                    next(from, transition),
                    expected,
                    "{from:?} + {transition:?}"
                );
                legal += usize::from(expected.is_some());
            }
        }
        assert_eq!(legal, LEGAL.len(), "an edge is listed twice");
    }

    #[test]
    fn an_illegal_transition_changes_nothing() {
        for from in ItemState::ALL {
            for transition in Transition::ALL {
                let mut item = Item::new(1, "/m/a.y4m".into(), "/m/a.ivf".into());
                item.state = from;
                match item.transition(transition) {
                    Ok(()) => assert_eq!(Some(item.state), next(from, transition)),
                    Err(e) => {
                        assert_eq!(e, IllegalTransition { from, transition });
                        assert_eq!(item.state, from);
                    }
                }
            }
        }
    }

    #[test]
    fn running_items_are_the_only_ones_that_cannot_be_touched() {
        let active: Vec<_> = ItemState::ALL
            .into_iter()
            .filter(|s| s.is_active())
            .collect();
        assert_eq!(active, [S::Running, S::Publishing]);
        let retry: Vec<_> = ItemState::ALL
            .into_iter()
            .filter(|s| s.can_retry())
            .collect();
        assert_eq!(retry, [S::Cancelled, S::Failed, S::Lost, S::Interrupted]);
        // Completion is never reached without the publication intent, and a
        // completed file is never converted again by retry.
        assert_eq!(next(S::Running, T::Complete), None);
        assert_eq!(next(S::Completed, T::Retry), None);
        // Nothing starts that was not read and waiting.
        for from in ItemState::ALL {
            assert_eq!(
                next(from, T::Start).is_some(),
                from == S::Waiting,
                "{from:?}"
            );
        }
    }
}
