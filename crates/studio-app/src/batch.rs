// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Session-local validation jobs and their report requests.

use std::path::{Path, PathBuf};

use studio_domain::analysis::{MAX_PAGE_ELEMENTS, SourceDigest};
use studio_domain::job::{SourceFingerprint, WirePath};
use studio_domain::protocol::{Command, Event};
use studio_domain::validation::{
    BatchOutcome, Finding, MAX_BATCH_FILES, MAX_EXTERNAL_HLS_BYTES, ReportFormat,
    VALIDATION_SCHEMA, ValidationSettings, ValidationSummary,
};

/// One file submitted through Validate Files.
#[derive(Debug)]
pub struct File {
    pub(crate) id: u64,
    pub(crate) path: PathBuf,
    pub(crate) phase: Phase,
    pub(crate) archived: bool,
}

#[derive(Debug)]
pub(crate) enum Phase {
    Queued,
    Opening,
    Validating,
    Cancelling,
    Report(Box<BatchOutcome>),
    Cancelled,
    Failed(String),
    Interrupted,
}

pub(crate) enum Effect {
    Send(u64, Command),
    Show(Report),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Report {
    pub path: PathBuf,
    pub fingerprint: SourceFingerprint,
    pub digest: SourceDigest,
    pub summary: ValidationSummary,
    pub findings: Vec<Finding>,
}

#[derive(Debug, Clone, Copy)]
enum Pending {
    Check(u64),
    Validate(u64),
}

impl Pending {
    const fn id(self) -> u64 {
        match self {
            Self::Check(id) | Self::Validate(id) => id,
        }
    }
}

struct Run {
    settings: ValidationSettings,
    pending: Pending,
}

enum Action {
    Show(Vec<Finding>),
    Export {
        destination: PathBuf,
        partial: Option<PathBuf>,
    },
    Remove,
}

struct ReportRequest {
    request: u64,
    validation: u64,
    action: Action,
}

#[derive(Default)]
pub(crate) struct Batch {
    pub files: Vec<File>,
    pub notice: Option<String>,
    run: Option<Run>,
    report: Option<ReportRequest>,
    next_id: u64,
}

impl Batch {
    pub const fn active(&self) -> bool {
        self.run.is_some()
    }

    pub fn needs_worker(&self) -> bool {
        self.active() || self.report.is_some() || self.files.iter().any(|file| file.archived)
    }

    pub fn start(&mut self, paths: Vec<PathBuf>, settings: ValidationSettings) -> Option<Effect> {
        if paths.is_empty() {
            return None;
        }
        if self.active() {
            self.notice = Some("Wait for the current validation batch or stop it.".into());
            return None;
        }
        let available = MAX_BATCH_FILES.saturating_sub(self.files.len());
        if paths.len() > available {
            self.notice = Some(format!(
                "{} of {MAX_BATCH_FILES} validation jobs remain in Jobs. Only {available} more files fit. Remove completed results or select fewer files.",
                self.files.len()
            ));
            return None;
        }
        if settings
            .external_hls
            .as_ref()
            .is_some_and(|bytes| bytes.len() > MAX_EXTERNAL_HLS_BYTES)
        {
            self.notice = Some(format!(
                "Select an external context of at most {MAX_EXTERNAL_HLS_BYTES} bytes."
            ));
            return None;
        }
        self.notice = None;
        for path in paths {
            let id = self.next();
            self.files.push(File {
                id,
                path,
                phase: Phase::Queued,
                archived: false,
            });
        }
        let id = self.next();
        self.run = Some(Run {
            settings: settings.clone(),
            pending: Pending::Check(id),
        });
        Some(Effect::Send(id, Command::BatchCheckSettings { settings }))
    }

    pub fn cancel_remaining(&mut self) -> Option<Effect> {
        for file in &mut self.files {
            if matches!(file.phase, Phase::Queued) {
                file.phase = Phase::Cancelled;
            }
        }
        let pending = self.run.as_ref()?.pending;
        match pending {
            Pending::Check(_) => {
                self.run = None;
                None
            }
            Pending::Validate(id) => self.cancel(id),
        }
    }

