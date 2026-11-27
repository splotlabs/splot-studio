// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Bounded decoder evidence for a coded frame in one document generation.
//!
//! A frame is scoped by the document ID in the command and event. Within
//! that frame, tile, tree and coded geometry identify a node of each record kind.
//! A partition and its block may have the same rectangle.
//! All record values come from the decoder. An absent value is unavailable.

use serde::{Deserialize, Serialize};

pub use self::coefficients::*;
pub use self::prediction::*;
use crate::identity::DecodeLayers;
use crate::layers::LayerDecodeSelection;
use crate::preview::SamplePlane;

mod coefficients;
mod prediction;

/// Most records in one result. The result fits one control message.
pub const MAX_BLOCK_RECORDS: u32 = 64;
/// Largest byte budget for native records.
pub const MAX_BLOCK_BYTES: u32 = 32 * 1024;

/// Evidence requested from the decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockEvidenceKind {
    /// Geometry, partition, prediction and quantizer facts.
    Structure,
    /// Partition decisions, including leaf markers.
    Partitions,
    /// Coding block and motion records.
    Coding,
    /// Transform geometry, or details for one selected transform.
    Transforms,
    /// Coefficient arrays for exactly one requested transform.
    Coefficients,
}

/// A rectangle in the coded frame's luma coordinate system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockRect {
    /// Horizontal coordinate.
    pub x: u32,
    /// Vertical coordinate.
    pub y: u32,
    /// Width in luma samples.
    pub width: u32,
    /// Height in luma samples.
    pub height: u32,
}

impl BlockRect {
    /// Checks that the rectangle has an area and representable edges.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.width > 0
            && self.height > 0
            && self.x.checked_add(self.width).is_some()
            && self.y.checked_add(self.height).is_some()
    }

    fn contains(self, other: Self) -> bool {
        self.x <= other.x
            && self.y <= other.y
            && self.x + self.width >= other.x + other.width
            && self.y + self.height >= other.y + other.height
    }

    fn intersects(self, other: Self) -> bool {
        self.x < other.x + other.width
            && other.x < self.x + self.width
            && self.y < other.y + other.height
            && other.y < self.y + self.height
    }
}

/// One decoder evidence query in the current inspection context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockQuery {
    /// File offset of the first coded OBU.
    pub coded_offset: u64,
    /// Requested decoder operating point.
    pub selection: LayerDecodeSelection,
    /// Evidence to capture.
    pub kind: BlockEvidenceKind,
    /// Optional intersection filter in coded-luma coordinates.
    pub region: Option<BlockRect>,
    /// Return details for exactly this transform.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<BlockTransformId>,
    /// Maximum number of records to capture.
    pub max_events: u32,
    /// Maximum native record bytes to capture.
    pub max_bytes: u32,
}

impl BlockQuery {
    /// Checks both independent budgets and the optional rectangle.
    #[must_use]
    pub fn is_valid(self) -> bool {
        (1..=MAX_BLOCK_RECORDS).contains(&self.max_events)
            && (1..=MAX_BLOCK_BYTES).contains(&self.max_bytes)
            && self.region.is_none_or(BlockRect::is_valid)
            && (self.kind != BlockEvidenceKind::Coefficients
                || (self.detail.is_some() && self.region.is_none() && self.max_events == 1))
            && self.detail.is_none_or(|id| {
                matches!(
                    self.kind,
                    BlockEvidenceKind::Transforms | BlockEvidenceKind::Coefficients
                ) && id.block.coded.is_valid()
                    && id.coded.is_valid()
            })
    }
}

/// Scope of a decoded frame in one document generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockFrame {
    /// File offset of the first coded OBU.
    pub coded_offset: u64,
    /// File offset of the sequence header used for this frame.
    pub sequence_offset: u64,
    /// Native layer IDs.
    pub layers: DecodeLayers,
}

/// The decoder's partition tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockTree {
    /// One tree for luma and chroma.
    Shared,
    /// Separate luma tree.
    Luma,
    /// Separate chroma tree.
    Chroma,
}

/// Identity of one partition or coding block in the selected frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockId {
    /// Tile index in the coded frame.
    pub tile: u32,
    /// Partition tree that owns the block.
    pub tree: BlockTree,
    /// Coded rectangle before clipping.
    pub coded: BlockRect,
}

/// A transform's stable identity in native plane coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockTransformId {
    /// Coding block that owns the transform.
    pub block: BlockId,
    /// Plane that contains the transform.
    pub plane: SamplePlane,
    /// Coded rectangle in samples of the plane.
    pub coded: BlockRect,
}

/// Facts available for an exact transform selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockTransformDetails {
    /// The plane codes one or more nonzero coefficients.
    pub has_coefficients: bool,
    /// The canonical AV2 `PlaneTxType`, if the plane codes coefficients.
    pub plane_tx_type: Option<String>,
    /// Whether the segment is lossless.
    pub lossless: bool,
}

