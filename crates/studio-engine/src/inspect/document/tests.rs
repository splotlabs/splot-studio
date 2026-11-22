// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Hand-built fixtures with every offset worked out by hand from the AV2
//! OBU header layout (`obu_header_extension_flag`, five bits of `obu_type`,
//! two of `obu_tlayer_id`; the extension byte holds three bits of
//! `obu_mlayer_id` and five of `obu_xlayer_id`) and the IVF layout (a
//! 32-byte file header, then a 12-byte header before each payload).
//! Payloads are stand-in bytes: only envelopes and headers are read.

use std::cell::Cell;
use std::io::Write as _;

use studio_domain::protocol::{Envelope, Event, write_message};

use super::*;

fn never() -> bool {
    false
}

fn scanned(bytes: &[u8]) -> Result<Scanned, Refusal> {
    scan(bytes, bytes.len() as u64, &never)
}

fn elements(scanned: &Scanned) -> Vec<Element> {
    scanned.items.iter().map(Item::element).collect()
}

/// TD; a sequence header with a 3-byte stand-in payload; a regular tile
/// group with the extension (tlayer 1, mlayer 2, xlayer 5); reserved type 27
/// with tlayer 3; padding with a 130-byte payload behind a 2-byte length.
fn annex_b() -> Vec<u8> {
    let mut bytes = vec![
        0x01, 0x08, // TD
        0x04, 0x04, 0xAA, 0xBB, 0xCC, // sequence header
        0x04, 0x9D, 0x45, 0x11, 0x22, // tile group, extended
        0x01, 0x6F, // reserved 27
        0x83, 0x01, 0x64, // padding, 131 bytes
    ];
    bytes.extend([0x5A; 130]);
    bytes
}

const TD_SH: [u8; 7] = [0x01, 0x08, 0x04, 0x04, 0xAA, 0xBB, 0xCC];
const TD_TG: [u8; 7] = [0x01, 0x08, 0x04, 0x9D, 0x45, 0x11, 0x22];

fn ivf_header(fourcc: [u8; 4], header_len: u16) -> Vec<u8> {
    let mut bytes = b"DKIF".to_vec();
    bytes.extend(0_u16.to_le_bytes());
    bytes.extend(header_len.to_le_bytes());
    bytes.extend(fourcc);
    bytes.extend(16_u16.to_le_bytes());
    bytes.extend(16_u16.to_le_bytes());
    bytes.extend(30_u32.to_le_bytes());
    bytes.extend(1_u32.to_le_bytes());
    bytes.extend(3_u32.to_le_bytes());
    bytes.extend(0_u32.to_le_bytes());
    bytes.resize(usize::from(header_len), 0);
    bytes
}

fn record(pts: u64, payload: &[u8]) -> Vec<u8> {
    let mut bytes = u32::try_from(payload.len())
        .unwrap_or(0)
        .to_le_bytes()
        .to_vec();
    bytes.extend(pts.to_le_bytes());
    bytes.extend(payload);
    bytes
}

/// Three records: TD and sequence header, an empty payload, TD and the
/// extended tile group. 82 bytes.
fn ivf() -> Vec<u8> {
    let mut bytes = ivf_header(*b"AV02", 32);
    bytes.extend(record(0, &TD_SH));
    bytes.extend(record(1, &[]));
    bytes.extend(record(2, &TD_TG));
    bytes
}

fn leb128(mut value: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            bytes.push(byte);
            return bytes;
        }
        bytes.push(byte | 0x80);
    }
}

/// The plain values of an OBU fact: type, name, reserved, the three layer
/// ids, the provenance of mlayer and xlayer, and the prefix, OBU, header,
/// payload and stream spans.
type Row = (u8, String, bool, [u8; 3], [Provenance; 2], [(u64, u64); 5]);

fn row(obu: &ObuFact) -> Row {
    let at = |span: Span| (span.offset, span.len);
    (
        obu.obu_type,
        obu.type_name.clone(),
        obu.reserved,
        [
            obu.temporal_layer_id.value,
            obu.embedded_layer_id.value,
            obu.extended_layer_id.value,
        ],
        [
            obu.embedded_layer_id.provenance,
            obu.extended_layer_id.provenance,
        ],
        [
            at(obu.length_prefix),
            at(obu.obu),
            at(obu.header),
            at(obu.payload),
            at(obu.stream),
        ],
    )
}

