// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Document view transforms and exact sample queries.

use studio_domain::job::ChromaLayout;
use studio_domain::preview::{
    DocumentPictureInfo, GrainPolicy, PictureBox, PictureFailure, PictureHeader, PictureOptions,
    PictureSurface, PictureView, SamplePlane, SamplePoint, SampleRect,
};
use studio_domain::protocol::Command;

use super::{Effect, Preview, Sent, StageView};

pub(crate) mod blocks;

/// The magnifications in the Analyze viewer. Zero represents Fit.
pub const ZOOMS: [u8; 6] = [0, 1, 2, 4, 8, 16];

/// The view of a picture in the current source and decode policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HistoryView {
    output: Option<u64>,
    options: PictureOptions,
    zoom: u8,
    origin: Option<(u32, u32)>,
    info: Option<DocumentPictureInfo>,
}

impl HistoryView {
    pub(crate) const fn output(self) -> Option<u64> {
        self.output
    }
}

#[derive(Debug, Default)]
pub(super) struct Viewer {
    pub(super) options: PictureOptions,
    zoom: u8,
    origin: Option<(u32, u32)>,
    pan_remainder: (i64, i64),
    info: Option<DocumentPictureInfo>,
    sample: Option<Sample>,
    sample_error: String,
}

#[derive(Debug)]
struct Sample {
    generation: u64,
    ordinal: u64,
    grain: GrainPolicy,
    point: SamplePoint,
    value: Option<u16>,
}

/// The precise viewer controls and status.
#[derive(Debug, Clone, Default)]
pub struct ViewerView {
    /// The selected surface in the surface menu.
    pub surface: i32,
    /// Whether the menu can offer U and V.
    pub chroma: bool,
    /// The selected entry in the zoom menu.
    pub zoom: i32,
    /// The actual picture's magnification in physical pixels. Zero is Fit.
    pub magnification: i32,
    /// Film grain is enabled for this view.
    pub grain: bool,
    /// The selected surface and stream geometry.
    pub geometry: String,
    /// Color metadata and display policy.
    pub policy: String,
    /// The plane the coordinates refer to.
    pub sample_plane: &'static str,
    /// The picture can accept a sample query.
    pub can_sample: bool,
    /// The exact value or the state of the query.
    pub sample_text: String,
    /// The selected coordinate in the visible plane.
    pub sample_point: Option<SamplePoint>,
    /// Text that copies the exact value.
    pub sample_copy: String,
}

impl Viewer {
    fn rect(&self) -> Option<SampleRect> {
        plane_rect(self.info?, plane(self.options.surface))
    }

    pub(super) fn view(&self, size: PictureBox) -> PictureView {
        let max = self.max_origin(size);
        let (x, y) = self.origin.map_or((max.0 / 2, max.1 / 2), |(x, y)| {
            (x.min(max.0), y.min(max.1))
        });
        match self.zoom {
            0 => PictureView::Fit,
            1 => PictureView::Native { x, y },
            factor => PictureView::Zoom { x, y, factor },
        }
    }

    fn max_origin(&self, size: PictureBox) -> (u32, u32) {
        let visible = |pixels: u16| (u32::from(pixels) / u32::from(self.zoom.max(1))).max(1);
        self.rect().map_or((0, 0), |rect| {
            (
                rect.width.saturating_sub(visible(size.width)),
                rect.height.saturating_sub(visible(size.height)),
            )
        })
    }

    pub(super) fn accept_picture(&mut self, header: &PictureHeader) {
        self.clear_sample();
        self.info = header.document_info.as_deref().copied();
        self.origin = match header.view {
            PictureView::Fit => None,
            PictureView::Native { x, y } | PictureView::Zoom { x, y, .. } => Some((x, y)),
        };
    }

    pub(super) fn clear_sample(&mut self) {
        self.sample = None;
        self.sample_error.clear();
    }

    pub(super) fn pending_sample(&self) -> Option<u64> {
        let sample = self.sample.as_ref()?;
        sample.value.is_none().then_some(sample.generation)
    }
}

impl Preview {
    pub(crate) fn history_view(&self, document: u64) -> Option<HistoryView> {
        (self.document_id() == Some(document)).then_some(HistoryView {
            output: self.position,
            options: self.viewer.options,
            zoom: self.viewer.zoom,
            origin: self.viewer.origin,
            info: self.viewer.info,
        })
    }

