// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The Analyze workspace without a window: the open documents, and for
//! each one its rows, its selection, its history and its byte window.
//!
//! [`Analysis`] is a state machine like [`crate::preview::Preview`]: user
//! [`Input`] and engine events change it and return the [`Effect`]s
//! (commands) for the one inspect process. Nothing here starts processes.
//!
//! Selection rules:
//! - Each document owns one selection: an element, identified by its index
//!   in the document. Clicking a row and clicking a byte both end in
//!   [`Analysis`]'s one `select`; a byte asks the engine for the innermost
//!   element that holds it first. Selecting the element already selected
//!   changes nothing, so nothing can loop.
//! - Every new selection is a history entry with the byte window and
//!   cursor it was made in; Back and Forward restore them.
//! - A filter decides which elements are rows. A selection the filter does
//!   not admit stays selected and is reported hidden, never moved.
//!
//! Transport rules:
//! - Rows arrive a page at a time and are kept by row position, at most
//!   [`KEPT_ROWS`] per document around the visible ones; a page answers for
//!   one filter generation and is dropped once the filter changed.
//! - Every request is recorded by its request id with its document; an
//!   answer with no record, or for another document, is dropped. Documents
//!   are engine ids that only increase, so a closed document's answers can
//!   never reach a newer one.
//! - A process that ends takes its documents with it: they fail and say
//!   so, and Reopen reads them again with a new id.
//!
//! Lifecycle rules:
//! - A document is not its read. A read that fails, stops or is cancelled
//!   leaves the document, with what the read kept, and keeps its view (the
//!   selection, filter and byte view) as a [`Restore`] for the next read.
//! - A restored document ([`Analysis::restored`]) shows nothing about its
//!   file until it is read again, and it is read only once it is shown
//!   ([`Analysis::read_shown`]). Its view applies only when the new read
//!   is of the same file ([`same_file`]); otherwise the document waits as
//!   [`Phase::Changed`] until the user reads it as a new file. A size or
//!   modification time that already differs is a change found without a
//!   read. A path with nothing at it is [`Phase::Missing`] until the user
//!   locates the file.

use std::collections::HashMap;
use std::path::PathBuf;

use studio_domain::analysis::{
    AnalysisFailure, DocumentSummary, KindFilter, LayerFilter, LocateTarget, ProblemKind, Row,
    RowFilter, ScanOutcome,
};
use studio_domain::job::WirePath;
use studio_domain::protocol::{Command, Event};

mod bytes;
mod document;
pub(crate) mod evidence;
#[cfg(test)]
mod tests;

pub use document::{
    Bytes, Document, Origin, Phase, Position, ReadEnd, Restore, ResultOf, Saved, Selected, extent,
    parse_offset, same_file,
};
use document::{in_window, line_of};
use evidence::{Pick, Request as EvidenceRequest, VIEW_ROWS};

use crate::recovery::{Disk, Kind, RestoredView};

/// Documents open at once: the inspect process keeps at most four.
pub const MAX_DOCUMENTS: usize = 4;

/// Rows the navigator shows at once; only these are drawn.
pub const VISIBLE_ROWS: u64 = 12;

/// Rows asked for in one page request.
pub const PAGE_ROWS: u32 = 64;

/// Rows kept per document, around the visible ones.
pub const KEPT_ROWS: u64 = 1024;

/// Bytes on one line of the byte view.
pub const LINE_BYTES: u64 = 16;

/// Lines of the byte view; the byte window is this many lines.
pub const WINDOW_LINES: u64 = 8;

/// History entries kept per document.
pub const HISTORY: usize = 64;

/// The bytes the byte view shows at once.
pub const WINDOW_BYTES: u64 = LINE_BYTES * WINDOW_LINES;

/// A command for the inspect process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effect {
    /// The envelope's request id.
    pub request_id: u64,
    /// The command.
    pub command: Command,
}

/// A movement in the navigator or the byte view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// One row or line up.
    Up,
    /// One row or line down.
    Down,
    /// One screen up.
    PageUp,
    /// One screen down.
    PageDown,
    /// To the first.
    Home,
    /// To the last.
    End,
    /// Navigator: to the packet that holds the selection. Byte view: one
    /// byte back.
    Left,
    /// Navigator: to the selection's first nested row. Byte view: one byte
    /// on.
    Right,
}

/// What the user did in the Analyze workspace.
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// Files picked for analysis.
    Open(Vec<PathBuf>),
    /// A converted file, handed over from Encode.
    OpenResult(ResultOf),
    /// Show the document at this index of [`Analysis::documents`].
    Show(i32),
    /// Close the shown document.
    Close,
    /// Read the shown document again, after it failed or its read was
    /// cancelled.
    Reopen,
    /// Stop reading the shown document.
    Cancel,
    /// Stop reading the document at this index of [`Analysis::documents`].
    CancelDocument(i32),
    /// The shown document's way out of [`Phase::Changed`] or
    /// [`Phase::Missing`]: a changed file is read as a new one; for a
    /// missing one, the caller asks where it is now and sends
    /// [`Input::Relink`].
    Resolve,
    /// Read the missing document of path `missing` from file `to` instead.
    /// It is shown, and checked and read by [`Analysis::read_shown`].
    Relink {
        /// Where it was.
        missing: PathBuf,
        /// Where it is now, as the user located it.
        to: PathBuf,
    },
    /// A visible navigator row, by its slot from the top.
    RowClicked(i32),
    /// Move the navigator selection.
    Move(Step),
    /// Scroll the navigator by this many rows.
    Scroll(i32),
    /// Scroll the navigator to this share of its rows.
    ScrollTo(f32),
    /// A byte of the byte window, by its slot.
    ByteClicked(i32),
    /// Move the byte cursor.
    Cursor(Step),
    /// Select the element that holds the byte cursor.
    SelectAtCursor,
    /// Show or hide the byte view.
    ToggleBytes,
    /// Back in the selection history.
    Back,
    /// Forward in the selection history.
    Forward,
    /// Go to a byte offset, as typed: decimal, or hexadecimal after `0x`.
    GoTo(String),
    /// The type filter, by index into [`Analysis::type_choices`].
    TypeFilter(i32),
    /// The layer filter, by index into [`Analysis::layer_choices`].
    LayerFilter(i32),
    /// Admit every element again.
    ClearFilters,
    /// Show Properties, Syntax or Findings.
    EvidenceView(i32),
    /// Search the open evidence view.
    EvidenceSearch(String),
    /// Filter finding rule IDs.
    EvidenceRule(String),
    /// Select a finding severity filter.
    EvidenceSeverity(i32),
    /// Select a finding layer filter.
    EvidenceLayer(i32),
    /// Select a visible evidence row.
    EvidencePick(i32),
    /// Move the selection within the current evidence list.
    EvidenceMove(Step),
    /// Change the current evidence page.
    EvidencePage(i32),
    /// Select the previous or next matching finding.
    FindingStep(i32),
    /// Validate the current source.
    Validate,
    /// Stop validation of the current source.
    CancelValidation,
}

