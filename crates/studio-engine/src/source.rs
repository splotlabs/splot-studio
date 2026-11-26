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

use std::fs::File;
use std::io::{self, BufRead, BufReader, Chain, Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::ChildStdout;
use std::sync::atomic::AtomicBool;

use splot_encode::y4m::{Y4mColorRange, Y4mError, Y4mHeader, Y4mReader};
use splot_encode::{ChromaSamplePosition, ContentDescription, MAX_FRAME_DIMENSION, PixelFormat};
use studio_domain::job::{
    ChromaLayout, ChromaSiting, Disclosure, FailureCode, Omissions, Rational, SourceColor,
    SourceFacts, SourceFingerprint, SourceReader, VideoTrack,
};
use studio_domain::preview::ColorMatrix;

use crate::job::Stop;
use crate::media::seek::{Finder, Start};
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
    /// The YCbCr matrix the source states; Y4M states none.
    pub matrix: Option<ColorMatrix>,
    /// The colour, range, chroma siting and pixel shape the source states,
    /// for the encoder to write.
    pub content: ContentDescription,
    path: PathBuf,
    identity_file: File,
    frames: Frames,
    /// The index of the frame [`Self::read_frame`] returns next.
    position: u64,
    /// For Y4M: what positioning needs.
    seek: Option<Y4mSeek>,
}

/// The source of a probed media file, before decoding starts.
fn media_source(probed: Probed, id: FileId, path: &Path, identity_file: File) -> Source {
    Source {
        header: probed.header,
        facts: probed.facts.clone(),
        id,
        matrix: probed.matrix,
        content: probed.content,
        path: path.to_path_buf(),
        identity_file,
        frames: Frames::Media {
            probed: Box::new(probed),
            decoding: None,
            times: None,
        },
        position: 0,
        seek: None,
    }
}

/// A Y4M file reopened at any frame.
#[derive(Debug)]
struct Y4mSeek {
    /// The stream header line, replayed before the frames.
    line: Vec<u8>,
    /// Bytes of one plain frame record, when every record is plain.
    record: Option<u64>,
}

#[derive(Debug)]
enum Frames {
    Y4m(FrameReader),
    /// Probed; [`Source::start`] scans and starts the decoder.
    Media {
        probed: Box<Probed>,
        decoding: Option<Decoding>,
        /// For pictures: how far frames are found, once looked up.
        times: Option<Finder>,
    },
}

#[derive(Debug)]
struct Decoding {
    reader: FrameReader,
    decoder: Option<Decoder>,
    /// The scanned frame count; `None` for an unscanned preview decode,
    /// which reads whatever the decoder delivers.
    expected: Option<u64>,
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
    let fingerprint = SourceFingerprint::of(&metadata);
    if selection
        .fingerprint
        .is_some_and(|seen| seen != fingerprint)
    {
        return Err(changed());
    }
    if metadata.len() == 0 {
        return Err(invalid("the source is empty"));
    }
    // A second handle for reopening at any frame (it shares the cursor,
    // which every reopening sets).
    let identity_file = file
        .try_clone()
        .map_err(|e| invalid(format!("cannot read the source: {e}")))?;
    let mut buffered = BufReader::new(Input::File(file));
    let starts_y4m = buffered
        .fill_buf()
        .map_err(|e| invalid(format!("cannot read the source: {e}")))?
        .starts_with(SIGNATURE);
    if !starts_y4m {
        let tool = media::tool()?;
        let probed = media::probe::probe(tool, path, fingerprint, selection.video_stream)?;
        return Ok(media_source(probed, id, path, identity_file));
    }
    if selection.video_stream.is_some_and(|stream| stream != 0) {
        return Err(unsupported("a Y4M file has one video stream, stream 0"));
    }
    let (line, header) = read_header(&mut buffered)?;
    let frame_bytes = check_contract(&header)?;
    let frame_count = frame_count(
        &mut buffered,
        metadata.len(),
        line.len() as u64,
        frame_bytes,
    )?;
    let content = header.content_description();
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
        color: source_color(&header, &content),
        frame_count,
        disclosures: disclosures(&header),
    };
    let seek = Y4mSeek {
        line: line.clone(),
        record: frame_count.map(|_| PLAIN_FRAME_HEADER.len() as u64 + frame_bytes),
    };
    let reader = Y4mReader::new(Cursor::new(line).chain(buffered)).map_err(header_error)?;
    Ok(Source {
        header,
        facts,
        id,
        matrix: None,
        content,
        path: path.to_path_buf(),
        identity_file,
        frames: Frames::Y4m(reader),
        position: 0,
        seek: Some(seek),
    })
}

