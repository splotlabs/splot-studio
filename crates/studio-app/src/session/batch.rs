// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Owns the dedicated inspect process that serves batch validation and reports.

use std::path::{Path, PathBuf};
use std::time::Instant;

use studio_domain::validation::{ReportFormat, ValidationSettings};

use super::{Changed, Delivery, Intent, Outcome, Role, Route, Session, Workspace, ending_text};
use crate::batch::{Effect, Report};
use crate::jobs::Work;
use crate::queue::PartialFate;
use crate::recovery;

impl Session {
    /// Submits local files and one immutable set of options.
    pub fn batch_start(&mut self, paths: Vec<PathBuf>, settings: ValidationSettings) -> Outcome {
        if self.shut_down {
            return Outcome::nothing();
        }
        let effect = self.batch.start(paths, settings);
        self.perform_batch(effect, Instant::now())
    }

    /// Whether the current batch has files waiting or running.
    #[must_use]
    pub const fn batch_active(&self) -> bool {
        self.batch.active()
    }

    /// The outcome of the latest batch or report action that needs attention.
    #[must_use]
    pub fn batch_notice(&self) -> Option<&str> {
        self.batch.notice.as_deref()
    }

    /// Stops the active file and cancels the files still waiting in this batch.
    pub fn batch_cancel_remaining(&mut self) -> Outcome {
        let effect = self.batch.cancel_remaining();
        self.perform_batch(effect, Instant::now())
    }

    /// Identifies a report before the user chooses a destination.
    #[must_use]
    pub fn batch_export_target(&self, index: usize) -> Option<(u64, String)> {
        let jobs = self.jobs();
        let job = jobs.get(index).filter(|job| job.can_export())?;
        let Work::Validate(file) = job.work else {
            return None;
        };
        Some((
            file.id,
            format!("{}-validation", crate::present::file_name(&file.path)),
        ))
    }

    /// Exports a complete report with the job ID captured when the dialog opened.
    pub fn batch_export(&mut self, id: u64, destination: &Path, format: ReportFormat) -> Outcome {
        let effect = self.batch.export(id, destination, format);
        self.perform_batch(effect, Instant::now())
    }

    /// Removes a terminal row from Jobs.
    pub fn remove_job(&mut self, index: usize) -> Outcome {
        let work = self
            .jobs()
            .get(index)
            .filter(|job| job.can_remove())
            .map(|job| job.work);
        match work {
            Some(Work::Validate(file)) => {
                let effect = self.batch.remove(file.id);
                self.perform_batch(effect, Instant::now())
            }
            Some(Work::Export(job)) => self.remove_export(job.id),
            _ => Outcome::nothing(),
        }
    }

    pub(super) fn show_batch(&mut self, id: u64, now: Instant) -> Outcome {
        let effect = self.batch.show(id);
        self.perform_batch(effect, now)
    }

    pub(super) fn cancel_batch_file(&mut self, id: u64, now: Instant) -> Outcome {
        let effect = self.batch.cancel(id);
        self.perform_batch(effect, now)
    }

    pub(super) fn deliver_batch(
        &mut self,
        spawn: u64,
        delivery: Delivery,
        now: Instant,
    ) -> Outcome {
        if self
            .batch_worker
            .as_ref()
            .is_none_or(|(current, _)| *current != spawn)
        {
            return Outcome::nothing();
        }
        let effect = match delivery {
            Delivery::Event(envelope) => self.batch.event(envelope.request_id, envelope.body),
            Delivery::Ended(ending) => {
                self.batch_worker = None;
                let detail = ending_text(&ending);
                let cleanup = self.cleanup_batch_partial();
                self.batch.interrupted(&format!("{detail}{cleanup}"));
                None
            }
            _ => return Outcome::nothing(),
        };
        self.perform_batch(effect, now)
    }

    fn perform_batch(&mut self, effect: Option<Effect>, now: Instant) -> Outcome {
        let mut outcome = Outcome {
            changed: Changed::Phase,
            ..Outcome::nothing()
        };
        match effect {
            Some(Effect::Send(id, command)) => self.send_batch(id, command),
            Some(Effect::Show(report)) => outcome.merge(self.show_batch_report(report, now)),
            None => {}
        }
        if !self.batch.needs_worker() {
            self.batch_worker = None;
        }
        outcome
    }

