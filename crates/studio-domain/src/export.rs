// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Decoded sample export requests and publication results.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::analysis::{Framing, SourceDigest};
use crate::job::{ChromaLayout, Failure, Rational, SourceFingerprint, WirePath};
use crate::preview::GrainPolicy;

/// The complete source observed in Analyze.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportSource {
    /// The local file to read.
    pub path: WirePath,
    /// The file size and modification time at inspection.
    pub fingerprint: SourceFingerprint,
    /// The verified length and SHA-256 of the complete file.
    pub digest: SourceDigest,
    /// The temporal unit framing from inspection.
    pub framing: Framing,
}

/// Selection in decoder display order, before layer filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExportRange {
    /// All output ordinals.
    All,
    /// Two inclusive zero-based endpoints. Equal endpoints select one frame.
    Inclusive {
        /// The first output ordinal.
        first: u64,
        /// The last output ordinal.
        last: u64,
    },
}

/// The exact layer of an output picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportLayer {
    /// The extended layer ID.
    pub extended: u8,
    /// The embedded layer ID.
    pub embedded: u8,
    /// The temporal layer ID.
    pub temporal: u8,
}

/// The expected decoded sample format from the inspected picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportSampleFormat {
    /// Bits per sample, 8 or 10.
    pub bit_depth: u8,
    /// The native chroma layout.
    pub chroma: ChromaLayout,
}

/// Output format and all destination paths for one export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExportFormat {
    /// A YUV4MPEG2 stream with a constant frame rate.
    Y4m {
        /// The video file.
        path: WirePath,
    },
    /// Packed decoded planes and a separate layout description.
    Raw {
        /// The decoded sample file.
        samples: WirePath,
        /// The layout and timing description.
        layout: WirePath,
    },
    /// A report of canonical pre-grain decoded sample hashes.
    Hashes {
        /// The hash report file.
        path: WirePath,
    },
}

impl ExportFormat {
    /// The files required by this format.
    pub fn destinations(&self) -> impl Iterator<Item = (ExportArtifactKind, &WirePath)> {
        let (first, second) = match self {
            Self::Y4m { path } => ((ExportArtifactKind::Video, path), None),
            Self::Raw { samples, layout } => (
                (ExportArtifactKind::Layout, layout),
                Some((ExportArtifactKind::Samples, samples)),
            ),
            Self::Hashes { path } => ((ExportArtifactKind::Hashes, path), None),
        };
        [Some(first), second].into_iter().flatten()
    }
}

/// An immutable request to export decoded samples or their hashes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportRequest {
    /// The inspected source identity.
    pub source: ExportSource,
    /// The selected output ordinals.
    pub range: ExportRange,
    /// The layer to export, or all layers when absent.
    pub layer: Option<ExportLayer>,
    /// The film grain policy.
    pub grain: GrainPolicy,
    /// An explicit rate for unknown timing. Known variable timing still cannot use Y4M.
    pub frame_rate: Option<Rational>,
    /// The sample format selected in Analyze.
    pub expected: ExportSampleFormat,
    /// The output format and paths.
    pub format: ExportFormat,
}

/// A request field contains an unsupported value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ExportRequestError {
    /// The request lacks a full source digest.
    #[error("Read the complete source in Analyze before exporting.")]
    Source,
    /// The selection has reversed endpoints.
    #[error("Set the last output at or after the first output.")]
    Range,
    /// A layer ID is outside its range.
    #[error("Select a valid output layer.")]
    Layer,
    /// The expected depth is unsupported.
    #[error("Select an 8-bit or 10-bit output.")]
    BitDepth,
    /// A frame rate component is zero.
    #[error("Specify a positive frame rate.")]
    FrameRate,
    /// The selected hash contract uses pre-grain samples.
    #[error("Turn off film grain to export canonical frame hashes.")]
    HashGrain,
    /// Two outputs name the same path.
    #[error("Select different paths for the samples and layout.")]
    DuplicateDestination,
}

