// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use std::fmt::Debug;

use splot_decode::block_evidence::{
    BlockChromaFromLuma, BlockChromaPrediction, BlockCompoundBlend, BlockInterMode,
    BlockLumaPrediction, BlockMotion as NativeMotion, BlockMotionPrecision, BlockMotionRefinement,
    BlockPrediction as NativePrediction, BlockReference as NativeReference, CompoundMode,
    InterMode, MotionVector,
};
use studio_domain::blocks::{
    BlockBlend, BlockMotion, BlockPrediction, BlockReference, BlockRefinement, BlockVector,
    IntraPredictor,
};

use super::{Refusal, unsupported, wire_rect};

pub(super) fn prediction(native: NativePrediction) -> Option<BlockPrediction> {
    Some(match native {
        NativePrediction::Intra(intra) => BlockPrediction::Intra {
            luma: intra.luma.and_then(luma),
            chroma: intra.chroma.and_then(chroma),
        },
        NativePrediction::Inter(inter) => BlockPrediction::Inter {
            mode: inter_mode(inter.mode)?,
            references: inter
                .references
                .map(|reference| reference.and_then(reference_of)),
            precision_eighths: inter.precision.and_then(precision),
            differences: inter
                .motion_differences
                .map(|difference| difference.map(vector)),
            interpolation: format!("{:?}", inter.interpolation),
            inter_intra: label(inter.inter_intra),
            bawp: inter.bawp,
            skip_mode: inter.skip_mode,
            adaptive_mvd: inter.adaptive_mvd,
        },
        NativePrediction::IntraBlockCopy => BlockPrediction::IntraBlockCopy,
        _ => return None,
    })
}

fn luma(prediction: BlockLumaPrediction) -> Option<IntraPredictor> {
    Some(match prediction {
        BlockLumaPrediction::Mode {
            mode,
            direction,
            dpcm,
            ..
        } => IntraPredictor::Mode {
            name: format!("{mode:?}"),
            angle_delta: direction.map(|direction| direction.angle_delta),
            angle_degrees: direction.map(|direction| direction.angle_degrees),
            reference_line: direction.map(|direction| direction.reference_line),
            above_reference_line: direction.map(|direction| direction.above_reference_line),
            secondary_reference_line: direction.map(|direction| direction.secondary_reference_line),
            dpcm: label(dpcm),
        },
        BlockLumaPrediction::Palette { color_count, .. } => IntraPredictor::Palette {
            colors: color_count,
        },
        BlockLumaPrediction::Dip {
            mode, transpose, ..
        } => IntraPredictor::Dip { mode, transpose },
        _ => return None,
    })
}

fn chroma(prediction: BlockChromaPrediction) -> Option<IntraPredictor> {
    Some(match prediction {
        BlockChromaPrediction::Mode {
            mode,
            angle_delta,
            dpcm,
            ..
        } => IntraPredictor::Mode {
            name: format!("{mode:?}"),
            angle_delta,
            angle_degrees: None,
            reference_line: None,
            above_reference_line: None,
            secondary_reference_line: None,
            dpcm: label(dpcm),
        },
        BlockChromaPrediction::Cfl(mode) => {
            let (mode, alpha) = match mode {
                BlockChromaFromLuma::Explicit {
                    alpha_u, alpha_v, ..
                } => ("Explicit", Some([alpha_u, alpha_v])),
                BlockChromaFromLuma::DerivedAlpha => ("DerivedAlpha", None),
                BlockChromaFromLuma::MultiDirect => ("MultiDirect", None),
                BlockChromaFromLuma::MultiAbove => ("MultiAbove", None),
                BlockChromaFromLuma::MultiLeft => ("MultiLeft", None),
                _ => return None,
            };
            IntraPredictor::ChromaFromLuma {
                mode: mode.to_owned(),
                alpha,
            }
        }
        _ => return None,
    })
}

