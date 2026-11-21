// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Preview pictures: what the app may ask to see and what the engine sends.
//!
//! A picture is a small presentation copy, never the samples that are encoded:
//! the engine converts a frame to 8-bit RGB with one display transform shared
//! by source, result and live pictures, and sends at most
//! [`MAX_PICTURE_BYTES`] of it. The header travels as JSON; the pixels follow
//! as raw bytes (see [`crate::protocol::write_picture`]), never inside JSON.
//!
//! This is an SDR approximation for comparing a source with its result. It is
//! not colour-managed and makes no claim of reference-monitor accuracy.

use serde::{Deserialize, Serialize};

use crate::job::{SourceFingerprint, WirePath};

/// The widest picture the engine sends, in pixels.
pub const MAX_PICTURE_WIDTH: u16 = 1280;
/// The tallest picture the engine sends, in pixels.
pub const MAX_PICTURE_HEIGHT: u16 = 720;
/// Bytes per pixel of a picture payload: 8-bit R, G, B.
pub const PICTURE_BYTES_PER_PIXEL: u32 = 3;
/// The largest picture payload.
pub const MAX_PICTURE_BYTES: u32 =
    MAX_PICTURE_WIDTH as u32 * MAX_PICTURE_HEIGHT as u32 * PICTURE_BYTES_PER_PIXEL;

/// The largest picture the app can show, in physical pixels. The engine never
/// sends a picture larger than this or than the frame itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PictureBox {
    /// Width, `1..=`[`MAX_PICTURE_WIDTH`].
    pub width: u16,
    /// Height, `1..=`[`MAX_PICTURE_HEIGHT`].
    pub height: u16,
}

impl PictureBox {
    /// A box clamped into the supported range.
    #[must_use]
    pub fn clamped(width: u32, height: u32) -> Self {
        let clamp =
            |value: u32, max: u16| u16::try_from(value.clamp(1, u32::from(max))).unwrap_or(max);
        Self {
            width: clamp(width, MAX_PICTURE_WIDTH),
            height: clamp(height, MAX_PICTURE_HEIGHT),
        }
    }

    /// Whether both sides are inside the supported range.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.width >= 1
            && self.width <= MAX_PICTURE_WIDTH
            && self.height >= 1
            && self.height <= MAX_PICTURE_HEIGHT
    }
}

/// Which part of the frame a picture shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PictureView {
    /// The whole frame, scaled down to fit the box (never up), aspect kept.
    Fit,
    /// Frame pixels one to one: the box-sized region whose top-left corner is
    /// at `x`, `y` in luma pixels. The engine moves an origin that would leave
    /// the frame back inside it and reports the origin it used.
    Native {
        /// Left edge in luma pixels.
        x: u32,
        /// Top edge in luma pixels.
        y: u32,
    },
}

/// The source a picture is read from: the same identity a job carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRef {
    /// The file. It is never modified.
    pub path: WirePath,
    /// The file the user looked at; a file that no longer matches is refused.
    pub fingerprint: SourceFingerprint,
    /// The video stream.
    pub video_stream: u32,
}

/// What to show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PictureTarget {
    /// A frame of the source exactly as the encoder would receive it.
    Source {
        /// The source.
        source: SourceRef,
    },
    /// A frame of a finished output file, decoded by the Splot decoder.
    Result {
        /// The output file.
        path: WirePath,
        /// The source it was made from. The output carries no colour
        /// description and, as `.obu`, no timing; both are taken from here so
        /// that the two sides are shown alike. `None` means neither is known.
        source: Option<SourceRef>,
    },
}

/// Which frame to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PictureFrame {
    /// The frame with this zero-based index in display order.
    Index {
        /// The index.
        index: u64,
    },
    /// A representative frame for a thumbnail, chosen by the engine. The
    /// answer says which frame it is.
    Poster,
}

/// Where a picture came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PictureOrigin {
    /// The source, as the encoder receives it.
    Source,
    /// A finished output file, through the Splot decoder.
    Result,
    /// The output of the running job so far, through the Splot decoder.
    Live,
}

