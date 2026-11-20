// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Sources: opening read-only, routing, the accepted input contract, and
//! the frames.
//!
//! A file that starts with the Y4M signature is read directly by upstream's
//! [`Y4mReader`]. Any other file is probed and decoded by the [`media`]
//! tools, whose Y4M output on a pipe goes through the same reader and the
//! same contract: 8 or 10 bits, progressive (or unstated) scan, square or
//! unstated pixels, limited (or unstated) range, and a frame size within the
//! byte budget. A source is bound to the [`SourceFingerprint`] the app saw.

use std::fs::{File, Metadata};
use std::io::{self, BufRead, BufReader, Chain, Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::ChildStdout;
use std::sync::atomic::AtomicBool;
use std::time::UNIX_EPOCH;

use splot_encode::y4m::{Y4mError, Y4mHeader, Y4mReader};
use splot_encode::{MAX_FRAME_DIMENSION, PixelFormat};
use studio_domain::job::{
    ChromaLayout, Disclosure, FailureCode, Omissions, Rational, SourceFacts, SourceFingerprint,
    SourceReader, VideoTrack,
};

use crate::job::Stop;
use crate::media::{self, decode::Decoder, probe::Probed};
use crate::output::FileId;

/// Bytes of the stream header line read ahead to inspect its tags. Upstream
/// accepts at most 4096 bytes before the newline; a longer line is replayed
/// to it unchanged and rejected there.
const HEADER_SCAN_BYTES: u64 = 8192;

/// A file starting with this is read as Y4M; the upstream reader then
/// requires the full `YUV4MPEG2 ` signature, so a damaged one is reported
/// as invalid Y4M rather than handed to `FFmpeg`.
const SIGNATURE: &[u8; 8] = b"YUV4MPEG";

/// Every frame record starts with this when it carries no frame parameters.
const PLAIN_FRAME_HEADER: &[u8; 6] = b"FRAME\n";

/// The largest working set a job may need, estimated as
/// [`FRAMES_IN_FLIGHT`] packed `u16` frames. About 4096x2160 4:2:0 fits.
pub const MEMORY_BUDGET_BYTES: u64 = 8 << 30;

/// A job's memory high-water mark as a multiple of one packed `u16` frame:
/// the eight-frame lookahead of every pinned preset plus references,
/// filtered copies and analysis planes. Release 1280x720 4:2:0 encodes at
/// presets 6 and 10 peaked at 425 to 483 MB RSS, up to 175 such frames;
/// this adds about a quarter.
pub const FRAMES_IN_FLIGHT: u64 = 224;

/// Where Y4M bytes come from.
#[derive(Debug)]
pub enum Input {
    /// A Y4M file.
    File(File),
    /// The standard output of the decoding `ffmpeg`.
    Pipe(ChildStdout),
}

impl Read for Input {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::File(file) => file.read(buf),
            Self::Pipe(pipe) => pipe.read(buf),
        }
    }
}

/// The Y4M frame reader of an open source.
pub type FrameReader = Y4mReader<Chain<Cursor<Vec<u8>>, BufReader<Input>>>;

/// Which file and stream a job means.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Selection {
    /// The file the app probed; `None` accepts the file as it is now.
    pub fingerprint: Option<SourceFingerprint>,
    /// The video stream; `None` picks the default one.
    pub video_stream: Option<u32>,
}

/// An open, accepted source.
#[derive(Debug)]
pub struct Source {
    /// The stream header the frames arrive with; for the media route, the
    /// one `ffmpeg` must produce.
    pub header: Y4mHeader,
    /// What the source is, for the app and the job report.
    pub facts: SourceFacts,
    /// The opened file's identity, for the source ≠ destination check.
    pub id: FileId,
    path: PathBuf,
    frames: Frames,
}

#[derive(Debug)]
enum Frames {
    Y4m(FrameReader),
    /// Probed; [`Source::start`] scans and starts the decoder.
    Media {
        probed: Box<Probed>,
        decoding: Option<Decoding>,
    },
}

#[derive(Debug)]
struct Decoding {
    reader: FrameReader,
    decoder: Option<Decoder>,
    expected: u64,
    read: u64,
}

