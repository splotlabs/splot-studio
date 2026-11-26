// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The preview decoder and decoded pictures as [`Frame`]s.
//!
//! Random access: a decoder can start at a temporal unit that
//! [`splot_core::stream_reader::is_random_access_point`] accepts. The pinned
//! encoder codes such a unit, with a closed-loop key frame, at the first
//! frame, every `keyint` frames after the last one (120 by default; Studio
//! does not change it) and at scene cuts, and nothing after it refers to
//! anything before it. Every temporal unit of this encoder outputs exactly
//! one frame, so a unit's index is its frame's display index.

use splot_decode::{
    BitDepth, DecodeLimitName, DecodeLimitThreshold, DecodeLimits, Decoder, DecoderSettings,
    FrameDelay, Picture, PictureSamples, PlaneId, PlaneRef, ReconSample, ThreadCount,
};

use studio_domain::preview::GrainPolicy;

use crate::picture::{Frame, Plane, Samples};
use crate::source::{FRAMES_IN_FLIGHT, MEMORY_BUDGET_BYTES};

/// Threads a preview decoder works with, so that a preview next to a
/// running encode takes at most this many cores from it.
pub const PREVIEW_DECODE_THREADS: usize = 2;

/// The largest temporal unit used for document decoding.
pub(crate) const MAX_UNIT_BYTES: usize = 64 << 20;

/// The maximum number of temporal units or pictures for document decoding.
pub(crate) const MAX_DOCUMENT_UNITS: usize = 1 << 20;

pub(crate) fn document_decoder(
    threads: usize,
    grain: GrainPolicy,
) -> splot_decode::Result<Decoder> {
    let limit = DecodeLimitThreshold::max(MAX_DOCUMENT_UNITS as u64);
    let limits = DecodeLimits::DEFAULT
        .with_max_frames_to_decode(limit)
        .with_max_output_frames(limit);
    preview_decoder(threads, limits, grain)
}

/// The most bytes of decoded frames a preview decoder may keep alive:
/// references, frames in flight and pictures being shown.
pub const DECODE_STORE_BYTES: u64 = 1 << 30;

/// A streaming decoder for pictures: `threads` workers, one frame in
/// flight, and frame sizes and decoded bytes bounded like a job's.
///
/// # Errors
/// When the decoder's threads cannot start.
pub fn decoder(threads: usize) -> splot_decode::Result<Decoder> {
    preview_decoder(threads, DecodeLimits::DEFAULT, GrainPolicy::Apply)
}

pub(crate) fn preview_decoder(
    threads: usize,
    limits: DecodeLimits,
    grain: GrainPolicy,
) -> splot_decode::Result<Decoder> {
    Decoder::new(preview_decoder_settings(threads, limits, grain))
}

pub(crate) fn preview_decoder_settings(
    threads: usize,
    limits: DecodeLimits,
    grain: GrainPolicy,
) -> DecoderSettings {
    let largest = MEMORY_BUDGET_BYTES / (2 * FRAMES_IN_FLIGHT);
    let limits = limits
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
    DecoderSettings::default()
        .with_threads(threads)
        .with_frame_delay(FrameDelay::Fixed(std::num::NonZeroUsize::MIN))
        .with_limits(limits)
        .with_skip_film_grain(grain == GrainPolicy::Skip)
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
