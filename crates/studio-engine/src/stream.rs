// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Decoding this encoder's AV2 output for pictures: the preview decoder,
//! random access points, and decoded pictures as [`Frame`]s.
//!
//! Random access: the pinned encoder codes a closed-loop key frame at the
//! first frame, every `keyint` frames after the last one (120 by default;
//! Studio does not change it) and at scene cuts. Each one is a temporal
//! unit of its own holding the sequence header and an
//! `OBU_CLOSED_LOOP_KEY` frame, and nothing after it refers to anything
//! before it. A temporal unit holding both OBU types is therefore where a
//! fresh decoder can start; only OBU headers are read to find it. Every
//! temporal unit of this encoder outputs exactly one frame, so a unit's
//! index is its frame's display index.

use splot_core::annexb::AnnexBObuCursor;
use splot_core::span::ByteOffset;
use splot_core::types::ObuType;
use splot_decode::{
    BitDepth, DecodeLimitName, DecodeLimitThreshold, DecodeLimits, Decoder, DecoderSettings,
    FrameDelay, Picture, PictureSamples, PlaneId, PlaneRef, ReconSample, ThreadCount,
};

use crate::picture::{Frame, Plane, Samples};
use crate::source::{FRAMES_IN_FLIGHT, MEMORY_BUDGET_BYTES};

/// Threads a preview decoder works with, so that a preview next to a
/// running encode takes at most this many cores from it.
pub const PREVIEW_DECODE_THREADS: usize = 2;

/// The most bytes of decoded frames a preview decoder may keep alive:
/// references, frames in flight and pictures being shown.
pub const DECODE_STORE_BYTES: u64 = 1 << 30;

/// Whether `unit`, one temporal unit, is a random access point (see the
/// module documentation). A unit that does not parse is not one.
#[must_use]
pub fn is_random_access(unit: &[u8]) -> bool {
    let mut cursor = AnnexBObuCursor::new(unit, ByteOffset::new(0));
    let (mut sequence, mut key) = (false, false);
    while let Ok(Some(obu)) = cursor.next_obu() {
        match obu.header.obu_type {
            ObuType::SequenceHeader => sequence = true,
            ObuType::ClosedLoopKey => key = true,
            _ => {}
        }
    }
    sequence && key
}

/// A streaming decoder for pictures: `threads` workers, one frame in
/// flight, and frame sizes and decoded bytes bounded like a job's.
///
/// # Errors
/// When the decoder's threads cannot start.
pub fn decoder(threads: usize) -> splot_decode::Result<Decoder> {
    let largest = MEMORY_BUDGET_BYTES / (2 * FRAMES_IN_FLIGHT);
    let limits = DecodeLimits::DEFAULT
        .with_limit(
            DecodeLimitName::MaxLumaSamplesPerFrame,
            DecodeLimitThreshold::max(largest),
        )
        .with_limit(
            DecodeLimitName::MaxReferenceStoreBytes,
            DecodeLimitThreshold::max(DECODE_STORE_BYTES),
        );
    let threads =
        std::num::NonZeroUsize::new(threads).map_or(ThreadCount::Auto, ThreadCount::Fixed);
    let settings = DecoderSettings::default()
        .with_threads(threads)
        .with_frame_delay(FrameDelay::Fixed(std::num::NonZeroUsize::MIN))
        .with_limits(limits);
    Decoder::new(settings)
}

/// The planes of a decoded picture, without a copy.
#[must_use]
pub fn frame(picture: &Picture) -> Option<Frame<'_>> {
    let plane = |id: PlaneId| match picture.plane(id)? {
        PictureSamples::Eight(plane) => view(plane, Samples::Narrow(plane.samples())),
        PictureSamples::Ten(plane) => view(plane, Samples::Wide(plane.samples())),
    };
    let chroma = match (plane(PlaneId::U), plane(PlaneId::V)) {
        (Some(u), Some(v)) => Some([u, v]),
        _ => None,
    };
    Some(Frame {
        luma: plane(PlaneId::Y)?,
        chroma,
        bit_depth: match picture.bit_depth() {
            BitDepth::Eight => 8,
            BitDepth::Ten => 10,
        },
    })
}

fn view<'a, T: ReconSample>(plane: PlaneRef<'a, T>, samples: Samples<'a>) -> Option<Plane<'a>> {
    let rect = plane.visible_rect();
    let stride = plane.stride_samples();
    let offset = rect.y().checked_mul(stride)?.checked_add(rect.x())?;
    Plane::new(samples, offset, stride, rect.width(), rect.height())
}
