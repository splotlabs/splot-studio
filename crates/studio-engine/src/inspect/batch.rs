// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Batch results belong to the inspect process and outlive its open documents.

use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::SyncSender;

use studio_domain::analysis::AnalysisFailure;
use studio_domain::job::WirePath;
use studio_domain::protocol::{Command, ErrorCode, Event};
use studio_domain::validation::{
    BatchOutcome, ContextIdentity, Finding, MAX_BATCH_FILES, ReportFormat, ValidationSettings,
};

use super::document::{Document, Refusal, evidence::page};
use super::{Pending, Queued, Shared, refusal, report, validation};
use crate::job::Stop;
use crate::output::FileId;

#[derive(Debug)]
pub(super) enum Request {
    CheckSettings(ValidationSettings),
    Validate {
        source: WirePath,
        settings: ValidationSettings,
    },
    Findings {
        validation: u64,
        first: u64,
        count: u32,
    },
    Export {
        validation: u64,
        destination: WirePath,
        format: ReportFormat,
    },
    Forget(u64),
}

impl Request {
    pub(super) fn from_command(command: &Command) -> Option<Self> {
        Some(match command {
            Command::BatchCheckSettings { settings } => Self::CheckSettings(settings.clone()),
            Command::BatchValidate { source, settings } => Self::Validate {
                source: source.clone(),
                settings: settings.clone(),
            },
            Command::BatchFindings {
                validation,
                first,
                count,
            } => Self::Findings {
                validation: *validation,
                first: *first,
                count: *count,
            },
            Command::BatchExport {
                validation,
                destination,
                format,
            } => Self::Export {
                validation: *validation,
                destination: destination.clone(),
                format: *format,
            },
            Command::BatchForget { validation } => Self::Forget(*validation),
            _ => return None,
        })
    }
}

pub(super) fn queue<W: Write>(
    shared: &Shared<W>,
    requests: &SyncSender<Queued>,
    id: u64,
    request: Request,
) {
    if matches!(request, Request::Validate { .. }) {
        let operation = Pending::BatchValidation(id);
        let mut ledger = shared.ledger();
        if ledger.contains(operation) {
            drop(ledger);
            shared.send(id, duplicate());
        } else if requests
            .try_send((id, 0, super::Request::Batch(request)))
            .is_ok()
        {
            ledger.pending.push((operation, false));
        } else {
            drop(ledger);
            shared.send(id, busy());
        }
    } else if requests
        .try_send((id, 0, super::Request::Batch(request)))
        .is_err()
    {
        shared.send(id, busy());
    }
}

#[derive(Default)]
pub(super) struct Archive {
    entries: Vec<(u64, Entry)>,
    newest: Option<u64>,
}

struct Entry {
    source: PathBuf,
    source_id: Option<FileId>,
    strict: bool,
    context: Option<ContextIdentity>,
    outcome: BatchOutcome,
    findings: Vec<Finding>,
}

impl Archive {
    pub(super) fn work<W: Write>(&mut self, shared: &Shared<W>, id: u64, request: Request) {
        let event = match request {
            Request::CheckSettings(settings) => match validation::check_settings(&settings) {
                Ok(_) => Event::BatchSettingsChecked,
                Err(error) => refusal(ErrorCode::Malformed, error.message),
            },
            Request::Validate { source, settings } => {
                let operation = Pending::BatchValidation(id);
                let event = self.validate(id, &source, &settings, shared, operation);
                let mut ledger = shared.ledger();
                ledger.settle(operation);
                shared.send(id, event);
                drop(ledger);
                return;
            }
            Request::Findings {
                validation,
                first,
                count,
            } => self.findings(validation, first, count),
            Request::Export {
                validation,
                destination,
                format,
            } => self.export(shared, id, validation, &destination, format),
            Request::Forget(validation) => {
                if let Some(at) = self.entries.iter().position(|(id, _)| *id == validation) {
                    self.entries.remove(at);
                    Event::BatchForgotten { validation }
                } else {
                    missing()
                }
            }
        };
        shared.send(id, event);
    }

