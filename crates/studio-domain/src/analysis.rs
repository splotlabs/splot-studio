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
//! Outside IVF payloads they cover every byte of the file exactly once. A
//! packet spans its record header and payload; the OBUs and unparsed
//! regions after it, up to the next packet, cover its payload exactly once,
//! nested inside it. A packet is not an OBU and not a frame; an OBU names
//! the record index of the packet that carries it.
//!
//! Every [`Span`] says where its numbers came from ([`Provenance`]), and
//! every layer id whether the file signals it or the specification infers
//! it.
//!
//! A page lists the elements a [`RowFilter`] admits, as [`Row`]s that carry
//! each element's index and its parent packet. A locate finds the innermost
//! element at a file byte, or an element by index, and its position among
//! the rows a filter admits.

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

/// One element of a page, with its place in the document.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Row {
    /// The element's index in the document: its identity there.
    pub index: u64,
    /// The index of the packet element whose payload holds it: set for the
    /// OBUs and unparsed regions inside an IVF payload, `None` otherwise.
    pub parent: Option<u64>,
    /// The element.
    pub element: Element,
}

/// Which elements a page lists and a locate counts. The default admits
/// every element; a layer filter admits only OBUs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RowFilter {
    /// Which kinds of element.
    pub kind: KindFilter,
    /// Only OBUs with these layer ids.
    pub layer: Option<LayerFilter>,
}

/// The kind part of a [`RowFilter`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KindFilter {
    /// Every kind.
    #[default]
    All,
    /// Only regions that did not parse.
    Unparsed,
    /// Only OBUs of this raw `obu_type`.
    Obu {
        /// The raw `obu_type`.
        obu_type: u8,
    },
}

/// The layer part of a [`RowFilter`]: one `obu_tlayer_id` and
/// `obu_mlayer_id` pair, signaled or inferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LayerFilter {
    /// `obu_tlayer_id`.
    pub temporal: u8,
    /// `obu_mlayer_id`.
    pub embedded: u8,
}

impl RowFilter {
    /// Whether it admits every element.
    #[must_use]
    pub const fn is_all(&self) -> bool {
        matches!(self.kind, KindFilter::All) && self.layer.is_none()
    }

    /// Whether it admits an OBU with this raw type and these layer ids.
    #[must_use]
    pub const fn admits_obu(&self, obu_type: u8, temporal: u8, embedded: u8) -> bool {
        let kind = match self.kind {
            KindFilter::All => true,
            KindFilter::Unparsed => false,
            KindFilter::Obu { obu_type: wanted } => wanted == obu_type,
        };
        let layer = match self.layer {
            None => true,
            Some(layer) => layer.temporal == temporal && layer.embedded == embedded,
        };
        kind && layer
    }

    /// Whether it admits a region that did not parse.
    #[must_use]
    pub const fn admits_unparsed(&self) -> bool {
        matches!(self.kind, KindFilter::All | KindFilter::Unparsed) && self.layer.is_none()
    }

    /// Whether it admits the IVF header or a packet.
    #[must_use]
    pub const fn admits_wrapper(&self) -> bool {
        self.is_all()
    }

    /// Whether it admits `element`.
    #[must_use]
    pub const fn admits(&self, element: &Element) -> bool {
        match element {
            Element::IvfHeader { .. } | Element::Packet { .. } => self.admits_wrapper(),
            Element::Obu { obu } => self.admits_obu(
                obu.obu_type,
                obu.temporal_layer_id.value,
                obu.embedded_layer_id.value,
            ),
            Element::Unparsed { .. } => self.admits_unparsed(),
        }
    }
}

/// Where to locate an element.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LocateTarget {
    /// The innermost element that holds this file byte: an OBU or unparsed
    /// region inside an IVF payload before its packet.
    Offset {
        /// The byte.
        offset: u64,
    },
    /// The element with this index.
    Element {
        /// The index.
        index: u64,
    },
}

