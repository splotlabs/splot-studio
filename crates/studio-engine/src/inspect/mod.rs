// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The inspect process: read-only analysis [`document`]s of local AV2 IVF
//! and Annex B files, over the same framed protocol as the worker.
//!
//! Two threads. The caller's thread reads commands and never touches a
//! file: it answers `Hello`, refuses what this process does not serve,
//! handles cancellation, and queues analysis requests for the work
//! thread (at most [`REQUEST_BACKLOG`]; a request beyond that is refused, so
//! a cancel is always read). The work thread serves the requests in order,
//! with at most [`MAX_DOCUMENTS`] documents open. Both write their replies
//! under one lock, each frame whole. The process ends at the end of its
//! input, which cancels pending open and validation requests.
//!
//! The [`Ledger`] records open and validation requests until the work thread
//! answers them. The ledger serializes cancellation and completion. A request
//! that already has an answer cannot be cancelled. A cancelled open stops at
//! the next unit. Its answer keeps the earlier data as a document
//! whose outcome is stopped by [`ProblemKind::Cancelled`] (or, when the read
//! finished first, with the whole document). Only an open cancelled before
//! its framing was known is answered [`AnalysisFailure::Cancelled`].
//! Validation also keeps completed evidence when cancelled. A cancel targets
//! the document generation and the request ID of the pending validation.

pub mod document;
mod syntax;
mod validation;

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;

#[cfg(doc)]
use studio_domain::analysis::ProblemKind;
use studio_domain::analysis::{AnalysisFailure, LocateTarget, RowFilter};
use studio_domain::job::WirePath;
use studio_domain::protocol::{
    AnalysisEvidenceIdentity, Command, Envelope, ErrorCode, Event, ProtocolError, decode,
    read_frame, write_message,
};
use studio_domain::syntax::SYNTAX_SCHEMA;
use studio_domain::validation::{VALIDATION_SCHEMA, ValidationSettings};

use self::document::{Document, Refusal};
use crate::job::JOB_STACK_BYTES;
use crate::worker::{SessionError, request_id_of};
use crate::{build_identity, capabilities};

/// Documents open at once; another open is refused until one is closed.
pub const MAX_DOCUMENTS: usize = 4;

/// Analysis requests waiting for the work thread.
pub const REQUEST_BACKLOG: usize = 64;

/// Serves the inspect protocol on `input` and `output` until the input ends.
/// Every frame gets exactly one reply, with its `request_id`.
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
        let work = thread::Builder::new()
            .name("studio-inspect".to_owned())
            .stack_size(JOB_STACK_BYTES)
            .spawn_scoped(scope, move || work(&queue, shared));
        let read = match work {
            Ok(work) => {
                let read = serve(input, &requests, shared);
                // Ends the work thread once the queue is empty.
                drop(requests);
                // Nobody can cancel later.
                shared.stopped.store(true, Ordering::SeqCst);
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
    Open { source: WirePath },
    Validate { settings: ValidationSettings },
    Inspect(Query),
    Close,
}

/// Reads the selected document on the work thread.
#[derive(Debug)]
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
}

/// A queued request: its `request_id`, its document and what it asks.
type Queued = (u64, u64, Request);

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
    Validation { document: u64, validation: u64 },
}

/// Long operations that the reader can cancel.
#[derive(Debug, Default)]
struct Ledger {
    /// The newest document id an open was queued for.
    newest: u64,
    /// At most the queue capacity and the one request running.
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
                queue_open(shared, requests, id, document, source);
            }
            Command::AnalysisValidate { document, settings } => {
                queue_validation(shared, requests, id, document, settings);
            }
            command => {
                if let Some((document, request)) = queued_request(command) {
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

fn queued_request(command: Command) -> Option<(u64, Request)> {
    Some(match command {
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
        .try_send((id, document, Request::Open { source }))
        .is_ok()
    {
        ledger.queue_open(document);
    } else {
        drop(ledger);
        busy(shared, id, document);
    }
}

fn queue_validation<W: Write>(
    shared: &Shared<W>,
    requests: &SyncSender<Queued>,
    id: u64,
    document: u64,
    settings: ValidationSettings,
) {
    let operation = Pending::Validation {
        document,
        validation: id,
    };
    let mut ledger = shared.ledger();
    if ledger.contains(operation) {
        drop(ledger);
        shared.send(
            id,
            failed(
                document,
                Refusal::new(
                    AnalysisFailure::InvalidRequest,
                    "This validation request ID is already pending.",
                ),
            ),
        );
    } else if requests
        .try_send((id, document, Request::Validate { settings }))
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

fn work<W: Write>(queue: &Receiver<Queued>, shared: &Shared<W>) {
    let mut documents: Vec<(u64, Document)> = Vec::new();
    while let Ok((id, document, request)) = queue.recv() {
        let open = documents.iter().position(|(d, _)| *d == document);
        let unknown = || {
            Refusal::new(
                AnalysisFailure::UnknownDocument,
                format!("document {document} is not open"),
            )
        };
        let answer = match request {
            Request::Open { source } => {
                open_document(shared, &mut documents, id, document, &source);
                continue;
            }
            Request::Validate { settings } => {
                let operation = Pending::Validation {
                    document,
                    validation: id,
                };
                let answer = match open {
                    Some(at) => match documents[at]
                        .1
                        .validate(id, &settings, &|| shared.cancelled(operation))
                    {
                        Ok((summary, total)) => Event::AnalysisValidated {
                            document,
                            summary: Box::new(summary),
                            total,
                        },
                        Err(refusal) => dropped(&mut documents, at, document, refusal),
                    },
                    None => failed(document, unknown()),
                };
                let mut ledger = shared.ledger();
                ledger.settle(operation);
                shared.send(id, answer);
                drop(ledger);
                continue;
            }
            Request::Close => match open {
                Some(at) => {
                    documents.remove(at);
                    Event::AnalysisClosed { document }
                }
                None => failed(document, unknown()),
            },
            Request::Inspect(query) => match open {
                Some(at) => match serve_document(&mut documents[at].1, document, id, query) {
                    Ok(answer) => answer,
                    Err(refusal) => dropped(&mut documents, at, document, refusal),
                },
                None => failed(document, unknown()),
            },
        };
        shared.send(id, answer);
    }
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

fn open_document<W: Write>(
    shared: &Shared<W>,
    documents: &mut Vec<(u64, Document)>,
    id: u64,
    document: u64,
    source: &WirePath,
) {
    let operation = Pending::Open(document);
    let opened = if shared.cancelled(operation) {
        // Not started: its answer is decided below.
        None
    } else if documents.len() >= MAX_DOCUMENTS {
        Some(Err(Refusal::new(
            AnalysisFailure::LimitReached,
            format!("{MAX_DOCUMENTS} documents are open; close one first"),
        )))
    } else {
        Some(Document::open(source, &|| shared.cancelled(operation)))
    };
    let mut ledger = shared.ledger();
    ledger.settle(operation);
    let answer = match opened {
        // A cancelled read keeps what it read: the outcome says it stopped.
        Some(Ok(opened)) => {
            let summary = opened.summary().clone();
            documents.push((document, opened));
            Event::AnalysisOpened { document, summary }
        }
        Some(Err(refusal)) => failed(document, refusal),
        None => failed(
            document,
            Refusal::new(AnalysisFailure::Cancelled, "the open was cancelled"),
        ),
    };
    // Sent under the ledger, so a cancel decided after this one is refused
    // and one decided before it was honoured above.
    shared.send(id, answer);
    drop(ledger);
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
