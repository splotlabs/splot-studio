// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The inspect process: read-only analysis [`document`]s of local AV2 IVF
//! and Annex B files, over the same framed protocol as the worker.
//!
//! The reader accepts commands and processes cancellation without opening files.
//! The work thread serves an ordered, bounded command queue. It advances active
//! progressive scans in round-robin order between commands. A scan keeps the
//! facts it has found if the reader requests cancellation.
//!
//! The [`Ledger`] serializes cancellation and terminal replies. Both threads
//! write complete replies under one lock. One additional thread can collect
//! document evidence while the work thread serves foreground queries.

mod batch;
pub mod document;
mod identity;
mod report;
mod syntax;
mod validation;

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use studio_domain::analysis::{
    AnalysisFailure, LocateTarget, RowFilter, ScanOutcome, SourceDigest,
};
use studio_domain::identity::IdentityKind;
use studio_domain::job::WirePath;
use studio_domain::preview::GrainPolicy;
use studio_domain::protocol::{
    AnalysisEvidenceIdentity, Command, Envelope, ErrorCode, Event, ProtocolError, decode,
    read_frame, write_message,
};
use studio_domain::syntax::SYNTAX_SCHEMA;
use studio_domain::validation::{VALIDATION_SCHEMA, ValidationEvidence, ValidationSettings};

use self::document::{Document, Refusal};
use crate::job::JOB_STACK_BYTES;
use crate::worker::{SessionError, request_id_of};
use crate::{build_identity, capabilities};

/// Documents open at once; another open is refused until one is closed.
pub const MAX_DOCUMENTS: usize = 4;

/// Analysis requests waiting for the work thread.
pub const REQUEST_BACKLOG: usize = 64;

/// Serves the inspect protocol on `input` and `output` until the input ends.
/// Every frame gets a terminal reply with its `request_id`.
/// Progressive opens, batch validation and report export can also report progress.
///
/// # Errors
/// [`SessionError::Read`] on an unrecoverable input frame or failure,
/// [`SessionError::Write`] when the output fails.
pub fn run(input: &mut impl Read, output: impl Write + Send) -> Result<(), SessionError> {
    let shared = Shared {
        out: Mutex::new(Out {
            writer: output,
            error: None,
        }),
        ledger: Mutex::new(Ledger::default()),
        stopped: AtomicBool::new(false),
    };
    let (requests, queue) = mpsc::sync_channel(REQUEST_BACKLOG);
    let shared = &shared;
    thread::scope(|scope| {
        let replies = requests.clone();
        let work = thread::Builder::new()
            .name("studio-inspect".to_owned())
            .stack_size(JOB_STACK_BYTES)
            .spawn_scoped(scope, move || {
                thread::scope(|scope| work(&queue, &replies, shared, scope));
            });
        let read = match work {
            Ok(work) => {
                let read = serve(input, &requests, shared);
                shared.stopped.store(true, Ordering::SeqCst);
                // The validation producer may still send a result through the queue.
                let _ = requests.send((0, 0, Request::ReaderStopped));
                // The work body has no panics to report.
                let _ = work.join();
                read
            }
            Err(error) => Err(ProtocolError::Io(error)),
        };
        let written = match shared.out().error.take() {
            Some(error) => Err(error),
            None => Ok(()),
        };
        read.map_err(SessionError::Read)
            .and(written.map_err(SessionError::Write))
    })
}

/// One request for the work thread.
#[derive(Debug)]
enum Request {
    Open { source: WirePath, progressive: bool },
    Evidence(EvidenceRequest),
    EvidenceDone(Box<EvidenceResult>),
    ReaderStopped,
    Inspect(Query),
    Batch(batch::Request),
    Close,
    ClearCache,
}

/// Reads the selected document on the work thread.
#[derive(Debug, Clone, Copy)]
enum Query {
    Page {
        filter: RowFilter,
        first: u64,
        count: u32,
    },
    Locate {
        filter: RowFilter,
        target: LocateTarget,
    },
    Bytes {
        offset: u64,
        len: u32,
    },
    Syntax {
        element: u64,
    },
    SyntaxPage {
        trace: u64,
        first: u64,
        count: u32,
    },
    Findings {
        validation: u64,
        first: u64,
        count: u32,
    },
    IdentityPage {
        scan: u64,
        first: u64,
        count: u32,
        kind: IdentityKind,
    },
}

