// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The Analyze state machine against scripted engine answers: one
//! selection per document, Back and Forward, filters, paging, stale answers
//! and a process that dies.

use studio_domain::analysis::{
    DocumentSummary, Element, Framing, LayerCount, LayerId, ObuFact, ObuTypeCount, PacketFact,
    ProblemKind, Provenance, ScanOutcome, Span,
};
use studio_domain::job::SourceFingerprint;
use studio_domain::protocol::ErrorCode;

use super::*;

fn span(offset: u64, len: u64) -> Span {
    Span {
        offset,
        len,
        provenance: Provenance::Located,
    }
}

/// An Annex B OBU at `prefix` with a one-byte length and a one-byte header.
fn obu(index: u64, prefix: u64, len: u64, obu_type: u8, temporal: u8) -> Row {
    let layer = |value| LayerId {
        value,
        provenance: Provenance::Signaled,
    };
    Row {
        index,
        parent: None,
        element: Element::Obu {
            obu: ObuFact {
                index,
                packet: None,
                obu_type,
                type_name: format!("TYPE_{obu_type}"),
                reserved: false,
                has_extension: false,
                temporal_layer_id: layer(temporal),
                embedded_layer_id: layer(0),
                extended_layer_id: layer(0),
                length_prefix: span(prefix, 1),
                obu: span(prefix + 1, len),
                header: span(prefix + 1, 1),
                payload: span(prefix + 2, len - 1),
                stream: span(prefix, len + 1),
            },
        },
    }
}

/// `n` OBUs of 9 bytes each (a 1-byte prefix and 8 bytes), alternately of
/// type 2 and 7.
fn stream(n: u64) -> Vec<Row> {
    (0..n)
        .map(|i| obu(i, i * 9, 8, if i % 2 == 0 { 2 } else { 7 }, 0))
        .collect()
}

fn summary(elements: u64) -> DocumentSummary {
    DocumentSummary {
        fingerprint: SourceFingerprint {
            len: elements * 9,
            modified: Some((1_800_000_000, 0)),
        },
        framing: Framing::AnnexB,
        ivf_header: None,
        elements,
        packets: 0,
        obus: elements,
        unparsed: 0,
        obu_types: vec![
            ObuTypeCount {
                obu_type: 2,
                type_name: "TYPE_2".into(),
                count: elements.div_ceil(2),
            },
            ObuTypeCount {
                obu_type: 7,
                type_name: "TYPE_7".into(),
                count: elements / 2,
            },
        ],
        layers: vec![LayerCount {
            temporal: 0,
            embedded: 0,
            count: elements,
            inferred: 0,
        }],
        outcome: ScanOutcome::Complete,
    }
}

/// The engine's filter rule for a whole element.
fn admits(filter: RowFilter, element: &Element) -> bool {
    match element {
        Element::IvfHeader { .. } | Element::Packet { .. } => filter.is_all(),
        Element::Obu { obu } => filter.admits_obu(
            obu.obu_type,
            obu.temporal_layer_id.value,
            obu.embedded_layer_id.value,
        ),
        Element::Unparsed { .. } => filter.admits_unparsed(),
    }
}

/// Plays the engine for one document of `rows`.
struct Engine {
    rows: Vec<Row>,
}

impl Engine {
    /// Answers every effect the way the inspect process would.
    fn answer(&self, analysis: &mut Analysis, effects: Vec<Effect>) -> Vec<Effect> {
        let mut next = Vec::new();
        for effect in effects {
            if let Some(event) = self.event(&effect.command) {
                next.extend(analysis.event(effect.request_id, event));
            }
        }
        next
    }

    /// Answers until nothing more is asked.
    fn settle(&self, analysis: &mut Analysis, mut effects: Vec<Effect>) {
        while !effects.is_empty() {
            effects = self.answer(analysis, effects);
        }
    }

    fn admitted(&self, filter: RowFilter) -> Vec<&Row> {
        self.rows
            .iter()
            .filter(|r| admits(filter, &r.element))
            .collect()
    }