/// Opens `path` read-only, routes it, and checks it against the input
/// contract and `selection`. Nothing is decoded yet.
///
/// # Errors
/// [`Stop::Failed`] with [`FailureCode::InvalidSource`],
/// [`FailureCode::UnsupportedSource`], [`FailureCode::TooLarge`],
/// [`FailureCode::SourceChanged`] or [`FailureCode::MediaToolMissing`].
pub fn open(path: &Path, selection: Selection) -> Result<Source, Stop> {
    let file = File::open(path).map_err(|e| invalid(format!("cannot open the source: {e}")))?;
    let metadata = file
        .metadata()
        .map_err(|e| invalid(format!("cannot inspect the source: {e}")))?;
    if !metadata.is_file() {
        return Err(invalid("the source is not a regular file"));
    }
    let id = FileId::of_file(&file, path)
        .map_err(|e| invalid(format!("cannot identify the source: {e}")))?;
    let fingerprint = fingerprint(&metadata);
    if selection
        .fingerprint
        .is_some_and(|seen| seen != fingerprint)
    {
        return Err(changed());
    }
    if metadata.len() == 0 {
        return Err(invalid("the source is empty"));
    }
    let mut buffered = BufReader::new(Input::File(file));
    let starts_y4m = buffered
        .fill_buf()
        .map_err(|e| invalid(format!("cannot read the source: {e}")))?
        .starts_with(SIGNATURE);
    if !starts_y4m {
        let tool = media::tool()?;
        let probed = media::probe::probe(tool, path, fingerprint, selection.video_stream)?;
        return Ok(Source {
            header: probed.header,
            facts: probed.facts.clone(),
            id,
            path: path.to_path_buf(),
            frames: Frames::Media {
                probed: Box::new(probed),
                decoding: None,
            },
        });
    }
    if selection.video_stream.is_some_and(|stream| stream != 0) {
        return Err(unsupported("a Y4M file has one video stream, stream 0"));
    }
    let (line, header, tags) = read_header(&mut buffered)?;
    let frame_bytes = check_header(&header, &tags)?;
    let frame_count = frame_count(
        &mut buffered,
        metadata.len(),
        line.len() as u64,
        frame_bytes,
    )?;
    let facts = SourceFacts {
        reader: SourceReader::Y4m,
        fingerprint,
        video_stream: 0,
        video_tracks: vec![VideoTrack {
            stream: 0,
            codec: "y4m".to_owned(),
            width: header.width,
            height: header.height,
            attached_picture: false,
        }],
        omissions: Omissions::default(),
        width: header.width,
        height: header.height,
        frame_rate: Rational {
            num: header.frame_rate.0,
            den: header.frame_rate.1,
        },
        pixel_aspect: match header.aspect {
            (0, 0) => None,
            (num, den) => Some(Rational { num, den }),
        },
        bit_depth: header.bit_depth,
        chroma: chroma(header.format),
        frame_count,
        disclosures: disclosures(&tags),
    };
    let reader = Y4mReader::new(Cursor::new(line).chain(buffered)).map_err(header_error)?;
    Ok(Source {
        header,
        facts,
        id,
        path: path.to_path_buf(),
        frames: Frames::Y4m(reader),
    })
}

/// Reads only what [`open`] reads and reports it.
///
/// # Errors
/// As [`open`].
pub fn probe(path: &Path, video_stream: Option<u32>) -> Result<SourceFacts, Stop> {
    open(
        path,
        Selection {
            fingerprint: None,
            video_stream,
        },
    )
    .map(|source| source.facts)
}

