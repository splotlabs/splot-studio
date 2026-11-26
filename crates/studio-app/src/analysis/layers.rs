// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Per-document layer inspection and decoder request state.

use studio_domain::analysis::{Element, ScanOutcome};
use studio_domain::layers::{
    LayerDecodeSelection, LayerDecodeStatus, LayerEvidence, LayerFact, LayerRow,
};

use super::{Analysis, Asked, Command, Document, Effect, Event, Input};

mod details;
pub(crate) use details::{Detail, selection_name};

const PAGE_ROWS: u32 = 8;
pub(super) const TAB: i32 = 3;

/// Controls in the layer inspector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerInput {
    /// Move to the previous or next evidence page.
    Page(i32),
    /// Show the source OBU of a visible record.
    Origin(i32),
    /// Request the operating point of a visible declaration.
    Decode(i32),
    /// Request the default operating point.
    DecodeDefault,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Anchor {
    element: u64,
    offset: u64,
    selection: LayerDecodeSelection,
    scanning: bool,
}

impl Anchor {
    fn same_target(self, other: Self) -> bool {
        self.element == other.element
            && self.offset == other.offset
            && self.selection == other.selection
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Request {
    Inspect(Anchor),
    Page { inspection: u64, first: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Inspection {
    id: u64,
    pub evidence: Box<LayerEvidence>,
    pub total: u64,
    pub first: u64,
    pub count: u64,
    pub rows: Vec<Detail>,
    pub previous: Vec<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LayerInspector {
    pub shown: bool,
    pub selection: LayerDecodeSelection,
    pub decode: Option<LayerDecodeStatus>,
    pub current: Option<Inspection>,
    pub error: Option<String>,
    key: Option<Anchor>,
    pending: Option<(u64, Request)>,
    cancelling: bool,
}

impl LayerInspector {
    pub(crate) const fn loading(&self) -> bool {
        self.pending.is_some()
    }

    pub(super) fn fail(&mut self, request_id: u64, request: Request, message: String) {
        if self
            .pending
            .is_some_and(|(pending, _)| pending == request_id)
        {
            self.pending = None;
            self.cancelling = false;
            if self.accepts(request) {
                self.error = Some(message);
            }
        }
    }

    fn accepts(&self, request: Request) -> bool {
        match request {
            Request::Inspect(key) => self.key == Some(key),
            Request::Page { inspection, .. } => self
                .current
                .as_ref()
                .is_some_and(|current| current.id == inspection),
        }
    }

    fn select(&mut self, selection: LayerDecodeSelection) -> bool {
        if selection == self.selection {
            return false;
        }
        self.selection = selection;
        self.decode = None;
        true
    }
}

fn anchor(document: &Document) -> Option<Anchor> {
    if !document.layers.shown {
        return None;
    }
    let summary = document.summary()?;
    let selected = document.selected()?;
    let Element::Obu { obu } = &selected.row.element else {
        return None;
    };
    Some(Anchor {
        element: selected.row.index,
        offset: obu.header.offset,
        selection: document.layers.selection,
        scanning: matches!(summary.outcome, ScanOutcome::Scanning),
    })
}

impl Analysis {
    /// Accepts playback feedback for the current source read and requested operating point.
    pub fn update_layer_decode_status(
        &mut self,
        document: u64,
        selection: LayerDecodeSelection,
        status: LayerDecodeStatus,
    ) {
        if let Some(at) = self.index_of(document) {
            let state = &mut self.documents[at].layers;
            if state.selection == selection && state.decode.as_ref() != Some(&status) {
                state.decode = Some(status);
                self.dirty = true;
            }
        }
    }

    pub(super) fn layer_input(&mut self, at: usize, input: LayerInput, out: &mut Vec<Effect>) {
        let state = &mut self.documents[at].layers;
        match input {
            LayerInput::Origin(index) if state.shown => {
                let origin = find_row(state, index).and_then(|row| row.origin);
                if let Some(origin) = origin {
                    state.shown = false;
                    self.cancel_timeline_location(at);
                    self.evidence_input(at, Input::EvidenceView(1), out);
                    self.go_to(at, &origin.to_string(), out);
                }
            }
            LayerInput::Decode(index) if state.shown => {
                if let Some(selection) = find_row(state, index).and_then(|row| row.request)
                    && state.select(selection)
                {
                    self.invalidate_timeline_policy(at, out);
                }
            }
            LayerInput::DecodeDefault
                if state.shown && state.select(LayerDecodeSelection::Default) =>
            {
                self.invalidate_timeline_policy(at, out);
            }
            LayerInput::Page(step) if state.shown && !state.loading() => {
                if let Some(page) = &state.current {
                    let first = if step < 0 {
                        page.previous.last().copied()
                    } else {
                        page.first
                            .checked_add(page.count)
                            .filter(|&first| first != page.first && first < page.total)
                    };
                    if let Some(first) = first {
                        self.request_layer_page(at, first, out);
                    }
                }
            }
            _ => {}
        }
    }

    pub(super) fn ensure_layers(&mut self, out: &mut Vec<Effect>) {
        let at = self.shown;
        let Some(doc) = self.documents.get_mut(at) else {
            return;
        };
        let wanted = anchor(doc);
        let document = doc.id;
        let state = &mut doc.layers;
        let refresh = match (wanted, state.key) {
            (Some(wanted), Some(key)) => {
                !wanted.same_target(key)
                    || (state.error.is_some() && key.scanning && !wanted.scanning)
            }
            (None, None) => false,
            _ => true,
        };
        if refresh {
            state.key = None;
            state.current = None;
            state.error = None;
        }
        if let Some((inspection, Request::Inspect(_))) = state.pending
            && state.key.is_none()
            && !state.cancelling
        {
            state.cancelling = true;
            self.send(document, Asked::CancelLayers, out, |document| {
                Command::AnalysisCancelLayers {
                    document,
                    inspection,
                }
            });
            return;
        }
        let Some(wanted) = wanted.filter(|_| !state.loading() && state.key.is_none()) else {
            return;
        };
        let selection = wanted.selection;
        let element = wanted.element;
        state.key = Some(wanted);
        let asked = Request::Inspect(wanted);
        let request = self.send(document, Asked::Layers(asked), out, |document| {
            Command::AnalysisLayers {
                document,
                element,
                selection,
            }
        });
        self.documents[at].layers.pending = Some((request, asked));
    }

    pub(super) fn layer_event(
        &mut self,
        at: usize,
        request_id: u64,
        asked: Request,
        event: Event,
        out: &mut Vec<Effect>,
    ) {
        let doc = &mut self.documents[at];
        let selected = anchor(doc);
        let state = &mut doc.layers;
        if state.pending != Some((request_id, asked)) {
            return;
        }
        state.pending = None;
        state.cancelling = false;
        if !selected
            .zip(state.key)
            .is_some_and(|(selected, key)| selected.same_target(key))
            || !state.accepts(asked)
        {
            return;
        }
        match (asked, event) {
            (
                Request::Inspect(key),
                Event::AnalysisLayersReady {
                    evidence, total, ..
                },
            ) if evidence.element == key.element
                && evidence.offset == key.offset
                && evidence.selection == key.selection =>
            {
                state.current = Some(Inspection {
                    id: request_id,
                    evidence,
                    total,
                    first: 0,
                    count: 0,
                    rows: Vec::new(),
                    previous: Vec::new(),
                });
                if total != 0 {
                    self.request_layer_page(at, 0, out);
                }
            }
            (
                Request::Page { inspection, first },
                Event::AnalysisLayerPage {
                    inspection: answered_inspection,
                    first: answered_first,
                    total,
                    mut rows,
                    ..
                },
            ) if inspection == answered_inspection && first == answered_first => {
                if let Some(page) = &mut state.current
                    && total == page.total
                    && valid_page(first, total, &rows)
                {
                    if page.count != 0 {
                        if first < page.first {
                            page.previous.pop();
                        } else if first > page.first {
                            page.previous.push(page.first);
                        }
                    }
                    page.first = first;
                    page.count = rows.len() as u64;
                    // Put the effective state first when its records are on this page.
                    rows.sort_by_key(|row| {
                        !matches!(row.fact, LayerFact::ContentInterpretation { .. })
                    });
                    page.rows = rows.into_iter().flat_map(details::detail_rows).collect();
                } else {
                    state.error =
                        Some("The engine returned an incorrect page of layer records.".into());
                }
            }
            _ => state.error = Some("The engine returned mismatched layer evidence.".into()),
        }
    }

    fn request_layer_page(&mut self, at: usize, first: u64, out: &mut Vec<Effect>) {
        let doc = &self.documents[at];
        let Some(inspection) = doc.layers.current.as_ref().map(|page| page.id) else {
            return;
        };
        let asked = Request::Page { inspection, first };
        let request = self.send(doc.id, Asked::Layers(asked), out, |document| {
            Command::AnalysisLayerPage {
                document,
                inspection,
                first,
                count: PAGE_ROWS,
            }
        });
        let state = &mut self.documents[at].layers;
        state.pending = Some((request, asked));
        state.error = None;
    }
}

fn find_row(state: &LayerInspector, index: i32) -> Option<&Detail> {
    let index = usize::try_from(index).ok()?;
    state.current.as_ref()?.rows.get(index)
}

fn valid_page(first: u64, total: u64, rows: &[LayerRow]) -> bool {
    let len = rows.len() as u64;
    len > 0 && len <= total.saturating_sub(first).min(u64::from(PAGE_ROWS))
}
