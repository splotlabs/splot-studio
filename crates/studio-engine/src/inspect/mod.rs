// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The inspect process: read-only analysis [`document`]s of local AV2 IVF
//! and Annex B files, over the same framed protocol as the worker.
//!
//! Two threads. The caller's thread reads commands and never touches a
//! file: it answers `Hello`, refuses what this process does not serve,
//! decides `AnalysisCancel`, and queues analysis requests for the work
//! thread (at most [`REQUEST_BACKLOG`]; a request beyond that is refused, so
//! a cancel is always read). The work thread serves the requests in order,
//! with at most [`MAX_DOCUMENTS`] documents open. Both write their replies
//! under one lock, each frame whole. The process ends at the end of its
//! input, which cancels every open not yet answered.
//!
//! The [`Ledger`] holds the opens that are queued or running and not yet
//! answered, and which of them are cancelled. A cancel is accepted only for
//! such an open, and the open's answer is decided from the same ledger under
//! the same lock: after `CancelRequested`, that open is answered
//! [`AnalysisFailure::Cancelled`], whatever else happened to it, and a
//! cancel that comes after the answer is refused. Cancelling one document
//! never touches another.

pub mod document;

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;

use studio_domain::analysis::{AnalysisFailure, LocateTarget, RowFilter};
use studio_domain::job::WirePath;
use studio_domain::protocol::{
    Command, Envelope, ErrorCode, Event, ProtocolError, decode, read_frame, write_message,
};

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
    Open {
        source: WirePath,
    },
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
    Close,
}

/// A queued request: its `request_id`, its document and what it asks.
type Queued = (u64, u64, Request);

struct Shared<W> {
    out: Mutex<Out<W>>,
    ledger: Mutex<Ledger>,
    /// The input ended or the output failed: every open is cancelled.
    stopped: AtomicBool,
}

struct Out<W> {
    writer: W,
    /// The output failed; everything after is discarded.
    error: Option<ProtocolError>,
}

/// The opens not yet answered.
#[derive(Debug, Default)]
struct Ledger {
    /// The newest document id an open was queued for.
    newest: u64,
    /// Opens queued or running and not yet answered: at most the queue's
    /// capacity and the one running.
    pending: Vec<u64>,
    /// Those of them that were cancelled.
    cancelled: Vec<u64>,
}

impl Ledger {
    /// Whether `document` is newer than every open queued before.
    fn admits(&self, document: u64) -> bool {
        document > self.newest
    }

    /// Records an open that [`Self::admits`] and the queue took. An open the
    /// queue refused is not recorded, so its id stays free for a retry.
    fn queue(&mut self, document: u64) {
        self.newest = document;
        self.pending.push(document);
    }

    /// Cancels `document`'s open; `false` when it is not pending.
    fn cancel(&mut self, document: u64) -> bool {
        if !self.pending.contains(&document) {
            return false;
        }
        if !self.cancelled.contains(&document) {
            self.cancelled.push(document);
        }
        true
    }

    fn is_cancelled(&self, document: u64) -> bool {
        self.cancelled.contains(&document)
    }

    /// Marks `document`'s open answered; `true` when it was cancelled.
    fn settle(&mut self, document: u64) -> bool {
        self.pending.retain(|d| *d != document);
        let cancelled = self.is_cancelled(document);
        self.cancelled.retain(|d| *d != document);
        cancelled
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
        if let Err(error) = write_message(&mut out.writer, &Envelope::new(request_id, event)) {
            out.error = Some(error);
            // Nobody can hear an open finish any more.
            self.stopped.store(true, Ordering::SeqCst);
        }
    }

