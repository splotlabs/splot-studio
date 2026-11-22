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

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use studio_domain::analysis::{
    AnalysisFailure, KindFilter, LayerFilter, LocateTarget, Row, RowFilter,
};
use studio_domain::job::WirePath;
use studio_domain::protocol::{Command, Event};

mod bytes;
mod document;
#[cfg(test)]
mod tests;

pub use document::{
    Bytes, Document, Origin, Phase, Position, ResultOf, Selected, extent, parse_offset,
};
use document::{Place, in_window, line_of};

/// Documents open at once: the inspect process keeps at most four.
pub const MAX_DOCUMENTS: usize = 4;

/// Rows the navigator shows at once; only these are drawn.
pub const VISIBLE_ROWS: u64 = 16;

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
    /// Read the shown document again, after it failed.
    Reopen,
    /// Stop reading the shown document.
    Cancel,
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
}

/// What a request was for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Asked {
    Open,
    Cancel,
    Close,
    Page { generation: u64, first: u64 },
    Locate { generation: u64, purpose: Purpose },
    Bytes { offset: u64 },
}

/// What a locate answer does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Purpose {
    /// Select what it found. From a byte or an offset the byte window
    /// stays; from the list it moves to the element.
    Select { from_list: bool },
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
    dirty: bool,
}

