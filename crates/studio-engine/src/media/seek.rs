// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Starting a decode at frame `N` by time, so its cost is the distance to
//! the key frame before it rather than `N`.
//!
//! Identity rule, the one the job's check enforces (see [`super::scan`]):
//! frame `N` is the frame at `pts_0 + N·P`, where `pts_0` is the first
//! decoded frame's timestamp and `P` the frame period in time-base ticks,
//! and every frame of an accepted file lies within one tick of that grid.
//! `ffmpeg` runs with `-copyts` (timestamps as the file has them),
//! `-seek_timestamp 1 -ss` to an absolute time [`SEEK_MARGIN_SECONDS`]
//! before the frame, `-noaccurate_seek`
//! (no trimming of its own), and a `select` that passes every frame whose
//! time is at least `pts_0 + N·P − P/2`. Before each seek, `ffprobe`
//! confirms that it lands on a key frame earlier than that threshold, so
//! decoding is established before it (an MPEG-TS seek lands on any packet
//! and fails this; such a file is then counted). The first frame through
//! is frame `N` whenever the grid rule holds and the threshold is more than
//! a tick from every frame: always for a whole-tick period, where the rule
//! means exact steps, and for a fractional one of at least
//! [`MIN_FRACTIONAL_TICKS`] ticks.
//!
//! A file whose first timestamp cannot be read, or whose period is too
//! fine for the rule to separate frames, gets no [`TimeIndex`]; it is then
//! read by counting frames from the start, which is exact for any file.
//! The rule is not checked for the file at hand (that needs the job's full
//! scan): a variable-frame-rate file, which a job refuses, can be shown
//! the frame nearest its nominal time.

use std::path::Path;
use std::time::Duration;

use super::probe::Probed;
use super::{Program, Tool, command, process};

/// How far before its frame a seek aims, in seconds.
pub const SEEK_MARGIN_SECONDS: i128 = 1;

/// The shortest fractional frame period, in ticks, read by time.
pub const MIN_FRACTIONAL_TICKS: i128 = 3;

/// Packets `ffprobe` may read to find the first frame's timestamp.
const FIRST_FRAME_PACKETS: &str = "%+#64";

/// How long finding the first frame may take.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(30);

/// Where frames are in time, for a file that can be read by time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeIndex {
    /// The first decoded frame's timestamp, in ticks.
    first: i128,
    /// Seconds per tick as `(num, den)`.
    time_base: (i128, i128),
    /// The frame period in ticks as `(num, den)`, reduced.
    period: (i128, i128),
}

/// How a file's far frames are found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finder {
    /// By counting every frame from the first.
    Count,
    /// By time.
    Time(TimeIndex),
}

impl Finder {
    /// Looks the file up once: by time when it can be, else by counting.
    #[must_use]
    pub fn of(tool: &Tool, path: &Path, probed: &Probed) -> Self {
        TimeIndex::of(tool, path, probed).map_or(Self::Count, Self::Time)
    }

    /// How to start at frame `index`. A seek is used only after `ffprobe`
    /// confirms that it lands on a key frame before the frame: the decode
    /// is then established before the threshold, and the first frame past
    /// it is frame `index`. A seek that lands anywhere else (an MPEG-TS
    /// seek lands on any packet) makes the file counted from then on.
    pub fn start(&mut self, tool: &Tool, path: &Path, probed: &Probed, index: u64) -> Start {
        let Self::Time(times) = self else {
            return Start::Count(index);
        };
        let start = times.start(index);
        if let Start::Time {
            seek: Some(seek), ..
        } = &start
        {
            let landed = landing(tool, path, probed, seek);
            if !landed.is_some_and(|ticks| times.before(ticks, index)) {
                *self = Self::Count;
                return Start::Count(index);
            }
        }
        start
    }
}

/// How a decode starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Start {
    /// At this frame, counting every decoded frame from the first.
    Count(u64),
    /// At the first frame at or after `from` seconds, seeking first to
    /// `seek` seconds when given; both absolute, as the file states them.
    Time {
        /// The absolute seek target.
        seek: Option<String>,
        /// The `select` threshold.
        from: String,
    },
}

impl TimeIndex {
    /// The index of `probed`'s file, or `None` when it must be counted.
    #[must_use]
    pub fn of(tool: &Tool, path: &Path, probed: &Probed) -> Option<Self> {
        Self::new(first_timestamp(tool, path, probed)?, probed)
    }

    /// The index for a first timestamp of `first` ticks.
    #[must_use]
    pub fn new(first: i64, probed: &Probed) -> Option<Self> {
        let (tb_num, tb_den) = probed.time_base;
        let (rate_num, rate_den) = probed.header.frame_rate;
        let num = i128::from(rate_den) * i128::from(tb_den);
        let den = i128::from(rate_num) * i128::from(tb_num);
        if num <= 0 || den <= 0 {
            return None;
        }
        let gcd = gcd(num, den);
        let period = (num / gcd, den / gcd);
        let whole = period.1 == 1;
        (whole || period.0 >= MIN_FRACTIONAL_TICKS * period.1).then_some(Self {
            first: i128::from(first),
            time_base: (i128::from(tb_num), i128::from(tb_den)),
            period,
        })
    }

    /// How to start at frame `index`.
    #[must_use]
    pub fn start(&self, index: u64) -> Start {
        let (pn, pd) = self.period;
        let (tn, td) = self.time_base;
        // Threshold in ticks: first + index·P − P/2, over 2·pd.
        let ticks = self.first * 2 * pd + i128::from(index) * 2 * pn - pn;
        let (num, den) = (ticks * tn, 2 * pd * td);
        let seek = num - SEEK_MARGIN_SECONDS * den;
        let first = self.first * tn * 2 * pd;
        Start::Time {
            // Before the first frame, the decode simply starts there.
            seek: (seek > first).then(|| seconds(seek, den)),
            from: seconds(num, den),
        }
    }
}

