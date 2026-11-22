// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Documents across reads: restored from the journal, read again only for
//! the same file, kept with their view when a read is cancelled, fails or
//! loses its engine.

use studio_domain::analysis::Problem;

use crate::recovery::RestoredView;

use super::*;

/// A disk where every path is `kind`.
struct Everything(Kind);

impl Disk for Everything {
    fn kind(&self, _: &std::path::Path) -> Kind {
        self.0
    }
}

/// A disk where every path is a file with this size and modification time.
struct Stat(SourceFingerprint);

impl Disk for Stat {
    fn kind(&self, _: &std::path::Path) -> Kind {
        Kind::File
    }

    fn fingerprint(&self, _: &std::path::Path) -> Option<SourceFingerprint> {
        Some(self.0)
    }
}

const PATH: &str = "/media/clip.obu";

/// The Analyze documents a journal restored.
fn restored(documents: Vec<Saved>, shown: usize, dropped: usize) -> Analysis {
    Analysis::restored(RestoredView {
        documents,
        shown,
        dropped,
        ..RestoredView::default()
    })
}

/// The fingerprint the scripted engine reports for `n` OBUs.
fn fingerprint(n: u64) -> SourceFingerprint {
    summary(n).fingerprint
}

fn restore(n: u64, selected: Option<u64>, filter: RowFilter) -> Restore {
    Restore {
        fingerprint: fingerprint(n),
        selected,
        filter,
        bytes_shown: false,
    }
}

fn saved(restore: Option<Restore>) -> Saved {
    Saved {
        path: PathBuf::from(PATH),
        origin: Origin::File,
        restore,
    }
}

fn type_7() -> RowFilter {
    RowFilter {
        kind: KindFilter::Obu { obu_type: 7 },
        layer: None,
    }
}

fn opens(effects: &[Effect]) -> Vec<u64> {
    commands(effects)
        .iter()
        .filter_map(|c| match c {
            Command::AnalysisOpen { document, .. } => Some(*document),
            _ => None,
        })
        .collect()
}

#[test]
fn a_restored_document_shows_nothing_until_it_is_read_again_from_the_same_file() {
    let engine = Engine { rows: stream(200) };
    let mut analysis = restored(vec![saved(Some(restore(200, Some(151), type_7())))], 0, 0);
    assert_eq!(*doc(&analysis).phase(), Phase::Unread);
    assert!(doc(&analysis).summary().is_none());
    assert!(!analysis.needs_engine());
    // Saving again before the read keeps the record as it was.
    assert_eq!(
        analysis.saved().0,
        [saved(Some(restore(200, Some(151), type_7())))]
    );

    let effects = analysis.read_shown(&Everything(Kind::File));
    assert_eq!(opens(&effects), [1]);
    assert!(analysis.needs_engine());
    engine.settle(&mut analysis, effects);
    let doc = doc(&analysis);
    assert!(doc.summary().is_some());
    assert_eq!(doc.filter(), type_7());
    assert!(!doc.bytes().shown);
    // OBU 151 is type 7, row 75 of the type-7 rows.
    assert_eq!(selected(&analysis), Some((151, Position::Row(75))));
    // Read once: a second call reads nothing.
    assert_eq!(analysis.read_shown(&Everything(Kind::File)).len(), 0);
}

#[test]
fn a_changed_file_waits_until_it_is_read_as_new() {
    let engine = Engine { rows: stream(200) };
    // Recorded at another length: not the same file.
    let old = restore(120, Some(3), type_7());
    let mut analysis = restored(vec![saved(Some(old))], 0, 0);
    let effects = analysis.read_shown(&Everything(Kind::File));
    let after_open = engine.answer(&mut analysis, effects);
    // Nothing of the new read is asked for or shown: the engine gives it
    // back.
    assert!(
        matches!(
            commands(&after_open)[..],
            [Command::AnalysisClose { document: 1 }]
        ),
        "{after_open:?}"
    );
    assert_eq!(
        *doc(&analysis).phase(),
        Phase::Changed {
            now: fingerprint(200)
        }
    );
    assert!(doc(&analysis).summary().is_none());
    assert!(doc(&analysis).selected().is_none());
    assert!(!analysis.needs_engine());
    // The journal keeps the old view, and Jobs has no read of it.
    assert_eq!(analysis.saved().0, [saved(Some(old))]);
    let state = crate::state::AppState::default();
    assert!(crate::jobs::list(&state, &analysis).is_empty());
    engine.settle(&mut analysis, after_open);

    let effects = analysis.input(Input::Resolve);
    engine.settle(&mut analysis, effects);
    assert!(doc(&analysis).summary().is_some());
    assert_eq!(doc(&analysis).filter(), RowFilter::default());
    assert!(doc(&analysis).selected().is_none());
    assert_eq!(
        analysis.saved().0[0].restore.map(|r| r.fingerprint),
        Some(fingerprint(200))
    );
}