fn obus(elements: &[Element]) -> Vec<&ObuFact> {
    elements
        .iter()
        .filter_map(|e| match e {
            Element::Obu { obu } => Some(obu),
            _ => None,
        })
        .collect()
}

/// The file bytes an element covers.
fn extent(element: &Element) -> (u64, u64) {
    let end = |span: Span| span.offset + span.len;
    match element {
        Element::IvfHeader { span } | Element::Unparsed { span, .. } => (span.offset, end(*span)),
        Element::Packet { packet } => {
            assert_eq!(end(packet.header), packet.payload.offset);
            (packet.header.offset, end(packet.payload))
        }
        Element::Obu { obu } => {
            assert_eq!(end(obu.length_prefix), obu.obu.offset);
            assert_eq!(obu.header.offset, obu.obu.offset);
            assert_eq!(end(obu.header), obu.payload.offset);
            assert_eq!(end(obu.payload), end(obu.obu));
            (obu.length_prefix.offset, end(obu.obu))
        }
    }
}

/// Every byte of the file is in exactly one element, in order. OBUs sit
/// inside the payload of the packet before them.
fn assert_tiles(elements: &[Element], len: u64) {
    let mut at = 0;
    let mut payload_end = None;
    for element in elements {
        let (start, end) = extent(element);
        match element {
            Element::Packet { packet } => {
                assert_eq!(start, at, "{element:?}");
                at = packet.payload.offset;
                payload_end = Some(end);
                if packet.payload.len == 0 {
                    payload_end = None;
                }
                continue;
            }
            _ => assert_eq!(start, at, "{element:?}"),
        }
        at = end;
        if payload_end == Some(end) {
            payload_end = None;
        }
    }
    assert_eq!(payload_end, None, "a payload is not covered");
    assert_eq!(at, len);
}

#[test]
fn annex_b_facts_and_spans() -> Result<(), Refusal> {
    use Provenance::{Inferred, Signaled};
    let bytes = annex_b();
    assert_eq!(bytes.len(), 147);
    let scanned = scanned(&bytes)?;
    assert_eq!(scanned.framing, Framing::AnnexB);
    assert_eq!(scanned.outcome, ScanOutcome::Complete);
    assert!(scanned.header.is_none());
    let elements = elements(&scanned);
    assert_tiles(&elements, 147);
    let rows: Vec<Row> = obus(&elements).into_iter().map(row).collect();
    let name = |n: &str| n.to_owned();
    assert_eq!(
        rows,
        [
            (
                2,
                name("OBU_TEMPORAL_DELIMITER"),
                false,
                [0, 0, 31],
                [Inferred; 2],
                [(0, 1), (1, 1), (1, 1), (2, 0), (0, 2)]
            ),
            (
                1,
                name("OBU_SEQUENCE_HEADER"),
                false,
                [0, 0, 0],
                [Inferred; 2],
                [(2, 1), (3, 4), (3, 1), (4, 3), (2, 5)]
            ),
            (
                7,
                name("OBU_REGULAR_TILE_GROUP"),
                false,
                [1, 2, 5],
                [Signaled; 2],
                [(7, 1), (8, 4), (8, 2), (10, 2), (7, 5)]
            ),
            (
                27,
                name("Reserved"),
                true,
                [3, 0, 0],
                [Inferred; 2],
                [(12, 1), (13, 1), (13, 1), (14, 0), (12, 2)]
            ),
            (
                25,
                name("OBU_PADDING"),
                false,
                [0, 0, 0],
                [Inferred; 2],
                [(14, 2), (16, 131), (16, 1), (17, 130), (14, 133)]
            ),
        ]
    );
    let first = obus(&elements)[0].clone();
    assert_eq!((first.index, first.packet), (0, None));
    assert_eq!(first.temporal_layer_id.provenance, Signaled);
    assert_eq!(
        [first.length_prefix, first.obu, first.payload, first.stream].map(|s| s.provenance),
        [
            Provenance::Derived,
            Provenance::Located,
            Provenance::Located,
            Provenance::Derived
        ]
    );
    assert_eq!(
        obus(&elements).iter().map(|o| o.index).collect::<Vec<_>>(),
        [0, 1, 2, 3, 4]
    );
    Ok(())
}

