// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The run: which file converts next, the video-only confirmation, Cancel,
//! and what a job's events do to its item.

use std::time::Instant;

use studio_domain::job::{Failure, FailureCode, JobSpec, SourceReader, Stage, WirePath};
use studio_domain::options::EffectiveOption;
use studio_domain::protocol::Event;

use super::files::RetryKind;
use super::{AppState, Asked, Confirmation, Effect, Run, Update};
use crate::activity::{self, Activity, Changed, Phase};
use crate::queue::{
    Acknowledgement, Attempt, Ending, Item, ItemId, ItemState, PartialFate, Submission, Transition,
};
use crate::recovery::Disk;
use crate::source::attached_pictures;

impl AppState {
    /// A file Convert would start: read, waiting, with a free destination,
    /// in a format the engine writes.
    pub(super) fn startable(&self, item: &Item) -> bool {
        item.state == ItemState::Waiting
            && item.facts.is_some()
            && !item.destination.exists
            && !item.source_changed()
            && self.capabilities().is_some_and(|c| {
                c.encode_outputs.contains(
                    &item
                        .submitted
                        .as_ref()
                        .map_or(self.draft_settings().base.output, |submission| {
                            submission.job.settings.base.output
                        }),
                )
            })
    }

    /// A file the run may start now: startable and, if its output leaves
    /// something out, acknowledged for the current settings.
    fn runnable(&self, item: &Item) -> bool {
        self.startable(item)
            && (item.submitted.is_some()
                || !item.leaves_out_something()
                || item.acknowledged_for(self.draft_settings().base))
    }

    /// Convert, Convert N Files or Continue.
    pub(super) fn convert(&mut self, disk: &dyn Disk) {
        if self.run.is_some() || self.confirmation.is_some() || self.blocker().is_some() {
            return;
        }
        // With one file, Convert also means "again" after it ended.
        if let [only] = self.items.as_slice()
            && only.state.can_retry()
        {
            let id = only.id;
            self.retry(id, None, RetryKind::NewRequest, disk);
        }
        let asked = self.unacknowledged(None);
        if !asked.is_empty() {
            self.confirmation = Some(Confirmation {
                asked,
                output: self.draft_settings().base.output,
            });
            return;
        }
        let pending =
            self.startable_count() > 0 || self.items.iter().any(|i| i.state == ItemState::Probing);
        if pending {
            self.begin_run();
        }
    }

    fn begin_run(&mut self) {
        self.run = Some(Run {
            settings: self.settings.clone(),
            attempted: Vec::new(),
            stop_after_current: false,
        });
        self.paused = false;
        self.restored = None;
    }

    /// Startable files whose output leaves something out without a valid
    /// acknowledgement, leaving out those `run` already dealt with.
    fn unacknowledged(&self, run: Option<&Run>) -> Vec<Asked> {
        self.items
            .iter()
            .filter(|i| self.startable(i) && i.leaves_out_something())
            .filter(|i| i.submitted.is_none() && !i.acknowledged_for(self.draft_settings().base))
            .filter(|i| run.is_none_or(|r| !r.attempted.contains(&i.id)))
            .filter_map(|i| {
                let facts = i.facts.as_ref()?;
                Some(Asked {
                    item: i.id,
                    ack: Acknowledgement::new(facts, self.draft_settings().base),
                    source: i.source.clone(),
                    omissions: facts.omissions,
                    attached_pictures: attached_pictures(facts),
                })
            })
            .collect()
    }

    /// Whether `confirmation` still describes exactly what confirming would
    /// acknowledge.
    pub(super) fn still_confirms(&self, confirmation: &Confirmation) -> bool {
        confirmation.output == self.draft_settings().base.output
            && confirmation.asked.iter().all(|asked| {
                self.item(asked.item).is_some_and(|item| {
                    self.startable(item)
                        && item
                            .facts
                            .as_ref()
                            .is_some_and(|f| asked.ack.holds_for(f, self.draft_settings().base))
                })
            })
    }

    /// Records the confirmed acknowledgements and starts (or resumes) the run.
    pub(super) fn confirm(&mut self) {
        let Some(confirmation) = self.confirmation.take() else {
            return;
        };
        if !self.still_confirms(&confirmation) {
            return;
        }
        for asked in confirmation.asked {
            if let Some(item) = self.item_mut(asked.item) {
                item.ack = Some(asked.ack);
            }
        }
        if self.run.is_none() {
            self.begin_run();
        }
    }

    /// Nothing is acknowledged; a run passes over the files it asked about.
    pub(super) fn decline(&mut self) {
        let Some(confirmation) = self.confirmation.take() else {
            return;
        };
        if let Some(run) = &mut self.run {
            run.attempted
                .extend(confirmation.asked.iter().map(|a| a.item));
        }
    }