/// A size or modification time that already differs shows the change
/// without a read of the file; Read as New then reads it.
#[test]
fn a_changed_size_or_time_waits_without_a_read() {
    let engine = Engine { rows: stream(200) };
    let old = restore(200, Some(3), type_7());
    let later = SourceFingerprint {
        modified: Some((1, 0)),
        ..fingerprint(200)
    };
    for now in [fingerprint(120), later] {
        let mut analysis = restored(vec![saved(Some(old))], 0, 0);
        assert_eq!(analysis.read_shown(&Stat(now)), []);
        assert_eq!(*doc(&analysis).phase(), Phase::Changed { now });
        assert!(!analysis.needs_engine());
        assert_eq!(analysis.saved().0, [saved(Some(old))]);
        // Closing it has no read to close.
        let mut closed = restored(vec![saved(Some(old))], 0, 0);
        closed.read_shown(&Stat(now));
        assert_eq!(closed.input(Input::Close), []);

        let effects = analysis.input(Input::Resolve);
        assert_eq!(opens(&effects), [1]);
        engine.settle(&mut analysis, effects);
        assert!(doc(&analysis).summary().is_some());
        assert_eq!(doc(&analysis).filter(), RowFilter::default());
        assert!(doc(&analysis).selected().is_none());
    }
    // The same size and time: it is read, and the engine's reading decides.
    let mut analysis = restored(vec![saved(Some(old))], 0, 0);
    let effects = analysis.read_shown(&Stat(fingerprint(200)));
    engine.settle(&mut analysis, effects);
    assert_eq!(selected(&analysis).map(|s| s.0), Some(3));
}

/// Only the shown document is read; another one is read when it is shown,
/// so a long read of a document not shown never delays the shown one.
#[test]
fn only_the_shown_document_is_read() {
    let engine = Engine { rows: stream(20) };
    let at = |name: &str| Saved {
        path: PathBuf::from(name),
        ..saved(Some(restore(20, Some(2), RowFilter::default())))
    };
    let mut analysis = restored(vec![at("/a"), at("/b"), at("/c")], 1, 0);
    let effects = analysis.read_shown(&Everything(Kind::File));
    assert_eq!(opens(&effects), [1]);
    let phases: Vec<_> = analysis.documents().iter().map(Document::phase).collect();
    assert_eq!(phases, [&Phase::Unread, &Phase::Opening, &Phase::Unread]);
    engine.settle(&mut analysis, effects);
    assert_eq!(selected(&analysis).map(|s| s.0), Some(2));
    // Reading again reads nothing.
    assert_eq!(analysis.read_shown(&Everything(Kind::File)), []);
    // Shown: read.
    analysis.input(Input::Show(2));
    let effects = analysis.read_shown(&Everything(Kind::File));
    assert_eq!(opens(&effects), [2]);
    assert_eq!(*analysis.documents()[0].phase(), Phase::Unread);
}

/// Closing the shown document shows the next one; when it is not read yet,
/// it is read.
#[test]
fn closing_the_shown_document_reads_the_next_one() {
    let engine = Engine { rows: stream(20) };
    let at = |name: &str| Saved {
        path: PathBuf::from(name),
        ..saved(None)
    };
    let mut analysis = restored(vec![at("/a"), at("/b"), at("/c")], 0, 0);
    let effects = analysis.read_shown(&Everything(Kind::File));
    engine.settle(&mut analysis, effects);
    analysis.input(Input::Close);
    assert_eq!(doc(&analysis).path(), std::path::Path::new("/b"));
    let effects = analysis.read_shown(&Everything(Kind::File));
    assert_eq!(opens(&effects), [2]);
    assert_eq!(*analysis.documents()[1].phase(), Phase::Unread);
}