    fn stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    fn cancelled(&self, document: u64) -> bool {
        self.stopped() || self.ledger().is_cancelled(document)
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
        let (document, request) = match envelope.body {
            Command::Hello { .. } => {
                shared.send(
                    id,
                    Event::Hello {
                        engine: build_identity(),
                        capabilities: capabilities(),
                    },
                );
                continue;
            }
            Command::AnalysisCancel { document } => {
                let mut ledger = shared.ledger();
                let reply = if ledger.cancel(document) {
                    Event::CancelRequested
                } else {
                    refusal(
                        ErrorCode::WrongState,
                        format!("document {document} is not being opened"),
                    )
                };
                // Under the ledger, so the open's answer cannot overtake it.
                shared.send(id, reply);
                drop(ledger);
                continue;
            }
            Command::Probe { .. }
            | Command::Start { .. }
            | Command::Cancel
            | Command::Picture { .. }
            | Command::Play { .. }
            | Command::StopPlayback => {
                shared.send(
                    id,
                    refusal(
                        ErrorCode::WrongState,
                        "the inspect process serves only analysis commands; \
                         analysis_cancel names the document to cancel",
                    ),
                );
                continue;
            }
            Command::AnalysisOpen { document, source } => {
                queue_open(shared, requests, id, document, source);
                continue;
            }
            Command::AnalysisPage {
                document,
                filter,
                first,
                count,
            } => (
                document,
                Request::Page {
                    filter,
                    first,
                    count,
                },
            ),
            Command::AnalysisLocate {
                document,
                filter,
                target,
            } => (document, Request::Locate { filter, target }),
            Command::AnalysisBytes {
                document,
                offset,
                len,
            } => (document, Request::Bytes { offset, len }),
            Command::AnalysisClose { document } => (document, Request::Close),
        };
        if requests.try_send((id, document, request)).is_err() {
            busy(shared, id, document);
        }
    }
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
        ledger.queue(document);
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
        let answer = match (request, open) {
            (Request::Open { source }, _) => {
                open_document(shared, &mut documents, id, document, &source);
                continue;
            }
            (_, None) => failed(document, unknown()),
            (
                Request::Page {
                    filter,
                    first,
                    count,
                },
                Some(at),
            ) => match documents[at].1.page(filter, first, count) {
                Ok((total, rows)) => Event::AnalysisPage {
                    document,
                    first,
                    total,
                    rows,
                },
                Err(refusal) => dropped(&mut documents, at, document, refusal),
            },
            (Request::Locate { filter, target }, Some(at)) => {
                match documents[at].1.locate(filter, target) {
                    Ok((row, found)) => Event::AnalysisLocated {
                        document,
                        row,
                        found: Box::new(found),
                    },
                    Err(refusal) => dropped(&mut documents, at, document, refusal),
                }
            }
            (Request::Bytes { offset, len }, Some(at)) => {
                match documents[at].1.bytes(offset, len) {
                    Ok(bytes) => Event::AnalysisBytes {
                        document,
                        offset,
                        bytes,
                    },
                    Err(refusal) => dropped(&mut documents, at, document, refusal),
                }
            }
            (Request::Close, Some(at)) => {
                documents.remove(at);
                Event::AnalysisClosed { document }
            }
        };
        shared.send(id, answer);
    }
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
    let opened = if shared.cancelled(document) {
        // Not started: its answer is decided below.
        None
    } else if documents.len() >= MAX_DOCUMENTS {
        Some(Err(Refusal::new(
            AnalysisFailure::LimitReached,
            format!("{MAX_DOCUMENTS} documents are open; close one first"),
        )))
    } else {
        Some(Document::open(source, &|| shared.cancelled(document)))
    };
    let mut ledger = shared.ledger();
    let cancelled = ledger.settle(document) || shared.stopped();
    let answer = match opened {
        _ if cancelled => failed(
            document,
            Refusal::new(AnalysisFailure::Cancelled, "the open was cancelled"),
        ),
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
    use super::Ledger;

    /// Queues `document` as the reader does when the queue takes it.
    fn queued(ledger: &mut Ledger, document: u64) -> bool {
        let admitted = ledger.admits(document);
        if admitted {
            ledger.queue(document);
        }
        admitted
    }

    #[test]
    fn a_cancel_counts_only_while_its_open_is_pending() {
        let mut ledger = Ledger::default();
        // Cancelled before it starts, then answered as cancelled.
        assert!(queued(&mut ledger, 1));
        assert!(ledger.cancel(1));
        assert!(ledger.settle(1));
        // After the answer, a cancel is refused.
        assert!(!ledger.cancel(1));
        // Answered first: not cancelled, and a later cancel is refused.
        assert!(queued(&mut ledger, 2));
        assert!(!ledger.settle(2));
        assert!(!ledger.cancel(2));
        // Cancelling one pending open leaves the other alone.
        assert!(queued(&mut ledger, 3) && queued(&mut ledger, 4));
        assert!(ledger.cancel(4));
        assert!(!ledger.settle(3));
        assert!(ledger.settle(4));
        // Ids only increase; an id never sent is not pending.
        assert!(!ledger.admits(4) && !ledger.admits(2));
        assert!(!ledger.cancel(9));
    }

    /// An open the full queue refused is not recorded: its id can be sent
    /// again and it cannot be cancelled.
    #[test]
    fn a_refused_open_keeps_its_id_free() {
        let mut ledger = Ledger::default();
        assert!(queued(&mut ledger, 1));
        // The queue refuses document 5: the reader checks and records nothing.
        assert!(ledger.admits(5));
        assert!(!ledger.cancel(5));
        assert!(queued(&mut ledger, 5));
        assert!(ledger.cancel(5));
    }
}
