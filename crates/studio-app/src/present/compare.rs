// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Display text for the Compare workspace. Every line states what the
//! engine read or what the job recorded. A value that is not known is
//! named as unknown.

use std::fmt::Write as _;

use studio_domain::compare::{
    AlignmentBasis, Clock, ClockFault, ComparisonSession, ComparisonSetup, Compatibility,
    ConfigCell, ConfigComparison, ConfigDifference, ConfigPair, DomainMismatch, DomainProperty,
    EncodeProvenance, Mapping, Reference, SampleDomain, Segment, Side, SideFacts, StreamInput,
};
use studio_domain::job::{Rational, WirePath};
use studio_domain::preview::GrainPolicy;

use super::{chroma, file_name, frame_rate};
use crate::compare::{Compare, Status};

/// The most alignment segments listed.
const SEGMENT_LINES: usize = 6;

/// Which state the workspace shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// No pair.
    Empty,
    /// The engine reads the files.
    Reading,
    /// Both sides are read.
    Ready,
    /// The pair cannot be compared.
    Failed,
}

/// One side card.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SideText {
    /// The file name, or what to choose.
    pub name: String,
    /// What is known about the side, one fact per line.
    pub lines: Vec<String>,
    /// Values that must stay exact: the digest and the path.
    pub exact: Vec<String>,
}

/// One option that the two encode jobs did not set alike.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingText {
    /// The option key.
    pub key: String,
    /// The value of side A and how it was set.
    pub a: String,
    /// The value of side B and how it was set.
    pub b: String,
    /// How the two differ.
    pub difference: &'static str,
}

/// Everything the Compare workspace shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    /// The state.
    pub state: State,
    /// Guidance, progress or the failure.
    pub message: String,
    /// A change that did not stop the comparison.
    pub notice: String,
    /// Side A.
    pub a: SideText,
    /// Side B.
    pub b: SideText,
    /// Whether sample values can be compared.
    pub verdict: String,
    /// Numerical comparison is not available.
    pub verdict_problem: bool,
    /// The reasons and the caveats.
    pub verdict_details: Vec<String>,
    /// The alignment bases the pair can use.
    pub basis_labels: Vec<&'static str>,
    /// The basis in use, as an index into the labels.
    pub basis_index: usize,
    /// The offset from side A to side B, when the basis is an index offset.
    pub offset: Option<i64>,
    /// The largest offset that can match a picture.
    pub offset_limit: u64,
    /// The result of the alignment.
    pub alignment: String,
    /// The basis gives no mapping.
    pub alignment_problem: bool,
    /// The first segments of the mapping.
    pub segments: Vec<String>,
    /// Film grain is applied.
    pub grain: bool,
    /// What is known about the settings of both sides.
    pub settings_status: String,
    /// The settings that differ.
    pub settings: Vec<SettingText>,
    /// All of the above as plain text.
    pub summary: String,
}

/// The alignment bases a pair can use, in the order of their labels.
fn bases(setup: &ComparisonSetup) -> Vec<(AlignmentBasis, &'static str)> {
    let offset = match setup.basis {
        AlignmentBasis::AssumedOffset { offset } => offset,
        _ => 0,
    };
    let mut bases = vec![
        (AlignmentBasis::Timestamps, "Exact timestamps"),
        (
            AlignmentBasis::AssumedOffset { offset },
            "Assumed index offset",
        ),
    ];
    if setup.common_source().is_some() {
        bases.insert(
            0,
            (AlignmentBasis::Provenance, "Frame order of the encode job"),
        );
    }
    bases
}

/// The basis at `index` of the labels the workspace shows.
#[must_use]
pub fn basis_at(compare: &Compare, index: usize) -> Option<AlignmentBasis> {
    let setup = compare.status().setup()?;
    bases(setup).get(index).map(|(basis, _)| *basis)
}

fn path_text(path: &WirePath) -> String {
    path.to_path_buf().map_or_else(
        |_| "The path was written on another kind of system.".to_owned(),
        |path| path.display().to_string(),
    )
}