fn inter_mode(mode: BlockInterMode) -> Option<String> {
    Some(match mode {
        BlockInterMode::Single(mode) => match mode {
            InterMode::Near => "NEARMV",
            InterMode::Global => "GLOBALMV",
            InterMode::New => "NEWMV",
        }
        .to_owned(),
        BlockInterMode::Compound(mode) => match mode {
            CompoundMode::NearNear => "NEAR_NEARMV",
            CompoundMode::NearNew => "NEAR_NEWMV",
            CompoundMode::NewNear => "NEW_NEARMV",
            CompoundMode::GlobalGlobal => "GLOBAL_GLOBALMV",
            CompoundMode::JointNew => "JOINT_NEWMV",
            CompoundMode::NewNew => "NEW_NEWMV",
        }
        .to_owned(),
        BlockInterMode::Warp(source) => format!("Warp/{source:?}"),
        BlockInterMode::BlockRefresh => "BlockRefresh".to_owned(),
        _ => return None,
    })
}

fn vector(vector: MotionVector) -> BlockVector {
    BlockVector {
        col: vector.col,
        row: vector.row,
    }
}

fn label(value: Option<impl Debug>) -> Option<String> {
    value.map(|value| format!("{value:?}"))
}

fn reference_of(reference: NativeReference) -> Option<BlockReference> {
    Some(match reference {
        NativeReference::Frame {
            list_index,
            slot,
            scaled,
            ..
        } => BlockReference::Frame {
            list_index,
            slot,
            scaled,
        },
        NativeReference::Tip => BlockReference::Tip,
        _ => return None,
    })
}

fn precision(precision: BlockMotionPrecision) -> Option<u8> {
    Some(match precision {
        BlockMotionPrecision::EightPel => 64,
        BlockMotionPrecision::FourPel => 32,
        BlockMotionPrecision::TwoPel => 16,
        BlockMotionPrecision::OnePel => 8,
        BlockMotionPrecision::HalfPel => 4,
        BlockMotionPrecision::QuarterPel => 2,
        BlockMotionPrecision::EighthPel => 1,
        _ => return None,
    })
}

pub(super) fn motion(motion: NativeMotion) -> Result<BlockMotion, Refusal> {
    Ok(match motion {
        NativeMotion::Inter(inter) => BlockMotion::Inter {
            vectors: inter.vectors.map(|vector_value| vector_value.map(vector)),
            warp: inter.warp,
            refinement: refinement(inter.refinement),
            blend: inter.blend.and_then(blend),
        },
        NativeMotion::IntraBlockCopy(copy) => BlockMotion::IntraBlockCopy {
            vector: vector(copy.vector),
            precision_eighths: copy.precision.and_then(precision),
            difference: copy.difference.map(vector),
            morph: copy.morph,
            source: copy.source.map(wire_rect).transpose()?,
        },
        _ => {
            return Err(unsupported(
                "The decoder returned an unknown motion record.",
            ));
        }
    })
}

fn refinement(refinement: BlockMotionRefinement) -> Option<BlockRefinement> {
    Some(match refinement {
        BlockMotionRefinement::None => BlockRefinement::None,
        BlockMotionRefinement::Tip => BlockRefinement::Tip,
        BlockMotionRefinement::Compound {
            refine_mv,
            optical_flow,
            ..
        } => BlockRefinement::Compound {
            refine_mv,
            optical_flow,
        },
        _ => return None,
    })
}

fn blend(blend: BlockCompoundBlend) -> Option<BlockBlend> {
    Some(match blend {
        BlockCompoundBlend::Average {
            implicit_mask,
            weight,
            ..
        } => BlockBlend::Average {
            implicit_mask,
            weight,
        },
        BlockCompoundBlend::DifferenceWeighted { inverse, .. } => {
            BlockBlend::DifferenceWeighted { inverse }
        }
        BlockCompoundBlend::Wedge { index, sign, .. } => BlockBlend::Wedge { index, sign },
        _ => return None,
    })
}