/// Describes the colour values that the probe can pass to the encoder.
pub(crate) fn source_color(header: &Y4mHeader, content: &ContentDescription) -> SourceColor {
    let stated = |value| (value != ContentDescription::UNSPECIFIED).then_some(value);
    SourceColor {
        primaries: stated(content.color_primaries),
        transfer: stated(content.transfer_characteristics),
        matrix: stated(content.matrix_coefficients),
        full_range: header.color_range.map(|_| content.full_range),
        chroma_siting: content.chroma_sample_position.map(chroma_siting),
    }
}

const fn chroma_siting(position: ChromaSamplePosition) -> ChromaSiting {
    match position {
        ChromaSamplePosition::Left => ChromaSiting::Left,
        ChromaSamplePosition::Center => ChromaSiting::Center,
        ChromaSamplePosition::TopLeft => ChromaSiting::TopLeft,
        ChromaSamplePosition::Top => ChromaSiting::Top,
        ChromaSamplePosition::BottomLeft => ChromaSiting::BottomLeft,
        ChromaSamplePosition::Bottom => ChromaSiting::Bottom,
    }
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
        let Frames::Media {
            probed, decoding, ..
        } = &mut self.frames
        else {
            return Ok(());
        };
        if decoding.is_some() {
            return Ok(());
        }
        let tool = media::tool()?;
        let expected = media::scan::scan(tool, &self.path, probed, cancel)?;
        still_the_same(&self.path, &self.id, self.facts.fingerprint)?;
        *decoding = Some(Decoding::start(
            tool,
            &self.path,
            probed,
            &Start::Count(0),
            Some(expected),
        )?);
        Ok(())
    }

    /// For pictures: makes frame `index` the next one [`Self::read_frame`]
    /// returns, through the same decode path as a job. A Y4M file of plain
    /// frame records is reopened at the frame; any other Y4M file is read
    /// from its first frame. The media route reads ahead through a running
    /// decode when `index` is at most `read_ahead` frames on, and otherwise
    /// restarts `ffmpeg` at the frame, without the scan (so frame counts
    /// are not checked).
    ///
    /// # Errors
    /// As [`Self::read_frame`], or when the decode cannot start.
    pub fn position_at(&mut self, index: u64, read_ahead: u64) -> Result<(), Stop> {
        if let Some(seek) = &self.seek {
            let (offset, at) = match seek.record {
                Some(record) => (record.saturating_mul(index), index),
                None if index >= self.position => return self.skip_to(index),
                None => (0, 0),
            };
            let mut file = self
                .identity_file
                .try_clone()
                .map_err(|e| invalid(format!("cannot read the source: {e}")))?;
            file.seek(SeekFrom::Start(seek.line.len() as u64 + offset))
                .map_err(|e| invalid(format!("cannot read the source: {e}")))?;
            let input = Cursor::new(seek.line.clone()).chain(BufReader::new(Input::File(file)));
            self.frames = Frames::Y4m(Y4mReader::new(input).map_err(header_error)?);
            self.position = at;
            return self.skip_to(index);
        }
        let Frames::Media {
            probed,
            decoding,
            times,
        } = &mut self.frames
        else {
            return Ok(());
        };
        let running = decoding.as_ref().is_some_and(|d| d.decoder.is_some());
        if running && index >= self.position && index - self.position <= read_ahead {
            return self.skip_to(index);
        }
        // The old decode is killed and reaped before the new one starts.
        *decoding = None;
        let tool = media::tool()?;
        // Near the start, counting costs no more than a seek would.
        let start = if index > read_ahead {
            times
                .get_or_insert_with(|| Finder::of(tool, &self.path, probed))
                .start(tool, &self.path, probed, index)
        } else {
            Start::Count(index)
        };
        *decoding = Some(Decoding::start(tool, &self.path, probed, &start, None)?);
        self.position = index;
        Ok(())
    }

    /// Reads and drops frames until `index` is next.
    fn skip_to(&mut self, index: u64) -> Result<(), Stop> {
        while self.position < index {
            if self.read_frame()?.is_none() {
                break;
            }
        }
        Ok(())
    }

    /// The index of the frame [`Self::read_frame`] returns next.
    #[must_use]
    pub const fn position(&self) -> u64 {
        self.position
    }

    /// The frame count: the file's for Y4M, the scan's after
    /// [`Self::start`] for the media route.
    #[must_use]
    pub fn frame_count(&self) -> Option<u64> {
        self.facts.frame_count.or(match &self.frames {
            Frames::Media {
                decoding: Some(decoding),
                ..
            } => decoding.expected,
            _ => None,
        })
    }

    /// Kills a running media decode from another thread; a read blocked on
    /// it then fails.
    #[must_use]
    pub fn killer(&self) -> Option<media::process::Killer> {
        match &self.frames {
            Frames::Media {
                decoding:
                    Some(Decoding {
                        decoder: Some(decoder),
                        ..
                    }),
                ..
            } => Some(decoder.killer()),
            _ => None,
        }
    }

    /// Stops a running media decode, releasing its process; positioning
    /// starts a new one.
    pub fn rest(&mut self) {
        if let Frames::Media { decoding, .. } = &mut self.frames {
            *decoding = None;
        }
    }

    /// For the media route: whether pictures find far frames by time
    /// (`Some(true)`) or by counting (`Some(false)`); `None` before the
    /// first far frame was asked for, or for Y4M.
    #[must_use]
    pub fn reads_by_time(&self) -> Option<bool> {
        match &self.frames {
            Frames::Media {
                times: Some(times), ..
            } => Some(matches!(times, Finder::Time(_))),
            _ => None,
        }
    }

    /// Whether the file is still the one opened.
    ///
    /// # Errors
    /// [`FailureCode::SourceChanged`].
    pub fn unchanged(&self) -> Result<(), Stop> {
        still_the_same(&self.path, &self.id, self.facts.fingerprint)
    }

    /// Hash the file already opened. Restore the shared cursor before the
    /// Y4M reader uses the file again.
    pub(crate) fn pass_identity(
        &self,
        cancel: &AtomicBool,
    ) -> Result<crate::artifact::SourceIdentity, Stop> {
        self.unchanged()?;
        let mut file = &self.identity_file;
        let position = file.stream_position().map_err(hash_io)?;
        file.seek(SeekFrom::Start(0)).map_err(hash_io)?;
        let digest = crate::artifact::hash_source(&mut file, cancel);
        file.seek(SeekFrom::Start(position)).map_err(hash_io)?;
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(Stop::Cancelled);
        }
        let sha256 = digest.map_err(hash_io)?;
        self.unchanged()?;
        let adapter = match &self.frames {
            Frames::Y4m(_) => None,
            Frames::Media { probed, .. } => {
                let tool = media::tool()?;
                Some(
                    serde_json::to_string(&(
                        &tool.identity,
                        &probed.pix_fmt,
                        probed.repack,
                        probed.time_base,
                    ))
                    .map_err(hash_io)?,
                )
            }
        };
        // The artifact carries an exact frame count. A probe may estimate it.
        let mut facts = self.facts.clone();
        facts.frame_count = None;
        Ok(crate::artifact::SourceIdentity {
            sha256,
            facts,
            adapter,
        })
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
        let frame = self.next_frame()?;
        if frame.is_some() {
            self.position += 1;
        }
        Ok(frame)
    }

    fn next_frame(&mut self) -> Result<Option<[Vec<u16>; 3]>, Stop> {
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
            if let Some(expected) = decoding.expected.filter(|&e| decoding.read > e) {
                let problem = invalid(format!(
                    "ffmpeg produced more frames than the {expected} the scan found"
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
        if let Some(expected) = decoding.expected.filter(|&e| e != decoding.read) {
            return Err(invalid(format!(
                "ffmpeg produced {} frames, but the scan found {expected}",
                decoding.read
            )));
        }
        still_the_same(&self.path, &self.id, self.facts.fingerprint).map(|()| None)
    }
}

impl Decoding {
    /// Starts `ffmpeg` at frame `first` and checks the header it produces.
    fn start(
        tool: &media::Tool,
        path: &Path,
        probed: &Probed,
        start: &Start,
        expected: Option<u64>,
    ) -> Result<Self, Stop> {
        let (decoder, stdout) = media::decode::start(tool, path, probed, start)?;
        let mut buffered = BufReader::new(Input::Pipe(stdout));
        let checked = read_header(&mut buffered).and_then(|(line, header)| {
            check_contract(&header)?;
            // A decode that starts later (by count or with `-copyts` by
            // time) can make the muxer guess another rate from the frames
            // it passes, or none; it is checked for everything else.
            let rate = *start == Start::Count(0);
            matches(&header, &probed.header, rate)?;
            Y4mReader::new(Cursor::new(line).chain(buffered)).map_err(header_error)
        });
        match checked {
            Ok(reader) => Ok(Self {
                reader,
                decoder: Some(decoder),
                expected,
                read: 0,
            }),
            Err(problem) => Err(decoder.fail(problem)),
        }
    }
}

fn hash_io(error: impl std::fmt::Display) -> Stop {
    invalid(format!("Cannot identify the first-pass source. {error}"))
}

/// Open an analysis source for content verification and later reads.
pub(crate) fn open_verified(path: &Path) -> io::Result<File> {
    let mut options = File::options();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        // A replacement between the regular-file check and open can be a FIFO.
        options.custom_flags(i32::from_ne_bytes(
            rustix::fs::OFlags::NONBLOCK.bits().to_ne_bytes(),
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        // Permit other readers. Deny handles with write or delete access.
        const FILE_SHARE_READ: u32 = 1;
        options.share_mode(FILE_SHARE_READ);
    }
    options.open(path)
}

/// Detects a Unix file change even if its modification time is restored.
/// Windows readers use an open handle that denies write and delete access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileChangeStamp {
    #[cfg(unix)]
    changed: (i64, i64),
}

impl FileChangeStamp {
    #[allow(unused_variables)] // Unix supplies the change time.
    pub(crate) fn of(metadata: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self {
                changed: (metadata.ctime(), metadata.ctime_nsec()),
            }
        }
        #[cfg(not(unix))]
        {
            Self {}
        }
    }
}

/// Check both the path and the original handle before using verified evidence.
pub(crate) fn still_verified(
    path: &Path,
    id: &FileId,
    fingerprint: SourceFingerprint,
    file: &File,
    stamp: FileChangeStamp,
) -> Result<(), Stop> {
    still_the_same(path, id, fingerprint)?;
    let metadata = file.metadata().map_err(|_| changed())?;
    if FileChangeStamp::of(&metadata) != stamp || SourceFingerprint::of(&metadata) != fingerprint {
        return Err(changed());
    }
    Ok(())
}

/// The tools open the file by name; it must still be the one probed.
pub(crate) fn still_the_same(
    path: &Path,
    id: &FileId,
    seen: SourceFingerprint,
) -> Result<(), Stop> {
    let now = std::fs::metadata(path).map_err(|_| changed())?;
    let same_file = FileId::of_path(path).is_ok_and(|now| now == *id);
    if same_file && SourceFingerprint::of(&now) == seen {
        Ok(())
    } else {
        Err(changed())
    }
}

/// Reads and parses one stream header line, keeping its bytes.
fn read_header(reader: &mut BufReader<Input>) -> Result<(Vec<u8>, Y4mHeader), Stop> {
    let mut line = Vec::new();
    (&mut *reader)
        .take(HEADER_SCAN_BYTES)
        .read_until(b'\n', &mut line)
        .map_err(|e| invalid(format!("cannot read the source: {e}")))?;
    let header = *Y4mReader::new(line.as_slice())
        .map_err(header_error)?
        .header();
    Ok((line, header))
}

/// The decoder's header must say what the probe said.
fn matches(found: &Y4mHeader, want: &Y4mHeader, with_rate: bool) -> Result<(), Stop> {
    let rate = |(num, den): (u32, u32)| {
        let (mut a, mut b) = (num, den);
        while b != 0 {
            (a, b) = (b, a % b);
        }
        (num / a.max(1), den / a.max(1))
    };
    let key = |h: &Y4mHeader| {
        let rate = if with_rate {
            rate(h.frame_rate)
        } else {
            (0, 0)
        };
        (h.width, h.height, h.bit_depth, h.format, rate)
    };
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

/// Applies the input contract to a stream header and returns the bytes of
/// one frame's samples.
///
/// # Errors
/// [`FailureCode::UnsupportedSource`], [`FailureCode::InvalidSource`] for an
/// invalid aspect ratio, or [`FailureCode::TooLarge`].
pub fn check_contract(header: &Y4mHeader) -> Result<u64, Stop> {
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
                "non-square pixels ({num}:{den}) are not supported yet"
            )));
        }
        _ => {}
    }
    if header.color_range == Some(Y4mColorRange::Full) {
        return Err(unsupported("full-range samples are not supported yet"));
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

fn disclosures(header: &Y4mHeader) -> Vec<Disclosure> {
    // Y4M has no field for primaries, transfer or matrix, so this always
    // applies.
    let mut disclosures = vec![Disclosure::ColorUnspecified];
    if header.interlace == '?' {
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

#[cfg(all(test, windows))]
mod verified_file_tests {
    use super::*;

    #[test]
    fn a_verified_reader_excludes_writes_and_deletion() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("source.ivf");
        std::fs::write(&path, b"source")?;
        let writer = File::options().write(true).open(&path)?;
        assert!(open_verified(&path).is_err());
        drop(writer);

        let reader = open_verified(&path)?;
        let other_reader = open_verified(&path)?;
        assert!(File::options().write(true).open(&path).is_err());
        assert!(std::fs::remove_file(&path).is_err());

        drop(other_reader);
        drop(reader);
        std::fs::remove_file(path)
    }
}
