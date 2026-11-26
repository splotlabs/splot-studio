// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The check every frame passes before anything is encoded.
//!
//! `ffprobe` decodes the selected stream once and prints one line of
//! metadata per frame (no pixels), in presentation order. Each frame must
//! sit on the constant-rate grid of the declared frame rate and keep the
//! probed size, pixel format, scan and colour. The decoder drops frames an
//! edit list hides, as the encode's decoder does, so the count is the
//! number of frames the encode must read.
//!
//! Decoded frames are checked rather than container packets because the
//! `ffmpeg` command line silently rescales (or, with automatic scaling off,
//! misreads) frames whose size or format changes mid-stream; the scan is
//! what catches that.
//!
//! The grid: with time base `tb` seconds per tick and rate `r` frames per
//! second, one frame lasts `P = 1 / (r · tb)` ticks, an exact fraction
//! `N / D`. Frame `k` must satisfy `|pts_k − pts_0 − k·P| < 1` tick, that
//! is `|(pts_k − pts_0)·D − k·N| < D` in integers. A whole-tick period
//! therefore needs exact steps; a fractional one (29.97 fps in Matroska's
//! millisecond ticks) allows only the rounding to the nearest tick that
//! the container itself forces.

use std::io::{BufRead, BufReader, Read};
use std::sync::atomic::{AtomicBool, Ordering};

use studio_domain::job::FailureCode;

use super::probe::{Probed, hdr, hdr_transfer, is_hdr_side_data};
use super::{Program, Tool, command, process};
use crate::job::Stop;

/// The longest metadata line accepted for one frame.
const MAX_LINE_BYTES: u64 = 64 * 1024;

/// What a frame entry must not change from the first frame.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Colour {
    range: String,
    space: String,
    primaries: String,
    transfer: String,
}

/// The frame period in ticks as `(N, D)`, reduced.
fn period(probed: &Probed) -> Option<(i128, i128)> {
    let (tb_num, tb_den) = probed.time_base;
    let (rate_num, rate_den) = probed.header.frame_rate;
    let num = i128::from(rate_den) * i128::from(tb_den);
    let den = i128::from(rate_num) * i128::from(tb_num);
    if num <= 0 || den <= 0 {
        return None;
    }
    let (mut a, mut b) = (num, den);
    while b != 0 {
        (a, b) = (b, a % b);
    }
    Some((num / a, den / a))
}

/// Decodes the selected stream without its pixels and checks every frame.
/// Returns the frame count.
///
/// # Errors
/// [`Stop::Cancelled`]; [`FailureCode::UnsupportedSource`] for variable
/// frame rate, mid-stream changes, interlaced frames, full range or HDR;
/// [`FailureCode::InvalidSource`] when the decoder reports errors or finds
/// no frames.
pub fn scan(
    tool: &Tool,
    path: &std::path::Path,
    probed: &Probed,
    cancel: &AtomicBool,
) -> Result<u64, Stop> {
    let stream = probed.facts.video_stream.to_string();
    let mut ffprobe = command(
        tool,
        Program::Ffprobe,
        path,
        &["-threads", "0", "-select_streams", &stream],
    )?;
    ffprobe.args([
        "-show_entries",
        "frame=pts,width,height,pix_fmt,interlaced_frame,color_range,color_space,\
         color_primaries,color_transfer:frame_side_data=side_data_type",
        "-of",
        "compact=p=0",
    ]);
    let (running, stdout) =
        process::spawn(ffprobe).map_err(|e| invalid(format!("cannot start ffprobe: {e}")))?;
    let mut grid = Grid::new(probed)?;
    if let Err(problem) = check_frames(BufReader::new(stdout), &mut grid, cancel) {
        // A truncated or damaged file often shows up as a timestamp gap or
        // a missing frame first; what the decoder said is the real reason.
        let said = running.abort();
        return Err(match problem {
            Stop::Failed(failure) if !said.trim().is_empty() => invalid(format!(
                "the source does not decode cleanly: ffprobe reported {} ({})",
                process::quote(said.trim()),
                failure.message
            )),
            other => other,
        });
    }
    let ended = running
        .finish()
        .map_err(|e| invalid(format!("cannot wait for ffprobe: {e}")))?;
    if !ended.clean() {
        return Err(invalid(format!(
            "the source does not decode cleanly: ffprobe {}",
            ended.describe()
        )));
    }
    if grid.frames == 0 {
        return Err(invalid("the selected video stream decodes to no frames"));
    }
    Ok(grid.frames)
}

