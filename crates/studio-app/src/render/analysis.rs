// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Pushes the Analyze workspace into the window's `Analysis` global: only
//! the visible rows and the one byte window, as display strings.

use slint::{ComponentHandle, ModelRc, SharedString};
use studio_domain::analysis::{Element, Span};

use super::model;
use crate::analysis::{
    Analysis, Bytes, Document, LINE_BYTES, Phase, Selected, VISIBLE_ROWS, WINDOW_BYTES, extent,
};
use crate::present::analysis::{self as text, Property as PropertyText};
use crate::present::file_name;
use crate::ui::{
    Analysis as Global, AnalysisState, AppWindow, ByteCell, ByteLine, ByteRegion, Legend, NavRow,
    Property,
};

/// Pushes the whole Analyze workspace.
///
/// Public so renderer snapshots and UI tests can show a state.
pub fn render_analysis(window: &AppWindow, analysis: &Analysis) {
    let global = window.global::<Global>();
    let documents: Vec<SharedString> = analysis
        .documents()
        .iter()
        .map(|d| file_name(d.path()).into())
        .collect();
    global.set_documents(model(documents));
    global.set_document_index(int(analysis.shown_index() as u64));
    let notice = analysis
        .shown()
        .and_then(Document::notice)
        .or_else(|| analysis.notice())
        .unwrap_or_default();
    global.set_notice(notice.into());
    let Some(doc) = analysis.shown() else {
        global.set_state(AnalysisState::Empty);
        global.set_message(
            "IVF and Annex B (.obu) files. Studio reads their OBU envelopes and headers and \
             never changes the file."
                .into(),
        );
        clear(&global);
        return;
    };
    let name = file_name(doc.path());
    let summary = match doc.phase() {
        Phase::Opening => {
            global.set_state(AnalysisState::Opening);
            global.set_message(format!("Reading {name}. Nothing is decoded.").into());
            None
        }
        Phase::Cancelling => {
            global.set_state(AnalysisState::Opening);
            global.set_message(format!("Stopping the read of {name}…").into());
            None
        }
        Phase::Failed { message, .. } => {
            global.set_state(AnalysisState::Failed);
            global.set_message(format!("{name}: {message}").into());
            None
        }
        Phase::Open(summary) => {
            global.set_state(AnalysisState::Open);
            Some(summary)
        }
    };
    let Some(summary) = summary else {
        clear(&global);
        return;
    };
    global.set_summary_line(text::summary_line(summary).into());
    let (outcome, problem) = text::outcome(summary);
    global.set_outcome_line(outcome.into());
    global.set_outcome_problem(problem);

    let filter = doc.filter();
    let kinds = analysis.type_choices();
    global.set_type_labels(model(
        kinds
            .iter()
            .map(|k| text::kind_label(*k, Some(summary)).into())
            .collect(),
    ));
    global.set_type_index(index_of(&kinds, &filter.kind));
    let layers = analysis.layer_choices();
    global.set_layer_labels(model(
        layers
            .iter()
            .map(|l| text::layer_label(*l, Some(summary)).into())
            .collect(),
    ));
    global.set_layer_index(index_of(&layers, &filter.layer));
    global.set_filtered(!filter.is_all());
    global.set_rows_line(text::rows_line(filter, doc.total(), summary.elements).into());
    global.set_hidden_note(text::hidden_note(doc.selected()).into());
    global.set_rows(rows(doc));
    global.set_total_rows(int(doc.total().unwrap_or(0)));
    global.set_top_row(int(doc.top()));
    global.set_can_back(doc.can_go_back());
    global.set_can_forward(doc.can_go_forward());

    let (title, properties) = text::properties(summary, doc.origin(), doc.path(), doc.selected());
    global.set_inspector_title(title.into());
    global.set_properties(model(properties.into_iter().map(property).collect()));
    let selected = doc.selected();
    global.set_selected_line(
        selected
            .map_or_else(String::new, |s| text::row_line(&s.row))
            .into(),
    );
    global.set_selected_offset(
        selected
            .map_or_else(String::new, |s| extent(&s.row.element).0.to_string())
            .into(),
    );
    render_bytes(&global, doc.bytes(), summary.fingerprint.len, selected);
}

/// Empties what only an open document has.
fn clear(global: &Global<'_>) {
    global.set_summary_line(SharedString::new());
    global.set_outcome_line(SharedString::new());
    global.set_rows(model(Vec::new()));
    global.set_total_rows(0);
    global.set_top_row(0);
    global.set_properties(model(Vec::new()));
    global.set_byte_lines(model(Vec::new()));
    global.set_selected_line(SharedString::new());
    global.set_selected_offset(SharedString::new());
    global.set_hidden_note(SharedString::new());
}

fn int(value: u64) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

fn index_of<T: PartialEq>(all: &[T], value: &T) -> i32 {
    all.iter()
        .position(|v| v == value)
        .and_then(|i| i32::try_from(i).ok())
        .unwrap_or(0)
}

fn property(p: PropertyText) -> Property {
    Property {
        label: p.label.into(),
        value: p.value.into(),
        note: p.note.into(),
    }
}