/// The engine stops while one document is read and another one waits as
/// changed: only the read in the engine ends, as interrupted.
#[test]
fn an_engine_stop_ends_only_the_reads_it_held() {
    use crate::jobs::{self, State};
    let engine = Engine { rows: stream(10) };
    let changed = saved(Some(restore(99, None, type_7())));
    let mut analysis = restored(vec![changed.clone()], 0, 0);
    let effects = analysis.read_shown(&Everything(Kind::File));
    engine.settle(&mut analysis, effects);
    // Closing it has no read to close.
    let mut closed = restored(vec![changed], 0, 0);
    let effects = closed.read_shown(&Everything(Kind::File));
    engine.settle(&mut closed, effects);
    assert_eq!(closed.input(Input::Close), []);

    analysis.input(Input::Open(vec![PathBuf::from("/media/b.obu")]));
    let state = crate::state::AppState::default();
    let states = |analysis: &Analysis| -> Vec<State> {
        jobs::list(&state, analysis)
            .iter()
            .map(|j| j.state)
            .collect()
    };
    assert_eq!(states(&analysis), [State::Running]);
    analysis.process_ended("signal: 9");
    assert_eq!(states(&analysis), [State::Interrupted]);
    assert!(matches!(
        analysis.documents()[0].phase(),
        Phase::Changed { .. }
    ));
}

#[test]
fn no_modification_time_is_never_the_same_file() {
    let with = |modified| SourceFingerprint { len: 9, modified };
    assert!(same_file(with(Some((1, 2))), with(Some((1, 2)))));
    assert!(!same_file(with(Some((1, 2))), with(Some((1, 3)))));
    assert!(!same_file(with(None), with(None)));
    assert!(!same_file(
        with(Some((1, 2))),
        SourceFingerprint {
            len: 10,
            modified: Some((1, 2))
        }
    ));
}

#[test]
fn a_missing_file_waits_for_its_new_place_and_is_checked_there() {
    let engine = Engine { rows: stream(200) };
    let mut analysis = restored(vec![saved(Some(restore(200, Some(8), type_7())))], 0, 0);
    // Nothing at the path: no process is needed and nothing is read.
    let effects = analysis.read_shown(&Everything(Kind::Missing));
    assert_eq!(effects.len(), 0);
    assert_eq!(*doc(&analysis).phase(), Phase::Missing);
    assert!(!analysis.needs_engine());
    // The file located there is checked as a restored one is.
    let effects = analysis.input(Input::Relink {
        missing: PathBuf::from(PATH),
        to: PathBuf::from("/moved/clip.obu"),
    });
    assert_eq!(effects.len(), 0);
    let effects = analysis.read_shown(&Everything(Kind::File));
    assert_eq!(opens(&effects), [1]);
    engine.settle(&mut analysis, effects);
    assert_eq!(
        doc(&analysis).path(),
        std::path::Path::new("/moved/clip.obu")
    );
    // The same file: its view is back (OBU 8 is type 2, so the type-7
    // filter hides it).
    assert_eq!(selected(&analysis), Some((8, Position::Hidden)));
    assert_eq!(doc(&analysis).filter(), type_7());
}

/// The scripted engine's summary of `n` OBUs, stopped by a cancel after
/// `kept` of them.
fn cancelled_after(n: u64, kept: u64) -> DocumentSummary {
    DocumentSummary {
        elements: kept + 1,
        obus: kept,
        unparsed: 1,
        outcome: ScanOutcome::Stopped {
            problem: Problem {
                kind: ProblemKind::Cancelled,
                offset: kept * 9,
                message: "reading was cancelled".into(),
            },
        },
        ..summary(n)
    }
}

