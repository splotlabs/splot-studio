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
        digest: None,
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
            Command::AnalysisOpenProgressive { document, .. } => Event::AnalysisOpened {
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
            Command::AnalysisCancelValidation { .. } => Event::CancelRequested,
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
fn clearing_the_cache_with_no_documents_keeps_the_engine_until_the_reply() {
    let mut analysis = Analysis::new();
    let effects = analysis.input(Input::ClearCache);
    assert_eq!(commands(&effects), [&Command::AnalysisClearCache]);
    assert!(analysis.needs_engine());
    assert_eq!(analysis.input(Input::ClearCache), []);
    assert_eq!(
        analysis.event(effects[0].request_id, Event::AnalysisCacheCleared),
        []
    );
    assert!(!analysis.needs_engine());
    assert_eq!(analysis.notice(), Some("Studio cleared the index cache."));
}

#[test]
fn progressive_scan_keeps_rows_available_through_cancel() {
    let mut analysis = Analysis::new();
    let open = analysis.input(Input::Open(vec![PathBuf::from("/media/long.obu")]));
    let request = open[0].request_id;
    let mut first = summary(4);
    let final_summary = summary(20);
    first.fingerprint = final_summary.fingerprint;
    first.outcome = ScanOutcome::Scanning;
    let effects = analysis.event(
        request,
        Event::AnalysisOpenProgress {
            document: 1,
            summary: first,
        },
    );
    for effect in effects {
        let available = if matches!(effect.command, Command::AnalysisBytes { .. }) {
            20
        } else {
            4
        };
        Engine {
            rows: stream(available),
        }
        .settle(&mut analysis, vec![effect]);
    }
    assert_eq!(doc(&analysis).total(), Some(4));
    assert_eq!(doc(&analysis).phase(), &Phase::Opening);
    let select = analysis.input(Input::RowClicked(2));
    Engine { rows: stream(4) }.settle(&mut analysis, select);
    assert_eq!(selected(&analysis), Some((2, Position::Row(2))));

    let cancel = analysis.input(Input::Cancel);
    assert!(matches!(
        commands(&cancel)[..],
        [Command::AnalysisCancel { document: 1 }]
    ));
    assert_eq!(doc(&analysis).phase(), &Phase::Cancelling);
    assert_eq!(doc(&analysis).total(), Some(4));
    let effects = analysis.event(
        request,
        Event::AnalysisOpened {
            document: 1,
            summary: final_summary,
        },
    );
    Engine { rows: stream(20) }.settle(&mut analysis, effects);
    assert!(matches!(doc(&analysis).phase(), Phase::Open(_)));
    assert_eq!(doc(&analysis).total(), Some(20));
    assert_eq!(selected(&analysis), Some((2, Position::Row(2))));
}

#[test]
fn opening_asks_for_the_first_page_and_byte_window() {
    let mut analysis = Analysis::new();
    let effects = analysis.input(Input::Open(vec![PathBuf::from("/media/clip.obu")]));
    assert!(matches!(
        commands(&effects)[..],
        [Command::AnalysisOpenProgressive { document: 1, .. }]
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
            Command::AnalysisOpenProgressive { document, .. } => Some(document),
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
fn playback_links_reveal_bytes_only_in_the_current_read() {
    let (mut analysis, engine) = opened(100);
    let document = doc(&analysis).id();
    let effects = analysis.input(Input::ToggleBytes);
    engine.settle(&mut analysis, effects);
    assert!(!doc(&analysis).bytes().shown);

    let effects = analysis.input(Input::PlaybackByte {
        document: document + 1,
        offset: 723,
    });
    assert_eq!(effects, []);
    assert!(selected(&analysis).is_none());
    assert!(!doc(&analysis).bytes().shown);

    let effects = analysis.input(Input::PlaybackByte {
        document,
        offset: 723,
    });
    engine.settle(&mut analysis, effects);
    assert_eq!(selected(&analysis), Some((80, Position::Row(80))));
    let bytes = doc(&analysis).bytes();
    assert!(bytes.shown);
    assert_eq!(bytes.cursor, 723);
    assert_eq!(bytes.offset, 720);
    assert!(bytes.data.is_some());
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
        [Command::AnalysisOpenProgressive { document: 2, .. }]
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

mod layers;
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

fn load_fixture_findings(analysis: &mut Analysis) {
    use studio_domain::validation::{
        Finding, Severity, VALIDATION_SCHEMA, ValidationCompletion, ValidationSummary,
    };

    let validate = analysis.input(Input::Validate);
    let report = validate[0].request_id;
    let page = analysis.event(
        report,
        Event::AnalysisValidated {
            document: 1,
            summary: Box::new(ValidationSummary {
                schema: VALIDATION_SCHEMA,
                spec_revision: "AV2 v1.0.0".into(),
                codec_revision: "fixture".into(),
                engine_version: "fixture".into(),
                strict: false,
                context: None,
                scope: "Fixture".into(),
                completion: ValidationCompletion::EndOfInput,
                checked_obus: 4,
                last_checked_obu_end: Some(36),
                malformed_units: 0,
                finalized: true,
                errors: 2,
                warnings: 0,
                info: 0,
                finding_limit: 2,
                omitted: 0,
            }),
            total: 2,
        },
    );
    let findings = [None, Some(10)]
        .into_iter()
        .map(|file_offset| Finding {
            rule_id: "fixture/rule".into(),
            severity: Severity::Error,
            message: "Fixture finding".into(),
            spec_section: None,
            file_offset,
            stream_offset: file_offset,
            bit_offset: None,
            extended_layer_id: None,
        })
        .collect();
    analysis.event(
        page[0].request_id,
        Event::AnalysisFindings {
            document: 1,
            validation: report,
            first: 0,
            total: 2,
            findings,
        },
    );
}

#[test]
fn unlocated_findings_keep_history_and_next_waits_for_the_locate() {
    let (mut analysis, engine) = opened(4);
    load_fixture_findings(&mut analysis);

    analysis.input(Input::EvidencePick(0));
    assert_eq!(doc(&analysis).evidence.selected_index(), Some(0));
    assert!(selected(&analysis).is_none());

    let next = analysis.input(Input::FindingStep(1));
    assert_eq!(next.len(), 1);
    assert_eq!(analysis.input(Input::FindingStep(1)).len(), 0);
    engine.settle(&mut analysis, next);
    assert_eq!(doc(&analysis).evidence.selected_index(), Some(1));
    assert_eq!(selected(&analysis), Some((1, Position::Row(1))));

    analysis.input(Input::FindingStep(-1));
    assert_eq!(doc(&analysis).evidence.selected_index(), Some(0));
    assert_eq!(selected(&analysis), Some((1, Position::Row(1))));
    for expected in [Some(1), Some(0)] {
        let back = analysis.input(Input::Back);
        engine.settle(&mut analysis, back);
        assert_eq!(doc(&analysis).evidence.selected_index(), expected);
    }
    assert!(selected(&analysis).is_none());
    for expected in [Some(1), Some(0)] {
        let forward = analysis.input(Input::Forward);
        engine.settle(&mut analysis, forward);
        assert_eq!(doc(&analysis).evidence.selected_index(), expected);
        assert_eq!(selected(&analysis), Some((1, Position::Row(1))));
    }
}

#[test]
fn a_locate_for_a_replaced_report_releases_the_navigation_slot() {
    let (mut analysis, engine) = opened(4);
    load_fixture_findings(&mut analysis);
    let old = analysis.input(Input::EvidencePick(1));
    assert!(doc(&analysis).selection_pending());
    assert_eq!(old.len(), 1);

    analysis.input(Input::Validate);
    engine.settle(&mut analysis, old);
    assert!(!doc(&analysis).selection_pending());
    assert!(selected(&analysis).is_none());
}

#[test]
fn a_restored_syntax_row_is_visible_only_after_its_page_arrives() {
    use studio_domain::syntax::{SYNTAX_SCHEMA, SyntaxEvent, SyntaxStatus, SyntaxSummary};

    let (mut analysis, engine) = opened(4);
    let effects = analysis.input(Input::RowClicked(0));
    engine.settle(&mut analysis, effects);
    let request = analysis.input(Input::EvidenceView(1));
    let trace = request[0].request_id;
    // History keeps the field identity while the new trace loads.
    analysis.documents[0].evidence.selected = Some(Pick::Syntax {
        element: 0,
        index: 0,
    });
    let page = analysis.event(
        trace,
        Event::AnalysisSyntaxReady {
            document: 1,
            element: 0,
            summary: Box::new(SyntaxSummary {
                schema_version: SYNTAX_SCHEMA,
                spec_revision: "AV2 v1.0.0".into(),
                status: SyntaxStatus::Parsed,
                error: None,
                retained_events: 1,
                observed_elements: 1,
                elements_truncated: false,
                depth_truncated: false,
            }),
            total: 1,
        },
    );
    assert_eq!(doc(&analysis).evidence.selected_index(), None);
    analysis.event(
        page[0].request_id,
        Event::AnalysisSyntaxPage {
            document: 1,
            trace,
            element: 0,
            first: 0,
            total: 1,
            events: vec![SyntaxEvent::Absent {
                name: "field".into(),
                condition: "condition".into(),
                inferred: None,
            }],
        },
    );
    assert_eq!(doc(&analysis).evidence.selected_index(), Some(0));
}

#[test]
fn timeline_history_keeps_coded_and_output_links_distinct() {
    use studio_domain::timeline::{TimelineSelection, TimelineTarget};

    let (mut analysis, engine) = opened(40);
    let scan = start_timeline(&mut analysis);
    let shown = TimelineSelection {
        coded_ordinal: 7,
        coded_offset: 19,
        output_ordinal: Some(2),
        showing_offset: Some(37),
        time: None,
    };
    resolve_timeline(
        &mut analysis,
        &engine,
        scan,
        TimelineTarget::Output { index: 2 },
        shown,
    );
    assert_eq!(selected(&analysis), Some((4, Position::Row(4))));
    assert_eq!(doc(&analysis).bytes().cursor, 37);
    assert_eq!(analysis.take_timeline_picture(), Some((1, Some(2))));

    resolve_timeline(
        &mut analysis,
        &engine,
        scan,
        TimelineTarget::Coded { index: 7 },
        shown,
    );
    assert_eq!(selected(&analysis), Some((2, Position::Row(2))));
    assert_eq!(doc(&analysis).bytes().cursor, 19);
    assert_eq!(analysis.take_timeline_picture(), Some((1, Some(2))));

    let hidden = TimelineSelection {
        coded_ordinal: 8,
        coded_offset: 28,
        output_ordinal: None,
        showing_offset: None,
        time: None,
    };
    resolve_timeline(
        &mut analysis,
        &engine,
        scan,
        TimelineTarget::Coded { index: 8 },
        hidden,
    );
    assert_eq!(selected(&analysis), Some((3, Position::Row(3))));
    assert_eq!(analysis.take_timeline_picture(), Some((1, None)));

    let effects = analysis.input(Input::Back);
    engine.settle(&mut analysis, effects);
    assert_eq!(selected(&analysis), Some((2, Position::Row(2))));
    assert_eq!(analysis.take_timeline_picture(), Some((1, Some(2))));
    let effects = analysis.input(Input::Back);
    engine.settle(&mut analysis, effects);
    assert_eq!(selected(&analysis), Some((4, Position::Row(4))));
    assert_eq!(analysis.take_timeline_picture(), Some((1, Some(2))));
}

#[test]
fn timeline_drops_a_superseded_time_before_linking_evidence() {
    use studio_domain::timeline::{TimelineSelection, TimelineTarget, TimelineTime};

    let (mut analysis, engine) = opened(40);
    let scan = start_timeline(&mut analysis);
    let target = |num| TimelineTarget::Time {
        time: TimelineTime { num, den: 1 },
    };
    let selection = TimelineSelection {
        coded_ordinal: 7,
        coded_offset: 19,
        output_ordinal: Some(2),
        showing_offset: Some(37),
        time: Some(TimelineTime { num: 1, den: 1 }),
    };
    let first = analysis.input(Input::TimelineSelect(target(1)));
    assert_eq!(
        analysis.input(Input::TimelineSelect(target(2))),
        [] as [Effect; 0]
    );
    let effects = analysis.event(first[0].request_id, timeline_reply(scan, selection));
    assert!(matches!(
        commands(&effects)[..],
        [Command::AnalysisTimelineLocate { target: next, .. }] if *next == target(2)
    ));
    assert_eq!(analysis.take_timeline_picture(), None);
    let picked = analysis.input(Input::RowClicked(5));
    engine.settle(&mut analysis, picked);
    let late = analysis.event(effects[0].request_id, timeline_reply(scan, selection));
    assert_eq!(late, [] as [Effect; 0]);
    assert_eq!(selected(&analysis), Some((5, Position::Row(5))));
    assert_eq!(analysis.take_timeline_picture(), None);
}

#[test]
fn a_live_timeline_keeps_one_range_request_and_refreshes_after_completion() {
    use studio_domain::timeline::{
        TimelineClock, TimelineFileBytes, TimelinePage, TimelineTarget, TimelineView,
    };

    let (mut analysis, _) = opened(40);
    let scan = analysis.input(Input::TimelineBuild)[0].request_id;
    assert!(doc(&analysis).timeline().can_refresh());
    let first = analysis.input(Input::TimelineRefresh);
    assert!(matches!(
        commands(&first)[..],
        [Command::AnalysisTimeline { .. }]
    ));
    assert!(!doc(&analysis).timeline().can_refresh());
    assert_eq!(analysis.input(Input::TimelineRefresh), []);

    let live_page = || Event::AnalysisTimeline {
        document: 1,
        scan,
        page: Box::new(TimelinePage {
            decode_selection: studio_domain::layers::LayerDecodeSelection::Default,
            view: TimelineView::Coded,
            first: 0,
            total: 1,
            complete: false,
            verified: false,
            clock: TimelineClock::Unavailable,
            file_bytes: TimelineFileBytes {
                file: 360,
                obu: 9,
                assigned_obu: 9,
                unassigned_obu: 0,
                framing: 0,
                unclassified: 351,
            },
            bytes: None,
            rate: None,
            buckets: Vec::new(),
        }),
    };
    assert_eq!(analysis.event(first[0].request_id, live_page()), []);
    assert!(doc(&analysis).timeline().can_refresh());
    let linked = analysis.input(Input::TimelineSelect(TimelineTarget::Coded { index: 0 }));
    assert!(matches!(
        commands(&linked)[..],
        [Command::AnalysisTimelineLocate { .. }]
    ));

    let in_flight = analysis.input(Input::TimelineRefresh);
    assert!(matches!(
        commands(&analysis.input(Input::TimelineCancel))[..],
        [Command::AnalysisCancelIdentity { .. }]
    ));
    assert!(!doc(&analysis).timeline().can_refresh());
    assert_eq!(analysis.input(Input::TimelineRefresh), []);
    assert_eq!(analysis.event(scan, timeline_finished()), []);
    let final_range = analysis.event(in_flight[0].request_id, live_page());
    assert!(matches!(
        commands(&final_range)[..],
        [Command::AnalysisTimeline { .. }]
    ));
    assert!(!doc(&analysis).timeline().can_refresh());
}

fn start_timeline(analysis: &mut Analysis) -> u64 {
    let effects = analysis.input(Input::TimelineBuild);
    let scan = effects[0].request_id;
    let _ = analysis.event(scan, timeline_finished());
    scan
}

fn timeline_finished() -> Event {
    use studio_domain::identity::{IDENTITY_SCHEMA, IdentityCompletion, IdentitySummary};
    use studio_domain::preview::GrainPolicy;

    let summary = IdentitySummary {
        schema: IDENTITY_SCHEMA,
        codec_revision: "codec".into(),
        spec_revision: "spec".into(),
        fingerprint: summary(40).fingerprint,
        grain: GrainPolicy::Skip,
        selection: studio_domain::layers::LayerDecodeSelection::Default,
        digest: None,
        matches_structure: false,
        completion: IdentityCompletion::Complete,
        temporal: 1,
        coded: 9,
        continuations: 0,
        outputs: 3,
    };
    Event::AnalysisIdentityScanned {
        document: 1,
        summary: Box::new(summary),
    }
}

fn timeline_reply(scan: u64, selection: studio_domain::timeline::TimelineSelection) -> Event {
    Event::AnalysisTimelineLocated {
        document: 1,
        scan,
        selection: Box::new(selection),
    }
}

fn resolve_timeline(
    analysis: &mut Analysis,
    engine: &Engine,
    scan: u64,
    target: studio_domain::timeline::TimelineTarget,
    selection: studio_domain::timeline::TimelineSelection,
) {
    let effects = analysis.input(Input::TimelineSelect(target));
    let links = analysis.event(effects[0].request_id, timeline_reply(scan, selection));
    engine.settle(analysis, links);
}

#[test]
fn reference_inspection_keeps_coded_identity_without_a_range_page() {
    use studio_domain::timeline::{TimelineSelection, TimelineTarget, TimelineView};

    let (mut analysis, engine) = opened(40);
    let scan = start_timeline(&mut analysis);
    let shown = TimelineSelection {
        coded_ordinal: 7,
        coded_offset: 19,
        output_ordinal: Some(2),
        showing_offset: Some(37),
        time: None,
    };
    resolve_timeline(
        &mut analysis,
        &engine,
        scan,
        TimelineTarget::Output { index: 2 },
        shown,
    );
    let effects = analysis.input(Input::EvidenceView(references::TAB));
    engine.settle(&mut analysis, effects);
    analysis.input(Input::TimelineRange {
        view: TimelineView::Output,
        first: 1,
        limit: 2,
        buckets: 1,
    });
    assert!(doc(&analysis).timeline().page().is_none());
    assert_eq!(doc(&analysis).bytes().cursor, 37);

    let inspection = analysis.input(Input::InspectReferences);
    assert!(matches!(
        commands(&inspection)[..],
        [Command::AnalysisReferences { query, .. }] if query.coded_offset == 19
    ));
    let request = inspection[0].request_id;
    let rebuild = analysis.input(Input::TimelineBuild);
    assert!(matches!(
        commands(&rebuild)[..],
        [
            Command::AnalysisCancelReferences { reference, .. },
            Command::AnalysisIdentityScan { .. }
        ] if *reference == request
    ));
    assert!(!doc(&analysis).references.busy());
    assert_eq!(doc(&analysis).coded_offset(), Some(37));
}

#[test]
fn reference_inspection_rejects_cancelled_and_mismatched_answers() {
    let (mut analysis, engine) = opened(5);
    let effects = analysis.input(Input::RowClicked(1));
    engine.settle(&mut analysis, effects);
    let effects = analysis.input(Input::EvidenceView(references::TAB));
    engine.settle(&mut analysis, effects);

    let (request, document, original) = inspect_reference(&mut analysis);
    assert_eq!(original.coded_offset, 10);
    let reply = |query| reference_reply(document, query);
    let stop = analysis.input(Input::StopReferences);
    assert!(stop.iter().any(|effect| matches!(
        effect.command, Command::AnalysisCancelReferences { reference, .. } if reference == request
    )));
    assert_eq!(analysis.event(request, reply(original)), Vec::new());
    assert!(doc(&analysis).references.evidence().is_none());

    let (request, _, _) = inspect_reference(&mut analysis);
    analysis.event(request, reply(original));
    assert!(doc(&analysis).references.evidence().is_none());
    assert!(!doc(&analysis).references.busy());

    let (request, _, current) = inspect_reference(&mut analysis);
    analysis.event(request, reply(current));
    assert!(doc(&analysis).references.evidence().is_some());
    let effects = analysis.input(Input::RowClicked(2));
    engine.settle(&mut analysis, effects);
    assert!(doc(&analysis).references.evidence().is_none());
}

#[test]
fn following_a_reference_keeps_the_verified_index_while_its_page_loads() {
    use studio_domain::timeline::{TimelineSelection, TimelineTarget, TimelineView};

    let (mut analysis, engine) = opened(40);
    let scan = analysis.input(Input::TimelineBuild)[0].request_id;
    let mut finished = timeline_finished();
    if let Event::AnalysisIdentityScanned { summary, .. } = &mut finished {
        summary.matches_structure = true;
    }
    analysis.event(scan, finished);
    let origin = TimelineSelection {
        coded_ordinal: 7,
        coded_offset: 19,
        output_ordinal: Some(2),
        showing_offset: Some(37),
        time: None,
    };
    resolve_timeline(
        &mut analysis,
        &engine,
        scan,
        TimelineTarget::Output { index: 2 },
        origin,
    );
    let effects = analysis.input(Input::EvidenceView(references::TAB));
    engine.settle(&mut analysis, effects);
    let (request, document, query) = inspect_reference(&mut analysis);
    analysis.event(request, reference_reply(document, query));
    analysis.input(Input::TimelineRange {
        view: TimelineView::Output,
        first: 1,
        limit: 2,
        buckets: 1,
    });

    assert!(doc(&analysis).timeline().page().is_none());
    let effects = analysis.input(Input::FollowReference(0));
    assert!(matches!(
        commands(&effects)[..],
        [Command::AnalysisTimelineLocate {
            target: TimelineTarget::CodedOffset { offset: 10 },
            ..
        }]
    ));
    let referenced = TimelineSelection {
        coded_ordinal: 4,
        coded_offset: 10,
        output_ordinal: Some(0),
        showing_offset: Some(10),
        time: None,
    };
    let links = analysis.event(effects[0].request_id, timeline_reply(scan, referenced));
    engine.settle(&mut analysis, links);
    assert_eq!(doc(&analysis).bytes().cursor, 10);
    assert_eq!(analysis.take_timeline_picture(), Some((document, Some(0))));
}

#[test]
fn opening_a_different_document_cancels_the_outgoing_reference_replay() {
    let first = PathBuf::from("/media/clip.obu");
    let second = PathBuf::from("/media/other.obu");
    let (mut analysis, engine) = opened(5);

    for path in [second, first] {
        let effects = analysis.input(Input::RowClicked(1));
        engine.settle(&mut analysis, effects);
        let effects = analysis.input(Input::EvidenceView(references::TAB));
        engine.settle(&mut analysis, effects);
        let (request, document, query) = inspect_reference(&mut analysis);

        let effects = analysis.input(Input::Open(vec![path.clone()]));
        assert!(matches!(
            effects.first().map(|effect| &effect.command),
            Some(Command::AnalysisCancelReferences { document: d, reference })
                if *d == document && *reference == request
        ));
        engine.settle(&mut analysis, effects);
        assert_eq!(doc(&analysis).path, path);
        assert_ne!(doc(&analysis).id, document);
        assert_eq!(
            analysis.event(request, reference_reply(document, query)),
            []
        );
        assert!(analysis.documents.iter().all(|doc| !doc.references.busy()));
    }
}

fn inspect_reference(
    analysis: &mut Analysis,
) -> (u64, u64, studio_domain::references::ReferenceQuery) {
    analysis
        .input(Input::InspectReferences)
        .iter()
        .find_map(|effect| match effect.command {
            Command::AnalysisReferences { document, query } => {
                Some((effect.request_id, document, query))
            }
            _ => None,
        })
        .unwrap_or_else(|| unreachable!("expected a reference query"))
}

fn reference_reply(document: u64, query: studio_domain::references::ReferenceQuery) -> Event {
    use studio_domain::identity::DecodeLayers;
    use studio_domain::references::{
        ReferenceDependency, ReferenceEvidence, ReferenceFrame, ReferenceOrigin, ReferenceSlot,
        ReferenceSlotState,
    };

    let frame = ReferenceFrame {
        coded_offset: query.coded_offset,
        sequence_offset: 1,
        layers: DecodeLayers {
            extended: 0,
            embedded: 0,
            temporal: 0,
        },
        width: 64,
        height: 64,
    };
    Event::AnalysisReferencesReady {
        document,
        evidence: Box::new(ReferenceEvidence {
            query,
            frame,
            frame_obu_type: 7,
            refresh_mask: 1,
            resets_sequence: false,
            allowed_input_slots: None,
            required_long_term_ids: Vec::new(),
            slots: vec![ReferenceSlot {
                slot: 0,
                before: ReferenceSlotState::Retained {
                    frame: ReferenceFrame {
                        coded_offset: 10,
                        ..frame
                    },
                    long_term_id: None,
                    restricted_order_hint: false,
                },
                after: ReferenceSlotState::Empty,
            }],
            dependencies: vec![ReferenceDependency {
                index: Some(0),
                slot: 0,
                origin: ReferenceOrigin::Signaled,
                scaled: false,
            }],
        }),
    }
}

#[test]
fn reference_history_restores_non_default_decode_policy_and_filter() {
    use studio_domain::layers::{LayerDecodeSelection, LayerScope};

    let (mut analysis, engine) = opened(5);
    let requested = LayerDecodeSelection::OperatingPoint {
        scope: LayerScope::Global,
        set: 0,
        point: 1,
    };
    for input in [
        Input::RowClicked(1),
        Input::EvidenceView(references::TAB),
        Input::LayerFilter(1),
    ] {
        let effects = analysis.input(input);
        engine.settle(&mut analysis, effects);
    }
    let filter = doc(&analysis).filter();
    assert!(filter.layer.is_some());
    analysis.documents[0].layers.selection = requested;
    analysis.save_reference_view(None);

    let effects = analysis.input(Input::RowClicked(2));
    engine.settle(&mut analysis, effects);
    let effects = analysis.input(Input::ClearFilters);
    engine.settle(&mut analysis, effects);
    analysis.documents[0].layers.selection = LayerDecodeSelection::Default;
    analysis.save_reference_view(None);

    let effects = analysis.input(Input::Back);
    engine.settle(&mut analysis, effects);
    assert_eq!(doc(&analysis).layer_decode_selection(), requested);
    assert_eq!(doc(&analysis).filter(), filter);
    assert_eq!(
        doc(&analysis).selected().map(|selected| selected.row.index),
        Some(1)
    );

    let effects = analysis.input(Input::Forward);
    engine.settle(&mut analysis, effects);
    assert_eq!(
        doc(&analysis).layer_decode_selection(),
        LayerDecodeSelection::Default
    );
    assert!(doc(&analysis).filter().is_all());
    assert_eq!(
        doc(&analysis).selected().map(|selected| selected.row.index),
        Some(2)
    );
}

#[test]
fn block_inspection_stops_and_ignores_replies_from_an_earlier_selection() {
    use studio_domain::analysis::SourceDigest;
    use studio_domain::blocks::{BlockCoverage, BlockEvidence};

    let (mut analysis, engine) = opened(6);
    if let Phase::Open(summary) = &mut analysis.documents[0].phase {
        summary.digest = Some(Box::new(SourceDigest {
            sha256: String::new(),
            bytes: summary.fingerprint.len,
        }));
    }
    for input in [Input::RowClicked(1), Input::EvidenceView(blocks::TAB)] {
        let effects = analysis.input(input);
        engine.settle(&mut analysis, effects);
    }

    let inspection = analysis.input(Input::InspectBlocks);
    let [Command::AnalysisBlocks { document, query }] = commands(&inspection)[..] else {
        unreachable!("expected a block query");
    };
    let request = inspection[0].request_id;
    let reply = |query, coverage| Event::AnalysisBlocksReady {
        document: *document,
        evidence: Box::new(BlockEvidence {
            query,
            frame: None,
            coverage,
            replayed_through: 0,
            records: Vec::new(),
        }),
    };

    let stopping = analysis.input(Input::StopBlocks);
    assert!(
        matches!(commands(&stopping)[..], [Command::AnalysisCancelBlocks { blocks, .. }] if *blocks == request)
    );
    assert!(doc(&analysis).blocks.stopping());
    analysis.event(request, reply(*query, BlockCoverage::Cancelled));
    assert_eq!(
        doc(&analysis)
            .block_evidence()
            .map(|evidence| evidence.coverage),
        Some(BlockCoverage::Cancelled)
    );

    let earlier = analysis.input(Input::InspectBlocks);
    let effects = analysis.input(Input::RowClicked(3));
    engine.settle(&mut analysis, effects);
    let current = analysis.input(Input::InspectBlocks);
    assert!(doc(&analysis).blocks.busy());
    assert_ne!(earlier[0].request_id, current[0].request_id);
    analysis.event(
        earlier[0].request_id,
        reply(*query, BlockCoverage::Cancelled),
    );
    assert!(doc(&analysis).block_evidence().is_none());
    assert!(doc(&analysis).blocks.busy());

    analysis.event(
        current[0].request_id,
        reply(*query, BlockCoverage::Cancelled),
    );
    assert!(doc(&analysis).block_evidence().is_none());
    assert!(!doc(&analysis).blocks.busy());
}