fn name_text(path: &WirePath) -> String {
    path.to_path_buf()
        .map_or_else(|_| "Unreadable path".to_owned(), |path| file_name(&path))
}

fn short(revision: &str) -> &str {
    revision.get(..12).unwrap_or(revision)
}

/// An H.273 code point, or that the side does not declare it.
fn code(value: Option<u8>) -> String {
    value.map_or_else(|| "not declared".to_owned(), |code| code.to_string())
}

/// The AV2 chroma sample position codes of the two fields.
fn position(value: Option<(u32, u32)>) -> String {
    match value {
        None => "not declared".to_owned(),
        Some((top, bottom)) if top == bottom => top.to_string(),
        Some((top, bottom)) => format!("{top} (top field), {bottom} (bottom field)"),
    }
}

fn domain_lines(domain: Option<&SampleDomain>) -> Vec<String> {
    let Some(domain) = domain else {
        return vec!["The sample format changes inside the stream.".into()];
    };
    let range = match domain.full_range {
        Some(true) => "full range",
        Some(false) => "limited range",
        None => "range not declared",
    };
    let mut lines = vec![
        format!(
            "{} × {} · {}-bit · {}",
            domain.width,
            domain.height,
            domain.bit_depth,
            chroma(domain.chroma)
        ),
        format!(
            "Primaries {} · transfer {} · matrix {} · {range}",
            code(domain.primaries),
            code(domain.transfer),
            code(domain.matrix)
        ),
    ];
    if domain.chroma_position.is_some() {
        lines.push(format!(
            "Chroma sample position {}",
            position(domain.chroma_position)
        ));
    }
    if domain.film_grain {
        lines.push("The stream signals film grain.".into());
    }
    lines
}

fn pictures(count: u64) -> String {
    if count == 1 {
        "1 picture".to_owned()
    } else {
        format!("{count} pictures")
    }
}

fn clock_text(clock: &Clock) -> String {
    match clock {
        Clock::Absent => "no presentation timestamps".to_owned(),
        Clock::Exact { tick, .. } => {
            format!("exact timestamps in ticks of {}/{} s", tick.num, tick.den)
        }
        Clock::Unusable { fault } => format!(
            "timestamps not usable: {}",
            match fault {
                ClockFault::Repeated => "two pictures share one timestamp",
                ClockFault::Decreasing => "a timestamp goes back",
                ClockFault::SharedPacket => "one timestamp covers more than one temporal unit",
                ClockFault::Irregular => "the cadence changes too often",
                ClockFault::Invalid => "the time base is not valid",
            }
        ),
    }
}

/// The rate a source declares: its clock ticks once for each frame.
fn source_rate(clock: &Clock) -> String {
    match clock {
        Clock::Exact { tick, .. } => format!(
            "constant rate {} declared by the source",
            frame_rate(Rational {
                num: tick.den,
                den: tick.num,
            })
        ),
        other => clock_text(other),
    }
}

fn stream_side(input: &StreamInput, facts: Option<(&SideFacts, u64)>) -> SideText {
    // The job was found by this path. Only the read facts can confirm it.
    let mut lines = vec![if input.encode.is_some() {
        "AV2 stream. An encode job recorded this path as its output.".to_owned()
    } else {
        "AV2 stream".to_owned()
    }];
    let mut exact = Vec::new();
    if let Some((facts, outputs)) = facts {
        lines.extend(domain_lines(facts.domain.as_ref()));
        lines.push(format!(
            "{} · {}",
            pictures(outputs),
            clock_text(&facts.clock)
        ));
        if let Some(digest) = &facts.digest {
            lines.push(format!("{} bytes", digest.bytes));
            exact.push(format!("SHA-256 {}", digest.sha256));
        }
        if input.encode.is_some() {
            lines.push("The size and the picture count match the encode job.".into());
        }
    }
    lines.push(input.encode.as_deref().map_or_else(
        || "Settings: not recorded. Studio does not infer settings from a stream.".to_owned(),
        settings_origin,
    ));
    exact.push(path_text(&input.path));
    SideText {
        name: name_text(&input.path),
        lines,
        exact,
    }
}

