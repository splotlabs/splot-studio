// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use std::collections::BTreeMap;

use studio_domain::analysis::SourceDigest;
use studio_domain::compare::ClockBuilder;
use studio_domain::job::{ChromaLayout, SourceFingerprint};
use studio_domain::layers::LayerDecodeSelection;
use studio_domain::options::{EffectiveOption, OptionRequest, OptionValue};
use studio_domain::preview::SourceRef;
use studio_domain::protocol::{Event, OptionsIdentity};

use super::*;
use crate::compare::Effect;
use crate::state::INITIAL_SETTINGS;

fn fingerprint(len: u64) -> SourceFingerprint {
    SourceFingerprint {
        len,
        modified: None,
    }
}

fn choice(text: &str) -> OptionValue {
    OptionValue::Choice(text.into())
}

/// A job with `explicit` requests, and `policy` rows as key, value and screen value.
fn encode(
    bytes: u64,
    revision: Option<&str>,
    explicit: &[&str],
    policy: &[(&str, i64, Option<i64>)],
) -> EncodeProvenance {
    let values: BTreeMap<String, OptionValue> = policy
        .iter()
        .filter(|(key, ..)| explicit.contains(key))
        .map(|(key, value, _)| ((*key).to_owned(), OptionValue::Integer(*value)))
        .collect();
    EncodeProvenance {
        source: SourceRef {
            path: WirePath::Utf8("/media/clip.y4m".into()),
            fingerprint: fingerprint(9_000),
            video_stream: 0,
        },
        codec: revision.map(|revision| OptionsIdentity {
            schema: 1,
            codec_revision: revision.into(),
        }),
        effective: policy
            .iter()
            .map(|(key, value, screen)| EffectiveOption {
                key: (*key).to_owned(),
                requested: values.get(*key).cloned(),
                value: OptionValue::Integer(*value),
                screen: screen.map(OptionValue::Integer),
                origin: choice(if values.contains_key(*key) {
                    "explicit"
                } else {
                    "codec default"
                }),
                screen_origin: screen.map(|_| choice("screen content")),
            })
            .collect(),
        requested: OptionRequest {
            base: INITIAL_SETTINGS,
            values,
        },
        frames: 5,
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

fn facts(len: u64, outputs: u64, clock: Clock) -> Event {
    Event::CompareDescribed {
        codec_revision: "658ace723b4a6c98".into(),
        facts: Box::new(SideFacts {
            fingerprint: fingerprint(len),
            digest: Some(SourceDigest {
                sha256: "ab".repeat(32),
                bytes: len,
            }),
            outputs: Some(outputs),
            domain: Some(SampleDomain {
                width: 64,
                height: 48,
                bit_depth: 10,
                chroma: ChromaLayout::Yuv420,
                primaries: Some(1),
                transfer: None,
                matrix: None,
                full_range: Some(false),
                chroma_position: Some((2, 2)),
                film_grain: false,
            }),
            clock,
        }),
    }
}

fn ticks(den: u32, count: u64) -> Clock {
    let mut builder = ClockBuilder::new(Rational { num: 1, den });
    (0..count).for_each(|tick| builder.push(tick));
    builder.finish()
}

/// Answers the read that `effects` asks for and returns the view.
fn read(compare: &mut Compare, effects: &[Effect], a: Event, b: Event) -> View {
    let Some(Effect::Read(requests)) = effects.last() else {
        unreachable!("{effects:?}")
    };
    compare.event(requests[0].0, a);
    compare.event(requests[1].0, b);
    view(compare)
}

#[test]
fn a_source_pair_says_what_was_read_and_what_was_declared() {
    let mut compare = Compare::default();
    let effects = compare.pair_encode(stream("clip.ivf", Some(encode(700, None, &[], &[]))));
    let before = view(&compare);
    assert_eq!(before.state, State::Reading);
    // Before the read, the job is a record for the path and no more.
    assert!(
        before.b.lines[0].ends_with("An encode job recorded this path as its output."),
        "{:?}",
        before.b.lines
    );
    let view = read(
        &mut compare,
        &effects,
        facts(9_000, 5, Clock::constant(Rational { num: 25, den: 1 })),
        facts(700, 5, ticks(25, 5)),
    );
    assert_eq!(view.state, State::Ready);
    let a = view.a.lines.join("\n");
    // The source gives a declared rate. Nothing read its frame times.
    assert!(
        a.contains("5 pictures read by the job · constant rate 25 fps declared by the source"),
        "{a}"
    );
    assert!(a.contains(
        "The size matches the encode job. The file system reports no modification time."
    ));
    assert!(
        a.contains("Primaries 1 · transfer not declared · matrix not declared · limited range")
    );
    assert!(a.contains("Chroma sample position 2"));
    assert!(
        view.b
            .lines
            .contains(&"The size and the picture count match the encode job.".to_owned())
    );
    assert_eq!(
        view.b.exact,
        [
            format!("SHA-256 {}", "ab".repeat(32)),
            "/media/clip.ivf".to_owned()
        ]
    );
    assert_eq!(
        view.verdict,
        "Sample values can be compared without a conversion."
    );
    assert!(
        view.verdict_details[0].contains(
            "transfer characteristics, matrix coefficients. Studio compares the code values"
        ),
        "{:?}",
        view.verdict_details
    );
    assert_eq!(
        view.alignment,
        "5 pictures match. Without a match: 0 of side A, 0 of side B. The stream timestamps \
         and the declared rate of the source agree with this order."
    );
    assert_eq!(
        view.basis_labels,
        [
            "Frame order of the encode job",
            "Exact timestamps",
            "Assumed index offset"
        ]
    );
    assert_eq!(view.offset, None);
    assert!(view.summary.contains("Side A: clip.y4m\n"));
    assert!(view.summary.contains("Film grain: applied\n"));
    assert_eq!(
        super::job(compare.status()),
        (
            "clip.y4m and clip.ivf".to_owned(),
            "Read 5 pictures of side A and 5 pictures of side B.".to_owned()
        )
    );
}

#[test]
fn refusals_faults_and_segments_are_said_in_words() {
    let mut compare = Compare::default();
    compare.pick(Side::A, stream("one.ivf", None));
    let effects = compare.pick(Side::B, stream("two.ivf", None));
    let mut shared = ClockBuilder::new(Rational { num: 1, den: 30 });
    shared.push(0);
    shared.fail(ClockFault::SharedPacket);
    let mut view = read(
        &mut compare,
        &effects,
        facts(500, 6, ticks(25, 6)),
        facts(600, 7, shared.finish()),
    );
    assert!(
        view.b.lines.contains(
            &"7 pictures · timestamps not usable: one timestamp covers more than one temporal unit"
                .to_owned()
        ),
        "{:?}",
        view.b.lines
    );
    assert!(view.alignment_problem);
    assert_eq!(
        view.alignment,
        "The timestamps of side B do not give each output one time. Choose another basis. \
         An index offset is an assumption that Studio records."
    );
    assert_eq!(
        view.basis_labels,
        ["Exact timestamps", "Assumed index offset"]
    );
    assert_eq!(
        view.settings_status,
        "No encode job is recorded for side A or side B. Studio does not infer settings from a stream."
    );

    compare.set_basis(AlignmentBasis::AssumedOffset { offset: -2 });
    view = super::view(&compare);
    assert_eq!(
        (view.offset, view.offset_limit, view.basis_index),
        (Some(-2), 7, 1)
    );
    assert!(
        view.alignment.ends_with(
            "This is your assumption: picture n of side A matches picture n − 2 of side B."
        ),
        "{}",
        view.alignment
    );
    assert_eq!(
        view.segments,
        [
            "A 0 to 1: no match in side B",
            "A 2 to 5 matches B 0 to 3",
            "B 4 to 6: no match in side A"
        ]
    );

    // 25 and 30 pictures per second meet at the first and the last picture.
    let effects = compare.pick(Side::B, stream("three.ivf", None));
    let lines = read(
        &mut compare,
        &effects,
        facts(500, 6, ticks(25, 6)),
        facts(900, 7, ticks(30, 7)),
    )
    .segments;
    assert_eq!(lines.len(), SEGMENT_LINES + 1);
    assert_eq!(lines[0], "A 0 matches B 0");
    assert!(lines[SEGMENT_LINES].ends_with("more segments are not listed."));
}

#[test]
fn setting_rows_show_the_value_and_how_it_was_set() {
    let policy = [
        ("keyint", 120, None),
        ("tools", 3, Some(4)),
        ("threads", 2, None),
    ];
    let other = [
        ("keyint", 120, None),
        ("tools", 3, Some(5)),
        ("threads", 2, None),
    ];
    let mut compare = Compare::default();
    compare.pick(
        Side::A,
        stream(
            "one.ivf",
            Some(encode(500, Some("aaaaaaaaaaaaaaaa"), &[], &policy)),
        ),
    );
    let effects = compare.pick(
        Side::B,
        stream(
            "two.ivf",
            Some(encode(600, Some("bbbbbbbbbbbbbbbb"), &["keyint"], &other)),
        ),
    );
    let both = || (facts(500, 5, Clock::Absent), facts(600, 5, Clock::Absent));
    let (a, b) = both();
    let view = read(&mut compare, &effects, a, b);
    assert_eq!(
        view.settings_status,
        "2 settings differ. 1 are equal. The codec revisions differ: side A aaaaaaaaaaaa, side B bbbbbbbbbbbb."
    );
    let rows: Vec<_> = view
        .settings
        .iter()
        .map(|row| {
            (
                row.key.as_str(),
                row.a.as_str(),
                row.b.as_str(),
                row.difference,
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            (
                "keyint",
                "120 (codec default)",
                "120 (explicit)",
                "Same value, set differently"
            ),
            (
                "tools",
                "3 (codec default) · screen content: 4 (screen content)",
                "3 (codec default) · screen content: 5 (screen content)",
                "Different value"
            ),
        ]
    );
    assert!(view.summary.contains(
        "  keyint: A 120 (codec default) | B 120 (explicit) | Same value, set differently\n"
    ));
}

/// A job with no recorded policy: only what the jobs requested is compared.
#[test]
fn an_unrecorded_policy_compares_the_requests_only() {
    let policy = [("keyint", 120, None)];
    let mut compare = Compare::default();
    compare.pick(
        Side::A,
        stream(
            "one.ivf",
            Some(encode(500, Some("aaaaaaaaaaaaaaaa"), &[], &policy)),
        ),
    );
    let both = || (facts(500, 5, Clock::Absent), facts(600, 5, Clock::Absent));
    let mut historical = encode(600, None, &["keyint"], &[("keyint", 60, None)]);
    historical.effective.clear();
    historical.requested.base.crf =
        studio_domain::settings::Crf::new(40).unwrap_or(INITIAL_SETTINGS.crf);
    let effects = compare.pick(Side::B, stream("two.ivf", Some(historical)));
    let (a, b) = both();
    let view = read(&mut compare, &effects, a, b);
    assert!(
        view.settings_status.ends_with(
            "A job did not record its codec revision. A job did not record its effective \
             policy. Studio compares the explicit settings only."
        ),
        "{}",
        view.settings_status
    );
    let rows: Vec<_> = view
        .settings
        .iter()
        .map(|row| {
            (
                row.key.as_str(),
                row.a.as_str(),
                row.b.as_str(),
                row.difference,
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("rate factor (Simple)", "20", "40", "Different value"),
            (
                "keyint",
                "inherited, value not recorded",
                "60 (explicit)",
                "Different request"
            ),
        ]
    );
}
