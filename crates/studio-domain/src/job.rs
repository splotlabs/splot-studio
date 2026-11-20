// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Jobs: what the app asks the engine to encode and what the engine reports.
//!
//! These are versioned transport values, not codec structs. The engine maps
//! them to the pinned Splot API and is the authority on what is accepted.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::settings::EncoderSettings;

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

/// What the engine read from a source's header. Nothing here is inferred.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFacts {
    /// Luma width in pixels.
    pub width: u32,
    /// Luma height in pixels.
    pub height: u32,
    /// The constant frame rate.
    pub frame_rate: Rational,
    /// The pixel aspect ratio; `None` when the source does not state one.
    pub pixel_aspect: Option<Rational>,
    /// Bits per sample.
    pub bit_depth: u8,
    /// The chroma layout.
    pub chroma: ChromaLayout,
    /// The frame count when it is known without decoding the whole file.
    pub frame_count: Option<u64>,
    /// Things the user must know before encoding this source.
    pub disclosures: Vec<Disclosure>,
}

/// A property of a job that is true but easy to assume otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disclosure {
    /// The source states no primaries, transfer, matrix or range. Samples are
    /// encoded unchanged and the output carries no colour description from
    /// the source.
    ColorUnspecified,
    /// The source does not say whether it is progressive or interlaced. It is
    /// encoded as progressive frames.
    ScanUnspecified,
}

/// One immutable encode request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSpec {
    /// The file to read. It is never modified.
    pub source: WirePath,
    /// The file to create. An existing file is never replaced.
    pub destination: WirePath,
    /// The requested encoder settings, including the output kind.
    pub settings: EncoderSettings,
}

/// Where a running job is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
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
