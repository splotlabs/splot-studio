// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Paged AV2 identities for one read-only scan and one decoder policy.
//!
//! The index observes temporal delimiters through Splot's envelope parser.
//! It gets coded frames and continuation OBUs from the decoder observer,
//! including coded frames that do not emit pictures. Output rows describe
//! pictures in display order, including pictures that display earlier samples.
//!
//! Link each OBU offset to the structural index with
//! [`crate::analysis::LocateTarget::Offset`]. The structural row identifies
//! its parent IVF packet. A packet can contain multiple temporal delimiters.
//! Use the nearest preceding delimiter in file order for temporal membership.
//!
//! Running pages and stopped scans can have partial coverage. Only a digest
//! match for the whole source verifies identity against the structure scan.
//! The index uses process-owned temporary files. Start another scan after
//! closing the source, changing the policy, or restarting the process.

use serde::{Deserialize, Serialize};

use crate::{
    analysis::SourceDigest, job::SourceFingerprint, layers::LayerDecodeSelection,
    preview::GrainPolicy,
};

/// The identity index schema.
pub const IDENTITY_SCHEMA: u32 = 2;

/// Maximum records per identity page.
pub const MAX_IDENTITY_PAGE: u32 = 128;

/// The rows a page selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityKind {
    /// Temporal delimiter locations.
    Temporal,
    /// Coded frames reported by the decoder.
    Coded,
    /// Continuation OBUs for coded frames.
    Continuation,
    /// Pictures the decoder returned, in display order.
    Output,
}

/// Layer IDs reported by the decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecodeLayers {
    /// Extended layer ID.
    pub extended: u8,
    /// Embedded layer ID.
    pub embedded: u8,
    /// Temporal layer ID.
    pub temporal: u8,
}

/// One identity record. Each offset names the first byte of an OBU header.
/// Resolve an offset through the structural index to obtain the OBU and its IVF packet.
/// An OBU belongs to the temporal unit of its nearest preceding delimiter, if one exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IdentityRow {
    /// A temporal delimiter observed in the source envelope.
    Temporal {
        /// File offset of the delimiter OBU.
        delimiter_offset: u64,
    },
    /// A coded frame admitted by the decoder. It can yield zero or more outputs.
    Coded {
        /// File offset of the first coded OBU.
        coded_offset: u64,
        /// File offset of the active sequence header.
        sequence_offset: u64,
        /// Layers of the coded frame.
        layers: DecodeLayers,
    },
    /// A tile group that continues an earlier coded frame.
    Continuation {
        /// File offset of the first coded OBU.
        coded_offset: u64,
        /// File offset of the continuation OBU.
        obu_offset: u64,
    },
    /// One decoded picture. The row index is its display ordinal.
    Output {
        /// File offset of the OBU that coded the samples.
        coded_offset: u64,
        /// File offset of the OBU that requested display.
        showing_offset: u64,
        /// File offset of the coded frame's sequence header.
        sequence_offset: u64,
        /// Layers of the output picture.
        layers: DecodeLayers,
    },
}

/// Why the scan stopped, or whether it still runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IdentityCompletion {
    /// More records may arrive.
    Running,
    /// The decoder reached the end of the source and returned all output pictures.
    Complete,
    /// The scan kept the records it had collected at cancellation.
    Cancelled,
    /// A read, decode or storage error stopped the scan with partial coverage.
    Stopped {
        /// The reason.
        message: String,
    },
}

/// Progress and verification for one identity scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentitySummary {
    /// The identity index schema.
    pub schema: u32,
    /// The exact Splot revision.
    pub codec_revision: String,
    /// The AV2 specification revision.
    pub spec_revision: String,
    /// The source fingerprint checked at the document boundary.
    pub fingerprint: SourceFingerprint,
    /// The film grain policy used during decoding.
    pub grain: GrainPolicy,
    /// The operating point requested for this identity scan.
    pub selection: LayerDecodeSelection,
    /// Contiguous prefix read during the identity scan. Read-ahead can include bytes
    /// beyond the decoded prefix. The digest is available after the scan stops.
    pub digest: Option<SourceDigest>,
    /// The identity and structure scans read matching bytes for this entire source.
    pub matches_structure: bool,
    /// The status of this scan.
    pub completion: IdentityCompletion,
    /// The number of temporal delimiters retained.
    pub temporal: u64,
    /// The number of coded frames retained.
    pub coded: u64,
    /// The number of continuation OBUs retained.
    pub continuations: u64,
    /// The number of decoded outputs retained.
    pub outputs: u64,
}

/// A page of identity records with a snapshot of coverage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityPage {
    /// The rows selected by the page request.
    pub kind: IdentityKind,
    /// Index of the first row in this page.
    pub first: u64,
    /// The current scan progress.
    pub summary: IdentitySummary,
    /// Records from `first` onward.
    pub rows: Vec<IdentityRow>,
}