/// A queued request: its `request_id`, its document and what it asks.
type Queued = (u64, u64, Request);
type ValidationResult = Result<(ValidationEvidence, SourceDigest), Refusal>;

#[derive(Debug)]
enum EvidenceRequest {
    Validate(ValidationSettings),
    Identity(GrainPolicy),
}

impl EvidenceRequest {
    fn pending(&self, document: u64, id: u64) -> Pending {
        match self {
            Self::Validate(_) => Pending::Validation {
                document,
                validation: id,
            },
            Self::Identity(_) => Pending::Identity { document, scan: id },
        }
    }

    fn prepare(self, doc: &mut Document, id: u64) -> Result<EvidenceTask, Refusal> {
        Ok(match self {
            Self::Validate(settings) => {
                EvidenceTask::Validate(doc.prepare_validation(id)?, settings)
            }
            Self::Identity(grain) => {
                let file = doc.open_evidence_source()?;
                let index = doc.begin_identity(id, grain)?;
                let digest = doc.summary().digest.clone();
                EvidenceTask::Identity {
                    file,
                    index,
                    digest,
                }
            }
        })
    }
}

enum EvidenceTask {
    Validate(File, ValidationSettings),
    Identity {
        file: File,
        index: identity::IdentityIndex,
        digest: Option<Box<SourceDigest>>,
    },
}

impl EvidenceTask {
    fn run(self, cancelled: &dyn Fn() -> bool) -> EvidenceResult {
        match self {
            Self::Validate(mut file, settings) => {
                EvidenceResult::Validation(validation::validate(&mut file, &settings, cancelled))
            }
            Self::Identity {
                mut file,
                index,
                digest,
            } => EvidenceResult::Identity(index.scan(&mut file, digest.as_deref(), cancelled)),
        }
    }
}

#[derive(Debug)]
enum EvidenceResult {
    Validation(ValidationResult),
    Identity(Result<(), Refusal>),
    Aborted(Pending),
}

impl EvidenceResult {
    fn finish(self, doc: &mut Document, document: u64, id: u64) -> Result<Event, Refusal> {
        match self {
            Self::Validation(result) => {
                let (summary, total) = doc.complete_validation(id, result)?;
                Ok(Event::AnalysisValidated {
                    document,
                    summary: Box::new(summary),
                    total,
                })
            }
            Self::Identity(result) => {
                let summary = doc.complete_identity(id, result)?;
                Ok(Event::AnalysisIdentityScanned {
                    document,
                    summary: Box::new(summary),
                })
            }
            Self::Aborted(operation) => {
                if matches!(operation, Pending::Identity { .. }) {
                    doc.drop_identity(id);
                }
                Err(Refusal::new(
                    AnalysisFailure::Io,
                    "The evidence thread stopped.",
                ))
            }
        }
    }
}

struct Shared<W> {
    out: Mutex<Out<W>>,
    ledger: Mutex<Ledger>,
    /// The input ended or the output failed. Cancel all pending operations.
    stopped: AtomicBool,
}

struct Out<W> {
    writer: W,
    /// The output failed; everything after is discarded.
    error: Option<ProtocolError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    Open(u64),
    BatchValidation(u64),
    Validation { document: u64, validation: u64 },
    Identity { document: u64, scan: u64 },
}

impl Pending {
    fn belongs_to(self, member: u64) -> bool {
        match self {
            Self::Validation { document, .. } | Self::Identity { document, .. } => {
                document == member
            }
            _ => false,
        }
    }
}

/// Long operations that the reader can cancel.
#[derive(Debug, Default)]
struct Ledger {
    /// The newest document id an open was queued for.
    newest: u64,
    /// At most the queued requests and active scans and evidence pass.
    pending: Vec<(Pending, bool)>,
}

impl Ledger {
    fn admits(&self, document: u64) -> bool {
        document > self.newest
    }

