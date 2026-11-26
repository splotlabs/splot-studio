// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Schedules one export on the shared worker lane.

use std::sync::Arc;
use std::time::Instant;

use studio_domain::export::{ExportOutcome, ExportRequest, ExportStage};
use studio_domain::protocol::{Command, ErrorCode, Event};

use super::{Changed, Delivery, Intent, Outcome, Route, Session, UiEffect, Worker, ending_text};
use crate::export_jobs::{ExportFrameSource, ExportSource, Phase};
use crate::journal;

#[derive(Clone, Copy)]
enum Save {
    Start,
    Proceed,
}

/// The job keeps its lane until the worker exits and the journal answers.
pub(super) struct Active {
    id: u64,
    save: Option<Save>,
    worker: Option<Worker>,
}

impl Session {
    /// All open documents that have a complete source digest, in tab order.
    #[must_use]
    pub fn export_sources(&self) -> Vec<ExportSource> {
        self.analysis
            .documents()
            .iter()
            .filter_map(ExportSource::from_document)
            .collect()
    }

    /// Captures the current Analyze output and its source as one snapshot.
    #[must_use]
    pub fn export_frame_source(&self) -> Option<ExportFrameSource> {
        let source = ExportSource::from_document(self.analysis.shown()?)?;
        self.preview.export_frame_source(source)
    }

    /// Adds one export job per source, with the options from the dialog.
    pub fn export_start(&mut self, requests: Vec<ExportRequest>, now: Instant) -> Outcome {
        if self.shut_down {
            return Outcome::nothing();
        }
        let sources = self.export_sources();
        if requests
            .iter()
            .any(|request| !sources.iter().any(|source| source.source == request.source))
        {
            self.export_notice =
                Some("An export source changed. Open the source in Analyze again.".to_owned());
        } else if self.journal.is_none() {
            self.export_notice = Some(
                "Studio cannot save export jobs in this session. Open a writable Studio session."
                    .to_owned(),
            );
        } else {
            self.export_notice = self.exports.add(requests).err();
            if self.export_notice.is_none() {
                self.persist();
                self.advance_export(now);
            }
        }
        export_changed()
    }

    /// Whether a file is waiting, running, or stopping.
    #[must_use]
    pub fn export_active(&self) -> bool {
        self.export.is_some() || self.exports.active()
    }

    pub(crate) fn export_cancelling(&self) -> bool {
        self.export.as_ref().is_some_and(|active| {
            self.exports
                .rows()
                .iter()
                .any(|job| job.id == active.id && job.phase == Phase::Cancelling)
        })
    }

    /// The latest request error that needs the user's attention.
    #[must_use]
    pub fn export_notice(&self) -> Option<&str> {
        self.export_notice.as_deref()
    }

    /// Cancels the active export and the remaining files in the batch.
    pub fn export_cancel_remaining(&mut self, now: Instant) -> Outcome {
        let had_queued = self.exports.next_queued().is_some();
        self.exports.cancel_queued();
        if had_queued {
            self.persist();
        }
        let mut outcome = export_changed();
        if let Some(id) = self.export.as_ref().map(|active| active.id) {
            outcome.merge(self.cancel_export_file(id, now));
        }
        outcome
    }

    pub(super) fn cancel_export_file(&mut self, id: u64, now: Instant) -> Outcome {
        let Some(job) = self.exports.job(id) else {
            return Outcome::nothing();
        };
        let mut outcome = export_changed();
        match job.phase {
            Phase::Queued => job.phase = Phase::Finished(ExportOutcome::Cancelled),
            Phase::Running(ExportStage::Publishing) => return Outcome::nothing(),
            Phase::Running(_) => {
                job.phase = Phase::Cancelling;
                if let Some(worker) = self
                    .export
                    .as_mut()
                    .and_then(|active| active.worker.as_mut())
                {
                    if worker.send(id, Command::Cancel).is_err() {
                        worker.close_input();
                    }
                    outcome.ui.push(UiEffect::ExportCancelTimer(id));
                } else {
                    job.phase = Phase::Finished(ExportOutcome::Cancelled);
                }
            }
            _ => return Outcome::nothing(),
        }
        self.persist();
        outcome.merge(self.release_export(now));
        outcome
    }

    /// Kills a worker if the same export is still stopping at its deadline.
    pub fn export_cancel_timed_out(&mut self, id: u64, now: Instant) -> Outcome {
        if self
            .exports
            .job(id)
            .is_some_and(|job| job.phase == Phase::Cancelling)
            && let Some(worker) = self
                .export
                .as_ref()
                .filter(|active| active.id == id)
                .and_then(|active| active.worker.as_ref())
        {
            worker.kill();
        }
        self.release_export(now)
    }

