// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The byte window of the shown document: the cursor, go to offset, and
//! the one bounded read that fills the window.

use studio_domain::analysis::LocateTarget;
use studio_domain::protocol::Command;

use super::{
    Analysis, Asked, Effect, LINE_BYTES, Purpose, Step, WINDOW_BYTES, in_window, line_of,
    parse_offset,
};

impl Analysis {
    /// Moves the byte cursor; the window follows it a line at a time.
    pub(super) fn move_cursor(&mut self, at: usize, step: Step, out: &mut Vec<Effect>) {
        let doc = &mut self.documents[at];
        let Some(len) = doc.len().filter(|l| *l > 0) else {
            return;
        };
        let cursor = doc.bytes.cursor;
        let back = |n: u64| cursor.saturating_sub(n);
        let on = |n: u64| cursor.saturating_add(n).min(len - 1);
        doc.bytes.cursor = match step {
            Step::Left => back(1),
            Step::Right => on(1),
            Step::Up => back(LINE_BYTES),
            Step::Down => on(LINE_BYTES),
            Step::PageUp => back(WINDOW_BYTES),
            Step::PageDown => on(WINDOW_BYTES),
            Step::Home => 0,
            Step::End => len - 1,
        };
        let cursor = doc.bytes.cursor;
        let offset = doc.bytes.offset;
        let shown = if cursor < offset {
            line_of(cursor)
        } else if cursor >= offset.saturating_add(WINDOW_BYTES) {
            line_of(cursor).saturating_sub(WINDOW_BYTES - LINE_BYTES)
        } else {
            offset
        };
        if shown != offset {
            doc.bytes.offset = shown;
            doc.bytes.data = None;
        }
        self.fetch_bytes(at, out);
    }

    /// Selects the element at a typed offset, or says why it cannot.
    pub(super) fn go_to(&mut self, at: usize, text: &str, out: &mut Vec<Effect>) {
        let doc = &mut self.documents[at];
        let Some(len) = doc.len() else {
            return;
        };
        match parse_offset(text).filter(|o| *o < len) {
            Some(offset) => {
                doc.bytes.cursor = offset;
                if !in_window(doc.bytes.offset, offset) {
                    doc.bytes.offset = line_of(offset);
                    doc.bytes.data = None;
                }
                self.locate(
                    at,
                    LocateTarget::Offset { offset },
                    Purpose::Select { from_list: false },
                    out,
                );
                self.fetch_bytes(at, out);
            }
            None => {
                doc.notice = Some(format!(
                    "\u{201c}{}\u{201d} is not a byte of this file. Type 0 to {} or 0x0 to 0x{:X}.",
                    text.trim(),
                    len.saturating_sub(1),
                    len.saturating_sub(1)
                ));
            }
        }
    }

    /// Asks for the byte window if it is shown and not read or asked for.
    pub(super) fn fetch_bytes(&mut self, at: usize, out: &mut Vec<Effect>) {
        let doc = &mut self.documents[at];
        let Some(len) = doc.len() else {
            return;
        };
        let offset = doc.bytes.offset;
        if !doc.bytes.shown
            || doc.bytes.data.is_some()
            || doc.bytes.asked.is_some_and(|(_, o)| o == offset)
            || offset >= len
        {
            return;
        }
        // At most WINDOW_BYTES, which MAX_BYTE_RANGE covers.
        let count = u32::try_from((len - offset).min(WINDOW_BYTES)).unwrap_or(1);
        let document = doc.id;
        let id = self.send(
            document,
            Asked::Bytes { offset, len: count },
            out,
            move |document| Command::AnalysisBytes {
                document,
                offset,
                len: count,
            },
        );
        self.documents[at].bytes.asked = Some((id, offset));
    }
}