    fn queue_open(&mut self, document: u64) {
        self.newest = document;
        self.pending.push((Pending::Open(document), false));
    }

    fn contains(&self, request: Pending) -> bool {
        self.pending.iter().any(|(pending, _)| *pending == request)
    }

    fn cancel(&mut self, request: Pending) -> bool {
        let Some((_, cancelled)) = self.pending.iter_mut().find(|(id, _)| *id == request) else {
            return false;
        };
        *cancelled = true;
        true
    }

    fn is_cancelled(&self, request: Pending) -> bool {
        self.pending
            .iter()
            .any(|(id, cancelled)| *id == request && *cancelled)
    }

    fn settle(&mut self, request: Pending) {
        self.pending.retain(|(id, _)| *id != request);
    }
}

impl<W: Write> Shared<W> {
    fn out(&self) -> MutexGuard<'_, Out<W>> {
        self.out.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn ledger(&self) -> MutexGuard<'_, Ledger> {
        self.ledger.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn send(&self, request_id: u64, event: Event) {
        let mut out = self.out();
        if out.error.is_some() {
            return;
        }
        let mut written = write_message(&mut out.writer, &Envelope::new(request_id, event));
        if matches!(written, Err(ProtocolError::InvalidLength { .. })) {
            // An oversized reply has written no bytes. Keep the stream usable.
            let reply = refusal(
                ErrorCode::PayloadLimit,
                "The reply exceeds the message limit.",
            );
            written = write_message(&mut out.writer, &Envelope::new(request_id, reply));
        }
        if let Err(error) = written {
            out.error = Some(error);
            // Nobody can hear an open finish any more.
            self.stopped.store(true, Ordering::SeqCst);
        }
    }

    fn stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    fn cancelled(&self, request: Pending) -> bool {
        self.stopped() || self.ledger().is_cancelled(request)
    }

    fn cancel(&self, id: u64, request: Pending) {
        let mut ledger = self.ledger();
        let reply = if ledger.cancel(request) {
            Event::CancelRequested
        } else {
            refusal(ErrorCode::WrongState, "The operation is no longer pending.")
        };
        // Serialize the acknowledgement with the terminal response.
        self.send(id, reply);
        drop(ledger);
    }
}

fn failed(document: u64, refusal: Refusal) -> Event {
    Event::AnalysisFailed {
        document,
        code: refusal.code,
        message: refusal.message,
    }
}

fn refusal(code: ErrorCode, message: impl Into<String>) -> Event {
    Event::Error {
        code,
        message: message.into(),
    }
}

fn serve<W: Write>(
    input: &mut impl Read,
    requests: &SyncSender<Queued>,
    shared: &Shared<W>,
) -> Result<(), ProtocolError> {
    loop {
        let Some(frame) = read_frame(input)? else {
            return Ok(());
        };
        let envelope = match decode::<Command>(&frame) {
            Ok(envelope) => envelope,
            Err(error) => {
                let code = match error {
                    ProtocolError::IncompatibleVersion { .. } => ErrorCode::IncompatibleVersion,
                    _ => ErrorCode::Malformed,
                };
                shared.send(request_id_of(&frame), refusal(code, error.to_string()));
                continue;
            }
        };
        let id = envelope.request_id;
        match envelope.body {
            Command::Hello { .. } => {
                shared.send(
                    id,
                    Event::Hello {
                        engine: build_identity(),
                        capabilities: studio_domain::protocol::Capabilities {
                            analysis_evidence: Some(AnalysisEvidenceIdentity {
                                syntax_schema: SYNTAX_SCHEMA,
                                validation_schema: VALIDATION_SCHEMA,
                                codec_revision: env!("SPLOT_CODEC_REVISION").into(),
                            }),
                            ..capabilities()
                        },
                    },
                );
            }
            Command::AnalysisClearCache => {
                if requests.try_send((id, 0, Request::ClearCache)).is_err() {
                    shared.send(
                        id,
                        refusal(ErrorCode::Busy, "The inspection queue is full."),
                    );
                }
            }
            Command::AnalysisCancel { document } => {
                shared.cancel(id, Pending::Open(document));
            }
            Command::AnalysisCancelValidation {
                document,
                validation,
            } => {
                shared.cancel(
                    id,
                    Pending::Validation {
                        document,
                        validation,
                    },
                );
            }
            Command::AnalysisOpen { document, source } => {
                queue_open(shared, requests, id, document, source, false);
            }
            Command::AnalysisOpenProgressive { document, source } => {
                queue_open(shared, requests, id, document, source, true);
            }
            Command::AnalysisValidate { document, settings } => {
                queue_evidence(
                    shared,
                    requests,
                    id,
                    document,
                    EvidenceRequest::Validate(settings),
                );
            }
            Command::AnalysisIdentityScan { document, grain } => {
                queue_evidence(
                    shared,
                    requests,
                    id,
                    document,
                    EvidenceRequest::Identity(grain),
                );
            }
            Command::AnalysisCancelIdentity { document, scan } => {
                shared.cancel(id, Pending::Identity { document, scan });
            }
            Command::BatchCancel { validation } => {
                shared.cancel(id, Pending::BatchValidation(validation));
            }
            command => {
                if let Some(request) = batch::Request::from_command(&command) {
                    batch::queue(shared, requests, id, request);
                } else if let Some((document, request)) = queued_request(&command) {
                    if requests.try_send((id, document, request)).is_err() {
                        busy(shared, id, document);
                    }
                } else {
                    shared.send(
                        id,
                        refusal(
                            ErrorCode::WrongState,
                            "Use an analysis command with the inspect process.",
                        ),
                    );
                }
            }
        }
    }
}