/// The YCbCr matrix a picture was converted with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorMatrix {
    /// ITU-R BT.601.
    Bt601,
    /// ITU-R BT.709.
    Bt709,
    /// ITU-R BT.2020 non-constant luminance.
    Bt2020,
    /// Luma only; no matrix applies.
    Monochrome,
}

/// How a picture's colours were produced. Limited range is always assumed:
/// full-range sources are refused before any picture exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ColorBasis {
    /// The matrix used.
    pub matrix: ColorMatrix,
    /// The source did not state its matrix, so this one is an assumption the
    /// app must show as such.
    pub assumed: bool,
}

/// An exact time in seconds, as a ratio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MediaTime {
    /// The numerator.
    pub num: u64,
    /// The denominator; never zero.
    pub den: u64,
}

/// Describes one picture. On the wire its pixels follow immediately:
/// `width * height * 3` bytes, rows top to bottom, R, G, B, no padding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PictureHeader {
    /// The generation of the request this answers; `0` for live pictures.
    /// The app drops any picture whose generation is not its newest.
    pub generation: u64,
    /// Where it came from.
    pub origin: PictureOrigin,
    /// The zero-based display index of the frame.
    pub frame: u64,
    /// The frame's presentation time; `None` when the stream has no timing.
    pub time: Option<MediaTime>,
    /// How many frames the stream has, when that is known.
    pub frame_count: Option<u64>,
    /// Picture width in pixels.
    pub width: u16,
    /// Picture height in pixels.
    pub height: u16,
    /// The full frame's width in luma pixels.
    pub frame_width: u32,
    /// The full frame's height in luma pixels.
    pub frame_height: u32,
    /// The view that was produced, with the origin actually used.
    pub view: PictureView,
    /// How the colours were produced.
    pub basis: ColorBasis,
}

impl PictureHeader {
    /// The payload length this header promises, or `None` when its
    /// dimensions are outside the supported range. Checked before anything is
    /// allocated or read.
    #[must_use]
    pub fn payload_len(&self) -> Option<u32> {
        let size = PictureBox {
            width: self.width,
            height: self.height,
        };
        size.is_valid()
            .then(|| u32::from(self.width) * u32::from(self.height) * PICTURE_BYTES_PER_PIXEL)
    }
}

/// Why a picture could not be produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PictureFailure {
    /// The file is missing, unreadable or damaged.
    Unreadable,
    /// The source no longer matches its fingerprint.
    SourceChanged,
    /// The source is not one this build can encode, so it is not shown.
    Unsupported,
    /// There is no such frame.
    OutOfRange,
    /// Playback was asked for a stream whose frame timing is not known.
    NoTiming,
    /// Reading this file needs `FFmpeg` and none is available.
    MediaToolMissing,
    /// The request itself is invalid, for example an oversized box.
    InvalidRequest,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(width: u16, height: u16) -> PictureHeader {
        PictureHeader {
            generation: 1,
            origin: PictureOrigin::Source,
            frame: 0,
            time: None,
            frame_count: None,
            width,
            height,
            frame_width: 1920,
            frame_height: 1080,
            view: PictureView::Fit,
            basis: ColorBasis {
                matrix: ColorMatrix::Bt709,
                assumed: true,
            },
        }
    }

    #[test]
    fn payload_length_is_bounded() {
        assert_eq!(header(4, 2).payload_len(), Some(24));
        assert_eq!(
            header(MAX_PICTURE_WIDTH, MAX_PICTURE_HEIGHT).payload_len(),
            Some(MAX_PICTURE_BYTES)
        );
        assert_eq!(header(0, 2).payload_len(), None);
        assert_eq!(header(MAX_PICTURE_WIDTH + 1, 2).payload_len(), None);
        assert_eq!(header(2, MAX_PICTURE_HEIGHT + 1).payload_len(), None);
    }

    #[test]
    fn boxes_are_clamped() {
        let small = PictureBox::clamped(0, 0);
        assert_eq!((small.width, small.height), (1, 1));
        let big = PictureBox::clamped(u32::MAX, u32::MAX);
        assert_eq!(
            (big.width, big.height),
            (MAX_PICTURE_WIDTH, MAX_PICTURE_HEIGHT)
        );
        assert!(small.is_valid() && big.is_valid());
    }
}