    pub(super) fn remove_export(&mut self, id: u64) -> Outcome {
        if self.export.as_ref().is_some_and(|active| active.id == id) {
            return Outcome::nothing();
        }
        if let Some(job) = self.exports.job(id).filter(|job| !job.phase.active()) {
            job.cleanup_partials();
            if job.temporary.is_empty() {
                self.exports.remove(id);
                self.export_notice = None;
            } else {
                self.export_notice = Some(
                    "Studio kept a temporary file for this export. See its details in Jobs."
                        .to_owned(),
                );
            }
            self.persist();
            return export_changed();
        }
        Outcome::nothing()
    }

    pub(super) fn advance_export(&mut self, now: Instant) {
        if self.job.is_some()
            || self.encode_save.is_some()
            || self.export.is_some()
            || self.shut_down
        {
            return;
        }
        let Some(id) = self.exports.next_queued() else {
            return;
        };
        if let Some(job) = self.exports.job(id) {
            job.phase = Phase::Running(ExportStage::Checking);
        }
        self.export = Some(Active {
            id,
            save: None,
            worker: None,
        });
        if let Err(message) = self.confirm_export(id, Save::Start) {
            if let Some(job) = self.exports.job(id) {
                job.failure(message);
            }
            self.exports
                .interrupt("Studio could not save the export queue.");
            self.persist();
            let _ = self.release_export(now);
        }
    }

    fn confirm_export(&mut self, id: u64, save: Save) -> Result<(), String> {
        let writer = self
            .journal
            .as_ref()
            .ok_or_else(|| "Studio cannot save the export queue.".to_owned())?;
        let bytes = journal::encode(&self.journal_snapshot(self.view()))
            .map_err(|error| error.to_string())?;
        if let Some(active) = self.export.as_mut() {
            active.save = Some(save);
        }
        let post = Arc::clone(&self.post);
        writer.submit_confirmed(bytes, move |result| {
            post(Route::ExportSaved(id), Delivery::JournalSaved(result));
        });
        Ok(())
    }

    pub(super) fn export_saved(&mut self, id: u64, delivery: Delivery, now: Instant) -> Outcome {
        let Delivery::JournalSaved(result) = delivery else {
            return Outcome::nothing();
        };
        let Some(active) = self.export.as_mut().filter(|active| active.id == id) else {
            return Outcome::nothing();
        };
        let Some(save) = active.save.take() else {
            return Outcome::nothing();
        };
        if self
            .exports
            .job(id)
            .is_none_or(|job| !matches!(job.phase, Phase::Running(_)))
        {
            return self.release_export(now);
        }
        let journal_saved = result.is_ok();
        let result = match (save, result) {
            (Save::Start, Ok(())) => self.start_export_worker(id),
            (Save::Proceed, Ok(())) => self.send_export(id, Command::ExportProceed),
            (_, Err(error)) => {
                self.exports
                    .interrupt("Studio could not save the export queue.");
                Err(format!("Cannot save the export job. {error}"))
            }
        };
        let mut outcome = export_changed();
        if let Err(message) = result {
            if journal_saved
                && self
                    .export
                    .as_ref()
                    .is_some_and(|active| active.worker.is_some())
            {
                self.interrupt_export(id, message);
            } else {
                self.fail_export(id, message);
            }
            outcome.merge(self.release_export(now));
        }
        outcome
    }

    fn start_export_worker(&mut self, id: u64) -> Result<(), String> {
        let export = self
            .exports
            .job(id)
            .ok_or_else(|| "The export job is missing.".to_owned())?
            .request
            .clone();
        let worker = self.spawn(Route::Export(id))?;
        let active = self
            .export
            .as_mut()
            .ok_or_else(|| "The export job is missing.".to_owned())?;
        active.worker = Some(worker);
        self.send_export(
            id,
            Command::ExportPrepare {
                export: Box::new(export),
            },
        )
    }

    fn send_export(&mut self, id: u64, command: Command) -> Result<(), String> {
        self.export
            .as_mut()
            .and_then(|active| active.worker.as_mut())
            .ok_or_else(|| "The export worker stopped.".to_owned())?
            .send(id, command)
            .map_err(|error| format!("Cannot send the export request. {error}"))
    }

    fn fail_export(&mut self, id: u64, message: String) {
        if let Some(job) = self.exports.job(id) {
            job.failure(message);
        }
        self.close_export_worker();
        self.persist();
    }