    /// Starts the next file of the run, waits for files still being read,
    /// asks about files that joined the run unacknowledged, or ends the run.
    pub(super) fn advance(&mut self, now: Instant, update: &mut Update) {
        if self.run.is_none() {
            return;
        }
        self.freeze_ready();
        let Some(run) = &self.run else {
            return;
        };
        if self.active().is_some() || self.confirmation.is_some() {
            return;
        }
        if run.stop_after_current {
            self.run = None;
            self.paused = self.items.iter().any(|i| i.state == ItemState::Waiting);
            return;
        }
        let fresh = |i: &Item| !run.attempted.contains(&i.id);
        if run.attempted.is_empty() && self.items.iter().any(|i| i.state == ItemState::Probing) {
            return;
        }
        if let Some(id) = self
            .items
            .iter()
            .find(|i| fresh(i) && self.runnable(i))
            .map(|i| i.id)
        {
            self.start(id, now, update);
            return;
        }
        if self
            .items
            .iter()
            .any(|i| fresh(i) && i.state == ItemState::Probing)
        {
            return;
        }
        let asked = self.unacknowledged(Some(run));
        if asked.is_empty() {
            self.run = None;
        } else {
            self.confirmation = Some(Confirmation {
                asked,
                output: self.draft_settings().base.output,
            });
        }
    }

    /// Freeze each ready job before the first job starts.
    fn freeze_ready(&mut self) {
        let Some(run) = &self.run else {
            return;
        };
        let settings = self.draft_settings().clone();
        let identity = self
            .capabilities()
            .and_then(|capabilities| capabilities.options.clone());
        let ready: Vec<_> = self
            .items
            .iter()
            .filter(|item| {
                !run.attempted.contains(&item.id) && item.submitted.is_none() && self.runnable(item)
            })
            .map(|item| item.id)
            .collect();
        for id in ready {
            if let Some(item) = self.item_mut(id)
                && let Some(facts) = &item.facts
            {
                item.submitted = Some(Submission {
                    job: JobSpec {
                        source: WirePath::from_path(&item.source),
                        fingerprint: facts.fingerprint,
                        video_stream: facts.video_stream,
                        omissions_acknowledged: facts.omissions.any()
                            && item.acknowledged_for(settings.base),
                        destination: WirePath::from_path(&item.destination.path),
                        settings: settings.clone(),
                        identity: identity.clone(),
                        purpose: studio_domain::job::JobPurpose::Encode,
                        pass_digest: None,
                        pass_artifact: None,
                    },
                    policy: Vec::new(),
                });
            }
        }
    }

    /// Starts the frozen submission for `id`.
    fn start(&mut self, id: ItemId, now: Instant, update: &mut Update) {
        let request = self.take_request();
        let identity = self
            .capabilities()
            .and_then(|capabilities| capabilities.options.clone());
        let Some(item) = self.item_mut(id) else {
            return;
        };
        let Some(facts) = &item.facts else {
            return;
        };
        let Some(Submission { mut job, policy }) = item.submitted.take() else {
            return;
        };
        if job.identity.is_none() {
            job.identity = identity;
        }
        let activity = Activity::new(
            item.destination.path.clone(),
            facts.frame_count,
            facts.reader == SourceReader::Ffmpeg,
            now,
        );
        if item.transition(Transition::Start).is_err() {
            return;
        }
        item.attempt = Some(Attempt {
            job: job.clone(),
            policy,
            request,
            activity: Some(activity),
            partial: None,
            ending: None,
        });
        if let Some(run) = &mut self.run {
            run.attempted.push(id);
        }
        update.effects.push(Effect::Start { id: request, job });
    }

    pub(super) fn queued_policy(
        &mut self,
        id: ItemId,
        result: Result<Vec<EffectiveOption>, Failure>,
    ) {
        let Some(item) = self
            .item_mut(id)
            .filter(|item| item.state == ItemState::Waiting)
        else {
            return;
        };
        let Some(submission) = item.submitted.as_mut() else {
            return;
        };
        match result {
            Ok(values) => submission.policy = values,
            Err(failure) => {
                let Some(Submission { job, policy }) = item.submitted.take() else {
                    return;
                };
                if item.transition(Transition::Start).is_ok()
                    && item.transition(Transition::Fail).is_ok()
                {
                    item.attempt = Some(Attempt {
                        job,
                        policy,
                        request: 0,
                        activity: None,
                        partial: None,
                        ending: Some(Ending::Failed(failure)),
                    });
                }
                if let Some(run) = &mut self.run {
                    run.attempted.push(id);
                }
            }
        }
    }

    pub(crate) fn pause_after_current(&mut self) {
        if let Some(run) = &mut self.run {
            run.stop_after_current = true;
        }
    }

    /// Cancel: the converting file stops, and the run with it.
    pub(super) fn cancel(&mut self, now: Instant, update: &mut Update) {
        let Some(attempt) = self
            .items
            .iter_mut()
            .find(|i| i.state.is_active())
            .and_then(|i| i.attempt.as_mut())
        else {
            return;
        };
        let request = attempt.request;
        let Some(activity) = &mut attempt.activity else {
            return;
        };
        let step = activity.apply(activity::Input::CancelPressed, now);
        if step.effect != activity::Effect::None {
            update.effects.push(Effect::Worker {
                id: request,
                effect: step.effect,
            });
        }
        self.pause_after_current();
    }