/// What a request was for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Asked {
    Open,
    Evidence(EvidenceRequest),
    Cancel,
    Close,
    Page { generation: u64, first: u64 },
    Locate { generation: u64, purpose: Purpose },
    Bytes { offset: u64, len: u32 },
}

/// What a locate answer does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Purpose {
    /// Select what it found. From a byte or an offset the byte window
    /// stays; from the list it moves to the element.
    Select {
        from_list: bool,
    },
    /// Select the element of a restored view, as from the list. Unlike a
    /// selection of the user's, it keeps a restored view the read could
    /// not apply in full.
    Restore,
    Evidence {
        pick: Pick,
        offset: u64,
    },
    /// Learn the selection's row under the filter.
    Position,
}

/// The Analyze workspace's documents and requests.
#[derive(Debug, Default)]
pub struct Analysis {
    documents: Vec<Document>,
    shown: usize,
    next_document: u64,
    next_request: u64,
    /// Requests not answered: request id to engine document id and purpose.
    pending: HashMap<u64, (u64, Asked)>,
    notice: Option<String>,
    /// Why the documents cannot be saved, while that is so.
    storage: Option<String>,
    dirty: bool,
}

impl Analysis {
    /// No documents.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The documents of the last session, not read yet, with the shown
    /// one shown. At most [`MAX_DOCUMENTS`] are kept, each file and origin
    /// once; the notice says how many were not, and when the view could
    /// not be read at all.
    #[must_use]
    pub fn restored(view: RestoredView) -> Self {
        let RestoredView {
            documents: saved,
            shown,
            mut dropped,
            unreadable,
            ..
        } = view;
        let wanted = saved.get(shown).map(|s| (s.path.clone(), s.origin.clone()));
        let mut documents: Vec<Document> = Vec::new();
        for saved in saved {
            let twin = documents
                .iter()
                .any(|d| d.path == saved.path && d.origin == saved.origin);
            if !twin && documents.len() < MAX_DOCUMENTS {
                documents.push(Document::new(saved.path, saved.origin, saved.restore));
            } else {
                dropped += 1;
            }
        }
        let why = "a path written on another kind of system, a second copy of a document, or \
                   more than four";
        let notice = match (unreadable, dropped) {
            (Some(copy), _) => Some(format!(
                "The documents of the last session could not be read, so none was restored. \
                 The saved list as it was is kept at {}.",
                copy.display()
            )),
            (None, 0) => None,
            (None, 1) => Some(format!(
                "1 document of the last session was not restored ({why})."
            )),
            (None, n) => Some(format!(
                "{n} documents of the last session were not restored ({why})."
            )),
        };
        // The shown one, wherever it is in the list kept.
        let shown = wanted
            .and_then(|(path, origin)| {
                documents
                    .iter()
                    .position(|d| d.path == path && d.origin == origin)
            })
            .unwrap_or(0);
        Self {
            shown,
            documents,
            notice,
            dirty: true,
            ..Self::default()
        }
    }

    /// Every document as the journal keeps it, and the shown one's index.
    #[must_use]
    pub fn saved(&self) -> (Vec<Saved>, usize) {
        (
            self.documents.iter().map(Document::saved).collect(),
            self.shown,
        )
    }

    /// Reads the shown document when it is not read yet; the others wait
    /// until they are shown, so none of them delays it. With nothing at its
    /// path on `disk` it becomes [`Phase::Missing`]. When its size or
    /// modification time on `disk` already differ from its restored view's,
    /// it becomes [`Phase::Changed`] without a read.
    pub fn read_shown(&mut self, disk: &dyn Disk) -> Vec<Effect> {
        let mut out = Vec::new();
        let at = self.shown;
        let Some(doc) = self.documents.get_mut(at) else {
            return out;
        };
        if doc.phase != Phase::Unread {
            return out;
        }
        self.dirty = true;
        if disk.kind(&doc.path) == Kind::Missing {
            doc.phase = Phase::Missing;
        } else if let Some((restore, now)) = doc
            .restore
            .and_then(|r| disk.fingerprint(&doc.path).map(|now| (r, now)))
            && !same_file(restore.fingerprint, now)
        {
            doc.phase = Phase::Changed { now };
        } else {
            self.read(at, &mut out);
        }
        out
    }

    /// Whether any document needs the inspect process: one is being read,
    /// or is open there.
    #[must_use]
    pub fn needs_engine(&self) -> bool {
        self.documents.iter().any(Document::in_engine)
    }

    /// The inspect process was let go while no document needed it: what
    /// was asked of it has nobody to answer.
    pub fn engine_released(&mut self) {
        self.pending.clear();
    }

    /// Whether the shown document can be read again: its read was cancelled
    /// and kept only the start of the file.
    #[must_use]
    pub fn can_read_again(&self) -> bool {
        self.shown().and_then(Document::summary).is_some_and(|s| {
            matches!(&s.outcome, ScanOutcome::Stopped { problem } if problem.kind == ProblemKind::Cancelled)
        })
    }

