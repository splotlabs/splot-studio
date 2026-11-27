// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Maps native decoder entropy observations to Studio's bounded protocol.

use splot_decode::block_evidence::{
    BlockEntropyEvidence, BlockEntropyScope, ENTROPY_SCHEMA_VERSION, ENTROPY_SPEC_VERSION,
    EntropyCoding as NativeCoding, EntropySymbol as NativeSymbol,
};
use studio_domain::blocks::{
    BlockEvidenceKind, BlockTransformId, EntropyCdf, EntropyCoding, EntropyContext,
    EntropyEvidence, EntropyScope, EntropySymbol, MICROBITS_PER_BIT,
};

use super::{Refusal, block_id, native_block, native_transform, transform_id, unsupported};

pub(super) fn native_scope(scope: EntropyScope) -> Result<BlockEntropyScope, Refusal> {
    Ok(match scope {
        EntropyScope::Block(block) => BlockEntropyScope::Block(native_block(block)?),
        EntropyScope::Transform(transform) => {
            BlockEntropyScope::Transform(native_transform(transform)?)
        }
    })
}

pub(super) fn wire_evidence(
    native: &BlockEntropyEvidence,
    kind: BlockEvidenceKind,
) -> Result<EntropyEvidence, Refusal> {
    let BlockEvidenceKind::Entropy { start, .. } = kind else {
        return Err(unsupported(
            "The decoder returned entropy evidence for another query.",
        ));
    };
    let scope = match native.scope {
        BlockEntropyScope::Block(block) => EntropyScope::Block(block_id(block)?),
        BlockEntropyScope::Transform(transform) => {
            EntropyScope::Transform(transform_id(transform)?)
        }
        _ => {
            return Err(unsupported(
                "The decoder returned an unknown entropy scope.",
            ));
        }
    };
    Ok(EntropyEvidence {
        schema: ENTROPY_SCHEMA_VERSION,
        spec: ENTROPY_SPEC_VERSION.to_owned(),
        codec_revision: env!("SPLOT_CODEC_REVISION").to_owned(),
        scope,
        start,
        tile_payload_bytes: native.tile_payload_bytes,
        end_ordinal: native.end_ordinal,
        symbols: native
            .symbols
            .iter()
            .copied()
            .enumerate()
            .map(|(index, symbol)| {
                wire_symbol(
                    symbol,
                    native.transform_for(index).map(transform_id).transpose()?,
                )
            })
            .collect::<Result<_, _>>()?,
    })
}

// The estimated bit count is nonnegative and bounded by the 16-bit range or bypass width.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn wire_symbol(
    native: NativeSymbol,
    transform: Option<BlockTransformId>,
) -> Result<EntropySymbol, Refusal> {
    let coding = match native.coding {
        NativeCoding::Cdf => EntropyCoding::Cdf,
        NativeCoding::Bypass => EntropyCoding::Bypass,
        NativeCoding::Unary => EntropyCoding::Unary,
        _ => {
            return Err(unsupported(
                "The decoder returned an unknown entropy operation.",
            ));
        }
    };
    let cdf = native
        .cdf
        .map(|cdf| {
            let cutoffs_q15 = cdf
                .arity
                .checked_sub(1)
                .and_then(|len| cdf.cutoffs.get(..usize::from(len)))
                .ok_or_else(|| unsupported("The decoder returned an invalid CDF size."))?
                .to_vec();
            Ok(EntropyCdf {
                cutoffs_q15,
                adaptation_count: cdf.adaptation_count,
                rate_index: cdf.rate_index,
                updates: cdf.updates,
            })
        })
        .transpose()?;
    let estimated_microbits = native
        .estimated_bits()
        .map(|bits| (bits * f64::from(MICROBITS_PER_BIT)).round() as u32);
    Ok(EntropySymbol {
        ordinal: native.ordinal,
        transform,
        syntax: native.syntax.map(|syntax| syntax.key.to_owned()),
        context: native
            .syntax
            .filter(|syntax| !syntax.context.is_empty())
            .map(|syntax| EntropyContext {
                names: syntax.context.to_owned(),
                values: syntax.values,
            }),
        coding,
        value: native.value,
        bypass_bits: (coding != EntropyCoding::Cdf).then_some(native.bypass_bits),
        cdf,
        estimated_microbits,
        range: native.range,
        interval: native.interval,
    })
}
