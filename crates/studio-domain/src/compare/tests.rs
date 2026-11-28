// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use std::collections::BTreeMap;

use super::*;
use crate::job::Rational;
use crate::options::OptionValue;
use crate::settings::{Crf, EncoderSettings, OutputKind, SpeedPreset};

fn fingerprint(len: u64) -> SourceFingerprint {
    SourceFingerprint {
        len,
        modified: Some((1_800_000_000, 0)),
    }
}

fn settings() -> EncoderSettings {
    EncoderSettings {
        output: OutputKind::Ivf,
        preset: SpeedPreset::new(6).unwrap_or_else(|_| unreachable!()),
        crf: Crf::new(20).unwrap_or_else(|_| unreachable!()),
    }
}

fn source() -> SourceRef {
    SourceRef {
        path: WirePath::Utf8("/media/clip.y4m".into()),
        fingerprint: fingerprint(9_000),
        video_stream: 0,
    }
}

fn encode(frames: u64, bytes: u64) -> EncodeProvenance {
    EncodeProvenance {
        source: source(),
        codec: Some(OptionsIdentity {
            schema: 1,
            codec_revision: "658ace72".into(),
        }),
        requested: settings().into(),
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

fn domain() -> SampleDomain {
    SampleDomain {
        width: 64,
        height: 48,
        bit_depth: 8,
        chroma: ChromaLayout::Yuv420,
        primaries: Some(1),
        transfer: Some(1),
        matrix: Some(1),
        full_range: Some(false),
        chroma_position: Some((0, 0)),
        film_grain: false,
    }
}

fn facts(len: u64, outputs: u64, clock: Clock) -> SideFacts {
    SideFacts {
        fingerprint: fingerprint(len),
        digest: Some(SourceDigest {
            sha256: "ab".repeat(32),
            bytes: len,
        }),
        outputs: Some(outputs),
        domain: Some(domain()),
        clock,
    }
}

const RATE: Rational = Rational { num: 25, den: 1 };

fn clock(tick: (u32, u32), ticks: impl IntoIterator<Item = u64>) -> Clock {
    let mut builder = ClockBuilder::new(Rational {
        num: tick.0,
        den: tick.1,
    });
    for pts in ticks {
        builder.push(pts);
    }
    builder.finish()
}

fn track(outputs: u64, clock: &Clock) -> Track<'_> {
    Track { outputs, clock }
}

/// A source and the result of its encode job, with `frames` frames.
fn encode_pair(frames: u64) -> (ComparisonSetup, SideFacts, SideFacts) {
    let setup = ComparisonSetup::new(
        Reference::EncodeSource,
        stream("clip.ivf", Some(encode(frames, 700))),
    );
    let mut a = facts(9_000, frames, Clock::constant(RATE));
    a.digest = None;
    let b = facts(700, frames, clock((1, 25), 0..frames));
    (setup, a, b)
}

fn resolved(setup: ComparisonSetup, a: SideFacts, b: SideFacts) -> ComparisonSession {
    ComparisonSession::resolve(setup, "658ace72".into(), a, b)
        .unwrap_or_else(|e| unreachable!("{e}"))
}

#[test]
fn an_encode_pairs_with_the_source_its_job_recorded() {
    let (setup, a, b) = encode_pair(10);
    assert_eq!(setup.basis, AlignmentBasis::Provenance);
    // The engine is asked for the job's own source: nothing is imported again.
    assert_eq!(
        setup.describe(Side::A),
        Ok(DescribeInput::Source { source: source() })
    );
    let session = resolved(setup, a, b);
    assert_eq!(
        session.alignment(),
        Ok(Mapping {
            segments: vec![Segment::Matched {
                a: 0,
                b: 0,
                count: 10
            }]
        })
    );
    assert_eq!(session.timestamps_agree(), Some(true));
    assert_eq!(
        session.compatibility(),
        Compatibility::Exact {
            undeclared: Vec::new()
        }
    );
}

#[test]
fn a_replaced_source_or_result_is_refused() {
    let (setup, mut a, b) = encode_pair(10);
    a.fingerprint.modified = Some((1_800_000_001, 0));
    assert_eq!(
        ComparisonSession::resolve(setup.clone(), "r".into(), a, b.clone()),
        Err(PairingRefusal::SourceChanged)
    );

    // A source that counts another number of frames is not what the job read.
    let (_, mut longer, _) = encode_pair(10);
    longer.outputs = Some(11);
    assert_eq!(
        ComparisonSession::resolve(setup.clone(), "r".into(), longer, b.clone()),
        Err(PairingRefusal::SourceChanged)
    );
    let (_, mut uncounted, _) = encode_pair(10);
    uncounted.outputs = None;
    let session = ComparisonSession::resolve(setup.clone(), "r".into(), uncounted, b.clone());
    assert_eq!(session.map(|s| s.outputs(Side::A)), Ok(10));

    let (_, a, _) = encode_pair(10);
    for changed in [
        facts(701, 10, Clock::Absent),
        facts(700, 9, Clock::Absent),
        SideFacts {
            outputs: None,
            ..b.clone()
        },
    ] {
        let refused = ComparisonSession::resolve(setup.clone(), "r".into(), a.clone(), changed);
        assert!(
            matches!(
                refused,
                Err(PairingRefusal::ResultChanged(Side::B) | PairingRefusal::Uncounted(Side::B))
            ),
            "{refused:?}"
        );
    }

    // A source cannot be paired with a stream that has no encode job.
    let orphan = ComparisonSetup::new(Reference::EncodeSource, stream("other.ivf", None));
    assert_eq!(orphan.describe(Side::A), Err(PairingRefusal::NoEncode));
    assert_eq!(
        ComparisonSession::resolve(orphan, "r".into(), a, b),
        Err(PairingRefusal::NoEncode)
    );
}

#[test]
fn unrelated_results_pair_without_invented_metadata() {
    let setup = ComparisonSetup::new(
        Reference::Stream(stream("one.ivf", None)),
        stream("two.obu", None),
    );
    assert_eq!(setup.basis, AlignmentBasis::Timestamps);
    assert_eq!(setup.common_source(), None);
    let session = resolved(
        setup,
        facts(500, 4, clock((1, 25), 0..4)),
        facts(600, 4, Clock::Absent),
    );
    assert_eq!(
        session.configuration(),
        ConfigComparison::Unknown { a: false, b: false }
    );
    // Missing timestamps are refused; nothing falls back to an index.
    assert_eq!(
        session.alignment(),
        Err(AlignmentRefusal::NoTimestamps(Side::B))
    );
    let mut provenance = session.clone();
    provenance.setup.basis = AlignmentBasis::Provenance;
    assert_eq!(provenance.alignment(), Err(AlignmentRefusal::NoProvenance));
    // The labelled assumption maps them and stays in the session.
    let mut assumed = session;
    assumed.setup.basis = AlignmentBasis::AssumedOffset { offset: 0 };
    assert_eq!(assumed.alignment().map(|m| m.matched()), Ok(4));
    assert_eq!(assumed.timestamps_agree(), None);
}

#[test]
fn two_encodes_of_one_source_share_its_provenance() {
    let other = EncodeProvenance {
        source: SourceRef {
            video_stream: 1,
            ..source()
        },
        ..encode(4, 600)
    };
    let same = ComparisonSetup::new(
        Reference::Stream(stream("one.ivf", Some(encode(4, 500)))),
        stream("two.ivf", Some(encode(4, 600))),
    );
    assert_eq!(same.basis, AlignmentBasis::Provenance);
    let differs = ComparisonSetup::new(
        Reference::Stream(stream("one.ivf", Some(encode(4, 500)))),
        stream("two.ivf", Some(other)),
    );
    assert_eq!(differs.basis, AlignmentBasis::Timestamps);
}

#[test]
fn the_builder_keeps_exact_runs_and_names_faults() {
    assert_eq!(clock((1, 25), []), Clock::Absent);
    assert_eq!(
        clock((1001, 30000), [7, 8, 9, 20, 21]),
        Clock::Exact {
            tick: Rational {
                num: 1001,
                den: 30000
            },
            runs: vec![
                ClockRun {
                    first: 0,
                    start: 7,
                    step: 1
                },
                ClockRun {
                    first: 3,
                    start: 20,
                    step: 1
                }
            ]
        }
    );
    let fault = |clock| match clock {
        Clock::Unusable { fault } => Some(fault),
        _ => None,
    };
    assert_eq!(fault(clock((1, 25), [0, 1, 1])), Some(ClockFault::Repeated));
    assert_eq!(
        fault(clock((1, 25), [0, 2, 1])),
        Some(ClockFault::Decreasing)
    );
    assert_eq!(fault(clock((0, 25), [0, 1])), Some(ClockFault::Invalid));
    // Alternating steps start a run at every second output.
    let cadence = (0..200).map(|n| n * 3 / 2);
    assert_eq!(fault(clock((1, 50), cadence)), Some(ClockFault::Irregular));
    let mut shared = ClockBuilder::new(RATE);
    shared.push(0);
    shared.fail(ClockFault::SharedPacket);
    shared.push(0);
    assert_eq!(fault(shared.finish()), Some(ClockFault::SharedPacket));
}

#[test]
fn timestamps_map_offsets_tails_and_gaps_across_time_bases() {
    // Side A: 10 frames at 25 fps. Side B starts 3 frames later in 90 kHz
    // ticks, drops two frames after its fourth and runs two frames longer.
    let a = clock((1, 25), 0..10);
    let b = clock(
        (1, 90_000),
        (3..7).chain(9..12).map(|frame: u64| frame * 3_600),
    );
    assert_eq!(
        align_by_timestamps(track(10, &a), track(7, &b)),
        Ok(Mapping {
            segments: vec![
                Segment::OnlyA { first: 0, count: 3 },
                Segment::Matched {
                    a: 3,
                    b: 0,
                    count: 4
                },
                Segment::OnlyA { first: 7, count: 2 },
                Segment::Matched {
                    a: 9,
                    b: 4,
                    count: 1
                },
                Segment::OnlyB { first: 5, count: 2 },
            ]
        })
    );
    let mapping = align_by_timestamps(track(10, &a), track(7, &b)).unwrap_or_default();
    assert_eq!(
        (
            mapping.matched(),
            mapping.unmatched(Side::A),
            mapping.unmatched(Side::B)
        ),
        (5, 5, 2)
    );
    assert_eq!(mapping.partner(Side::A, 5), Some(2));
    assert_eq!(mapping.partner(Side::B, 4), Some(9));
    assert_eq!(mapping.partner(Side::A, 7), None);
    assert_eq!(mapping.partner(Side::B, 6), None);
}

#[test]
fn timestamps_refuse_what_they_cannot_order() {
    let a = clock((1, 25), 0..10);
    let late = clock((1, 25), 100..110);
    assert_eq!(
        align_by_timestamps(track(10, &a), track(10, &late)),
        Err(AlignmentRefusal::Disjoint)
    );
    let backwards = clock((1, 25), [0, 1, 2, 1, 2]);
    assert_eq!(
        align_by_timestamps(track(10, &a), track(5, &backwards)),
        Err(AlignmentRefusal::UnusableClock(
            Side::B,
            ClockFault::Decreasing
        ))
    );
    // 25 and 30 frames per second meet every fifth and sixth frame.
    let faster = clock((1, 30), 0..6_000);
    let slower = clock((1, 25), 0..5_000);
    assert_eq!(
        align_by_timestamps(track(5_000, &slower), track(6_000, &faster)),
        Err(AlignmentRefusal::Fragmented)
    );
    let short = align_by_timestamps(track(6, &slower), track(7, &faster)).unwrap_or_default();
    assert_eq!(short.matched(), 2);
    assert_eq!(short.partner(Side::A, 5), Some(6));
}

#[test]
fn a_malformed_clock_is_refused_before_any_arithmetic() {
    let good = clock((1, 25), 0..4);
    let run = |first, start, step| ClockRun { first, start, step };
    let exact = |runs| Clock::Exact { tick: RATE, runs };
    for runs in [
        vec![],
        vec![run(1, 0, 1)],
        vec![run(0, 0, 0)],
        vec![run(0, 0, 1), run(2, 1, 1)],
        vec![run(0, 0, 1), run(9, 50, 1)],
        vec![run(0, u64::MAX, u64::MAX)],
        vec![run(0, 0, 1); MAX_CLOCK_RUNS + 1],
    ] {
        let bad = exact(runs);
        assert_eq!(
            align_by_timestamps(track(4, &good), track(4, &bad)),
            Err(AlignmentRefusal::UnusableClock(
                Side::B,
                ClockFault::Invalid
            )),
            "{bad:?}"
        );
    }
    let zero = Clock::Exact {
        tick: Rational { num: 1, den: 0 },
        runs: vec![run(0, 0, 1)],
    };
    assert_eq!(
        align_by_timestamps(track(4, &zero), track(4, &good)),
        Err(AlignmentRefusal::UnusableClock(
            Side::A,
            ClockFault::Invalid
        ))
    );
    // The widest valid values still compare exactly.
    let wide = Clock::Exact {
        tick: Rational {
            num: u32::MAX,
            den: u32::MAX,
        },
        runs: vec![run(0, u64::MAX - 3, 1)],
    };
    assert_eq!(
        align_by_timestamps(track(4, &wide), track(4, &wide)).map(|m| m.matched()),
        Ok(4)
    );
}

#[test]
fn an_assumed_offset_keeps_unequal_lengths_explicit() {
    assert_eq!(
        align_by_offset(10, 8, 3).segments,
        vec![
            Segment::OnlyB { first: 0, count: 3 },
            Segment::Matched {
                a: 0,
                b: 3,
                count: 5
            },
            Segment::OnlyA { first: 5, count: 5 },
        ]
    );
    assert_eq!(
        align_by_offset(10, 8, -4).segments,
        vec![
            Segment::OnlyA { first: 0, count: 4 },
            Segment::Matched {
                a: 4,
                b: 0,
                count: 6
            },
            Segment::OnlyB { first: 6, count: 2 },
        ]
    );
    for offset in [i64::MIN, -10, 8, i64::MAX] {
        let apart = align_by_offset(10, 8, offset);
        assert_eq!(
            (
                apart.matched(),
                apart.unmatched(Side::A),
                apart.unmatched(Side::B)
            ),
            (0, 10, 8),
            "{offset}"
        );
    }
    assert_eq!(align_by_offset(0, 0, 0), Mapping::default());
}

#[test]
fn provenance_reports_timestamps_that_disagree() {
    let (setup, a, mut b) = encode_pair(10);
    b.clock = clock((1, 25), 5..15);
    let session = resolved(setup, a, b);
    assert_eq!(session.alignment().map(|m| m.matched()), Ok(10));
    assert_eq!(session.timestamps_agree(), Some(false));
}

#[test]
fn incompatible_domains_refuse_numerical_comparison() {
    let (setup, a, b) = encode_pair(10);
    let with = |change: fn(&mut SampleDomain)| {
        let mut b = b.clone();
        if let Some(domain) = &mut b.domain {
            change(domain);
        }
        resolved(setup.clone(), a.clone(), b).compatibility()
    };
    let refused = |property| Compatibility::Refused {
        reasons: vec![DomainMismatch::Differs(property)],
    };
    assert_eq!(with(|d| d.width = 66), refused(DomainProperty::Width));
    assert_eq!(with(|d| d.height = 50), refused(DomainProperty::Height));
    assert_eq!(
        with(|d| d.bit_depth = 10),
        refused(DomainProperty::BitDepth)
    );
    assert_eq!(
        with(|d| d.chroma = ChromaLayout::Yuv444),
        refused(DomainProperty::Chroma)
    );
    assert_eq!(
        with(|d| d.matrix = Some(9)),
        refused(DomainProperty::Matrix)
    );
    assert_eq!(
        with(|d| d.transfer = Some(16)),
        refused(DomainProperty::Transfer)
    );
    assert_eq!(
        with(|d| d.full_range = Some(true)),
        refused(DomainProperty::Range)
    );
    assert_eq!(
        with(|d| d.chroma_position = Some((2, 2))),
        refused(DomainProperty::ChromaPosition)
    );
    assert_eq!(
        with(|d| {
            d.width = 32;
            d.primaries = Some(9);
        }),
        Compatibility::Refused {
            reasons: vec![
                DomainMismatch::Differs(DomainProperty::Width),
                DomainMismatch::Differs(DomainProperty::Primaries)
            ]
        }
    );
    // A value that one side does not declare is unknown, not a mismatch.
    assert_eq!(
        with(|d| {
            d.primaries = None;
            d.full_range = None;
        }),
        Compatibility::Exact {
            undeclared: vec![DomainProperty::Primaries, DomainProperty::Range]
        }
    );
    // Synthesized grain on one side only is a different sample domain.
    assert_eq!(
        with(|d| d.film_grain = true),
        refused(DomainProperty::Grain)
    );
    let mut skipped = setup.clone();
    skipped.grain = GrainPolicy::Skip;
    let mut grainy = b.clone();
    if let Some(domain) = &mut grainy.domain {
        domain.film_grain = true;
    }
    assert!(matches!(
        resolved(skipped, a.clone(), grainy).compatibility(),
        Compatibility::Exact { .. }
    ));
    let mut varies = b.clone();
    varies.domain = None;
    assert_eq!(
        resolved(setup.clone(), a.clone(), varies).compatibility(),
        Compatibility::Refused {
            reasons: vec![DomainMismatch::FormatVaries(Side::B)]
        }
    );
}

#[test]
fn evidence_identity_follows_inputs_codec_and_policy() -> serde_json::Result<()> {
    let (setup, a, b) = encode_pair(10);
    let session = resolved(setup.clone(), a.clone(), b.clone());
    let identity = session.identity();
    assert!(identity.is_some());

    let mut assumed = session.clone();
    assumed.setup.basis = AlignmentBasis::AssumedOffset { offset: -2 };
    assumed.setup.grain = GrainPolicy::Skip;
    assert_ne!(assumed.identity(), identity);

    // The stored values are the setup and the identity. A session made
    // again from them and the same facts has the same policy and identity.
    let stored = serde_json::to_string(&(&assumed.setup, assumed.identity()))?;
    let (kept, key): (ComparisonSetup, Option<EvidenceIdentity>) = serde_json::from_str(&stored)?;
    assert_eq!(kept.basis, AlignmentBasis::AssumedOffset { offset: -2 });
    assert_eq!(kept.grain, GrainPolicy::Skip);
    let restored = resolved(kept, a.clone(), b.clone());
    assert_eq!(restored, assumed);
    assert_eq!(restored.identity(), key);
    assert_eq!(restored.alignment(), assumed.alignment());

    // A new codec revision, or a replaced file of the same size, is another identity.
    let codec = ComparisonSession::resolve(setup.clone(), "0000".into(), a.clone(), b.clone());
    assert_ne!(codec.ok().and_then(|s| s.identity()), identity);
    let mut replaced = b.clone();
    replaced.digest = Some(SourceDigest {
        sha256: "cd".repeat(32),
        bytes: 700,
    });
    assert_ne!(
        resolved(setup.clone(), a.clone(), replaced).identity(),
        identity
    );
    let mut touched = b;
    touched.fingerprint.modified = None;
    assert_ne!(resolved(setup, a, touched).identity(), identity);
    Ok(())
}

fn choice(text: &str) -> OptionValue {
    OptionValue::Choice(text.into())
}

fn effective(key: &str, requested: Option<OptionValue>, value: OptionValue) -> EffectiveOption {
    EffectiveOption {
        key: key.into(),
        origin: choice(if requested.is_some() {
            "explicit"
        } else {
            "codec default"
        }),
        requested,
        value,
        screen: None,
        screen_origin: None,
    }
}

fn job(values: &[(&str, OptionValue)], policy: &[(&str, OptionValue)]) -> EncodeProvenance {
    let requested: BTreeMap<String, OptionValue> = values
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect();
    EncodeProvenance {
        effective: policy
            .iter()
            .map(|(key, value)| effective(key, requested.get(*key).cloned(), value.clone()))
            .collect(),
        requested: OptionRequest {
            base: settings(),
            values: requested,
        },
        ..encode(4, 500)
    }
}

#[test]
fn an_explicit_default_is_not_an_inherited_value() {
    let policy = [
        ("keyint", OptionValue::Integer(120)),
        ("tune", choice("psnr")),
        ("threads", OptionValue::Integer(4)),
    ];
    // Side B states the default interval and changes the tune.
    let inherited = job(&[], &policy);
    let explicit = job(
        &[
            ("keyint", OptionValue::Integer(120)),
            ("tune", choice("ssim")),
        ],
        &[
            ("keyint", OptionValue::Integer(120)),
            ("tune", choice("ssim")),
            ("threads", OptionValue::Integer(4)),
        ],
    );
    let pair = compare_configuration(&inherited, &explicit);
    assert_eq!(pair.same, 1);
    assert_eq!(pair.effective_known, (true, true));
    let rows: Vec<_> = pair
        .rows
        .iter()
        .map(|row| {
            (
                row.key.as_str(),
                row.difference,
                row.a.requested.is_some(),
                row.b.requested.is_some(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("keyint", ConfigDifference::Declaration, false, true),
            ("tune", ConfigDifference::Effective, false, true),
        ]
    );
    // Equal values keep their effective cells, so the view can show both.
    assert_eq!(
        pair.rows[0].a.effective.as_ref().map(|e| &e.value),
        pair.rows[0].b.effective.as_ref().map(|e| &e.value)
    );
    assert_eq!(compare_configuration(&explicit, &explicit).rows, []);
}

#[test]
fn an_unrecorded_policy_is_unknown_and_codecs_are_listed() {
    // A historical job recorded its requests only.
    let historical = EncodeProvenance {
        codec: None,
        ..job(&[("tune", choice("ssim"))], &[])
    };
    let current = job(
        &[("keyint", OptionValue::Integer(60))],
        &[
            ("keyint", OptionValue::Integer(60)),
            ("tune", choice("psnr")),
        ],
    );
    let pair = compare_configuration(&historical, &current);
    assert_eq!(pair.effective_known, (false, true));
    assert_eq!(pair.codec.0, None);
    assert_eq!(
        pair.codec.1.as_ref().map(|c| c.codec_revision.as_str()),
        Some("658ace72")
    );
    assert_eq!(pair.same, 0);
    for row in &pair.rows {
        assert_eq!(row.difference, ConfigDifference::Requested, "{}", row.key);
        assert_eq!((&row.a.effective, &row.b.effective), (&None, &None));
    }
    let keys: Vec<_> = pair.rows.iter().map(|row| row.key.as_str()).collect();
    assert_eq!(keys, ["keyint", "tune"]);

    // A key that only one schema has is one-sided, not equal.
    let newer = job(
        &[],
        &[
            ("keyint", OptionValue::Integer(60)),
            ("new_tool", OptionValue::Bool(true)),
        ],
    );
    let older = job(&[], &[("keyint", OptionValue::Integer(60))]);
    let pair = compare_configuration(&older, &newer);
    assert_eq!(pair.same, 1);
    assert_eq!(
        pair.rows
            .iter()
            .map(|row| (row.key.as_str(), row.difference))
            .collect::<Vec<_>>(),
        [("new_tool", ConfigDifference::OneSided)]
    );
}

#[test]
fn a_session_compares_settings_only_when_both_jobs_are_known() {
    let both = ComparisonSetup::new(
        Reference::Stream(stream("one.ivf", Some(encode(4, 500)))),
        stream("two.ivf", Some(encode(4, 600))),
    );
    let session = resolved(
        both,
        facts(500, 4, Clock::Absent),
        facts(600, 4, Clock::Absent),
    );
    assert!(matches!(
        session.configuration(),
        ConfigComparison::Known(_)
    ));
    let (setup, a, b) = encode_pair(4);
    assert_eq!(
        resolved(setup, a, b).configuration(),
        ConfigComparison::Unknown { a: false, b: true }
    );
}

#[test]
fn a_first_pass_job_publishes_no_stream() {
    let report = JobReport {
        destination: WirePath::Utf8("/media/clip.ivf".into()),
        pass_digest: None,
        frames: 10,
        bytes: 700,
        checks: Vec::new(),
        disclosures: Vec::new(),
    };
    let mut spec = JobSpec {
        purpose: JobPurpose::Encode,
        source: source().path,
        fingerprint: source().fingerprint,
        video_stream: 0,
        omissions_acknowledged: false,
        destination: report.destination.clone(),
        settings: settings().into(),
        identity: None,
        pass_artifact: None,
        pass_digest: None,
    };
    let provenance = EncodeProvenance::of(&spec, &report, &[]);
    assert_eq!(
        provenance.map(|p| (p.source, p.frames, p.bytes)),
        Some((source(), 10, 700))
    );
    spec.purpose = JobPurpose::FirstPass;
    assert_eq!(EncodeProvenance::of(&spec, &report, &[]), None);
}