    /// Every document, in the order they were opened.
    #[must_use]
    pub fn documents(&self) -> &[Document] {
        &self.documents
    }

    /// The index of the shown document.
    #[must_use]
    pub const fn shown_index(&self) -> usize {
        self.shown
    }

    /// The shown document.
    #[must_use]
    pub fn shown(&self) -> Option<&Document> {
        self.documents.get(self.shown)
    }

    /// A problem that is about no single document.
    #[must_use]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// The one notice the workspace shows: the shown document's request
    /// problem, then a problem about no single document, then why the
    /// documents are not saved, then what stays true of the shown file.
    #[must_use]
    pub fn notice_line(&self) -> Option<&str> {
        let doc = self.shown();
        doc.and_then(Document::notice)
            .or(self.notice.as_deref())
            .or(self.storage.as_deref())
            .or_else(|| doc.and_then(Document::caution))
    }

    /// Records why the documents cannot be saved (`None` once they can).
    pub fn set_storage_problem(&mut self, problem: Option<String>) {
        if self.storage != problem {
            self.storage = problem;
            self.dirty = true;
        }
    }

    /// Whether a request to the inspect process is not answered yet.
    #[must_use]
    pub fn waiting(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Whether anything changed since the last call.
    pub const fn take_dirty(&mut self) -> bool {
        let dirty = self.dirty;
        self.dirty = false;
        dirty
    }

    /// The type filter's choices for the shown document: every element,
    /// unparsed regions when there are any, then each OBU type present.
    #[must_use]
    pub fn type_choices(&self) -> Vec<KindFilter> {
        kind_choices(self.shown().and_then(Document::summary))
    }

    /// The layer filter's choices for the shown document: every layer, then
    /// each layer pair present.
    #[must_use]
    pub fn layer_choices(&self) -> Vec<Option<LayerFilter>> {
        layer_choices(self.shown().and_then(Document::summary))
    }

    /// The user did something.
    pub fn input(&mut self, input: Input) -> Vec<Effect> {
        let mut out = Vec::new();
        self.dirty = true;
        match input {
            Input::Open(paths) => {
                for path in paths {
                    self.open(path, Origin::File, &mut out);
                }
            }
            Input::OpenResult(result) => {
                let path = result.output.clone();
                self.open(path, Origin::Result(result), &mut out);
            }
            Input::Show(index) => {
                if let Some(index) = usize::try_from(index)
                    .ok()
                    .filter(|i| *i < self.documents.len())
                {
                    self.shown = index;
                }
            }
            Input::Close => self.close(&mut out),
            Input::Reopen => {
                let at = self.shown;
                if self.can_read_again() {
                    // The engine still holds the part that was read.
                    self.restore_from_live(at);
                    let document = self.documents[at].id;
                    self.pending.retain(|_, (d, _)| *d != document);
                    let validation = self.documents[at].evidence.running;
                    self.release(document, validation, &mut out);
                    self.read(at, &mut out);
                } else if matches!(
                    self.documents.get(at).map(Document::phase),
                    Some(Phase::Failed { .. })
                ) {
                    self.read(at, &mut out);
                }
            }
            Input::Cancel => self.cancel(self.shown, &mut out),
            Input::CancelDocument(index) => {
                if let Ok(at) = usize::try_from(index) {
                    self.cancel(at, &mut out);
                }
            }
            Input::Resolve => {
                let at = self.shown;
                if let Some(doc) = self.documents.get_mut(at)
                    && let Phase::Changed { .. } = doc.phase
                {
                    doc.restore = None;
                    self.read(at, &mut out);
                }
            }
            Input::Relink { missing, to } => {
                let at = self
                    .documents
                    .iter()
                    .position(|d| d.phase == Phase::Missing && d.path == missing);
                if let Some(at) = at {
                    let doc = &mut self.documents[at];
                    if let Origin::Result(result) = &mut doc.origin {
                        result.output.clone_from(&to);
                    }
                    doc.path = to;
                    doc.phase = Phase::Unread;
                    self.shown = at;
                }
            }
            other => {
                if let Some(at) = self.open_index() {
                    self.document_input(at, other, &mut out);
                }
            }
        }
        out
    }

    /// The shown document's index, when it is open.
    fn open_index(&self) -> Option<usize> {
        self.documents
            .get(self.shown)
            .filter(|d| d.summary().is_some())
            .map(|_| self.shown)
    }

    fn document_input(&mut self, at: usize, input: Input, out: &mut Vec<Effect>) {
        // A problem stays in view until the user does something else here.
        self.documents[at].notice = None;
        match input {
            Input::RowClicked(slot) => {
                let Ok(slot) = u64::try_from(slot) else {
                    return;
                };
                let doc = &self.documents[at];
                let position = doc.top.saturating_add(slot);
                if let Some(row) = doc.rows.get(&position).cloned() {
                    self.select(at, row, Position::Row(position), true, out);
                }
            }
            Input::Move(step) => self.step(at, step, out),
            Input::Scroll(rows) => {
                let doc = &mut self.documents[at];
                doc.top = if rows < 0 {
                    doc.top.saturating_sub(u64::from(rows.unsigned_abs()))
                } else {
                    doc.top.saturating_add(u64::from(rows.unsigned_abs()))
                };
                self.clamp_top(at);
                self.fill(at, out);
            }
            Input::ScrollTo(share) => {
                let doc = &mut self.documents[at];
                let last = doc.total.unwrap_or(0).saturating_sub(VISIBLE_ROWS);
                // `last` rows fit an f64 exactly far beyond any document.
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    clippy::cast_precision_loss
                )]
                let top = (f64::from(share.clamp(0.0, 1.0)) * last as f64).round() as u64;
                doc.top = top;
                self.clamp_top(at);
                self.fill(at, out);
            }
            Input::ByteClicked(slot) => {
                let Ok(slot) = u64::try_from(slot) else {
                    return;
                };
                let doc = &mut self.documents[at];
                let offset = doc.bytes.offset.saturating_add(slot);
                if doc.len().is_some_and(|len| offset < len) {
                    doc.bytes.cursor = offset;
                    self.locate(
                        at,
                        LocateTarget::Offset { offset },
                        Purpose::Select { from_list: false },
                        out,
                    );
                }
            }
            Input::Cursor(step) => self.move_cursor(at, step, out),
            Input::SelectAtCursor => {
                let offset = self.documents[at].bytes.cursor;
                self.locate(
                    at,
                    LocateTarget::Offset { offset },
                    Purpose::Select { from_list: false },
                    out,
                );
            }
            Input::ToggleBytes => {
                let bytes = &mut self.documents[at].bytes;
                bytes.shown = !bytes.shown;
                self.fetch_bytes(at, out);
            }
            Input::Back => self.travel(at, false, out),
            Input::Forward => self.travel(at, true, out),
            Input::GoTo(text) => self.go_to(at, &text, out),
            Input::TypeFilter(index) => self.choose_kind(at, index, out),
            Input::LayerFilter(index) => self.choose_layer(at, index, out),
            Input::ClearFilters => self.refilter(at, RowFilter::default(), out),
            Input::EvidenceView(_)
            | Input::EvidenceSearch(_)
            | Input::EvidenceRule(_)
            | Input::EvidenceSeverity(_)
            | Input::EvidenceLayer(_)
            | Input::EvidencePick(_)
            | Input::EvidenceMove(_)
            | Input::EvidencePage(_)
            | Input::FindingStep(_)
            | Input::Validate
            | Input::CancelValidation => self.evidence_input(at, input, out),
            Input::Open(_)
            | Input::OpenResult(_)
            | Input::Show(_)
            | Input::Close
            | Input::Reopen
            | Input::Cancel
            | Input::CancelDocument(_)
            | Input::Resolve
            | Input::Relink { .. } => {}
        }
    }

    fn evidence_input(&mut self, at: usize, input: Input, out: &mut Vec<Effect>) {
        match input {
            Input::EvidenceView(tab) => {
                let evidence = &mut self.documents[at].evidence;
                evidence.tab = tab.clamp(0, 2);
                let doc = &mut self.documents[at];
                if doc.evidence.selected.is_none()
                    && let Some(place) = doc.history.get_mut(doc.at)
                {
                    place.evidence_tab = doc.evidence.tab;
                }
                let evidence = &mut doc.evidence;
                evidence.first = 0;
                evidence.reveal_selection();
                self.ensure_syntax(at, out);
            }
            Input::EvidenceSearch(query) => {
                let evidence = &mut self.documents[at].evidence;
                if evidence.tab == 2 {
                    evidence.finding_query = query;
                } else {
                    evidence.syntax_query = query;
                }
                evidence.first = 0;
            }
            Input::EvidenceRule(rule) => {
                self.documents[at].evidence.rule_query = rule;
                self.documents[at].evidence.first = 0;
            }
            Input::EvidenceSeverity(severity) => {
                self.documents[at].evidence.severity = severity;
                self.documents[at].evidence.first = 0;
            }
            Input::EvidenceLayer(layer) => {
                self.documents[at].evidence.layer = layer;
                self.documents[at].evidence.first = 0;
            }
            Input::EvidencePick(slot) => {
                let evidence = &self.documents[at].evidence;
                if let Ok(slot) = usize::try_from(slot)
                    && let Some(&index) =
                        evidence.matching().get(evidence.first.saturating_add(slot))
                {
                    self.pick_evidence(at, index, out);
                }
            }
            Input::EvidenceMove(step) => self.move_evidence(at, step, out),
            Input::EvidencePage(direction) => {
                let evidence = &mut self.documents[at].evidence;
                if direction < 0 {
                    evidence.first = evidence.first.saturating_sub(VIEW_ROWS);
                } else if evidence.first + VIEW_ROWS < evidence.matching().len() {
                    evidence.first += VIEW_ROWS;
                }
            }
            Input::FindingStep(direction) => {
                self.move_evidence(at, if direction < 0 { Step::Up } else { Step::Down }, out);
            }
            Input::Validate => self.start_validation(
                at,
                studio_domain::validation::ValidationSettings::default(),
                out,
            ),
            Input::CancelValidation => self.stop_validation(at, out),
            _ => {}
        }
    }

    fn choose_kind(&mut self, at: usize, index: i32, out: &mut Vec<Effect>) {
        let choice = usize::try_from(index)
            .ok()
            .and_then(|i| self.type_choices().get(i).copied());
        if let Some(kind) = choice {
            let filter = RowFilter {
                kind,
                ..self.documents[at].filter
            };
            self.refilter(at, filter, out);
        }
    }

    fn choose_layer(&mut self, at: usize, index: i32, out: &mut Vec<Effect>) {
        let choice = usize::try_from(index)
            .ok()
            .and_then(|i| self.layer_choices().get(i).copied());
        if let Some(layer) = choice {
            let filter = RowFilter {
                layer,
                ..self.documents[at].filter
            };
            self.refilter(at, filter, out);
        }
    }

    /// Opens `path`, or shows it when it is already open or opening.
    fn open(&mut self, path: PathBuf, origin: Origin, out: &mut Vec<Effect>) {
        if let Some(at) = self.documents.iter().position(|d| {
            d.path == path
                && !matches!(d.phase, Phase::Failed { .. } | Phase::Missing)
                && d.origin == origin
        }) {
            self.shown = at;
            return;
        }
        if self.documents.len() >= MAX_DOCUMENTS {
            self.notice = Some(format!(
                "At most {MAX_DOCUMENTS} documents can be open. Close one to open {}.",
                crate::present::file_name(&path)
            ));
            return;
        }
        self.notice = None;
        self.documents.push(Document::new(path, origin, None));
        self.shown = self.documents.len() - 1;
        self.read(self.shown, out);
    }

    /// Starts a new read of document `at` with a new engine id. Everything
    /// the last read showed is dropped; its view stays in `restore`.
    fn read(&mut self, at: usize, out: &mut Vec<Effect>) {
        self.next_document += 1;
        let id = self.next_document;
        let old = &self.documents[at];
        let (path, origin, restore) = (old.path.clone(), old.origin.clone(), old.restore);
        let source = WirePath::from_path(&path);
        let mut doc = Document::new(path, origin, restore);
        doc.id = id;
        doc.phase = Phase::Opening;
        self.documents[at] = doc;
        self.send(id, Asked::Open, out, move |document| {
            Command::AnalysisOpen { document, source }
        });
    }

    /// Keeps the view on document `at`'s current read for its next one.
    fn restore_from_live(&mut self, at: usize) {
        let doc = &mut self.documents[at];
        doc.restore = doc.restore();
    }

    /// Stops reading document `at`.
    fn cancel(&mut self, at: usize, out: &mut Vec<Effect>) {
        if let Some(doc) = self.documents.get_mut(at)
            && doc.phase == Phase::Opening
        {
            self.dirty = true;
            doc.phase = Phase::Cancelling;
            let document = doc.id;
            self.send(document, Asked::Cancel, out, |document| {
                Command::AnalysisCancel { document }
            });
        }
    }

    fn close(&mut self, out: &mut Vec<Effect>) {
        if self.shown >= self.documents.len() {
            return;
        }
        let doc = self.documents.remove(self.shown);
        // Its answers have nobody to go to now.
        self.pending.retain(|_, (document, _)| *document != doc.id);
        if doc.phase == Phase::Opening {
            self.send(doc.id, Asked::Cancel, out, |document| {
                Command::AnalysisCancel { document }
            });
        }
        // A cancelled read keeps what it read, so the engine holds it until
        // it is closed. The close waits behind the open in the engine's
        // queue, so the slot is free before any later open.
        if doc.in_engine() {
            self.release(doc.id, doc.evidence.running, out);
        }
        self.shown = self.shown.min(self.documents.len().saturating_sub(1));
        self.notice = None;
    }

    fn release(&mut self, document: u64, validation: Option<u64>, out: &mut Vec<Effect>) {
        if let Some(validation) = validation {
            self.send(
                document,
                Asked::Evidence(EvidenceRequest::Cancel),
                out,
                |document| Command::AnalysisCancelValidation {
                    document,
                    validation,
                },
            );
        }
        self.send(document, Asked::Close, out, |document| {
            Command::AnalysisClose { document }
        });
    }

    fn send(
        &mut self,
        document: u64,
        asked: Asked,
        out: &mut Vec<Effect>,
        command: impl FnOnce(u64) -> Command,
    ) -> u64 {
        self.next_request += 1;
        let request_id = self.next_request;
        self.pending.insert(request_id, (document, asked));
        out.push(Effect {
            request_id,
            command: command(document),
        });
        request_id
    }

    fn index_of(&self, document: u64) -> Option<usize> {
        self.documents.iter().position(|d| d.id == document)
    }

    /// An answer from the inspect process.
    pub fn event(&mut self, request_id: u64, event: Event) -> Vec<Effect> {
        let mut out = Vec::new();
        let Some((document, asked)) = self.pending.remove(&request_id) else {
            return out;
        };
        if reply_document(&event).is_some_and(|d| d != document) {
            return out;
        }
        let Some(at) = self.index_of(document) else {
            return out;
        };
        self.dirty = true;
        match (asked, event) {
            (Asked::Open, Event::AnalysisOpened { summary, .. }) => {
                let doc = &mut self.documents[at];
                if let Origin::Result(result) = &doc.origin
                    && summary.fingerprint.len != result.bytes
                {
                    doc.caution = Some(format!(
                        "This file is {} bytes now; the conversion wrote {} bytes. It may have \
                         changed since.",
                        summary.fingerprint.len, result.bytes
                    ));
                }
                match doc.restore.take() {
                    Some(restore) if !same_file(restore.fingerprint, summary.fingerprint) => {
                        // Never trust the path alone: the view waits. The
                        // engine gives the read back, so nothing of it
                        // outlives the change, an engine stop included.
                        doc.restore = Some(restore);
                        doc.phase = Phase::Changed {
                            now: summary.fingerprint,
                        };
                        self.send(document, Asked::Close, &mut out, |document| {
                            Command::AnalysisClose { document }
                        });
                    }
                    restore => {
                        doc.phase = Phase::Open(Box::new(summary));
                        if let Some(restore) = restore {
                            self.apply(at, restore, &mut out);
                        }
                        self.fill(at, &mut out);
                        self.fetch_bytes(at, &mut out);
                    }
                }
            }
            (
                Asked::Page { generation, first },
                Event::AnalysisPage {
                    first: answered,
                    total,
                    rows,
                    ..
                },
            ) => {
                let asked = (generation, first);
                self.paged(at, asked, answered, total, rows, &mut out);
            }
            (
                Asked::Locate {
                    generation,
                    purpose,
                },
                Event::AnalysisLocated { row, found, .. },
            ) => {
                self.located(at, request_id, generation, purpose, row, *found, &mut out);
            }
            (Asked::Bytes { offset, len }, Event::AnalysisBytes { bytes, .. }) => {
                let doc = &mut self.documents[at];
                if doc.bytes.asked == Some((request_id, offset)) {
                    doc.bytes.asked = None;
                }
                if bytes.len() != len as usize {
                    self.broken(
                        at,
                        "a byte range of another length than asked for",
                        &mut out,
                    );
                } else if doc.bytes.offset == offset {
                    doc.bytes.data = Some(bytes);
                }
            }
            (_, Event::AnalysisFailed { code, message, .. }) => {
                self.failed(at, request_id, asked, Some(code), message);
            }
            // A refused cancel: the open was answered first, and decides.
            (Asked::Cancel, Event::Error { .. }) => {}
            (_, Event::Error { message, .. }) => {
                self.failed(at, request_id, asked, None, message);
            }
            (Asked::Evidence(request), event) => {
                self.evidence_event(at, request_id, request, event, &mut out);
            }
            // An answer of the wrong kind breaks the protocol.
            (Asked::Cancel | Asked::Close, _) => {}
            _ => self.broken(at, "an answer of the wrong kind", &mut out),
        }
        out
    }

    /// A page answered the request for `(generation, first)`.
    fn paged(
        &mut self,
        at: usize,
        (generation, first): (u64, u64),
        answered: u64,
        total: u64,
        rows: Vec<Row>,
        out: &mut Vec<Effect>,
    ) {
        let doc = &mut self.documents[at];
        doc.asked.retain(|a| *a != (generation, first));
        if generation == doc.filter_generation {
            if answered != first || !page_fits(first, total, rows.len()) {
                self.broken(at, "a page that does not fit the rows it counts", out);
                return;
            }
            doc.total = Some(total);
            for (position, row) in (first..).zip(rows) {
                doc.rows.insert(position, row);
            }
            self.clamp_top(at);
            self.take_wanted(at, out);
        }
        self.fill(at, out);
    }

    #[allow(clippy::too_many_arguments)]
    fn located(
        &mut self,
        at: usize,
        request_id: u64,
        generation: u64,
        purpose: Purpose,
        row: Option<u64>,
        found: Row,
        out: &mut Vec<Effect>,
    ) {
        let doc = &mut self.documents[at];
        let current = generation == doc.filter_generation;
        match purpose {
            Purpose::Select { .. } | Purpose::Restore => {
                if doc.locating != Some(request_id) {
                    return;
                }
                doc.locating = None;
                let position = if current {
                    Position::of(row)
                } else {
                    Position::Unknown
                };
                // The restored element is no choice of the user's: what
                // was kept of the restored view stays.
                let kept = (purpose == Purpose::Restore)
                    .then(|| doc.restore.take())
                    .flatten();
                let from_list = !matches!(purpose, Purpose::Select { from_list: false });
                self.select(at, found, position, from_list, out);
                if kept.is_some() {
                    self.documents[at].restore = kept;
                }
                if !current {
                    self.position_selection(at, out);
                }
            }
            Purpose::Evidence { pick, offset } => {
                if doc.locating != Some(request_id) {
                    return;
                }
                doc.locating = None;
                if !doc.evidence.has_finding(pick) {
                    return;
                }
                let position = if current {
                    Position::of(row)
                } else {
                    Position::Unknown
                };
                self.select_evidence(at, found, position, pick, Some(offset), out);
                if !current {
                    self.position_selection(at, out);
                }
            }
            Purpose::Position => {
                let same = doc
                    .selected
                    .as_ref()
                    .is_some_and(|s| s.row.index == found.index);
                if current && same {
                    if let Some(selected) = &mut doc.selected {
                        selected.position = Position::of(row);
                    }
                    if let Some(position) = row {
                        self.reveal(at, position);
                        self.fill(at, out);
                    }
                }
            }
        }
    }

    /// A request failed, with the engine's reason (`None` for a refused
    /// command). A changed or unknown document fails as a whole; a failed
    /// open is the document's answer; anything else is a notice.
    fn failed(
        &mut self,
        at: usize,
        request_id: u64,
        asked: Asked,
        code: Option<AnalysisFailure>,
        message: String,
    ) {
        let doc = &mut self.documents[at];
        match asked {
            Asked::Open => match code {
                Some(AnalysisFailure::Cancelled) => doc.fail(
                    "Reading was cancelled before anything was read.".to_owned(),
                    ReadEnd::Cancel,
                ),
                _ => doc.fail(message, ReadEnd::Failure),
            },
            Asked::Page { generation, first } => {
                doc.asked.retain(|a| *a != (generation, first));
                fail_or_notice(doc, code, &message);
            }
            Asked::Bytes { offset, .. } => {
                if doc.bytes.asked == Some((request_id, offset)) {
                    doc.bytes.asked = None;
                }
                fail_or_notice(doc, code, &message);
            }
            Asked::Locate { .. } => {
                // Only the selecting locate's own failure stops it.
                if doc.locating == Some(request_id) {
                    doc.locating = None;
                }
                fail_or_notice(doc, code, &message);
            }
            Asked::Evidence(request) => {
                if matches!(
                    code,
                    Some(AnalysisFailure::SourceChanged | AnalysisFailure::UnknownDocument)
                ) {
                    fail_or_notice(doc, code, &message);
                } else {
                    doc.evidence.fail(request_id, request, message);
                }
            }
            Asked::Cancel | Asked::Close => {}
        }
    }

    /// The engine broke the protocol for document `at`: nothing more it
    /// says about it is trusted, and its engine slot is given back.
    fn broken(&mut self, at: usize, what: &str, out: &mut Vec<Effect>) {
        let doc = &mut self.documents[at];
        doc.fail(
            format!("The engine sent {what}. Reopen the file to read it again."),
            ReadEnd::Failure,
        );
        let document = doc.id;
        self.pending.retain(|_, (d, _)| *d != document);
        self.send(document, Asked::Close, out, |document| {
            Command::AnalysisClose { document }
        });
    }

    /// The inspect process ended: every document it held is gone.
    pub fn process_ended(&mut self, detail: &str) {
        self.end_reads(
            &format!("The inspection engine stopped ({detail}). Reopen the file to read it again."),
            ReadEnd::EngineStopped,
        );
    }

    /// The inspect process could not be started: no read began.
    pub fn engine_failed(&mut self, why: &str) {
        self.end_reads(
            &format!("The inspection engine did not start ({why}). Reopen the file to try again."),
            ReadEnd::Failure,
        );
    }

    /// Every document in the engine fails with `message`.
    fn end_reads(&mut self, message: &str, by: ReadEnd) {
        self.pending.clear();
        for doc in &mut self.documents {
            if doc.in_engine() {
                doc.fail(message.to_owned(), by);
                self.dirty = true;
            }
        }
    }

    /// Puts back the view `restore` on document `at`, just read from the
    /// same file. A filter the document offers no choice for, and an
    /// element past its end (a read that stopped early may not reach it),
    /// are not applied. Whenever any part is not applied, the whole view
    /// is kept for the journal and the next read until the user chooses a
    /// selection or filter of their own.
    fn apply(&mut self, at: usize, restore: Restore, out: &mut Vec<Effect>) {
        let doc = &mut self.documents[at];
        doc.bytes.shown = restore.bytes_shown;
        let summary = doc.summary();
        let filter = restore.filter;
        let offered = kind_choices(summary).contains(&filter.kind)
            && layer_choices(summary).contains(&filter.layer);
        let elements = summary.map_or(0, |s| s.elements);
        let selected = restore.selected.filter(|i| *i < elements);
        if offered {
            doc.filter = filter;
        }
        if !offered || selected != restore.selected {
            doc.restore = Some(restore);
        }
        if let Some(index) = selected {
            self.locate(at, LocateTarget::Element { index }, Purpose::Restore, out);
        }
    }

    /// Makes `row` the selection of document `at`. `position` is its row
    /// under the current filter when known. A row from the list moves the
    /// byte window to its first byte; a byte keeps the window where it is.
    fn select(
        &mut self,
        at: usize,
        row: Row,
        position: Position,
        from_list: bool,
        out: &mut Vec<Effect>,
    ) {
        let doc = &mut self.documents[at];
        doc.wanted = None;
        doc.locating = None;
        doc.evidence.selected = None;
        if doc
            .selected
            .as_ref()
            .is_some_and(|s| s.row.index == row.index)
        {
            if let Some(selected) = &mut doc.selected
                && position != Position::Unknown
            {
                selected.position = position;
            }
            return;
        }
        // A new selection replaces a restored view kept; the restored
        // element's own locate puts it back (`Purpose::Restore`).
        doc.restore = None;
        let start = extent(&row.element).0;
        if from_list {
            doc.bytes.cursor = start;
            if !in_window(doc.bytes.offset, start) {
                doc.bytes.offset = line_of(start);
                doc.bytes.data = None;
            }
        }
        doc.selected = Some(Selected { row, position });
        doc.remember_selection();
        if let Position::Row(position) = position {
            self.reveal(at, position);
        }
        self.fill(at, out);
        self.fetch_bytes(at, out);
        self.ensure_syntax(at, out);
    }

    fn travel(&mut self, at: usize, forward: bool, out: &mut Vec<Effect>) {
        let doc = &mut self.documents[at];
        let to = if forward {
            doc.at + 1
        } else {
            match doc.at.checked_sub(1) {
                Some(to) => to,
                None => return,
            }
        };
        let Some(place) = doc.history.get(to).cloned() else {
            return;
        };
        doc.at = to;
        doc.wanted = None;
        doc.locating = None;
        if doc.bytes.offset != place.window {
            doc.bytes.offset = place.window;
            doc.bytes.data = None;
        }
        doc.bytes.cursor = place.cursor;
        doc.selected = place.row.map(|row| Selected {
            row,
            position: Position::Unknown,
        });
        doc.evidence.selected = place.evidence.filter(|pick| {
            !matches!(pick, Pick::Finding { .. }) || doc.evidence.has_finding(*pick)
        });
        doc.evidence.tab = place.evidence_tab;
        doc.bytes.shown = place.bytes_shown;
        doc.evidence.reveal_selection();
        self.position_selection(at, out);
        self.fetch_bytes(at, out);
        self.ensure_syntax(at, out);
    }

    /// Asks where the selection is under the current filter.
    fn position_selection(&mut self, at: usize, out: &mut Vec<Effect>) {
        if let Some(index) = self.documents[at].selected.as_ref().map(|s| s.row.index) {
            self.locate(at, LocateTarget::Element { index }, Purpose::Position, out);
        }
    }

    fn locate(&mut self, at: usize, target: LocateTarget, purpose: Purpose, out: &mut Vec<Effect>) {
        let doc = &self.documents[at];
        let (filter, generation) = (doc.filter, doc.filter_generation);
        let id = self.send(
            doc.id,
            Asked::Locate {
                generation,
                purpose,
            },
            out,
            move |document| Command::AnalysisLocate {
                document,
                filter,
                target,
            },
        );
        if let Purpose::Select { .. } | Purpose::Restore | Purpose::Evidence { .. } = purpose {
            self.documents[at].locating = Some(id);
        }
    }

    fn refilter(&mut self, at: usize, filter: RowFilter, out: &mut Vec<Effect>) {
        let doc = &mut self.documents[at];
        if doc.filter == filter {
            return;
        }
        // A filter of the user's own replaces a restored view kept.
        doc.restore = None;
        doc.filter = filter;
        doc.filter_generation += 1;
        doc.total = None;
        doc.rows.clear();
        doc.top = 0;
        doc.wanted = None;
        if let Some(selected) = &mut doc.selected {
            selected.position = Position::Unknown;
        }
        self.position_selection(at, out);
        self.fill(at, out);
    }

    fn step(&mut self, at: usize, step: Step, out: &mut Vec<Effect>) {
        let doc = &self.documents[at];
        let Some(total) = doc.total else {
            // No page has answered for this filter yet: ask again, so a key
            // press restarts paging that failed.
            self.fill(at, out);
            return;
        };
        if total == 0 {
            return;
        }
        let last = total - 1;
        let current = doc.selected.as_ref().and_then(|s| s.position.row());
        let target = match (step, current) {
            (Step::Left, _) => {
                let parent = doc.selected.as_ref().and_then(|s| s.row.parent);
                if let Some(index) = parent {
                    let purpose = Purpose::Select { from_list: true };
                    self.locate(at, LocateTarget::Element { index }, purpose, out);
                }
                return;
            }
            (Step::Right, Some(position)) => {
                let index = doc.selected.as_ref().map(|s| s.row.index);
                let next = position.saturating_add(1);
                match doc.rows.get(&next) {
                    Some(row) if row.parent.is_some() && row.parent == index => next,
                    _ => return,
                }
            }
            (Step::Right, None) => return,
            (Step::End, _) => last,
            (Step::Home, _) | (_, None) => 0,
            (Step::Up, Some(p)) => p.saturating_sub(1),
            (Step::Down, Some(p)) => p.saturating_add(1).min(last),
            (Step::PageUp, Some(p)) => p.saturating_sub(VISIBLE_ROWS - 1),
            (Step::PageDown, Some(p)) => p.saturating_add(VISIBLE_ROWS - 1).min(last),
        };
        if let Some(row) = doc.rows.get(&target).cloned() {
            self.select(at, row, Position::Row(target), true, out);
        } else {
            self.documents[at].wanted = Some(target);
            self.reveal(at, target);
            self.fill(at, out);
        }
    }

    /// Selects the row the keyboard moved to, once it has arrived.
    fn take_wanted(&mut self, at: usize, out: &mut Vec<Effect>) {
        let doc = &self.documents[at];
        if let Some(position) = doc.wanted
            && let Some(row) = doc.rows.get(&position).cloned()
        {
            self.select(at, row, Position::Row(position), true, out);
        }
    }

    /// Keeps `position` among the visible rows.
    fn reveal(&mut self, at: usize, position: u64) {
        let doc = &mut self.documents[at];
        if position < doc.top {
            doc.top = position;
        } else if position >= doc.top.saturating_add(VISIBLE_ROWS) {
            doc.top = position.saturating_add(1).saturating_sub(VISIBLE_ROWS);
        }
        self.clamp_top(at);
    }

    fn clamp_top(&mut self, at: usize) {
        let doc = &mut self.documents[at];
        if let Some(total) = doc.total {
            doc.top = doc.top.min(total.saturating_sub(VISIBLE_ROWS));
        }
    }

    /// Asks for the first visible row that has not arrived and is not
    /// asked for, and lets go of rows far from the visible ones.
    fn fill(&mut self, at: usize, out: &mut Vec<Effect>) {
        let doc = &mut self.documents[at];
        if doc.summary().is_none() {
            return;
        }
        let top = doc.top;
        let keep = top.saturating_sub(KEPT_ROWS / 2)..top.saturating_add(KEPT_ROWS / 2);
        doc.rows.retain(|position, _| keep.contains(position));
        let generation = doc.filter_generation;
        let end = doc.total.map_or(top.saturating_add(1), |t| {
            t.min(top.saturating_add(VISIBLE_ROWS))
        });
        let asked = |p: u64| {
            doc.asked.iter().any(|&(g, f)| {
                g == generation && (f..f.saturating_add(u64::from(PAGE_ROWS))).contains(&p)
            })
        };
        let missing = (top..end).find(|p| !doc.rows.contains_key(p) && !asked(*p));
        // Two pages in flight per document at most, of any filter; the
        // engine queues 64.
        if let Some(first) = missing.filter(|_| doc.asked.len() < 2) {
            doc.asked.push((generation, first));
            let (id, filter) = (doc.id, doc.filter);
            self.send(
                id,
                Asked::Page { generation, first },
                out,
                move |document| Command::AnalysisPage {
                    document,
                    filter,
                    first,
                    count: PAGE_ROWS,
                },
            );
        }
    }
}

