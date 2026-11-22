// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The inspect process: read-only analysis [`document`]s of local AV2 IVF
//! and Annex B files, over the same framed protocol as the worker.
//!
//! Two threads. The caller's thread reads commands and never touches a
//! file: it answers `Hello`, refuses what this process does not serve,
//! acknowledges `Cancel`, and queues analysis requests for the work thread
//! (at most [`REQUEST_BACKLOG`]; a request beyond that is refused, so a
//! `Cancel` is always read). The work thread serves the requests in order,
//! with at most [`MAX_DOCUMENTS`] documents open. Both write their replies
//! under one lock, each frame whole. The process ends at the end of its
//! input, which cancels every open not yet answered.
//!
//! An open is cancelled when its document id is at most the cancel mark:
//! the reader raises it to the newest open it has queued, and the work
//! thread polls it while reading. Whether an open was still pending, and so
//! whether it ends as cancelled or open, is decided under one lock, so a
//! `CancelRequested` is never followed by that open succeeding.

pub mod document;

use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;

use studio_domain::analysis::AnalysisFailure;
use studio_domain::job::WirePath;
use studio_domain::protocol::{
    Command, Envelope, ErrorCode, Event, ProtocolError, decode, read_frame, write_message,
};

use self::document::{Document, Refusal};
use crate::job::JOB_STACK_BYTES;
use crate::worker::SessionError;
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
        cancel_upto: AtomicU64::new(0),
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
                shared.cancel_upto.store(u64::MAX, Ordering::SeqCst);
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
    Page { first: u64, count: u32 },
    Bytes { offset: u64, len: u32 },
    Close,
}

/// A queued request: its `request_id`, its document and what it asks.
type Queued = (u64, u64, Request);

struct Shared<W> {
    out: Mutex<Out<W>>,
    ledger: Mutex<Ledger>,
    /// Opens of documents up to this id are cancelled.
    cancel_upto: AtomicU64,
}

struct Out<W> {
    writer: W,
    /// The output failed; everything after is discarded.
    error: Option<ProtocolError>,
}

#[derive(Debug, Default)]
struct Ledger {
    /// The newest document id an open was queued for.
    queued: u64,
    /// The newest document id an open was answered for.
    answered: u64,
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
            self.cancel_upto.store(u64::MAX, Ordering::SeqCst);
        }
    }

    fn cancelled(&self, document: u64) -> bool {
        document <= self.cancel_upto.load(Ordering::SeqCst)
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

/// Best-effort `request_id` of a frame that did not decode as a command.
fn request_id_of(frame: &[u8]) -> u64 {
    serde_json::from_slice::<serde_json::Value>(frame)
        .ok()
        .and_then(|value| value.get("request_id")?.as_u64())
        .unwrap_or(0)
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
            Command::Cancel => {
                let ledger = shared.ledger();
                if ledger.answered < ledger.queued {
                    shared.cancel_upto.store(ledger.queued, Ordering::SeqCst);
                    shared.send(id, Event::CancelRequested);
                } else {
                    shared.send(
                        id,
                        refusal(ErrorCode::WrongState, "no document is being opened"),
                    );
                }
                continue;
            }
            Command::Probe { .. }
            | Command::Start { .. }
            | Command::Picture { .. }
            | Command::Play { .. }
            | Command::StopPlayback => {
                shared.send(
                    id,
                    refusal(
                        ErrorCode::WrongState,
                        "the inspect process serves only analysis commands",
                    ),
                );
                continue;
            }
            Command::AnalysisOpen { document, source } => {
                let mut ledger = shared.ledger();
                if document <= ledger.queued {
                    let message = format!(
                        "document ids must increase; {document} is not above {}",
                        ledger.queued
                    );
                    shared.send(
                        id,
                        failed(
                            document,
                            Refusal::new(AnalysisFailure::InvalidRequest, message),
                        ),
                    );
                    continue;
                }
                if requests
                    .try_send((id, document, Request::Open { source }))
                    .is_ok()
                {
                    ledger.queued = document;
                } else {
                    drop(ledger);
                    busy(shared, id, document);
                }
                continue;
            }
            Command::AnalysisPage {
                document,
                first,
                count,
            } => (document, Request::Page { first, count }),
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
            (Request::Page { first, count }, Some(at)) => {
                match documents[at].1.page(first, count) {
                    Ok(elements) => Event::AnalysisPage {
                        document,
                        first,
                        elements,
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
    let opened = if documents.len() >= MAX_DOCUMENTS {
        Err(Refusal::new(
            AnalysisFailure::LimitReached,
            format!("{MAX_DOCUMENTS} documents are open; close one first"),
        ))
    } else {
        Document::open(source, &|| shared.cancelled(document))
    };
    let mut ledger = shared.ledger();
    ledger.answered = document;
    let answer = match opened {
        Ok(_) if shared.cancelled(document) => failed(
            document,
            Refusal::new(AnalysisFailure::Cancelled, "the open was cancelled"),
        ),
        Ok(opened) => {
            let summary = opened.summary().clone();
            documents.push((document, opened));
            Event::AnalysisOpened { document, summary }
        }
        Err(refusal) => failed(document, refusal),
    };
    // Sent under the ledger, so a cancel decided after this sees it answered
    // and one decided before it is honoured above.
    shared.send(id, answer);
    drop(ledger);
}