    pub(crate) fn restore_history_view(&mut self, document: u64, view: HistoryView) {
        if self.document_id() == Some(document) && self.history_view(document) != Some(view) {
            self.viewer.options = view.options;
            self.viewer.zoom = view.zoom;
            self.viewer.origin = view.origin;
            self.viewer.info = view.info;
            self.refresh_document_view();
        }
    }

    pub(super) fn choose_surface(&mut self, surface: PictureSurface) {
        if self.document_id().is_some() && self.viewer.options.surface != surface {
            self.viewer.options.surface = surface;
            self.viewer.origin = None;
            self.refresh_document_view();
        }
    }

    pub(super) fn choose_grain(&mut self, grain: GrainPolicy) {
        if self.document_id().is_some() && self.viewer.options.grain != grain {
            self.viewer.options.grain = grain;
            self.reset_layer_decode_feedback();
            self.refresh_document_view();
        }
    }

    pub(super) fn choose_zoom(&mut self, factor: u8) {
        if self.document_id().is_some() && ZOOMS.contains(&factor) && self.viewer.zoom != factor {
            self.viewer.zoom = factor;
            self.viewer.origin = None;
            self.refresh_document_view();
        }
    }

    fn refresh_document_view(&mut self) {
        self.viewer.clear_sample();
        self.viewer.pan_remainder = (0, 0);
        self.before_next = None;
        self.failed = None;
        self.stage_failure = None;
        self.shown = None;
        self.dirty.picture = true;
        self.dirty.view = true;
    }

    pub(super) fn pan_document(&mut self, dx: i64, dy: i64) {
        let (x, y, factor) = match self.viewer.view(self.stage_box) {
            PictureView::Fit => return,
            PictureView::Native { x, y } => (x, y, 1),
            PictureView::Zoom { x, y, factor } => (x, y, factor),
        };
        let max = self.viewer.max_origin(self.stage_box);
        let shift = |origin: u32, delta: i64, remainder: &mut i64, max: u32| {
            let factor = i64::from(factor);
            let pixels = remainder.saturating_add(delta);
            let coordinate = i64::from(origin)
                .saturating_add(pixels / factor)
                .clamp(0, i64::from(max));
            *remainder = if (coordinate == 0 && pixels < 0)
                || (coordinate == i64::from(max) && pixels > 0)
            {
                0
            } else {
                pixels % factor
            };
            u32::try_from(coordinate).unwrap_or(0)
        };
        let (rx, ry) = &mut self.viewer.pan_remainder;
        let position = (shift(x, dx, rx, max.0), shift(y, dy, ry, max.1));
        if position != (x, y) {
            self.viewer.origin = Some(position);
            self.viewer.clear_sample();
            self.before_next = None;
            self.failed = None;
            self.dirty.view = true;
        }
    }

    fn can_sample(&self) -> bool {
        self.document_id().is_some()
            && self.visible
            && self.broken.is_none()
            && !self.playing
            && matches!(
                self.inflight,
                None | Some(Sent::Play { stopping: true, .. })
            )
            && self
                .shown
                .as_ref()
                .is_some_and(|shown| shown.picture.header.document_info.is_some())
    }

    pub(super) fn pick_sample(&mut self, x: i32, y: i32, width: i32, height: i32) -> Vec<Effect> {
        let coordinate = self
            .shown
            .as_ref()
            .and_then(|shown| position_from_paint(&shown.picture.header, x, y, width, height));
        coordinate.map_or_else(Vec::new, |(x, y)| self.read_sample(Some(x), Some(y)))
    }

