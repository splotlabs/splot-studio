// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The decoding `ffmpeg` child: the selected stream as a Y4M pipe.
//!
//! Frames pass through untouched (`-fps_mode passthrough`: none dropped or
//! repeated; `-noautoscale`: never resized). The only pixel operation ever
//! requested is the repack of a semi-planar format to the planar one of the
//! same depth and subsampling. The pipe is the only buffer, so memory stays
//! bounded however long the file is.

use std::path::Path;
use std::process::ChildStdout;

use studio_domain::job::FailureCode;

use super::probe::Probed;
use super::process::{self, Running};
use super::{Program, Tool, command};
use crate::job::Stop;

/// A running decode. Dropping it kills and reaps the child.
#[derive(Debug)]
pub struct Decoder {
    running: Running,
}

/// Starts `ffmpeg` on the selected stream of `path`.
///
/// # Errors
/// [`FailureCode::InvalidSource`] when it cannot start.
pub fn start(tool: &Tool, path: &Path, probed: &Probed) -> Result<(Decoder, ChildStdout), Stop> {
    let map = format!("0:{}", probed.facts.video_stream);
    let mut ffmpeg = command(tool, Program::Ffmpeg, path, &["-nostdin", "-nostats"])?;
    ffmpeg.args(["-map", &map, "-an", "-sn", "-dn"]);
    ffmpeg.args(["-fps_mode", "passthrough", "-noautoscale"]);
    if let Some(planar) = probed.repack {
        ffmpeg.args(["-pix_fmt", planar]);
    }
    // `-strict -1` admits the 10-bit Y4M colourspaces, which are not in the
    // original Y4M specification.
    ffmpeg.args(["-strict", "-1", "-f", "yuv4mpegpipe", "pipe:1"]);
    let (running, stdout) = process::spawn(ffmpeg).map_err(|e| {
        Stop::failed(
            FailureCode::InvalidSource,
            format!("cannot start ffmpeg: {e}"),
        )
    })?;
    Ok((Decoder { running }, stdout))
}

impl Decoder {
    /// After the pipe ended: the decode must have exited cleanly.
    ///
    /// # Errors
    /// [`FailureCode::InvalidSource`] with `ffmpeg`'s diagnostics.
    pub fn finish(self) -> Result<(), Stop> {
        let ended = self.running.finish().map_err(|e| {
            Stop::failed(
                FailureCode::InvalidSource,
                format!("cannot wait for ffmpeg: {e}"),
            )
        })?;
        if ended.clean() {
            Ok(())
        } else {
            Err(Stop::failed(
                FailureCode::InvalidSource,
                format!("decoding failed: ffmpeg {}", ended.describe()),
            ))
        }
    }

    /// Stops the decode after `problem` and adds what `ffmpeg` said.
    #[must_use]
    pub fn fail(self, problem: Stop) -> Stop {
        let said = self.running.abort();
        let said = said.trim();
        match problem {
            Stop::Failed(mut failure) if !said.is_empty() => {
                failure.message = format!(
                    "{}; ffmpeg reported: {}",
                    failure.message,
                    process::quote(said)
                );
                Stop::Failed(failure)
            }
            other => other,
        }
    }
}