fn settings_origin(encode: &EncodeProvenance) -> String {
    encode.codec.as_ref().map_or_else(
        || "Settings: from the encode job. The job did not record its codec revision.".to_owned(),
        |codec| {
            format!(
                "Settings: from the encode job, codec {}",
                short(&codec.codec_revision)
            )
        },
    )
}

fn source_side(encode: Option<&EncodeProvenance>, facts: Option<(&SideFacts, u64)>) -> SideText {
    let Some(encode) = encode else {
        return SideText {
            name: "No source".into(),
            ..SideText::default()
        };
    };
    let mut lines = vec![format!(
        "Source that the encode job of side B read · video stream {}",
        encode.source.video_stream
    )];
    if let Some((facts, outputs)) = facts {
        lines.extend(domain_lines(facts.domain.as_ref()));
        // The probe reads no frame time: the times follow from the declared rate.
        lines.push(format!(
            "{} read by the job · {}",
            pictures(outputs),
            source_rate(&facts.clock)
        ));
        lines.push(
            if facts.fingerprint.modified.is_some() {
                "The size and the modification time match the encode job."
            } else {
                "The size matches the encode job. The file system reports no modification time."
            }
            .into(),
        );
    }
    SideText {
        name: name_text(&encode.source.path),
        lines,
        exact: vec![path_text(&encode.source.path)],
    }
}

fn sides(setup: &ComparisonSetup, session: Option<&ComparisonSession>) -> (SideText, SideText) {
    let facts = |side| session.map(|session| (session.facts(side), session.outputs(side)));
    let a = match &setup.a {
        Reference::EncodeSource => source_side(setup.b.encode.as_deref(), facts(Side::A)),
        Reference::Stream(input) => stream_side(input, facts(Side::A)),
    };
    (a, stream_side(&setup.b, facts(Side::B)))
}

fn picked(input: Option<&StreamInput>) -> SideText {
    input.map_or_else(
        || SideText {
            name: "No file".into(),
            ..SideText::default()
        },
        |input| stream_side(input, None),
    )
}

const NO_CONVERSION: &str =
    "Studio applies no crop, scale, depth change or colour conversion to make the sides match.";

fn property_name(property: DomainProperty) -> &'static str {
    match property {
        DomainProperty::Width => "width",
        DomainProperty::Height => "height",
        DomainProperty::BitDepth => "bit depth",
        DomainProperty::Chroma => "chroma layout",
        DomainProperty::Primaries => "colour primaries",
        DomainProperty::Transfer => "transfer characteristics",
        DomainProperty::Matrix => "matrix coefficients",
        DomainProperty::Range => "range",
        DomainProperty::ChromaPosition => "chroma sample position",
        DomainProperty::Grain => "film grain",
    }
}

fn property_value(property: DomainProperty, domain: &SampleDomain, grain: GrainPolicy) -> String {
    match property {
        DomainProperty::Width => domain.width.to_string(),
        DomainProperty::Height => domain.height.to_string(),
        DomainProperty::BitDepth => format!("{}-bit", domain.bit_depth),
        DomainProperty::Chroma => chroma(domain.chroma).to_owned(),
        DomainProperty::Primaries => code(domain.primaries),
        DomainProperty::Transfer => code(domain.transfer),
        DomainProperty::Matrix => code(domain.matrix),
        DomainProperty::Range => match domain.full_range {
            Some(true) => "full".to_owned(),
            Some(false) => "limited".to_owned(),
            None => "not declared".to_owned(),
        },
        DomainProperty::ChromaPosition => position(domain.chroma_position),
        DomainProperty::Grain => if domain.film_grain && grain == GrainPolicy::Apply {
            "synthesized grain"
        } else {
            "no synthesized grain"
        }
        .to_owned(),
    }
}