    pub fn cancel(&mut self, validation: u64) -> Option<Effect> {
        let file = self.file_mut(validation)?;
        match file.phase {
            Phase::Queued => {
                file.phase = Phase::Cancelled;
                None
            }
            Phase::Opening | Phase::Validating => {
                file.phase = Phase::Cancelling;
                Some(Effect::Send(
                    self.next(),
                    Command::BatchCancel { validation },
                ))
            }
            _ => None,
        }
    }

    pub fn show(&mut self, validation: u64) -> Option<Effect> {
        self.file(validation).filter(|file| {
            file.archived
                && matches!(file.phase, Phase::Report(ref outcome)
                if matches!(**outcome, BatchOutcome::Validated { .. }))
        })?;
        self.fetch(validation, Vec::new())
    }

    pub fn export(
        &mut self,
        validation: u64,
        destination: &Path,
        format: ReportFormat,
    ) -> Option<Effect> {
        self.file(validation).filter(|file| file.archived)?;
        self.begin_report_request(
            validation,
            Action::Export {
                destination: destination.to_path_buf(),
                partial: None,
            },
            Command::BatchExport {
                validation,
                destination: WirePath::from_path(destination),
                format,
            },
        )
    }

    pub fn remove(&mut self, validation: u64) -> Option<Effect> {
        let file = self.file(validation)?;
        if file.archived {
            self.begin_report_request(
                validation,
                Action::Remove,
                Command::BatchForget { validation },
            )
        } else if matches!(
            file.phase,
            Phase::Cancelled | Phase::Failed(_) | Phase::Interrupted | Phase::Report(_)
        ) {
            self.files.retain(|file| file.id != validation);
            self.notice = None;
            None
        } else {
            None
        }
    }

    pub fn event(&mut self, request: u64, event: Event) -> Option<Effect> {
        if let Some(run) = &self.run
            && run.pending.id() == request
        {
            return match run.pending {
                Pending::Check(_) => self.settings_checked(event),
                Pending::Validate(id) => self.validation_event(id, event),
            };
        }
        if self
            .report
            .as_ref()
            .is_some_and(|pending| pending.request == request)
            && let Some(pending) = self.report.take()
        {
            return self.report_event(pending, event);
        }
        None
    }

    pub fn take_partial(&mut self) -> Option<(PathBuf, PathBuf)> {
        let Action::Export {
            destination,
            partial,
        } = &mut self.report.as_mut()?.action
        else {
            return None;
        };
        Some((partial.take()?, destination.clone()))
    }

    pub fn interrupted(&mut self, detail: &str) {
        self.run = None;
        self.report = None;
        for file in &mut self.files {
            file.archived = false;
            if matches!(
                file.phase,
                Phase::Opening | Phase::Validating | Phase::Cancelling | Phase::Queued
            ) {
                file.phase = Phase::Interrupted;
            }
        }
        self.notice = Some(format!("The validation worker stopped. {detail}"));
    }

    fn settings_checked(&mut self, event: Event) -> Option<Effect> {
        match event {
            Event::BatchSettingsChecked => self.next_file(),
            event => {
                let why = refusal(event);
                self.run = None;
                for file in &mut self.files {
                    if matches!(file.phase, Phase::Queued) {
                        file.phase = Phase::Failed(why.clone());
                    }
                }
                self.notice = Some(why);
                None
            }
        }
    }

    fn validation_event(&mut self, validation: u64, event: Event) -> Option<Effect> {
        let file = self.file_mut(validation)?;
        match event {
            Event::BatchValidating {
                validation: reported,
            } if reported == validation => {
                if matches!(file.phase, Phase::Opening) {
                    file.phase = Phase::Validating;
                }
                return None;
            }
            Event::BatchValidated { outcome } => {
                file.archived = true;
                file.phase = if matches!(&*outcome, BatchOutcome::Validated { summary, .. }
                    if summary.schema != VALIDATION_SCHEMA)
                {
                    Phase::Failed("The worker returned an unsupported validation schema.".into())
                } else {
                    Phase::Report(outcome)
                };
            }
            event => file.phase = Phase::Failed(refusal(event)),
        }
        self.next_file()
    }