/// Feeds every frame entry of `lines` to `grid`.
fn check_frames(
    mut lines: BufReader<std::process::ChildStdout>,
    grid: &mut Grid<'_>,
    cancel: &AtomicBool,
) -> Result<(), Stop> {
    let mut line = Vec::new();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Stop::Cancelled);
        }
        line.clear();
        let read = (&mut lines)
            .take(MAX_LINE_BYTES)
            .read_until(b'\n', &mut line)
            .map_err(|e| invalid(format!("cannot read ffprobe's frame list: {e}")))?;
        if read == 0 {
            return Ok(());
        }
        if line.last() != Some(&b'\n') && read as u64 == MAX_LINE_BYTES {
            return Err(invalid(format!(
                "ffprobe printed a frame entry longer than {MAX_LINE_BYTES} bytes"
            )));
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim();
        if !text.is_empty() {
            grid.frame(text)?;
        }
    }
}

/// The running check.
struct Grid<'a> {
    probed: &'a Probed,
    /// The frame period `N / D` in ticks.
    period: (i128, i128),
    first: Option<(i64, Colour)>,
    frames: u64,
}

impl<'a> Grid<'a> {
    fn new(probed: &'a Probed) -> Result<Self, Stop> {
        let period =
            period(probed).ok_or_else(|| unsupported("the frame rate or time base is zero"))?;
        if period.0 < period.1 {
            return Err(unsupported(format!(
                "the stream's time base 1/{} is coarser than one frame at {}/{} fps, so a \
                 constant frame rate cannot be proven",
                probed.time_base.1 / probed.time_base.0.max(1),
                probed.header.frame_rate.0,
                probed.header.frame_rate.1
            )));
        }
        Ok(Self {
            probed,
            period,
            first: None,
            frames: 0,
        })
    }

    /// Checks one `key=value|...` entry.
    fn frame(&mut self, entry: &str) -> Result<(), Stop> {
        let k = self.frames;
        let mut pts = None;
        let mut size = (None, None);
        let mut pix_fmt = None;
        let mut colour = Colour::default();
        for field in entry.split('|') {
            let Some((key, value)) = field.split_once('=') else {
                continue;
            };
            match key {
                "pts" => pts = value.parse::<i64>().ok(),
                "width" => size.0 = value.parse::<u32>().ok(),
                "height" => size.1 = value.parse::<u32>().ok(),
                "pix_fmt" => pix_fmt = Some(value),
                "interlaced_frame" if value != "0" => {
                    return Err(unsupported(format!(
                        "frame {k} is interlaced; only progressive video is supported"
                    )));
                }
                "color_range" => value.clone_into(&mut colour.range),
                "color_space" => value.clone_into(&mut colour.space),
                "color_primaries" => value.clone_into(&mut colour.primaries),
                "color_transfer" => value.clone_into(&mut colour.transfer),
                key if key.ends_with("side_data_type") && is_hdr_side_data(value) => {
                    return Err(hdr(&format!("marked with {value} at frame {k}")));
                }
                _ => {}
            }
        }
        let probed = self.probed;
        let want = (Some(probed.header.width), Some(probed.header.height));
        if size != want || pix_fmt != Some(probed.pix_fmt.as_str()) {
            return Err(unsupported(format!(
                "frame {k} is {}x{} {}, not {}x{} {} like the stream; video whose size or \
                 format changes is not supported",
                size.0.unwrap_or(0),
                size.1.unwrap_or(0),
                pix_fmt.unwrap_or("of unknown format"),
                probed.header.width,
                probed.header.height,
                probed.pix_fmt
            )));
        }
        if colour.range == "pc" {
            return Err(unsupported(format!(
                "frame {k} is full range, which is not supported yet"
            )));
        }
        hdr_transfer(&colour.transfer)?;
        let Some(pts) = pts else {
            return Err(unsupported(format!(
                "frame {k} has no timestamp, so a constant frame rate cannot be proven"
            )));
        };
        let Some((pts_0, colour_0)) = &self.first else {
            self.first = Some((pts, colour));
            self.frames = 1;
            return Ok(());
        };
        if colour != *colour_0 {
            return Err(unsupported(format!(
                "the colour description changes at frame {k} ({colour_0:?} to {colour:?}); \
                 this is not supported"
            )));
        }
        let (n, d) = self.period;
        let error = (i128::from(pts) - i128::from(*pts_0)) * d - i128::from(k) * n;
        if error.abs() >= d {
            let (tb_num, tb_den) = probed.time_base;
            let millis = |ticks: i128| ticks * 1000 * i128::from(tb_num) / i128::from(tb_den);
            let found = millis(i128::from(pts) - i128::from(*pts_0));
            let expected = millis(i128::from(k) * n / d);
            return Err(unsupported(format!(
                "variable frame rate: frame {k} is at {found} ms, but a constant {}/{} fps \
                 puts it at {expected} ms. Only constant frame rate video is supported",
                probed.header.frame_rate.0, probed.header.frame_rate.1
            )));
        }
        self.frames += 1;
        Ok(())
    }
}

fn invalid(message: impl Into<String>) -> Stop {
    Stop::failed(FailureCode::InvalidSource, message)
}

fn unsupported(message: impl Into<String>) -> Stop {
    Stop::failed(FailureCode::UnsupportedSource, message)
}