impl Source {
    /// Makes the frames readable. For the media route this decodes the
    /// selected stream once to prove its timing, geometry and format (see
    /// [`media::scan`]), then starts the decoder and checks the header it
    /// produces against the probe. Nothing is written.
    ///
    /// # Errors
    /// [`Stop::Cancelled`], or a failure of the scan, the decoder start or
    /// the header check.
    pub fn start(&mut self, cancel: &AtomicBool) -> Result<(), Stop> {
        let Frames::Media { probed, decoding } = &mut self.frames else {
            return Ok(());
        };
        if decoding.is_some() {
            return Ok(());
        }
        let tool = media::tool()?;
        let expected = media::scan::scan(tool, &self.path, probed, cancel)?;
        still_the_same(&self.path, &self.id, self.facts.fingerprint)?;
        let (decoder, stdout) = media::decode::start(tool, &self.path, probed)?;
        let mut buffered = BufReader::new(Input::Pipe(stdout));
        let checked = read_header(&mut buffered).and_then(|(line, header, tags)| {
            check_header(&header, &tags)?;
            matches(&header, &probed.header)?;
            Y4mReader::new(Cursor::new(line).chain(buffered)).map_err(header_error)
        });
        let reader = match checked {
            Ok(reader) => reader,
            Err(problem) => return Err(decoder.fail(problem)),
        };
        *decoding = Some(Decoding {
            reader,
            decoder: Some(decoder),
            expected,
            read: 0,
        });
        Ok(())
    }

    /// The next frame's planes, or `None` after the last one. For the media
    /// route the end is accepted only when the decoder exited cleanly after
    /// exactly the scanned number of frames and the file is unchanged.
    ///
    /// # Errors
    /// [`FailureCode::InvalidSource`] for a malformed, truncated or
    /// miscounted stream, [`FailureCode::SourceChanged`], or
    /// [`FailureCode::Encoder`] when called before [`Self::start`].
    pub fn read_frame(&mut self) -> Result<Option<[Vec<u16>; 3]>, Stop> {
        let decoding = match &mut self.frames {
            Frames::Y4m(reader) => return reader.read_frame().map_err(read_error),
            Frames::Media {
                decoding: Some(decoding),
                ..
            } => decoding,
            Frames::Media { decoding: None, .. } => {
                return Err(Stop::failed(
                    FailureCode::Encoder,
                    "the source was read before it was started",
                ));
            }
        };
        let frame = match decoding.reader.read_frame() {
            Ok(frame) => frame,
            Err(error) => {
                let problem = read_error(error);
                return Err(match decoding.decoder.take() {
                    Some(decoder) => decoder.fail(problem),
                    None => problem,
                });
            }
        };
        if frame.is_some() {
            decoding.read += 1;
            if decoding.read > decoding.expected {
                let problem = invalid(format!(
                    "ffmpeg produced more frames than the {} the scan found",
                    decoding.expected
                ));
                return Err(match decoding.decoder.take() {
                    Some(decoder) => decoder.fail(problem),
                    None => problem,
                });
            }
            return Ok(frame);
        }
        if let Some(decoder) = decoding.decoder.take() {
            decoder.finish()?;
        }
        if decoding.read != decoding.expected {
            return Err(invalid(format!(
                "ffmpeg produced {} frames, but the scan found {}",
                decoding.read, decoding.expected
            )));
        }
        still_the_same(&self.path, &self.id, self.facts.fingerprint).map(|()| None)
    }
}

/// The tools open the file by name; it must still be the one probed.
fn still_the_same(path: &Path, id: &FileId, seen: SourceFingerprint) -> Result<(), Stop> {
    let now = std::fs::metadata(path).map_err(|_| changed())?;
    let same_file = FileId::of_path(path).is_ok_and(|now| now == *id);
    if same_file && fingerprint(&now) == seen {
        Ok(())
    } else {
        Err(changed())
    }
}

/// The file's size and modification time.
#[must_use]
pub fn fingerprint(metadata: &Metadata) -> SourceFingerprint {
    let modified = metadata.modified().ok().map(|time| {
        match time.duration_since(UNIX_EPOCH) {
            Ok(after) => (
                i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
                after.subsec_nanos(),
            ),
            // Before the epoch: whole seconds rounded down, nanoseconds up.
            Err(before) => {
                let before = before.duration();
                let secs = i64::try_from(before.as_secs()).unwrap_or(i64::MAX);
                match before.subsec_nanos() {
                    0 => (-secs, 0),
                    nanos => (-secs - 1, 1_000_000_000 - nanos),
                }
            }
        }
    });
    SourceFingerprint {
        len: metadata.len(),
        modified,
    }
}