#[test]
fn ivf_records_are_packets_and_their_obus_map_to_the_stream() -> Result<(), Refusal> {
    let bytes = ivf();
    assert_eq!(bytes.len(), 82);
    let scanned = scanned(&bytes)?;
    assert_eq!(scanned.framing, Framing::Ivf);
    assert_eq!(scanned.outcome, ScanOutcome::Complete);
    let summary = scanned.summary(SourceFingerprint {
        len: 82,
        modified: None,
    });
    assert_eq!(
        summary.ivf_header,
        Some(IvfHeaderFact {
            version: 0,
            header_len: 32,
            fourcc: *b"AV02",
            width: 16,
            height: 16,
            timebase_numerator: 1,
            timebase_denominator: 30,
            frame_count: 3,
        })
    );
    assert_eq!((summary.elements, summary.packets, summary.obus), (8, 3, 4));
    let elements = self::elements(&scanned);
    assert_tiles(&elements, 82);
    assert_eq!(
        elements[0],
        Element::IvfHeader {
            span: Span {
                offset: 0,
                len: 32,
                provenance: Provenance::Signaled
            }
        }
    );
    let packets: Vec<_> = elements
        .iter()
        .filter_map(|e| match e {
            Element::Packet { packet } => Some((
                packet.index,
                packet.pts,
                (packet.header.offset, packet.header.len),
                (packet.payload.offset, packet.payload.len),
                (packet.stream.offset, packet.stream.len),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        packets,
        [
            (0, 0, (32, 12), (44, 7), (0, 7)),
            (1, 1, (51, 12), (63, 0), (7, 0)),
            (2, 2, (63, 12), (75, 7), (7, 7)),
        ]
    );
    let found: Vec<_> = obus(&elements)
        .iter()
        .map(|o| (o.index, o.packet, row(o).5))
        .collect();
    assert_eq!(
        found,
        [
            (0, Some(0), [(44, 1), (45, 1), (45, 1), (46, 0), (0, 2)]),
            (1, Some(0), [(46, 1), (47, 4), (47, 1), (48, 3), (2, 5)]),
            (2, Some(2), [(75, 1), (76, 1), (76, 1), (77, 0), (7, 2)]),
            (3, Some(2), [(77, 1), (78, 4), (78, 2), (80, 2), (9, 5)]),
        ]
    );
    // The payloads one after another are an Annex B stream; its OBUs are
    // where the IVF document says they are in the stream.
    let stream: Vec<u8> = [TD_SH, TD_TG].concat();
    let plain = self::elements(&self::scanned(&stream)?);
    let in_stream: Vec<_> = obus(&elements).iter().map(|o| row(o).5[4]).collect();
    let in_file: Vec<_> = obus(&plain).iter().map(|o| row(o).5[4]).collect();
    assert_eq!(in_stream, in_file);
    Ok(())
}

#[test]
fn framing_comes_from_contents_and_the_two_framings_cannot_overlap() {
    // An Annex B stream must open with a temporal delimiter, and `DKIF`
    // read as Annex B is a 68-byte OBU of type 18: never both.
    let dkif = AnnexBObuCursor::new(b"DKIF\0\0\0\0", ByteOffset::new(0)).next_obu();
    assert!(
        !matches!(dkif, Ok(Some(ref obu)) if obu.header.obu_type == ObuType::TemporalDelimiter),
        "{dkif:?}"
    );
    let unsupported = |bytes: &[u8]| {
        matches!(
            scanned(bytes),
            Err(Refusal {
                code: AnalysisFailure::Unsupported,
                ..
            })
        )
    };
    assert!(unsupported(&[]));
    assert!(unsupported(b"YUV4MPEG2 W16 H16 F30:1 C420\nFRAME\n"));
    // A sequence header first: framed like Annex B, but not evidence of it.
    assert!(unsupported(&annex_b()[2..]));
    assert!(matches!(scanned(&[]), Err(Refusal { ref message, .. }) if message.contains("empty")));
}

#[test]
fn an_ivf_of_another_codec_keeps_its_header_and_reads_nothing_else() -> Result<(), Refusal> {
    let mut bytes = ivf_header(*b"AV01", 32);
    bytes.extend(record(0, &TD_SH));
    let scanned = scanned(&bytes)?;
    assert_eq!(scanned.header.map(|h| h.fourcc), Some(*b"AV01"));
    let elements = elements(&scanned);
    assert_tiles(&elements, 51);
    assert!(matches!(
        elements.as_slice(),
        [
            Element::IvfHeader { .. },
            Element::Unparsed {
                span: Span {
                    offset: 32,
                    len: 19,
                    ..
                },
                kind: ProblemKind::Unsupported,
                ..
            }
        ]
    ));
    assert!(matches!(
        scanned.outcome,
        ScanOutcome::Stopped {
            problem: Problem {
                kind: ProblemKind::Unsupported,
                offset: 32,
                ..
            }
        }
    ));
    Ok(())
}

/// The kind, offset and length of the final region, and the outcome's.
fn stopped(scanned: &Scanned) -> Option<(ProblemKind, u64, u64)> {
    let Some(Item::Unparsed {
        offset, len, kind, ..
    }) = scanned.items.last()
    else {
        return None;
    };
    match &scanned.outcome {
        ScanOutcome::Stopped { problem } if problem.kind == *kind && problem.offset == *offset => {
            Some((*kind, *offset, *len))
        }
        _ => None,
    }
}

#[test]
fn a_broken_ivf_header_is_one_malformed_region() -> Result<(), Refusal> {
    let mut short = ivf();
    short[6..8].copy_from_slice(&16_u16.to_le_bytes());
    for bytes in [b"DKIF\0\0".to_vec(), short, ivf()[..31].to_vec()] {
        let scanned = scanned(&bytes)?;
        assert_eq!(scanned.framing, Framing::Ivf);
        assert!(scanned.header.is_none());
        assert_eq!(scanned.items.len(), 1);
        assert_eq!(
            stopped(&scanned),
            Some((ProblemKind::Malformed, 0, bytes.len() as u64)),
            "{:?}",
            scanned.outcome
        );
    }
    Ok(())
}

#[test]
fn truncated_ivf_keeps_every_whole_record_before_the_cut() -> Result<(), Refusal> {
    // Inside the last payload: that record is not read.
    let cut = &ivf()[..80];
    let scanned = scanned(cut)?;
    let elements = elements(&scanned);
    assert_tiles(&elements, 80);
    assert_eq!(elements.len(), 6);
    assert_eq!(stopped(&scanned), Some((ProblemKind::Malformed, 63, 17)));
    assert!(
        matches!(&scanned.outcome, ScanOutcome::Stopped { problem } if problem.message.contains("ivf/truncated-frame-payload"))
    );
    // Inside a record header after the last record.
    let mut bytes = ivf();
    bytes.extend([1, 2, 3, 4, 5]);
    let scanned = self::scanned(&bytes)?;
    assert_eq!(scanned.items.len(), 9);
    assert_eq!(stopped(&scanned), Some((ProblemKind::Malformed, 82, 5)));
    assert!(
        matches!(&scanned.outcome, ScanOutcome::Stopped { problem } if problem.message.contains("ivf/trailing-partial-frame-header"))
    );
    Ok(())
}

#[test]
fn a_bad_obu_in_one_payload_leaves_the_next_records_readable() -> Result<(), Refusal> {
    let mut bytes = ivf_header(*b"AV02", 32);
    // TD, then an OBU that declares 5 bytes and has 1.
    bytes.extend(record(0, &[0x01, 0x08, 0x05, 0x04]));
    bytes.extend(record(1, &TD_TG));
    let scanned = scanned(&bytes)?;
    let elements = elements(&scanned);
    assert_tiles(&elements, bytes.len() as u64);
    assert!(
        matches!(
            elements.as_slice(),
            [
                Element::IvfHeader { .. },
                Element::Packet { .. },
                Element::Obu { .. },
                Element::Unparsed {
                    span: Span {
                        offset: 46,
                        len: 2,
                        ..
                    },
                    kind: ProblemKind::Malformed,
                    ..
                },
                Element::Packet { .. },
                Element::Obu { .. },
                Element::Obu { .. },
            ]
        ),
        "{elements:?}"
    );
    assert!(matches!(
        scanned.outcome,
        ScanOutcome::Damaged {
            first: Problem {
                kind: ProblemKind::Malformed,
                offset: 46,
                ..
            }
        }
    ));
    Ok(())
}

#[test]
fn annex_b_stops_at_a_bad_or_truncated_length() -> Result<(), Refusal> {
    let bytes = annex_b();
    let mut cases = vec![
        // Inside the padding payload, then inside its two-byte length.
        (bytes[..100].to_vec(), 14, 86),
        (bytes[..15].to_vec(), 14, 1),
        // Inside the sequence header.
        (bytes[..5].to_vec(), 2, 3),
    ];
    let mut bad = bytes[..2].to_vec();
    bad.extend([0x80; 8]);
    cases.push((bad, 2, 8));
    let mut empty_obu = bytes[..2].to_vec();
    empty_obu.push(0x00);
    cases.push((empty_obu, 2, 1));
    for (bytes, offset, len) in cases {
        let scanned = scanned(&bytes)?;
        assert_tiles(&elements(&scanned), bytes.len() as u64);
        assert_eq!(
            stopped(&scanned),
            Some((ProblemKind::Malformed, offset, len)),
            "{bytes:02x?} {:?}",
            scanned.outcome
        );
    }
    Ok(())
}

#[test]
fn a_declared_unit_over_the_cap_is_a_limit_and_at_the_cap_is_not() -> Result<(), Refusal> {
    let cap = MAX_UNIT_BYTES as u64;
    for (declared, kind) in [
        (cap + 1, ProblemKind::LimitReached),
        (cap, ProblemKind::Malformed),
    ] {
        let mut bytes = vec![0x01, 0x08];
        bytes.extend(leb128(declared));
        bytes.push(0x04);
        let scanned = scanned(&bytes)?;
        assert_eq!(stopped(&scanned), Some((kind, 2, bytes.len() as u64 - 2)));
        let mut bytes = ivf_header(*b"AV02", 32);
        bytes.extend(u32::try_from(declared).unwrap_or(u32::MAX).to_le_bytes());
        bytes.extend(0_u64.to_le_bytes());
        bytes.extend([0x01, 0x08]);
        let scanned = self::scanned(&bytes)?;
        assert_eq!(
            stopped(&scanned),
            Some((kind, 32, 14)),
            "{:?}",
            scanned.outcome
        );
    }
    Ok(())
}

#[test]
fn the_element_limit_is_exact() -> Result<(), Refusal> {
    let full = [0x01, 0x08].repeat(MAX_RETAINED_ELEMENTS);
    let scanned = scanned(&full)?;
    assert_eq!(scanned.outcome, ScanOutcome::Complete);
    assert_eq!(scanned.items.len(), MAX_RETAINED_ELEMENTS);
    let over = [0x01, 0x08].repeat(MAX_RETAINED_ELEMENTS + 1);
    let scanned = self::scanned(&over)?;
    let end = 2 * MAX_RETAINED_ELEMENTS as u64;
    assert_eq!(stopped(&scanned), Some((ProblemKind::LimitReached, end, 2)));
    assert_eq!(scanned.items.len(), MAX_RETAINED_ELEMENTS + 1);
    Ok(())
}

/// Serves `bytes`, failing once `fail_at` bytes were read, and raises
/// `trip` after its first read.
struct Source<'a> {
    bytes: &'a [u8],
    at: usize,
    fail_at: usize,
    trip: Option<&'a Cell<bool>>,
}

impl Read for Source<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(trip) = self.trip {
            trip.set(true);
        }
        if self.at >= self.fail_at {
            return Err(io::Error::other("disk on fire"));
        }
        let n = buf
            .len()
            .min(self.fail_at - self.at)
            .min(self.bytes.len() - self.at);
        buf[..n].copy_from_slice(&self.bytes[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

#[test]
fn a_read_failure_keeps_what_was_read_and_cancelling_keeps_nothing() -> Result<(), Refusal> {
    let bytes = annex_b();
    let len = bytes.len() as u64;
    let source = |fail_at, trip| Source {
        bytes: &bytes,
        at: 0,
        fail_at,
        trip,
    };
    let scanned = scan(source(12, None), len, &never)?;
    assert_eq!(obus(&elements(&scanned)).len(), 3);
    assert_eq!(stopped(&scanned), Some((ProblemKind::Io, 12, 135)));
    let first = scan(source(0, None), len, &never);
    assert!(
        matches!(
            first,
            Err(Refusal {
                code: AnalysisFailure::Io,
                ..
            })
        ),
        "{first:?}"
    );
    let trip = Cell::new(false);
    let cancelled = scan(source(usize::MAX, Some(&trip)), len, &|| trip.get());
    assert!(
        matches!(
            cancelled,
            Err(Refusal {
                code: AnalysisFailure::Cancelled,
                ..
            })
        ),
        "{cancelled:?}"
    );
    Ok(())
}

#[test]
fn spans_past_four_gibibytes_are_exact() -> Result<(), Refusal> {
    let len = 5 << 30;
    let mut bytes = vec![0x01, 0x08];
    bytes.extend([0x80; 8]);
    let scanned = scan(bytes.as_slice(), len, &never)?;
    assert_eq!(
        stopped(&scanned),
        Some((ProblemKind::Malformed, 2, len - 2))
    );
    let page = page(&scanned.items, 1, 1)?;
    assert!(
        matches!(page.as_slice(), [Element::Unparsed { span: Span { len: l, .. }, .. }] if *l == len - 2)
    );
    Ok(())
}

fn fits(event: Event) -> bool {
    write_message(&mut Vec::new(), &Envelope::new(u64::MAX, event)).is_ok()
}

#[test]
fn pages_are_bounded_by_count_range_and_message_size() -> Result<(), Refusal> {
    let invalid = |got: Result<Vec<Element>, Refusal>| {
        matches!(
            got,
            Err(Refusal {
                code: AnalysisFailure::InvalidRequest,
                ..
            })
        )
    };
    let items = scanned(&annex_b())?.items;
    assert!(invalid(page(&items, 0, 0)));
    assert!(invalid(page(&items, 0, MAX_PAGE_ELEMENTS + 1)));
    assert!(invalid(page(&items, 5, 1)));
    assert!(invalid(page(&items, u64::MAX, 1)));
    assert_eq!(page(&items, 4, MAX_PAGE_ELEMENTS)?.len(), 1);
    assert_eq!(page(&items, 0, 2)?.len(), 2);
    // Messages that JSON escapes to twice their size.
    let wide: Vec<Item> = (0..300)
        .map(|i| Item::Unparsed {
            offset: u64::MAX - 1000 + i,
            len: 1,
            kind: ProblemKind::Malformed,
            message: "\"".repeat(MAX_PROBLEM_MESSAGE),
        })
        .collect();
    let first = page(&wide, 0, MAX_PAGE_ELEMENTS)?;
    assert!(
        !first.is_empty() && first.len() < MAX_PAGE_ELEMENTS as usize,
        "{}",
        first.len()
    );
    let next = first.len() as u64;
    assert!(fits(Event::AnalysisPage {
        document: u64::MAX,
        first: u64::MAX,
        elements: first
    }));
    let second = page(&wide, next, MAX_PAGE_ELEMENTS)?;
    assert!(
        matches!(second.first(), Some(Element::Unparsed { span, .. }) if span.offset == u64::MAX - 1000 + next)
    );
    // A full page of the largest OBU facts.
    let header = AnnexBObuCursor::new(&[0x02, 0x9D, 0x45], ByteOffset::new(0))
        .next_obu()
        .ok()
        .flatten()
        .map(|obu| obu.header);
    let Some(header) = header else {
        unreachable!("the tile group header parses");
    };
    let big: Vec<Item> = (0..u64::from(MAX_PAGE_ELEMENTS))
        .map(|i| Item::Obu {
            index: u64::MAX - i,
            packet: Some(u64::MAX),
            prefix: u64::MAX - 10,
            offset: u64::MAX - 8,
            size: u32::MAX,
            header,
            stream_offset: u64::MAX - 10,
        })
        .collect();
    let full = page(&big, 0, MAX_PAGE_ELEMENTS)?;
    assert!(fits(Event::AnalysisPage {
        document: u64::MAX,
        first: 0,
        elements: full
    }));
    Ok(())
}

/// A file of `len` bytes whose byte at `n` is `n % 251`, read lazily.
struct Pattern {
    len: u64,
    at: u64,
}

impl Read for Pattern {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = buf
            .len()
            .min(usize::try_from(self.len - self.at).unwrap_or(usize::MAX));
        for (i, byte) in buf[..n].iter_mut().enumerate() {
            *byte = ((self.at + i as u64) % 251) as u8;
        }
        self.at += n as u64;
        Ok(n)
    }
}

impl Seek for Pattern {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let SeekFrom::Start(at) = to else {
            return Err(io::Error::other("only from the start"));
        };
        self.at = at.min(self.len);
        Ok(self.at)
    }
}

#[test]
fn byte_ranges_are_exact_bounded_and_inside_the_file() -> Result<(), Refusal> {
    let len = (5 << 30) + 7;
    let mut file = Pattern { len, at: 0 };
    let invalid = |got: Result<Vec<u8>, Refusal>| {
        matches!(
            got,
            Err(Refusal {
                code: AnalysisFailure::InvalidRequest,
                ..
            })
        )
    };
    assert!(invalid(read_range(&mut file, len, 0, 0)));
    assert!(invalid(read_range(&mut file, len, 0, MAX_BYTE_RANGE + 1)));
    assert!(invalid(read_range(&mut file, len, u64::MAX, 1)));
    assert!(invalid(read_range(&mut file, len, len - 3, 4)));
    let offset = len - u64::from(MAX_BYTE_RANGE);
    let bytes = read_range(&mut file, len, offset, MAX_BYTE_RANGE)?;
    assert_eq!(bytes.len(), MAX_BYTE_RANGE as usize);
    assert!(
        bytes
            .iter()
            .zip(offset..)
            .all(|(b, at)| u64::from(*b) == at % 251)
    );
    let full = vec![0xFF; MAX_BYTE_RANGE as usize];
    assert!(fits(Event::AnalysisBytes {
        document: u64::MAX,
        offset: u64::MAX,
        bytes: full
    }));
    Ok(())
}

#[test]
fn a_changed_or_replaced_source_is_refused() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("stream.ivf");
    std::fs::write(&path, annex_b())?;
    let source = WirePath::from_path(&path);
    let mut document = Document::open(&source, &never).map_err(|r| r.message)?;
    assert_eq!(document.summary().framing, Framing::AnnexB);
    assert_eq!(
        document.bytes(14, 3).map_err(|r| r.message)?,
        [0x83, 0x01, 0x64]
    );
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)?
        .write_all(&[0x01, 0x08])?;
    let changed = |got: Result<(), Refusal>| {
        matches!(
            got,
            Err(Refusal {
                code: AnalysisFailure::SourceChanged,
                ..
            })
        )
    };
    assert!(changed(document.page(0, 1).map(drop)));
    assert!(changed(document.bytes(0, 1).map(drop)));

    let document = Document::open(&source, &never).map_err(|r| r.message)?;
    let other = dir.path().join("other");
    std::fs::write(&other, annex_b())?;
    std::fs::rename(&other, &path)?;
    assert!(changed(document.page(0, 1).map(drop)));

    let directory = Document::open(&WirePath::from_path(dir.path()), &never);
    assert!(
        matches!(
            directory,
            Err(Refusal {
                code: AnalysisFailure::Unsupported,
                ..
            })
        ),
        "{directory:?}"
    );
    let missing = Document::open(&WirePath::from_path(&dir.path().join("gone")), &never);
    assert!(
        matches!(
            missing,
            Err(Refusal {
                code: AnalysisFailure::Io,
                ..
            })
        ),
        "{missing:?}"
    );
    Ok(())
}