fn queued_request(command: &Command) -> Option<(u64, Request)> {
    Some(match *command {
        Command::AnalysisPage {
            document,
            filter,
            first,
            count,
        } => (
            document,
            Request::Inspect(Query::Page {
                filter,
                first,
                count,
            }),
        ),
        Command::AnalysisLocate {
            document,
            filter,
            target,
        } => (document, Request::Inspect(Query::Locate { filter, target })),
        Command::AnalysisBytes {
            document,
            offset,
            len,
        } => (document, Request::Inspect(Query::Bytes { offset, len })),
        Command::AnalysisSyntax { document, element } => {
            (document, Request::Inspect(Query::Syntax { element }))
        }
        Command::AnalysisSyntaxPage {
            document,
            trace,
            first,
            count,
        } => (
            document,
            Request::Inspect(Query::SyntaxPage {
                trace,
                first,
                count,
            }),
        ),
        Command::AnalysisFindings {
            document,
            validation,
            first,
            count,
        } => (
            document,
            Request::Inspect(Query::Findings {
                validation,
                first,
                count,
            }),
        ),
        Command::AnalysisIdentityPage {
            document,
            scan,
            first,
            count,
            kind,
        } => (
            document,
            Request::Inspect(Query::IdentityPage {
                scan,
                first,
                count,
                kind,
            }),
        ),
        Command::AnalysisClose { document } => (document, Request::Close),
        _ => return None,
    })
}

/// Queues an open, or refuses it: its id must be new, and the queue must
/// take it.
fn queue_open<W: Write>(
    shared: &Shared<W>,
    requests: &SyncSender<Queued>,
    id: u64,
    document: u64,
    source: WirePath,
    progressive: bool,
) {
    // The ledger is held until the open is recorded, so the work thread
    // cannot answer it first.
    let mut ledger = shared.ledger();
    if !ledger.admits(document) {
        let message = format!(
            "document ids must increase; {document} is not above {}",
            ledger.newest
        );
        drop(ledger);
        shared.send(
            id,
            failed(
                document,
                Refusal::new(AnalysisFailure::InvalidRequest, message),
            ),
        );
        return;
    }
    if requests
        .try_send((
            id,
            document,
            Request::Open {
                source,
                progressive,
            },
        ))
        .is_ok()
    {
        ledger.queue_open(document);
    } else {
        drop(ledger);
        busy(shared, id, document);
    }
}