/// Evidence for one decoded transform region.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockTransform {
    /// Stable identity including the native plane and coded size.
    pub id: BlockTransformId,
    /// Coded rectangle projected into the luma grid.
    pub luma_coded: BlockRect,
    /// Details present only for a query for this exact transform.
    pub details: Option<BlockTransformDetails>,
}

/// Facts associated with a partition node or a coding block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BlockData {
    /// The split the decoder used at this partition node.
    Partition {
        /// Native partition name.
        split: String,
    },
    /// Prediction and quantizer of a leaf.
    Coding {
        /// This coding block covers chroma samples.
        has_chroma: bool,
        /// Prediction used by the block, when available.
        prediction: Option<BlockPrediction>,
        /// Effective qindex before plane-specific adjustments.
        qindex: u32,
        /// Segment ID.
        segment: u8,
    },
    /// Resolved motion for the coding block with the same identity.
    Motion {
        /// The motion facts available from the decoder.
        motion: BlockMotion,
    },
    /// A decoded transform region owned by this coding block.
    Transform {
        /// Geometry and optional details.
        transform: BlockTransform,
    },
}

/// One structural decoder record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockRecord {
    /// Tile, tree and coded rectangle. The data kind distinguishes record types.
    pub id: BlockId,
    /// Immediate partition parent, when available. This always identifies a partition.
    pub parent: Option<BlockId>,
    /// Geometry clipped to the visible picture, if it intersects.
    pub visible: Option<BlockRect>,
    /// Observed partition or coding facts.
    pub data: BlockData,
}

/// Coverage of the requested block structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockCoverage {
    /// All matching blocks were captured.
    Complete,
    /// A budget stopped capture. Some records can be available.
    Truncated,
    /// The operation stopped after a cancellation request.
    Cancelled,
    /// No admitted coded frame matched the requested source offset.
    FrameNotFound,
    /// The frame's block structure is unavailable.
    Unavailable,
    /// The decoder failed after it started the capture.
    DecodeFailed,
    /// The decoder could not allocate space for more records.
    AllocationFailed,
}

/// The inspection process's bounded result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockEvidence {
    /// Exact scope and capture limits for these records.
    pub query: BlockQuery,
    /// Native coded-frame scope, if admitted.
    pub frame: Option<BlockFrame>,
    /// Coverage of the query.
    pub coverage: BlockCoverage,
    /// End of the last source unit submitted to the decoder, in file bytes.
    pub replayed_through: u64,
    /// Decoder records, including any requested transform geometry.
    pub records: Vec<BlockRecord>,
    /// Exact coefficient evidence, only for a coefficient query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coefficients: Option<Box<BlockCoefficients>>,
}

impl BlockEvidence {
    /// Checks that a reply agrees with its request and contains valid records.
    #[must_use]
    pub fn matches(&self, query: BlockQuery) -> bool {
        query.is_valid()
            && self.query == query
            && self.records.len() <= query.max_events as usize
            && self.frame.is_none_or(|frame| {
                frame.coded_offset == query.coded_offset
                    && frame.coded_offset < self.replayed_through
            })
            && match (self.coverage, self.frame) {
                (
                    BlockCoverage::Complete | BlockCoverage::Truncated | BlockCoverage::Unavailable,
                    None,
                )
                | (BlockCoverage::FrameNotFound, Some(_)) => false,
                (BlockCoverage::FrameNotFound | BlockCoverage::Unavailable, _) | (_, None) => {
                    self.records.is_empty()
                }
                _ => true,
            }
            && self.coefficients.as_ref().is_none_or(|coefficients| {
                query.kind == BlockEvidenceKind::Coefficients
                    && self.coverage == BlockCoverage::Complete
                    && query.detail == Some(coefficients.id)
                    && self.frame.is_some()
                    && matches!(self.records.as_slice(), [BlockRecord {
                        data: BlockData::Transform { transform }, ..
                    }] if transform.details.as_ref().is_some_and(|details| {
                        details.has_coefficients == (coefficients.eob != 0)
                    }))
                    && coefficients.is_valid()
            })
            && self.records.iter().all(|record| record.matches(query))
    }
}

