// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! One selected transform's validated coefficient data and export snapshot.

use serde::Serialize;
use studio_domain::analysis::SourceDigest;
use studio_domain::blocks::{
    BlockCoefficients, BlockData, BlockEvidence, BlockFrame, BlockTransformDetails,
};

#[derive(Serialize)]
struct Report<'a> {
    schema: &'static str,
    version: u8,
    frame: BlockFrame,
    layer_selection: studio_domain::layers::LayerDecodeSelection,
    source_name: String,
    source_digest: &'a SourceDigest,
    transform: Option<&'a BlockTransformDetails>,
    primary_transform_provenance: &'static str,
    orientation: &'static str,
    scan_mapping: &'static str,
    quantized_stage: &'static str,
    dequantized_stage: &'static str,
    scale: u8,
    storage: &'static str,
    dc_ac_units: &'static str,
    matrix_orientation: &'static str,
    values: &'a BlockCoefficients,
}

#[derive(Debug)]
pub(crate) struct CoefficientCapture {
    pub(super) evidence: Box<BlockEvidence>,
    pub(crate) report: slint::SharedString,
    pub(crate) scan_at: [u16; 1024],
}

impl CoefficientCapture {
    pub(super) fn new(evidence: Box<BlockEvidence>, document: &super::Document) -> Option<Self> {
        let values = evidence.coefficients.as_deref()?;
        let frame = evidence.frame?;
        let digest = document.summary()?.digest.as_deref()?;
        let transform = evidence
            .records
            .first()
            .and_then(|record| match &record.data {
                BlockData::Transform { transform } => transform.details.as_ref(),
                _ => None,
            });
        let report = serde_json::to_string(&Report {
            schema: "splot-studio-transform-coefficients",
            version: 1,
            frame,
            layer_selection: evidence.query.selection,
            source_name: crate::present::file_name(document.path()),
            source_digest: digest,
            transform,
            primary_transform_provenance: "decoder_resolved",
            orientation: "row_major_y_then_x_top_left_origin",
            scan_mapping: "zero_based_scan_position_to_zero_based_raster_index",
            quantized_stage: "decoded_residual_levels",
            dequantized_stage: "dequantizer_output_before_inverse_secondary_cross_component_or_primary_transform",
            scale: 1,
            storage: "signed_i32",
            dc_ac_units: "eighths_before_matrix_weighting",
            matrix_orientation: "same_rows_and_columns_as_coefficient_grid",
            values,
        }).ok()?;
        let mut scan_at = [0; 1024];
        if let Some(scan) = &values.scan {
            for (order, raster) in (0_u16..).zip(scan) {
                scan_at[usize::from(*raster)] = order;
            }
        }
        Some(Self {
            evidence,
            report: report.into(),
            scan_at,
        })
    }
}
