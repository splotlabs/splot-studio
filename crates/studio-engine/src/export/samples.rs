// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Native sample layout and metadata from the borrowed decoder frame.

use serde::Serialize;
use splot_core::headers::metadata::MetadataPayload;
use splot_decode::{Picture, PictureFrame};
use splot_recon::{BitDepth, DecodedFrame, DecodedFrameHashInput, PixelFormat, ReconSample};
use studio_domain::export::{ExportLayer, ExportSampleFormat};
use studio_domain::job::{ChromaLayout, FailureCode, Rational};
use studio_domain::preview::{PictureChromaPosition, PictureColor, SamplePlane};

use crate::job::Stop;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct Layout {
    pub width: u32,
    pub height: u32,
    storage_bits: u8,
    endianness: &'static str,
    padding: &'static str,
    planes: Vec<PlaneLayout>,
    pub frame_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
struct PlaneLayout {
    name: SamplePlane,
    width: u32,
    height: u32,
    row_bytes: u64,
}

impl Layout {
    pub fn of<T: ReconSample>(frame: &DecodedFrame<T>) -> Result<Self, Stop> {
        let bytes_per_sample = if frame.bit_depth() == BitDepth::Eight {
            1
        } else {
            2
        };
        let y = frame.y().visible_size();
        let planes = [
            (SamplePlane::Y, Some(frame.y())),
            (SamplePlane::U, frame.u()),
            (SamplePlane::V, frame.v()),
        ];
        let mut layout = Vec::with_capacity(3);
        for (name, plane) in planes {
            if let Some(plane) = plane {
                let visible = plane.visible_size();
                let width = dimension(visible.width())?;
                let height = dimension(visible.height())?;
                layout.push(PlaneLayout {
                    name,
                    width,
                    height,
                    row_bytes: u64::from(width) * u64::from(bytes_per_sample),
                });
            }
        }
        let frame_bytes = DecodedFrameHashInput::new(frame)
            .byte_len()
            .map_err(|e| unsupported(format!("Cannot describe the native sample layout. {e}")))?;
        Ok(Self {
            width: dimension(y.width())?,
            height: dimension(y.height())?,
            storage_bits: bytes_per_sample * 8,
            endianness: if bytes_per_sample == 1 {
                "none"
            } else {
                "little"
            },
            padding: "none",
            planes: layout,
            frame_bytes: frame_bytes as u64,
        })
    }
}

fn dimension(value: usize) -> Result<u32, Stop> {
    u32::try_from(value).map_err(|_| unsupported("The decoded picture is too large."))
}

#[derive(Clone, Copy, Serialize)]
pub(super) struct Metadata {
    color: Option<PictureColor>,
    chroma_position: Option<PictureChromaPosition>,
    scan_type: Option<u8>,
    scan_structure: Option<ScanStructure>,
    sample_aspect_ratio: Option<Rational>,
}

#[derive(Clone, Copy, Serialize)]
struct ScanStructure {
    picture_structure: u8,
    source_scan_type: u8,
    duplicate: bool,
}

impl Metadata {
    pub fn of(picture: &Picture) -> Self {
        let content = picture.content_interpretation();
        let color = content.and_then(|c| c.color_description).map(|c| {
            let derived = c.derived();
            PictureColor {
                primaries: derived.code_points.color_primaries,
                transfer: derived.code_points.transfer_characteristics,
                matrix: derived.code_points.matrix_coefficients,
                full_range: derived.full_range,
            }
        });
        let chroma_position =
            content
                .and_then(|c| c.chroma_sample_position)
                .map(|p| PictureChromaPosition {
                    top: p.top,
                    bottom: p.bottom,
                });
        let sample_aspect_ratio = content
            .and_then(|c| c.aspect_ratio)
            .and_then(|ar| ar.derived_sar())
            .map(|(num, den)| Rational { num, den });
        let scan_structure = picture.metadata().iter().find_map(|unit| {
            if let MetadataPayload::ScanType(mps) = &unit.payload {
                Some(ScanStructure {
                    picture_structure: mps.mps_pic_struct_type,
                    source_scan_type: mps.mps_source_scan_type_idc,
                    duplicate: mps.mps_duplicate_flag,
                })
            } else {
                None
            }
        });
        Self {
            color,
            chroma_position,
            scan_structure,
            scan_type: content.map(|c| c.scan_type_idc.get()),
            sample_aspect_ratio,
        }
    }

    pub fn check_y4m(self) -> Result<(), Stop> {
        let reason = if matches!(self.scan_type, Some(2 | 3)) {
            "Y4M cannot preserve signaled interlace."
        } else if self
            .scan_structure
            .is_some_and(|mps| mps.picture_structure != 0)
        {
            "Y4M cannot preserve the signaled picture structure."
        } else if self.color.is_some_and(|c| c.full_range) {
            "Y4M cannot preserve the signaled full sample range."
        } else if self
            .color
            .is_some_and(|c| c.primaries != 2 || c.transfer != 2 || c.matrix != 2)
        {
            "Y4M cannot preserve the signaled color description."
        } else if self.chroma_position.is_some() {
            "Y4M cannot preserve the signaled chroma position."
        } else if self
            .sample_aspect_ratio
            .is_some_and(|ar| ar.num != 0 && ar.den != 0)
        {
            "Y4M cannot preserve the signaled sample aspect ratio."
        } else {
            return Ok(());
        };
        Err(unsupported(format!("{reason} Choose raw samples.")))
    }
}

pub(super) fn check_format(picture: &Picture, expected: ExportSampleFormat) -> Result<(), Stop> {
    let chroma = match picture.pixel_format() {
        PixelFormat::Monochrome => ChromaLayout::Monochrome,
        PixelFormat::Yuv420 => ChromaLayout::Yuv420,
        PixelFormat::Yuv422 => ChromaLayout::Yuv422,
        PixelFormat::Yuv444 => ChromaLayout::Yuv444,
    };
    if expected.bit_depth == picture.bit_depth().bits() && expected.chroma == chroma {
        Ok(())
    } else {
        Err(unsupported(
            "The selected outputs use a different sample depth or chroma layout.",
        ))
    }
}

pub(super) fn layer(picture: &Picture) -> ExportLayer {
    ExportLayer {
        extended: picture.extended_layer_id().get(),
        embedded: picture.embedded_layer_id().get(),
        temporal: picture.temporal_layer_id().get(),
    }
}

pub(super) fn frame(picture: &Picture) -> Result<PictureFrame<'_>, Stop> {
    picture.decoded_frame().ok_or_else(|| {
        Stop::failed(
            FailureCode::InvalidSource,
            "The decoder did not return reconstructed samples.",
        )
    })
}

pub(super) fn unsupported(message: impl Into<String>) -> Stop {
    Stop::failed(FailureCode::UnsupportedSource, message)
}
