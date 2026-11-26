// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! One analysis document's values: where it came from, how far it got, its
//! selection, history and byte window, and the byte arithmetic they use.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use studio_domain::analysis::{DocumentSummary, Element, Row, RowFilter, Span};
use studio_domain::job::SourceFingerprint;
use studio_domain::timeline::TimelineSelection;

use super::evidence::{Evidence, Pick};
use super::{HISTORY, LINE_BYTES, Timeline, WINDOW_BYTES};
use crate::queue::ItemId;

/// A converted file and what it was converted from, fixed when Encode
/// hands it over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultOf {
    /// The queue item that produced it.
    pub item: ItemId,
    /// Its source file.
    pub source: PathBuf,
    /// The converted file.
    pub output: PathBuf,
    /// Its size when the job verified it.
    pub bytes: u64,
}

/// Where a document came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// Opened for analysis.
    File,
    /// A converted file from Encode.
    Result(ResultOf),
}

/// How far a document got. A document outlives every read of it: a read
/// that fails or stops leaves the document, and what the read kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Restored from the last session and not read yet: nothing about the
    /// file is shown until it is read again.
    Unread,
    /// The engine is reading it.
    Opening,
    /// Cancel was asked for; the engine has not answered yet.
    Cancelling,
    /// It is open.
    Open(Box<DocumentSummary>),
    /// It is not the file the restored view was made on. Nothing about it
    /// is shown, and the engine holds no read of it, until the user reads
    /// it as a new file.
    Changed {
        /// The file as it is now.
        now: SourceFingerprint,
    },
    /// Restored, and nothing is at its path now.
    Missing,
    /// It has no facts: why, for the user.
    Failed {
        /// The explanation.
        message: String,
        /// What ended the read.
        by: ReadEnd,
    },
}

/// What ended a read that left no facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadEnd {
    /// The engine refused or could not read the file, or broke the
    /// protocol.
    Failure,
    /// The user cancelled it before anything was read.
    Cancel,
    /// The inspection engine stopped while it held the document.
    EngineStopped,
}

/// The view on a read of a document, to restore on the next read of the
/// same file: kept across a failed read, saved in the journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Restore {
    /// The file as it was read. The view applies only to a file that
    /// matches it.
    pub fingerprint: SourceFingerprint,
    /// The selected element, by index.
    pub selected: Option<u64>,
    /// The filter.
    pub filter: RowFilter,
    /// The byte view was shown.
    pub bytes_shown: bool,
}

/// A document as the journal keeps it: no facts, only where to read them
/// and the view to restore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    /// The file.
    pub path: PathBuf,
    /// Where it came from.
    pub origin: Origin,
    /// The view on its last good read; `None` when it was never read.
    pub restore: Option<Restore>,
}

/// Whether two readings are of the same file. A file system that reports
/// no modification time cannot tell, so that is never the same.
#[must_use]
pub fn same_file(a: SourceFingerprint, b: SourceFingerprint) -> bool {
    a.len == b.len && a.modified.is_some() && a.modified == b.modified
}

/// The selected element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selected {
    /// Its facts and place in the document.
    pub row: Row,
    /// Its row under the current filter.
    pub position: Position,
}

/// Where the selection is among the rows of the current filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    /// Not known yet: the filter changed or the history moved, and the
    /// engine has not answered.
    Unknown,
    /// The filter does not admit it; it stays selected.
    Hidden,
    /// This row.
    Row(u64),
}

impl Position {
    /// The row, when there is one.
    #[must_use]
    pub const fn row(self) -> Option<u64> {
        match self {
            Self::Row(row) => Some(row),
            Self::Unknown | Self::Hidden => None,
        }
    }

    /// From a locate answer's row.
    pub(super) const fn of(row: Option<u64>) -> Self {
        match row {
            Some(row) => Self::Row(row),
            None => Self::Hidden,
        }
    }
}

/// A history entry: a selection and the byte view it was made in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Place {
    pub(super) row: Option<Row>,
    pub(super) window: u64,
    pub(super) cursor: u64,
    pub(super) evidence: Option<Pick>,
    pub(super) evidence_tab: i32,
    pub(super) bytes_shown: bool,
    pub(super) timeline: Option<(u64, TimelineSelection)>,
    pub(super) reference: Option<super::references::History>,
}

/// The byte view of one document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bytes {
    /// The first byte of the window, a multiple of [`LINE_BYTES`].
    pub offset: u64,
    /// The window's bytes, once read.
    pub data: Option<Vec<u8>>,
    /// The byte the keyboard acts on.
    pub cursor: u64,
    /// The byte view is shown.
    pub shown: bool,
    /// The request reading the window, and its offset.
    pub(super) asked: Option<(u64, u64)>,
}

