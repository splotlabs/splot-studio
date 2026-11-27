// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use splot_decode::block_evidence::{
    DecodeStage as NativeStage, StageEvidence, StageEvidenceStatus as NativeStatus,
};
use splot_decode::{Picture, PictureSamples, PlaneId, PlaneRect, PlaneRef, ReconSample};
use studio_domain::preview::{SamplePlane, SampleRect};
use studio_domain::stages::{DecodeStage, StageSamples, StageScope, StageStatus};

use super::{Refusal, decode_failed, unsupported};

pub(super) fn region(r: SampleRect) -> Result<PlaneRect, Refusal> {
    PlaneRect::new(
        r.x as usize,
        r.y as usize,
        r.width as usize,
        r.height as usize,
    )
    .map_err(|error| unsupported(error.to_string()))
}

pub(super) const fn plane(plane: SamplePlane) -> PlaneId {
    match plane {
        SamplePlane::Y => PlaneId::Y,
        SamplePlane::U => PlaneId::U,
        SamplePlane::V => PlaneId::V,
    }
}

fn wire(rect: PlaneRect) -> Result<SampleRect, Refusal> {
    let r = super::wire_rect(rect)?;
    Ok(SampleRect {
        x: r.x,
        y: r.y,
        width: r.width,
        height: r.height,
    })
}

pub(super) fn evidence(scope: StageScope, stage: StageEvidence) -> Result<StageSamples, Refusal> {
    if stage.plane != plane(scope.plane) {
        return Err(unsupported(
            "The decoder returned samples for another plane.",
        ));
    }
    let kind = match stage.stage {
        NativeStage::Prediction => DecodeStage::Prediction,
        NativeStage::Residual => DecodeStage::Residual,
        NativeStage::Reconstruction => DecodeStage::Reconstruction,
        NativeStage::Deblock => DecodeStage::Deblock,
        NativeStage::Cdef => DecodeStage::Cdef,
        NativeStage::Ccso => DecodeStage::Ccso,
        NativeStage::Restoration => DecodeStage::Restoration,
        NativeStage::Gdf => DecodeStage::Gdf,
        NativeStage::Reference => DecodeStage::Reference,
        _ => return Err(unsupported("The decoder returned an unknown stage.")),
    };
    let status = match stage.status {
        NativeStatus::Applied => StageStatus::Applied,
        NativeStatus::Skipped => StageStatus::Skipped,
        NativeStatus::Unsupported => StageStatus::Unsupported,
        NativeStatus::Uncaptured => StageStatus::Uncaptured,
        _ => return Err(unsupported("The decoder returned an unknown stage status.")),
    };
    Ok(StageSamples {
        stage: kind,
        status,
        bit_depth: stage.bit_depth.bits(),
        region: stage.region.map(wire).transpose()?,
        plane_extent: Some(wire(stage.plane_extent)?),
        samples: stage.samples,
    })
}

/// Applies the decoder's presentation policy to a picture with verified direct provenance.
pub(super) fn grain(
    scope: StageScope,
    picture: &Picture,
    remaining: usize,
) -> Result<StageSamples, Refusal> {
    let mut value = StageSamples {
        stage: DecodeStage::Grain,
        status: StageStatus::Skipped,
        bit_depth: picture.bit_depth().bits(),
        region: None,
        plane_extent: None,
        samples: Vec::new(),
    };
    let id = plane(scope.plane);
    let extent = match picture.plane(id) {
        Some(PictureSamples::Eight(plane)) => plane.visible_rect(),
        Some(PictureSamples::Ten(plane)) => plane.visible_rect(),
        None => {
            value.status = StageStatus::Unsupported;
            return Ok(value);
        }
    };
    let extent = wire(extent)?;
    value.plane_extent = Some(extent);
    if !picture.has_film_grain() {
        return Ok(value);
    }

    value.status = StageStatus::Uncaptured;
    let Some(region) = intersection(scope.region, extent) else {
        return Ok(value);
    };
    let bytes = region.width as usize * region.height as usize * size_of::<i32>();
    if bytes > remaining {
        return Ok(value);
    }
    let display = picture
        .apply_film_grain()
        .map_err(|error| decode_failed(&error))?;
    value.samples = match display.plane(id) {
        Some(PictureSamples::Eight(plane)) => crop(region, extent, plane),
        Some(PictureSamples::Ten(plane)) => crop(region, extent, plane),
        None => None,
    }
    .ok_or_else(|| unsupported("The decoder changed the grain plane geometry."))?;
    value.status = StageStatus::Applied;
    value.region = Some(region);
    Ok(value)
}

fn intersection(region: SampleRect, extent: SampleRect) -> Option<SampleRect> {
    let x = region.x.max(extent.x);
    let y = region.y.max(extent.y);
    let end_x = (region.x + region.width).min(extent.x.checked_add(extent.width)?);
    let end_y = (region.y + region.height).min(extent.y.checked_add(extent.height)?);
    Some(SampleRect {
        x,
        y,
        width: end_x.checked_sub(x).filter(|width| *width > 0)?,
        height: end_y.checked_sub(y).filter(|height| *height > 0)?,
    })
}

fn crop<T: ReconSample + Into<i32>>(
    region: SampleRect,
    extent: SampleRect,
    plane: PlaneRef<'_, T>,
) -> Option<Vec<i32>> {
    if wire(plane.visible_rect()).ok()? != extent {
        return None;
    }
    let left = (region.x - extent.x) as usize;
    let top = (region.y - extent.y) as usize;
    let right = left + region.width as usize;
    Some(
        plane
            .visible_rows()
            .skip(top)
            .take(region.height as usize)
            .flat_map(|row| row[left..right].iter().copied())
            .map(Into::into)
            .collect(),
    )
}
