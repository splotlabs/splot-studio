// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Jobs: what the app asks the engine to encode and what the engine reports.
//!
//! These are versioned transport values, not codec structs. The engine maps
//! them to the pinned Splot API and is the authority on what is accepted.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::options::OptionRequest;
use crate::protocol::OptionsIdentity;

/// A native filesystem path in a form that survives JSON.
///
/// Paths that are not valid Unicode keep their exact bytes (Unix) or UTF-16
/// units (Windows), so a display string is never the filesystem authority.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum WirePath {
    /// A path that is valid Unicode on any platform.
    Utf8(String),
    /// The raw bytes of a non-Unicode Unix path.
    Unix(Vec<u8>),
    /// The UTF-16 units of a non-Unicode Windows path.
    Windows(Vec<u16>),
}

/// A [`WirePath`] made on another platform family cannot name a file here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("the path was encoded on another platform and cannot be used here")]
pub struct ForeignPath;

impl WirePath {
    /// Encodes `path` without loss.
    #[must_use]
    pub fn from_path(path: &Path) -> Self {
        if let Some(text) = path.to_str() {
            return Self::Utf8(text.to_owned());
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            Self::Unix(path.as_os_str().as_bytes().to_vec())
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            Self::Windows(path.as_os_str().encode_wide().collect())
        }
        #[cfg(not(any(unix, windows)))]
        {
            Self::Utf8(path.to_string_lossy().into_owned())
        }
    }

    /// Decodes the native path.
    ///
    /// # Errors
    /// Returns [`ForeignPath`] for a non-Unicode path from the other platform
    /// family.
    pub fn to_path_buf(&self) -> Result<PathBuf, ForeignPath> {
        match self {
            Self::Utf8(text) => Ok(PathBuf::from(text)),
            #[cfg(unix)]
            Self::Unix(bytes) => {
                use std::os::unix::ffi::OsStringExt;
                Ok(std::ffi::OsString::from_vec(bytes.clone()).into())
            }
            #[cfg(windows)]
            Self::Windows(units) => {
                use std::os::windows::ffi::OsStringExt;
                Ok(std::ffi::OsString::from_wide(units).into())
            }
            #[allow(unreachable_patterns)]
            _ => Err(ForeignPath),
        }
    }
}

/// An exact ratio, such as a frame rate of 30000/1001. Never a float.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rational {
    /// The numerator.
    pub num: u32,
    /// The denominator.
    pub den: u32,
}

/// How chroma is subsampled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChromaLayout {
    /// Luma only.
    Monochrome,
    /// 4:2:0.
    Yuv420,
    /// 4:2:2.
    Yuv422,
    /// 4:4:4.
    Yuv444,
}

/// The position of a chroma sample relative to the luma grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChromaSiting {
    /// At the left of the chroma cell.
    Left,
    /// At the center of the chroma cell.
    Center,
    /// At the top left of the chroma cell.
    TopLeft,
    /// At the top of the chroma cell.
    Top,
    /// At the bottom left of the chroma cell.
    BottomLeft,
    /// At the bottom of the chroma cell.
    Bottom,
}

/// Source colour values that the encoder can carry. Absent values are unspecified.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceColor {
    /// H.273 colour primaries.
    pub primaries: Option<u8>,
    /// H.273 transfer characteristics.
    pub transfer: Option<u8>,
    /// H.273 matrix coefficients.
    pub matrix: Option<u8>,
    /// Whether the source declares full range. False means declared limited range.
    pub full_range: Option<bool>,
    /// Chroma sample position.
    pub chroma_siting: Option<ChromaSiting>,
}

/// How the engine reads a source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceReader {
    /// Splot's own Y4M reader, straight from the file.
    Y4m,
    /// Decoded by an `FFmpeg` child process and piped to the same reader.
    Ffmpeg,
}

/// Identifies the exact file a probe looked at, so a job can refuse a source
/// that was replaced or edited since. Not a content hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceFingerprint {
    /// File length in bytes.
    pub len: u64,
    /// Modification time as seconds and nanoseconds since the Unix epoch;
    /// `None` when the filesystem reports none.
    pub modified: Option<(i64, u32)>,
}

