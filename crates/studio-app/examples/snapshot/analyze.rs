// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Analyze scenarios: the app's real `Analysis`, answered with the facts
//! the inspect engine reports for two hand-built fixtures of the engine
//! tests (`ivf()` and a truncated `annex_b()` in
//! `studio-engine/src/inspect/document/tests.rs`), and their bytes.
//!
//! `analyze-empty`, `analyze-opening`, `analyze-ivf` (the extended tile
//! group selected), `analyze-filtered` (temporal delimiters only: the
//! selection is hidden), `analyze-damaged` (a truncated Annex B stream, its
//! unparsed region selected) and `analyze-failed` (not an AV2 stream).

use std::path::PathBuf;

use studio_app::analysis::{Analysis, Effect, Input};
use studio_domain::analysis::{
    AnalysisFailure, DocumentSummary, Element, Framing, IvfHeaderFact, KindFilter, LayerCount,
    LayerId, LocateTarget, ObuFact, ObuTypeCount, PacketFact, Problem, ProblemKind, Provenance,
    Row, ScanOutcome, Span,
};
use studio_domain::job::SourceFingerprint;
use studio_domain::protocol::{Command, Event};

/// A document's facts and bytes, as the engine would serve them.
struct Fixture {
    summary: DocumentSummary,
    rows: Vec<Row>,
    bytes: Vec<u8>,
}

fn span(offset: u64, len: u64, provenance: Provenance) -> Span {
    Span {
        offset,
        len,
        provenance,
    }
}

/// An OBU fact: `[prefix, obu, header, payload, stream]` spans as
/// `(offset, len)`, and the three layer ids, signaled when `extension`.
fn obu(
    index: u64,
    packet: Option<u64>,
    (obu_type, name): (u8, &str),
    extension: bool,
    layers: [u8; 3],
    spans: [(u64, u64); 5],
) -> Element {
    use Provenance::{Derived, Inferred, Located, Signaled};
    let ext = if extension { Signaled } else { Inferred };
    let [prefix, whole, header, payload, stream] = spans;
    Element::Obu {
        obu: ObuFact {
            index,
            packet,
            obu_type,
            type_name: name.into(),
            reserved: false,
            has_extension: extension,
            temporal_layer_id: LayerId {
                value: layers[0],
                provenance: Signaled,
            },
            embedded_layer_id: LayerId {
                value: layers[1],
                provenance: ext,
            },
            extended_layer_id: LayerId {
                value: layers[2],
                provenance: ext,
            },
            length_prefix: span(prefix.0, prefix.1, Derived),
            obu: span(whole.0, whole.1, Located),
            header: span(header.0, header.1, Located),
            payload: span(payload.0, payload.1, Located),
            stream: span(stream.0, stream.1, Derived),
        },
    }
}

fn packet(index: u64, at: u64, len: u64, stream: u64) -> Element {
    Element::Packet {
        packet: PacketFact {
            index,
            pts: index,
            header: span(at, 12, Provenance::Derived),
            payload: span(at + 12, len, Provenance::Located),
            stream: span(stream, len, Provenance::Derived),
        },
    }
}

const TD: (u8, &str) = (2, "OBU_TEMPORAL_DELIMITER");
const SH: (u8, &str) = (1, "OBU_SEQUENCE_HEADER");
const TG: (u8, &str) = (7, "OBU_REGULAR_TILE_GROUP");

