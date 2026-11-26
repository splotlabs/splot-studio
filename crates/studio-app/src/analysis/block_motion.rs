// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The selected block's prediction and its verified reference targets.

use studio_domain::blocks::{
    BlockData, BlockFrame, BlockId, BlockMotion, BlockPrediction, BlockReference,
};
use studio_domain::references::{ReferenceEvidence, ReferenceFrame, ReferenceQuery};

use super::{Analysis, Document, Effect, block_overlay::Key, references::retained_frame};

/// An action for the selected block's prediction and motion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockMotionInput {
    /// Read the input slots of the block's coded frame.
    InspectReferences,
    /// Preview the reference at a predictor position.
    Preview(i32),
    /// Follow the reference at a predictor position.
    Follow(i32),
    /// Show or hide motion on the picture.
    Overlay(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PreviewSelection {
    pub(super) document: u64,
    pub(super) frame: BlockFrame,
    pub(super) block: BlockId,
    pub(super) position: usize,
}

pub(crate) struct SelectedMotion<'a> {
    pub(crate) document: &'a Document,
    pub(crate) frame: BlockFrame,
    pub(crate) block: BlockId,
    pub(crate) prediction: Option<&'a BlockPrediction>,
    pub(crate) motion: Option<BlockMotion>,
    pub(crate) references: Option<&'a ReferenceEvidence>,
}

impl SelectedMotion<'_> {
    pub(crate) fn retained_reference(
        &self,
        index: usize,
    ) -> Option<(ReferenceQuery, ReferenceFrame)> {
        let Some(BlockPrediction::Inter { references, .. }) = self.prediction else {
            return None;
        };
        let BlockReference::Frame { slot, .. } = (*references.get(index)?)? else {
            return None;
        };
        let evidence = self.references?;
        Some((
            evidence.query,
            retained_frame(evidence, u8::try_from(slot).ok()?)?,
        ))
    }
}

impl Analysis {
    pub(crate) fn selected_motion(&self) -> Option<SelectedMotion<'_>> {
        let document = self.shown()?;
        let selected = self.overlay.selected?;
        let Key::Coding(block) = selected.key else {
            return None;
        };
        let record = self.overlay.selected_record()?;
        let BlockData::Coding { prediction, .. } = &record.data else {
            return None;
        };
        let references = document
            .reference_evidence_for(selected.frame.coded_offset)
            .filter(|evidence| {
                // This is coded layer identity. A display OBU can show the same
                // reconstruction in a different layer.
                evidence.frame.sequence_offset == selected.frame.sequence_offset
                    && evidence.frame.layers == selected.frame.layers
            });
        Some(SelectedMotion {
            document,
            frame: selected.frame,
            block,
            prediction: prediction.as_ref(),
            motion: self.overlay.selected_motion(),
            references,
        })
    }

    pub(crate) fn motion_preview_position(&self) -> Option<usize> {
        let preview = self.motion_preview?;
        let selected = self.overlay.selected?;
        (self.shown().is_some_and(|doc| doc.id() == preview.document)
            && selected.frame == preview.frame
            && selected.key == Key::Coding(preview.block))
        .then_some(preview.position)
    }

    pub(crate) const fn show_motion(&self) -> bool {
        self.show_motion
    }

    pub(super) fn block_motion_input(
        &mut self,
        at: usize,
        input: BlockMotionInput,
        out: &mut Vec<Effect>,
    ) {
        let Some(selected) = self.selected_motion() else {
            return;
        };
        match input {
            BlockMotionInput::InspectReferences => {
                if matches!(selected.prediction, Some(BlockPrediction::Inter { references, .. })
                    if references.iter().any(|reference| matches!(reference, Some(BlockReference::Frame { .. }))))
                {
                    self.request_references_for(at, selected.frame.coded_offset, out);
                }
            }
            BlockMotionInput::Preview(index) | BlockMotionInput::Follow(index) => {
                let Some((position, (scope, frame))) = usize::try_from(index)
                    .ok()
                    .and_then(|position| Some((position, selected.retained_reference(position)?)))
                else {
                    return;
                };
                if matches!(input, BlockMotionInput::Preview(_)) {
                    self.motion_preview = Some(PreviewSelection {
                        document: selected.document.id(),
                        frame: selected.frame,
                        block: selected.block,
                        position,
                    });
                    self.request_reference_preview(at, scope, frame, out);
                } else {
                    self.motion_preview = None;
                    self.follow_reference_frame(at, frame, out);
                }
            }
            BlockMotionInput::Overlay(show) => self.show_motion = show,
        }
    }

    pub(crate) fn discard_other_motion_preview(&mut self) {
        if let Some(preview) = self.motion_preview
            && self.motion_preview_position().is_none()
        {
            self.motion_preview = None;
            if let Some(at) = self.index_of(preview.document) {
                self.cancel_reference_preview(at);
            }
        }
    }
}