fn verdict(session: &ComparisonSession) -> (String, bool, Vec<String>) {
    match session.compatibility() {
        Compatibility::Exact { undeclared } => {
            let mut details = Vec::new();
            if !undeclared.is_empty() {
                let names: Vec<_> = undeclared.iter().map(|p| property_name(*p)).collect();
                details.push(format!(
                    "Not declared on one side or on both sides: {}. Studio compares the code values as they are.",
                    names.join(", ")
                ));
            }
            details.push(NO_CONVERSION.to_owned());
            (
                "Sample values can be compared without a conversion.".to_owned(),
                false,
                details,
            )
        }
        Compatibility::Refused { reasons } => {
            let mut details: Vec<String> = reasons
                .iter()
                .map(|reason| match *reason {
                    DomainMismatch::FormatVaries(side) => {
                        format!("The sample format changes inside the stream of side {side}.")
                    }
                    DomainMismatch::Differs(property) => {
                        let value = |facts: &SideFacts| {
                            facts.domain.as_ref().map_or_else(String::new, |domain| {
                                property_value(property, domain, session.setup.grain)
                            })
                        };
                        format!(
                            "The {} differs: side A has {}, side B has {}.",
                            property_name(property),
                            value(&session.a),
                            value(&session.b)
                        )
                    }
                })
                .collect();
            if reasons.contains(&DomainMismatch::Differs(DomainProperty::Grain)) {
                details.push("Clear Apply film grain to compare the pictures before grain.".into());
            }
            details.push(NO_CONVERSION.to_owned());
            (
                "Numerical comparison is not available.".to_owned(),
                true,
                details,
            )
        }
    }
}

fn range(first: u64, count: u64) -> String {
    if count == 1 {
        first.to_string()
    } else {
        format!("{first} to {}", first + count - 1)
    }
}

fn segment_lines(mapping: &Mapping) -> Vec<String> {
    let mut lines: Vec<String> = mapping
        .segments
        .iter()
        .take(SEGMENT_LINES)
        .map(|segment| match *segment {
            Segment::Matched { a, b, count } => {
                format!("A {} matches B {}", range(a, count), range(b, count))
            }
            Segment::OnlyA { first, count } => {
                format!("A {}: no match in side B", range(first, count))
            }
            Segment::OnlyB { first, count } => {
                format!("B {}: no match in side A", range(first, count))
            }
        })
        .collect();
    if mapping.segments.len() > SEGMENT_LINES {
        lines.push(format!(
            "{} more segments are not listed.",
            mapping.segments.len() - SEGMENT_LINES
        ));
    }
    lines
}

fn alignment(session: &ComparisonSession) -> (String, bool, Vec<String>) {
    match session.alignment() {
        Ok(mapping) => {
            let mut text = format!(
                "{} match. Without a match: {} of side A, {} of side B.",
                pictures(mapping.matched()),
                mapping.unmatched(Side::A),
                mapping.unmatched(Side::B)
            );
            match (session.setup.basis, session.timestamps_agree()) {
                (AlignmentBasis::Provenance, Some(agree)) => {
                    let subject = if matches!(session.setup.a, Reference::EncodeSource) {
                        "The stream timestamps and the declared rate of the source"
                    } else {
                        "The timestamps of the two streams"
                    };
                    let _ = write!(
                        text,
                        " {subject} {} this order.",
                        if agree {
                            "agree with"
                        } else {
                            "do not agree with"
                        }
                    );
                    if !agree {
                        text.push_str(" The mapping uses the frame order of the job.");
                    }
                }
                (AlignmentBasis::AssumedOffset { offset }, _) => {
                    let _ = write!(
                        text,
                        " This is your assumption: picture n of side A matches picture n {} {} of side B.",
                        if offset < 0 { '−' } else { '+' },
                        offset.unsigned_abs()
                    );
                }
                _ => {}
            }
            (text, false, segment_lines(&mapping))
        }
        Err(refusal) => (
            format!(
                "{refusal} Choose another basis. An index offset is an assumption that Studio records."
            ),
            true,
            Vec::new(),
        ),
    }
}