#[test]
fn a_cancelled_read_keeps_the_document_and_what_it_read() {
    let engine = Engine { rows: stream(40) };
    let mut analysis = Analysis::new();
    let open = analysis.input(Input::Open(vec![PathBuf::from(PATH)]));
    let cancel = analysis.input(Input::CancelDocument(0));
    assert!(matches!(
        commands(&cancel)[..],
        [Command::AnalysisCancel { document: 1 }]
    ));
    assert_eq!(*doc(&analysis).phase(), Phase::Cancelling);
    let next = analysis.event(
        open[0].request_id,
        Event::AnalysisOpened {
            document: 1,
            summary: cancelled_after(40, 20),
        },
    );
    engine.settle(&mut analysis, next);
    assert_eq!(analysis.documents().len(), 1);
    assert_eq!(doc(&analysis).summary().map(|s| s.elements), Some(21));
    assert!(analysis.can_read_again());
    let effects = analysis.input(Input::RowClicked(10));
    engine.settle(&mut analysis, effects);

    // Read Again: the part read is closed, the whole file read, and the
    // selection comes back on it.
    let effects = analysis.input(Input::Reopen);
    assert!(matches!(
        commands(&effects)[..],
        [
            Command::AnalysisClose { document: 1 },
            Command::AnalysisOpen { document: 2, .. }
        ]
    ));
    engine.settle(&mut analysis, effects);
    assert_eq!(doc(&analysis).summary().map(|s| s.elements), Some(40));
    assert!(!analysis.can_read_again());
    assert_eq!(selected(&analysis), Some((10, Position::Row(10))));
}

#[test]
fn a_cancel_before_anything_is_read_keeps_the_document_to_reopen() {
    let engine = Engine { rows: stream(40) };
    let mut analysis = Analysis::new();
    let open = analysis.input(Input::Open(vec![PathBuf::from(PATH)]));
    analysis.input(Input::Cancel);
    analysis.event(
        open[0].request_id,
        Event::AnalysisFailed {
            document: 1,
            code: AnalysisFailure::Cancelled,
            message: "the open was cancelled".into(),
        },
    );
    assert!(matches!(
        doc(&analysis).phase(),
        Phase::Failed {
            by: ReadEnd::Cancel,
            ..
        }
    ));
    assert!(!analysis.needs_engine());
    let effects = analysis.input(Input::Reopen);
    engine.settle(&mut analysis, effects);
    assert!(doc(&analysis).summary().is_some());
}

#[test]
fn a_dead_engine_keeps_the_view_for_the_next_read() {
    let (mut analysis, engine) = opened(200);
    let effects = analysis.input(Input::TypeFilter(2));
    engine.settle(&mut analysis, effects);
    let effects = analysis.input(Input::RowClicked(4));
    engine.settle(&mut analysis, effects);
    assert_eq!(selected(&analysis).map(|s| s.0), Some(9));
    analysis.process_ended("signal: 9");
    assert!(matches!(
        doc(&analysis).phase(),
        Phase::Failed {
            by: ReadEnd::EngineStopped,
            ..
        }
    ));
    assert!(!analysis.needs_engine());
    let kept = analysis.saved().0[0].restore;
    assert_eq!(
        kept.map(|r| (r.selected, r.filter)),
        Some((Some(9), type_7()))
    );
    let effects = analysis.input(Input::Reopen);
    engine.settle(&mut analysis, effects);
    assert_eq!(selected(&analysis), Some((9, Position::Row(4))));
    assert_eq!(doc(&analysis).filter(), type_7());
}

#[test]
fn a_restored_view_the_file_does_not_offer_is_not_applied() {
    let engine = Engine { rows: stream(20) };
    let odd = RowFilter {
        kind: KindFilter::Obu { obu_type: 30 },
        layer: None,
    };
    let mut analysis = restored(vec![saved(Some(restore(20, Some(20), odd)))], 0, 0);
    let effects = analysis.read_shown(&Everything(Kind::File));
    let next = engine.answer(&mut analysis, effects);
    // No locate for an element past the end.
    assert!(
        !commands(&next)
            .iter()
            .any(|c| matches!(c, Command::AnalysisLocate { .. })),
        "{next:?}"
    );
    engine.settle(&mut analysis, next);
    assert_eq!(doc(&analysis).filter(), RowFilter::default());
    assert!(doc(&analysis).selected().is_none());
}

