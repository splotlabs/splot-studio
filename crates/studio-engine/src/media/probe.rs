// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The stream inventory from `ffprobe`'s JSON, and the input contract.
//!
//! Only the machine-readable JSON is interpreted; diagnostics are quoted in
//! messages, and matched only to tell a refused container from a broken
//! file. Nothing is decoded beyond what `ffprobe` needs to fill in stream
//! parameters, and the first packets are read only for their side data
//! (encryption).
//!
//! Accepted: one progressive (or unstated) video stream in planar 8-bit or
//! 10-bit little-endian YUV 4:2:0, 4:2:2, 4:4:4 or greyscale, or the
//! semi-planar `nv12`/`p010le`, which `FFmpeg` repacks to planar samples at
//! the same depth. Limited or unstated range, SDR or unstated transfer,
//! square or unstated pixels, no rotation, crop, stereo or alpha. Everything
//! else is refused with the reason.

use std::time::Duration;

use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::Value;
use splot_encode::y4m::Y4mHeader;
use splot_encode::{MAX_FRAME_DIMENSION, PixelFormat};
use studio_domain::job::{
    ChromaLayout, Disclosure, FailureCode, Omissions, Rational, SourceFacts, SourceFingerprint,
    SourceReader, VideoTrack,
};

use super::{Program, Tool, command, process};
use crate::job::Stop;
use crate::source;

/// The longest JSON report read; anything longer is refused.
const MAX_REPORT_BYTES: usize = 8 << 20;

/// How long `ffprobe` may take on one file.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// The most streams a source may have.
pub const MAX_STREAMS: usize = 256;

/// Packets read from the start of the file for their side data.
const PACKETS_INSPECTED: &str = "%+#16";

/// Characters of a codec name kept for display.
const MAX_CODEC_CHARS: usize = 32;

/// A probed and accepted source.
#[derive(Debug, Clone)]
pub struct Probed {
    /// What the app sees.
    pub facts: SourceFacts,
    /// The Y4M stream header `ffmpeg` must produce for the selected stream.
    pub header: Y4mHeader,
    /// The pixel format the decoder produces, as `FFmpeg` names it.
    pub pix_fmt: String,
    /// The planar format `ffmpeg` is asked for when `pix_fmt` is
    /// semi-planar.
    pub repack: Option<&'static str>,
    /// The selected stream's time base, seconds per tick, as `(num, den)`.
    pub time_base: (i64, i64),
}