/// One open, opening or failed document.
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    /// The engine's id of the current read.
    pub(super) id: u64,
    pub(super) path: PathBuf,
    pub(super) origin: Origin,
    pub(super) phase: Phase,
    /// Structure available before the scan completes.
    pub(super) partial: Option<Box<DocumentSummary>>,
    /// The index grew. Query the visible page even when its rows are cached.
    pub(super) refresh_total: bool,
    pub(super) filter: RowFilter,
    pub(super) filter_generation: u64,
    /// Rows under the filter; `None` until a page says.
    pub(super) total: Option<u64>,
    pub(super) rows: BTreeMap<u64, Row>,
    /// The pages asked for and not answered: filter generation and first
    /// row. Pages of an older filter still count against the limit.
    pub(super) asked: Vec<(u64, u64)>,
    pub(super) top: u64,
    /// A row the keyboard moved to before it arrived.
    pub(super) wanted: Option<u64>,
    pub(super) selected: Option<Selected>,
    pub(super) history: Vec<Place>,
    pub(crate) evidence: Evidence,
    pub(super) timeline: Timeline,
    pub(crate) layers: super::layers::LayerInspector,
    pub(crate) references: super::references::References,
    /// Report awaiting source fingerprint verification at open.
    pub(super) pending_batch: Option<Box<crate::batch::Report>>,
    pub(super) at: usize,
    pub(super) bytes: Bytes,
    /// The newest locate that may select; older ones are dropped.
    pub(super) locating: Option<u64>,
    /// A problem that did not end the document, for the user.
    pub(super) notice: Option<String>,
    /// What stays true of the file for as long as it is open, such as a
    /// converted file whose size changed since it was verified.
    pub(super) caution: Option<String>,
    /// The view to restore when the file is read again; while open, a
    /// restored view the read could not apply.
    pub(super) restore: Option<Restore>,
}

impl Document {
    /// A document of `path` that is not read yet.
    pub(super) fn new(path: PathBuf, origin: Origin, restore: Option<Restore>) -> Self {
        Self {
            id: 0,
            path,
            origin,
            phase: Phase::Unread,
            partial: None,
            refresh_total: false,
            filter: RowFilter::default(),
            filter_generation: 0,
            total: None,
            rows: BTreeMap::new(),
            asked: Vec::new(),
            top: 0,
            wanted: None,
            selected: None,
            history: Vec::new(),
            evidence: Evidence::default(),
            timeline: Timeline::default(),
            layers: super::layers::LayerInspector::default(),
            references: super::references::References::default(),
            pending_batch: None,
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
            caution: None,
            restore,
        }
    }

    pub(super) fn remember_selection(&mut self) {
        self.history.truncate(self.at.saturating_add(1));
        self.history.push(Place {
            row: self
                .selected
                .as_ref()
                .map(|selection| selection.row.clone()),
            window: self.bytes.offset,
            cursor: self.bytes.cursor,
            evidence: self.evidence.selected,
            evidence_tab: self.evidence.tab,
            bytes_shown: self.bytes.shown,
            timeline: self.timeline.history(),
            reference: None,
        });
        if self.history.len() > HISTORY {
            self.history.remove(0);
        }
        self.at = self.history.len() - 1;
    }

    pub(crate) fn selection_pending(&self) -> bool {
        self.locating.is_some()
    }

    /// The view on the current read, or the one kept for the next.
    #[must_use]
    pub fn restore(&self) -> Option<Restore> {
        match &self.phase {
            // A restored view this read could not apply, until replaced.
            Phase::Open(_) if self.restore.is_some() => self.restore.map(|r| Restore {
                bytes_shown: self.bytes.shown,
                ..r
            }),
            Phase::Open(summary) => Some(Restore {
                fingerprint: summary.fingerprint,
                selected: self.selected.as_ref().map(|s| s.row.index),
                filter: self.filter,
                bytes_shown: self.bytes.shown,
            }),
            _ => self.restore,
        }
    }

    /// The document as the journal keeps it.
    #[must_use]
    pub fn saved(&self) -> Saved {
        Saved {
            path: self.path.clone(),
            origin: self.origin.clone(),
            restore: self.restore(),
        }
    }

    /// The engine holds this document: it is being read or is open there.
    #[must_use]
    pub const fn in_engine(&self) -> bool {
        matches!(
            self.phase,
            Phase::Opening | Phase::Cancelling | Phase::Open(_)
        )
    }