    fn event(&self, command: &Command) -> Option<Event> {
        let len = self.rows.len() as u64 * 9;
        Some(match command.clone() {
            Command::AnalysisOpen { document, .. } => Event::AnalysisOpened {
                document,
                summary: summary(self.rows.len() as u64),
            },
            Command::AnalysisPage {
                document,
                filter,
                first,
                count,
            } => {
                let admitted = self.admitted(filter);
                Event::AnalysisPage {
                    document,
                    first,
                    total: admitted.len() as u64,
                    rows: admitted
                        .into_iter()
                        .skip(usize::try_from(first).unwrap_or(usize::MAX))
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
                let index = match target {
                    LocateTarget::Offset { offset } => offset / 9,
                    LocateTarget::Element { index } => index,
                };
                let found = self.rows.get(usize::try_from(index).ok()?)?.clone();
                let row = admits(filter, &found.element).then(|| {
                    self.admitted(filter)
                        .iter()
                        .position(|r| r.index == index)
                        .unwrap_or(0) as u64
                });
                Event::AnalysisLocated {
                    document,
                    row,
                    found: Box::new(found),
                }
            }
            Command::AnalysisBytes {
                document,
                offset,
                len: count,
            } => {
                assert!(offset + u64::from(count) <= len, "{offset} + {count}");
                Event::AnalysisBytes {
                    document,
                    offset,
                    bytes: (offset..offset + u64::from(count))
                        .map(|b| (b % 251) as u8)
                        .collect(),
                }
            }
            Command::AnalysisClose { document } => Event::AnalysisClosed { document },
            _ => return None,
        })
    }
}

/// An analysis with one open document of `n` OBUs, everything answered.
fn opened(n: u64) -> (Analysis, Engine) {
    let engine = Engine { rows: stream(n) };
    let mut analysis = Analysis::new();
    let effects = analysis.input(Input::Open(vec![PathBuf::from("/media/clip.obu")]));
    engine.settle(&mut analysis, effects);
    (analysis, engine)
}

fn doc(analysis: &Analysis) -> &Document {
    analysis
        .shown()
        .unwrap_or_else(|| unreachable!("a document is shown"))
}

fn selected(analysis: &Analysis) -> Option<(u64, Position)> {
    doc(analysis).selected().map(|s| (s.row.index, s.position))
}

fn commands(effects: &[Effect]) -> Vec<&Command> {
    effects.iter().map(|e| &e.command).collect()
}

#[test]
fn opening_asks_for_the_first_page_and_byte_window() {
    let mut analysis = Analysis::new();
    let effects = analysis.input(Input::Open(vec![PathBuf::from("/media/clip.obu")]));
    assert!(matches!(
        commands(&effects)[..],
        [Command::AnalysisOpen { document: 1, .. }]
    ));
    assert_eq!(*doc(&analysis).phase(), Phase::Opening);
    let engine = Engine { rows: stream(40) };
    let next = engine.answer(&mut analysis, effects);
    assert!(
        matches!(
            commands(&next)[..],
            [
                Command::AnalysisPage {
                    document: 1,
                    first: 0,
                    count: PAGE_ROWS,
                    ..
                },
                Command::AnalysisBytes {
                    document: 1,
                    offset: 0,
                    len: 128,
                },
            ]
        ),
        "{next:?}"
    );
    engine.settle(&mut analysis, next);
    let doc = doc(&analysis);
    assert_eq!(doc.total(), Some(40));
    assert_eq!(doc.row(13).map(|r| r.index), Some(13));
    assert_eq!(
        doc.bytes().data.as_ref().map(Vec::len),
        usize::try_from(WINDOW_BYTES).ok()
    );
    // Nothing is selected until the user selects.
    assert!(doc.selected().is_none());
}

/// A row and a byte inside the same element are the same selection: the
/// second changes nothing and adds no history.
#[test]
fn a_row_and_a_byte_of_it_resolve_to_one_selection() {
    let (mut analysis, engine) = opened(40);
    let effects = analysis.input(Input::RowClicked(3));
    engine.settle(&mut analysis, effects);
    assert_eq!(selected(&analysis), Some((3, Position::Row(3))));
    // OBU 3 starts at byte 27, inside the first window.
    assert_eq!(doc(&analysis).bytes().cursor, 27);
    assert!(!doc(&analysis).can_go_back());

    // Byte 30 is inside OBU 3.
    let effects = analysis.input(Input::ByteClicked(30));
    assert!(matches!(
        commands(&effects)[..],
        [Command::AnalysisLocate {
            target: LocateTarget::Offset { offset: 30 },
            ..
        }]
    ));
    engine.settle(&mut analysis, effects);
    assert_eq!(selected(&analysis), Some((3, Position::Row(3))));
    assert!(
        !doc(&analysis).can_go_back(),
        "a repeated selection is no history"
    );
    assert_eq!(doc(&analysis).bytes().cursor, 30);

    // Byte 50 is in OBU 5: selected, and the window stays.
    let effects = analysis.input(Input::ByteClicked(50));
    engine.settle(&mut analysis, effects);
    assert_eq!(selected(&analysis), Some((5, Position::Row(5))));
    assert_eq!(doc(&analysis).bytes().offset, 0);
    assert!(doc(&analysis).can_go_back());
}

/// Back and Forward restore the selection with the byte window and cursor
/// it was made in.
#[test]
fn back_and_forward_restore_the_selection_and_its_view() {
    let (mut analysis, engine) = opened(100);
    let effects = analysis.input(Input::RowClicked(1));
    engine.settle(&mut analysis, effects);
    // Row 40 is past the visible rows: End then a row near it.
    let effects = analysis.input(Input::Move(Step::End));
    engine.settle(&mut analysis, effects);
    assert_eq!(selected(&analysis), Some((99, Position::Row(99))));
    let far = doc(&analysis).bytes().offset;
    assert_eq!(far, 99 * 9 - (99 * 9) % 16);
    assert_eq!(doc(&analysis).top(), 100 - VISIBLE_ROWS);

    let effects = analysis.input(Input::Back);
    engine.settle(&mut analysis, effects);
    assert_eq!(selected(&analysis), Some((1, Position::Row(1))));
    assert_eq!(doc(&analysis).bytes().offset, 0);
    assert_eq!(doc(&analysis).bytes().cursor, 9);
    assert_eq!(
        doc(&analysis).top(),
        1,
        "the list shows the selection again"
    );
    assert!(doc(&analysis).bytes().data.is_some());
    assert!(!doc(&analysis).can_go_back());

    let effects = analysis.input(Input::Forward);
    engine.settle(&mut analysis, effects);
    assert_eq!(selected(&analysis), Some((99, Position::Row(99))));
    assert_eq!(doc(&analysis).bytes().offset, far);
    assert!(!doc(&analysis).can_go_forward());

    // A new selection after Back drops what was ahead.
    let effects = analysis.input(Input::Back);
    engine.settle(&mut analysis, effects);
    let effects = analysis.input(Input::Move(Step::Down));
    engine.settle(&mut analysis, effects);
    assert_eq!(selected(&analysis), Some((2, Position::Row(2))));
    assert!(!doc(&analysis).can_go_forward());
}

/// A filter that hides the selection keeps it selected and says so; one
/// that admits it finds its new row.
#[test]
fn a_filtered_out_selection_stays_selected() {
    let (mut analysis, engine) = opened(40);
    let effects = analysis.input(Input::RowClicked(4));
    engine.settle(&mut analysis, effects);
    assert_eq!(selected(&analysis), Some((4, Position::Row(4))));
    let choices = analysis.type_choices();
    let seven = choices
        .iter()
        .position(|k| *k == KindFilter::Obu { obu_type: 7 })
        .and_then(|i| i32::try_from(i).ok())
        .unwrap_or(-1);
    let effects = analysis.input(Input::TypeFilter(seven));
    // The selection's row is not known until the engine says.
    assert_eq!(selected(&analysis), Some((4, Position::Unknown)));
    engine.settle(&mut analysis, effects);
    assert_eq!(
        selected(&analysis),
        Some((4, Position::Hidden)),
        "hidden, kept"
    );
    assert_eq!(doc(&analysis).total(), Some(20));
    assert_eq!(doc(&analysis).row(0).map(|r| r.index), Some(1));

    // Type 2 admits OBU 4 at row 2.
    let two = choices
        .iter()
        .position(|k| *k == KindFilter::Obu { obu_type: 2 })
        .and_then(|i| i32::try_from(i).ok())
        .unwrap_or(-1);
    let effects = analysis.input(Input::TypeFilter(two));
    engine.settle(&mut analysis, effects);
    assert_eq!(selected(&analysis), Some((4, Position::Row(2))));
    let effects = analysis.input(Input::ClearFilters);
    engine.settle(&mut analysis, effects);
    assert_eq!(selected(&analysis), Some((4, Position::Row(4))));
    assert!(!doc(&analysis).can_go_back(), "filters are no selections");
}

/// A page asked for under one filter is dropped when it answers under
/// another; so is any answer whose request is unknown or names another
/// document.
#[test]
fn stale_answers_are_dropped() {
    let (mut analysis, engine) = opened(200);
    let effects = analysis.input(Input::Scroll(100));
    assert!(matches!(
        commands(&effects)[..],
        [Command::AnalysisPage { first: 100, .. }]
    ));
    // The filter changes before the page answers.
    let filter = analysis.input(Input::TypeFilter(1));
    let stale = engine.answer(&mut analysis, effects);
    assert_eq!(stale.len(), 0, "{stale:?}");
    assert_eq!(doc(&analysis).total(), None);
    assert_eq!(doc(&analysis).kept_rows(), 0);
    engine.settle(&mut analysis, filter);
    assert_eq!(doc(&analysis).total(), Some(100));

    // An unknown request id, and an answer for another document.
    let before = doc(&analysis).clone();
    let unknown = analysis.event(
        999,
        Event::AnalysisPage {
            document: 1,
            first: 0,
            total: 1,
            rows: vec![obu(0, 0, 8, 1, 0)],
        },
    );
    assert_eq!(unknown, []);
    let effects = analysis.input(Input::ByteClicked(0));
    let id = effects[0].request_id;
    analysis.event(
        id,
        Event::AnalysisLocated {
            document: 2,
            row: Some(0),
            found: Box::new(obu(0, 0, 8, 1, 0)),
        },
    );
    assert_eq!(doc(&analysis).selected(), before.selected());
    assert_eq!(doc(&analysis).total(), before.total());
}

/// Two documents keep their own selections and filters, and an answer for
/// one never changes the other.
#[test]
fn documents_do_not_share_selections_or_events() {
    let engine = Engine { rows: stream(40) };
    let mut analysis = Analysis::new();
    let effects = analysis.input(Input::Open(vec![
        PathBuf::from("/media/a.obu"),
        PathBuf::from("/media/b.obu"),
    ]));
    let ids: Vec<u64> = effects
        .iter()
        .filter_map(|e| match e.command {
            Command::AnalysisOpen { document, .. } => Some(document),
            _ => None,
        })
        .collect();
    assert_eq!(ids, [1, 2]);
    engine.settle(&mut analysis, effects);
    assert_eq!(analysis.shown_index(), 1);
    let effects = analysis.input(Input::RowClicked(7));
    engine.settle(&mut analysis, effects);
    let effects = analysis.input(Input::Show(0));
    engine.settle(&mut analysis, effects);
    assert!(selected(&analysis).is_none(), "a has no selection");
    let effects = analysis.input(Input::RowClicked(2));
    // b's request is still out when a's selection changes.
    let effects_b = {
        let mut b = analysis.input(Input::Show(1));
        b.extend(analysis.input(Input::ByteClicked(100)));
        b
    };
    engine.settle(&mut analysis, effects);
    engine.settle(&mut analysis, effects_b);
    assert_eq!(selected(&analysis), Some((11, Position::Row(11))));
    let effects = analysis.input(Input::Show(0));
    engine.settle(&mut analysis, effects);
    assert_eq!(selected(&analysis), Some((2, Position::Row(2))));
}

/// Only pages near the visible rows are asked for and kept, whatever the
/// row count; a selection survives the pages that arrive around it.
#[test]
fn large_documents_are_paged_and_bounded() {
    let (mut analysis, engine) = opened(20_000);
    assert_eq!(doc(&analysis).total(), Some(20_000));
    assert!(doc(&analysis).kept_rows() <= PAGE_ROWS as usize);
    let effects = analysis.input(Input::RowClicked(5));
    engine.settle(&mut analysis, effects);
    let mut asked = 0;
    for _ in 0..300 {
        let effects = analysis.input(Input::Scroll(40));
        asked += effects.len();
        engine.settle(&mut analysis, effects);
        assert!(doc(&analysis).kept_rows() as u64 <= KEPT_ROWS);
    }
    assert_eq!(doc(&analysis).top(), 12_000);
    assert!(asked <= 300, "{asked} page requests for 300 scrolls");
    assert_eq!(selected(&analysis), Some((5, Position::Row(5))));
    // Scrolling to the end shows the last rows; the share is exact.
    let effects = analysis.input(Input::ScrollTo(1.0));
    engine.settle(&mut analysis, effects);
    assert_eq!(doc(&analysis).top(), 20_000 - VISIBLE_ROWS);
    assert_eq!(doc(&analysis).row(19_999).map(|r| r.index), Some(19_999));
}

/// A keyboard move to a row that has not arrived selects it when its page
/// does.
#[test]
fn a_move_waits_for_its_row() {
    let (mut analysis, engine) = opened(20_000);
    let effects = analysis.input(Input::Move(Step::End));
    assert!(selected(&analysis).is_none());
    assert!(matches!(
        commands(&effects)[..],
        [Command::AnalysisPage { first, .. }] if *first == 20_000 - VISIBLE_ROWS
    ));
    engine.settle(&mut analysis, effects);
    assert_eq!(selected(&analysis), Some((19_999, Position::Row(19_999))));
    let effects = analysis.input(Input::Move(Step::PageUp));
    engine.settle(&mut analysis, effects);
    let up = 19_999 - (VISIBLE_ROWS - 1);
    assert_eq!(selected(&analysis), Some((up, Position::Row(up))));
}

#[test]
fn go_to_selects_the_element_at_an_offset_and_refuses_others() {
    let (mut analysis, engine) = opened(100);
    let effects = analysis.input(Input::GoTo("0x2D0".into()));
    engine.settle(&mut analysis, effects);
    // 0x2D0 = 720 = OBU 80.
    assert_eq!(selected(&analysis), Some((80, Position::Row(80))));
    assert_eq!(doc(&analysis).bytes().cursor, 720);
    assert_eq!(doc(&analysis).bytes().offset, 720);
    for bad in ["900", "-1", "0x", "twelve", ""] {
        let effects = analysis.input(Input::GoTo(bad.into()));
        assert_eq!(effects.len(), 0, "{bad}");
        assert!(
            doc(&analysis)
                .notice()
                .is_some_and(|n| n.contains("0 to 899")),
            "{bad}: {:?}",
            doc(&analysis).notice()
        );
        assert_eq!(selected(&analysis), Some((80, Position::Row(80))));
    }
}

#[test]
fn the_byte_cursor_moves_the_window_by_lines() {
    let (mut analysis, engine) = opened(100);
    let effects = analysis.input(Input::Cursor(Step::End));
    engine.settle(&mut analysis, effects);
    let bytes = doc(&analysis).bytes();
    assert_eq!(bytes.cursor, 899);
    assert_eq!(bytes.offset, 896 - (WINDOW_LINES - 1) * LINE_BYTES);
    assert_eq!(
        bytes.data.as_ref().map(Vec::len),
        usize::try_from(900 - bytes.offset).ok()
    );
    let effects = analysis.input(Input::Cursor(Step::Home));
    engine.settle(&mut analysis, effects);
    assert_eq!(
        (doc(&analysis).bytes().cursor, doc(&analysis).bytes().offset),
        (0, 0)
    );
    // Selecting at the cursor is the same as clicking that byte.
    let effects = analysis.input(Input::Cursor(Step::Down));
    engine.settle(&mut analysis, effects);
    let effects = analysis.input(Input::SelectAtCursor);
    engine.settle(&mut analysis, effects);
    assert_eq!(selected(&analysis).map(|s| s.0), Some(1));
}

/// A dead process fails every document; Reopen reads it again under a new,
/// larger id, and the old requests' answers are dropped.
#[test]
fn a_dead_process_fails_its_documents_until_reopened() {
    let (mut analysis, engine) = opened(200);
    let pending = analysis.input(Input::Scroll(100));
    assert_eq!(pending.len(), 1);
    analysis.process_ended("signal: 9");
    assert!(matches!(
        doc(&analysis).phase(),
        Phase::Failed { message, .. } if message.contains("signal: 9")
    ));
    assert_eq!(engine.answer(&mut analysis, pending).len(), 0);
    let effects = analysis.input(Input::Reopen);
    assert!(matches!(
        commands(&effects)[..],
        [Command::AnalysisOpen { document: 2, .. }]
    ));
    engine.settle(&mut analysis, effects);
    assert!(doc(&analysis).summary().is_some());
    assert_eq!(analysis.documents().len(), 1);
}

#[test]
fn a_changed_source_fails_the_document_and_other_failures_are_notices() {
    let (mut analysis, _engine) = opened(200);
    let effects = analysis.input(Input::ByteClicked(3));
    analysis.event(
        effects[0].request_id,
        Event::AnalysisFailed {
            document: 1,
            code: AnalysisFailure::LimitReached,
            message: "more than 64 requests are waiting".into(),
        },
    );
    assert!(doc(&analysis).summary().is_some());
    assert!(doc(&analysis).notice().is_some_and(|n| n.contains("64")));
    let effects = analysis.input(Input::Scroll(100));
    analysis.event(
        effects[0].request_id,
        Event::AnalysisFailed {
            document: 1,
            code: AnalysisFailure::SourceChanged,
            message: "changed".into(),
        },
    );
    assert!(matches!(
        doc(&analysis).phase(),
        Phase::Failed { message, .. } if message.contains("changed or was replaced")
    ));
}

/// A converted file keeps the identity Encode handed over; a size that no
/// longer matches the verified one is said, not hidden.
#[test]
fn an_encoded_result_keeps_its_identity() {
    let engine = Engine { rows: stream(40) };
    let mut analysis = Analysis::new();
    let result = ResultOf {
        item: 7,
        source: PathBuf::from("/media/clip.y4m"),
        output: PathBuf::from("/media/clip.obu"),
        bytes: 360,
    };
    let effects = analysis.input(Input::OpenResult(result.clone()));
    engine.settle(&mut analysis, effects);
    assert_eq!(*doc(&analysis).origin(), Origin::Result(result.clone()));
    assert!(doc(&analysis).notice().is_none());
    // Handing the same result over again shows it; it is not read twice.
    assert_eq!(analysis.input(Input::OpenResult(result.clone())).len(), 0);
    assert_eq!(analysis.documents().len(), 1);

    let mut analysis = Analysis::new();
    let changed = ResultOf {
        bytes: 999,
        ..result
    };
    let effects = analysis.input(Input::OpenResult(changed));
    engine.settle(&mut analysis, effects);
    assert!(
        doc(&analysis)
            .caution()
            .is_some_and(|n| n.contains("360 bytes now") && n.contains("999"))
    );
}

#[test]
fn closing_while_opening_cancels_and_closes_and_drops_the_late_open() {
    let mut analysis = Analysis::new();
    let open = analysis.input(Input::Open(vec![PathBuf::from("/media/a.obu")]));
    let close = analysis.input(Input::Close);
    // The close follows the open in the engine's queue: whatever the read
    // kept is let go before any later open.
    assert!(matches!(
        commands(&close)[..],
        [
            Command::AnalysisCancel { document: 1 },
            Command::AnalysisClose { document: 1 }
        ]
    ));
    assert_eq!(analysis.documents().len(), 0);
    // The open finished before the cancel arrived: nobody hears it.
    let late = analysis.event(
        open[0].request_id,
        Event::AnalysisOpened {
            document: 1,
            summary: summary(3),
        },
    );
    assert_eq!(late.len(), 0);
    assert_eq!(analysis.documents().len(), 0);
}

#[test]
fn at_most_four_documents_are_open() {
    let mut analysis = Analysis::new();
    let paths: Vec<PathBuf> = (0..5)
        .map(|i| PathBuf::from(format!("/media/{i}.obu")))
        .collect();
    let effects = analysis.input(Input::Open(paths));
    assert_eq!(effects.len(), MAX_DOCUMENTS);
    assert_eq!(analysis.documents().len(), MAX_DOCUMENTS);
    assert!(analysis.notice().is_some_and(|n| n.contains("4.obu")));
}

#[test]
fn offsets_parse_as_decimal_or_hexadecimal() {
    assert_eq!(parse_offset("0"), Some(0));
    assert_eq!(parse_offset(" 1 234 "), Some(1234));
    assert_eq!(parse_offset("1_000"), Some(1000));
    assert_eq!(parse_offset("0x1F"), Some(31));
    assert_eq!(parse_offset("0XffFF"), Some(65535));
    assert_eq!(parse_offset("18446744073709551615"), Some(u64::MAX));
    for bad in ["", "0x", "-3", "1e3", "0x1G", "18446744073709551616", "abc"] {
        assert_eq!(parse_offset(bad), None, "{bad}");
    }
}

#[test]
fn left_selects_the_packet_and_right_its_first_obu() {
    let mut rows = vec![Row {
        index: 0,
        parent: None,
        element: Element::Packet {
            packet: PacketFact {
                index: 0,
                pts: 0,
                header: span(0, 12),
                payload: span(12, 18),
                stream: span(0, 18),
            },
        },
    }];
    for i in 1..=2 {
        let mut row = obu(i, 12 + (i - 1) * 9, 8, 2, 0);
        row.parent = Some(0);
        rows.push(row);
    }
    rows.push(Row {
        index: 3,
        parent: None,
        element: Element::Unparsed {
            span: span(30, 6),
            kind: ProblemKind::Malformed,
            message: "cut".into(),
        },
    });
    let engine = Engine { rows };
    let mut analysis = Analysis::new();
    let effects = analysis.input(Input::Open(vec![PathBuf::from("/media/a.ivf")]));
    engine.settle(&mut analysis, effects);
    let effects = analysis.input(Input::RowClicked(2));
    engine.settle(&mut analysis, effects);
    let effects = analysis.input(Input::Move(Step::Left));
    assert!(matches!(
        commands(&effects)[..],
        [Command::AnalysisLocate {
            target: LocateTarget::Element { index: 0 },
            ..
        }]
    ));
    engine.settle(&mut analysis, effects);
    assert_eq!(selected(&analysis), Some((0, Position::Row(0))));
    let effects = analysis.input(Input::Move(Step::Right));
    engine.settle(&mut analysis, effects);
    assert_eq!(selected(&analysis), Some((1, Position::Row(1))));
    // An OBU has no nested rows; the region after the packet is no child.
    let effects = analysis.input(Input::Move(Step::Right));
    assert_eq!(effects.len(), 0);
    assert_eq!(selected(&analysis).map(|s| s.0), Some(1));
}

fn pages(effects: &[Effect]) -> Vec<(u64, u64)> {
    effects
        .iter()
        .filter_map(|e| match e.command {
            Command::AnalysisPage { first, .. } => Some((e.request_id, first)),
            _ => None,
        })
        .collect()
}

/// A page of an older filter frees only its own slot: the newer page with
/// the same first row stays asked for, and two pages at most are out.
#[test]
fn a_stale_page_frees_only_its_own_slot() {
    let (mut analysis, engine) = opened(200);
    let a = analysis.input(Input::Scroll(100));
    assert_eq!(pages(&a).len(), 1);
    // Filter 1: page 0 goes out next to the old one.
    let b = analysis.input(Input::TypeFilter(1));
    assert_eq!(pages(&b).len(), 1);
    // Filter 2: two are out already, so none is sent yet.
    let c = analysis.input(Input::ClearFilters);
    assert_eq!(pages(&c), []);
    // The first stale answer frees a slot for filter 2's page 0.
    let after_a = engine.answer(&mut analysis, a);
    assert_eq!(pages(&after_a).len(), 1, "{after_a:?}");
    // Filter 1's stale page 0 must not free filter 2's page 0.
    let b_pages: Vec<Effect> = b
        .into_iter()
        .filter(|e| matches!(e.command, Command::AnalysisPage { .. }))
        .collect();
    let after_b = engine.answer(&mut analysis, b_pages);
    assert_eq!(pages(&after_b), [], "page 0 asked for twice");
    engine.settle(&mut analysis, after_a);
    engine.settle(&mut analysis, c);
    assert_eq!(doc(&analysis).total(), Some(200));
    assert_eq!(doc(&analysis).row(0).map(|r| r.index), Some(0));
}

/// A refused command is a failure like any other: an open fails, and a
/// page or byte range gives its slot back.
#[test]
fn a_refused_command_fails_its_request() {
    let refusal = |document_message: &str| Event::Error {
        code: ErrorCode::Malformed,
        message: document_message.into(),
    };
    let mut analysis = Analysis::new();
    let open = analysis.input(Input::Open(vec![PathBuf::from("/media/a.obu")]));
    analysis.event(open[0].request_id, refusal("bad frame"));
    assert!(matches!(
        doc(&analysis).phase(),
        Phase::Failed { message, .. } if message == "bad frame"
    ));

    let (mut analysis, engine) = opened(200);
    let page = analysis.input(Input::Scroll(100));
    analysis.event(page[0].request_id, refusal("bad page"));
    assert!(
        doc(&analysis)
            .notice()
            .is_some_and(|n| n.contains("bad page"))
    );
    // The slot is free: the same rows are asked for again.
    let again = analysis.input(Input::Scroll(1));
    assert_eq!(pages(&again).len(), 1);
    engine.settle(&mut analysis, again);
    let bytes = analysis.input(Input::Cursor(Step::End));
    let asked: Vec<&Effect> = bytes
        .iter()
        .filter(|e| matches!(e.command, Command::AnalysisBytes { .. }))
        .collect();
    assert_eq!(asked.len(), 1);
    analysis.event(asked[0].request_id, refusal("bad range"));
    // Showing the bytes again asks again.
    analysis.input(Input::ToggleBytes);
    let shown = analysis.input(Input::ToggleBytes);
    assert!(
        shown
            .iter()
            .any(|e| matches!(e.command, Command::AnalysisBytes { .. }))
    );
}

/// Answers that break the protocol fail the document; huge totals never
/// overflow the row arithmetic.
#[test]
fn answers_that_do_not_fit_fail_the_document() {
    let broken = |rows: Vec<Row>, first: u64, total: u64| {
        let (mut analysis, _engine) = opened(200);
        let page = analysis.input(Input::Scroll(100));
        let effects = analysis.event(
            page[0].request_id,
            Event::AnalysisPage {
                document: 1,
                first,
                total,
                rows,
            },
        );
        let closed = matches!(
            commands(&effects)[..],
            [Command::AnalysisClose { document: 1 }]
        );
        let failed = matches!(doc(&analysis).phase(), Phase::Failed { .. });
        // A broken document gives its engine slot back.
        assert_eq!(closed, failed, "{effects:?}");
        failed
    };
    assert!(broken(stream(65), 100, 200), "more rows than asked for");
    assert!(broken(stream(64), 100, 150), "rows past the total");
    assert!(broken(Vec::new(), 100, 200), "no rows where there are some");
    assert!(
        broken(stream(1), 99, 200),
        "another first row than asked for"
    );
    assert!(!broken(stream(64), 100, 200));

    let (mut analysis, _engine) = opened(200);
    let bytes = analysis.input(Input::Cursor(Step::End));
    let (asked, offset) = bytes
        .iter()
        .find_map(|e| match e.command {
            Command::AnalysisBytes { offset, .. } => Some((e.request_id, offset)),
            _ => None,
        })
        .unwrap_or_default();
    let effects = analysis.event(
        asked,
        Event::AnalysisBytes {
            document: 1,
            offset,
            bytes: vec![0; 3],
        },
    );
    assert!(matches!(doc(&analysis).phase(), Phase::Failed { .. }));
    assert!(matches!(
        commands(&effects)[..],
        [Command::AnalysisClose { document: 1 }]
    ));

    // A document that claims u64::MAX rows: End and Page Down saturate.
    let (mut analysis, _engine) = opened(200);
    let page = analysis.input(Input::Scroll(100));
    analysis.event(
        page[0].request_id,
        Event::AnalysisPage {
            document: 1,
            first: 100,
            total: u64::MAX,
            rows: stream(64),
        },
    );
    let end = analysis.input(Input::Move(Step::End));
    assert!(matches!(
        commands(&end)[..],
        [Command::AnalysisPage { first, .. }] if *first == u64::MAX - VISIBLE_ROWS
    ));
    assert_eq!(doc(&analysis).top(), u64::MAX - VISIBLE_ROWS);
    analysis.input(Input::Scroll(i32::MAX));
    assert_eq!(doc(&analysis).top(), u64::MAX - VISIBLE_ROWS);
    analysis.input(Input::Move(Step::PageDown));
    // Rows that would end past u64::MAX do not fit.
    analysis.event(
        end[0].request_id,
        Event::AnalysisPage {
            document: 1,
            first: u64::MAX - VISIBLE_ROWS,
            total: u64::MAX,
            rows: stream(VISIBLE_ROWS + 1),
        },
    );
    assert!(matches!(doc(&analysis).phase(), Phase::Failed { .. }));
}

/// A failed position lookup does not cancel a byte click that waits for
/// its answer.
#[test]
fn a_failed_position_lookup_keeps_a_pending_byte_selection() {
    let (mut analysis, engine) = opened(40);
    let effects = analysis.input(Input::RowClicked(3));
    engine.settle(&mut analysis, effects);
    let click = analysis.input(Input::ByteClicked(50));
    let filter = analysis.input(Input::TypeFilter(1));
    for effect in &filter {
        if matches!(effect.command, Command::AnalysisLocate { .. }) {
            analysis.event(
                effect.request_id,
                Event::AnalysisFailed {
                    document: 1,
                    code: AnalysisFailure::InvalidRequest,
                    message: "no".into(),
                },
            );
        }
    }
    engine.settle(&mut analysis, click);
    assert_eq!(selected(&analysis).map(|s| s.0), Some(5));
}

/// A request's problem stays until the next input for that document; what
/// stays true of the file stays.
#[test]
fn notices_clear_at_the_next_input_and_cautions_stay() {
    let (mut analysis, _engine) = opened(200);
    let effects = analysis.input(Input::ByteClicked(3));
    analysis.event(
        effects[0].request_id,
        Event::AnalysisFailed {
            document: 1,
            code: AnalysisFailure::LimitReached,
            message: "more than 64 requests are waiting".into(),
        },
    );
    assert!(doc(&analysis).notice().is_some());
    analysis.input(Input::Scroll(1));
    assert_eq!(doc(&analysis).notice(), None);

    let engine = Engine { rows: stream(40) };
    let mut analysis = Analysis::new();
    let effects = analysis.input(Input::OpenResult(ResultOf {
        item: 1,
        source: PathBuf::from("/media/a.y4m"),
        output: PathBuf::from("/media/a.obu"),
        bytes: 5,
    }));
    engine.settle(&mut analysis, effects);
    analysis.input(Input::Scroll(1));
    analysis.input(Input::Move(Step::Down));
    assert!(
        doc(&analysis)
            .caution()
            .is_some_and(|n| n.contains("conversion wrote 5"))
    );
    assert_eq!(analysis.notice_line(), doc(&analysis).caution());
}

/// The workspace's one notice: the document's request problem first, then
/// a refusal about no single document, then what stays true of the file.
#[test]
fn a_caution_never_hides_a_refusal() {
    let engine = Engine { rows: stream(40) };
    let mut analysis = Analysis::new();
    let effects = analysis.input(Input::OpenResult(ResultOf {
        item: 1,
        source: PathBuf::from("/media/a.y4m"),
        output: PathBuf::from("/media/a.obu"),
        bytes: 5,
    }));
    engine.settle(&mut analysis, effects);
    for i in 0..3 {
        let effects = analysis.input(Input::Open(vec![PathBuf::from(format!("/media/{i}.obu"))]));
        engine.settle(&mut analysis, effects);
    }
    analysis.input(Input::Show(0));
    assert!(
        analysis
            .notice_line()
            .is_some_and(|n| n.contains("conversion wrote 5"))
    );
    // A fifth document is refused: that is what the workspace says.
    analysis.input(Input::Open(vec![PathBuf::from("/media/fifth.obu")]));
    assert!(
        analysis
            .notice_line()
            .is_some_and(|n| n.contains("fifth.obu")),
        "{:?}",
        analysis.notice_line()
    );
    // A problem of the shown document's last request comes first.
    let effects = analysis.input(Input::ByteClicked(3));
    analysis.event(
        effects[0].request_id,
        Event::AnalysisFailed {
            document: 1,
            code: AnalysisFailure::LimitReached,
            message: "busy".into(),
        },
    );
    assert!(analysis.notice_line().is_some_and(|n| n.contains("busy")));
}

/// Pages that fail after a filter change leave no rows and no total; the
/// next key press asks for them again, and the keys work once they come.
#[test]
fn a_key_press_restarts_paging_that_failed() {
    let (mut analysis, engine) = opened(200);
    let old = analysis.input(Input::Scroll(100));
    let first = analysis.input(Input::TypeFilter(1));
    for effect in old.iter().chain(&first) {
        if matches!(effect.command, Command::AnalysisPage { .. }) {
            analysis.event(
                effect.request_id,
                Event::AnalysisFailed {
                    document: 1,
                    code: AnalysisFailure::LimitReached,
                    message: "more than 64 requests are waiting".into(),
                },
            );
        }
    }
    assert_eq!(doc(&analysis).total(), None);
    let retry = analysis.input(Input::Move(Step::Down));
    assert_eq!(pages(&retry).len(), 1, "{retry:?}");
    engine.settle(&mut analysis, retry);
    assert_eq!(doc(&analysis).total(), Some(100));
    let effects = analysis.input(Input::Move(Step::Down));
    engine.settle(&mut analysis, effects);
    assert!(selected(&analysis).is_some());
}

mod lifecycle;

#[test]
fn closing_cancels_validation_and_old_syntax_cannot_reach_a_new_generation() {
    use studio_domain::syntax::{SYNTAX_SCHEMA, SyntaxStatus, SyntaxSummary};

    let (mut analysis, engine) = opened(4);
    let old = doc(&analysis).id();
    let effects = analysis.input(Input::RowClicked(0));
    engine.settle(&mut analysis, effects);
    let effects = analysis.input(Input::EvidenceView(1));
    let syntax = effects.first().map_or(0, |effect| effect.request_id);
    assert_ne!(syntax, 0);
    let effects = analysis.input(Input::Validate);
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect.command, Command::AnalysisValidate { .. }))
    );
    let effects = analysis.input(Input::Close);
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect.command, Command::AnalysisCancelValidation { .. }))
    );
    engine.settle(&mut analysis, effects);
    let effects = analysis.input(Input::Open(vec![PathBuf::from("/media/clip.obu")]));
    engine.settle(&mut analysis, effects);
    assert_ne!(old, doc(&analysis).id());

    let effects = analysis.event(
        syntax,
        Event::AnalysisSyntaxReady {
            document: old,
            element: 0,
            summary: Box::new(SyntaxSummary {
                schema_version: SYNTAX_SCHEMA,
                spec_revision: "AV2 v1.0.0".to_owned(),
                status: SyntaxStatus::Parsed,
                error: None,
                retained_events: 0,
                observed_elements: 0,
                elements_truncated: false,
                depth_truncated: false,
            }),
            total: 0,
        },
    );
    assert_eq!(effects.len(), 0);
    assert!(doc(&analysis).evidence.syntax.is_none());
    assert!(doc(&analysis).evidence.validation.is_none());
}

