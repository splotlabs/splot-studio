// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Exact signed coefficient values at two decoder stages.

use serde::{Deserialize, Serialize};

use super::BlockTransformId;
use crate::preview::SamplePlane;

/// The component basis of the captured values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "basis", rename_all = "snake_case")]
pub enum BlockCoefficientDomain {
    /// The native plane named by the transform identity.
    Plane,
    /// The coded component before the inverse cross-component transform.
    CrossComponent {
        /// The cross-component mode signaled in the U transform slot.
        mode: u8,
    },
}

/// The bounded coefficient grid of a selected transform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockCoefficients {
    /// Coding block, plane and coded transform rectangle.
    pub id: BlockTransformId,
    /// Full Git revision of the Splot decoder that captured both grids.
    pub codec_revision: String,
    /// Number of horizontal coefficient positions.
    pub width: u8,
    /// Number of vertical coefficient positions.
    pub height: u8,
    /// Component basis of both grids.
    pub domain: BlockCoefficientDomain,
    /// Signed decoded integer levels in row-major order.
    pub quantized: Vec<i32>,
    /// Signed integer dequantizer output in row-major order, before inverse transforms.
    pub dequantized: Vec<i32>,
    /// Raster position for each scan position. Absent when the decoder used no scan.
    pub scan: Option<Vec<u16>>,
    /// Number of coded scan positions. Forward skip coding uses a suffix.
    pub eob: u16,
    /// The residual used trellis-coded quantization.
    pub use_tcq: bool,
    /// The coefficient parser used forward skip coding.
    pub forward_scan: bool,
    /// The decoder derived DC magnitude parity from the other levels.
    pub dc_parity_hidden: bool,
    /// Dequantizer inputs when the decoder applied dequantization.
    pub quantization: Option<BlockCoefficientQuantization>,
    /// Signaled secondary transform syntax when present.
    pub secondary: Option<BlockSecondaryTransform>,
}

impl BlockCoefficients {
    pub(super) fn is_valid(&self) -> bool {
        let size = usize::from(self.width) * usize::from(self.height);
        if self.codec_revision.len() != 40
            || !self
                .codec_revision
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
            || u32::from(self.width) != self.id.coded.width.min(32)
            || u32::from(self.height) != self.id.coded.height.min(32)
            || matches!(self.domain, BlockCoefficientDomain::CrossComponent { mode }
                if mode == 0 || self.id.plane == SamplePlane::Y)
            || size == 0
            || self.quantized.len() != size
            || self.dequantized.len() != size
            || usize::from(self.eob) > size
            || self.quantization.is_some() != (self.eob > 0)
            || self
                .quantization
                .is_some_and(|q| q.denominator == 0 || !matches!(q.bit_depth, 8 | 10 | 12))
        {
            return false;
        }
        if let Some(scan) = &self.scan {
            let mut seen = [false; 1024];
            self.eob > 0
                && scan.len() == size
                && scan.iter().all(|&index| {
                    let index = usize::from(index);
                    index < size && !std::mem::replace(&mut seen[index], true)
                })
        } else {
            self.eob == 0
                && self
                    .quantized
                    .iter()
                    .chain(&self.dequantized)
                    .all(|&value| value == 0)
        }
    }
}

/// Inputs to the decoder's coefficient dequantizer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockCoefficientQuantization {
    /// Effective block quantizer index before plane adjustments.
    pub qindex: u32,
    /// DC quantizer scale in eighths before matrix weighting.
    pub dc_quant: u32,
    /// AC quantizer scale in eighths before matrix weighting.
    pub ac_quant: u32,
    /// Denominator applied by the dequantizer.
    pub denominator: u32,
    /// Source sample precision, in bits.
    pub bit_depth: u8,
    /// Resolved matrix selection.
    pub matrix: Option<BlockCoefficientMatrix>,
}

/// Matrix selection that the decoder applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockCoefficientMatrix {
    /// Resolved matrix level.
    pub level: u8,
    /// The matrix came from stream-defined weights.
    pub user_defined: bool,
}

/// Signaled AV2 secondary transform syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockSecondaryTransform {
    /// Signaled AV2 `SecTxType` value. Zero selects no secondary transform.
    pub sec_tx_type: u8,
    /// Signaled most probable secondary transform set, when present.
    pub most_probable_stx_set: Option<u8>,
}
