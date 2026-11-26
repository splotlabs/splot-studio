// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Playback of an independently opened Analyze document.

use studio_domain::layers::{LayerDecodeSelection, LayerDecodeStatus, LayerScope};
use studio_domain::preview::{
    OutputEvent, PictureFailure, PictureFrame, PictureHeader, PictureOrigin, PictureStage,
    PictureTarget,
};
use studio_domain::protocol::Command;

use super::{Effect, Preview, Sent, StageView, Subject, Want};

impl Preview {
    pub(super) const fn document_id(&self) -> Option<u64> {
        match self.scene.stage {
            Some(Subject::Document { document, .. }) => Some(document),
            _ => None,
        }
    }

    pub(crate) fn take_layer_decode_feedback(
        &mut self,
    ) -> Option<(u64, LayerDecodeSelection, LayerDecodeStatus)> {
        self.layer_decode_feedback.take()
    }

    pub(super) fn accept_layer_decode(&mut self, shown: &super::Shown) {
        let Some((target, output)) = shown.target.as_ref().zip(shown.picture.header.output) else {
            return;
        };
        let PictureTarget::Document {
            document,
            selection: selection @ LayerDecodeSelection::OperatingPoint { scope, .. },
            ..
        } = target
        else {
            return;
        };
        if !self.on_side(target)
            || matches!(scope, LayerScope::Extended { id } if *id != output.extended_layer)
        {
            return;
        }
        self.layer_decode_feedback = Some((*document, *selection, LayerDecodeStatus::Active));
    }

    pub(super) fn reject_layer_decode(
        &mut self,
        target: &PictureTarget,
        code: PictureFailure,
        message: &str,
    ) {
        let PictureTarget::Document {
            document,
            selection: selection @ LayerDecodeSelection::OperatingPoint { .. },
            ..
        } = target
        else {
            return;
        };
        if matches!(
            code,
            PictureFailure::Unreadable
                | PictureFailure::Unsupported
                | PictureFailure::SourceChanged
        ) || (code == PictureFailure::OutOfRange && self.current_frame() == Some(0))
        {
            self.layer_decode_feedback = Some((
                *document,
                *selection,
                LayerDecodeStatus::Rejected(message.to_owned()),
            ));
        }
    }

    pub(super) fn reset_layer_decode_feedback(&mut self) {
        if let Some(Subject::Document {
            document,
            selection: selection @ LayerDecodeSelection::OperatingPoint { .. },
            ..
        }) = &self.scene.stage
        {
            self.layer_decode_feedback = Some((*document, *selection, LayerDecodeStatus::Pending));
        }
    }

    pub(super) fn release_document(&mut self, out: &mut Vec<Effect>) {
        if let Some(document) = self.document_id() {
            if self.document_request().is_some() {
                self.inflight = None;
            }
            out.push(Effect {
                request_id: self.take_generation(),
                command: Command::CloseDocumentPlayback { document },
            });
        }
    }

    fn document_request(&self) -> Option<(u64, &PictureTarget)> {
        let (generation, target) = match self.inflight.as_ref()? {
            Sent::Picture {
                generation, key, ..
            } => (*generation, &key.target),
            Sent::Play {
                generation, target, ..
            } => (*generation, target),
        };
        matches!(target, PictureTarget::Document { .. }).then_some((generation, target))
    }

    pub(super) fn document_request_generation(&self) -> Option<u64> {
        if matches!(self.inflight, Some(Sent::Play { stopping: true, .. })) {
            return None;
        }
        self.document_request().map(|(generation, _)| generation)
    }

    pub(super) fn valid_document_answer(&self, header: &PictureHeader) -> bool {
        let Some((generation, PictureTarget::Document { fingerprint, .. })) =
            self.document_request()
        else {
            return true;
        };
        if generation != header.generation {
            return true;
        }
        let Some(event) = header.output else {
            return false;
        };
        let len = fingerprint.len;
        let spans_match = event.unit_len > 0
            && event
                .unit_offset
                .checked_add(event.unit_len)
                .is_some_and(|end| end <= len)
            && [
                event.coded_offset,
                event.showing_offset,
                event.sequence_offset,
            ]
            .into_iter()
            .flatten()
            .all(|offset| offset < len);
        let frame_matches = match &self.inflight {
            Some(Sent::Picture { key, .. }) => {
                key.options == header.options
                    && super::viewer::valid_geometry(header, key.view, key.size)
                    && key.frame
                        == PictureFrame::Index {
                            index: header.frame,
                        }
            }
            Some(Sent::Play {
                options,
                view,
                size,
                ..
            }) => *options == header.options && super::viewer::valid_geometry(header, *view, *size),
            None => false,
        };
        header.origin == PictureOrigin::Document
            && event.ordinal == header.frame
            && spans_match
            && frame_matches
    }

    pub(super) fn start_document_request(&mut self) {
        self.viewer.clear_sample();
        self.document_progress.clear();
        self.document_notice.clear();
        self.stage_failure = None;
        self.failed = None;
        let position = self.current_frame();
        if self
            .shown
            .as_ref()
            .is_some_and(|s| Some(s.picture.header.frame) != position)
        {
            self.shown = None;
            self.dirty.picture = true;
        }
        self.dirty.view = true;
    }