#[cfg(test)]
mod tests {
    use splot_encode::PixelFormat;
    use splot_encode::y4m::Y4mHeader;
    use studio_domain::job::{
        ChromaLayout, Omissions, Rational, SourceFacts, SourceFingerprint, SourceReader,
    };

    use super::*;

    fn probed(rate: (u32, u32), time_base: (i64, i64)) -> Probed {
        Probed {
            facts: SourceFacts {
                reader: SourceReader::Ffmpeg,
                fingerprint: SourceFingerprint {
                    len: 0,
                    modified: None,
                },
                video_stream: 0,
                video_tracks: Vec::new(),
                omissions: Omissions::default(),
                width: 8,
                height: 8,
                frame_rate: Rational {
                    num: rate.0,
                    den: rate.1,
                },
                pixel_aspect: None,
                color: Default::default(),
                bit_depth: 8,
                chroma: ChromaLayout::Yuv420,
                frame_count: None,
                disclosures: Vec::new(),
            },
            header: Y4mHeader {
                width: 8,
                height: 8,
                frame_rate: rate,
                aspect: (0, 0),
                interlace: 'p',
                format: PixelFormat::Yuv420,
                bit_depth: 8,
                color_range: None,
                chroma_siting: None,
            },
            pix_fmt: "yuv420p".into(),
            repack: None,
            time_base,
            matrix: None,
            content: splot_encode::ContentDescription::default(),
        }
    }

    fn check(probed: &Probed, pts: &[i64]) -> Result<u64, String> {
        let mut grid = Grid::new(probed).map_err(|e| e.to_string())?;
        for p in pts {
            grid.frame(&format!(
                "pts={p}|width=8|height=8|pix_fmt=yuv420p|interlaced_frame=0"
            ))
            .map_err(|e| e.to_string())?;
        }
        Ok(grid.frames)
    }

    #[test]
    fn whole_tick_periods_need_exact_steps() {
        let mp4 = probed((30000, 1001), (1, 30000));
        assert_eq!(check(&mp4, &[0, 1001, 2002, 3003]), Ok(4));
        assert!(check(&mp4, &[0, 1001, 2003]).is_err());
        assert!(check(&mp4, &[0, 1001, 3003]).is_err(), "a dropped frame");
        // A start offset is fine; the grid starts at the first frame.
        assert_eq!(check(&mp4, &[5000, 6001, 7002]), Ok(3));
    }

    /// 29.97 fps in Matroska's millisecond ticks, as muxers round it.
    #[test]
    fn fractional_periods_allow_tick_rounding_only() {
        let mkv = probed((30000, 1001), (1, 1000));
        assert_eq!(check(&mkv, &[0, 33, 67, 100, 133, 167, 200, 234]), Ok(8));
        assert!(check(&mkv, &[0, 33, 67, 102]).is_err());
        assert!(check(&mkv, &[0, 33, 33]).is_err());
        // 25 fps whole ticks in milliseconds: 40 exactly.
        let pal = probed((25, 1), (1, 1000));
        assert!(check(&pal, &[0, 40, 80, 160]).is_err(), "the VFR fixture");
        assert_eq!(check(&pal, &[0, 40, 80, 120]), Ok(4));
    }

    #[test]
    fn missing_timestamps_and_coarse_time_bases_are_refused() {
        let mp4 = probed((25, 1), (1, 12800));
        let mut grid = Grid::new(&mp4).unwrap_or_else(|_| unreachable!());
        assert!(
            grid.frame("pts=N/A|width=8|height=8|pix_fmt=yuv420p")
                .is_err()
        );
        assert!(Grid::new(&probed((50, 1), (1, 25))).is_err());
    }

    #[test]
    fn changes_and_hdr_frames_are_refused() {
        let p = probed((25, 1), (1, 25));
        let mut grid = Grid::new(&p).unwrap_or_else(|_| unreachable!());
        assert!(
            grid.frame("pts=0|width=8|height=8|pix_fmt=yuv420p|color_range=tv")
                .is_ok()
        );
        for bad in [
            "pts=1|width=4|height=8|pix_fmt=yuv420p|color_range=tv",
            "pts=1|width=8|height=8|pix_fmt=nv12|color_range=tv",
            "pts=1|width=8|height=8|pix_fmt=yuv420p|color_range=pc",
            "pts=1|width=8|height=8|pix_fmt=yuv420p|color_range=unknown",
            "pts=1|width=8|height=8|pix_fmt=yuv420p|color_range=tv|interlaced_frame=1",
            "pts=1|width=8|height=8|pix_fmt=yuv420p|color_range=tv|color_transfer=smpte2084",
            "pts=1|width=8|height=8|pix_fmt=yuv420p|color_range=tv|side_datum/x:side_data_type=Mastering display metadata",
        ] {
            assert!(grid.frame(bad).is_err(), "{bad}");
        }
        assert!(
            grid.frame("pts=1|width=8|height=8|pix_fmt=yuv420p|color_range=tv")
                .is_ok()
        );
    }
}