/// Three records: TD and sequence header, an empty payload, TD and the
/// extended tile group. 82 bytes.
fn ivf() -> Fixture {
    let mut bytes = b"DKIF".to_vec();
    bytes.extend(0_u16.to_le_bytes());
    bytes.extend(32_u16.to_le_bytes());
    bytes.extend(b"AV02");
    bytes.extend(16_u16.to_le_bytes());
    bytes.extend(16_u16.to_le_bytes());
    bytes.extend(30_u32.to_le_bytes());
    bytes.extend(1_u32.to_le_bytes());
    bytes.extend(3_u32.to_le_bytes());
    bytes.extend(0_u32.to_le_bytes());
    for (pts, payload) in [
        (0_u64, &[0x01, 0x08, 0x04, 0x04, 0xAA, 0xBB, 0xCC][..]),
        (1, &[]),
        (2, &[0x01, 0x08, 0x04, 0x9D, 0x45, 0x11, 0x22]),
    ] {
        bytes.extend(u32::try_from(payload.len()).unwrap_or(0).to_le_bytes());
        bytes.extend(pts.to_le_bytes());
        bytes.extend(payload);
    }
    let elements = vec![
        Element::IvfHeader {
            span: span(0, 32, Provenance::Signaled),
        },
        packet(0, 32, 7, 0),
        obu(
            0,
            Some(0),
            TD,
            false,
            [0, 0, 31],
            [(44, 1), (45, 1), (45, 1), (46, 0), (0, 2)],
        ),
        obu(
            1,
            Some(0),
            SH,
            false,
            [0, 0, 0],
            [(46, 1), (47, 4), (47, 1), (48, 3), (2, 5)],
        ),
        packet(1, 51, 0, 7),
        packet(2, 63, 7, 7),
        obu(
            2,
            Some(2),
            TD,
            false,
            [0, 0, 31],
            [(75, 1), (76, 1), (76, 1), (77, 0), (7, 2)],
        ),
        obu(
            3,
            Some(2),
            TG,
            true,
            [1, 2, 5],
            [(77, 1), (78, 4), (78, 2), (80, 2), (9, 5)],
        ),
    ];
    let parents = [None, None, Some(1), Some(1), None, None, Some(5), Some(5)];
    Fixture {
        summary: DocumentSummary {
            fingerprint: SourceFingerprint {
                len: 82,
                modified: None,
            },
            framing: Framing::Ivf,
            ivf_header: Some(IvfHeaderFact {
                version: 0,
                header_len: 32,
                fourcc: *b"AV02",
                width: 16,
                height: 16,
                timebase_numerator: 1,
                timebase_denominator: 30,
                frame_count: 3,
            }),
            elements: 8,
            packets: 3,
            obus: 4,
            unparsed: 0,
            obu_types: vec![count(SH, 1), count(TD, 2), count(TG, 1)],
            layers: vec![layer(0, 0, 3), layer(1, 2, 1)],
            outcome: ScanOutcome::Complete,
        },
        rows: rows(elements, &parents),
        bytes,
    }
}

/// TD, sequence header, then a padding OBU that declares 131 bytes and has
/// two: reading stops there. 11 bytes.
fn damaged() -> Fixture {
    let message = "the OBU does not parse: OBU payload out of range at byte 9: size 131 exceeds 2 \
                   remaining byte(s)";
    let elements = vec![
        obu(
            0,
            None,
            TD,
            false,
            [0, 0, 31],
            [(0, 1), (1, 1), (1, 1), (2, 0), (0, 2)],
        ),
        obu(
            1,
            None,
            SH,
            false,
            [0, 0, 0],
            [(2, 1), (3, 4), (3, 1), (4, 3), (2, 5)],
        ),
        Element::Unparsed {
            span: span(7, 4, Provenance::Derived),
            kind: ProblemKind::Malformed,
            message: message.into(),
        },
    ];
    Fixture {
        summary: DocumentSummary {
            fingerprint: SourceFingerprint {
                len: 11,
                modified: None,
            },
            framing: Framing::AnnexB,
            ivf_header: None,
            elements: 3,
            packets: 0,
            obus: 2,
            unparsed: 1,
            obu_types: vec![count(SH, 1), count(TD, 1)],
            layers: vec![layer(0, 0, 2)],
            outcome: ScanOutcome::Stopped {
                problem: Problem {
                    kind: ProblemKind::Malformed,
                    offset: 7,
                    message: message.into(),
                },
            },
        },
        rows: rows(elements, &[None, None, None]),
        bytes: vec![
            0x01, 0x08, 0x04, 0x04, 0xAA, 0xBB, 0xCC, 0x83, 0x01, 0x64, 0x5A,
        ],
    }
}

fn count((obu_type, name): (u8, &str), count: u64) -> ObuTypeCount {
    ObuTypeCount {
        obu_type,
        type_name: name.into(),
        count,
    }
}

const fn layer(temporal: u8, embedded: u8, count: u64) -> LayerCount {
    LayerCount {
        temporal,
        embedded,
        count,
    }
}

fn rows(elements: Vec<Element>, parents: &[Option<u64>]) -> Vec<Row> {
    (0..)
        .zip(elements)
        .zip(parents)
        .map(|((index, element), parent)| Row {
            index,
            parent: *parent,
            element,
        })
        .collect()
}