    pub(super) fn read_sample(&mut self, x: Option<u32>, y: Option<u32>) -> Vec<Effect> {
        if !self.can_sample() {
            return Vec::new();
        }
        let Some(rect) = self.viewer.rect() else {
            return Vec::new();
        };
        let (Some(x), Some(y)) = (
            x.filter(|x| *x < rect.width),
            y.filter(|y| *y < rect.height),
        ) else {
            let effects = self.cancel_sample();
            self.viewer.clear_sample();
            self.viewer.sample_error = format!(
                "Enter X from 0 to {} and Y from 0 to {}.",
                rect.width.saturating_sub(1),
                rect.height.saturating_sub(1)
            );
            self.dirty.view = true;
            return effects;
        };
        let Some(shown) = &self.shown else {
            return Vec::new();
        };
        let Some(target) = shown.target.clone() else {
            return Vec::new();
        };
        let ordinal = shown.picture.header.frame;
        let grain = shown.picture.header.options.grain;
        let point = SamplePoint {
            plane: plane(shown.picture.header.options.surface),
            x,
            y,
        };
        let generation = self.take_generation();
        self.viewer.sample_error.clear();
        self.document_progress.clear();
        self.viewer.sample = Some(Sample {
            generation,
            ordinal,
            grain,
            point,
            value: None,
        });
        self.dirty.view = true;
        vec![Effect {
            request_id: generation,
            command: Command::PictureSample {
                generation,
                target,
                ordinal,
                grain,
                point,
            },
        }]
    }

    pub(super) fn cancel_sample(&mut self) -> Vec<Effect> {
        let Some(request_id) = self.viewer.pending_sample() else {
            return Vec::new();
        };
        self.viewer.clear_sample();
        self.document_progress.clear();
        self.dirty.view = true;
        vec![Effect {
            request_id,
            command: Command::StopPlayback,
        }]
    }

    /// Accepts an exact sample only while its request is current.
    pub fn sample_answer(
        &mut self,
        generation: u64,
        ordinal: u64,
        grain: GrainPolicy,
        point: SamplePoint,
        value: u16,
    ) {
        let Some(sample) = self.viewer.sample.as_mut() else {
            return;
        };
        if sample.generation != generation
            || sample.ordinal != ordinal
            || sample.grain != grain
            || sample.point != point
            || sample.value.is_some()
        {
            return;
        }
        let valid_value = self
            .viewer
            .info
            .is_some_and(|info| (1_u32 << info.bit_depth.min(16)) > u32::from(value));
        if valid_value {
            sample.value = Some(value);
        } else {
            self.viewer.clear_sample();
            self.viewer.sample_error = "The sample exceeds the decoded bit depth.".into();
        }
        self.document_progress.clear();
        self.dirty.view = true;
    }

    pub(super) fn sample_unavailable(
        &mut self,
        generation: u64,
        code: PictureFailure,
        message: &str,
    ) -> bool {
        if self.viewer.pending_sample() != Some(generation) {
            return false;
        }
        self.viewer.clear_sample();
        self.document_progress.clear();
        if code == PictureFailure::SourceChanged {
            self.fail_document(super::text::DOCUMENT_CHANGED.into());
        } else {
            self.viewer.sample_error = format!("Cannot read the sample. {message}");
            self.dirty.view = true;
        }
        true
    }

    pub(super) fn viewer_view(&self, view: &mut StageView) {
        let surface = self.viewer.options.surface;
        let sample_plane = plane(surface);
        let mut result = ViewerView {
            surface: match surface {
                PictureSurface::Rgb => 0,
                PictureSurface::Y => 1,
                PictureSurface::U => 2,
                PictureSurface::V => 3,
            },
            chroma: self
                .viewer
                .info
                .is_none_or(|info| info.u.is_some() && info.v.is_some()),
            zoom: ZOOMS
                .iter()
                .position(|&factor| factor == self.viewer.zoom)
                .and_then(|index| i32::try_from(index).ok())
                .unwrap_or(0),
            magnification: view
                .picture
                .as_ref()
                .map_or(0, |picture| match picture.header.view {
                    PictureView::Fit => 0,
                    PictureView::Native { .. } => 1,
                    PictureView::Zoom { factor, .. } => i32::from(factor),
                }),
            grain: self.viewer.options.grain == GrainPolicy::Apply,
            sample_plane: plane_label(sample_plane),
            can_sample: self.can_sample(),
            ..ViewerView::default()
        };
        result.sample_text.clone_from(&self.viewer.sample_error);
        result.sample_point = self.viewer.sample.as_ref().map(|sample| sample.point);
        if let Some(sample) = &self.viewer.sample
            && let Some(value) = sample.value
        {
            result.sample_text = format!(
                "{} ({}, {}) = {value}",
                plane_label(sample.point.plane),
                sample.point.x,
                sample.point.y
            );
            result.sample_copy = value.to_string();
        }
        super::text::viewer_policy(&mut result, view.picture.as_ref().map(|p| &p.header));
        view.viewer = result;
    }
}