fn queue_evidence<W: Write>(
    shared: &Shared<W>,
    requests: &SyncSender<Queued>,
    id: u64,
    document: u64,
    request: EvidenceRequest,
) {
    let operation = request.pending(document, id);
    let mut ledger = shared.ledger();
    if ledger.contains(operation) {
        drop(ledger);
        shared.send(
            id,
            failed(
                document,
                Refusal::new(
                    AnalysisFailure::InvalidRequest,
                    "This request ID is already pending.",
                ),
            ),
        );
    } else if requests
        .try_send((id, document, Request::Evidence(request)))
        .is_ok()
    {
        ledger.pending.push((operation, false));
    } else {
        drop(ledger);
        busy(shared, id, document);
    }
}

/// Refuses a request the queue did not take.
fn busy<W: Write>(shared: &Shared<W>, id: u64, document: u64) {
    let message = format!("more than {REQUEST_BACKLOG} requests are waiting");
    shared.send(
        id,
        failed(
            document,
            Refusal::new(AnalysisFailure::LimitReached, message),
        ),
    );
}

// A sustained stream of requests must still leave time for scanning.
const FOREGROUND_BURST: u8 = 8;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

struct Scan {
    request: u64,
    document: u64,
    reported: Instant,
}

fn work<'scope, W: Write + Send>(
    queue: &Receiver<Queued>,
    replies: &'scope SyncSender<Queued>,
    shared: &'scope Shared<W>,
    scope: &'scope thread::Scope<'scope, '_>,
) {
    let mut documents: Vec<(u64, Document)> = Vec::new();
    let mut batch = batch::Archive::default();
    let mut scans = VecDeque::<Scan>::new();
    let mut queued_evidence = VecDeque::<Queued>::new();
    let mut active_evidence = None;
    let mut burst = 0;
    let mut reader_stopped = false;
    loop {
        let ready_evidence = if active_evidence.is_none() {
            queued_evidence.pop_front()
        } else {
            None
        };
        let request = if ready_evidence.is_some() {
            ready_evidence
        } else if scans.is_empty() {
            if reader_stopped && active_evidence.is_none() {
                break;
            }
            queue.recv().ok()
        } else if burst < FOREGROUND_BURST && !shared.stopped() {
            queue.try_recv().ok()
        } else {
            None
        };
        let Some((id, document, request)) = request else {
            let Some(scan) = scans.pop_front() else {
                break;
            };
            if let Some(at) = documents.iter().position(|(id, _)| *id == scan.document) {
                advance_scan(shared, &mut documents, at, &mut scans, scan);
            }
            burst = 0;
            continue;
        };
        if !scans.is_empty() {
            burst += 1;
        }
        let open = documents.iter().position(|(d, _)| *d == document);
        let unknown = || {
            Refusal::new(
                AnalysisFailure::UnknownDocument,
                format!("document {document} is not open"),
            )
        };
        let answer = match request {
            Request::Batch(request) => {
                batch.work(shared, id, request);
                continue;
            }
            Request::Open {
                source,
                progressive,
            } => {
                if progressive {
                    start_scan(shared, &mut documents, &mut scans, id, document, &source);
                } else {
                    open_document(shared, &mut documents, id, document, &source);
                }
                continue;
            }
            Request::Evidence(request) => {
                let operation = request.pending(document, id);
                let result = match open {
                    Some(_)
                        if matches!(request, EvidenceRequest::Validate(_))
                            && scans.iter().any(|scan| scan.document == document) =>
                    {
                        Err(scan_pending())
                    }
                    Some(_) if active_evidence.is_some() => {
                        if queued_evidence.len() < REQUEST_BACKLOG {
                            queued_evidence.push_back((id, document, Request::Evidence(request)));
                            continue;
                        }
                        Err(Refusal::new(
                            AnalysisFailure::LimitReached,
                            "The evidence queue is full.",
                        ))
                    }
                    Some(at) => request.prepare(&mut documents[at].1, id),
                    None => Err(unknown()),
                };
                let answer = match result {
                    Ok(task) => {
                        let job = thread::Builder::new()
                            .name("studio-inspect-evidence".to_owned())
                            .stack_size(JOB_STACK_BYTES)
                            .spawn_scoped(scope, move || {
                                // Always wake the scheduler, even if the codec unwinds.
                                let result = catch_unwind(AssertUnwindSafe(|| {
                                    task.run(&|| shared.cancelled(operation))
                                }))
                                .unwrap_or(EvidenceResult::Aborted(operation));
                                let _ = replies.send((
                                    id,
                                    document,
                                    Request::EvidenceDone(Box::new(result)),
                                ));
                            });
                        match job {
                            Ok(job) => {
                                active_evidence = Some((operation, job));
                                continue;
                            }
                            Err(error) => {
                                if let (Some(at), Pending::Identity { .. }) = (open, operation) {
                                    documents[at].1.drop_identity(id);
                                }
                                failed(
                                    document,
                                    Refusal::new(
                                        AnalysisFailure::Io,
                                        format!("Cannot start the evidence thread. {error}"),
                                    ),
                                )
                            }
                        }
                    }
                    Err(error) => match open {
                        Some(at) => drop_evidence_source(
                            shared,
                            &mut documents,
                            &mut scans,
                            at,
                            document,
                            error,
                        ),
                        None => failed(document, error),
                    },
                };
                settle_evidence(shared, id, operation, answer);
                continue;
            }
            Request::EvidenceDone(result) => {
                let Some((operation, job)) = active_evidence.take() else {
                    continue;
                };
                let _ = job.join();
                let answer = match open {
                    Some(at) => match result.finish(&mut documents[at].1, document, id) {
                        Ok(event) => event,
                        Err(error) => drop_evidence_source(
                            shared,
                            &mut documents,
                            &mut scans,
                            at,
                            document,
                            error,
                        ),
                    },
                    None => failed(document, unknown()),
                };
                settle_evidence(shared, id, operation, answer);
                continue;
            }
            Request::ReaderStopped => {
                reader_stopped = true;
                continue;
            }
            Request::ClearCache if !scans.is_empty() => {
                refusal(ErrorCode::Busy, "Wait for the document scans to finish.")
            }
            Request::ClearCache => match document::clear_cache() {
                Ok(()) => Event::AnalysisCacheCleared,
                Err(error) => refusal(ErrorCode::WrongState, error.message),
            },
            Request::Close => match open {
                Some(at) => {
                    documents.remove(at);
                    cancel_evidence(
                        shared,
                        document,
                        active_evidence.as_ref().map(|(op, _)| *op),
                    );
                    abandon_scan(
                        shared,
                        &mut scans,
                        document,
                        AnalysisFailure::Cancelled,
                        "The document closed during the scan.",
                    );
                    Event::AnalysisClosed { document }
                }
                None => failed(document, unknown()),
            },
            Request::Inspect(query) => match open {
                Some(at) => match serve_document(&mut documents[at].1, document, id, query) {
                    Ok(answer) => answer,
                    Err(refusal) => {
                        if refusal.code == AnalysisFailure::SourceChanged {
                            cancel_evidence(
                                shared,
                                document,
                                active_evidence.as_ref().map(|(op, _)| *op),
                            );
                            abandon_scan(
                                shared,
                                &mut scans,
                                document,
                                refusal.code,
                                &refusal.message,
                            );
                        }
                        dropped(&mut documents, at, document, refusal)
                    }
                },
                None => failed(document, unknown()),
            },
        };
        shared.send(id, answer);
    }
}