#[test]
fn incompatible_syntax_schema_preserves_byte_inspection() {
    use studio_domain::syntax::{SYNTAX_SCHEMA, SyntaxStatus, SyntaxSummary};

    let (mut analysis, engine) = opened(4);
    let effects = analysis.input(Input::RowClicked(0));
    engine.settle(&mut analysis, effects);
    let effects = analysis.input(Input::EvidenceView(1));
    let syntax = effects.first().map_or(0, |effect| effect.request_id);
    assert_ne!(syntax, 0);
    let document = doc(&analysis).id();
    let next = analysis.event(
        syntax,
        Event::AnalysisSyntaxReady {
            document,
            element: 0,
            summary: Box::new(SyntaxSummary {
                schema_version: SYNTAX_SCHEMA + 1,
                spec_revision: "AV2 v1.0.0".to_owned(),
                status: SyntaxStatus::Parsed,
                error: None,
                retained_events: 2,
                observed_elements: 2,
                elements_truncated: false,
                depth_truncated: false,
            }),
            total: 2,
        },
    );
    assert_eq!(next.len(), 0);
    assert!(doc(&analysis).summary().is_some());
    assert!(doc(&analysis).bytes().data.is_some());
    assert!(doc(&analysis).evidence.syntax.is_none());
    assert!(doc(&analysis).evidence.syntax_error.is_some());
}
