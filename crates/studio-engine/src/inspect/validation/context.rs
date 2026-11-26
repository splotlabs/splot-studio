// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use std::sync::atomic::AtomicBool;

use splot_validate::{ExternalHlsMode, ExternalHlsObjects, ValidationOptions};
use studio_domain::analysis::AnalysisFailure;
use studio_domain::validation::{ContextIdentity, MAX_EXTERNAL_HLS_BYTES};

use super::Refusal;

pub(super) fn options(
    bytes: Option<&[u8]>,
) -> Result<(ValidationOptions, Option<ContextIdentity>), Refusal> {
    let Some(mut bytes) = bytes else {
        return Ok((ValidationOptions::default(), None));
    };
    if bytes.is_empty() || bytes.len() > MAX_EXTERNAL_HLS_BYTES {
        return Err(Refusal::new(
            AnalysisFailure::InvalidRequest,
            format!("Supply 1 to {MAX_EXTERNAL_HLS_BYTES} bytes of external AV2 context."),
        ));
    }
    let objects = ExternalHlsObjects::from_annex_b(bytes).map_err(|error| {
        Refusal::new(
            AnalysisFailure::InvalidRequest,
            format!("Invalid external AV2 context. {error}"),
        )
    })?;
    let sha256 = crate::artifact::hash_source(&mut bytes, &AtomicBool::new(false))
        .map_err(|error| Refusal::new(AnalysisFailure::Io, error.to_string()))?;
    let identity = ContextIdentity {
        sha256,
        sequence_headers: objects
            .sequence_headers()
            .map(|(layer, id)| (layer.get(), u32::from(id.get())))
            .collect(),
        operating_point_sets: objects
            .operating_point_sets()
            .map(|(layer, id)| (layer.get(), id))
            .collect(),
    };
    Ok((
        ValidationOptions {
            external_hls: ExternalHlsMode::Objects(objects),
        },
        Some(identity),
    ))
}