    fn validate<W: Write>(
        &mut self,
        id: u64,
        source: &WirePath,
        settings: &ValidationSettings,
        shared: &Shared<W>,
        operation: Pending,
    ) -> Event {
        if self.newest.is_some_and(|newest| id <= newest) {
            return duplicate();
        }
        if self.entries.len() >= MAX_BATCH_FILES {
            return refusal(
                ErrorCode::Busy,
                format!("Keep at most {MAX_BATCH_FILES} batch reports."),
            );
        }
        let source_path = match source.to_path_buf() {
            Ok(path) => path,
            Err(error) => return refusal(ErrorCode::Malformed, error.to_string()),
        };
        let context = match validation::check_settings(settings) {
            Ok(context) => context,
            Err(error) => return refusal(ErrorCode::Malformed, error.message),
        };
        let mut entry = Entry {
            source: source_path,
            source_id: None,
            strict: settings.strict,
            context,
            outcome: BatchOutcome::Failed {
                code: AnalysisFailure::Cancelled,
                message: "Validation was cancelled before the source opened.".into(),
            },
            findings: Vec::new(),
        };
        let cancelled = || shared.cancelled(operation);
        if !cancelled() {
            entry.collect(source, settings, &cancelled, &|| {
                shared.send(id, Event::BatchValidating { validation: id });
            });
        }
        let outcome = Box::new(entry.outcome.clone());
        self.entries.push((id, entry));
        self.newest = Some(id);
        Event::BatchValidated { outcome }
    }

    fn find(&self, validation: u64) -> Option<&Entry> {
        self.entries
            .iter()
            .find(|(id, _)| *id == validation)
            .map(|(_, entry)| entry)
    }

    fn findings(&self, validation: u64, first: u64, count: u32) -> Event {
        let Some(entry) = self.find(validation) else {
            return missing();
        };
        if let BatchOutcome::Validated { fingerprint, .. } = &entry.outcome
            && entry.source_id.as_ref().is_none_or(|identity| {
                crate::source::still_the_same(&entry.source, identity, *fingerprint).is_err()
            })
        {
            return refusal(
                ErrorCode::WrongState,
                "The source changed. Validate it again to inspect its findings.",
            );
        }
        match page(&entry.findings, first, count) {
            Ok(page) => Event::BatchFindings {
                validation,
                first,
                total: page.total,
                findings: page.entries,
            },
            Err(error) => refusal(ErrorCode::Malformed, error.message),
        }
    }

    fn export<W: Write>(
        &self,
        shared: &Shared<W>,
        request: u64,
        validation: u64,
        destination: &WirePath,
        format: ReportFormat,
    ) -> Event {
        let Some(entry) = self.find(validation) else {
            return missing();
        };
        let destination = match destination.to_path_buf() {
            Ok(path) => path,
            Err(error) => return refusal(ErrorCode::Malformed, error.to_string()),
        };
        let report = report::Report {
            source: &entry.source,
            source_id: entry.source_id.as_ref(),
            strict: entry.strict,
            context: entry.context.as_ref(),
            outcome: &entry.outcome,
            findings: &entry.findings,
        };
        let prepared = |partial: &std::path::Path| {
            shared.send(
                request,
                Event::BatchExportPrepared {
                    validation,
                    partial: WirePath::from_path(partial),
                },
            );
            if shared.stopped() {
                Err(Stop::Cancelled)
            } else {
                Ok(())
            }
        };
        match report::export(&destination, &report, format, &prepared, &|| {
            shared.stopped()
        }) {
            Ok(bytes) => Event::BatchExported { validation, bytes },
            Err(error) => refusal(ErrorCode::WrongState, error.to_string()),
        }
    }
}

impl Entry {
    fn collect(
        &mut self,
        source: &WirePath,
        settings: &ValidationSettings,
        cancelled: &dyn Fn() -> bool,
        validating: &dyn Fn(),
    ) {
        let report = (|| {
            let mut document = Document::open(source, cancelled)?;
            self.source_id = Some(document.identity().clone());
            validating();
            let fingerprint = document.summary().fingerprint;
            let digest = document.summary().digest.clone().ok_or_else(|| {
                Refusal::new(
                    AnalysisFailure::Io,
                    "The source scan did not return a digest.",
                )
            })?;
            let evidence = document.collect_validation(settings, cancelled)?;
            let total = evidence.findings.len() as u64;
            self.findings = evidence.findings;
            Ok(BatchOutcome::Validated {
                fingerprint,
                digest: *digest,
                summary: Box::new(evidence.summary),
                total,
            })
        })();
        self.outcome = report
            .unwrap_or_else(|Refusal { code, message }| BatchOutcome::Failed { code, message });
    }
}

fn duplicate() -> Event {
    refusal(
        ErrorCode::WrongState,
        "Use increasing batch validation request IDs.",
    )
}

fn missing() -> Event {
    refusal(ErrorCode::WrongState, "The batch report is unavailable.")
}

fn busy() -> Event {
    refusal(ErrorCode::Busy, "The inspect request queue is full.")
}