impl ExportRequest {
    /// Checks request fields before file access.
    ///
    /// # Errors
    /// Returns the first invalid request field.
    pub fn validate(&self) -> Result<(), ExportRequestError> {
        let digest = &self.source.digest;
        if digest.bytes == 0
            || digest.bytes != self.source.fingerprint.len
            || digest.sha256.len() != 64
            || !digest
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ExportRequestError::Source);
        }
        if matches!(self.range, ExportRange::Inclusive { first, last } if first > last) {
            return Err(ExportRequestError::Range);
        }
        if self
            .layer
            .is_some_and(|layer| layer.extended > 30 || layer.embedded > 7 || layer.temporal > 3)
        {
            return Err(ExportRequestError::Layer);
        }
        if !matches!(self.expected.bit_depth, 8 | 10) {
            return Err(ExportRequestError::BitDepth);
        }
        if self
            .frame_rate
            .is_some_and(|rate| rate.num == 0 || rate.den == 0)
        {
            return Err(ExportRequestError::FrameRate);
        }
        match &self.format {
            ExportFormat::Hashes { .. } if self.grain != GrainPolicy::Skip => {
                Err(ExportRequestError::HashGrain)
            }
            ExportFormat::Raw { samples, layout } if samples == layout => {
                Err(ExportRequestError::DuplicateDestination)
            }
            _ => Ok(()),
        }
    }
}

/// An output file's role in the batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportArtifactKind {
    /// YUV4MPEG2 video.
    Video,
    /// Packed decoded samples.
    Samples,
    /// The raw sample layout description.
    Layout,
    /// The decoded frame hash report.
    Hashes,
}

/// A Unix file identity for safe recovery of a temporary output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportTempIdentity {
    /// Device number.
    pub device: u64,
    /// Inode number.
    pub inode: u64,
}

/// An exclusively created temporary file and its intended destination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportTemp {
    /// The file's role.
    pub kind: ExportArtifactKind,
    /// The temporary path created by the worker.
    pub partial: WirePath,
    /// The requested output path.
    pub destination: WirePath,
    /// The identity of the original open temporary file. Preserve the file if absent.
    pub identity: Option<ExportTempIdentity>,
    /// The identity of its original parent. Preserve the file if absent.
    pub parent_identity: Option<ExportTempIdentity>,
}

/// The result of securing the published file name on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportDurability {
    /// Publication and the supported sync checks succeeded.
    Confirmed,
    /// The file was published, but a sync check failed.
    Unconfirmed,
}

/// A file that the worker moved to its destination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportArtifact {
    /// The file's role.
    pub kind: ExportArtifactKind,
    /// The final path.
    pub path: WirePath,
    /// The verified file size.
    pub bytes: u64,
    /// The result of the publication sync check.
    pub durability: ExportDurability,
}

/// The current export phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportStage {
    /// Check the source and output paths.
    Checking,
    /// Decode frames and write the selected outputs.
    Decoding,
    /// Verify the source and staged files.
    Verifying,
    /// Move the verified files into place.
    Publishing,
}

/// Counts of work actually done by the exporter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportProgress {
    /// Output pictures decoded, including prerequisites outside the selection.
    pub decoded: u64,
    /// Selected output pictures written.
    pub exported: u64,
    /// Bytes written to the temporary files.
    pub bytes_written: u64,
}

/// The final result of one export request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExportOutcome {
    /// All requested files were published and synced.
    Completed {
        /// The number of selected output pictures.
        frames: u64,
        /// Every published file.
        files: Vec<ExportArtifact>,
    },
    /// At least one file was published, but the export did not complete.
    Partial {
        /// The verified frame count of the staged export.
        frames: u64,
        /// The files that reached their destinations.
        files: Vec<ExportArtifact>,
        /// Why the export did not complete.
        failure: Failure,
    },
    /// The export failed before it published a file.
    Failed {
        /// Why the export failed.
        failure: Failure,
    },
    /// The export stopped before publication.
    Cancelled,
}
