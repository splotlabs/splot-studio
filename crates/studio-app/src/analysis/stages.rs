// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Stage replay and local numerical inspection.

use studio_domain::blocks::{BlockEvidence, BlockEvidenceKind, BlockQuery};
use studio_domain::preview::{SamplePlane, SampleRect};
use studio_domain::protocol::Command;
use studio_domain::stages::{MAX_STAGE_BYTES, STAGE_SIDE, StageScope};
use studio_domain::{analysis::AnalysisFailure, blocks::BlockCoverage};

use super::{Analysis, Asked, Document, Effect};

pub(crate) const TAB: i32 = 6;

/// Inputs for the decoder stage inspector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageInput {
    /// Supply the plane and absolute origin for one bounded capture.
    Capture(i32, String, String, bool),
    /// Use the selected block's visible origin in luma coordinates.
    UseBlock,
    /// Change the plane for the next capture.
    Plane(i32),
    /// Set display amplification without altering the sample values.
    Gain(i32),
    /// Include grain on the selected frame's direct display output.
    Grain(bool),
    /// Stop the current replay.
    Cancel,
    /// Select stage A or B.
    Choose(bool, i32),
    /// Show both stages, the signed difference, or the absolute difference.
    Mode(i32),
    /// Set the linked magnification.
    Zoom(i32),
    /// Move both views by the specified number of samples.
    Pan(i32, i32),
    /// Read both operands and their difference at one absolute coordinate.
    Sample(String, String),
    /// Read from a point on one painted stage image.
    Pick(bool, i32, i32, i32, i32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stages {
    pub(super) pending: Option<u64>,
    pub(crate) stopping: bool,
    pub(crate) evidence: Option<Box<BlockEvidence>>,
    pub(crate) notice: Option<String>,
    pub(crate) selections: [usize; 2],
    pub(crate) mode: i32,
    pub(crate) gain: i32,
    pub(crate) zoom: i32,
    pub(crate) pan: (u32, u32),
    pub(crate) sample: Option<(u32, u32)>,
    pub(crate) origin: (u32, u32),
    pub(crate) plane: i32,
    pub(crate) include_grain: bool,
}

impl Default for Stages {
    fn default() -> Self {
        Self {
            pending: None,
            stopping: false,
            evidence: None,
            notice: None,
            selections: [0, 8],
            mode: 0,
            gain: 0,
            zoom: 0,
            pan: (0, 0),
            sample: None,
            origin: (0, 0),
            plane: 0,
            include_grain: false,
        }
    }
}

impl Stages {
    pub(crate) const fn busy(&self) -> bool {
        self.pending.is_some()
    }

    pub(super) fn accept(&mut self, id: u64, evidence: Box<BlockEvidence>) {
        if self.pending != Some(id) {
            return;
        }
        self.pending = None;
        let stopped = std::mem::take(&mut self.stopping);
        if stopped || evidence.coverage == BlockCoverage::Cancelled {
            self.notice = Some("Stage inspection stopped.".into());
        } else {
            self.evidence = Some(evidence);
        }
    }

    pub(super) fn fail(&mut self, id: u64, code: Option<AnalysisFailure>, message: &str) {
        if self.pending == Some(id) {
            self.pending = None;
            self.stopping = false;
            self.notice = Some(if code == Some(AnalysisFailure::Cancelled) {
                "Stage inspection stopped.".into()
            } else {
                format!("Studio could not inspect the decoder stages. {message}")
            });
        }
    }

    pub(crate) const fn extent(&self) -> u32 {
        STAGE_SIDE >> self.zoom
    }

    pub(crate) fn stage(&self, right: bool) -> Option<&studio_domain::stages::StageSamples> {
        let kind = studio_domain::stages::DecodeStage::ALL[self.selections[usize::from(right)]];
        self.evidence
            .as_ref()?
            .stages
            .iter()
            .find(|stage| stage.stage == kind)
    }

    pub(crate) fn crop(&self, r: SampleRect) -> SampleRect {
        let width = r.width.min(self.extent());
        let height = r.height.min(self.extent());
        SampleRect {
            x: r.x + self.pan.0.min(r.width - width),
            y: r.y + self.pan.1.min(r.height - height),
            width,
            height,
        }
    }

    fn pan_limit(&self) -> (u32, u32) {
        [self.stage(false), self.stage(true)]
            .into_iter()
            .flatten()
            .filter_map(|stage| stage.region)
            .fold((0, 0), |(w, h), r| {
                (
                    w.max(r.width.saturating_sub(self.extent())),
                    h.max(r.height.saturating_sub(self.extent())),
                )
            })
    }
}

impl Analysis {
    pub(crate) fn stage_block_origin(&self, doc: &Document) -> Option<(u32, u32)> {
        let overlay = self.overlay();
        let selected = overlay.selected?;
        if overlay.picture?.document != doc.id()
            || selected.frame.coded_offset != doc.coded_offset()?
            || selected.policy != doc.layer_decode_selection()
        {
            return None;
        }
        let r = overlay.selected_record()?.visible?;
        Some((r.x, r.y))
    }

    pub(super) fn stage_input(&mut self, at: usize, input: StageInput, out: &mut Vec<Effect>) {
        if self.documents[at].evidence.tab != TAB || self.documents[at].layers.shown {
            return;
        }
        match input {
            StageInput::Capture(plane, x, y, grain) => {
                self.capture_stages(at, plane, &x, &y, grain, out);
            }
            StageInput::Cancel => self.stop_stages(at, out),
            StageInput::UseBlock => {
                if let Some(origin) = self.stage_block_origin(&self.documents[at]) {
                    let stages = &mut self.documents[at].stages;
                    stages.origin = origin;
                    stages.plane = 0;
                }
            }
            input => {
                let stages = &mut self.documents[at].stages;
                match input {
                    StageInput::Choose(right, index) => {
                        if let Ok(index) = usize::try_from(index)
                            && index < studio_domain::stages::DecodeStage::ALL.len()
                        {
                            stages.selections[usize::from(right)] = index;
                        }
                    }
                    StageInput::Plane(plane) => stages.plane = plane.clamp(0, 2),
                    StageInput::Gain(gain) => stages.gain = gain.clamp(0, 2),
                    StageInput::Grain(include)
                        if !stages.busy() && stages.include_grain != include =>
                    {
                        stages.include_grain = include;
                        stages.evidence = None;
                    }
                    StageInput::Mode(mode) => stages.mode = mode.clamp(0, 2),
                    StageInput::Zoom(zoom) => stages.zoom = zoom.clamp(0, 3),
                    StageInput::Pan(x, y) => {
                        let (w, h) = stages.pan_limit();
                        stages.pan = (
                            stages.pan.0.saturating_add_signed(x).min(w),
                            stages.pan.1.saturating_add_signed(y).min(h),
                        );
                    }
                    StageInput::Sample(x, y) => stages.sample = coords(&x, &y),
                    StageInput::Pick(right, x, y, width, height) => {
                        if let Some(r) = stages.stage(right).and_then(|stage| stage.region) {
                            let r = stages.crop(r);
                            if let Some((col, row)) =
                                project(x, width, r.width).zip(project(y, height, r.height))
                            {
                                stages.sample = Some((r.x + col, r.y + row));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn capture_stages(
        &mut self,
        at: usize,
        plane: i32,
        x: &str,
        y: &str,
        include_grain: bool,
        out: &mut Vec<Effect>,
    ) {
        let doc = &mut self.documents[at];
        if !doc.block_query_ready() || doc.stages.busy() {
            return;
        }
        let Some((x, y)) = coords(x, y) else {
            doc.stages.notice = Some("Enter a nonnegative column and row.".into());
            return;
        };
        let sample_plane = match plane {
            0 => SamplePlane::Y,
            1 => SamplePlane::U,
            2 => SamplePlane::V,
            _ => return,
        };
        let Some(coded_offset) = doc.coded_offset() else {
            return;
        };
        let scope = StageScope {
            plane: sample_plane,
            include_grain,
            region: SampleRect {
                x,
                y,
                width: STAGE_SIDE,
                height: STAGE_SIDE,
            },
        };
        if !scope.is_valid() {
            doc.stages.notice = Some("The region exceeds the coordinate limit.".into());
            return;
        }
        let query = BlockQuery {
            coded_offset,
            selection: doc.layer_decode_selection(),
            kind: BlockEvidenceKind::Stages(scope),
            region: None,
            detail: None,
            max_events: 1,
            max_bytes: MAX_STAGE_BYTES,
        };
        let document = doc.id;
        let request = self.send(document, Asked::Blocks(query), out, |document| {
            Command::AnalysisBlocks { document, query }
        });
        self.documents[at].stages = Stages {
            pending: Some(request),
            origin: (x, y),
            sample: Some((x, y)),
            plane,
            include_grain,
            ..Stages::default()
        };
    }

    fn stop_stages(&mut self, at: usize, out: &mut Vec<Effect>) {
        let doc = &mut self.documents[at];
        if let Some(blocks) = doc.stages.pending
            && !doc.stages.stopping
        {
            doc.stages.stopping = true;
            let document = doc.id;
            self.send(document, Asked::Cancel, out, |document| {
                Command::AnalysisCancelBlocks { document, blocks }
            });
        }
    }

    pub(super) fn cancel_stages(&mut self, at: usize, out: &mut Vec<Effect>) {
        self.stop_stages(at, out);
        if let Some(id) = self.documents[at].stages.pending {
            self.pending.remove(&id);
        }
        self.documents[at].stages = Stages::default();
    }
}

fn coords(x: &str, y: &str) -> Option<(u32, u32)> {
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

fn project(point: i32, pixels: i32, samples: u32) -> Option<u32> {
    if !(0..pixels).contains(&point) {
        return None;
    }
    u32::try_from(i64::from(point) * i64::from(samples) / i64::from(pixels)).ok()
}