fn cancel_evidence<W: Write>(shared: &Shared<W>, document: u64, operation: Option<Pending>) {
    if let Some(op) = operation.filter(|op| op.belongs_to(document)) {
        shared.ledger().cancel(op);
    }
}

fn settle_evidence<W: Write>(shared: &Shared<W>, id: u64, operation: Pending, answer: Event) {
    let mut ledger = shared.ledger();
    ledger.settle(operation);
    shared.send(id, answer);
    drop(ledger);
}

fn serve_document(
    doc: &mut Document,
    document: u64,
    id: u64,
    query: Query,
) -> Result<Event, Refusal> {
    Ok(match query {
        Query::Page {
            filter,
            first,
            count,
        } => {
            let (total, rows) = doc.page(filter, first, count)?;
            Event::AnalysisPage {
                document,
                first,
                total,
                rows,
            }
        }
        Query::Locate { filter, target } => {
            let (row, found) = doc.locate(filter, target)?;
            Event::AnalysisLocated {
                document,
                row,
                found: Box::new(found),
            }
        }
        Query::Bytes { offset, len } => {
            let bytes = doc.bytes(offset, len)?;
            Event::AnalysisBytes {
                document,
                offset,
                bytes,
            }
        }
        Query::Syntax { element } => {
            let (summary, total) = doc.syntax(id, element)?;
            Event::AnalysisSyntaxReady {
                document,
                element,
                summary: Box::new(summary),
                total,
            }
        }
        Query::SyntaxPage {
            trace,
            first,
            count,
        } => {
            let (element, page) = doc.syntax_page(trace, first, count)?;
            Event::AnalysisSyntaxPage {
                document,
                trace,
                element,
                first,
                total: page.total,
                events: page.entries,
            }
        }
        Query::IdentityPage {
            scan,
            first,
            count,
            kind,
        } => Event::AnalysisIdentityPage {
            document,
            scan,
            page: Box::new(doc.identity_page(scan, kind, first, count)?),
        },
        Query::Findings {
            validation,
            first,
            count,
        } => {
            let page = doc.findings(validation, first, count)?;
            Event::AnalysisFindings {
                document,
                validation,
                first,
                total: page.total,
                findings: page.entries,
            }
        }
    })
}

