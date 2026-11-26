// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Scoped block selection for the current Analyze picture.

use studio_domain::analysis::AnalysisFailure;
use studio_domain::blocks::{
    BlockCoverage, BlockData, BlockEvidence, BlockEvidenceKind, BlockFrame, BlockId, BlockMotion,
    BlockPrediction, BlockQuery, BlockRecord, BlockRect, BlockTransformId, BlockTree,
    MAX_BLOCK_BYTES, MAX_BLOCK_RECORDS,
};
use studio_domain::layers::LayerDecodeSelection;
use studio_domain::preview::{SamplePlane, SampleRect};
use studio_domain::protocol::Command;

use super::{Analysis, Asked, Effect};
use crate::preview::{BlockPicture, HistoryView};

/// A viewer action for decoder evidence.
#[derive(Debug, Clone, Copy)]
pub enum BlockOverlayInput {
    /// Select the map to display.
    Mode(i32),
    /// Select the plane used for the overlay.
    Plane(i32),
    /// Select a tool to show.
    Tool(i32),
    /// Select a visible record in the text list.
    Select(i32),
    /// Move the selection by one row or one page.
    Move(i32),
    /// Select a region using coordinates relative to the painted picture.
    Pick {
        /// Physical horizontal coordinate.
        x: i32,
        /// Physical vertical coordinate.
        y: i32,
        /// Painted width in physical pixels.
        width: i32,
        /// Painted height in physical pixels.
        height: i32,
    },
    /// Cancel the current capture.
    Stop,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Mode {
    #[default]
    Off,
    Partitions,
    Transforms,
    Quantizer,
    Segment,
    Tools,
}

impl Mode {
    const fn from_index(index: i32) -> Self {
        match index {
            1 => Self::Partitions,
            2 => Self::Transforms,
            3 => Self::Quantizer,
            4 => Self::Segment,
            5 => Self::Tools,
            _ => Self::Off,
        }
    }