fn cell(cell: &ConfigCell) -> String {
    let value = crate::advanced::display;
    match (&cell.effective, &cell.requested) {
        (Some(effective), _) => {
            let mut text = format!("{} ({})", value(&effective.value), value(&effective.origin));
            if let Some(screen) = &effective.screen {
                let _ = write!(text, " · screen content: {}", value(screen));
                if let Some(origin) = &effective.screen_origin {
                    let _ = write!(text, " ({})", value(origin));
                }
            }
            text
        }
        (None, Some(requested)) => format!("{} (explicit)", value(requested)),
        (None, None) => "inherited, value not recorded".to_owned(),
    }
}

fn setting_rows(pair: &ConfigPair) -> Vec<SettingText> {
    let base = |key: &str, a: String, b: String| SettingText {
        key: key.to_owned(),
        a,
        b,
        difference: "Different value",
    };
    let (a, b) = pair.base;
    let mut rows = Vec::new();
    if a.output != b.output {
        rows.push(base(
            "output",
            a.output.label().into(),
            b.output.label().into(),
        ));
    }
    // The effective policy holds the Simple controls when both jobs recorded it.
    if !(pair.effective_known.0 && pair.effective_known.1) {
        if a.preset != b.preset {
            rows.push(base(
                "preset (Simple)",
                a.preset.get().to_string(),
                b.preset.get().to_string(),
            ));
        }
        if a.crf != b.crf {
            rows.push(base(
                "rate factor (Simple)",
                a.crf.get().to_string(),
                b.crf.get().to_string(),
            ));
        }
    }
    rows.extend(pair.rows.iter().map(|row| SettingText {
        key: row.key.clone(),
        a: cell(&row.a),
        b: cell(&row.b),
        difference: match row.difference {
            ConfigDifference::Effective => "Different value",
            ConfigDifference::Declaration => "Same value, set differently",
            ConfigDifference::Requested => "Different request",
            ConfigDifference::OneSided => "Recorded by one job only",
        },
    }));
    rows
}

fn settings(session: &ComparisonSession) -> (String, Vec<SettingText>) {
    let pair = match session.configuration() {
        ConfigComparison::Known(pair) => pair,
        ConfigComparison::Unknown { a, b } => {
            let source = matches!(session.setup.a, Reference::EncodeSource);
            let text = match (a, b, source) {
                (_, true, true) => "Side A is the source. It has no encoder settings.",
                (false, false, _) => {
                    "No encode job is recorded for side A or side B. Studio does not infer settings from a stream."
                }
                (false, _, _) => {
                    "No encode job is recorded for side A. Studio does not infer settings from a stream."
                }
                _ => {
                    "No encode job is recorded for side B. Studio does not infer settings from a stream."
                }
            };
            return (text.to_owned(), Vec::new());
        }
    };
    let rows = setting_rows(&pair);
    let mut text = match rows.len() {
        0 => "The recorded settings are equal.".to_owned(),
        1 => format!("1 setting differs. {} are equal.", pair.same),
        n => format!("{n} settings differ. {} are equal.", pair.same),
    };
    let revision = |side: Side| {
        let codec = match side {
            Side::A => &pair.codec.0,
            Side::B => &pair.codec.1,
        };
        codec.as_ref().map(|codec| codec.codec_revision.as_str())
    };
    match (revision(Side::A), revision(Side::B)) {
        (Some(a), Some(b)) if a == b => {}
        (Some(a), Some(b)) => {
            let _ = write!(
                text,
                " The codec revisions differ: side A {}, side B {}.",
                short(a),
                short(b)
            );
        }
        _ => text.push_str(" A job did not record its codec revision."),
    }
    if !(pair.effective_known.0 && pair.effective_known.1) {
        text.push_str(
            " A job did not record its effective policy. Studio compares the explicit settings only.",
        );
    }
    (text, rows)
}

