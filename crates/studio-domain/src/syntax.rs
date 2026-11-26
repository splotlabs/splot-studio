// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Syntax that the codec observed within one OBU.

use serde::{Deserialize, Serialize};

/// Syntax evidence schema version.
pub const SYNTAX_SCHEMA: u16 = 1;

/// A zero-based bit position in both document coordinate systems.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BitPosition {
    /// Bit offset from the start of the file.
    pub file_bit: u64,
    /// Bit offset from the start of the elementary stream.
    pub stream_bit: u64,
}

/// Exact bits read for a signaled field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyntaxSpan {
    /// First bit of the field.
    pub start: BitPosition,
    /// Number of bits read.
    pub bit_len: u64,
}

/// Typed raw or inferred syntax value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyntaxValue {
    /// A flag.
    Bool(bool),
    /// An unsigned integer.
    Unsigned(u64),
    /// A signed integer.
    Signed(i64),
    /// Length of an opaque byte sequence.
    Bytes(u64),
    /// A value without a representation in this schema.
    Unavailable,
}

/// One syntax observation, in parse order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SyntaxEvent {
    /// Enter a named syntax structure.
    Structure {
        /// Canonical AV2 name.
        name: String,
        /// Section of the AV2 specification.
        spec_section: Option<String>,
    },
    /// Enter a loop iteration.
    LoopItem {
        /// Canonical name of the loop variable or collection.
        name: String,
        /// Zero-based iteration index.
        index: u64,
    },
    /// Enter a conditional branch.
    Condition {
        /// Canonical condition.
        condition: String,
        /// Whether the parser took the branch.
        taken: bool,
    },
    /// Enter a scope without a representation in this schema.
    UnknownScope,
    /// Leave the most recent open scope.
    ExitScope,
    /// Read a field from the bitstream.
    Signaled {
        /// Canonical AV2 field name.
        name: String,
        /// AV2 syntax descriptor, such as `f(5)`.
        descriptor: String,
        /// Raw value from the bitstream.
        value: SyntaxValue,
        /// Codec evidence translated to document coordinates.
        span: Option<SyntaxSpan>,
    },
    /// Skip a conditional field.
    Absent {
        /// Canonical AV2 field name.
        name: String,
        /// Condition that controls the field.
        condition: String,
        /// Value inferred by the specification, if it supplies one.
        inferred: Option<SyntaxValue>,
    },
    /// An event without a representation in this schema.
    UnknownEvent,
}

/// Evidence available for the OBU payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SyntaxStatus {
    /// The codec parsed the modeled payload syntax.
    Parsed,
    /// The codec parsed a prefix. The remainder needs additional state.
    Prefix {
        /// State required to continue.
        blocked_on: String,
        /// Codec feature that owns the remaining syntax.
        feature: String,
    },
    /// The payload is opaque, for example in a reserved OBU.
    Opaque,
    /// The codec does not implement this feature.
    Unsupported {
        /// Codec feature that owns the unsupported syntax.
        feature: String,
    },
    /// The OBU extension flag is set. The extension has no decoded syntax.
    UnknownExtension,
    /// The payload exceeds the syntax inspection byte budget.
    ResourceLimited,
    /// Parsing failed. The earlier observations remain available.
    Malformed,
}

/// First parser error for the OBU.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyntaxError {
    /// Codec message.
    pub message: String,
    /// Field at the error, when the parser names it.
    pub field: Option<String>,
    /// Error cursor, which may be at the end of the OBU.
    pub position: Option<BitPosition>,
}

/// Coverage of one syntax observation, independent of paging.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyntaxSummary {
    /// Syntax evidence schema version.
    pub schema_version: u16,
    /// AV2 specification revision used by the parser.
    pub spec_revision: String,
    /// Payload outcome.
    pub status: SyntaxStatus,
    /// First parser error, if parsing failed.
    pub error: Option<SyntaxError>,
    /// Number of retained events, including scope exits.
    pub retained_events: u32,
    /// Number of observed elements, including ones omitted by the limits.
    pub observed_elements: u64,
    /// The recording reached its element limit.
    pub elements_truncated: bool,
    /// The recording reached its nesting limit.
    pub depth_truncated: bool,
}

/// A bounded trace of one OBU.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyntaxEvidence {
    /// Coverage and outcome.
    pub summary: SyntaxSummary,
    /// Observations in parse order.
    pub events: Vec<SyntaxEvent>,
}