    fn show_batch_report(&mut self, report: Report, now: Instant) -> Outcome {
        let effects = self.analysis.open_batch_result(report);
        self.send_inspect(effects);
        self.persist_view();
        let mut outcome = self.dispatch(Intent::WorkspaceRequested(Workspace::Analyze), now);
        outcome.analysis |= self.analysis.take_dirty();
        outcome
    }

    fn send_batch(&mut self, id: u64, command: studio_domain::protocol::Command) {
        if self.batch_worker.is_none() {
            let spawn = self.batch_workers_started;
            self.batch_workers_started += 1;
            match self.spawn_as(Role::Inspect, Route::Batch(spawn)) {
                Ok(worker) => self.batch_worker = Some((spawn, worker)),
                Err(error) => {
                    self.batch.interrupted(&error);
                    return;
                }
            }
        }
        if let Some((_, worker)) = &mut self.batch_worker
            && let Err(error) = worker.send(id, command)
        {
            worker.close_input();
            self.batch.notice = Some(format!("Cannot send the validation request. {error}"));
        }
    }

    pub(super) fn cleanup_batch_partial(&mut self) -> String {
        let Some((partial, destination)) = self.batch.take_partial() else {
            return String::new();
        };
        match recovery::remove_partial(&partial, &destination) {
            PartialFate::Kept { path, reason } => {
                format!(
                    " Studio kept the temporary report at {}. {reason}",
                    path.display()
                )
            }
            _ => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;
    use std::sync::Arc;

    use studio_domain::analysis::AnalysisFailure;
    use studio_domain::job::WirePath;
    use studio_domain::protocol::{Command, Event};
    use studio_domain::validation::{BatchOutcome, ReportFormat, ValidationSettings};

    use super::{Effect, Session};
    use crate::state::AppState;
    use crate::testing::Scratch;

    #[test]
    fn interrupted_export_recovers_only_the_announced_partial() -> Result<(), Box<dyn Error>> {
        let scratch = Scratch::new("batch-export-recovery");
        let destination = scratch.path("report.json");
        let partial = scratch.path(".report.json.abcdef.partial");
        let unrelated = scratch.path(".report.json.123456.partial");
        fs::write(&partial, b"incomplete report")?;
        fs::write(&unrelated, b"unrelated")?;
        let mut session = Session::new(
            AppState::default(),
            Err("No engine in this test.".into()),
            Arc::new(|_, _| {}),
            None,
        );
        let Some(Effect::Send(check, Command::BatchCheckSettings { .. })) = session.batch.start(
            vec![scratch.path("source.obu")],
            ValidationSettings::default(),
        ) else {
            return Err("expected settings check".into());
        };
        let Some(Effect::Send(validation, Command::BatchValidate { .. })) =
            session.batch.event(check, Event::BatchSettingsChecked)
        else {
            return Err("expected validation".into());
        };
        session.batch.event(
            validation,
            Event::BatchValidated {
                outcome: Box::new(BatchOutcome::Failed {
                    code: AnalysisFailure::Io,
                    message: "Cannot open the source.".into(),
                }),
            },
        );
        let Some(Effect::Send(request, Command::BatchExport { .. })) =
            session
                .batch
                .export(validation, &destination, ReportFormat::Json)
        else {
            return Err("expected export".into());
        };
        session.batch.event(
            request + 1,
            Event::BatchExportPrepared {
                validation,
                partial: WirePath::from_path(&unrelated),
            },
        );
        session.batch.event(
            request,
            Event::BatchExportPrepared {
                validation,
                partial: WirePath::from_path(&partial),
            },
        );
        assert_eq!(session.cleanup_batch_partial(), "");
        assert!(!partial.exists());
        assert_eq!(fs::read(&unrelated)?, b"unrelated");
        assert!(!destination.exists());
        Ok(())
    }
}