/// Reads and parses one stream header line, keeping its bytes.
fn read_header(reader: &mut BufReader<Input>) -> Result<(Vec<u8>, Y4mHeader, Tags), Stop> {
    let mut line = Vec::new();
    (&mut *reader)
        .take(HEADER_SCAN_BYTES)
        .read_until(b'\n', &mut line)
        .map_err(|e| invalid(format!("cannot read the source: {e}")))?;
    let header = *Y4mReader::new(line.as_slice())
        .map_err(header_error)?
        .header();
    let tags = tags(&line);
    Ok((line, header, tags))
}

/// The decoder's header must say what the probe said.
fn matches(found: &Y4mHeader, want: &Y4mHeader) -> Result<(), Stop> {
    let rate = |(num, den): (u32, u32)| {
        let (mut a, mut b) = (num, den);
        while b != 0 {
            (a, b) = (b, a % b);
        }
        (num / a.max(1), den / a.max(1))
    };
    let key = |h: &Y4mHeader| (h.width, h.height, h.bit_depth, h.format, rate(h.frame_rate));
    if key(found) == key(want) {
        Ok(())
    } else {
        Err(invalid(format!(
            "ffmpeg produced {:?}, but the probe found {:?} (width, height, bits, layout, rate)",
            key(found),
            key(want)
        )))
    }
}

/// Maps a frame read error.
#[must_use]
pub fn read_error(error: Y4mError) -> Stop {
    match error {
        Y4mError::Io(e) => invalid(format!("cannot read the source: {e}")),
        other => invalid(other.to_string()),
    }
}

/// The interlace and colour-range tags of the header line, which the parsed
/// [`Y4mHeader`] cannot tell apart from their defaults.
#[derive(Debug, Default)]
struct Tags {
    interlace: Option<Vec<u8>>,
    color_range: Option<Vec<u8>>,
}

fn tags(line: &[u8]) -> Tags {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let mut tags = Tags::default();
    // Later tags win, as in the upstream parser.
    for token in line.split(|&byte| byte == b' ').skip(1) {
        if let Some(value) = token.strip_prefix(b"I") {
            tags.interlace = Some(value.to_vec());
        } else if let Some(value) = token.strip_prefix(b"XCOLORRANGE=") {
            tags.color_range = Some(value.to_vec());
        }
    }
    tags
}

fn check_header(header: &Y4mHeader, tags: &Tags) -> Result<u64, Stop> {
    let full_range = tags
        .color_range
        .as_deref()
        .is_some_and(|value| value.eq_ignore_ascii_case(b"FULL"));
    check_contract(header, full_range)
}

/// Applies the input contract to a stream header and returns the bytes of
/// one frame's samples.
///
/// # Errors
/// [`FailureCode::UnsupportedSource`], [`FailureCode::InvalidSource`] for an
/// invalid aspect ratio, or [`FailureCode::TooLarge`].
pub fn check_contract(header: &Y4mHeader, full_range: bool) -> Result<u64, Stop> {
    if !matches!(header.bit_depth, 8 | 10) {
        return Err(unsupported(format!(
            "{}-bit samples are not supported; use 8-bit or 10-bit input",
            header.bit_depth
        )));
    }
    match header.aspect {
        (0, 0) => {}
        (0, _) | (_, 0) => {
            return Err(invalid(format!(
                "pixel aspect ratio {}:{} is invalid",
                header.aspect.0, header.aspect.1
            )));
        }
        (num, den) if num != den => {
            return Err(unsupported(format!(
                "non-square pixels ({num}:{den}) cannot be signalled by this encoder"
            )));
        }
        _ => {}
    }
    if full_range {
        return Err(unsupported(
            "full-range samples cannot be signalled by this encoder",
        ));
    }
    let bytes_per_sample = if header.bit_depth > 8 { 2 } else { 1 };
    let samples: u64 = header
        .plane_sizes()
        .iter()
        .map(|&(w, h)| w as u64 * h as u64)
        .sum();
    let in_flight = samples * 2 * FRAMES_IN_FLIGHT;
    if in_flight > MEMORY_BUDGET_BYTES {
        return Err(Stop::failed(
            FailureCode::TooLarge,
            format!(
                "a {}x{} job needs about {} MiB, over the {} MiB budget",
                header.width,
                header.height,
                in_flight >> 20,
                MEMORY_BUDGET_BYTES >> 20
            ),
        ));
    }
    Ok(samples * bytes_per_sample)
}