impl BlockRecord {
    fn matches(&self, query: BlockQuery) -> bool {
        let coded = match &self.data {
            BlockData::Transform { transform } => transform.luma_coded,
            _ => self.id.coded,
        };
        self.id.coded.is_valid()
            && coded.is_valid()
            && query.region.is_none_or(|region| region.intersects(coded))
            && self
                .visible
                .is_none_or(|visible| visible.is_valid() && coded.contains(visible))
            && self.parent.is_none_or(|parent| {
                parent.tile == self.id.tile
                    && parent.coded.is_valid()
                    && parent.coded.contains(coded)
            })
            && match &self.data {
                BlockData::Partition { split } => {
                    matches!(
                        query.kind,
                        BlockEvidenceKind::Structure | BlockEvidenceKind::Partitions
                    ) && valid_label(split)
                }
                BlockData::Coding { prediction, .. } => {
                    matches!(
                        query.kind,
                        BlockEvidenceKind::Structure | BlockEvidenceKind::Coding
                    ) && prediction.as_ref().is_none_or(BlockPrediction::is_valid)
                }
                BlockData::Motion { motion } => {
                    matches!(
                        query.kind,
                        BlockEvidenceKind::Structure | BlockEvidenceKind::Coding
                    ) && motion.is_valid()
                }
                BlockData::Transform { transform } => {
                    matches!(
                        query.kind,
                        BlockEvidenceKind::Transforms | BlockEvidenceKind::Coefficients
                    ) && self.id == transform.id.block
                        && transform.id.coded.is_valid()
                        && match (query.detail, &transform.details) {
                            (None, None) => true,
                            (Some(id), Some(details)) if id == transform.id => details
                                .plane_tx_type
                                .as_deref()
                                .is_none_or(|name| details.has_coefficients && valid_label(name)),
                            _ => false,
                        }
                }
            }
    }
}