const fn plane(surface: PictureSurface) -> SamplePlane {
    match surface {
        PictureSurface::Rgb | PictureSurface::Y => SamplePlane::Y,
        PictureSurface::U => SamplePlane::U,
        PictureSurface::V => SamplePlane::V,
    }
}

const fn plane_rect(info: DocumentPictureInfo, plane: SamplePlane) -> Option<SampleRect> {
    match plane {
        SamplePlane::Y => Some(info.y),
        SamplePlane::U => info.u,
        SamplePlane::V => info.v,
    }
}

const fn plane_label(plane: SamplePlane) -> &'static str {
    match plane {
        SamplePlane::Y => "Y luma",
        SamplePlane::U => "U chroma",
        SamplePlane::V => "V chroma",
    }
}

fn valid_info(info: &DocumentPictureInfo) -> bool {
    let y = info.y;
    if !matches!(info.bit_depth, 8 | 10)
        || y.width == 0
        || y.height == 0
        || y.x
            .checked_add(y.width)
            .is_none_or(|end| end > info.coded_width)
        || y.y
            .checked_add(y.height)
            .is_none_or(|end| end > info.coded_height)
    {
        return false;
    }
    if info.chroma == ChromaLayout::Monochrome {
        return info.u.is_none() && info.v.is_none();
    }
    let (sx, sy) = subsampling(info.chroma);
    let chroma = SampleRect {
        x: y.x / sx,
        y: y.y / sy,
        width: y.width.div_ceil(sx),
        height: y.height.div_ceil(sy),
    };
    y.x.is_multiple_of(sx)
        && y.y.is_multiple_of(sy)
        && info.u == Some(chroma)
        && info.v == Some(chroma)
}

const fn subsampling(chroma: ChromaLayout) -> (u32, u32) {
    match chroma {
        ChromaLayout::Yuv420 => (2, 2),
        ChromaLayout::Yuv422 => (2, 1),
        ChromaLayout::Monochrome | ChromaLayout::Yuv444 => (1, 1),
    }
}

pub(super) fn valid_geometry(
    header: &PictureHeader,
    requested: PictureView,
    box_size: PictureBox,
) -> bool {
    let Some(info) = header
        .document_info
        .as_deref()
        .filter(|info| valid_info(info))
    else {
        return false;
    };
    let Some(rect) = plane_rect(*info, plane(header.options.surface)) else {
        return false;
    };
    let width = u32::from(header.width);
    let height = u32::from(header.height);
    let (x, y, factor) = match (requested, header.view) {
        (PictureView::Fit, PictureView::Fit) => (0, 0, 1),
        (PictureView::Native { .. }, PictureView::Native { x, y }) => (x, y, 1),
        (PictureView::Zoom { factor: wanted, .. }, PictureView::Zoom { x, y, factor })
            if wanted == factor =>
        {
            (x, y, u32::from(factor))
        }
        _ => return false,
    };
    (header.frame_width, header.frame_height) == (rect.width, rect.height)
        && width * factor <= u32::from(box_size.width)
        && height * factor <= u32::from(box_size.height)
        && x.checked_add(width).is_some_and(|end| end <= rect.width)
        && y.checked_add(height).is_some_and(|end| end <= rect.height)
}

// The pointer is relative to the exact physical rectangle Slint paints.
fn position_from_paint(
    header: &PictureHeader,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> Option<(u32, u32)> {
    let x = u32::try_from(x).ok()?;
    let y = u32::try_from(y).ok()?;
    let width = u32::try_from(width).ok()?;
    let height = u32::try_from(height).ok()?;
    if x >= width || y >= height {
        return None;
    }
    let scale = |at: u32, from: u32, to: u32| {
        u32::try_from(u64::from(at) * u64::from(to) / u64::from(from)).ok()
    };
    let position = match header.view {
        PictureView::Fit => (
            scale(x, width, header.frame_width)?,
            scale(y, height, header.frame_height)?,
        ),
        PictureView::Native { x: left, y: top } => (left.checked_add(x)?, top.checked_add(y)?),
        PictureView::Zoom {
            x: left,
            y: top,
            factor,
        } => {
            let factor = u32::from(factor.max(1));
            (left.checked_add(x / factor)?, top.checked_add(y / factor)?)
        }
    };
    (position.0 < header.frame_width && position.1 < header.frame_height).then_some(position)
}
