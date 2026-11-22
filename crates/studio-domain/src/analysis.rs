// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Read-only analysis documents: the facts the engine's inspect process reads
//! from a local AV2 IVF or Annex B file, and the limits on asking for them.
//!
//! A document is one opened file, named by a `document` id the app chooses
//! (ids increase within one inspect process). Its facts are [`Element`]s in
//! file order, numbered from zero and fetched a page at a time: the IVF file
//! header, IVF frame records ([`PacketFact`], wrapper packets), OBUs
//! ([`ObuFact`]) and regions that did not parse ([`Element::Unparsed`]).
//! Together they cover every byte of the file exactly once. A packet is not
//! an OBU and not a frame; an OBU names the packet that carries it.
//!
//! Every [`Span`] says where its numbers came from ([`Provenance`]), and
//! every layer id whether the file signals it or the specification infers
//! it.

use serde::{Deserialize, Serialize};

use crate::job::SourceFingerprint;

/// The most elements one page request may ask for. A page holds fewer when
/// the file ends first or when they would not fit in one protocol message.
pub const MAX_PAGE_ELEMENTS: u32 = 256;

/// The most bytes one byte-range request may ask for. Bytes travel as a JSON
/// array, at most four characters a byte, so a full range stays well inside
/// [`crate::protocol::MAX_MESSAGE_BYTES`].
pub const MAX_BYTE_RANGE: u32 = 8 * 1024;

/// How the file wraps its OBUs, identified from its contents, never from its
/// name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Framing {
    /// An IVF file: it starts with the `DKIF` signature. Each frame record
    /// carries a payload of length-delimited OBUs.
    Ivf,
    /// An AV2 Annex B length-delimited OBU stream: not IVF, and its first
    /// OBU is a temporal delimiter.
    AnnexB,
}

/// Where a value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// A field in the file states it.
    Signaled,
    /// The file does not carry it; the specification infers this value when
    /// the field is absent.
    Inferred,
    /// A position and length that Splot's reader or parser reported.
    Located,
    /// Computed by Studio from values Splot reported.
    Derived,
}

/// A run of bytes: in the file, or in the elementary stream (the IVF
/// payloads one after another, or the whole file for Annex B).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Span {
    /// The first byte.
    pub offset: u64,
    /// The number of bytes.
    pub len: u64,
    /// Where `offset` and `len` came from.
    pub provenance: Provenance,
}

/// An OBU header layer id and where it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LayerId {
    /// The id.
    pub value: u8,
    /// [`Provenance::Signaled`] or [`Provenance::Inferred`].
    pub provenance: Provenance,
}

/// The IVF file header, as signaled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IvfHeaderFact {
    /// The version field.
    pub version: u16,
    /// The header length in bytes.
    pub header_len: u16,
    /// The codec four-character code; `AV02` for AV2.
    pub fourcc: [u8; 4],
    /// The coded width in pixels.
    pub width: u16,
    /// The coded height in pixels.
    pub height: u16,
    /// The time base numerator.
    pub timebase_numerator: u32,
    /// The time base denominator.
    pub timebase_denominator: u32,
    /// The frame count the header declares. Writers may leave it zero; it is
    /// not a count of the records.
    pub frame_count: u32,
}

/// One IVF frame record: a wrapper packet, not an OBU.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PacketFact {
    /// Zero-based record index in file order.
    pub index: u64,
    /// The record's presentation timestamp, in time base units.
    pub pts: u64,
    /// The 12-byte record header in the file.
    pub header: Span,
    /// The payload in the file. It may be empty.
    pub payload: Span,
    /// The payload in the elementary stream.
    pub stream: Span,
}

/// One OBU's envelope and header.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ObuFact {
    /// Zero-based OBU index in file order: the OBU's identity in its document.
    pub index: u64,
    /// The index of the IVF record that carries it; `None` for Annex B.
    pub packet: Option<u64>,
    /// The raw `obu_type`.
    pub obu_type: u8,
    /// The specification's name for the type, or `Reserved`.
    pub type_name: String,
    /// The type is reserved. Reserved OBUs are kept, not treated as errors.
    pub reserved: bool,
    /// `obu_header_extension_flag`.
    pub has_extension: bool,
    /// `obu_tlayer_id`.
    pub temporal_layer_id: LayerId,
    /// `obu_mlayer_id`.
    pub embedded_layer_id: LayerId,
    /// `obu_xlayer_id`.
    pub extended_layer_id: LayerId,
    /// The leb128 length that precedes the OBU in the file.
    pub length_prefix: Span,
    /// The OBU in the file: header and payload, `num_bytes_in_obu` long.
    pub obu: Span,
    /// The OBU header in the file.
    pub header: Span,
    /// The OBU payload in the file. It may be empty.
    pub payload: Span,
    /// The length prefix and the OBU in the elementary stream.
    pub stream: Span,
}