/// A failure of a request on an open document.
fn fail_or_notice(doc: &mut Document, code: Option<AnalysisFailure>, message: &str) {
    match code {
        Some(AnalysisFailure::SourceChanged) => doc.fail(
            "The file changed or was replaced since it was opened. Reopen it to read it again."
                .to_owned(),
            ReadEnd::Failure,
        ),
        Some(AnalysisFailure::UnknownDocument) => doc.fail(
            "The engine no longer has this document open. Reopen it to read it again.".to_owned(),
            ReadEnd::Failure,
        ),
        _ => doc.notice = Some(format!("The engine could not answer: {message}")),
    }
}

/// The type filter's choices for `summary`: every element, unparsed
/// regions when there are any, then each OBU type present.
fn kind_choices(summary: Option<&DocumentSummary>) -> Vec<KindFilter> {
    let mut choices = vec![KindFilter::All];
    if let Some(summary) = summary {
        if summary.unparsed > 0 {
            choices.push(KindFilter::Unparsed);
        }
        choices.extend(summary.obu_types.iter().map(|t| KindFilter::Obu {
            obu_type: t.obu_type,
        }));
    }
    choices
}

/// The layer filter's choices for `summary`: every layer, then each layer
/// pair present.
fn layer_choices(summary: Option<&DocumentSummary>) -> Vec<Option<LayerFilter>> {
    let mut choices = vec![None];
    if let Some(summary) = summary {
        choices.extend(summary.layers.iter().map(|l| {
            Some(LayerFilter {
                temporal: l.temporal,
                embedded: l.embedded,
            })
        }));
    }
    choices
}

/// Whether a page of `len` rows from `first` fits `total` rows: no more
/// than asked for, none past the end, and none missing at the start.
fn page_fits(first: u64, total: u64, len: usize) -> bool {
    let len = len as u64;
    let end = first.checked_add(len);
    len <= u64::from(PAGE_ROWS)
        && end.is_some_and(|end| end <= total)
        && (len > 0 || first >= total)
}

fn reply_document(event: &Event) -> Option<u64> {
    match event {
        Event::AnalysisOpened { document: d, .. }
        | Event::AnalysisPage { document: d, .. }
        | Event::AnalysisLocated { document: d, .. }
        | Event::AnalysisBytes { document: d, .. }
        | Event::AnalysisSyntaxReady { document: d, .. }
        | Event::AnalysisSyntaxPage { document: d, .. }
        | Event::AnalysisValidated { document: d, .. }
        | Event::AnalysisFindings { document: d, .. }
        | Event::AnalysisClosed { document: d }
        | Event::AnalysisFailed { document: d, .. } => Some(*d),
        _ => None,
    }
}
