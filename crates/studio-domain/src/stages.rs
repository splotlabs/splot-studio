// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Exact native samples captured at decoder stage boundaries.

use serde::{Deserialize, Serialize};

use crate::preview::{SamplePlane, SampleRect};

/// Maximum width and height of one stage capture in native plane samples.
pub const STAGE_SIDE: u32 = 32;

/// Maximum capture bytes for all stage regions.
pub const MAX_STAGE_BYTES: u32 = 40 * 1024;

/// One region of a decoder plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageScope {
    /// Selected sample plane.
    pub plane: SamplePlane,
    /// Include grain on a direct display output.
    pub include_grain: bool,
    /// Region in native coordinates of the selected plane.
    pub region: SampleRect,
}

impl StageScope {
    /// Checks the coordinate range and capture limit.
    #[must_use]
    pub fn is_valid(self) -> bool {
        let r = self.region;
        (1..=STAGE_SIDE).contains(&r.width)
            && (1..=STAGE_SIDE).contains(&r.height)
            && r.x.checked_add(r.width).is_some()
            && r.y.checked_add(r.height).is_some()
    }
}

/// A point in decoder processing. The decoder reports each path's stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecodeStage {
    /// Prediction before the decoder adds residual samples.
    Prediction,
    /// Signed inverse-transform output.
    Residual,
    /// Reconstructed samples before loop filters.
    Reconstruction,
    /// Samples at the deblock boundary.
    Deblock,
    /// Samples at the CDEF boundary.
    Cdef,
    /// Samples at the CCSO boundary.
    Ccso,
    /// Samples at the loop restoration boundary.
    Restoration,
    /// Samples after guided detail filtering.
    Gdf,
    /// Final reference reconstruction.
    Reference,
    /// Presentation samples after film grain.
    Grain,
}

impl DecodeStage {
    /// Stage choices in the inspector.
    pub const ALL: [Self; 10] = [
        Self::Prediction,
        Self::Residual,
        Self::Reconstruction,
        Self::Deblock,
        Self::Cdef,
        Self::Ccso,
        Self::Restoration,
        Self::Gdf,
        Self::Reference,
        Self::Grain,
    ];

    /// Whether the values are signed decoded residuals.
    #[must_use]
    pub const fn is_residual(self) -> bool {
        matches!(self, Self::Residual)
    }
}

/// Availability at one boundary in the requested region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageStatus {
    /// The decoder applied the stage, including cases where result samples do not change.
    Applied,
    /// The decoder did not apply the stage.
    Skipped,
    /// The decoder currently has no capture support for this case.
    Unsupported,
    /// The decoder did not capture samples for this request.
    Uncaptured,
}

/// Metadata and exact row-major native samples for one stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageSamples {
    /// Decoder boundary.
    pub stage: DecodeStage,
    /// Availability at this boundary.
    pub status: StageStatus,
    /// Bit depth of the decoded plane.
    pub bit_depth: u8,
    /// Actual captured extent in the requested plane's coordinates.
    pub region: Option<SampleRect>,
    /// Full visible plane extent in the stage coordinate system.
    pub plane_extent: Option<SampleRect>,
    /// Signed samples. The protocol appends them as little-endian binary.
    #[serde(skip)]
    pub samples: Vec<i32>,
}

impl StageSamples {
    pub(crate) fn length(&self, scope: StageScope) -> Option<usize> {
        if !matches!(self.bit_depth, 8 | 10)
            || (self.stage == DecodeStage::Grain && !scope.include_grain)
        {
            return None;
        }
        match (self.status, self.region) {
            (StageStatus::Applied | StageStatus::Skipped, Some(r)) => {
                let request = scope.region;
                let extent = self.plane_extent?;
                (r.width > 0
                    && r.height > 0
                    && r.x >= extent.x
                    && r.y >= extent.y
                    && r.x.checked_add(r.width)? <= extent.x.checked_add(extent.width)?
                    && r.y.checked_add(r.height)? <= extent.y.checked_add(extent.height)?
                    && r.x >= request.x
                    && r.y >= request.y
                    && r.x.checked_add(r.width)? <= request.x.checked_add(request.width)?
                    && r.y.checked_add(r.height)? <= request.y.checked_add(request.height)?)
                .then_some((r.width as usize).checked_mul(r.height as usize)?)
            }
            (StageStatus::Applied, None) | (_, Some(_)) => None,
            (_, None) => Some(0),
        }
    }

    pub(crate) fn values_are_valid(&self) -> bool {
        matches!(self.bit_depth, 8 | 10)
            && (self.stage.is_residual()
                || self
                    .samples
                    .iter()
                    .all(|sample| (0..(1 << self.bit_depth)).contains(sample)))
    }

    /// Reads one sample in absolute plane coordinates.
    #[must_use]
    pub fn sample(&self, x: u32, y: u32) -> Option<i32> {
        let r = self.region?;
        let col = x.checked_sub(r.x)?;
        let row = y.checked_sub(r.y)?;
        if col >= r.width || row >= r.height {
            return None;
        }
        let index = (row as usize)
            .checked_mul(r.width as usize)?
            .checked_add(col as usize)?;
        self.samples.get(index).copied()
    }

