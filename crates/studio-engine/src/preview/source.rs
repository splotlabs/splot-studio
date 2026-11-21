// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Source pictures: frames exactly as the encoder receives them, through
//! the job's own [`Source`] (same reader, same `ffmpeg` command line, same
//! repack and acceptance rules).
//!
//! A Y4M file of plain frame records is opened at the frame's offset. The
//! media route has no index: `ffmpeg` starts at the frame with a `select`
//! on the decoded frame number, so it decodes (but does not send) every
//! frame before it, and a running decode is read ahead when the next
//! request is at most [`READ_AHEAD_FRAMES`] later. Its cost therefore grows
//! with the frame index; going back restarts the decode.

use studio_domain::preview::{ColorMatrix, MediaTime, PictureFailure, SourceRef};

use super::{Context, Decoded, Outcome, Stopped};
use crate::source::{self, Selection, Source};

/// A running media decode is read through, rather than restarted, for a
/// request at most this many frames ahead.
pub const READ_AHEAD_FRAMES: u64 = 48;

/// An open source.
pub struct SourcePictures {
    source: Source,
    sizes: [(usize, usize); 3],
    /// The frame count, once known: Y4M's, or found at the end of a decode.
    count: Option<u64>,
    /// The frame the last successful read returned.
    last_read: Option<u64>,
}

impl SourcePictures {
    /// Opens the source, refusing a changed file and anything a job would
    /// refuse.
    pub fn open(reference: &SourceRef) -> Outcome<Self> {
        let path = reference
            .path
            .to_path_buf()
            .map_err(|e| Stopped::unavailable(PictureFailure::Unreadable, e.to_string()))?;
        let source = source::open(
            &path,
            Selection {
                fingerprint: Some(reference.fingerprint),
                video_stream: Some(reference.video_stream),
            },
        )
        .map_err(Stopped::from_stop)?;
        let sizes = source.header.plane_sizes();
        Ok(Self {
            count: source.frame_count(),
            source,
            sizes,
            last_read: None,
        })
    }

    /// Releases the decode; the file stays open.
    pub fn rest(&mut self) {
        self.source.rest();
        self.last_read = None;
    }

    pub fn check(&self) -> Outcome<()> {
        self.source.unchanged().map_err(Stopped::from_stop)
    }

    pub const fn frame_count(&self) -> Option<u64> {
        self.count
    }

    pub const fn matrix(&self) -> Option<ColorMatrix> {
        self.source.matrix
    }

    pub fn time(&self, index: u64) -> Option<MediaTime> {
        let (num, den) = self.source.header.frame_rate;
        (num > 0).then(|| MediaTime {
            num: index.saturating_mul(u64::from(den)),
            den: u64::from(num),
        })
    }

    pub fn second(&self) -> u64 {
        let (num, den) = self.source.header.frame_rate;
        u64::from(num + den / 2) / u64::from(den.max(1))
    }

    pub fn seek(&mut self, context: &Context<'_>, index: u64) -> Outcome<()> {
        if self.count.is_some_and(|count| index >= count) {
            return Err(super::out_of_range(index, self.count));
        }
        if self.source.position() == index && self.last_read.is_some_and(|last| last + 1 == index) {
            return Ok(());
        }
        let killer = self.source.killer();
        let positioned =
            context.guarded(killer, || self.source.position_at(index, READ_AHEAD_FRAMES));
        context.check()?;
        positioned.map_err(Stopped::from_stop)
    }

    pub fn next(&mut self, context: &Context<'_>) -> Outcome<Option<(u64, Decoded)>> {
        let killer = self.source.killer();
        let read = context.guarded(killer, || self.source.read_frame());
        context.check()?;
        let index = self.source.position();
        let Some(planes) = read.map_err(Stopped::from_stop)? else {
            // Frames 0..index exist when the last one read was index - 1.
            if self.last_read.is_some_and(|last| last + 1 == index) || index == 0 {
                self.count = Some(index);
            }
            return Ok(None);
        };
        let index = index.saturating_sub(1);
        self.last_read = Some(index);
        Ok(Some((
            index,
            Decoded::Planes {
                planes,
                sizes: self.sizes,
                bit_depth: self.source.header.bit_depth,
            },
        )))
    }
}