/// The memory bound documented on [`MAX_RETAINED_ELEMENTS`].
#[test]
fn a_kept_element_takes_at_most_64_bytes() {
    assert!(size_of::<Item>() <= 64, "{}", size_of::<Item>());
}

/// Serves `bytes` and raises `trip` on read number `trip_at`.
struct Tripping<'a> {
    bytes: &'a [u8],
    at: usize,
    reads: usize,
    trip_at: usize,
    trip: &'a Cell<bool>,
}

impl Read for Tripping<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reads += 1;
        if self.reads == self.trip_at {
            self.trip.set(true);
        }
        let n = buf.len().min(self.bytes.len() - self.at);
        buf[..n].copy_from_slice(&self.bytes[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// A cancel during the read of one large unit stops inside it: the next
/// read fails, and the failure is reported as the cancel, not as I/O.
#[test]
fn a_cancel_inside_one_large_unit_stops_its_read() {
    let mut bytes = vec![0x01, 0x08, 0x81, 0x80, 0x40, 0x64];
    bytes.resize(bytes.len() + (1 << 20), 0);
    let trip = Cell::new(false);
    let cancelled = || trip.get();
    let mut source = Tripping {
        bytes: &bytes,
        at: 0,
        reads: 0,
        trip_at: 4,
        trip: &trip,
    };
    let checked = Checked {
        inner: &mut source,
        cancelled: &cancelled,
    };
    let got = scan(checked, bytes.len() as u64, &cancelled);
    assert!(
        matches!(
            got,
            Err(Refusal {
                code: AnalysisFailure::Cancelled,
                ..
            })
        ),
        "{got:?}"
    );
    // Stopped inside the padding OBU, long before its end.
    assert!(source.at < bytes.len() / 2, "{}", source.at);
}

/// The file changes while the open reads it: the open is refused.
#[test]
fn a_source_that_changes_during_the_open_is_refused() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("stream.obu");
    std::fs::write(&path, annex_b())?;
    let touched = Cell::new(false);
    let change = || {
        if !touched.replace(true) {
            let appended = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .and_then(|mut file| file.write_all(&[0x01, 0x08]));
            assert!(appended.is_ok(), "{appended:?}");
        }
        false
    };
    let got = Document::open(&WirePath::from_path(&path), &change);
    assert!(touched.get());
    assert!(
        matches!(
            got,
            Err(Refusal {
                code: AnalysisFailure::SourceChanged,
                ..
            })
        ),
        "{got:?}"
    );
    Ok(())
}

#[test]
fn the_ivf_element_limit_counts_the_header() -> Result<(), Refusal> {
    let records = |n: usize| {
        let mut bytes = ivf_header(*b"AV02", 32);
        for pts in 0..n as u64 {
            bytes.extend(record(pts, &[]));
        }
        bytes
    };
    let full = records(MAX_RETAINED_ELEMENTS - 1);
    let scanned = scanned(&full)?;
    assert_eq!(scanned.outcome, ScanOutcome::Complete);
    assert_eq!(scanned.items.len(), MAX_RETAINED_ELEMENTS);
    let over = records(MAX_RETAINED_ELEMENTS);
    let scanned = self::scanned(&over)?;
    assert_eq!(scanned.items.len(), MAX_RETAINED_ELEMENTS + 1);
    assert_eq!(
        stopped(&scanned),
        Some((
            ProblemKind::LimitReached,
            over.len() as u64 - IVF_FRAME_HEADER_SIZE as u64,
            IVF_FRAME_HEADER_SIZE as u64
        ))
    );
    Ok(())
}

/// The messages of regions inside payloads stay within their budget; the
/// regions beyond it are still reported, with a fixed text.
#[test]
fn region_messages_stay_within_their_budget() -> Result<(), Refusal> {
    let mut bytes = ivf_header(*b"AV02", 32);
    for pts in 0..2000 {
        // An OBU that declares 5 bytes and has 1.
        bytes.extend(record(pts, &[0x05, 0x04]));
    }
    let scanned = scanned(&bytes)?;
    assert!(matches!(scanned.outcome, ScanOutcome::Damaged { .. }));
    let messages: Vec<&String> = scanned
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Unparsed { message, .. } => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(messages.len(), 2000);
    let kept: usize = messages.iter().map(|m| m.capacity()).sum();
    assert!(kept <= MAX_KEPT_MESSAGE_BYTES, "{kept}");
    assert!(messages[0].contains("IVF record 0"));
    let last = scanned.items.last().map(Item::element);
    assert!(
        matches!(last, Some(Element::Unparsed { ref message, .. }) if message == MESSAGE_NOT_KEPT),
        "{last:?}"
    );
    Ok(())
}

/// Opening a FIFO would wait for a writer forever; it is refused first.
#[cfg(unix)]
#[test]
fn a_fifo_is_refused_without_waiting() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("pipe.obu");
    let made = std::process::Command::new("mkfifo").arg(&path).status()?;
    assert!(made.success());
    let (tx, rx) = std::sync::mpsc::channel();
    let source = WirePath::from_path(&path);
    std::thread::spawn(move || {
        let _ = tx.send(Document::open(&source, &never).map(drop));
    });
    let got = rx.recv_timeout(std::time::Duration::from_secs(10))?;
    assert!(
        matches!(
            got,
            Err(Refusal {
                code: AnalysisFailure::Unsupported,
                ..
            })
        ),
        "{got:?}"
    );
    Ok(())
}
