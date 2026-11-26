// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The single reference query for a document's current inspection context.

use studio_domain::analysis::{AnalysisFailure, Element, LocateTarget, RowFilter};
use studio_domain::layers::LayerDecodeSelection;
use studio_domain::protocol::Command;
use studio_domain::references::{
    ReferenceEvidence, ReferenceFrame, ReferenceQuery, ReferenceSlotState,
};
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
    preview: Option<PreviewLookup>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PreviewLookup {
    Pending(u64),
    Ready { frame: ReferenceFrame, index: u64 },
    Unavailable(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ReferencePreviewQuery {
    pub(super) scope: ReferenceQuery,
    pub(super) scan: u64,
    pub(super) frame: ReferenceFrame,
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

    pub(super) fn fail(&mut self, request: u64, code: Option<AnalysisFailure>, message: &str) {
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
    pub(crate) fn coded_offset(&self) -> Option<u64> {
        if let Some(frame) = self.coded_timeline() {
            return Some(frame.coded_offset);
        }
        let Element::Obu { obu } = &self.selected.as_ref()?.row.element else {
            return None;
        };
        Some(obu.header.offset)
    }

    pub(crate) fn coded_timeline(&self) -> Option<&TimelineSelection> {
        self.timeline.selection_for(self.layer_decode_selection())
    }

    pub(crate) fn reference_evidence(&self) -> Option<&ReferenceEvidence> {
        let offset = self.coded_offset()?;
        self.references.evidence().filter(|evidence| {
            evidence.query.coded_offset == offset
                && evidence.query.selection == self.layer_decode_selection()
        })
    }

    pub(crate) fn reference_preview_picture(&self) -> Option<(ReferenceFrame, u64)> {
        match self.references.preview {
            Some(PreviewLookup::Ready { frame, index }) => Some((frame, index)),
            _ => None,
        }
    }

    pub(crate) fn reference_preview_notice(&self) -> Option<&str> {
        match self.references.preview.as_ref()? {
            PreviewLookup::Pending(_) => Some("Finding the reference's indexed output…"),
            PreviewLookup::Unavailable(message) => Some(message),
            PreviewLookup::Ready { .. } => None,
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
        let Some(coded_offset) = doc.coded_offset() else {
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
        self.cancel_reference_preview(at);
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

    pub(super) fn cancel_reference_preview(&mut self, at: usize) {
        if let Some(PreviewLookup::Pending(request)) = self.documents[at].references.preview.take()
        {
            self.pending.remove(&request);
        }
    }

    pub(super) fn request_reference_preview(
        &mut self,
        at: usize,
        frame: ReferenceFrame,
        out: &mut Vec<Effect>,
    ) {
        self.cancel_reference_preview(at);
        let doc = &mut self.documents[at];
        let Some(scope) = doc.reference_evidence().map(|evidence| evidence.query) else {
            return;
        };
        let Some(scan) = doc.timeline.scan_for(scope.selection) else {
            doc.references.preview = Some(PreviewLookup::Unavailable(
                "Index the timeline for this operating point to preview the reference.".to_owned(),
            ));
            return;
        };
        let query = ReferencePreviewQuery { scope, scan, frame };
        let document = doc.id;
        let request = self.send(document, Asked::ReferencePreview(query), out, |document| {
            Command::AnalysisTimelineLocate {
                document,
                scan,
                target: TimelineTarget::CodedOffset {
                    offset: frame.coded_offset,
                },
            }
        });
        self.documents[at].references.preview = Some(PreviewLookup::Pending(request));
    }

    pub(super) fn accept_reference_preview(
        &mut self,
        at: usize,
        request: u64,
        query: ReferencePreviewQuery,
        scan: u64,
        selection: TimelineSelection,
    ) {
        let doc = &mut self.documents[at];
        if doc.references.preview != Some(PreviewLookup::Pending(request)) {
            return;
        }
        if doc.timeline.scan_for(query.scope.selection) != Some(scan)
            || query.scan != scan
            || doc.reference_evidence().map(|evidence| evidence.query) != Some(query.scope)
            || selection.coded_offset != query.frame.coded_offset
        {
            doc.references.preview = Some(PreviewLookup::Unavailable(
                "The timeline result does not match the selected reference.".to_owned(),
            ));
            return;
        }
        doc.references.preview = Some(selection.output_ordinal.map_or_else(
            || {
                let message = if doc.timeline.has_verified_index(query.scope.selection) {
                    "The timeline records no output for this reference."
                } else {
                    "The indexed range has no output for this reference yet."
                };
                PreviewLookup::Unavailable(message.to_owned())
            },
            |index| PreviewLookup::Ready {
                frame: query.frame,
                index,
            },
        ));
    }

    pub(super) fn fail_reference_preview(&mut self, at: usize, request: u64, message: String) {
        let reference = &mut self.documents[at].references;
        if reference.preview == Some(PreviewLookup::Pending(request)) {
            reference.preview = Some(PreviewLookup::Unavailable(message));
        }
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
        let Some(frame) = usize::try_from(index)
            .ok()
            .zip(doc.reference_evidence())
            .and_then(|(index, evidence)| {
                retained_frame(evidence, evidence.dependencies.get(index)?.slot)
            })
        else {
            return;
        };
        self.follow_reference_frame(at, frame, out);
    }

    pub(super) fn follow_reference_frame(
        &mut self,
        at: usize,
        frame: ReferenceFrame,
        out: &mut Vec<Effect>,
    ) {
        let doc = &self.documents[at];
        let offset = frame.coded_offset;
        let document = doc.id;
        let indexed = doc
            .timeline
            .has_verified_index(doc.layer_decode_selection());
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
    retained_frame(evidence, evidence.dependencies.get(index)?.slot).map(|frame| frame.coded_offset)
}

pub(super) fn retained_frame(
    evidence: &ReferenceEvidence,
    input_slot: u8,
) -> Option<ReferenceFrame> {
    let slot = evidence.slots.iter().find(|slot| slot.slot == input_slot)?;
    match slot.before {
        ReferenceSlotState::Retained { frame, .. } => Some(frame),
        _ => None,
    }
}