impl TimeIndex {
    /// Whether a frame at `ticks` comes before frame `index`'s threshold.
    fn before(&self, ticks: i64, index: u64) -> bool {
        let (pn, pd) = self.period;
        i128::from(ticks) * 2 * pd < self.first * 2 * pd + i128::from(index) * 2 * pn - pn
    }
}

/// `num / den` seconds (`den > 0`) in decimal with nine places, rounded
/// down; every threshold is at least half a tick from a frame.
fn seconds(num: i128, den: i128) -> String {
    let nanos = num.saturating_mul(1_000_000_000).div_euclid(den);
    let sign = if nanos < 0 { "-" } else { "" };
    let nanos = nanos.unsigned_abs();
    format!(
        "{sign}{}.{:09}",
        nanos / 1_000_000_000,
        nanos % 1_000_000_000
    )
}

fn gcd(mut a: i128, mut b: i128) -> i128 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a.max(1)
}

/// The timestamp `ffmpeg` gives the first decoded frame of the selected
/// stream: its best-effort timestamp, as `ffprobe` reports it.
fn first_timestamp(tool: &Tool, path: &Path, probed: &Probed) -> Option<i64> {
    first_frame(tool, path, probed, FIRST_FRAME_PACKETS).map(|(ticks, _)| ticks)
}

/// The timestamp of the frame a seek to `seek` seconds starts decoding
/// with, when that is a key frame decoded without complaint. `ffprobe`
/// seeks as `ffmpeg -seek_timestamp 1 -ss` does (`avformat_seek_file` to
/// the absolute time) and decodes the one packet there.
fn landing(tool: &Tool, path: &Path, probed: &Probed, seek: &str) -> Option<i64> {
    let interval = format!("{seek}%+#1");
    match first_frame(tool, path, probed, &interval)? {
        (ticks, true) => Some(ticks),
        (_, false) => None,
    }
}

/// The first frame `ffprobe` decodes in `interval`: its best-effort
/// timestamp and whether it is a key frame. `None` when it decodes none
/// or reports any error.
fn first_frame(tool: &Tool, path: &Path, probed: &Probed, interval: &str) -> Option<(i64, bool)> {
    let stream = probed.facts.video_stream.to_string();
    let mut ffprobe = command(
        tool,
        Program::Ffprobe,
        path,
        &["-select_streams", &stream, "-read_intervals", interval],
    )
    .ok()?;
    ffprobe.args([
        "-show_entries",
        "frame=best_effort_timestamp,key_frame",
        "-of",
        "compact=p=0",
    ]);
    let output = process::run_bounded(ffprobe, 64 * 1024, FIRST_FRAME_TIMEOUT).ok()?;
    if !output.ended.clean() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let first = text.lines().find(|line| !line.trim().is_empty())?;
    let field = |name: &str| {
        first
            .split('|')
            .find_map(|field| field.trim().strip_prefix(name)?.strip_prefix('='))
    };
    Some((
        field("best_effort_timestamp")?.parse().ok()?,
        field("key_frame")? == "1",
    ))
}

#[cfg(test)]
mod tests {
    use splot_encode::PixelFormat;
    use splot_encode::y4m::Y4mHeader;
    use studio_domain::job::{
        ChromaLayout, Omissions, Rational, SourceColor, SourceFacts, SourceFingerprint,
        SourceReader,
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
                color: SourceColor::default(),
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

    fn time(start: &Start) -> (Option<&str>, &str) {
        match start {
            Start::Time { seek, from } => (seek.as_deref(), from.as_str()),
            Start::Count(_) => (None, ""),
        }
    }

    /// 29.97 fps in an MP4's 1/30000 ticks: frame 300 at 300.3 periods of
    /// 1001 ticks, threshold half a period earlier.
    #[test]
    fn thresholds_sit_half_a_period_before_the_frame() {
        let Some(index) = TimeIndex::new(0, &probed((30000, 1001), (1, 30000))) else {
            unreachable!()
        };
        // (300 · 1001 − 500.5) / 30000 s.
        assert_eq!(
            time(&index.start(300)),
            (Some("8.993316666"), "9.993316666")
        );
        // Near the start there is nothing to seek to.
        assert_eq!(time(&index.start(1)), (None, "0.016683333"));
        // A start offset (an MPEG-TS clock at 1.4 s in 1/90000 ticks).
        let Some(ts) = TimeIndex::new(126_000, &probed((25, 1), (1, 90000))) else {
            unreachable!()
        };
        assert_eq!(time(&ts.start(50)), (Some("2.380000000"), "3.380000000"));
    }

    /// Whole-tick periods are always read by time; fractional ones only
    /// when a frame is at least three ticks long.
    #[test]
    fn fine_periods_are_counted() {
        assert!(
            TimeIndex::new(0, &probed((30000, 1001), (1001, 30000))).is_some(),
            "AVI"
        );
        assert!(
            TimeIndex::new(0, &probed((30000, 1001), (1, 1000))).is_some(),
            "Matroska"
        );
        assert!(TimeIndex::new(0, &probed((30000, 1001), (1, 60))).is_none());
        assert!(TimeIndex::new(0, &probed((25, 1), (1, 0))).is_none());
        assert_eq!(seconds(-3, 2), "-1.500000000");
    }
}
