// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! One analysis document's values: where it came from, how far it got, its
//! selection, history and byte window, and the byte arithmetic they use.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use studio_domain::analysis::{DocumentSummary, Element, Row, RowFilter, Span};

use super::{LINE_BYTES, WINDOW_BYTES};
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

/// How far a document got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// The engine is reading it.
    Opening,
    /// Cancel was asked for; the engine has not answered yet.
    Cancelling,
    /// It is open.
    Open(Box<DocumentSummary>),
    /// It has no facts: why, for the user, and the engine's reason if any.
    Failed {
        /// The explanation.
        message: String,
    },
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
    pub(super) row: Row,
    pub(super) window: u64,
    pub(super) cursor: u64,
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
    pub(super) at: usize,
    pub(super) bytes: Bytes,
    /// The newest locate that may select; older ones are dropped.
    pub(super) locating: Option<u64>,
    /// A problem that did not end the document, for the user.
    pub(super) notice: Option<String>,
    /// What stays true of the file for as long as it is open, such as a
    /// converted file whose size changed since it was verified.
    pub(super) caution: Option<String>,
}

impl Document {
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

    /// What reading it found, once open.
    #[must_use]
    pub fn summary(&self) -> Option<&DocumentSummary> {
        match &self.phase {
            Phase::Open(summary) => Some(summary),
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

    /// A problem that did not end the document: the last request's, until
    /// the next input; otherwise what stays true of the file.
    #[must_use]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref().or(self.caution.as_deref())
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