/// The frame count, when the file is exactly a whole number of plain frame
/// records. The first and last records are checked; since a record is never
/// shorter than a plain one, a plain last record at the expected offset means
/// every record before it is plain too.
fn frame_count(
    file: &mut BufReader<Input>,
    file_len: u64,
    header_len: u64,
    frame_bytes: u64,
) -> Result<Option<u64>, Stop> {
    let body = file_len.saturating_sub(header_len);
    if body == 0 {
        return Err(invalid("the source has no frames"));
    }
    let record = PLAIN_FRAME_HEADER.len() as u64 + frame_bytes;
    if !body.is_multiple_of(record) {
        return Ok(None);
    }
    let count = body / record;
    let read = |file: &mut BufReader<Input>, at: u64| -> io::Result<bool> {
        let mut bytes = [0; PLAIN_FRAME_HEADER.len()];
        file.seek(SeekFrom::Start(at))?;
        file.read_exact(&mut bytes)?;
        Ok(&bytes == PLAIN_FRAME_HEADER)
    };
    let plain = read(file, header_len)
        .and_then(|first| Ok(first && read(file, header_len + (count - 1) * record)?))
        .and_then(|plain| file.seek(SeekFrom::Start(header_len)).map(|_| plain))
        .map_err(|e| invalid(format!("cannot read the source: {e}")))?;
    Ok(plain.then_some(count))
}

impl Seek for Input {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        match self {
            Self::File(file) => file.seek(position),
            Self::Pipe(_) => Err(io::Error::from(io::ErrorKind::Unsupported)),
        }
    }
}

/// Every layout the pinned encoder takes; each is tested end to end.
fn chroma(format: PixelFormat) -> ChromaLayout {
    match format {
        PixelFormat::Monochrome => ChromaLayout::Monochrome,
        PixelFormat::Yuv420 => ChromaLayout::Yuv420,
        PixelFormat::Yuv422 => ChromaLayout::Yuv422,
        PixelFormat::Yuv444 => ChromaLayout::Yuv444,
    }
}

fn disclosures(tags: &Tags) -> Vec<Disclosure> {
    // Y4M has no field for primaries, transfer or matrix, and the pinned
    // encoder writes no colour description, so this always applies.
    let mut disclosures = vec![Disclosure::ColorUnspecified];
    if tags.interlace.as_deref().is_none_or(|value| value == b"?") {
        disclosures.push(Disclosure::ScanUnspecified);
    }
    disclosures
}

fn header_error(error: Y4mError) -> Stop {
    match error {
        Y4mError::InvalidTag { tag: 'I', value } if matches!(value.as_str(), "t" | "b" | "m") => {
            unsupported(format!(
                "interlaced sources (I{value}) are not supported; only progressive"
            ))
        }
        Y4mError::InvalidTag {
            tag: tag @ ('W' | 'H'),
            value,
        } if value
            .parse::<u64>()
            .is_ok_and(|v| v > u64::from(MAX_FRAME_DIMENSION)) =>
        {
            Stop::failed(
                FailureCode::TooLarge,
                format!("{tag}{value} exceeds the encoder limit of {MAX_FRAME_DIMENSION}"),
            )
        }
        Y4mError::UnsupportedColorspace(colorspace) => {
            unsupported(format!("Y4M colourspace {colorspace:?} is not supported"))
        }
        other => read_error(other),
    }
}

fn changed() -> Stop {
    Stop::failed(
        FailureCode::SourceChanged,
        "the source file changed since it was probed; probe it again",
    )
}

fn invalid(message: impl Into<String>) -> Stop {
    Stop::failed(FailureCode::InvalidSource, message)
}

fn unsupported(message: impl Into<String>) -> Stop {
    Stop::failed(FailureCode::UnsupportedSource, message)
}