    fn interrupt_export(&mut self, id: u64, message: String) {
        if let Some(job) = self.exports.job(id) {
            job.phase = Phase::Interrupted(message);
        }
        self.close_export_worker();
        self.persist();
    }

    fn close_export_worker(&mut self) {
        if let Some(worker) = self
            .export
            .as_mut()
            .and_then(|active| active.worker.as_mut())
        {
            worker.close_input();
        }
    }

    pub(super) fn deliver_export(&mut self, id: u64, delivery: Delivery, now: Instant) -> Outcome {
        if self
            .export
            .as_ref()
            .is_none_or(|active| active.id != id || active.worker.is_none())
        {
            return Outcome::nothing();
        }
        match delivery {
            Delivery::Event(event) if event.request_id == id => self.export_event(id, event.body),
            Delivery::Ended(ending) => {
                if let Some(active) = self.export.as_mut() {
                    active.worker = None;
                }
                if let Some(job) = self.exports.job(id) {
                    if job.phase.active() {
                        job.phase = Phase::Interrupted(format!(
                            "The export worker stopped. {} Check the output paths for incomplete files.",
                            ending_text(&ending)
                        ));
                    }
                    job.cleanup_partials();
                }
                self.persist();
                let mut outcome = export_changed();
                outcome.merge(self.release_export(now));
                outcome
            }
            _ => Outcome::nothing(),
        }
    }

    fn export_event(&mut self, id: u64, event: Event) -> Outcome {
        let temps_saved = self
            .export
            .as_ref()
            .is_some_and(|active| active.save.is_none());
        let Some(job) = self.exports.job(id).filter(|job| job.phase.active()) else {
            return Outcome::nothing();
        };
        match event {
            Event::ExportTempCreated { file } => {
                if let Err(message) = job.register(file) {
                    self.fail_export(id, message);
                } else {
                    self.persist();
                }
            }
            Event::ExportPrepared { files } => {
                if matches!(job.phase, Phase::Cancelling) {
                    return Outcome::nothing();
                }
                let prepared = job.prepared(&files);
                if prepared.is_ok() {
                    job.publication_possible = true;
                }
                if let Err(message) = prepared.and_then(|()| self.confirm_export(id, Save::Proceed))
                {
                    self.fail_export(id, message);
                }
            }
            Event::ExportStage { stage } => {
                if !job.set_stage(stage) {
                    self.fail_export(
                        id,
                        "The worker reported an unexpected export stage.".to_owned(),
                    );
                }
            }
            Event::ExportProgress { progress } => {
                job.progress = Some(progress);
                return Outcome {
                    changed: Changed::Counters,
                    ..Outcome::nothing()
                };
            }
            Event::ExportPublishReady { frames } => {
                match job.authorize_publication(frames, temps_saved) {
                    Ok(true) => {
                        if let Err(message) = self.send_export(id, Command::ExportPublish) {
                            self.interrupt_export(id, message);
                        }
                    }
                    Ok(false) => return Outcome::nothing(),
                    Err(message) => self.fail_export(id, message.to_owned()),
                }
            }
            Event::ExportPublished { file } => {
                if job.published(file) {
                    self.persist();
                } else {
                    self.fail_export(id, "The worker reported an unexpected output.".to_owned());
                }
            }
            Event::ExportFinished { outcome } => {
                job.finish(outcome);
                self.close_export_worker();
                self.persist();
            }
            Event::Error {
                code: ErrorCode::WrongState,
                ..
            } if job.phase == Phase::Cancelling => {
                return Outcome::nothing();
            }
            Event::Error { message, .. } => self.fail_export(
                id,
                format!("The export worker refused the request. {message}"),
            ),
            _ => return Outcome::nothing(),
        }
        export_changed()
    }

    fn release_export(&mut self, now: Instant) -> Outcome {
        if self
            .export
            .as_ref()
            .is_some_and(|active| active.worker.is_none() && active.save.is_none())
        {
            self.export = None;
            self.dispatch(Intent::WorkerIdle, now)
        } else {
            Outcome::nothing()
        }
    }

    pub(super) fn interrupt_exports(&mut self) {
        self.exports
            .interrupt("Studio closed before this export finished. Its outputs may be incomplete.");
    }

    pub(super) fn take_export_worker(&mut self) -> Option<Worker> {
        self.export.take()?.worker
    }
}

fn export_changed() -> Outcome {
    Outcome {
        changed: Changed::Phase,
        ..Outcome::nothing()
    }
}
