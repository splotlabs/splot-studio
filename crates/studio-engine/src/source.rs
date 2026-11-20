// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Y4M sources: opening read-only, the accepted input contract, and probing.
//!
//! Parsing is upstream's [`Y4mReader`]. This module only decides what Studio
//! accepts on top of it: 8 or 10 bits, progressive (or unstated) scan, square
//! or unstated pixels, limited (or unstated) range, and a frame size within
//! the byte budget.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Chain, Cursor, Read, Seek, SeekFrom};
use std::path::Path;

use splot_encode::y4m::{Y4mError, Y4mHeader, Y4mReader};
use splot_encode::{MAX_FRAME_DIMENSION, PixelFormat};
use studio_domain::job::{ChromaLayout, Disclosure, FailureCode, Rational, SourceFacts};

use crate::job::Stop;
use crate::output::FileId;

/// Bytes of the stream header line read ahead to inspect its tags. Upstream
/// accepts at most 4096 bytes before the newline; a longer line is replayed
/// to it unchanged and rejected there.
const HEADER_SCAN_BYTES: u64 = 8192;

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

/// The Y4M frame reader of an open source.
pub type FrameReader = Y4mReader<Chain<Cursor<Vec<u8>>, BufReader<File>>>;

/// An open, accepted source.
#[derive(Debug)]
pub struct Source {
    /// Streams the frames, one at a time.
    pub reader: FrameReader,
    /// The parsed stream header.
    pub header: Y4mHeader,
    /// What the header says, for the app and the job report.
    pub facts: SourceFacts,
    /// The opened file's identity, for the source ≠ destination check.
    pub id: FileId,
}

/// Opens `path` read-only and checks its header against the input contract.
///
/// # Errors
/// [`Stop::Failed`] with [`FailureCode::InvalidSource`],
/// [`FailureCode::UnsupportedSource`] or [`FailureCode::TooLarge`].
pub fn open(path: &Path) -> Result<Source, Stop> {
    let file = File::open(path).map_err(|e| invalid(format!("cannot open the source: {e}")))?;
    let metadata = file
        .metadata()
        .map_err(|e| invalid(format!("cannot inspect the source: {e}")))?;
    if !metadata.is_file() {
        return Err(invalid("the source is not a regular file"));
    }
    let id = FileId::of_file(&file, path)
        .map_err(|e| invalid(format!("cannot identify the source: {e}")))?;
    let mut buffered = BufReader::new(file);
    let mut line = Vec::new();
    (&mut buffered)
        .take(HEADER_SCAN_BYTES)
        .read_until(b'\n', &mut line)
        .map_err(|e| invalid(format!("cannot read the source: {e}")))?;
    let header = *Y4mReader::new(line.as_slice())
        .map_err(header_error)?
        .header();
    let tags = tags(&line);
    let frame_bytes = check_header(&header, &tags)?;
    let frame_count = frame_count(
        &mut buffered,
        metadata.len(),
        line.len() as u64,
        frame_bytes,
    )?;
    let facts = SourceFacts {
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
        reader,
        header,
        facts,
        id,
    })
}

/// Reads only what [`open`] reads and reports it.
///
/// # Errors
/// As [`open`].
pub fn probe(path: &Path) -> Result<SourceFacts, Stop> {
    open(path).map(|source| source.facts)
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
struct Tags<'a> {
    interlace: Option<&'a [u8]>,
    color_range: Option<&'a [u8]>,
}

fn tags(line: &[u8]) -> Tags<'_> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let mut tags = Tags::default();
    // Later tags win, as in the upstream parser.
    for token in line.split(|&byte| byte == b' ').skip(1) {
        if let Some(value) = token.strip_prefix(b"I") {
            tags.interlace = Some(value);
        } else if let Some(value) = token.strip_prefix(b"XCOLORRANGE=") {
            tags.color_range = Some(value);
        }
    }
    tags
}

/// Applies the input contract and returns the bytes of one frame's samples.
fn check_header(header: &Y4mHeader, tags: &Tags<'_>) -> Result<u64, Stop> {
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
    if tags
        .color_range
        .is_some_and(|value| value.eq_ignore_ascii_case(b"FULL"))
    {
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
    file: &mut BufReader<File>,
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
    let read = |file: &mut BufReader<File>, at: u64| -> io::Result<bool> {
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

/// Every layout the pinned encoder takes; each is tested end to end.
fn chroma(format: PixelFormat) -> ChromaLayout {
    match format {
        PixelFormat::Monochrome => ChromaLayout::Monochrome,
        PixelFormat::Yuv420 => ChromaLayout::Yuv420,
        PixelFormat::Yuv422 => ChromaLayout::Yuv422,
        PixelFormat::Yuv444 => ChromaLayout::Yuv444,
    }
}

fn disclosures(tags: &Tags<'_>) -> Vec<Disclosure> {
    // Y4M has no field for primaries, transfer or matrix, and the pinned
    // encoder writes no colour description, so this always applies.
    let mut disclosures = vec![Disclosure::ColorUnspecified];
    if tags.interlace.is_none_or(|value| value == b"?") {
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

fn invalid(message: impl Into<String>) -> Stop {
    Stop::failed(FailureCode::InvalidSource, message)
}

fn unsupported(message: impl Into<String>) -> Stop {
    Stop::failed(FailureCode::UnsupportedSource, message)
}