/// The visible rows: those that arrived, and a loading row for each that
/// has not.
fn rows(doc: &Document) -> ModelRc<NavRow> {
    let selected = doc.selected().map(|s| s.row.index);
    let end = doc
        .total()
        .map_or(doc.top() + 1, |t| t.min(doc.top() + VISIBLE_ROWS));
    model(
        (doc.top()..end)
            .map(|position| match doc.row(position) {
                Some(row) => NavRow {
                    loaded: true,
                    depth: i32::from(row.parent.is_some()),
                    title: text::row_title(row).into(),
                    detail: text::row_detail(row).into(),
                    offset: text::row_offset(row).into(),
                    selected: selected == Some(row.index),
                    accessible: text::row_line(row).into(),
                },
                None => NavRow {
                    loaded: false,
                    depth: 0,
                    title: SharedString::new(),
                    detail: SharedString::new(),
                    offset: SharedString::new(),
                    selected: false,
                    accessible: "Loading".into(),
                },
            })
            .collect(),
    )
}

/// Which part of the selected element `offset` is in, from its spans.
fn region(element: Option<&Element>, offset: u64) -> ByteRegion {
    let inside = |span: Span| (span.offset..span.offset.saturating_add(span.len)).contains(&offset);
    match element {
        Some(Element::IvfHeader { span }) if inside(*span) => ByteRegion::FileHeader,
        Some(Element::Packet { packet }) if inside(packet.header) => ByteRegion::RecordHeader,
        Some(Element::Packet { packet }) if inside(packet.payload) => ByteRegion::Payload,
        Some(Element::Obu { obu }) if inside(obu.length_prefix) => ByteRegion::Prefix,
        Some(Element::Obu { obu }) if inside(obu.header) => ByteRegion::Header,
        Some(Element::Obu { obu }) if inside(obu.payload) => ByteRegion::Payload,
        Some(Element::Unparsed { span, .. }) if inside(*span) => ByteRegion::Unparsed,
        _ => ByteRegion::Outside,
    }
}

/// The legend for the selected element: its parts that the engine
/// located, and where each span came from.
fn legend(element: Option<&Element>) -> Vec<Legend> {
    let item = |label: &str, region| Legend {
        label: label.into(),
        region,
    };
    match element {
        None => Vec::new(),
        Some(Element::IvfHeader { .. }) => vec![item("IVF file header", ByteRegion::FileHeader)],
        Some(Element::Packet { .. }) => vec![
            item("Record header (derived)", ByteRegion::RecordHeader),
            item("Payload (located)", ByteRegion::Payload),
        ],
        Some(Element::Obu { .. }) => vec![
            item("Length prefix (derived)", ByteRegion::Prefix),
            item("Header (located)", ByteRegion::Header),
            item("Payload (located)", ByteRegion::Payload),
        ],
        Some(Element::Unparsed { .. }) => vec![item("Unparsed", ByteRegion::Unparsed)],
    }
}

const fn region_name(region: ByteRegion) -> &'static str {
    match region {
        ByteRegion::Outside => "outside the selection",
        ByteRegion::Prefix => "in the selection's length prefix",
        ByteRegion::Header => "in the selection's header",
        ByteRegion::Payload => "in the selection's payload",
        ByteRegion::RecordHeader => "in the selection's record header",
        ByteRegion::FileHeader => "in the IVF file header",
        ByteRegion::Unparsed => "in the selected unparsed region",
    }
}

fn render_bytes(global: &Global<'_>, bytes: &Bytes, len: u64, selected: Option<&Selected>) {
    let element = selected.map(|s| &s.row.element);
    global.set_bytes_shown(bytes.shown);
    global.set_legend(model(legend(element)));
    let end = bytes.offset.saturating_add(WINDOW_BYTES).min(len);
    global.set_bytes_title(
        match &bytes.data {
            _ if len == 0 => "The file is empty.".to_owned(),
            Some(_) => format!(
                "0x{:X} to 0x{:X} of {}",
                bytes.offset,
                end.saturating_sub(1),
                text::byte_count(len)
            ),
            None => "Reading bytes…".to_owned(),
        }
        .into(),
    );
    let data = bytes.data.as_deref().unwrap_or_default();
    let cursor = data
        .get(usize::try_from(bytes.cursor.saturating_sub(bytes.offset)).unwrap_or(usize::MAX))
        .filter(|_| bytes.cursor >= bytes.offset);
    global.set_cursor_line(
        match cursor {
            Some(value) => format!(
                "Byte {} is 0x{value:02X}, {}.",
                text::offset(bytes.cursor),
                region_name(region(element, bytes.cursor))
            ),
            None => format!("Byte {}.", text::offset(bytes.cursor)),
        }
        .into(),
    );
    // 16, which every target's usize holds.
    let line = usize::try_from(LINE_BYTES).unwrap_or(16);
    let lines: Vec<ByteLine> = data
        .chunks(line)
        .zip((bytes.offset..).step_by(line))
        .map(|(chunk, start)| ByteLine {
            offset: format!("{start:08X}").into(),
            cells: model(
                chunk
                    .iter()
                    .zip(start..)
                    .map(|(value, at)| ByteCell {
                        text: format!("{value:02X}").into(),
                        region: region(element, at),
                        cursor: at == bytes.cursor,
                    })
                    .collect(),
            ),
            ascii: chunk
                .iter()
                .map(|b| {
                    if b.is_ascii_graphic() || *b == b' ' {
                        char::from(*b)
                    } else {
                        '.'
                    }
                })
                .collect::<String>()
                .into(),
        })
        .collect();
    global.set_byte_lines(model(lines));
}
