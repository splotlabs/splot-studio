// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Document output metadata and the one exact picture retained for sample queries.

use std::rc::Rc;

use splot_core::headers::ContentInterpretation;
use splot_decode::{BitDepth, Picture, PictureFrame, PictureSamples, PixelFormat, PlaneId};
use studio_domain::job::ChromaLayout;
use studio_domain::preview::{
    ColorMatrix, DocumentPictureInfo, GrainPolicy, MediaTime, OutputEvent, PictureChromaPosition,
    PictureColor, PictureFailure, PictureTarget, SamplePoint, SampleRect,
};

use super::{Decoded, Outcome, Stopped};
use crate::stream;

/// A decoded output and its links to source evidence.
pub(super) struct DocumentPicture {
    pub picture: Picture,
    pub output: OutputEvent,
    pub time: Option<MediaTime>,
}

impl DocumentPicture {
    pub fn matrix(&self) -> Outcome<Option<ColorMatrix>> {
        let Some(color) = self
            .picture
            .content_interpretation()
            .map(ContentInterpretation::derived_color)
        else {
            return Ok(None);
        };
        let unsupported = |message| Stopped::unavailable(PictureFailure::Unsupported, message);
        if color.full_range || matches!(color.code_points.transfer_characteristics, 16 | 18) {
            return Err(unsupported(
                "RGB preview supports limited-range SDR only. Select a native plane to inspect samples.",
            ));
        }
        if self.picture.pixel_format() == PixelFormat::Monochrome {
            return Ok(None);
        }
        match color.code_points.matrix_coefficients {
            1 => Ok(Some(ColorMatrix::Bt709)),
            2 => Ok(None),
            5 | 6 => Ok(Some(ColorMatrix::Bt601)),
            9 => Ok(Some(ColorMatrix::Bt2020)),
            _ => Err(unsupported(
                "RGB preview does not support this matrix. Select a native plane to inspect samples.",
            )),
        }
    }

    pub fn info(&self) -> Option<DocumentPictureInfo> {
        let coded = match self.picture.decoded_frame()? {
            PictureFrame::Eight(frame) => frame.coded_luma_size(),
            PictureFrame::Ten(frame) => frame.coded_luma_size(),
        };
        let bit_depth = match self.picture.bit_depth() {
            BitDepth::Eight => 8,
            BitDepth::Ten => 10,
        };
        let chroma = match self.picture.pixel_format() {
            PixelFormat::Monochrome => ChromaLayout::Monochrome,
            PixelFormat::Yuv420 => ChromaLayout::Yuv420,
            PixelFormat::Yuv422 => ChromaLayout::Yuv422,
            PixelFormat::Yuv444 => ChromaLayout::Yuv444,
        };
        let content = self.picture.content_interpretation();
        let color = content
            .and_then(|content| content.color_description)
            .map(|color| {
                let color = color.derived();
                PictureColor {
                    primaries: color.code_points.color_primaries,
                    transfer: color.code_points.transfer_characteristics,
                    matrix: color.code_points.matrix_coefficients,
                    full_range: color.full_range,
                }
            });
        let chroma_position = content
            .and_then(|content| content.chroma_sample_position)
            .map(|p| PictureChromaPosition {
                top: p.top,
                bottom: p.bottom,
            });
        let y = self.rect(PlaneId::Y)?;
        let (u, v) = if chroma == ChromaLayout::Monochrome {
            (None, None)
        } else {
            (Some(self.rect(PlaneId::U)?), Some(self.rect(PlaneId::V)?))
        };
        Some(DocumentPictureInfo {
            bit_depth,
            chroma,
            coded_width: u32::try_from(coded.width()).ok()?,
            coded_height: u32::try_from(coded.height()).ok()?,
            y,
            u,
            v,
            color,
            chroma_position,
            film_grain: self.picture.has_film_grain(),
        })
    }

    fn rect(&self, plane: PlaneId) -> Option<SampleRect> {
        let rect = match self.picture.plane(plane)? {
            PictureSamples::Eight(plane) => plane.visible_rect(),
            PictureSamples::Ten(plane) => plane.visible_rect(),
        };
        Some(SampleRect {
            x: u32::try_from(rect.x()).ok()?,
            y: u32::try_from(rect.y()).ok()?,
            width: u32::try_from(rect.width()).ok()?,
            height: u32::try_from(rect.height()).ok()?,
        })
    }

    pub fn sample(&self, point: SamplePoint) -> Outcome<u16> {
        let frame = stream::frame(&self.picture).ok_or_else(|| {
            Stopped::unavailable(
                PictureFailure::Unreadable,
                "The decoder returned invalid planes.",
            )
        })?;
        let plane = frame.plane(point.plane).ok_or_else(|| {
            Stopped::unavailable(
                PictureFailure::Unsupported,
                "The picture has no selected plane.",
            )
        })?;
        plane.sample(point.x, point.y).ok_or_else(|| {
            Stopped::unavailable(
                PictureFailure::OutOfRange,
                "The sample coordinate is outside the visible plane.",
            )
        })
    }
}

struct Retained {
    target: PictureTarget,
    grain: GrainPolicy,
    picture: Rc<DocumentPicture>,
}

/// The latest exact picture across all open previews.
#[derive(Default)]
pub(super) struct Recent(Option<Retained>);

impl Recent {
    pub fn prepare(&mut self, target: &PictureTarget, grain: GrainPolicy) {
        if self
            .0
            .as_ref()
            .is_some_and(|retained| retained.target != *target || retained.grain != grain)
        {
            self.0 = None;
        }
    }

    pub fn forget_matching(&mut self, matches: impl Fn(&PictureTarget) -> bool) {
        if self
            .0
            .as_ref()
            .is_some_and(|retained| matches(&retained.target))
        {
            self.0 = None;
        }
    }

    pub fn get(&self, target: &PictureTarget, ordinal: u64, grain: GrainPolicy) -> Option<Decoded> {
        let retained = self.0.as_ref()?;
        (retained.target == *target
            && retained.grain == grain
            && retained.picture.output.ordinal == ordinal)
            .then(|| Decoded::Document(Rc::clone(&retained.picture)))
    }

    pub fn keep(&mut self, target: &PictureTarget, grain: GrainPolicy, decoded: &Decoded) {
        let Decoded::Document(picture) = decoded else {
            return;
        };
        if let Some(retained) = self.0.as_mut()
            && retained.target == *target
            && retained.grain == grain
        {
            retained.picture = Rc::clone(picture);
            return;
        }
        self.0 = Some(Retained {
            target: target.clone(),
            grain,
            picture: Rc::clone(picture),
        });
    }
}
