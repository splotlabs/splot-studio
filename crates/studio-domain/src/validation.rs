// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Findings and execution evidence from native AV2 validation.

use serde::{Deserialize, Serialize};

use crate::analysis::{AnalysisFailure, SourceDigest};
use crate::job::SourceFingerprint;

/// Maximum validation files retained or queued in one session.
pub const MAX_BATCH_FILES: usize = 32;

/// Output representation for an individual validation report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportFormat {
    /// Versioned JSON with structured findings.
    Json,
    /// Plain text for people to read.
    Text,
}

/// Terminal outcome of one file in a validation batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BatchOutcome {
    /// Validation returned execution evidence, which may be partial.
    Validated {
        /// Identity of the validated input.
        fingerprint: SourceFingerprint,
        /// SHA-256 and byte count from the validation reader, including read-ahead.
        digest: SourceDigest,
        /// Execution evidence and requested policy.
        summary: Box<ValidationSummary>,
        /// Retained finding count, independent of paging.
        total: u64,
    },
    /// The worker could not start or retain evidence for this file.
    Failed {
        /// Machine-readable reason.
        code: AnalysisFailure,
        /// Explanation for the local user.
        message: String,
    },
}

/// Version of the validation evidence format.
pub const VALIDATION_SCHEMA: u16 = 2;
/// Largest external Annex B context accepted in a request.
pub const MAX_EXTERNAL_HLS_BYTES: usize = 8192;

/// Options supplied with an individual validation request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationSettings {
    /// Treat warnings as a policy failure.
    pub strict: bool,
    /// Explicitly supplied sequence header and operating point set OBUs.
    pub external_hls: Option<Vec<u8>>,
}

/// Identity and availability keys of the supplied external high-level syntax.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextIdentity {
    /// SHA-256 digest of the supplied bytes.
    pub sha256: String,
    /// External (extended layer, sequence header) identifiers.
    pub sequence_headers: Vec<(u8, u32)>,
    /// External (extended layer, operating point set) identifiers.
    pub operating_point_sets: Vec<(u8, u8)>,
}

/// Native severity, independent of the strict option.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// A validation error.
    Error,
    /// A validation warning.
    Warning,
    /// An informational diagnostic.
    Info,
}

/// A finding in native emission order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// Stable Splot rule identifier.
    pub rule_id: String,
    /// Severity assigned by the rule.
    pub severity: Severity,
    /// Native diagnostic text.
    pub message: String,
    /// Applicable section if the rule supplied one.
    pub spec_section: Option<String>,
    /// Absolute file byte offset, including any IVF wrapper.
    pub file_offset: Option<u64>,
    /// Elementary-stream byte offset when the finding points into a known payload.
    pub stream_offset: Option<u64>,
    /// Bit index in the indicated byte, most significant bit first.
    pub bit_offset: Option<u8>,
    /// Extended layer ID of the indexed OBU that contains the finding's byte.
    pub extended_layer_id: Option<u8>,
}

/// Why validation ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ValidationCompletion {
    /// The reader reached the end of the input.
    EndOfInput,
    /// Malformed framing stopped the reader.
    MalformedInput,
    /// The user interrupted validation.
    Cancelled,
    /// A source read failed.
    Io {
        /// Reader error.
        message: String,
    },
    /// A declared unit exceeds the configured cap.
    UnitTooLarge {
        /// Absolute file byte offset.
        offset: u64,
        /// Declared size in bytes.
        declared: u64,
        /// Maximum permitted size in bytes.
        cap: u64,
    },
    /// Validation reached the configured OBU count limit.
    ObuLimit {
        /// Maximum number of OBUs checked.
        max_obus: u64,
    },
    /// The engine cannot interpret the native completion reason.
    Unsupported,
}

/// Execution evidence that applies to the entire run, including omitted findings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationSummary {
    /// Validation evidence format.
    pub schema: u16,
    /// AV2 specification revision used by these checks.
    pub spec_revision: String,
    /// Exact Splot Git revision used by the engine.
    pub codec_revision: String,
    /// The engine package version.
    pub engine_version: String,
    /// Requested policy for warnings.
    pub strict: bool,
    /// Identity of the supplied high-level syntax, when present.
    pub context: Option<ContextIdentity>,
    /// Which native checks this execution can describe.
    pub scope: String,
    /// Reason the validator stopped.
    pub completion: ValidationCompletion,
    /// Number of OBU envelopes passed to the native checks.
    pub checked_obus: u64,
    /// Exclusive file offset after the last checked OBU. Earlier IVF records may contain gaps.
    pub last_checked_obu_end: Option<u64>,
    /// Framing failures, including malformed units inside an IVF record.
    pub malformed_units: u64,
    /// Whether the native stream finalization checks ran.
    pub finalized: bool,
    /// Total error count before truncation.
    pub errors: u64,
    /// Total warning count before truncation.
    pub warnings: u64,
    /// Total informational count before truncation.
    pub info: u64,
    /// Maximum findings the worker retains.
    pub finding_limit: u32,
    /// Number of findings omitted by the retention limit.
    pub omitted: u64,
}

impl ValidationSummary {
    /// Whether the observed findings fail the requested severity policy.
    #[must_use]
    pub fn policy_failed(&self) -> bool {
        self.errors > 0 || (self.strict && self.warnings > 0)
    }
}

/// The result retained in the worker for paging.
#[derive(Debug)]
pub struct ValidationEvidence {
    /// Execution metadata, independent of the current page.
    pub summary: ValidationSummary,
    /// Retained findings in native emission order.
    pub findings: Vec<Finding>,
}