/// The failure of a request on an open document; a changed source closes it.
fn dropped(
    documents: &mut Vec<(u64, Document)>,
    at: usize,
    document: u64,
    refusal: Refusal,
) -> Event {
    if refusal.code == AnalysisFailure::SourceChanged {
        documents.remove(at);
    }
    failed(document, refusal)
}

fn drop_evidence_source<W: Write>(
    shared: &Shared<W>,
    documents: &mut Vec<(u64, Document)>,
    scans: &mut VecDeque<Scan>,
    at: usize,
    document: u64,
    error: Refusal,
) -> Event {
    if error.code == AnalysisFailure::SourceChanged {
        abandon_scan(shared, scans, document, error.code, &error.message);
    }
    dropped(documents, at, document, error)
}

fn scan_pending() -> Refusal {
    Refusal::new(
        AnalysisFailure::InvalidRequest,
        "Wait for the document scan to finish.",
    )
}

fn open_limit() -> Refusal {
    Refusal::new(
        AnalysisFailure::LimitReached,
        format!("Close a document before opening more than {MAX_DOCUMENTS} documents."),
    )
}

fn cancelled_open() -> Refusal {
    Refusal::new(AnalysisFailure::Cancelled, "The open was cancelled.")
}

fn settle_open<W: Write>(shared: &Shared<W>, id: u64, document: u64, answer: Event) {
    let mut ledger = shared.ledger();
    ledger.settle(Pending::Open(document));
    shared.send(id, answer);
    drop(ledger);
}

fn start_scan<W: Write>(
    shared: &Shared<W>,
    documents: &mut Vec<(u64, Document)>,
    scans: &mut VecDeque<Scan>,
    id: u64,
    document: u64,
    source: &WirePath,
) {
    let cancelled = || shared.cancelled(Pending::Open(document));
    let opened = if cancelled() {
        Err(cancelled_open())
    } else if documents.len() >= MAX_DOCUMENTS {
        Err(open_limit())
    } else {
        Document::open_progressive(source, &cancelled)
    };
    match opened {
        Ok(doc) => {
            let summary = doc.summary().clone();
            documents.push((document, doc));
            if summary.outcome == ScanOutcome::Scanning {
                shared.send(id, Event::AnalysisOpenProgress { document, summary });
                scans.push_back(Scan {
                    request: id,
                    document,
                    reported: Instant::now(),
                });
            } else {
                settle_open(
                    shared,
                    id,
                    document,
                    Event::AnalysisOpened { document, summary },
                );
            }
        }
        Err(error) => settle_open(shared, id, document, failed(document, error)),
    }
}