    pub(super) fn document_decoding(&self) -> bool {
        self.viewer.pending_sample().is_some()
            || self.document_request_generation().is_some()
                && (self.shown.is_none() || !self.document_progress.is_empty())
    }

    pub(super) fn cancel_document_work(&mut self) -> Vec<Effect> {
        if self.viewer.pending_sample().is_some() {
            return self.cancel_sample();
        }
        self.cancel_document_decode()
    }

    pub(super) fn cancel_document_decode(&mut self) -> Vec<Effect> {
        let Some(request_id) = self.document_request_generation() else {
            return Vec::new();
        };
        self.inflight = None;
        self.playing = false;
        self.replay = false;
        self.document_progress.clear();
        self.dirty.view = true;
        if self.shown.is_none() {
            self.fail_document("Decode cancelled.".into());
        }
        vec![Effect {
            request_id,
            command: Command::StopPlayback,
        }]
    }

    pub(super) fn partial_coverage(&mut self, message: String) {
        self.playing = false;
        self.document_progress.clear();
        if self.shown.is_some() {
            self.document_notice = message;
        } else {
            self.stage_failure = Some(message);
            self.dirty.picture = true;
        }
        if let Some(Want::Picture(key)) = self.want() {
            self.failed = Some(key);
        }
        self.dirty.view = true;
    }

    pub(super) fn fail_document(&mut self, message: String) {
        self.viewer.clear_sample();
        self.playing = false;
        self.replay = false;
        self.before_next = None;
        self.shown = None;
        self.document_progress.clear();
        self.stage_failure = Some(message);
        if let Some(Want::Picture(key)) = self.want() {
            self.failed = Some(key);
        }
        self.dirty.picture = true;
        self.dirty.view = true;
    }

    /// New work counts for a document seek. Older requests cannot change the status.
    pub fn progress(
        &mut self,
        generation: u64,
        stage: PictureStage,
        completed: u64,
        total: Option<u64>,
    ) {
        if self.document_request_generation() != Some(generation)
            && self.viewer.pending_sample() != Some(generation)
        {
            return;
        }
        let count = total.map_or_else(
            || completed.to_string(),
            |total| format!("{completed} / {total}"),
        );
        self.document_progress = match stage {
            PictureStage::Scan => format!("Reading file bytes ({count})"),
            PictureStage::Replay => {
                format!("Restoring decoder state ({count} output pictures decoded)")
            }
        };
        self.dirty.view = true;
    }

    pub(super) fn seek_output(&mut self, output: u64) {
        if self.document_id().is_none() {
            return;
        }
        if let Some(index) = output
            .checked_sub(1)
            .filter(|index| self.frames.is_none_or(|count| *index < count))
        {
            self.seek(index);
        } else {
            self.document_notice = self.frames.map_or_else(
                || "Enter a positive output number.".into(),
                |count| format!("Enter an output number from 1 to {count}."),
            );
            self.dirty.view = true;
        }
    }

    pub(super) fn document_view(&self, view: &mut StageView) {
        view.seek_error.clone_from(&self.document_notice);
        view.position = self.current_frame().unwrap_or(0);
        view.frames = self.frames.unwrap_or(0);
        view.frame_text = format!(
            "Output {}",
            super::text::frame_of(view.position, self.frames)
        );
        if self.document_decoding() {
            view.decode_work = if self.document_progress.is_empty() {
                if self.viewer.pending_sample().is_some() {
                    "Reading sample…".into()
                } else {
                    format!("Decoding to output {}…", view.position.saturating_add(1))
                }
            } else {
                self.document_progress.clone()
            };
            if self.shown.is_none() {
                view.message.clone_from(&view.decode_work);
            }
        }
    }

    pub(super) fn document_play_note(&self) -> String {
        if let Some(shown) = &self.shown {
            let header = &shown.picture.header;
            if header.time.is_some_and(|time| time.den > 0)
                || header
                    .output
                    .as_ref()
                    .and_then(|output| output.interval)
                    .is_some_and(|time| time.num > 0 && time.den > 0)
            {
                return String::new();
            }
            return "Studio assumes 30 fps for playback. Stream timing is unavailable.".into();
        }
        if self.stage_failure.is_some() {
            return String::new();
        }
        "Playback uses 30 fps if stream timing is unavailable.".into()
    }

    /// Pauses Analyze playback while the user examines linked bytes.
    pub fn pause_document(&mut self) -> Vec<Effect> {
        if self.document_id().is_some() && self.playing {
            self.input(super::Input::PlayPause)
        } else {
            Vec::new()
        }
    }

    /// The selected Analyze document and the byte offset linked to its shown output.
    #[must_use]
    pub fn output_byte(&self, link: i32) -> Option<(u64, u64)> {
        let document = self.document_id()?;
        let OutputEvent {
            unit_offset,
            coded_offset,
            showing_offset,
            sequence_offset,
            ..
        } = self.shown.as_ref()?.picture.header.output.as_ref()?;
        let offset = match link {
            0 => Some(*unit_offset),
            1 => *coded_offset,
            2 => *showing_offset,
            3 => *sequence_offset,
            _ => None,
        }?;
        Some((document, offset))
    }
}