    fn next_file(&mut self) -> Option<Effect> {
        let mut run = self.run.take()?;
        let file = self
            .files
            .iter_mut()
            .find(|file| matches!(file.phase, Phase::Queued))?;
        file.phase = Phase::Opening;
        let validation = file.id;
        let command = Command::BatchValidate {
            source: WirePath::from_path(&file.path),
            settings: run.settings.clone(),
        };
        run.pending = Pending::Validate(validation);
        self.run = Some(run);
        Some(Effect::Send(validation, command))
    }

    fn report_event(&mut self, pending: ReportRequest, event: Event) -> Option<Effect> {
        let id = pending.validation;
        match (pending.action, event) {
            (
                Action::Show(findings),
                Event::BatchFindings {
                    validation,
                    first,
                    total,
                    findings: page,
                },
            ) if validation == id => self.accept_page(id, findings, first, total, page),
            (
                Action::Export {
                    destination,
                    partial: None,
                },
                Event::BatchExportPrepared {
                    validation,
                    partial,
                },
            ) if validation == id => {
                let partial = partial.to_path_buf().ok();
                self.report = Some(ReportRequest {
                    action: Action::Export {
                        destination,
                        partial,
                    },
                    ..pending
                });
                None
            }
            (Action::Export { .. }, Event::BatchExported { validation, .. })
                if validation == id =>
            {
                self.notice = Some("The validation report was exported.".into());
                None
            }
            (Action::Remove, Event::BatchForgotten { validation }) if validation == id => {
                self.files.retain(|file| file.id != id);
                None
            }
            (_, event) => {
                self.notice = Some(refusal(event));
                None
            }
        }
    }

    fn accept_page(
        &mut self,
        validation: u64,
        mut retained: Vec<Finding>,
        first: u64,
        total: u64,
        page: Vec<Finding>,
    ) -> Option<Effect> {
        let file = self.file(validation)?;
        let Phase::Report(outcome) = &file.phase else {
            return None;
        };
        let BatchOutcome::Validated {
            fingerprint,
            digest,
            summary,
            total: expected,
        } = &**outcome
        else {
            return None;
        };
        let end = first.saturating_add(page.len() as u64);
        if first != retained.len() as u64
            || total != *expected
            || total > u64::from(summary.finding_limit).min(4_096)
            || page.len() > MAX_PAGE_ELEMENTS as usize
            || end > total
            || (first < total && page.is_empty())
        {
            self.notice = Some("The worker returned an invalid validation page.".into());
            return None;
        }
        retained.extend(page);
        if end < total {
            self.fetch(validation, retained)
        } else {
            Some(Effect::Show(Report {
                path: file.path.clone(),
                fingerprint: *fingerprint,
                digest: digest.clone(),
                summary: (**summary).clone(),
                findings: retained,
            }))
        }
    }

    fn fetch(&mut self, validation: u64, findings: Vec<Finding>) -> Option<Effect> {
        let first = findings.len() as u64;
        self.begin_report_request(
            validation,
            Action::Show(findings),
            Command::BatchFindings {
                validation,
                first,
                count: MAX_PAGE_ELEMENTS,
            },
        )
    }

    fn begin_report_request(
        &mut self,
        validation: u64,
        action: Action,
        command: Command,
    ) -> Option<Effect> {
        if self.report.is_some() {
            self.notice = Some("Wait for the current report operation.".into());
            return None;
        }
        self.notice = None;
        let request = self.next();
        self.report = Some(ReportRequest {
            request,
            validation,
            action,
        });
        Some(Effect::Send(request, command))
    }

    fn file(&self, validation: u64) -> Option<&File> {
        self.files.iter().find(|file| file.id == validation)
    }

    fn file_mut(&mut self, validation: u64) -> Option<&mut File> {
        self.files.iter_mut().find(|file| file.id == validation)
    }

    fn next(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }
}

fn refusal(event: Event) -> String {
    match event {
        Event::Error { message, .. } => message,
        _ => "The validation worker returned an unexpected reply.".into(),
    }
}
