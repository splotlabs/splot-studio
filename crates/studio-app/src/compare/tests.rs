// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use studio_domain::analysis::SourceDigest;
use studio_domain::compare::{Clock, EncodeProvenance, SampleDomain};
use studio_domain::job::{ChromaLayout, Rational, SourceFingerprint, WirePath};
use studio_domain::layers::LayerDecodeSelection;
use studio_domain::preview::SourceRef;
use studio_domain::protocol::ErrorCode;

use super::*;
use crate::state::INITIAL_SETTINGS;

const RATE: Rational = Rational { num: 25, den: 1 };

fn fingerprint(len: u64) -> SourceFingerprint {
    SourceFingerprint {
        len,
        modified: Some((1_800_000_000, 0)),
    }
}

fn job(frames: u64, bytes: u64) -> EncodeProvenance {
    EncodeProvenance {
        source: SourceRef {
            path: WirePath::Utf8("/media/clip.y4m".into()),
            fingerprint: fingerprint(9_000),
            video_stream: 0,
        },
        codec: None,
        requested: INITIAL_SETTINGS.into(),
        effective: Vec::new(),
        frames,
        bytes,
    }
}

fn stream(name: &str, encode: Option<EncodeProvenance>) -> StreamInput {
    StreamInput {
        path: WirePath::Utf8(format!("/media/{name}")),
        selection: LayerDecodeSelection::Default,
        encode: encode.map(Box::new),
    }
}

fn facts(len: u64, outputs: u64, sha: &str) -> Event {
    Event::CompareDescribed {
        codec_revision: "658ace72".into(),
        facts: Box::new(SideFacts {
            fingerprint: fingerprint(len),
            digest: Some(SourceDigest {
                sha256: sha.repeat(32),
                bytes: len,
            }),
            outputs: Some(outputs),
            domain: Some(SampleDomain {
                width: 64,
                height: 48,
                bit_depth: 8,
                chroma: ChromaLayout::Yuv420,
                primaries: None,
                transfer: None,
                matrix: None,
                full_range: None,
                chroma_position: None,
                film_grain: false,
            }),
            clock: Clock::constant(RATE),
        }),
    }
}

/// The requests of the one `Read` in `effects`, A then B.
fn requests(effects: &[Effect]) -> [u64; 2] {
    match effects {
        [Effect::Read(requests)] => match requests.as_slice() {
            [(a, _), (b, _)] => [*a, *b],
            other => unreachable!("{other:?}"),
        },
        other => unreachable!("{other:?}"),
    }
}

fn ready(compare: &Compare) -> &ComparisonSession {
    match compare.status() {
        Status::Ready(session) => session,
        other => unreachable!("{other:?}"),
    }
}

fn failure(compare: &Compare) -> &str {
    match compare.status() {
        Status::Failed { message, .. } => message,
        other => unreachable!("{other:?}"),
    }
}

#[test]
fn an_encode_pair_reads_the_job_source_and_ignores_stale_answers() {
    let mut compare = Compare::default();
    let first = requests(&compare.pair_encode(stream("clip.ivf", Some(job(5, 700)))));
    // A second request for the same pair replaces the first read.
    let effects = compare.pair_encode(stream("clip.ivf", Some(job(5, 700))));
    match effects.as_slice() {
        [Effect::Read(sent)] => {
            assert!(matches!(
                &sent[0].1,
                DescribeInput::Source { source } if source.path == WirePath::Utf8("/media/clip.y4m".into())
            ));
            assert!(matches!(&sent[1].1, DescribeInput::Stream { .. }));
        }
        other => unreachable!("{other:?}"),
    }
    let [a, b] = requests(&effects);
    assert!(compare.reading());
    for stale in first {
        assert_eq!(compare.event(stale, facts(1, 1, "00")), []);
    }
    assert_eq!(compare.event(a, facts(9_000, 5, "aa")), []);
    assert!(compare.reading());
    assert_eq!(compare.event(b, facts(700, 5, "bb")), [Effect::Stop]);
    assert!(!compare.reading());
    let session = ready(&compare);
    assert_eq!(session.setup.basis, AlignmentBasis::Provenance);
    assert_eq!(session.alignment().map(|m| m.matched()), Ok(5));
    assert_eq!(compare.notice(), None);
    let saved = compare.saved();
    assert!(saved.as_ref().is_some_and(|s| s.identity.is_some()));
    // An answer after the pair is ready changes nothing.
    assert_eq!(compare.event(b, facts(1, 1, "00")), []);
    assert_eq!(compare.saved(), saved);
}

#[test]
fn a_changed_source_or_result_stops_an_encode_pair() {
    let mut compare = Compare::default();
    let [a, _] = requests(&compare.pair_encode(stream("clip.ivf", Some(job(5, 700)))));
    let changed = Event::CompareFailed {
        code: AnalysisFailure::SourceChanged,
        message: "the file changed".into(),
    };
    assert_eq!(compare.event(a, changed), [Effect::Stop]);
    assert_eq!(
        failure(&compare),
        "The source changed after the encode. Studio cannot verify it."
    );

    let [a, b] = requests(&compare.pair_encode(stream("clip.ivf", Some(job(5, 700)))));
    compare.event(a, facts(9_000, 5, "aa"));
    // The file at the destination is not what the job published.
    compare.event(b, facts(650, 5, "bb"));
    assert_eq!(
        failure(&compare),
        "The file of side B is not the file its encode job published."
    );

    let [_, b] = requests(&compare.pair_encode(stream("clip.ivf", Some(job(5, 700)))));
    let unreadable = Event::CompareFailed {
        code: AnalysisFailure::Io,
        message: "Cannot open the file.".into(),
    };
    compare.event(b, unreadable);
    assert_eq!(
        failure(&compare),
        "Studio cannot read side B. Cannot open the file."
    );
    let [a, _] = requests(&compare.pair_encode(stream("clip.ivf", Some(job(5, 700)))));
    let refused = Event::Error {
        code: ErrorCode::Busy,
        message: "The inspection queue is full.".into(),
    };
    compare.event(a, refused);
    assert!(failure(&compare).starts_with("The engine refused the request."));
    // A failed pair is still what the journal records.
    assert!(compare.saved().is_some());
}