fn valid_label(label: &str) -> bool {
    !label.is_empty() && label.len() <= 48 && label.bytes().all(|byte| byte.is_ascii_graphic())
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;

    use super::*;
    use crate::protocol::{Command, Envelope, Event, ProtocolError, read_message, write_message};

    fn query() -> BlockQuery {
        BlockQuery {
            coded_offset: u64::MAX - 1,
            selection: LayerDecodeSelection::Default,
            kind: BlockEvidenceKind::Structure,
            region: None,
            detail: None,
            max_events: MAX_BLOCK_RECORDS,
            max_bytes: MAX_BLOCK_BYTES,
        }
    }

    #[test]
    fn block_payload_fits_the_checked_frame_and_round_trips() -> Result<(), ProtocolError> {
        let query = query();
        let rect = BlockRect {
            x: u32::MAX - 1,
            y: u32::MAX - 1,
            width: 1,
            height: 1,
        };
        let id = BlockId {
            tile: u32::MAX,
            tree: BlockTree::Chroma,
            coded: rect,
        };
        let reference = Some(BlockReference::Frame {
            list_index: u8::MAX,
            slot: u32::MAX,
            scaled: Some(true),
        });
        let record = BlockRecord {
            id,
            parent: Some(id),
            visible: Some(rect),
            data: BlockData::Coding {
                has_chroma: true,
                prediction: Some(BlockPrediction::Inter {
                    mode: "X".repeat(48),
                    references: [reference; 2],
                    precision_eighths: Some(64),
                    differences: [Some(BlockVector {
                        col: i32::MIN,
                        row: i32::MIN,
                    }); 2],
                    interpolation: "Y".repeat(48),
                    inter_intra: Some("Z".repeat(48)),
                    bawp: true,
                    skip_mode: true,
                    adaptive_mvd: true,
                }),
                qindex: u32::MAX,
                segment: u8::MAX,
            },
        };
        let evidence = BlockEvidence {
            query,
            frame: Some(BlockFrame {
                coded_offset: query.coded_offset,
                sequence_offset: u64::MAX - 2,
                layers: DecodeLayers {
                    extended: 31,
                    embedded: 31,
                    temporal: 7,
                },
            }),
            coverage: BlockCoverage::Truncated,
            replayed_through: u64::MAX,
            records: vec![record; MAX_BLOCK_RECORDS as usize],
            coefficients: None,
        };
        assert!(evidence.matches(query));
        let mut malformed = evidence.clone();
        malformed.records[0].visible = Some(BlockRect { x: 0, ..rect });
        assert!(!malformed.matches(query));
        malformed.records[0].visible = Some(rect);
        malformed.query.kind = BlockEvidenceKind::Partitions;
        assert!(!malformed.matches(malformed.query));
        malformed.query = BlockQuery {
            region: Some(BlockRect { x: 0, ..rect }),
            ..query
        };
        assert!(!malformed.matches(malformed.query));
        let invalid = BlockQuery {
            max_events: MAX_BLOCK_RECORDS - 1,
            ..query
        };
        assert!(!evidence.matches(invalid));

        let mut wire = Vec::new();
        write_message(
            &mut wire,
            &Envelope::new(
                u64::MAX,
                Event::AnalysisBlocksReady {
                    document: u64::MAX,
                    evidence: Box::new(evidence.clone()),
                },
            ),
        )?;
        assert!(matches!(
            read_message::<Event>(&mut wire.as_slice())?,
            Some(Envelope { body: Event::AnalysisBlocksReady { document: u64::MAX, evidence: actual }, .. })
                if *actual == evidence
        ));
        Ok(())
    }

    fn full_coefficient_grid() -> BlockEvidence {
        let rect = BlockRect {
            x: 0,
            y: 0,
            width: 32,
            height: 32,
        };
        let block = BlockId {
            tile: 0,
            tree: BlockTree::Shared,
            coded: rect,
        };
        let id = BlockTransformId {
            block,
            plane: SamplePlane::Y,
            coded: rect,
        };
        let query = BlockQuery {
            kind: BlockEvidenceKind::Coefficients,
            detail: Some(id),
            max_events: 1,
            ..query()
        };
        BlockEvidence {
            query,
            frame: Some(BlockFrame {
                coded_offset: query.coded_offset,
                sequence_offset: 0,
                layers: DecodeLayers {
                    extended: 0,
                    embedded: 0,
                    temporal: 0,
                },
            }),
            coverage: BlockCoverage::Complete,
            replayed_through: u64::MAX,
            records: vec![BlockRecord {
                id: block,
                parent: Some(block),
                visible: Some(rect),
                data: BlockData::Transform {
                    transform: BlockTransform {
                        id,
                        luma_coded: rect,
                        details: Some(BlockTransformDetails {
                            has_coefficients: true,
                            plane_tx_type: Some("DCT_DCT".into()),
                            lossless: false,
                        }),
                    },
                },
            }],
            coefficients: Some(Box::new(BlockCoefficients {
                id,
                codec_revision: "a".repeat(40),
                width: 32,
                height: 32,
                domain: BlockCoefficientDomain::Plane,
                quantized: vec![i32::MIN; 1024],
                dequantized: vec![i32::MAX; 1024],
                scan: Some((0..1024).rev().collect()),
                eob: 1024,
                use_tcq: false,
                forward_scan: false,
                dc_parity_hidden: false,
                quantization: Some(BlockCoefficientQuantization {
                    qindex: 255,
                    dc_quant: 8000,
                    ac_quant: 8000,
                    denominator: 4,
                    bit_depth: 10,
                    matrix: None,
                }),
                secondary: None,
            })),
        }
    }

    #[test]
    fn largest_coefficient_result_fits_one_frame_and_round_trips() -> Result<(), ProtocolError> {
        let evidence = full_coefficient_grid();
        let query = evidence.query;
        let id = query
            .detail
            .unwrap_or_else(|| unreachable!("missing transform selection"));
        assert!(evidence.matches(query));
        let wrong_selection = BlockQuery {
            detail: Some(BlockTransformId {
                plane: SamplePlane::U,
                ..id
            }),
            ..query
        };
        assert!(!evidence.matches(wrong_selection));
        let mut duplicate_scan = evidence.clone();
        let Some(capture) = duplicate_scan.coefficients.as_deref_mut() else {
            unreachable!("missing capture")
        };
        capture.scan = Some(vec![0; 1024]);
        assert!(!duplicate_scan.matches(query));
        let mut wire = Vec::new();
        write_message(
            &mut wire,
            &Envelope::new(
                1,
                Event::AnalysisBlocksReady {
                    document: 1,
                    evidence: Box::new(evidence.clone()),
                },
            ),
        )?;
        assert!(matches!(
            read_message::<Event>(&mut wire.as_slice())?,
            Some(Envelope { body: Event::AnalysisBlocksReady { document: 1, evidence: actual }, .. })
                if *actual == evidence
        ));
        Ok(())
    }

    #[test]
    fn unknown_kind_and_invalid_regions_or_budgets_are_rejected() -> Result<(), ProtocolError> {
        let mut wire = Vec::new();
        let query = query();
        write_message(
            &mut wire,
            &Envelope::new(1, Command::AnalysisBlocks { document: 1, query }),
        )?;
        let body = std::str::from_utf8(&wire[4..])
            .unwrap_or_default()
            .replace("structure", "unexpected");
        let len = u32::try_from(body.len()).unwrap_or_default();
        assert!(matches!(
            read_message::<Command>(&mut len.to_le_bytes().as_slice().chain(body.as_bytes())),
            Err(ProtocolError::Malformed(_))
        ));
        assert!(
            !BlockQuery {
                max_bytes: MAX_BLOCK_BYTES + 1,
                ..query
            }
            .is_valid()
        );
        assert!(
            !BlockQuery {
                max_events: 0,
                ..query
            }
            .is_valid()
        );
        assert!(
            !BlockQuery {
                region: Some(BlockRect {
                    x: u32::MAX,
                    y: 0,
                    width: 2,
                    height: 1
                }),
                ..query
            }
            .is_valid()
        );
        Ok(())
    }
}