fn advance_scan<W: Write>(
    shared: &Shared<W>,
    documents: &mut Vec<(u64, Document)>,
    at: usize,
    scans: &mut VecDeque<Scan>,
    mut scan: Scan,
) {
    let Scan {
        request, document, ..
    } = scan;
    let doc = &mut documents[at].1;
    match doc.advance(&|| shared.cancelled(Pending::Open(document))) {
        Ok(true) => {
            let summary = doc.summary().clone();
            settle_open(
                shared,
                request,
                document,
                Event::AnalysisOpened { document, summary },
            );
        }
        Ok(false) => {
            if scan.reported.elapsed() >= PROGRESS_INTERVAL {
                shared.send(
                    request,
                    Event::AnalysisOpenProgress {
                        document,
                        summary: doc.summary().clone(),
                    },
                );
                scan.reported = Instant::now();
            }
            scans.push_back(scan);
        }
        Err(error) => {
            documents.remove(at);
            settle_open(shared, request, document, failed(document, error));
        }
    }
}

fn abandon_scan<W: Write>(
    shared: &Shared<W>,
    scans: &mut VecDeque<Scan>,
    document: u64,
    code: AnalysisFailure,
    message: &str,
) {
    if let Some(at) = scans.iter().position(|scan| scan.document == document)
        && let Some(scan) = scans.remove(at)
    {
        settle_open(
            shared,
            scan.request,
            document,
            failed(document, Refusal::new(code, message)),
        );
    }
}

fn open_document<W: Write>(
    shared: &Shared<W>,
    documents: &mut Vec<(u64, Document)>,
    id: u64,
    document: u64,
    source: &WirePath,
) {
    let operation = Pending::Open(document);
    let opened = if shared.cancelled(operation) {
        Err(cancelled_open())
    } else if documents.len() >= MAX_DOCUMENTS {
        Err(open_limit())
    } else {
        Document::open(source, &|| shared.cancelled(operation))
    };
    let answer = match opened {
        Ok(opened) => {
            let summary = opened.summary().clone();
            documents.push((document, opened));
            Event::AnalysisOpened { document, summary }
        }
        Err(error) => failed(document, error),
    };
    settle_open(shared, id, document, answer);
}

#[cfg(test)]
mod tests {
    use super::{Ledger, Pending};

    /// Queues `document` as the reader does when the queue takes it.
    fn queued(ledger: &mut Ledger, document: u64) -> bool {
        let admitted = ledger.admits(document);
        if admitted {
            ledger.queue_open(document);
        }
        admitted
    }

    #[test]
    fn a_cancel_counts_only_while_its_open_is_pending() {
        let mut ledger = Ledger::default();
        // Cancelled before it starts, then answered.
        assert!(queued(&mut ledger, 1));
        assert!(ledger.cancel(Pending::Open(1)));
        assert!(ledger.is_cancelled(Pending::Open(1)));
        ledger.settle(Pending::Open(1));
        // After the answer, a cancel is refused.
        assert!(!ledger.cancel(Pending::Open(1)) && !ledger.is_cancelled(Pending::Open(1)));
        // Answered first: not cancelled, and a later cancel is refused.
        assert!(queued(&mut ledger, 2));
        assert!(!ledger.is_cancelled(Pending::Open(2)));
        ledger.settle(Pending::Open(2));
        assert!(!ledger.cancel(Pending::Open(2)));
        // Cancelling one pending open leaves the other alone.
        assert!(queued(&mut ledger, 3) && queued(&mut ledger, 4));
        assert!(ledger.cancel(Pending::Open(4)));
        assert!(!ledger.is_cancelled(Pending::Open(3)) && ledger.is_cancelled(Pending::Open(4)));
        // Ids only increase; an id never sent is not pending.
        assert!(!ledger.admits(4) && !ledger.admits(2));
        assert!(!ledger.cancel(Pending::Open(9)));
    }

    /// An open the full queue refused is not recorded: its id can be sent
    /// again and it cannot be cancelled.
    #[test]
    fn a_refused_open_keeps_its_id_free() {
        let mut ledger = Ledger::default();
        assert!(queued(&mut ledger, 1));
        // The queue refuses document 5: the reader checks and records nothing.
        assert!(ledger.admits(5));
        assert!(!ledger.cancel(Pending::Open(5)));
        assert!(queued(&mut ledger, 5));
        assert!(ledger.cancel(Pending::Open(5)));
    }
}