impl Fixture {
    /// Answers `effects` and everything they lead to.
    fn settle(&self, analysis: &mut Analysis, mut effects: Vec<Effect>) {
        while !effects.is_empty() {
            let mut next = Vec::new();
            for effect in effects {
                if let Some(event) = self.answer(&effect.command) {
                    next.extend(analysis.event(effect.request_id, event));
                }
            }
            effects = next;
        }
    }

    fn answer(&self, command: &Command) -> Option<Event> {
        Some(match command.clone() {
            Command::AnalysisOpen { document, .. } => Event::AnalysisOpened {
                document,
                summary: self.summary.clone(),
            },
            Command::AnalysisPage {
                document,
                filter,
                first,
                count,
            } => {
                let admitted: Vec<&Row> = self
                    .rows
                    .iter()
                    .filter(|r| filter.admits(&r.element))
                    .collect();
                Event::AnalysisPage {
                    document,
                    first,
                    total: admitted.len() as u64,
                    rows: admitted
                        .into_iter()
                        .skip(usize::try_from(first).ok()?)
                        .take(count as usize)
                        .cloned()
                        .collect(),
                }
            }
            Command::AnalysisLocate {
                document,
                filter,
                target,
            } => {
                let found = match target {
                    LocateTarget::Element { index } => {
                        self.rows.get(usize::try_from(index).ok()?)?
                    }
                    // The last element that starts at or before the byte.
                    LocateTarget::Offset { offset } => self
                        .rows
                        .iter()
                        .rev()
                        .find(|r| studio_app::analysis::extent(&r.element).0 <= offset)?,
                };
                let row = filter.admits(&found.element).then(|| {
                    self.rows
                        .iter()
                        .take_while(|r| r.index != found.index)
                        .filter(|r| filter.admits(&r.element))
                        .count() as u64
                });
                Event::AnalysisLocated {
                    document,
                    row,
                    found: Box::new(found.clone()),
                }
            }
            Command::AnalysisBytes {
                document,
                offset,
                len,
            } => {
                let start = usize::try_from(offset).ok()?;
                Event::AnalysisBytes {
                    document,
                    offset,
                    bytes: self.bytes.get(start..start + len as usize)?.to_vec(),
                }
            }
            _ => return None,
        })
    }
}

/// The Analyze state of `name`, or `None` when it is no Analyze scenario.
pub fn scenario(name: &str) -> Option<Analysis> {
    let mut analysis = Analysis::new();
    let open = |analysis: &mut Analysis, file: &str| {
        analysis.input(Input::Open(vec![PathBuf::from(format!(
            "/home/user/Videos/{file}"
        ))]))
    };
    match name {
        "analyze-empty" => {}
        "analyze-opening" => {
            open(&mut analysis, "park walk.ivf");
        }
        "analyze-ivf" | "analyze-filtered" => {
            let fixture = ivf();
            let effects = open(&mut analysis, "park walk.ivf");
            fixture.settle(&mut analysis, effects);
            let effects = analysis.input(Input::RowClicked(7));
            fixture.settle(&mut analysis, effects);
            if name == "analyze-filtered" {
                let td = analysis
                    .type_choices()
                    .iter()
                    .position(|k| *k == KindFilter::Obu { obu_type: 2 })
                    .and_then(|i| i32::try_from(i).ok())?;
                let effects = analysis.input(Input::TypeFilter(td));
                fixture.settle(&mut analysis, effects);
            }
        }
        "analyze-damaged" => {
            let fixture = damaged();
            let effects = open(&mut analysis, "cut short.obu");
            fixture.settle(&mut analysis, effects);
            let effects = analysis.input(Input::RowClicked(2));
            fixture.settle(&mut analysis, effects);
        }
        "analyze-failed" => {
            let effects = open(&mut analysis, "notes.txt");
            for effect in effects {
                analysis.event(
                    effect.request_id,
                    Event::AnalysisFailed {
                        document: 1,
                        code: AnalysisFailure::Unsupported,
                        message: "neither an IVF file nor an AV2 Annex B stream that starts with \
                                  a temporal delimiter OBU"
                            .into(),
                    },
                );
            }
        }
        _ => return None,
    }
    Some(analysis)
}