/// How many OBUs of one type a document has.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ObuTypeCount {
    /// The raw `obu_type`.
    pub obu_type: u8,
    /// The specification's name for the type, or `Reserved`.
    pub type_name: String,
    /// The number of OBUs.
    pub count: u64,
}

/// How many OBUs of one layer pair a document has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LayerCount {
    /// `obu_tlayer_id`.
    pub temporal: u8,
    /// `obu_mlayer_id`.
    pub embedded: u8,
    /// The number of OBUs.
    pub count: u64,
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
    /// The number of unparsed regions among them.
    pub unparsed: u64,
    /// The OBU types present, by raw type: at most 32, one per `obu_type`
    /// value.
    pub obu_types: Vec<ObuTypeCount>,
    /// The layer pairs present, by `obu_tlayer_id` then `obu_mlayer_id`: at
    /// most 32.
    pub layers: Vec<LayerCount>,
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
        let filter = RowFilter {
            kind: KindFilter::Obu { obu_type: 7 },
            layer: Some(LayerFilter {
                temporal: 1,
                embedded: 2,
            }),
        };
        let text = serde_json::to_string(&filter)?;
        assert_eq!(
            text,
            r#"{"kind":{"type":"obu","obu_type":7},"layer":{"temporal":1,"embedded":2}}"#
        );
        assert_eq!(serde_json::from_str::<RowFilter>(&text)?, filter);
        Ok(())
    }

    fn obu(obu_type: u8, temporal: u8, embedded: u8) -> Element {
        let span = Span {
            offset: 0,
            len: 1,
            provenance: Provenance::Located,
        };
        let layer = |value| LayerId {
            value,
            provenance: Provenance::Signaled,
        };
        Element::Obu {
            obu: ObuFact {
                index: 0,
                packet: None,
                obu_type,
                type_name: String::new(),
                reserved: false,
                has_extension: true,
                temporal_layer_id: layer(temporal),
                embedded_layer_id: layer(embedded),
                extended_layer_id: layer(0),
                length_prefix: span,
                obu: span,
                header: span,
                payload: span,
                stream: span,
            },
        }
    }

    #[test]
    fn filters_admit_by_kind_and_layer_pair() {
        let header = Element::IvfHeader {
            span: Span {
                offset: 0,
                len: 32,
                provenance: Provenance::Signaled,
            },
        };
        let unparsed = Element::Unparsed {
            span: Span {
                offset: 0,
                len: 1,
                provenance: Provenance::Derived,
            },
            kind: ProblemKind::Malformed,
            message: String::new(),
        };
        let all = RowFilter::default();
        assert!(all.is_all());
        assert!(
            [&header, &unparsed, &obu(7, 1, 2)]
                .iter()
                .all(|e| all.admits(e))
        );

        let tiles = RowFilter {
            kind: KindFilter::Obu { obu_type: 7 },
            layer: None,
        };
        assert!(tiles.admits(&obu(7, 3, 0)) && !tiles.admits(&obu(1, 3, 0)));
        assert!(!tiles.admits(&header) && !tiles.admits(&unparsed));

        let damage = RowFilter {
            kind: KindFilter::Unparsed,
            layer: None,
        };
        assert!(damage.admits(&unparsed) && !damage.admits(&obu(7, 0, 0)));
        assert!(!damage.admits(&header));

        // A layer pair admits OBUs of exactly that pair, and nothing else.
        let layer = RowFilter {
            kind: KindFilter::All,
            layer: Some(LayerFilter {
                temporal: 1,
                embedded: 2,
            }),
        };
        assert!(layer.admits(&obu(7, 1, 2)) && layer.admits(&obu(1, 1, 2)));
        assert!(!layer.admits(&obu(7, 2, 1)) && !layer.admits(&obu(7, 1, 0)));
        assert!(!layer.admits(&unparsed) && !layer.admits(&header));
        // Both parts must hold.
        let both = RowFilter {
            kind: KindFilter::Obu { obu_type: 7 },
            layer: layer.layer,
        };
        assert!(both.admits(&obu(7, 1, 2)) && !both.admits(&obu(1, 1, 2)));
    }
}