impl SourceFingerprint {
    /// The size and modification time in `metadata`. The app and the engine
    /// both use this, so their readings of one file compare equal.
    #[must_use]
    pub fn of(metadata: &std::fs::Metadata) -> Self {
        let modified = metadata.modified().ok().map(|time| {
            match time.duration_since(std::time::UNIX_EPOCH) {
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
        Self {
            len: metadata.len(),
            modified,
        }
    }
}

/// One video stream of a source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoTrack {
    /// The stream index inside the container.
    pub stream: u32,
    /// The codec name as the reader reports it, for display only.
    pub codec: String,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Cover art or another still attached to the file, not a movie track.
    pub attached_picture: bool,
}

/// Everything in the source that a video-only output leaves out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Omissions {
    /// Video streams other than the selected one, attached pictures included.
    pub other_video: u32,
    /// Audio streams.
    pub audio: u32,
    /// Subtitle streams.
    pub subtitles: u32,
    /// Attachments such as fonts.
    pub attachments: u32,
    /// Data and other streams.
    pub other: u32,
    /// Chapters.
    pub chapters: u32,
}

impl Omissions {
    /// Whether the output would leave anything out.
    #[must_use]
    pub const fn any(&self) -> bool {
        self.other_video > 0
            || self.audio > 0
            || self.subtitles > 0
            || self.attachments > 0
            || self.other > 0
            || self.chapters > 0
    }
}

/// What the engine read from a source. Nothing here is inferred; the video
/// fields describe the selected stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFacts {
    /// How the source is read.
    pub reader: SourceReader,
    /// The file the probe saw.
    pub fingerprint: SourceFingerprint,
    /// The selected video stream; `0` for Y4M.
    pub video_stream: u32,
    /// Every video stream, the selected one included.
    pub video_tracks: Vec<VideoTrack>,
    /// What a video-only output of the selected stream leaves out.
    pub omissions: Omissions,
    /// Luma width in pixels.
    pub width: u32,
    /// Luma height in pixels.
    pub height: u32,
    /// The constant frame rate the source declares. A job checks the real
    /// timestamps before encoding.
    pub frame_rate: Rational,
    /// The pixel aspect ratio; `None` when the source does not state one.
    pub pixel_aspect: Option<Rational>,
    /// Bits per sample.
    pub bit_depth: u8,
    /// The chroma layout.
    pub chroma: ChromaLayout,
    /// Colour and sample properties that can be carried into the output.
    #[serde(default)]
    pub color: SourceColor,
    /// The frame count when it is known without decoding the whole file.
    pub frame_count: Option<u64>,
    /// Things the user must know before encoding this source.
    pub disclosures: Vec<Disclosure>,
}

/// A property of a job that is true but easy to assume otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disclosure {
    /// The source states no primaries, transfer or matrix. Samples are
    /// encoded unchanged and the output carries no colour description.
    ColorUnspecified,
    /// The source does not say whether it is progressive or interlaced. It is
    /// encoded as progressive frames.
    ScanUnspecified,
    /// The source states a primaries, transfer or matrix value the output
    /// has no code for. Samples are encoded unchanged and the output leaves
    /// that value unspecified.
    ColorNotCarried,
    /// The video starts at a non-zero time in the source. The output starts
    /// at zero.
    StartOffsetDropped,
    /// Samples were moved from a semi-planar layout to separate planes at the
    /// same bit depth and subsampling. No value changed.
    LayoutRepacked,
}

/// One immutable encode request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSpec {
    /// The file to read. It is never modified.
    pub source: WirePath,
    /// The file the user looked at, from [`SourceFacts::fingerprint`]. The
    /// engine refuses the job when the file no longer matches.
    pub fingerprint: SourceFingerprint,
    /// The video stream to encode, from [`SourceFacts::video_stream`].
    pub video_stream: u32,
    /// The user confirmed that everything in [`SourceFacts::omissions`] is
    /// left out. Required when there is anything to leave out; it belongs to
    /// this source, stream and output, and to no other job.
    pub omissions_acknowledged: bool,
    /// The file to create. An existing file is never replaced.
    pub destination: WirePath,
    /// The requested encoder settings, including the output kind.
    pub settings: OptionRequest,
    /// Option schema and codec revision used to preflight this job.
    /// Historical Simple jobs did not record an identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<OptionsIdentity>,
    /// Local first-pass artifact selected for this job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass_artifact: Option<WirePath>,
}

