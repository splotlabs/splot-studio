// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The single reference query for a document's current inspection context.

use studio_domain::analysis::{AnalysisFailure, Element, LocateTarget, RowFilter};
use studio_domain::layers::LayerDecodeSelection;
use studio_domain::protocol::Command;
use studio_domain::references::{ReferenceEvidence, ReferenceQuery, ReferenceSlotState};
use studio_domain::timeline::{TimelineSelection, TimelineTarget};

use super::{Analysis, Asked, Document, Effect, Input, Purpose};
use crate::preview::HistoryView;

pub(crate) const TAB: i32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct History {
    pub(super) view: Option<HistoryView>,
    pub(super) filter: RowFilter,
    pub(super) selection: LayerDecodeSelection,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct References {
    pending: Option<u64>,
    evidence: Option<Box<ReferenceEvidence>>,
    notice: Option<String>,
}

impl References {
    pub(crate) const fn busy(&self) -> bool {
        self.pending.is_some()
    }

    pub(crate) fn evidence(&self) -> Option<&ReferenceEvidence> {
        self.evidence.as_deref()
    }

    pub(crate) fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    pub(super) fn accept(&mut self, request: u64, evidence: Box<ReferenceEvidence>) {
        if self.pending == Some(request) {
            self.pending = None;
            self.evidence = Some(evidence);
        }
    }

    pub(super) fn fail(&mut self, request: u64, code: Option<AnalysisFailure>, message: String) {
        if self.pending == Some(request) {
            self.pending = None;
            self.notice = Some(match code {
                Some(AnalysisFailure::Cancelled) => "Reference inspection stopped.".to_owned(),
                Some(AnalysisFailure::Unsupported) => {
                    format!("Reference state is unavailable. {message}")
                }
                _ => format!("Studio could not read the reference state. {message}"),
            });
        }
    }
}

impl Document {
    pub(crate) fn reference_offset(&self) -> Option<u64> {
        if let Some(frame) = self.reference_timeline() {
            return Some(frame.coded_offset);
        }
        let Element::Obu { obu } = &self.selected.as_ref()?.row.element else {
            return None;
        };
        Some(obu.header.offset)
    }

    pub(crate) fn reference_timeline(&self) -> Option<&TimelineSelection> {
        if self.layer_decode_selection() == LayerDecodeSelection::Default {
            self.timeline.selection()
        } else {
            None
        }
    }
}

impl Analysis {
    pub(crate) fn save_reference_view(&mut self, view: Option<HistoryView>) {
        let Some(doc) = self.documents.get_mut(self.shown) else {
            return;
        };
        let selection = doc.layer_decode_selection();
        if let Some(place) = doc.history.get_mut(doc.at)
            && (doc.evidence.tab == TAB || place.reference.is_some())
        {
            place.reference = Some(History {
                view,
                filter: doc.filter,
                selection,
            });
        }
    }

    pub(crate) fn take_reference_view(&mut self) -> Option<(u64, HistoryView)> {
        self.reference_view.take()
    }

    pub(super) fn request_references(&mut self, at: usize, out: &mut Vec<Effect>) {
        let doc = &self.documents[at];
        if doc.evidence.tab != TAB || doc.selection_pending() || doc.references.busy() {
            return;
        }
        let Some(coded_offset) = doc.reference_offset() else {
            return;
        };
        let query = ReferenceQuery {
            coded_offset,
            selection: doc.layer_decode_selection(),
            generation: self.next_request + 1,
        };
        let request = self.send(doc.id, Asked::References(query), out, |document| {
            Command::AnalysisReferences { document, query }
        });
        self.documents[at].references = References {
            pending: Some(request),
            ..References::default()
        };
    }

    pub(super) fn cancel_references(&mut self, at: usize, out: &mut Vec<Effect>) {
        let doc = &mut self.documents[at];
        if let Some(reference) = doc.references.pending.take() {
            self.pending.remove(&reference);
            let document = doc.id;
            self.send(document, Asked::Cancel, out, |document| {
                Command::AnalysisCancelReferences {
                    document,
                    reference,
                }
            });
        }
        self.documents[at].references = References::default();
    }

    pub(super) fn stop_references(&mut self, at: usize, out: &mut Vec<Effect>) {
        if self.documents[at].references.busy() {
            self.cancel_references(at, out);
            self.documents[at].references.notice = Some("Reference inspection stopped.".to_owned());
        }
    }

    pub(super) fn follow_reference(&mut self, at: usize, index: i32, out: &mut Vec<Effect>) {
        let doc = &self.documents[at];
        if doc.evidence.tab != TAB || doc.selection_pending() {
            return;
        }
        let Some(offset) = usize::try_from(index)
            .ok()
            .zip(doc.references.evidence())
            .and_then(|(index, evidence)| follow_offset(evidence, index))
        else {
            return;
        };
        let document = doc.id;
        let indexed = doc.layer_decode_selection() == LayerDecodeSelection::Default
            && doc
                .timeline
                .page()
                .is_some_and(|page| page.complete && page.verified);
        self.cancel_references(at, out);
        if indexed {
            self.timeline_input(
                at,
                &Input::TimelineSelect(TimelineTarget::CodedOffset { offset }),
                out,
            );
        } else {
            self.timeline_picture = Some((document, None));
            self.locate(
                at,
                LocateTarget::Offset { offset },
                Purpose::Select { from_list: true },
                out,
            );
        }
    }
}

/// Follow a native mapping only when the input slot identifies its coded frame.
pub(crate) fn follow_offset(evidence: &ReferenceEvidence, index: usize) -> Option<u64> {
    let dependency = evidence.dependencies.get(index)?;
    let slot = evidence
        .slots
        .iter()
        .find(|slot| slot.slot == dependency.slot)?;
    match slot.before {
        ReferenceSlotState::Retained { frame, .. } => Some(frame.coded_offset),
        _ => None,
    }
}