    /// Hands `input` to the job of request `id`, then moves its item through
    /// the table to match.
    pub(super) fn job_input(
        &mut self,
        id: u64,
        input: activity::Input,
        now: Instant,
        disk: &dyn Disk,
        update: &mut Update,
    ) {
        let Some(index) = self.items.iter().position(|i| {
            i.attempt
                .as_ref()
                .is_some_and(|a| a.request == id && id != 0)
        }) else {
            update.changed = Changed::Nothing;
            return;
        };
        let item = &mut self.items[index];
        let Some(attempt) = &mut item.attempt else {
            return;
        };
        if let activity::Input::Event(Event::Working { partial }) = &input {
            // Recorded once, while running; it is journalled with the item.
            if item.state.is_active() && attempt.partial.is_none() {
                attempt.partial = partial.to_path_buf().ok();
            }
            return;
        }
        let worker_ended = matches!(input, activity::Input::WorkerEnded { .. });
        let Some(activity) = &mut attempt.activity else {
            update.changed = Changed::Nothing;
            return;
        };
        let step = activity.apply(input, now);
        update.changed = step.changed;
        if step.effect != activity::Effect::None {
            update.effects.push(Effect::Worker {
                id,
                effect: step.effect,
            });
        }
        if self.follow_activity(index, disk) {
            update.changed = Changed::Phase;
        }
        // The worker is gone: what it left behind can be removed now.
        let item = &self.items[index];
        if worker_ended
            && let (Some(Ending::Lost { partial, .. }), Some(attempt)) =
                (item.ending(), &item.attempt)
            && let PartialFate::Pending(path) = partial
        {
            update.effects.push(Effect::CleanPartial {
                item: item.id,
                partial: path.clone(),
                destination: attempt.job.destination.to_path_buf().unwrap_or_default(),
            });
        }
    }

    /// Moves item `index` through the table to where its activity is.
    /// Returns whether its state changed.
    fn follow_activity(&mut self, index: usize, disk: &dyn Disk) -> bool {
        let item = &mut self.items[index];
        let before = item.state;
        let Some(attempt) = &mut item.attempt else {
            return false;
        };
        let Some(activity) = &attempt.activity else {
            return false;
        };
        // Stage events are never dropped, so Publishing precedes Completed.
        let publishing = activity.stage() == Some(Stage::Publishing);
        let (transition, ending) = match activity.phase() {
            Phase::Completed(report) => (
                Transition::Complete,
                Some(Ending::Completed(report.clone())),
            ),
            Phase::Cancelled => (Transition::Cancel, Some(Ending::Cancelled)),
            Phase::Failed(failure) => (Transition::Fail, Some(Ending::Failed(failure.clone()))),
            Phase::Lost(why) => (
                Transition::Lose,
                Some(Ending::Lost {
                    why: why.clone(),
                    publishing: before == ItemState::Publishing || publishing,
                    partial: PartialFate::pending(attempt.partial.as_deref()),
                }),
            ),
            Phase::Starting | Phase::Running(_) | Phase::Cancelling { .. } => {
                (Transition::PublishIntent, None)
            }
        };
        if before == ItemState::Running && (publishing || transition == Transition::Complete) {
            let _ = item.transition(Transition::PublishIntent);
        }
        if let Some(ending) = ending
            && item.transition(transition).is_ok()
            && let Some(attempt) = &mut item.attempt
        {
            let changed =
                matches!(&ending, Ending::Failed(f) if f.code == FailureCode::SourceChanged);
            attempt.ending = Some(ending);
            // The engine refused a file that changed since it was read: read
            // it again (its acknowledgement then no longer holds). This run
            // does not convert it again; Convert does.
            if changed {
                let id = item.id;
                self.retry(id, None, RetryKind::Refresh, disk);
            }
        }
        self.items[index].state != before
    }

    /// [`Effect::CleanPartial`] was carried out for `id`.
    pub(super) fn partial_cleaned(&mut self, id: ItemId, fate: PartialFate) {
        if let Some(ending) = self
            .item_mut(id)
            .and_then(|i| i.attempt.as_mut())
            .and_then(|a| a.ending.as_mut())
            && matches!(ending.partial(), Some(PartialFate::Pending(_)))
        {
            ending.set_partial(fate);
        }
    }

    /// A row's action: Remove a file that has not run, Cancel the
    /// converting one, Retry one that ended, Show a converted one.
    pub(super) fn row_action(
        &mut self,
        index: i32,
        now: Instant,
        disk: &dyn Disk,
        update: &mut Update,
    ) {
        let Some(item) = self.at(index) else {
            return;
        };
        let id = item.id;
        match item.state {
            ItemState::Probing | ItemState::Waiting | ItemState::Unreadable => {
                if self.confirmation.is_none() {
                    self.remove(id);
                }
            }
            ItemState::Running | ItemState::Publishing => self.cancel(now, update),
            ItemState::Cancelled | ItemState::Failed | ItemState::Lost | ItemState::Interrupted => {
                if self.confirmation.is_none() {
                    self.retry(id, None, RetryKind::Replay, disk);
                }
            }
            ItemState::Completed => {
                let path = item.destination.path.clone();
                update.effects.push(Effect::Reveal(path));
            }
        }
    }
}
