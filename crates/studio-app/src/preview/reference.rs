// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! A compact picture for an exact coded reference in the current inspection.

use studio_domain::preview::{PictureHeader, PictureOrigin};
use studio_domain::references::ReferenceFrame;

use super::{
    Command, Effect, Key, Picture, PictureBox, PictureFailure, PictureFrame, PictureTarget,
    PictureView, Preview, Purpose, Sent, text,
};

#[derive(Debug)]
pub(super) struct ReferencePreview {
    pub(super) key: Key,
    frame: ReferenceFrame,
    result: Option<Result<Picture, String>>,
}

impl ReferencePreview {
    fn matches_frame(&self, header: &PictureHeader) -> bool {
        // A first indexed output can come from a later display OBU. Match the
        // decoded sample source and its layers instead of the display OBU.
        header.origin == PictureOrigin::Document
            && header.output.is_some_and(|event| {
                event.coded_offset == Some(self.frame.coded_offset)
                    && event.sequence_offset == Some(self.frame.sequence_offset)
                    && event.extended_layer == self.frame.layers.extended
                    && event.embedded_layer == self.frame.layers.embedded
                    && event.temporal_layer == self.frame.layers.temporal
            })
    }
}

impl Preview {
    pub(crate) fn request_reference_preview(
        &mut self,
        target: PictureTarget,
        frame: ReferenceFrame,
        index: u64,
    ) -> Vec<Effect> {
        if !matches!(target, PictureTarget::Document { .. }) {
            return self.clear_reference_preview();
        }
        let key = Key {
            target,
            frame: PictureFrame::Index { index },
            view: PictureView::Fit,
            options: self.viewer.options,
            size: PictureBox::clamped(240, 136),
        };
        if self
            .reference
            .as_ref()
            .is_some_and(|preview| preview.key == key && preview.frame == frame)
        {
            return Vec::new();
        }
        let mut out = self.clear_reference_preview();
        self.playing = false;
        self.dirty.view = true;
        self.reference = Some(ReferencePreview {
            key,
            frame,
            result: self.broken.as_ref().map(|error| Err(error.clone())),
        });
        out.extend(self.pump());
        out
    }

    pub(crate) fn clear_reference_preview(&mut self) -> Vec<Effect> {
        self.dirty.view |= self.reference.take().is_some();
        if let Some(Sent::Picture {
            generation,
            purpose: Purpose::Reference,
            ..
        }) = self.inflight
        {
            self.inflight = None;
            return vec![Effect {
                request_id: generation,
                command: Command::StopPlayback,
            }];
        }
        Vec::new()
    }

    pub(crate) fn reference_preview(&self) -> Option<Result<&Picture, &str>> {
        match self.reference.as_ref()?.result.as_ref()? {
            Ok(picture) => Some(Ok(picture)),
            Err(error) => Some(Err(error)),
        }
    }

    pub(super) fn next_reference(&self) -> Option<Key> {
        self.reference
            .as_ref()
            .filter(|request| request.result.is_none())
            .map(|request| request.key.clone())
    }

    pub(super) fn show_reference(&mut self, key: &Key, picture: Picture) {
        if let Some(reference) = &mut self.reference
            && reference.key == *key
        {
            reference.result = Some(if reference.matches_frame(&picture.header) {
                Ok(picture)
            } else {
                Err("The preview does not match the selected coded reference.".to_owned())
            });
            self.dirty.view = true;
        }
    }

    pub(super) fn reference_failed(&mut self, code: PictureFailure, message: &str) {
        self.set_reference_error(if code == PictureFailure::SourceChanged {
            text::DOCUMENT_CHANGED.to_owned()
        } else {
            text::picture_failure(code, message, false)
        });
    }

    pub(super) fn set_reference_error(&mut self, message: String) {
        if let Some(reference) = &mut self.reference
            && reference.result.is_none()
        {
            reference.result = Some(Err(message));
            self.dirty.view = true;
        }
    }
}