#[derive(Debug, Deserialize)]
struct Report {
    #[serde(default)]
    streams: Vec<Stream>,
    #[serde(default)]
    chapters: Vec<IgnoredAny>,
    #[serde(default)]
    packets: Vec<Packet>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Stream {
    index: u32,
    codec_type: String,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    pix_fmt: Option<String>,
    color_range: Option<String>,
    color_space: Option<String>,
    color_transfer: Option<String>,
    color_primaries: Option<String>,
    field_order: Option<String>,
    sample_aspect_ratio: Option<String>,
    r_frame_rate: Option<String>,
    time_base: Option<String>,
    start_pts: Option<i64>,
    disposition: Disposition,
    side_data_list: Vec<SideData>,
    tags: serde_json::Map<String, Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Disposition {
    attached_pic: i64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SideData {
    side_data_type: String,
    displaymatrix: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Packet {
    side_data_list: Vec<SideData>,
}

/// Probes `path` with `tool` and selects `requested`, or the first video
/// stream that is not an attached picture.
///
/// # Errors
/// [`FailureCode::InvalidSource`] when `ffprobe` cannot read the file,
/// [`FailureCode::UnsupportedSource`] for a refused container, stream
/// selection or video, [`FailureCode::TooLarge`] beyond the size limits.
pub fn probe(
    tool: &Tool,
    path: &std::path::Path,
    fingerprint: SourceFingerprint,
    requested: Option<u32>,
) -> Result<Probed, Stop> {
    let mut ffprobe = command(
        tool,
        Program::Ffprobe,
        path,
        &["-read_intervals", PACKETS_INSPECTED],
    )?;
    ffprobe.args([
        "-print_format",
        "json",
        "-show_streams",
        "-show_chapters",
        "-show_entries",
        "packet=stream_index:packet_side_data=side_data_type",
    ]);
    let output = process::run_bounded(ffprobe, MAX_REPORT_BYTES, PROBE_TIMEOUT)
        .map_err(|e| invalid(format!("ffprobe could not read the source: {e}")))?;
    if !output.ended.status.success() {
        return Err(refusal(&output.ended));
    }
    let report: Report = serde_json::from_slice(&output.stdout)
        .map_err(|e| invalid(format!("ffprobe's report is not readable: {e}")))?;
    accept(&report, fingerprint, requested, &output.ended.stderr)
}

/// Maps a failed `ffprobe`. The tool says "not on whitelist" when the
/// detected format is not one of [`super::FORMAT_WHITELIST`].
fn refusal(ended: &process::Ended) -> Stop {
    if ended.stderr.contains("whitelist") {
        return unsupported(
            "this kind of file is not read: Studio reads MP4, MOV, Matroska (MKV, WebM), \
             MPEG-TS and AVI files. Playlists, concatenation lists, image sequences and \
             other formats that can refer to further files are refused",
        );
    }
    invalid(format!(
        "ffprobe cannot read the source: it {}",
        ended.describe()
    ))
}

/// `diagnostics` is what `ffprobe` printed while it succeeded.
fn accept(
    report: &Report,
    fingerprint: SourceFingerprint,
    requested: Option<u32>,
    diagnostics: &str,
) -> Result<Probed, Stop> {
    if report.streams.len() > MAX_STREAMS {
        return Err(unsupported(format!(
            "the source has {} streams; at most {MAX_STREAMS} are read",
            report.streams.len()
        )));
    }
    let encrypted = report
        .packets
        .iter()
        .flat_map(|packet| &packet.side_data_list)
        .chain(report.streams.iter().flat_map(|s| &s.side_data_list))
        .any(|side| side.side_data_type.contains("Encryption"));
    if encrypted {
        return Err(unsupported(
            "the source is encrypted (DRM); encrypted media cannot be read",
        ));
    }
    let videos: Vec<&Stream> = report
        .streams
        .iter()
        .filter(|s| s.codec_type == "video")
        .collect();
    let selected = select(&report.streams, &videos, requested)?;
    let video_tracks = videos
        .iter()
        .map(|s| VideoTrack {
            stream: s.index,
            codec: s
                .codec_name
                .as_deref()
                .unwrap_or("unknown")
                .chars()
                .take(MAX_CODEC_CHARS)
                .collect(),
            width: s.width.unwrap_or(0),
            height: s.height.unwrap_or(0),
            attached_picture: s.disposition.attached_pic != 0,
        })
        .collect();
    let mut omissions = Omissions::default();
    for stream in &report.streams {
        if stream.index == selected.index {
            continue;
        }
        let count = match stream.codec_type.as_str() {
            "video" => &mut omissions.other_video,
            "audio" => &mut omissions.audio,
            "subtitle" => &mut omissions.subtitles,
            "attachment" => &mut omissions.attachments,
            _ => &mut omissions.other,
        };
        *count += 1;
    }
    omissions.chapters = u32::try_from(report.chapters.len()).unwrap_or(u32::MAX);
    let accepted = contract(selected, diagnostics)?;
    Ok(Probed {
        facts: SourceFacts {
            reader: SourceReader::Ffmpeg,
            fingerprint,
            video_stream: selected.index,
            video_tracks,
            omissions,
            width: accepted.header.width,
            height: accepted.header.height,
            frame_rate: Rational {
                num: accepted.header.frame_rate.0,
                den: accepted.header.frame_rate.1,
            },
            pixel_aspect: accepted.square.then_some(Rational { num: 1, den: 1 }),
            bit_depth: accepted.header.bit_depth,
            chroma: chroma(accepted.header.format),
            // Container frame counts include packets an edit list hides;
            // the job counts decoded frames instead.
            frame_count: None,
            disclosures: accepted.disclosures,
        },
        header: accepted.header,
        pix_fmt: accepted.pix_fmt,
        repack: accepted.repack,
        time_base: accepted.time_base,
    })
}

fn select<'a>(
    streams: &'a [Stream],
    videos: &[&'a Stream],
    requested: Option<u32>,
) -> Result<&'a Stream, Stop> {
    let Some(index) = requested else {
        return videos
            .iter()
            .copied()
            .find(|s| s.disposition.attached_pic == 0)
            .ok_or_else(|| {
                unsupported(if videos.is_empty() {
                    "the source has no video stream"
                } else {
                    "the source's only video is an attached picture (cover art)"
                })
            });
    };
    let stream = streams
        .iter()
        .find(|s| s.index == index)
        .ok_or_else(|| unsupported(format!("the source has no stream {index}")))?;
    if stream.codec_type != "video" {
        return Err(unsupported(format!(
            "stream {index} is {}, not video",
            if stream.codec_type.is_empty() {
                "of unknown type"
            } else {
                &stream.codec_type
            }
        )));
    }
    if stream.disposition.attached_pic != 0 {
        return Err(unsupported(format!(
            "stream {index} is an attached picture (cover art), not a video"
        )));
    }
    Ok(stream)
}

/// What the contract derived from the selected stream.
struct Accepted {
    header: Y4mHeader,
    pix_fmt: String,
    repack: Option<&'static str>,
    square: bool,
    time_base: (i64, i64),
    disclosures: Vec<Disclosure>,
}

/// Planar formats taken as they are, and semi-planar ones repacked to the
/// planar format of the same depth and subsampling.
fn pixel_format(name: &str) -> Option<(PixelFormat, u8, Option<&'static str>)> {
    Some(match name {
        "yuv420p" => (PixelFormat::Yuv420, 8, None),
        "yuv422p" => (PixelFormat::Yuv422, 8, None),
        "yuv444p" => (PixelFormat::Yuv444, 8, None),
        "gray" => (PixelFormat::Monochrome, 8, None),
        "yuv420p10le" => (PixelFormat::Yuv420, 10, None),
        "yuv422p10le" => (PixelFormat::Yuv422, 10, None),
        "yuv444p10le" => (PixelFormat::Yuv444, 10, None),
        "gray10le" => (PixelFormat::Monochrome, 10, None),
        "nv12" => (PixelFormat::Yuv420, 8, Some("yuv420p")),
        "p010le" => (PixelFormat::Yuv420, 10, Some("yuv420p10le")),
        _ => return None,
    })
}

fn pixel_format_refusal(name: &str) -> String {
    let reason = if name.starts_with("yuvj") {
        "full-range (JPEG) samples cannot be signalled by this encoder"
    } else if name.contains("yuva") || name.contains("rgba") || name.contains("bgra") {
        "it has an alpha channel"
    } else if name.contains("rgb") || name.contains("bgr") || name.starts_with("gbr") {
        "RGB samples are not encoded; only YUV"
    } else {
        "only planar 8-bit or 10-bit YUV 4:2:0, 4:2:2, 4:4:4 or greyscale is encoded"
    };
    format!("the video's pixel format {name} is not supported: {reason}")
}

/// A colour field the source actually states.
fn stated(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !matches!(*v, "" | "unknown" | "unspecified" | "reserved"))
}

/// The sample format, range, transfer and marks of the selected stream.
fn samples(
    stream: &Stream,
    diagnostics: &str,
) -> Result<(String, PixelFormat, u8, Option<&'static str>), Stop> {
    let index = stream.index;
    let codec = stream.codec_name.as_deref().unwrap_or("unknown");
    let Some(pix_fmt) = stream.pix_fmt.as_deref() else {
        // At `-v error` a missing decoder is silent; unreadable data is not.
        let diagnostics = diagnostics.trim();
        if !diagnostics.is_empty() {
            return Err(invalid(format!(
                "the {codec} video in stream {index} does not decode: ffprobe reported {}",
                process::quote(diagnostics)
            )));
        }
        return Err(unsupported(format!(
            "this FFmpeg build cannot decode the {codec} video in stream {index}"
        )));
    };
    let (format, bit_depth, repack) =
        pixel_format(pix_fmt).ok_or_else(|| unsupported(pixel_format_refusal(pix_fmt)))?;
    if stated(stream.color_range.as_deref()) == Some("pc") {
        return Err(unsupported(
            "full-range samples cannot be signalled by this encoder",
        ));
    }
    if let Some(transfer) = stated(stream.color_transfer.as_deref()) {
        hdr_transfer(transfer)?;
    }
    for side in &stream.side_data_list {
        side_data(side)?;
    }
    if stream
        .tags
        .get("rotate")
        .and_then(Value::as_str)
        .is_some_and(|r| r.trim() != "0")
    {
        return Err(rotated());
    }
    if stream
        .tags
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("alpha_mode") && v.as_str() != Some("0"))
    {
        return Err(unsupported(
            "the video has an alpha channel, which is not encoded",
        ));
    }
    Ok((pix_fmt.to_owned(), format, bit_depth, repack))
}

/// The input contract for the selected stream.
fn contract(stream: &Stream, diagnostics: &str) -> Result<Accepted, Stop> {
    let index = stream.index;
    let (pix_fmt, format, bit_depth, repack) = samples(stream, diagnostics)?;
    let interlace = match stated(stream.field_order.as_deref()) {
        None => '?',
        Some("progressive") => 'p',
        Some(order) => {
            return Err(unsupported(format!(
                "interlaced video (field order {order}) is not supported; only progressive"
            )));
        }
    };
    let square = match stream
        .sample_aspect_ratio
        .as_deref()
        .and_then(|s| ratio(s, ':'))
    {
        None | Some((0, _) | (_, 0)) => false,
        Some((num, den)) if num == den => true,
        Some((num, den)) => {
            return Err(unsupported(format!(
                "non-square pixels ({num}:{den}) cannot be signalled by this encoder"
            )));
        }
    };
    let (Some(width), Some(height)) = (stream.width, stream.height) else {
        return Err(invalid(format!("stream {index} states no frame size")));
    };
    if width == 0 || height == 0 {
        return Err(invalid(format!("stream {index} has an empty frame size")));
    }
    if width > MAX_FRAME_DIMENSION || height > MAX_FRAME_DIMENSION {
        return Err(Stop::failed(
            FailureCode::TooLarge,
            format!("{width}x{height} exceeds the encoder limit of {MAX_FRAME_DIMENSION}"),
        ));
    }
    let frame_rate = stream
        .r_frame_rate
        .as_deref()
        .and_then(|s| ratio(s, '/'))
        .filter(|&(n, d)| n > 0 && d > 0)
        .and_then(|(n, d)| {
            let (n, d) = reduce(n, d);
            Some((u32::try_from(n).ok()?, u32::try_from(d).ok()?))
        })
        .ok_or_else(|| {
            unsupported(format!(
                "stream {index} declares no usable frame rate ({}); only constant frame \
                 rate video is supported",
                stream.r_frame_rate.as_deref().unwrap_or("none")
            ))
        })?;
    let time_base = stream
        .time_base
        .as_deref()
        .and_then(|s| ratio(s, '/'))
        .filter(|&(n, d)| n > 0 && d > 0)
        .and_then(|(n, d)| Some((i64::try_from(n).ok()?, i64::try_from(d).ok()?)))
        .ok_or_else(|| unsupported(format!("stream {index} has no usable time base")))?;
    let header = Y4mHeader {
        width,
        height,
        frame_rate,
        aspect: if square { (1, 1) } else { (0, 0) },
        interlace,
        format,
        bit_depth,
    };
    source::check_contract(&header, false)?;
    let colour = [
        &stream.color_range,
        &stream.color_space,
        &stream.color_transfer,
        &stream.color_primaries,
    ];
    let mut disclosures = vec![if colour.iter().any(|v| stated(v.as_deref()).is_some()) {
        Disclosure::ColorNotCarried
    } else {
        Disclosure::ColorUnspecified
    }];
    if interlace == '?' {
        disclosures.push(Disclosure::ScanUnspecified);
    }
    if stream.start_pts.is_some_and(|pts| pts != 0) {
        disclosures.push(Disclosure::StartOffsetDropped);
    }
    if repack.is_some() {
        disclosures.push(Disclosure::LayoutRepacked);
    }
    Ok(Accepted {
        header,
        pix_fmt,
        repack,
        square,
        time_base,
        disclosures,
    })
}

/// Refuses PQ and HLG.
///
/// # Errors
/// [`FailureCode::UnsupportedSource`] for an HDR transfer.
pub fn hdr_transfer(transfer: &str) -> Result<(), Stop> {
    let name = match transfer {
        "smpte2084" => "HDR10/PQ (SMPTE ST 2084)",
        "arib-std-b67" => "HLG (ARIB STD-B67)",
        _ => return Ok(()),
    };
    Err(hdr(name))
}

/// The HDR refusal.
#[must_use]
pub fn hdr(what: &str) -> Stop {
    unsupported(format!(
        "the video is {what} HDR; HDR is not supported yet, only SDR"
    ))
}

/// Whether a side data type names HDR metadata.
#[must_use]
pub fn is_hdr_side_data(kind: &str) -> bool {
    kind.contains("HDR")
        || kind.contains("Dolby Vision")
        || kind.contains("DOVI")
        || kind.contains("Mastering display")
        || kind.contains("Content light level")
}

fn side_data(side: &SideData) -> Result<(), Stop> {
    let kind = side.side_data_type.as_str();
    if is_hdr_side_data(kind) {
        return Err(hdr(&format!("marked with {kind}")));
    }
    match kind {
        "Display Matrix" => {
            if side.displaymatrix.as_deref().and_then(matrix) != Some(IDENTITY) {
                return Err(rotated());
            }
        }
        "Frame Cropping" => {
            return Err(unsupported(
                "the container crops the picture, which is not applied or signalled",
            ));
        }
        "Stereo 3D" | "Spherical Mapping" => {
            return Err(unsupported(format!(
                "the video is marked with {kind}, which this encoder cannot signal"
            )));
        }
        _ => {}
    }
    Ok(())
}

/// The display matrix that leaves the picture as it is.
const IDENTITY: [i64; 9] = [65536, 0, 0, 0, 65536, 0, 0, 0, 1 << 30];

/// The nine numbers of `ffprobe`'s matrix dump: three rows of
/// `"<index>: a b c"`.
fn matrix(dump: &str) -> Option<[i64; 9]> {
    let numbers: Vec<i64> = dump
        .lines()
        .filter_map(|line| line.split_once(':'))
        .flat_map(|(_, row)| row.split_whitespace().map(str::parse))
        .collect::<Result<_, _>>()
        .ok()?;
    numbers.try_into().ok()
}

fn rotated() -> Stop {
    unsupported("the video is rotated or flipped for display, which is not applied or signalled")
}

/// `"a/b"` or `"a:b"` as two unsigned numbers.
fn ratio(text: &str, separator: char) -> Option<(u64, u64)> {
    let (num, den) = text.split_once(separator)?;
    Some((num.trim().parse().ok()?, den.trim().parse().ok()?))
}

fn reduce(num: u64, den: u64) -> (u64, u64) {
    let (mut a, mut b) = (num, den);
    while b != 0 {
        (a, b) = (b, a % b);
    }
    match (num.checked_div(a), den.checked_div(a)) {
        (Some(num), Some(den)) => (num, den),
        _ => (num, den),
    }
}

fn chroma(format: PixelFormat) -> ChromaLayout {
    match format {
        PixelFormat::Monochrome => ChromaLayout::Monochrome,
        PixelFormat::Yuv420 => ChromaLayout::Yuv420,
        PixelFormat::Yuv422 => ChromaLayout::Yuv422,
        PixelFormat::Yuv444 => ChromaLayout::Yuv444,
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
    use super::*;

    const FINGERPRINT: SourceFingerprint = SourceFingerprint {
        len: 1,
        modified: None,
    };

    fn report(json: &str) -> Report {
        serde_json::from_str(json).unwrap_or_else(|e| unreachable!("{e}"))
    }

    fn code(result: Result<Probed, Stop>) -> Option<FailureCode> {
        match result {
            Err(Stop::Failed(failure)) => Some(failure.code),
            _ => None,
        }
    }

    const VIDEO: &str = r#"{"index":1,"codec_type":"video","codec_name":"h264","width":64,
        "height":48,"pix_fmt":"yuv420p","r_frame_rate":"30000/1001","time_base":"1/90000",
        "field_order":"progressive","sample_aspect_ratio":"1:1","start_pts":0}"#;

    /// An attached picture first does not become the default selection.
    #[test]
    fn cover_art_is_never_the_default() {
        let json = format!(
            r#"{{"streams":[{{"index":0,"codec_type":"video","codec_name":"mjpeg","width":8,
            "height":8,"pix_fmt":"yuvj420p","disposition":{{"attached_pic":1}}}},{VIDEO},
            {{"index":2,"codec_type":"audio"}},{{"index":3,"codec_type":"data"}}],
            "chapters":[{{}},{{}},{{}}]}}"#
        );
        let probed = accept(&report(&json), FINGERPRINT, None, "");
        let Ok(probed) = probed else {
            unreachable!("{probed:?}");
        };
        assert_eq!(probed.facts.video_stream, 1);
        assert_eq!(probed.facts.video_tracks.len(), 2);
        assert!(probed.facts.video_tracks[0].attached_picture);
        assert_eq!(
            probed.facts.omissions,
            Omissions {
                other_video: 1,
                audio: 1,
                other: 1,
                chapters: 3,
                ..Omissions::default()
            }
        );
        assert_eq!(probed.facts.disclosures, [Disclosure::ColorUnspecified]);
        assert_eq!(
            code(accept(&report(&json), FINGERPRINT, Some(0), "")),
            Some(FailureCode::UnsupportedSource)
        );
        let only_cover = r#"{"streams":[{"index":0,"codec_type":"video",
            "disposition":{"attached_pic":1}}]}"#;
        assert_eq!(
            code(accept(&report(only_cover), FINGERPRINT, None, "")),
            Some(FailureCode::UnsupportedSource)
        );
    }

    #[test]
    fn identity_matrix_is_accepted_and_others_are_not() {
        let dump = "\n00000000:        65536           0           0\n00000001:            0       65536           0\n00000002:            0           0  1073741824\n";
        assert_eq!(matrix(dump), Some(IDENTITY));
        let rotated = "\n00000000:            0      -65536           0\n00000001:        65536           0           0\n00000002:            0           0  1073741824\n";
        assert_ne!(matrix(rotated), Some(IDENTITY));
        assert_eq!(matrix("garbage"), None);
    }

    #[test]
    fn too_many_streams_are_refused() {
        let streams: Vec<String> = (0..=MAX_STREAMS)
            .map(|i| format!(r#"{{"index":{i},"codec_type":"audio"}}"#))
            .collect();
        let json = format!(r#"{{"streams":[{}]}}"#, streams.join(","));
        assert_eq!(
            code(accept(&report(&json), FINGERPRINT, None, "")),
            Some(FailureCode::UnsupportedSource)
        );
    }

    #[test]
    fn rationals_reduce() {
        assert_eq!(reduce(60000, 2002), (30000, 1001));
        assert_eq!(reduce(0, 1), (0, 1));
        assert_eq!(ratio("16:15", ':'), Some((16, 15)));
        assert_eq!(ratio("0/0", '/'), Some((0, 0)));
        assert_eq!(ratio("N/A", '/'), None);
    }
}