/// Where a running job is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// Decoding the source once to prove every frame has the same size,
    /// format and constant timing. Only for sources read through `FFmpeg`.
    /// Nothing is written and no [`Progress`] is reported.
    Checking,
    /// Reading frames and writing packets.
    Encoding,
    /// Flushing the encoder and finishing the temporary file.
    Finalizing,
    /// Checking the temporary file.
    Verifying,
    /// Moving the verified file to its destination.
    Publishing,
}

/// Real counters of a running job. There is no percentage here on purpose.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    /// Frames read from the source and given to the encoder.
    pub frames_read: u64,
    /// Frames the encoder has returned as packets. Lags `frames_read`.
    pub frames_encoded: u64,
    /// Bytes written to the output so far.
    pub bytes_written: u64,
    /// Frames the encoder has coded, in thousandths of a frame. A frame
    /// still inside the encoder counts by the share of its blocks coded, so
    /// this moves between packets. It runs ahead of `frames_encoded` and
    /// never goes back.
    pub frames_coded_milli: u64,
}

/// A named verification operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    /// The container or stream framing parses and its counts match the encode.
    Structure,
    /// Splot's own decoder decodes every frame. Same codebase as the encoder,
    /// so this is not independent proof.
    SplotDecode,
    /// An independent AVM decoder agrees with Splot's decode.
    AvmDecode,
}

/// What happened to one check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CheckOutcome {
    /// The check ran and passed.
    Passed,
    /// The check ran and failed.
    Failed {
        /// What was wrong.
        detail: String,
    },
    /// The check did not run. This is never a pass.
    NotRun {
        /// Why not.
        reason: String,
    },
}

/// One verification check and its outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    /// Which check.
    pub kind: CheckKind,
    /// What happened.
    pub outcome: CheckOutcome,
}

/// The result of a job whose file was published.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobReport {
    /// The published file.
    pub destination: WirePath,
    /// Frames encoded.
    pub frames: u64,
    /// Size of the published file in bytes.
    pub bytes: u64,
    /// Every check, including those that did not run.
    pub checks: Vec<Check>,
    /// Disclosures that applied to this job.
    pub disclosures: Vec<Disclosure>,
}

/// Why a probe or job did not succeed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCode {
    /// The source is missing, unreadable, malformed, empty or truncated.
    InvalidSource,
    /// The source is valid but outside what this build encodes.
    UnsupportedSource,
    /// The source needs `FFmpeg` and this build has no usable one.
    MediaToolMissing,
    /// The file changed since it was probed. Probe it again.
    SourceChanged,
    /// The source has streams a video-only output leaves out and the job
    /// does not acknowledge that.
    OmissionsNotAcknowledged,
    /// The frame size or memory needed exceeds the engine's limits.
    TooLarge,
    /// The settings are not accepted by the encoder.
    InvalidSettings,
    /// Source and destination are the same file.
    SourceIsDestination,
    /// The destination already exists.
    DestinationExists,
    /// Creating, writing or publishing the output failed.
    Output,
    /// The encoder reported an error.
    Encoder,
    /// The output was written but a verification check failed. Nothing was
    /// published.
    VerificationFailed,
}

/// A failure with its explanation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    /// The machine-readable reason.
    pub code: FailureCode,
    /// A human-readable explanation, safe to show.
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_paths_round_trip_as_text() {
        let path = Path::new("/tmp/zażółć gęślą jaźń/clip 01.y4m");
        let wire = WirePath::from_path(path);
        assert!(matches!(wire, WirePath::Utf8(_)));
        assert_eq!(wire.to_path_buf().as_deref(), Ok(path));
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_unix_paths_keep_their_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let raw = std::ffi::OsStr::from_bytes(b"/tmp/clip-\xff\xfe.y4m");
        let wire = WirePath::from_path(Path::new(raw));
        assert!(matches!(wire, WirePath::Unix(_)));
        let json = serde_json::to_string(&wire).unwrap_or_default();
        let back: Result<WirePath, _> = serde_json::from_str(&json);
        assert_eq!(
            back.ok().and_then(|w| w.to_path_buf().ok()).as_deref(),
            Some(Path::new(raw))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_windows_path_is_foreign_on_unix() {
        assert_eq!(
            WirePath::Windows(vec![0xD800]).to_path_buf(),
            Err(ForeignPath)
        );
    }
}