#[test]
fn at_most_four_documents_are_restored_each_once() {
    let at = |name: &str| Saved {
        path: PathBuf::from(name),
        origin: Origin::File,
        restore: None,
    };
    let analysis = restored(
        vec![at("/a"), at("/a"), at("/b"), at("/c"), at("/d"), at("/e")],
        9,
        2,
    );
    let paths: Vec<_> = analysis.documents().iter().map(Document::path).collect();
    assert_eq!(paths, ["/a", "/b", "/c", "/d"].map(std::path::Path::new));
    // Past the end: the first.
    assert_eq!(analysis.shown_index(), 0);
    // The shown one stays shown when a twin before it is dropped.
    let twins = restored(vec![at("/a"), at("/a"), at("/b")], 2, 0);
    assert_eq!(doc(&twins).path(), std::path::Path::new("/b"));
    // Every document not restored is counted: the 2 foreign paths of the
    // journal, the twin and the fifth.
    assert!(
        analysis
            .notice_line()
            .is_some_and(|n| n.starts_with("4 documents")),
        "{:?}",
        analysis.notice_line()
    );
    assert!(
        twins
            .notice_line()
            .is_some_and(|n| n.starts_with("1 document of")),
        "{:?}",
        twins.notice_line()
    );
}

#[test]
fn the_engine_is_needed_only_while_a_document_is_in_it() {
    let (mut analysis, engine) = opened(30);
    assert!(analysis.needs_engine());
    let pending = analysis.input(Input::ByteClicked(3));
    assert!(analysis.waiting());
    let effects = analysis.input(Input::Close);
    assert!(!analysis.needs_engine());
    analysis.engine_released();
    assert!(!analysis.waiting());
    // Answers of the released process reach nobody.
    assert_eq!(engine.answer(&mut analysis, pending).len(), 0);
    assert_eq!(engine.answer(&mut analysis, effects).len(), 0);
}

#[test]
fn storage_problems_are_noticed_until_they_clear() {
    let mut analysis = Analysis::new();
    analysis.set_storage_problem(Some("read-only".into()));
    assert_eq!(analysis.notice_line(), Some("read-only"));
    analysis.set_storage_problem(None);
    assert_eq!(analysis.notice_line(), None);
}

/// Jobs shows each document's read: the engine reads one file at a time,
/// so the first pending read runs and later ones are queued. A document
/// with no read (not read yet, missing) has no job.
#[test]
fn jobs_list_each_read_with_its_own_state() {
    use crate::jobs::{self, State};
    let states = |analysis: &Analysis| -> Vec<State> {
        jobs::list(&crate::state::AppState::default(), analysis)
            .iter()
            .map(|j| j.state)
            .collect()
    };
    let mut analysis = restored(
        vec![
            saved(None),
            Saved {
                path: PathBuf::from("/media/gone.obu"),
                ..saved(None)
            },
        ],
        0,
        0,
    );
    assert_eq!(states(&analysis), []);
    analysis.documents[1].phase = Phase::Missing;
    let first = analysis.read_shown(&Everything(Kind::File));
    let second = analysis.input(Input::Open(vec![PathBuf::from("/media/b.obu")]));
    let third = analysis.input(Input::Open(vec![PathBuf::from("/media/c.obu")]));
    assert_eq!(
        states(&analysis),
        [State::Running, State::Queued, State::Queued]
    );
    analysis.input(Input::CancelDocument(3));
    assert_eq!(
        states(&analysis),
        [State::Running, State::Queued, State::Cancelling]
    );
    // The first ends with a read failure part way; the third is cancelled
    // with what it read; then the engine dies, and every document it held
    // is interrupted.
    let stopped = |kind| DocumentSummary {
        outcome: ScanOutcome::Stopped {
            problem: Problem {
                kind,
                offset: 9,
                message: "stopped".into(),
            },
        },
        ..summary(3)
    };
    analysis.event(
        first[0].request_id,
        Event::AnalysisOpened {
            document: 1,
            summary: stopped(ProblemKind::Io),
        },
    );
    analysis.event(
        third[0].request_id,
        Event::AnalysisOpened {
            document: 3,
            summary: stopped(ProblemKind::Cancelled),
        },
    );
    assert_eq!(
        states(&analysis),
        [State::Failed, State::Running, State::Cancelled]
    );
    drop(second);
    analysis.process_ended("signal: 9");
    assert_eq!(
        states(&analysis),
        [State::Interrupted, State::Interrupted, State::Interrupted]
    );
    // A whole read is complete; one that stopped where the file does not
    // parse is not, and says so in words as well as in its tone.
    let (analysis, _) = opened(3);
    assert_eq!(states(&analysis), [State::Completed]);
    for kind in [ProblemKind::Malformed, ProblemKind::Unsupported] {
        let mut analysis = Analysis::new();
        let open = analysis.input(Input::Open(vec![PathBuf::from(PATH)]));
        analysis.event(
            open[0].request_id,
            Event::AnalysisOpened {
                document: 1,
                summary: stopped(kind),
            },
        );
        assert_eq!(states(&analysis), [State::Stopped]);
        let state = crate::state::AppState::default();
        let line = crate::present::jobs::line(&jobs::list(&state, &analysis)[0]);
        assert_eq!(line.state, "Stopped");
        assert_eq!(line.tone, crate::present::Tone::Warning);
        assert!(line.detail.contains("stopped at byte 9"), "{}", line.detail);
    }
}

