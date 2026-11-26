// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Prediction and motion facts, in native AV2 units.

use serde::{Deserialize, Serialize};

use super::{BlockRect, valid_label};

/// Prediction used by a coding block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BlockPrediction {
    /// Prediction from the current picture.
    Intra {
        /// Luma predictor, when available.
        luma: Option<IntraPredictor>,
        /// Chroma predictor, when available.
        chroma: Option<IntraPredictor>,
    },
    /// Prediction from reference pictures.
    Inter {
        /// Name of the native inter mode.
        mode: String,
        /// Reference identities in predictor order.
        references: [Option<BlockReference>; 2],
        /// Precision before refinement, in eighths of a luma sample.
        precision_eighths: Option<u8>,
        /// Decoded motion-vector differences in predictor order.
        differences: [Option<BlockVector>; 2],
        /// The native name of the interpolation filter.
        interpolation: String,
        /// The intra predictor if the block uses inter-intra.
        inter_intra: Option<String>,
        /// Block-adaptive weighted prediction was used.
        bawp: bool,
        /// Inter skip mode was used.
        skip_mode: bool,
        /// Adaptive motion-vector difference coding was used.
        adaptive_mvd: bool,
    },
    /// Prediction from a copied region in the current picture.
    IntraBlockCopy,
}

impl BlockPrediction {
    pub(super) fn is_valid(&self) -> bool {
        match self {
            Self::Intra { luma, chroma } => luma.iter().chain(chroma).all(IntraPredictor::is_valid),
            Self::Inter {
                mode,
                interpolation,
                inter_intra,
                precision_eighths,
                ..
            } => {
                valid_label(mode)
                    && valid_label(interpolation)
                    && inter_intra.as_deref().is_none_or(valid_label)
                    && precision_eighths.is_none_or(valid_precision)
            }
            Self::IntraBlockCopy => true,
        }
    }
}

/// The decoded intra predictor for one component group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IntraPredictor {
    /// A directional or non-directional intra mode.
    Mode {
        /// The native mode name.
        name: String,
        /// Directional angle delta and its source, when available.
        angle_delta: Option<IntraAngleDelta>,
        /// Luma prediction angle in the block plan, in degrees.
        angle_degrees: Option<u16>,
        /// Selected luma reference line.
        reference_line: Option<u8>,
        /// Effective above reference line after adjustment.
        above_reference_line: Option<u8>,
        /// The luma predictor uses secondary reference-line blending.
        secondary_reference_line: Option<bool>,
        /// Differential pulse-code modulation direction, if used.
        dpcm: Option<String>,
    },
    /// Palette prediction.
    Palette {
        /// Number of colors.
        colors: u8,
    },
    /// Data-driven intra prediction.
    Dip {
        /// Mode index.
        mode: u8,
        /// The decoder transposes the predictor.
        transpose: bool,
    },
    /// Chroma prediction from luma.
    ChromaFromLuma {
        /// The selected native predictor.
        mode: String,
        /// Explicit U and V alpha parameters, if present.
        alpha: Option<[i8; 2]>,
    },
}

/// Source and value of an intra angle delta in steps of three degrees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", content = "delta", rename_all = "snake_case")]
pub enum IntraAngleDelta {
    /// Value from the decoded joint luma mode.
    DecodedLuma(i8),
    /// The chroma mode uses the luma angle delta.
    ChromaFromLuma(i8),
    /// The decoder derives the chroma angle delta.
    ChromaDerived(i8),
    /// DPCM uses a zero delta.
    DpcmZero,
}

impl IntraPredictor {
    fn is_valid(&self) -> bool {
        match self {
            Self::Mode { name, dpcm, .. } => {
                valid_label(name) && dpcm.as_deref().is_none_or(valid_label)
            }
            Self::ChromaFromLuma { mode, .. } => valid_label(mode),
            Self::Palette { .. } | Self::Dip { .. } => true,
        }
    }
}

/// Reference identity inside the frame that contains the block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BlockReference {
    /// A frame in the decoded picture buffer.
    Frame {
        /// Index in the current frame's reference list.
        list_index: u8,
        /// Slot in the input snapshot for this coded frame.
        slot: u32,
        /// Whether the frame uses scaling, if known.
        scaled: Option<bool>,
    },
    /// A synthetic temporal interpolated reference.
    Tip,
}

/// Signed vector components, in eighths of a luma sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockVector {
    /// Horizontal component.
    pub col: i32,
    /// Vertical component.
    pub row: i32,
}

/// Resolved motion from the native decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BlockMotion {
    /// Resolved base inter motion before subblock refinement.
    Inter {
        /// Base vectors in predictor order.
        vectors: [Option<BlockVector>; 2],
        /// Affine models in native AV2 `WarpParams` units.
        warp: [Option<[i32; 6]>; 2],
        /// Subblock refinement planned by the native predictor.
        refinement: Option<BlockRefinement>,
        /// The resolved blend, if this is an ordinary compound block.
        blend: Option<BlockBlend>,
    },
    /// Motion that copies samples from the current picture.
    IntraBlockCopy {
        /// Resolved block vector.
        vector: BlockVector,
        /// Decoded block-vector precision, in eighths of a luma sample.
        precision_eighths: Option<u8>,
        /// Decoded vector difference, if present.
        difference: Option<BlockVector>,
        /// Morph prediction uses the copied samples.
        morph: bool,
        /// Exact luma source rectangle for a direct integer copy.
        source: Option<BlockRect>,
    },
}

impl BlockMotion {
    pub(super) fn is_valid(self) -> bool {
        match self {
            Self::Inter { .. } => true,
            Self::IntraBlockCopy {
                precision_eighths,
                source,
                ..
            } => {
                source.is_none_or(BlockRect::is_valid)
                    && precision_eighths.is_none_or(valid_precision)
            }
        }
    }
}

fn valid_precision(precision: u8) -> bool {
    matches!(precision, 1 | 2 | 4 | 8 | 16 | 32 | 64)
}

/// Later motion derivation in the prediction plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BlockRefinement {
    /// The plan uses no later subblock step.
    None,
    /// The plan derives motion for a synthetic TIP reference.
    Tip,
    /// The plan uses compound subblock derivation.
    Compound {
        /// The plan enables refine-MV.
        refine_mv: bool,
        /// The plan permits optical flow.
        optical_flow: bool,
    },
}

/// The resolved blend of two reference predictors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BlockBlend {
    /// Average with native weights.
    Average {
        /// The blend uses an implicit mask.
        implicit_mask: bool,
        /// Native AV2 weight for the first predictor.
        weight: i16,
    },
    /// Difference-weighted mask.
    DifferenceWeighted {
        /// The decoder inverts the mask.
        inverse: bool,
    },
    /// Wedge mask.
    Wedge {
        /// Wedge index.
        index: u8,
        /// The decoder inverts the mask.
        sign: bool,
    },
}