#[test]
fn picked_files_wait_for_both_sides_and_keep_only_verified_jobs() {
    let mut compare = Compare::default();
    assert_eq!(
        compare.pick(Side::B, stream("two.ivf", Some(job(5, 600)))),
        [Effect::Stop]
    );
    assert!(matches!(compare.status(), Status::Empty([None, Some(_)])));
    assert_eq!(compare.saved(), None);
    let [a, b] = requests(&compare.pick(Side::A, stream("one.ivf", Some(job(5, 500)))));
    // Two encodes of one source: the job's frame order aligns them.
    assert_eq!(
        compare.status().setup().map(|setup| setup.basis),
        Some(AlignmentBasis::Provenance)
    );
    compare.event(a, facts(500, 5, "aa"));
    // Side B was replaced after its job: it keeps no settings.
    compare.event(b, facts(600, 4, "bb"));
    let session = ready(&compare);
    assert!(session.setup.encode(Side::A).is_some());
    assert_eq!(session.setup.encode(Side::B), None);
    assert_eq!(session.setup.basis, AlignmentBasis::Timestamps);
    assert_eq!(
        compare.notice(),
        Some(
            "The file of side B is not the file that its encode job published. Studio does not use the settings of that job."
        )
    );

    // Replacing one side reads the pair again and keeps the other side.
    let [a, b] = requests(&compare.pick(Side::B, stream("three.obu", None)));
    assert_eq!(compare.notice(), None);
    compare.event(a, facts(500, 5, "aa"));
    compare.event(b, facts(900, 7, "cc"));
    let session = ready(&compare);
    assert_eq!(
        session.setup.b.path,
        WirePath::Utf8("/media/three.obu".into())
    );
    assert_eq!(
        session
            .alignment()
            .map(|m| (m.matched(), m.unmatched(Side::B))),
        Ok((5, 2))
    );

    assert_eq!(compare.close(), [Effect::Stop]);
    assert!(matches!(compare.status(), Status::Empty([None, None])));
    assert_eq!(compare.saved(), None);
}

#[test]
fn a_restored_pair_keeps_its_policy_and_reports_a_changed_identity() -> serde_json::Result<()> {
    let mut compare = Compare::default();
    let [a, b] = requests(
        &compare
            .pick(Side::A, stream("one.ivf", None))
            .into_iter()
            .chain(compare.pick(Side::B, stream("two.ivf", None)))
            .filter(|effect| matches!(effect, Effect::Read(_)))
            .collect::<Vec<_>>(),
    );
    compare.event(a, facts(500, 5, "aa"));
    compare.event(b, facts(600, 5, "bb"));
    // Session edits: they change the saved policy and the evidence identity.
    let before = ready(&compare).identity();
    compare.set_basis(AlignmentBasis::AssumedOffset { offset: -1 });
    compare.set_grain(GrainPolicy::Skip);
    assert!(compare.take_dirty());
    assert_ne!(ready(&compare).identity(), before);
    let saved = compare.saved();
    let stored = serde_json::to_string(&saved)?;
    let restored: Option<SavedComparison> = serde_json::from_str(&stored)?;
    assert_eq!(restored, saved);
    assert!(restored.as_ref().is_some_and(SavedComparison::is_native));

    for (sha, changed) in [("bb", false), ("cc", true)] {
        let mut again = Compare::restored(restored.clone());
        assert!(!again.reading());
        assert_eq!(again.saved(), saved, "nothing is lost before the read");
        let [a, b] = requests(&again.read_saved());
        assert_eq!(again.read_saved(), [], "one read");
        again.event(a, facts(500, 5, "aa"));
        again.event(b, facts(600, 5, sha));
        let session = ready(&again);
        assert_eq!(
            session.setup.basis,
            AlignmentBasis::AssumedOffset { offset: -1 }
        );
        assert_eq!(session.setup.grain, GrainPolicy::Skip);
        assert_eq!(again.notice().is_some(), changed, "{sha}");
        assert_eq!(again.saved() == saved, !changed);
    }
    Ok(())
}

#[test]
fn a_lost_engine_fails_the_read_once() {
    let mut compare = Compare::default();
    assert_eq!(compare.interrupted("exit status: 1"), []);
    let _ = compare.pair_encode(stream("clip.ivf", Some(job(5, 700))));
    assert_eq!(compare.interrupted("exit status: 1"), [Effect::Stop]);
    assert_eq!(
        failure(&compare),
        "The engine stopped before it read both files. exit status: 1"
    );
    assert_eq!(compare.interrupted("again"), []);
    // A source without a job cannot be asked for.
    let mut orphan = Compare::default();
    assert_eq!(orphan.pair_encode(stream("clip.ivf", None)), [Effect::Stop]);
    assert_eq!(
        failure(&orphan),
        "Side B has no recorded encode job. Studio cannot identify its source."
    );
}