/// A read again that stops before the restored selection keeps the whole
/// restored view for the journal and the next read, until the user
/// chooses a selection of their own.
#[test]
fn a_read_that_stops_early_keeps_the_restored_view() {
    let engine = Engine { rows: stream(200) };
    let old = restore(200, Some(151), type_7());
    let mut analysis = restored(vec![saved(Some(old))], 0, 0);
    let open = analysis.read_shown(&Everything(Kind::File));
    // The read is cancelled after 20 OBUs, long before OBU 151.
    let next = analysis.event(
        open[0].request_id,
        Event::AnalysisOpened {
            document: 1,
            summary: cancelled_after(200, 20),
        },
    );
    engine.settle(&mut analysis, next);
    assert!(doc(&analysis).selected().is_none());
    assert_eq!(doc(&analysis).filter(), type_7());
    assert_eq!(analysis.saved().0, [saved(Some(old))]);
    // Read Again reads the whole file, and the selection comes back.
    let effects = analysis.input(Input::Reopen);
    engine.settle(&mut analysis, effects);
    assert_eq!(selected(&analysis).map(|s| s.0), Some(151));
    // A choice of the user's own replaces the restored view.
    let mut analysis = restored(vec![saved(Some(old))], 0, 0);
    let open = analysis.read_shown(&Everything(Kind::File));
    let next = analysis.event(
        open[0].request_id,
        Event::AnalysisOpened {
            document: 1,
            summary: cancelled_after(200, 20),
        },
    );
    engine.settle(&mut analysis, next);
    let effects = analysis.input(Input::RowClicked(2));
    engine.settle(&mut analysis, effects);
    let now = analysis.saved().0[0].restore.and_then(|r| r.selected);
    assert_eq!(now, selected(&analysis).map(|s| s.0));
    assert!(now.is_some_and(|i| i < 20), "{now:?}");
}

/// Input that changes neither selection nor filter keeps a restored view
/// the read could not apply; a selection by byte, answered later, replaces
/// it.
#[test]
fn only_a_real_choice_replaces_the_kept_view() {
    let engine = Engine { rows: stream(200) };
    let old = restore(200, Some(151), type_7());
    let mut analysis = restored(vec![saved(Some(old))], 0, 0);
    let open = analysis.read_shown(&Everything(Kind::File));
    let next = analysis.event(
        open[0].request_id,
        Event::AnalysisOpened {
            document: 1,
            summary: cancelled_after(200, 20),
        },
    );
    engine.settle(&mut analysis, next);
    let applied = analysis
        .type_choices()
        .iter()
        .position(|k| *k == type_7().kind)
        .and_then(|i| i32::try_from(i).ok())
        .unwrap_or(-1);
    for input in [
        Input::Back,
        Input::Forward,
        Input::GoTo("not an offset".into()),
        Input::RowClicked(99),
        Input::TypeFilter(applied),
        Input::Scroll(3),
        Input::ToggleBytes,
    ] {
        let effects = analysis.input(input.clone());
        engine.settle(&mut analysis, effects);
        let kept = analysis.saved().0[0].restore;
        assert_eq!(kept.and_then(|r| r.selected), Some(151), "{input:?}");
        assert_eq!(kept.map(|r| r.filter), Some(type_7()), "{input:?}");
    }
    // A byte selects asynchronously, through a locate: that is a choice.
    let effects = analysis.input(Input::ByteClicked(20));
    engine.settle(&mut analysis, effects);
    let kept = analysis.saved().0[0].restore.and_then(|r| r.selected);
    assert_eq!(kept, selected(&analysis).map(|s| s.0));
    assert!(kept.is_some_and(|i| i != 151), "{kept:?}");
}
