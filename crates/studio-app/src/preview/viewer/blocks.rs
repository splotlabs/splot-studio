// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Maps structural decoder evidence to the painted picture.

use studio_domain::layers::LayerDecodeSelection;
use studio_domain::preview::{
    OutputEvent, PictureHeader, PictureTarget, PictureView, SamplePlane, SampleRect,
};

use super::{Preview, plane, plane_rect, position_from_paint, subsampling};

/// The displayed output and the view into its coded frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BlockPicture {
    pub(crate) document: u64,
    pub(crate) output: OutputEvent,
    pub(crate) selection: LayerDecodeSelection,
    pub(crate) viewport: BlockViewport,
}

impl Preview {
    /// Returns the accepted picture only when it can be inspected.
    pub(crate) fn block_picture(&self) -> Option<BlockPicture> {
        let (header, document, selection) = self.block_picture_header()?;
        let output = header.output?;
        output.coded_offset?;
        output.sequence_offset?;
        Some(BlockPicture {
            document,
            output,
            selection,
            viewport: BlockViewport::from_header(header)?,
        })
    }

    /// Convert a click into a point in the displayed frame's coded luma grid.
    pub(crate) fn block_point(
        &self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Option<(u32, u32)> {
        let (header, _, _) = self.block_picture_header()?;
        BlockViewport::from_header(header)?.point(header, x, y, width, height)
    }

    fn block_picture_header(&self) -> Option<(&PictureHeader, u64, LayerDecodeSelection)> {
        if !self.can_sample() {
            return None;
        }
        let shown = self.shown.as_ref()?;
        let target = shown.target.as_ref()?;
        let PictureTarget::Document {
            document,
            selection,
            ..
        } = target
        else {
            return None;
        };
        self.on_side(target)
            .then_some((&shown.picture.header, *document, *selection))
    }
}

/// The painted crop in plane coordinates and its projection into coded luma.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BlockViewport {
    plane: SamplePlane,
    visible_plane: SampleRect,
    visible_luma: SampleRect,
    crop: SampleRect,
    scale: (u32, u32),
}

/// A rectangle in the painted sample grid. Keep edges past the crop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProjectedRect {
    pub(crate) x: i64,
    pub(crate) y: i64,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

impl BlockViewport {
    /// Use the accepted picture's crop. Fit buffers can contain fewer samples.
    pub(crate) fn from_header(header: &PictureHeader) -> Option<Self> {
        let info = *header.document_info.as_deref()?;
        let plane = plane(header.options.surface);
        let visible_plane = plane_rect(info, plane)?;
        let (x, y, width, height) = match header.view {
            PictureView::Fit => (0, 0, visible_plane.width, visible_plane.height),
            PictureView::Native { x, y } | PictureView::Zoom { x, y, .. } => {
                (x, y, u32::from(header.width), u32::from(header.height))
            }
        };
        Some(Self {
            plane,
            visible_plane,
            visible_luma: info.y,
            crop: SampleRect {
                x: visible_plane.x.checked_add(x)?,
                y: visible_plane.y.checked_add(y)?,
                width,
                height,
            },
            scale: if plane == SamplePlane::Y {
                (1, 1)
            } else {
                subsampling(info.chroma)
            },
        })
    }

    /// Plane that the viewer shows.
    pub(crate) const fn plane(self) -> SamplePlane {
        self.plane
    }

    /// Dimensions of the painted crop in samples of the current plane.
    pub(crate) const fn size(self) -> (u32, u32) {
        (self.crop.width, self.crop.height)
    }

    /// Visible coded luma region covered by the painted crop.
    pub(crate) fn region(self) -> Option<SampleRect> {
        let (sx, sy) = self.scale;
        intersect(
            SampleRect {
                x: self.crop.x.checked_mul(sx)?,
                y: self.crop.y.checked_mul(sy)?,
                width: self.crop.width.checked_mul(sx)?,
                height: self.crop.height.checked_mul(sy)?,
            },
            self.visible_luma,
        )
    }

    /// Return the coded luma coordinates of a point in the painted picture.
    pub(crate) fn point(
        self,
        header: &PictureHeader,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Option<(u32, u32)> {
        let (x, y) = position_from_paint(header, x, y, width, height)?;
        let (sx, sy) = self.scale;
        Some((
            self.visible_plane.x.checked_add(x)?.checked_mul(sx)?,
            self.visible_plane.y.checked_add(y)?.checked_mul(sy)?,
        ))
    }

    /// Project geometry that intersects the crop. The stage clips the outline
    /// so a crop edge is never shown as a coding boundary.
    pub(crate) fn project(self, coded: SampleRect) -> Option<ProjectedRect> {
        intersect(coded, self.region()?)?;
        let (sx, sy) = self.scale;
        let x = coded.x / sx;
        let y = coded.y / sy;
        let right = coded.x.checked_add(coded.width)?.div_ceil(sx);
        let bottom = coded.y.checked_add(coded.height)?.div_ceil(sy);
        Some(ProjectedRect {
            x: i64::from(x) - i64::from(self.crop.x),
            y: i64::from(y) - i64::from(self.crop.y),
            width: right.checked_sub(x)?,
            height: bottom.checked_sub(y)?,
        })
    }

    /// Project a signed luma point in eighths into the painted rectangle.
    pub(crate) fn project_eighths(self, x: i64, y: i64) -> (f32, f32) {
        let (sx, sy) = self.scale;
        let project = |value: i64, scale: u32, start: u32, size: u32| {
            // The decoder uses u32 sample coordinates and i32 vectors.
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
            let fraction = ((value as f64 / (8.0 * f64::from(scale)) - f64::from(start))
                / f64::from(size.max(1))) as f32;
            fraction
        };
        (
            project(x, sx, self.crop.x, self.crop.width),
            project(y, sy, self.crop.y, self.crop.height),
        )
    }
}

fn intersect(a: SampleRect, b: SampleRect) -> Option<SampleRect> {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let right = a.x.checked_add(a.width)?.min(b.x.checked_add(b.width)?);
    let bottom = a.y.checked_add(a.height)?.min(b.y.checked_add(b.height)?);
    (right > x && bottom > y).then(|| SampleRect {
        x,
        y,
        width: right - x,
        height: bottom - y,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn rect(x: u32, y: u32, width: u32, height: u32) -> SampleRect {
        SampleRect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn padded_chroma_pan_preserves_odd_luma_edge() {
        let viewport = BlockViewport {
            plane: SamplePlane::U,
            visible_plane: rect(2, 3, 67, 69),
            visible_luma: rect(4, 3, 133, 69),
            crop: rect(63, 63, 6, 9),
            scale: (2, 1),
        };
        assert_eq!(viewport.region(), Some(rect(126, 63, 11, 9)));
        assert_eq!(
            viewport.project(rect(128, 64, 16, 16)),
            Some(ProjectedRect {
                x: 1,
                y: 1,
                width: 8,
                height: 16
            })
        );
        assert_eq!(
            viewport.project(rect(120, 60, 16, 16)),
            Some(ProjectedRect {
                x: -3,
                y: -3,
                width: 8,
                height: 16
            })
        );
        assert_eq!(viewport.project(rect(112, 64, 12, 16)), None);
    }
}