impl Analysis {
    /// No documents.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
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
        let mut choices = vec![KindFilter::All];
        if let Some(summary) = self.shown().and_then(Document::summary) {
            if summary.unparsed > 0 {
                choices.push(KindFilter::Unparsed);
            }
            choices.extend(summary.obu_types.iter().map(|t| KindFilter::Obu {
                obu_type: t.obu_type,
            }));
        }
        choices
    }

    /// The layer filter's choices for the shown document: every layer, then
    /// each layer pair present.
    #[must_use]
    pub fn layer_choices(&self) -> Vec<Option<LayerFilter>> {
        let mut choices = vec![None];
        if let Some(summary) = self.shown().and_then(Document::summary) {
            choices.extend(summary.layers.iter().map(|l| {
                Some(LayerFilter {
                    temporal: l.temporal,
                    embedded: l.embedded,
                })
            }));
        }
        choices
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
                if let Some(doc) = self.documents.get_mut(self.shown)
                    && matches!(doc.phase, Phase::Failed { .. })
                {
                    let (path, origin) = (doc.path.clone(), doc.origin.clone());
                    self.documents.remove(self.shown);
                    self.insert_at(self.shown, path, origin, &mut out);
                }
            }
            Input::Cancel => {
                if let Some(doc) = self.documents.get_mut(self.shown)
                    && doc.phase == Phase::Opening
                {
                    doc.phase = Phase::Cancelling;
                    let document = doc.id;
                    self.send(document, Asked::Cancel, &mut out, |document| {
                        Command::AnalysisCancel { document }
                    });
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
            Input::Open(_)
            | Input::OpenResult(_)
            | Input::Show(_)
            | Input::Close
            | Input::Reopen
            | Input::Cancel => {}
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
            d.path == path && !matches!(d.phase, Phase::Failed { .. }) && d.origin == origin
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
        let at = self.documents.len();
        self.insert_at(at, path, origin, out);
    }

    /// Starts reading `path` as a new document at index `at`, and shows it.
    fn insert_at(&mut self, at: usize, path: PathBuf, origin: Origin, out: &mut Vec<Effect>) {
        self.next_document += 1;
        let id = self.next_document;
        let source = WirePath::from_path(&path);
        self.documents.insert(
            at,
            Document {
                id,
                path,
                origin,
                phase: Phase::Opening,
                filter: RowFilter::default(),
                filter_generation: 0,
                total: None,
                rows: BTreeMap::new(),
                asked: Vec::new(),
                top: 0,
                wanted: None,
                selected: None,
                history: Vec::new(),
                at: 0,
                bytes: Bytes {
                    offset: 0,
                    data: None,
                    cursor: 0,
                    shown: true,
                    asked: None,
                },
                locating: None,
                notice: None,
            },
        );
        self.shown = at;
        self.send(id, Asked::Open, out, move |document| {
            Command::AnalysisOpen { document, source }
        });
    }

    fn close(&mut self, out: &mut Vec<Effect>) {
        if self.shown >= self.documents.len() {
            return;
        }
        let doc = self.documents.remove(self.shown);
        // Its answers have nobody to go to now, except an open's: one that
        // still succeeds is closed when it answers.
        self.pending
            .retain(|_, (document, asked)| *document != doc.id || *asked == Asked::Open);
        match doc.phase {
            Phase::Opening => {
                self.send(doc.id, Asked::Cancel, out, |document| {
                    Command::AnalysisCancel { document }
                });
            }
            Phase::Open(_) => {
                self.send(doc.id, Asked::Close, out, |document| {
                    Command::AnalysisClose { document }
                });
            }
            // Cancel is on its way; a failed document is not open.
            Phase::Cancelling | Phase::Failed { .. } => {}
        }
        self.shown = self.shown.min(self.documents.len().saturating_sub(1));
        self.notice = None;
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
        let answered = match &event {
            Event::AnalysisOpened { document: d, .. }
            | Event::AnalysisPage { document: d, .. }
            | Event::AnalysisLocated { document: d, .. }
            | Event::AnalysisBytes { document: d, .. }
            | Event::AnalysisClosed { document: d }
            | Event::AnalysisFailed { document: d, .. } => Some(*d),
            _ => None,
        };
        if answered.is_some_and(|d| d != document) {
            return out;
        }
        let Some(at) = self.index_of(document) else {
            // A document closed while it was being opened: close it there too.
            if asked == Asked::Open && matches!(event, Event::AnalysisOpened { .. }) {
                self.send(document, Asked::Close, &mut out, |document| {
                    Command::AnalysisClose { document }
                });
            }
            return out;
        };
        self.dirty = true;
        match (asked, event) {
            (Asked::Open, Event::AnalysisOpened { summary, .. }) => {
                let doc = &mut self.documents[at];
                if let Origin::Result(result) = &doc.origin
                    && summary.fingerprint.len != result.bytes
                {
                    doc.notice = Some(format!(
                        "This file is {} bytes now; the conversion wrote {} bytes. It may have \
                         changed since.",
                        summary.fingerprint.len, result.bytes
                    ));
                }
                doc.phase = Phase::Open(Box::new(summary));
                self.fill(at, &mut out);
                self.fetch_bytes(at, &mut out);
            }
            (Asked::Page { generation, first }, Event::AnalysisPage { total, rows, .. }) => {
                let doc = &mut self.documents[at];
                doc.asked.retain(|f| *f != first);
                if generation == doc.filter_generation {
                    doc.total = Some(total);
                    for (position, row) in (first..).zip(rows) {
                        doc.rows.insert(position, row);
                    }
                    self.clamp_top(at);
                    self.take_wanted(at, &mut out);
                    self.fill(at, &mut out);
                }
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
            (Asked::Bytes { offset }, Event::AnalysisBytes { bytes, .. }) => {
                let doc = &mut self.documents[at];
                if doc.bytes.asked.is_some_and(|(_, o)| o == offset) {
                    doc.bytes.asked = None;
                }
                if doc.bytes.offset == offset {
                    doc.bytes.data = Some(bytes);
                }
            }
            (_, Event::AnalysisFailed { code, message, .. }) => {
                self.failed(at, asked, code, message);
            }
            // A refused cancel: the open was answered first, and decides.
            _ => {}
        }
        out
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
            Purpose::Select { from_list } => {
                if doc.locating != Some(request_id) {
                    return;
                }
                doc.locating = None;
                let position = if current {
                    Position::of(row)
                } else {
                    Position::Unknown
                };
                self.select(at, found, position, from_list, out);
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

    /// A request failed. A changed or unknown document fails as a whole; a
    /// failed open is the document's answer; anything else is a notice.
    fn failed(&mut self, at: usize, asked: Asked, code: AnalysisFailure, message: String) {
        let doc = &mut self.documents[at];
        match asked {
            Asked::Open => {
                doc.phase = Phase::Failed {
                    code: Some(code),
                    message: match code {
                        AnalysisFailure::Cancelled => "Reading was cancelled.".to_owned(),
                        _ => message,
                    },
                };
            }
            Asked::Page { first, .. } => {
                doc.asked.retain(|f| *f != first);
                fail_or_notice(doc, code, &message);
            }
            Asked::Bytes { .. } => {
                doc.bytes.asked = None;
                fail_or_notice(doc, code, &message);
            }
            Asked::Locate { .. } => {
                doc.locating = None;
                fail_or_notice(doc, code, &message);
            }
            Asked::Cancel | Asked::Close => {}
        }
    }

    /// The inspect process ended: every document it held is gone.
    pub fn process_ended(&mut self, detail: &str) {
        self.pending.clear();
        for doc in &mut self.documents {
            if !matches!(doc.phase, Phase::Failed { .. }) {
                doc.phase = Phase::Failed {
                    code: None,
                    message: format!(
                        "The inspection engine stopped ({detail}). Reopen the file to read it \
                         again."
                    ),
                };
                self.dirty = true;
            }
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
        let start = extent(&row.element).0;
        if from_list {
            doc.bytes.cursor = start;
            if !in_window(doc.bytes.offset, start) {
                doc.bytes.offset = line_of(start);
                doc.bytes.data = None;
            }
        }
        // Forward history ends at a new selection.
        doc.history
            .truncate(doc.at.saturating_add(1).min(doc.history.len()));
        doc.history.push(Place {
            row: row.clone(),
            window: doc.bytes.offset,
            cursor: doc.bytes.cursor,
        });
        if doc.history.len() > HISTORY {
            doc.history.remove(0);
        }
        doc.at = doc.history.len() - 1;
        doc.selected = Some(Selected { row, position });
        if let Position::Row(position) = position {
            self.reveal(at, position);
        }
        self.fill(at, out);
        self.fetch_bytes(at, out);
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
        doc.selected = Some(Selected {
            row: place.row,
            position: Position::Unknown,
        });
        self.position_selection(at, out);
        self.fetch_bytes(at, out);
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
        if let Purpose::Select { .. } = purpose {
            self.documents[at].locating = Some(id);
        }
    }

    fn refilter(&mut self, at: usize, filter: RowFilter, out: &mut Vec<Effect>) {
        let doc = &mut self.documents[at];
        if doc.filter == filter {
            return;
        }
        doc.filter = filter;
        doc.filter_generation += 1;
        doc.total = None;
        doc.rows.clear();
        doc.asked.clear();
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
        let Some(total) = doc.total.filter(|t| *t > 0) else {
            return;
        };
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
                match doc.rows.get(&(position + 1)) {
                    Some(next) if next.parent.is_some() && next.parent == index => position + 1,
                    _ => return,
                }
            }
            (Step::Right, None) => return,
            (Step::End, _) => last,
            (Step::Home, _) | (_, None) => 0,
            (Step::Up, Some(p)) => p.saturating_sub(1),
            (Step::Down, Some(p)) => (p + 1).min(last),
            (Step::PageUp, Some(p)) => p.saturating_sub(VISIBLE_ROWS - 1),
            (Step::PageDown, Some(p)) => (p + VISIBLE_ROWS - 1).min(last),
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
        } else if position >= doc.top + VISIBLE_ROWS {
            doc.top = position + 1 - VISIBLE_ROWS;
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
        let end = doc.total.map_or(top + 1, |t| t.min(top + VISIBLE_ROWS));
        let asked = |p: u64| {
            doc.asked
                .iter()
                .any(|f| (*f..f.saturating_add(u64::from(PAGE_ROWS))).contains(&p))
        };
        let missing = (top..end).find(|p| !doc.rows.contains_key(p) && !asked(*p));
        // Two pages in flight per document at most; the engine queues 64.
        if let Some(first) = missing.filter(|_| doc.asked.len() < 2) {
            doc.asked.push(first);
            let (id, filter, generation) = (doc.id, doc.filter, doc.filter_generation);
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
fn fail_or_notice(doc: &mut Document, code: AnalysisFailure, message: &str) {
    match code {
        AnalysisFailure::SourceChanged => {
            doc.phase = Phase::Failed {
                code: Some(code),
                message: "The file changed or was replaced since it was opened. Reopen it to \
                          read it again."
                    .to_owned(),
            };
        }
        AnalysisFailure::UnknownDocument => {
            doc.phase = Phase::Failed {
                code: Some(code),
                message: "The engine no longer has this document open. Reopen it to read it \
                          again."
                    .to_owned(),
            };
        }
        _ => doc.notice = Some(format!("The engine could not answer: {message}")),
    }
}