fn summary(view: &View) -> String {
    let mut text = String::new();
    for (title, side) in [("Side A", &view.a), ("Side B", &view.b)] {
        let _ = writeln!(text, "{title}: {}", side.name);
        for line in side.lines.iter().chain(&side.exact) {
            let _ = writeln!(text, "  {line}");
        }
    }
    let _ = writeln!(text, "Sample comparison: {}", view.verdict);
    for line in &view.verdict_details {
        let _ = writeln!(text, "  {line}");
    }
    let _ = writeln!(
        text,
        "Film grain: {}",
        if view.grain { "applied" } else { "not applied" }
    );
    let basis = view
        .basis_labels
        .get(view.basis_index)
        .copied()
        .unwrap_or("");
    let _ = writeln!(text, "Alignment ({basis}): {}", view.alignment);
    for line in &view.segments {
        let _ = writeln!(text, "  {line}");
    }
    let _ = writeln!(text, "Settings: {}", view.settings_status);
    for row in &view.settings {
        let _ = writeln!(
            text,
            "  {}: A {} | B {} | {}",
            row.key, row.a, row.b, row.difference
        );
    }
    if !view.notice.is_empty() {
        let _ = writeln!(text, "Notice: {}", view.notice);
    }
    text
}

/// The name and the detail of the comparison read in Jobs.
#[must_use]
pub fn job(status: Status<'_>) -> (String, String) {
    let Some(setup) = status.setup() else {
        return (String::new(), String::new());
    };
    let a = match &setup.a {
        Reference::Stream(input) => name_text(&input.path),
        Reference::EncodeSource => setup
            .b
            .encode
            .as_ref()
            .map_or_else(|| "No source".to_owned(), |e| name_text(&e.source.path)),
    };
    let detail = match status {
        Status::Ready(session) => format!(
            "Read {} of side A and {} of side B.",
            pictures(session.outputs(Side::A)),
            pictures(session.outputs(Side::B))
        ),
        Status::Failed { message, .. } => message.to_owned(),
        _ => "Reading both files. Each stream is decoded once.".to_owned(),
    };
    (format!("{a} and {}", name_text(&setup.b.path)), detail)
}

#[cfg(test)]
mod tests;

/// The text of the whole workspace.
#[must_use]
pub fn view(compare: &Compare) -> View {
    let mut view = View {
        state: State::Empty,
        message: String::new(),
        notice: compare.notice().unwrap_or_default().to_owned(),
        a: SideText::default(),
        b: SideText::default(),
        verdict: String::new(),
        verdict_problem: false,
        verdict_details: Vec::new(),
        basis_labels: Vec::new(),
        basis_index: 0,
        offset: None,
        offset_limit: 0,
        alignment: String::new(),
        alignment_problem: false,
        segments: Vec::new(),
        grain: false,
        settings_status: String::new(),
        settings: Vec::new(),
        summary: String::new(),
    };
    match compare.status() {
        Status::Empty([a, b]) => {
            (view.a, view.b) = (picked(a), picked(b));
            if a.is_some() || b.is_some() {
                "Choose the other AV2 file."
            } else {
                "In Encode, select a converted file and use Compare with Source. Or choose two AV2 files here."
            }
            .clone_into(&mut view.message);
        }
        Status::Reading(setup) => {
            view.state = State::Reading;
            (view.a, view.b) = sides(setup, None);
            "Studio reads both files. It decodes each stream once.".clone_into(&mut view.message);
        }
        Status::Failed { setup, message } => {
            view.state = State::Failed;
            (view.a, view.b) = sides(setup, None);
            message.clone_into(&mut view.message);
        }
        Status::Ready(session) => {
            view.state = State::Ready;
            (view.a, view.b) = sides(&session.setup, Some(session));
            (view.verdict, view.verdict_problem, view.verdict_details) = verdict(session);
            let bases = bases(&session.setup);
            view.basis_index = bases
                .iter()
                .position(|(basis, _)| *basis == session.setup.basis)
                .unwrap_or(0);
            view.basis_labels = bases.into_iter().map(|(_, label)| label).collect();
            if let AlignmentBasis::AssumedOffset { offset } = session.setup.basis {
                view.offset = Some(offset);
            }
            view.offset_limit = session.outputs(Side::A).max(session.outputs(Side::B));
            (view.alignment, view.alignment_problem, view.segments) = alignment(session);
            view.grain = session.setup.grain == GrainPolicy::Apply;
            (view.settings_status, view.settings) = settings(session);
            view.summary = summary(&view);
        }
    }
    view
}