    /// Checks whether the operands share geometry and numerical units.
    #[must_use]
    pub fn can_compare(&self, other: &Self) -> bool {
        self.region.is_some()
            && self.region == other.region
            && self.plane_extent == other.plane_extent
            && self.bit_depth == other.bit_depth
            && self.stage.is_residual() == other.stage.is_residual()
            && self.samples.len() == other.samples.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::{BlockCoverage, BlockEvidence, BlockEvidenceKind, BlockFrame, BlockQuery};
    use crate::identity::DecodeLayers;
    use crate::layers::LayerDecodeSelection;
    use crate::protocol::{
        Envelope, Event, ProtocolError, read_event, read_message, write_inspect_event,
        write_message,
    };

    fn stage_reply() -> Envelope<Event> {
        let region = SampleRect {
            x: 15,
            y: 9,
            width: 2,
            height: 1,
        };
        let query = BlockQuery {
            coded_offset: 47,
            selection: LayerDecodeSelection::Default,
            kind: BlockEvidenceKind::Stages(StageScope {
                plane: SamplePlane::V,
                include_grain: false,
                region,
            }),
            max_events: 1,
            max_bytes: 32_768,
            region: None,
            detail: None,
        };
        Envelope::new(
            3,
            Event::AnalysisBlocksReady {
                document: 2,
                evidence: Box::new(BlockEvidence {
                    entropy: None,
                    coefficients: None,
                    query,
                    frame: Some(BlockFrame {
                        coded_offset: 47,
                        sequence_offset: 28,
                        layers: DecodeLayers {
                            extended: 0,
                            embedded: 0,
                            temporal: 0,
                        },
                    }),
                    coverage: BlockCoverage::Complete,
                    replayed_through: 512,
                    stages: vec![
                        StageSamples {
                            stage: DecodeStage::Residual,
                            status: StageStatus::Applied,
                            bit_depth: 10,
                            region: Some(region),
                            plane_extent: Some(region),
                            samples: vec![-300, 1_024],
                        },
                        StageSamples {
                            stage: DecodeStage::Ccso,
                            status: StageStatus::Skipped,
                            bit_depth: 10,
                            region: None,
                            plane_extent: Some(region),
                            samples: vec![],
                        },
                    ],
                    records: vec![],
                }),
            },
        )
    }

    #[test]
    fn exact_stage_samples_are_binary_and_do_not_consume_the_next_message() {
        let reply = stage_reply();
        let mut output = Vec::new();
        assert!(write_inspect_event(&mut output, &reply).is_ok());
        let trailing = Envelope::new(4, Event::AnalysisClosed { document: 2 });
        assert!(write_message(&mut output, &trailing).is_ok());
        let mut stream = output.as_slice();
        let Ok(Some((header, None))) = read_event(&mut stream) else {
            unreachable!();
        };
        assert_eq!(header, reply);
        let Event::AnalysisBlocksReady { evidence, .. } = header.body else {
            unreachable!();
        };
        assert!(evidence.matches(evidence.query));
        assert_eq!(evidence.stages[0].sample(15, 9), Some(-300));
        assert_eq!(evidence.stages[0].sample(16, 9), Some(1_024));
        assert_eq!(evidence.stages[0].sample(17, 9), None);
        assert!(matches!(read_message::<Event>(&mut stream), Ok(Some(next)) if next == trailing));
    }

    #[test]
    fn stage_protocol_rejects_unrequested_grain_and_excess_sample_bytes() {
        let mut reply = stage_reply();
        let Event::AnalysisBlocksReady { evidence, .. } = &mut reply.body else {
            unreachable!();
        };
        evidence.query.max_bytes = 7;
        assert!(!evidence.matches(evidence.query));
        evidence.query.max_bytes = MAX_STAGE_BYTES;
        evidence.stages[0].stage = DecodeStage::Grain;
        evidence.stages[0].samples = vec![0, 1];
        assert!(!evidence.matches(evidence.query));
        let BlockEvidenceKind::Stages(ref mut scope) = evidence.query.kind else {
            unreachable!();
        };
        scope.include_grain = true;
        assert!(evidence.matches(evidence.query));
    }

    #[test]
    fn stage_protocol_rejects_duplicate_oversized_and_truncated_payloads() {
        let reply = stage_reply();
        let mut output = Vec::new();
        assert!(write_inspect_event(&mut output, &reply).is_ok());
        output.pop();
        assert!(matches!(
            read_event(&mut output.as_slice()),
            Err(ProtocolError::Truncated)
        ));
        for duplicate in [false, true] {
            let mut reply = stage_reply();
            let Event::AnalysisBlocksReady { evidence, .. } = &mut reply.body else {
                unreachable!();
            };
            if duplicate {
                evidence.stages[1] = evidence.stages[0].clone();
            } else {
                evidence.stages[0].region = evidence.stages[0].region.map(|mut r| {
                    r.width = u32::MAX;
                    r
                });
            }
            let mut output = Vec::new();
            assert!(matches!(
                write_inspect_event(&mut output, &reply),
                Err(ProtocolError::InvalidStageSamples)
            ));
            assert_eq!(output, [] as [u8; 0]);
            assert!(write_message(&mut output, &reply).is_ok());
            assert!(matches!(
                read_event(&mut output.as_slice()),
                Err(ProtocolError::InvalidStageSamples)
            ));
        }
    }
}