    /// The read ended without facts: keeps the view for the next read.
    pub(super) fn fail(&mut self, message: String, by: ReadEnd) {
        self.pending_batch = None;
        self.references = super::references::References::default();
        self.restore = self.restore();
        self.partial = None;
        self.timeline = Timeline::default();
        self.phase = Phase::Failed { message, by };
    }

    /// The timeline for the current document generation.
    #[must_use]
    pub const fn timeline(&self) -> &Timeline {
        &self.timeline
    }

    /// The engine's id of the current read; `0` before the first.
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// The operating point requested for playback of this document.
    #[must_use]
    pub const fn layer_decode_selection(&self) -> studio_domain::layers::LayerDecodeSelection {
        self.layers.selection
    }

    /// The file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Where it came from.
    #[must_use]
    pub const fn origin(&self) -> &Origin {
        &self.origin
    }

    /// How far it got.
    #[must_use]
    pub const fn phase(&self) -> &Phase {
        &self.phase
    }

    pub(super) fn update_partial(&mut self, summary: DocumentSummary) {
        self.refresh_total |= self
            .partial
            .as_ref()
            .is_none_or(|old| old.elements != summary.elements);
        self.partial = Some(Box::new(summary));
    }

    /// What the current read found.
    #[must_use]
    pub fn summary(&self) -> Option<&DocumentSummary> {
        match &self.phase {
            Phase::Open(summary) => Some(summary),
            Phase::Opening | Phase::Cancelling => self.partial.as_deref(),
            _ => None,
        }
    }

    /// The filter.
    #[must_use]
    pub const fn filter(&self) -> RowFilter {
        self.filter
    }

    /// Rows under the filter, once known.
    #[must_use]
    pub const fn total(&self) -> Option<u64> {
        self.total
    }

    /// The first visible row.
    #[must_use]
    pub const fn top(&self) -> u64 {
        self.top
    }

    /// The row at `position`, if it has arrived.
    #[must_use]
    pub fn row(&self, position: u64) -> Option<&Row> {
        self.rows.get(&position)
    }

    /// The number of rows kept.
    #[must_use]
    pub fn kept_rows(&self) -> usize {
        self.rows.len()
    }

    /// The selection.
    #[must_use]
    pub const fn selected(&self) -> Option<&Selected> {
        self.selected.as_ref()
    }

    /// Back has somewhere to go.
    #[must_use]
    pub const fn can_go_back(&self) -> bool {
        self.at > 0
    }

    /// Forward has somewhere to go.
    #[must_use]
    pub const fn can_go_forward(&self) -> bool {
        self.at + 1 < self.history.len()
    }

    /// The byte view.
    #[must_use]
    pub const fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// The last request's problem, until the next input here.
    #[must_use]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// What stays true of the file while it is open.
    #[must_use]
    pub fn caution(&self) -> Option<&str> {
        self.caution.as_deref()
    }

    /// The file's length, once open.
    pub(super) fn len(&self) -> Option<u64> {
        self.summary().map(|s| s.fingerprint.len)
    }
}

/// The file bytes an element covers: `(first, end)`.
#[must_use]
pub fn extent(element: &Element) -> (u64, u64) {
    let end = |span: Span| span.offset.saturating_add(span.len);
    match element {
        Element::IvfHeader { span } | Element::Unparsed { span, .. } => (span.offset, end(*span)),
        Element::Packet { packet } => (packet.header.offset, end(packet.payload)),
        Element::Obu { obu } => (obu.length_prefix.offset, end(obu.obu)),
    }
}

pub(super) const fn line_of(offset: u64) -> u64 {
    offset - offset % LINE_BYTES
}

pub(super) const fn in_window(window: u64, offset: u64) -> bool {
    offset >= window && offset < window.saturating_add(WINDOW_BYTES)
}

/// A byte offset as typed: decimal digits, or hexadecimal after `0x`.
/// Spaces, underscores and thin separators are ignored.
#[must_use]
pub fn parse_offset(text: &str) -> Option<u64> {
    let clean: String = text
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '_' && *c != ',')
        .collect();
    let hex = clean
        .strip_prefix("0x")
        .or_else(|| clean.strip_prefix("0X"));
    let (digits, radix) = hex.map_or((clean.as_str(), 10), |hex| (hex, 16));
    // `from_str_radix` also takes a sign, which is no offset.
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return None;
    }
    u64::from_str_radix(digits, radix).ok()
}