    pub(crate) const fn kind(self) -> BlockEvidenceKind {
        match self {
            Self::Off | Self::Partitions => BlockEvidenceKind::Structure,
            Self::Transforms => BlockEvidenceKind::Transforms,
            Self::Quantizer | Self::Segment | Self::Tools => BlockEvidenceKind::Coding,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Key {
    Partition(BlockId),
    Coding(BlockId),
    Transform(BlockTransformId),
}

impl Key {
    fn of(record: &BlockRecord) -> Option<Self> {
        match &record.data {
            BlockData::Partition { .. } => Some(Self::Partition(record.id)),
            BlockData::Coding { .. } => Some(Self::Coding(record.id)),
            BlockData::Transform { transform } => Some(Self::Transform(transform.id)),
            BlockData::Motion { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct History {
    document: u64,
    pub(crate) frame: BlockFrame,
    pub(super) output: u64,
    pub(super) policy: LayerDecodeSelection,
    pub(crate) key: Key,
    pub(super) point: (u32, u32),
    pub(super) mode: Mode,
    pub(super) plane: Option<SamplePlane>,
    pub(super) view: Option<HistoryView>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Capture {
    View,
    Point(u32, u32, Option<HistoryView>),
    Details(BlockTransformId),
    Motion(BlockId, u32, u32),
}

#[derive(Debug, Default)]
pub(crate) struct Overlay {
    pub(crate) mode: Mode,
    pub(crate) plane: Option<SamplePlane>,
    pub(crate) tool: i32,
    pub(crate) picture: Option<BlockPicture>,
    pub(crate) viewport: Option<Box<BlockEvidence>>,
    captured_document: Option<u64>,
    point: Option<Box<BlockEvidence>>,
    pub(crate) details: Option<Box<BlockEvidence>>,
    pub(crate) selected: Option<History>,
    pending: Option<(u64, Capture)>,
    pub(crate) notice: Option<String>,
}

impl Overlay {
    pub(crate) const fn busy(&self) -> bool {
        self.pending.is_some()
    }

    pub(crate) fn active_plane(&self) -> Option<SamplePlane> {
        self.plane
            .or(self.picture.map(|picture| picture.viewport.plane()))
    }

    pub(crate) fn records(&self) -> Vec<(&BlockRecord, Key)> {
        let mut records = Vec::new();
        let Some(plane) = self.active_plane() else {
            return records;
        };
        let Some(picture) = self.picture else {
            return records;
        };
        for evidence in [&self.viewport, &self.point].into_iter().flatten() {
            for record in &evidence.records {
                if let Some(key) = Key::of(record)
                    && eligible(self.mode, plane, record)
                    && record
                        .visible
                        .and_then(|rect| picture.viewport.project(sample_rect(rect)))
                        .is_some()
                    && !records.iter().any(|(_, seen)| *seen == key)
                {
                    records.push((record, key));
                }
            }
        }
        records
    }

    fn hit(&self, x: u32, y: u32) -> Option<Key> {
        self.records()
            .into_iter()
            .filter(|(record, _)| record.visible.is_some_and(|r| contains(r, x, y)))
            .min_by_key(|(record, key)| {
                let rect = geometry(record);
                (
                    matches!(key, Key::Partition(_)),
                    u64::from(rect.width) * u64::from(rect.height),
                    record.id.tile,
                    rect.y,
                    rect.x,
                )
            })
            .map(|(_, key)| key)
    }

    pub(crate) fn selected_record(&self) -> Option<&BlockRecord> {
        let selected = self.selected?;
        if self.picture.is_none_or(|picture| {
            picture.document != selected.document || !matches_picture(picture, selected.frame)
        }) {
            return None;
        }
        self.records()
            .into_iter()
            .find_map(|(record, key)| (key == selected.key).then_some(record))
    }

    pub(crate) fn selected_motion(&self) -> Option<BlockMotion> {
        let selected = self.selected?;
        let Key::Coding(block) = selected.key else {
            return None;
        };
        [&self.point, &self.viewport, &self.details]
            .into_iter()
            .flatten()
            .filter(|evidence| evidence.frame == Some(selected.frame))
            .flat_map(|evidence| &evidence.records)
            .find_map(|record| match record.data {
                BlockData::Motion { motion } if record.id == block => Some(motion),
                _ => None,
            })
    }

    fn clear_picture(&mut self) {
        self.picture = None;
        self.viewport = None;
        self.captured_document = None;
        self.point = None;
        self.details = None;
        self.notice = None;
    }

    pub(super) fn end(&mut self) {
        self.pending = None;
        self.selected = None;
        self.clear_picture();
    }
}

impl Analysis {
    pub(crate) fn overlay(&self) -> &Overlay {
        &self.overlay
    }

    pub(crate) fn sync_overlay(&mut self, picture: Option<BlockPicture>) -> Vec<Effect> {
        let mut out = Vec::new();
        let overlay = &self.overlay;
        let picture = picture.filter(|picture| {
            overlay.mode != Mode::Off
                && self.shown().is_some_and(|doc| {
                    doc.id() == picture.document
                        && doc.layer_decode_selection() == picture.selection
                        && picture.output.coded_offset.is_some_and(|offset| {
                            doc.summary()
                                .and_then(|summary| summary.digest.as_deref())
                                .is_some_and(|digest| offset < digest.bytes)
                        })
                })
        });
        if overlay.picture == picture {
            return out;
        }
        self.cancel_overlay(&mut out);
        self.overlay.picture = picture;
        self.overlay.notice = None;
        self.dirty = true;
        if let Some(picture) = picture {
            if self.overlay.selected.is_some_and(|selected| {
                picture.document != selected.document || !matches_picture(picture, selected.frame)
            }) {
                self.overlay.selected = None;
            }
            let covered = self.overlay.captured_document == Some(picture.document)
                && self.overlay.viewport.as_deref().is_some_and(|evidence| {
                    let region = picture.viewport.region().map(block_rect);
                    evidence.coverage == BlockCoverage::Complete
                        && evidence.query.selection == picture.selection
                        && evidence.query.kind == self.overlay.mode.kind()
                        && evidence
                            .frame
                            .is_some_and(|frame| matches_picture(picture, frame))
                        && evidence
                            .query
                            .region
                            .zip(region)
                            .is_some_and(|(outer, inner)| {
                                contains(outer, inner.x, inner.y)
                                    && contains(
                                        outer,
                                        inner.x + inner.width - 1,
                                        inner.y + inner.height - 1,
                                    )
                            })
                });
            if !covered {
                self.overlay.viewport = None;
                self.overlay.point = None;
                self.overlay.details = None;
                self.request_overlay(Capture::View, &mut out);
            } else if self.overlay.details.is_none()
                && let Some(History {
                    key: Key::Transform(id),
                    ..
                }) = self.overlay.selected
                && self.overlay.selected_record().is_some()
            {
                self.request_overlay(Capture::Details(id), &mut out);
            }
        }
        out
    }

    pub(crate) fn change_overlay(
        &mut self,
        input: BlockOverlayInput,
        picture: Option<BlockPicture>,
        point: Option<(u32, u32)>,
        view: Option<HistoryView>,
    ) -> Vec<Effect> {
        let mut out = Vec::new();
        match input {
            BlockOverlayInput::Mode(index) => {
                let mode = Mode::from_index(index);
                if self.overlay.mode != mode {
                    let replace = self.overlay.mode == Mode::Off
                        || mode == Mode::Off
                        || self.overlay.mode.kind() != mode.kind();
                    self.overlay.mode = mode;
                    self.overlay.selected = None;
                    self.overlay.details = None;
                    if replace {
                        self.cancel_overlay(&mut out);
                        self.overlay.clear_picture();
                    }
                    out.extend(self.sync_overlay(picture));
                }
            }
            BlockOverlayInput::Plane(index) => {
                self.overlay.plane = match index {
                    1 => Some(SamplePlane::Y),
                    2 => Some(SamplePlane::U),
                    3 => Some(SamplePlane::V),
                    _ => None,
                };
                self.overlay.selected = None;
                self.overlay.details = None;
                if self.overlay.pending.is_some_and(|(_, capture)| {
                    matches!(capture, Capture::Details(_) | Capture::Motion(..))
                }) {
                    self.cancel_overlay(&mut out);
                }
            }
            BlockOverlayInput::Tool(index) => self.overlay.tool = index.clamp(0, 3),
            BlockOverlayInput::Pick { .. } => {
                if self.overlay.picture == picture
                    && let Some((x, y)) = point
                {
                    let selected = self.overlay.hit(x, y);
                    let needs_point = self
                        .overlay
                        .viewport
                        .as_deref()
                        .is_none_or(|e| e.coverage != BlockCoverage::Complete)
                        && selected.is_none_or(|key| matches!(key, Key::Partition(_)));
                    if needs_point {
                        self.overlay.selected = None;
                        self.overlay.details = None;
                        self.request_overlay(Capture::Point(x, y, view), &mut out);
                    } else if let Some(key) = selected {
                        self.select_overlay(key, (x, y), view, &mut out);
                    } else {
                        self.overlay.selected = None;
                        self.overlay.details = None;
                        self.overlay.notice = Some("No decoded region at this point.".to_owned());
                    }
                }
            }
            BlockOverlayInput::Select(index) => {
                if let Ok(index) = usize::try_from(index) {
                    self.select_overlay_index(index, view, &mut out);
                }
            }
            BlockOverlayInput::Move(step) => {
                let records = self.overlay.records();
                if !records.is_empty() {
                    let selected = self.overlay.selected.and_then(|selected| {
                        records.iter().position(|(_, key)| *key == selected.key)
                    });
                    let index = selected.map_or(0, |selected| {
                        selected
                            .saturating_add_signed(step as isize)
                            .min(records.len() - 1)
                    });
                    self.select_overlay_index(index, view, &mut out);
                }
            }
            BlockOverlayInput::Stop => {
                self.cancel_overlay(&mut out);
                self.overlay.notice = Some("Block capture stopped.".to_owned());
            }
        }
        self.dirty = true;
        out
    }

    fn select_overlay_index(
        &mut self,
        index: usize,
        view: Option<HistoryView>,
        out: &mut Vec<Effect>,
    ) {
        if let Some(&(record, key)) = self.overlay.records().get(index)
            && let Some(rect) = record.visible
        {
            self.select_overlay(key, (rect.x, rect.y), view, out);
        }
    }

    fn select_overlay(
        &mut self,
        key: Key,
        point: (u32, u32),
        view: Option<HistoryView>,
        out: &mut Vec<Effect>,
    ) {
        let Some(picture) = self.overlay.picture else {
            return;
        };
        let Some(frame) = self
            .overlay
            .viewport
            .as_ref()
            .and_then(|e| e.frame)
            .or_else(|| self.overlay.point.as_ref().and_then(|e| e.frame))
        else {
            return;
        };
        if self
            .overlay
            .selected
            .is_some_and(|s| s.key == key && s.frame == frame)
        {
            return;
        }
        let selected = History {
            document: picture.document,
            frame,
            output: picture.output.ordinal,
            policy: picture.selection,
            key,
            point,
            mode: self.overlay.mode,
            plane: self.overlay.plane,
            view,
        };
        self.overlay.selected = Some(selected);
        if let Some(doc) = self.documents.get_mut(self.shown) {
            doc.remember_selection();
            doc.history[doc.at].block = Some(selected);
        }
        self.overlay.notice = None;
        self.overlay.details = None;
        self.request_selected_details(out);
    }

    fn request_selected_details(&mut self, out: &mut Vec<Effect>) {
        let Some(selected) = self.overlay.selected else {
            return;
        };
        match selected.key {
            Key::Transform(id) => self.request_overlay(Capture::Details(id), out),
            Key::Coding(block)
                if self.overlay.selected_motion().is_none()
                    && self.overlay.selected_record().is_some_and(|record| {
                        matches!(
                            record.data,
                            BlockData::Coding {
                                prediction: Some(
                                    BlockPrediction::Inter { .. } | BlockPrediction::IntraBlockCopy
                                ),
                                ..
                            }
                        )
                    }) =>
            {
                let (x, y) = selected.point;
                self.request_overlay(Capture::Motion(block, x, y), out);
            }
            _ => {}
        }
    }

    fn request_overlay(&mut self, capture: Capture, out: &mut Vec<Effect>) {
        let Some(picture) = self.overlay.picture else {
            return;
        };
        let Some(coded_offset) = picture.output.coded_offset else {
            return;
        };
        let region = match capture {
            Capture::View => {
                self.overlay.captured_document =
                    self.overlay.picture.map(|picture| picture.document);
                let Some(region) = picture.viewport.region() else {
                    return;
                };
                Some(block_rect(region))
            }
            Capture::Point(x, y, _) | Capture::Motion(_, x, y) => Some(BlockRect {
                x,
                y,
                width: 1,
                height: 1,
            }),
            Capture::Details(_) => None,
        };
        let detail = match capture {
            Capture::Details(id) => Some(id),
            _ => None,
        };
        let query = BlockQuery {
            coded_offset,
            selection: picture.selection,
            kind: if matches!(capture, Capture::Motion(..)) {
                BlockEvidenceKind::Coding
            } else {
                self.overlay.mode.kind()
            },
            region,
            detail,
            max_events: if detail.is_some() {
                1
            } else if matches!(capture, Capture::Motion(..)) {
                8
            } else {
                MAX_BLOCK_RECORDS
            },
            max_bytes: MAX_BLOCK_BYTES,
        };
        self.cancel_overlay(out);
        let request = self.send(picture.document, Asked::Overlay(query), out, |document| {
            Command::AnalysisBlocks { document, query }
        });
        self.overlay.pending = Some((request, capture));
        self.overlay.notice = None;
    }

    pub(super) fn cancel_overlay(&mut self, out: &mut Vec<Effect>) {
        let Some((request, _)) = self.overlay.pending.take() else {
            return;
        };
        if let Some((document, _)) = self.pending.remove(&request) {
            self.send(document, Asked::Cancel, out, |document| {
                Command::AnalysisCancelBlocks {
                    document,
                    blocks: request,
                }
            });
        }
    }

    pub(super) fn accept_overlay(
        &mut self,
        request: u64,
        query: BlockQuery,
        evidence: Box<BlockEvidence>,
        out: &mut Vec<Effect>,
    ) {
        let Some((id, capture)) = self.overlay.pending else {
            return;
        };
        if id != request {
            return;
        }
        self.overlay.pending = None;
        if !evidence.matches(query)
            || evidence.frame.is_some_and(|actual| {
                self.overlay
                    .picture
                    .is_none_or(|picture| !matches_picture(picture, actual))
            })
        {
            self.overlay.notice =
                Some("The decoder returned evidence for a different region.".to_owned());
            return;
        }
        match capture {
            Capture::View => {
                self.overlay.viewport = Some(evidence);
                if let Some(selected) = self.overlay.selected {
                    if self.overlay.selected_record().is_none() {
                        let (x, y) = selected.point;
                        if self
                            .overlay
                            .picture
                            .and_then(|p| p.viewport.region())
                            .is_some_and(|region| contains(block_rect(region), x, y))
                        {
                            self.request_overlay(Capture::Point(x, y, selected.view), out);
                        }
                    } else {
                        self.request_selected_details(out);
                    }
                }
            }
            Capture::Point(x, y, view) => {
                self.overlay.point = Some(evidence);
                if self.overlay.selected.is_some() {
                    if self.overlay.selected_record().is_some() {
                        self.request_selected_details(out);
                    }
                } else if let Some(key) = self.overlay.hit(x, y) {
                    self.select_overlay(key, (x, y), view, out);
                } else {
                    self.overlay.notice =
                        Some("This capture returned no decoder record for the point.".to_owned());
                }
            }
            Capture::Details(id) => {
                if self
                    .overlay
                    .selected
                    .is_some_and(|s| s.key == Key::Transform(id))
                {
                    self.overlay.details = Some(evidence);
                }
            }
            Capture::Motion(block, _, _) => {
                if self.overlay.selected.is_some_and(|selected| {
                    selected.key == Key::Coding(block) && evidence.frame == Some(selected.frame)
                }) {
                    self.overlay.details = Some(evidence);
                }
            }
        }
    }

    pub(super) fn fail_overlay(
        &mut self,
        request: u64,
        code: Option<AnalysisFailure>,
        message: &str,
    ) {
        if self
            .overlay
            .pending
            .is_some_and(|pending| pending.0 == request)
        {
            self.overlay.pending = None;
            self.overlay.notice = Some(if code == Some(AnalysisFailure::Cancelled) {
                "Block capture stopped.".to_owned()
            } else {
                format!("Studio could not read block evidence. {message}")
            });
        }
    }

    pub(super) fn restore_block_selection(
        &mut self,
        selected: Option<History>,
        out: &mut Vec<Effect>,
    ) {
        if self.overlay.selected != selected {
            self.cancel_overlay(out);
            self.overlay.end();
            self.overlay.selected = selected;
            if let Some(selected) = selected {
                self.overlay.mode = selected.mode;
                self.overlay.plane = selected.plane;
            }
        }
    }
}

fn matches_picture(picture: BlockPicture, frame: BlockFrame) -> bool {
    picture.output.coded_offset == Some(frame.coded_offset)
        && picture.output.sequence_offset == Some(frame.sequence_offset)
}

pub(crate) const fn geometry(record: &BlockRecord) -> BlockRect {
    match &record.data {
        BlockData::Transform { transform } => transform.luma_coded,
        _ => record.id.coded,
    }
}

pub(crate) const fn sample_rect(rect: BlockRect) -> SampleRect {
    SampleRect {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
    }
}

const fn block_rect(rect: SampleRect) -> BlockRect {
    BlockRect {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
    }
}

const fn contains(rect: BlockRect, x: u32, y: u32) -> bool {
    x >= rect.x && y >= rect.y && x - rect.x < rect.width && y - rect.y < rect.height
}

fn eligible(mode: Mode, plane: SamplePlane, record: &BlockRecord) -> bool {
    if let BlockData::Transform { transform } = &record.data {
        return mode == Mode::Transforms && transform.id.plane == plane;
    }
    match (&record.data, mode) {
        (BlockData::Partition { .. }, Mode::Partitions) => {}
        (
            BlockData::Coding { has_chroma, .. },
            Mode::Partitions | Mode::Quantizer | Mode::Segment | Mode::Tools,
        ) => {
            if record.id.tree == BlockTree::Shared && plane != SamplePlane::Y && !has_chroma {
                return false;
            }
        }
        _ => return false,
    }
    match record.id.tree {
        BlockTree::Shared => true,
        BlockTree::Luma => plane == SamplePlane::Y,
        BlockTree::Chroma => plane != SamplePlane::Y,
    }
}