/// What kind of problem stopped or interrupted reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProblemKind {
    /// The bytes break the framing: a truncated or invalid length or header.
    Malformed,
    /// The bytes are framed but are not something this engine reads, such
    /// as an IVF file for another codec.
    Unsupported,
    /// Reading the file failed.
    Io,
    /// A limit of this engine was reached: a declared unit larger than it
    /// reads, or more elements than it keeps.
    LimitReached,
}

/// A problem and the first byte it left unparsed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Problem {
    /// What went wrong.
    pub kind: ProblemKind,
    /// The first byte of the region it left unparsed.
    pub offset: u64,
    /// A human-readable explanation, with Splot's own diagnostic.
    pub message: String,
}

/// One fact of a document, in file order.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Element {
    /// The IVF file header; its fields are in [`DocumentSummary::ivf_header`].
    IvfHeader {
        /// The header in the file.
        span: Span,
    },
    /// An IVF frame record.
    Packet {
        /// The record.
        packet: PacketFact,
    },
    /// An OBU.
    Obu {
        /// The OBU.
        obu: ObuFact,
    },
    /// Bytes that did not parse. Nothing in them is interpreted.
    Unparsed {
        /// The bytes in the file.
        span: Span,
        /// Why they did not parse.
        kind: ProblemKind,
        /// A human-readable explanation.
        message: String,
    },
}

/// How far reading the file got.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScanOutcome {
    /// Every byte belongs to a parsed element.
    Complete,
    /// The end of the file was reached, but some regions did not parse.
    Damaged {
        /// The first of them.
        first: Problem,
    },
    /// Reading stopped early; the rest of the file is one unparsed region,
    /// the last element.
    Stopped {
        /// Why.
        problem: Problem,
    },
}

/// What opening a document found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentSummary {
    /// The file that was read. Every later request is refused once the file
    /// no longer matches it.
    pub fingerprint: SourceFingerprint,
    /// How the file wraps its OBUs.
    pub framing: Framing,
    /// The IVF file header, when one was read.
    pub ivf_header: Option<IvfHeaderFact>,
    /// The number of elements; pages index into `0..elements`.
    pub elements: u64,
    /// The number of IVF frame records among them.
    pub packets: u64,
    /// The number of OBUs among them.
    pub obus: u64,
    /// How far reading got.
    pub outcome: ScanOutcome,
}

/// Why an analysis request has no answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisFailure {
    /// The request is outside the protocol's limits or the document's range.
    InvalidRequest,
    /// No open document has this id: it was never opened in this process,
    /// or it was closed, cancelled or dropped after a source change.
    UnknownDocument,
    /// The file is neither IVF nor an AV2 Annex B stream.
    Unsupported,
    /// The file changed or was replaced since it was opened. The document
    /// is closed; open the file again.
    SourceChanged,
    /// The file could not be read.
    Io,
    /// The open was cancelled.
    Cancelled,
    /// Too many documents are open, or too many requests are waiting.
    LimitReached,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire names are the protocol: a renamed variant breaks the peer.
    #[test]
    fn wire_names_are_snake_case_and_tagged() -> Result<(), serde_json::Error> {
        let element = Element::Unparsed {
            span: Span {
                offset: u64::MAX,
                len: 1,
                provenance: Provenance::Derived,
            },
            kind: ProblemKind::LimitReached,
            message: "m".into(),
        };
        let text = serde_json::to_string(&element)?;
        assert_eq!(
            text,
            r#"{"type":"unparsed","span":{"offset":18446744073709551615,"len":1,"provenance":"derived"},"kind":"limit_reached","message":"m"}"#
        );
        assert_eq!(serde_json::from_str::<Element>(&text)?, element);
        assert_eq!(serde_json::to_string(&Framing::AnnexB)?, r#""annex_b""#);
        Ok(())
    }
}
