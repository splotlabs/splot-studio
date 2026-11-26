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
mod layers;
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
use studio_domain::layers::LayerDecodeSelection;
use studio_domain::preview::GrainPolicy;
use studio_domain::protocol::{
    AnalysisEvidenceIdentity, Command, Envelope, ErrorCode, Event, ProtocolError, decode,
    read_frame, write_message,
};
use studio_domain::syntax::SYNTAX_SCHEMA;
use studio_domain::timeline::{TimelineTarget, TimelineView};
use studio_domain::validation::{VALIDATION_SCHEMA, ValidationEvidence, ValidationSettings};

use self::document::{Document, OpeningDocument, Refusal};
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
    EvidenceDone(EvidenceResult),
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
    Timeline {
        scan: u64,
        query: TimelineQuery,
    },
    LayerPage {
        inspection: u64,
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

#[derive(Debug, Clone, Copy)]
enum TimelineQuery {
    Range {
        view: TimelineView,
        first: u64,
        limit: u64,
        buckets: u32,
    },
    Locate(TimelineTarget),
}

/// A queued request: its `request_id`, its document and what it asks.
type Queued = (u64, u64, Request);
type ValidationResult = Result<(ValidationEvidence, SourceDigest), Refusal>;

#[derive(Debug)]
enum EvidenceRequest {
    Validate(ValidationSettings),
    Identity {
        grain: GrainPolicy,
        selection: LayerDecodeSelection,
    },
    Layers {
        element: u64,
        selection: LayerDecodeSelection,
    },
}

impl EvidenceRequest {
    fn pending(&self, document: u64, id: u64) -> Pending {
        match self {
            Self::Validate(_) => Pending::Validation {
                document,
                validation: id,
            },
            Self::Identity { .. } => Pending::Identity { document, scan: id },
            Self::Layers { .. } => Pending::Layers {
                document,
                inspection: id,
            },
        }
    }

    fn prepare(self, doc: &mut Document, id: u64) -> Result<EvidenceTask, Refusal> {
        Ok(match self {
            Self::Validate(settings) => {
                EvidenceTask::Validate(doc.prepare_validation(id)?, settings)
            }
            Self::Layers { element, selection } => {
                let (file, query) = doc.prepare_layers(id, element, selection)?;
                EvidenceTask::Layers(file, query)
            }
            Self::Identity { grain, selection } => {
                let file = doc.open_evidence_source()?;
                let index = doc.begin_identity(id, grain, selection)?;
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
    Layers(File, layers::Query),
    Identity {
        file: File,
        index: identity::IdentityIndex,
        digest: Option<Box<SourceDigest>>,
    },
}

impl EvidenceTask {
    fn run(self, cancelled: &dyn Fn() -> bool) -> EvidenceResult {
        match self {
            Self::Validate(mut file, settings) => EvidenceResult::Validation(Box::new(
                validation::validate(&mut file, &settings, cancelled),
            )),
            Self::Layers(file, query) => {
                EvidenceResult::Layers(Box::new(layers::inspect(file, query, cancelled)))
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
    Validation(Box<ValidationResult>),
    Identity(Result<(), Refusal>),
    Layers(Box<Result<layers::Report, Refusal>>),
    Aborted(Pending),
}

impl EvidenceResult {
    fn finish(self, doc: &mut Document, document: u64, id: u64) -> Result<Event, Refusal> {
        match self {
            Self::Validation(result) => {
                let (summary, total) = doc.complete_validation(id, *result)?;
                Ok(Event::AnalysisValidated {
                    document,
                    summary: Box::new(summary),
                    total,
                })
            }
            Self::Layers(result) => {
                let (evidence, total) = doc.complete_layers(id, *result)?;
                Ok(Event::AnalysisLayersReady {
                    document,
                    evidence: Box::new(evidence),
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
    Layers { document: u64, inspection: u64 },
}

impl Pending {
    fn belongs_to(self, member: u64) -> bool {
        match self {
            Self::Validation { document, .. }
            | Self::Identity { document, .. }
            | Self::Layers { document, .. } => document == member,
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
        route(shared, requests, envelope.request_id, envelope.body);
    }
}

fn route<W: Write>(shared: &Shared<W>, requests: &SyncSender<Queued>, id: u64, command: Command) {
    match command {
        Command::Hello { .. } => shared.send(id, analysis_hello()),
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
        Command::AnalysisClose { document } => {
            let mut ledger = shared.ledger();
            if requests.try_send((id, document, Request::Close)).is_ok() {
                // Let the active read yield before the scheduler handles the close.
                ledger.cancel(Pending::Open(document));
            } else {
                drop(ledger);
                busy(shared, id, document);
            }
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
        Command::AnalysisIdentityScan {
            document,
            grain,
            selection,
        } => {
            queue_evidence(
                shared,
                requests,
                id,
                document,
                EvidenceRequest::Identity { grain, selection },
            );
        }
        Command::AnalysisLayers {
            document,
            element,
            selection,
        } => {
            queue_evidence(
                shared,
                requests,
                id,
                document,
                EvidenceRequest::Layers { element, selection },
            );
        }
        Command::AnalysisCancelLayers {
            document,
            inspection,
        } => {
            shared.cancel(
                id,
                Pending::Layers {
                    document,
                    inspection,
                },
            );
        }
        Command::AnalysisCancelIdentity { document, scan } => {
            shared.cancel(id, Pending::Identity { document, scan });
        }
        Command::BatchCancel { validation } => {
            shared.cancel(id, Pending::BatchValidation(validation));
        }
        command => queue_query(shared, requests, id, &command),
    }
}

fn analysis_hello() -> Event {
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
    }
}

fn queue_query<W: Write>(
    shared: &Shared<W>,
    requests: &SyncSender<Queued>,
    id: u64,
    command: &Command,
) {
    if let Some(request) = batch::Request::from_command(command) {
        batch::queue(shared, requests, id, request);
    } else if let Some((document, request)) = queued_request(command) {
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
        Command::AnalysisLayerPage {
            document,
            inspection,
            first,
            count,
        } => (
            document,
            Request::Inspect(Query::LayerPage {
                inspection,
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
        _ => return queued_timeline_request(command),
    })
}

fn queued_timeline_request(command: &Command) -> Option<(u64, Request)> {
    let (document, scan, query) = match *command {
        Command::AnalysisTimeline {
            document,
            scan,
            view,
            first,
            limit,
            buckets,
        } => (
            document,
            scan,
            TimelineQuery::Range {
                view,
                first,
                limit,
                buckets,
            },
        ),
        Command::AnalysisTimelineLocate {
            document,
            scan,
            target,
        } => (document, scan, TimelineQuery::Locate(target)),
        _ => return None,
    };
    Some((document, Request::Inspect(Query::Timeline { scan, query })))
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
    let mut scheduler = Scheduler {
        shared,
        scope,
        replies,
        documents: Vec::new(),
        batch: batch::Archive::default(),
        scans: VecDeque::new(),
        waiting: VecDeque::new(),
        active: None,
    };
    let mut burst = 0;
    let mut reader_stopped = false;
    loop {
        let ready = if scheduler.active.is_none() {
            scheduler.waiting.pop_front()
        } else {
            None
        };
        let request = if ready.is_some() {
            ready
        } else if scheduler.scans.is_empty() {
            if reader_stopped && scheduler.active.is_none() {
                break;
            }
            queue.recv().ok()
        } else if burst < FOREGROUND_BURST && !shared.stopped() {
            queue.try_recv().ok()
        } else {
            None
        };
        match request {
            Some((_, _, Request::ReaderStopped)) => reader_stopped = true,
            Some(request) => {
                if !scheduler.scans.is_empty() {
                    burst += 1;
                }
                scheduler.handle(request);
            }
            None => {
                let Some(scan) = scheduler.scans.pop_front() else {
                    break;
                };
                scheduler.advance_scan(scan);
                burst = 0;
            }
        }
    }
}

struct Scheduler<'scope, 'env, W> {
    shared: &'scope Shared<W>,
    scope: &'scope thread::Scope<'scope, 'env>,
    replies: &'scope SyncSender<Queued>,
    documents: Vec<(u64, OpeningDocument<'scope>)>,
    batch: batch::Archive,
    scans: VecDeque<Scan>,
    waiting: VecDeque<Queued>,
    active: Option<(Pending, thread::ScopedJoinHandle<'scope, ()>)>,
}

impl<W: Write + Send> Scheduler<'_, '_, W> {
    fn document_index(&self, document: u64) -> Option<usize> {
        self.documents.iter().position(|(id, _)| *id == document)
    }

    fn handle(&mut self, (id, document, request): Queued) {
        let shared = self.shared;
        let answer = match request {
            Request::Batch(request) => {
                self.batch.work(shared, id, request);
                return;
            }
            Request::Open {
                source,
                progressive,
            } => {
                self.open_document(id, document, &source, progressive);
                return;
            }
            Request::Evidence(request) => {
                let operation = request.pending(document, id);
                if let Err(error) = self.begin_evidence(id, document, request) {
                    let answer = self.fail_document(document, error);
                    settle_evidence(shared, id, operation, answer);
                }
                return;
            }
            Request::EvidenceDone(result) => {
                self.finish_evidence(id, document, result);
                return;
            }
            Request::ReaderStopped => return,
            Request::ClearCache if !self.scans.is_empty() => {
                refusal(ErrorCode::Busy, "Wait for the document scans to finish.")
            }
            Request::ClearCache => match document::clear_cache() {
                Ok(()) => Event::AnalysisCacheCleared,
                Err(error) => refusal(ErrorCode::WrongState, error.message),
            },
            Request::Close => {
                if let Some(at) = self.document_index(document) {
                    self.remove_document(
                        at,
                        document,
                        &Refusal::new(
                            AnalysisFailure::Cancelled,
                            "The document closed during the scan.",
                        ),
                    );
                    Event::AnalysisClosed { document }
                } else {
                    failed(document, unknown_document(document))
                }
            }
            Request::Inspect(query) => {
                let result = self
                    .document_index(document)
                    .ok_or_else(|| unknown_document(document))
                    .and_then(|at| {
                        serve_document(self.documents[at].1.document_mut(), document, id, query)
                    });
                result.unwrap_or_else(|error| self.fail_document(document, error))
            }
        };
        shared.send(id, answer);
    }

    fn open_document(&mut self, id: u64, document: u64, source: &WirePath, progressive: bool) {
        let shared = self.shared;
        let cancelled = move || shared.cancelled(Pending::Open(document));
        let opened = if cancelled() {
            Err(cancelled_open())
        } else if self.documents.len() >= MAX_DOCUMENTS {
            Err(open_limit())
        } else if progressive {
            Document::open_progressive(source, cancelled)
        } else {
            Document::open_complete(source, cancelled)
        };
        let answer = match opened {
            Ok(doc) => {
                let summary = doc.summary().clone();
                self.documents.push((document, doc));
                if summary.outcome == ScanOutcome::Scanning {
                    self.shared
                        .send(id, Event::AnalysisOpenProgress { document, summary });
                    self.scans.push_back(Scan {
                        request: id,
                        document,
                        reported: Instant::now(),
                    });
                    return;
                }
                Event::AnalysisOpened { document, summary }
            }
            Err(error) => failed(document, error),
        };
        settle_open(self.shared, id, document, answer);
    }

    fn advance_scan(&mut self, mut scan: Scan) {
        let Scan {
            request, document, ..
        } = scan;
        let Some(at) = self.document_index(document) else {
            settle_open(
                self.shared,
                request,
                document,
                failed(document, unknown_document(document)),
            );
            return;
        };
        let shared = self.shared;
        let doc = &mut self.documents[at].1;
        match doc.advance() {
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
                self.scans.push_back(scan);
            }
            Err(error) => {
                self.remove_document(at, document, &error);
                settle_open(shared, request, document, failed(document, error));
            }
        }
    }

    fn begin_evidence(
        &mut self,
        id: u64,
        document: u64,
        request: EvidenceRequest,
    ) -> Result<(), Refusal> {
        let at = self
            .document_index(document)
            .ok_or_else(|| unknown_document(document))?;
        if matches!(request, EvidenceRequest::Validate(_))
            && self.scans.iter().any(|scan| scan.document == document)
        {
            return Err(scan_pending());
        }
        if self.active.is_some() {
            if self.waiting.len() >= REQUEST_BACKLOG {
                return Err(Refusal::new(
                    AnalysisFailure::LimitReached,
                    "The evidence queue is full.",
                ));
            }
            self.waiting
                .push_back((id, document, Request::Evidence(request)));
            return Ok(());
        }

        let operation = request.pending(document, id);
        let task = request.prepare(self.documents[at].1.document_mut(), id)?;
        let shared = self.shared;
        let replies = self.replies;
        let job = thread::Builder::new()
            .name("studio-inspect-evidence".to_owned())
            .stack_size(JOB_STACK_BYTES)
            .spawn_scoped(self.scope, move || {
                // Always wake the scheduler, even if the codec unwinds.
                let result = catch_unwind(AssertUnwindSafe(|| {
                    task.run(&|| shared.cancelled(operation))
                }))
                .unwrap_or(EvidenceResult::Aborted(operation));
                let _ = replies.send((id, document, Request::EvidenceDone(result)));
            })
            .map_err(|error| {
                if matches!(operation, Pending::Identity { .. }) {
                    self.documents[at].1.document_mut().drop_identity(id);
                }
                Refusal::new(
                    AnalysisFailure::Io,
                    format!("Cannot start the evidence thread. {error}"),
                )
            })?;
        self.active = Some((operation, job));
        Ok(())
    }

    fn finish_evidence(&mut self, id: u64, document: u64, result: EvidenceResult) {
        let Some((operation, job)) = self.active.take() else {
            return;
        };
        let _ = job.join();
        let answer = self
            .document_index(document)
            .ok_or_else(|| unknown_document(document))
            .and_then(|at| result.finish(self.documents[at].1.document_mut(), document, id))
            .unwrap_or_else(|error| self.fail_document(document, error));
        settle_evidence(self.shared, id, operation, answer);
    }

    fn fail_document(&mut self, document: u64, error: Refusal) -> Event {
        if error.code == AnalysisFailure::SourceChanged
            && let Some(at) = self.document_index(document)
        {
            self.remove_document(at, document, &error);
        }
        failed(document, error)
    }

    fn remove_document(&mut self, at: usize, document: u64, reason: &Refusal) {
        self.documents.remove(at);
        if let Some((operation, _)) = &self.active
            && operation.belongs_to(document)
        {
            self.shared.ledger().cancel(*operation);
        }
        abandon_scan(
            self.shared,
            &mut self.scans,
            document,
            reason.code,
            &reason.message,
        );
    }
}

fn unknown_document(document: u64) -> Refusal {
    Refusal::new(
        AnalysisFailure::UnknownDocument,
        format!("Document {document} is not open."),
    )
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
        Query::Timeline { scan, query } => serve_timeline(doc, document, scan, query)?,
        Query::LayerPage {
            inspection,
            first,
            count,
        } => {
            let page = doc.layer_page(inspection, first, count)?;
            Event::AnalysisLayerPage {
                document,
                inspection,
                first,
                total: page.total,
                rows: page.entries,
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

fn serve_timeline(
    doc: &Document,
    document: u64,
    scan: u64,
    query: TimelineQuery,
) -> Result<Event, Refusal> {
    Ok(match query {
        TimelineQuery::Range {
            view,
            first,
            limit,
            buckets,
        } => Event::AnalysisTimeline {
            document,
            scan,
            page: Box::new(doc.timeline(scan, view, first, limit, buckets)?),
        },
        TimelineQuery::Locate(target) => Event::AnalysisTimelineLocated {
            document,
            scan,
            selection: Box::new(doc.timeline_locate(scan, target)?),
        },
    })
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

#[cfg(test)]
mod tests {
    use super::{Command, Ledger, Out, Pending, Request, Shared, route};
    use std::sync::{Mutex, mpsc};

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

    #[test]
    fn a_queued_close_cancels_only_its_inflight_read() {
        let shared = Shared {
            out: Mutex::new(Out {
                writer: Vec::new(),
                error: None,
            }),
            ledger: Mutex::new(Ledger::default()),
            stopped: false.into(),
        };
        shared.ledger().queue_open(1);
        shared.ledger().queue_open(2);
        let (tx, rx) = mpsc::sync_channel(1);
        route(&shared, &tx, 3, Command::AnalysisClose { document: 1 });
        // The scheduler has not yet received the close.
        assert!(shared.cancelled(Pending::Open(1)));
        assert!(!shared.cancelled(Pending::Open(2)));
        route(&shared, &tx, 4, Command::AnalysisClose { document: 2 });
        assert!(!shared.cancelled(Pending::Open(2)));
        assert!(matches!(rx.try_recv(), Ok((3, 1, Request::Close))));
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
