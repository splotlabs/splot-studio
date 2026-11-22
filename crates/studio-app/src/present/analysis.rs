// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Display text for analysis documents: rows, properties with their units
//! and provenance, and what reading the file found. Only what the engine
//! reported is shown; a value it did not report is said to be unknown,
//! never shown as zero.

use studio_domain::analysis::{
    DocumentSummary, Element, Framing, IvfHeaderFact, KindFilter, LayerFilter, LayerId, ObuFact,
    PacketFact, ProblemKind, Provenance, Row, RowFilter, ScanOutcome, Span,
};

use super::{count, file_name};
use crate::analysis::{Origin, Position, Selected};

/// One property of the inspector: a label, its value with units, and where
/// the value came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Property {
    /// What it is.
    pub label: String,
    /// The value, with its unit.
    pub value: String,
    /// Where it came from, or what it means; may be empty.
    pub note: String,
}

fn property(label: &str, value: impl Into<String>, note: impl Into<String>) -> Property {
    Property {
        label: label.to_owned(),
        value: value.into(),
        note: note.into(),
    }
}

/// `1 byte`, `1,234 bytes`.
#[must_use]
pub fn byte_count(n: u64) -> String {
    if n == 1 {
        "1 byte".to_owned()
    } else {
        format!("{} bytes", count(n))
    }
}

/// A file offset, exact in decimal and hexadecimal: `46 (0x2E)`.
#[must_use]
pub fn offset(n: u64) -> String {
    format!("{} (0x{n:X})", count(n))
}

/// A span: `7 bytes at 44 (0x2C)`; an empty span says where it would be.
#[must_use]
pub fn span(span: Span) -> String {
    if span.len == 0 {
        format!("none, at {}", offset(span.offset))
    } else {
        format!("{} at {}", byte_count(span.len), offset(span.offset))
    }
}

/// Where a value came from, in words.
#[must_use]
pub const fn provenance(provenance: Provenance) -> &'static str {
    match provenance {
        Provenance::Signaled => "signaled in the file",
        Provenance::Inferred => "inferred: the file does not carry it",
        Provenance::Located => "located by Splot",
        Provenance::Derived => "derived by Studio from Splot's values",
    }
}

/// The framing, for people.
#[must_use]
pub const fn framing(framing: Framing) -> &'static str {
    match framing {
        Framing::Ivf => "IVF",
        Framing::AnnexB => "Annex B",
    }
}

/// `IVF · 82 bytes · 3 packets, 4 OBUs`.
#[must_use]
pub fn summary_line(summary: &DocumentSummary) -> String {
    let mut parts = vec![
        framing(summary.framing).to_owned(),
        byte_count(summary.fingerprint.len),
    ];
    let mut counts = Vec::new();
    if summary.framing == Framing::Ivf {
        counts.push(plural(summary.packets, "packet", "packets"));
    }
    counts.push(plural(summary.obus, "OBU", "OBUs"));
    if summary.unparsed > 0 {
        counts.push(plural(
            summary.unparsed,
            "unparsed region",
            "unparsed regions",
        ));
    }
    parts.push(counts.join(", "));
    parts.join(" · ")
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{} {}", count(n), if n == 1 { one } else { many })
}

/// What reading found, and whether it is a problem.
#[must_use]
pub fn outcome(summary: &DocumentSummary) -> (String, bool) {
    match &summary.outcome {
        ScanOutcome::Complete => ("Every byte belongs to a parsed element.".to_owned(), false),
        ScanOutcome::Damaged { first } => (
            format!(
                "Read to the end, but {} did not parse. The first is at byte {}: {}",
                plural(summary.unparsed, "region", "regions"),
                offset(first.offset),
                first.message
            ),
            true,
        ),
        ScanOutcome::Stopped { problem } => (
            format!(
                "Reading stopped at byte {} ({}): {} The rest of the file is one unparsed region.",
                offset(problem.offset),
                problem_kind(problem.kind),
                sentence(&problem.message)
            ),
            true,
        ),
    }
}

fn sentence(text: &str) -> String {
    let text = text.trim_end();
    if text.ends_with('.') {
        text.to_owned()
    } else {
        format!("{text}.")
    }
}

/// A problem's kind, in words.
#[must_use]
pub const fn problem_kind(kind: ProblemKind) -> &'static str {
    match kind {
        ProblemKind::Malformed => "malformed",
        ProblemKind::Unsupported => "not supported",
        ProblemKind::Io => "read failure",
        ProblemKind::LimitReached => "engine limit",
    }
}

/// A row's title: `OBU 3 · OBU_SEQUENCE_HEADER`.
#[must_use]
pub fn row_title(row: &Row) -> String {
    match &row.element {
        Element::IvfHeader { .. } => "IVF file header".to_owned(),
        Element::Packet { packet } => format!("IVF record {}", count(packet.index)),
        Element::Obu { obu } => format!("OBU {} · {}", count(obu.index), obu.type_name),
        Element::Unparsed { kind, .. } => format!("Unparsed · {}", problem_kind(*kind)),
    }
}

/// A row's second column: its size, and for an OBU its layer ids.
#[must_use]
pub fn row_detail(row: &Row) -> String {
    match &row.element {
        Element::IvfHeader { span } | Element::Unparsed { span, .. } => byte_count(span.len),
        Element::Packet { packet } => format!("payload {}", byte_count(packet.payload.len)),
        Element::Obu { obu } => format!(
            "{} · T{} M{}",
            byte_count(obu.obu.len),
            obu.temporal_layer_id.value,
            obu.embedded_layer_id.value
        ),
    }
}

/// A row's first byte in hexadecimal: `0x2C`.
#[must_use]
pub fn row_offset(row: &Row) -> String {
    format!("0x{:X}", crate::analysis::extent(&row.element).0)
}

/// One line that names a row, for screen readers and the clipboard.
#[must_use]
pub fn row_line(row: &Row) -> String {
    let (start, end) = crate::analysis::extent(&row.element);
    format!(
        "{}, {}, bytes {} to {}",
        row_title(row),
        row_detail(row),
        offset(start),
        offset(end.saturating_sub(1).max(start))
    )
}

/// The type filter's label for one choice.
#[must_use]
pub fn kind_label(kind: KindFilter, summary: Option<&DocumentSummary>) -> String {
    match kind {
        KindFilter::All => "All elements".to_owned(),
        KindFilter::Unparsed => format!(
            "Unparsed regions ({})",
            count(summary.map_or(0, |s| s.unparsed))
        ),
        KindFilter::Obu { obu_type } => {
            let found = summary.and_then(|s| s.obu_types.iter().find(|t| t.obu_type == obu_type));
            match found {
                Some(t) => format!("{} ({})", t.type_name, count(t.count)),
                None => format!("OBU type {obu_type}"),
            }
        }
    }
}

/// The layer filter's label for one choice.
#[must_use]
pub fn layer_label(layer: Option<LayerFilter>, summary: Option<&DocumentSummary>) -> String {
    match layer {
        None => "All layers".to_owned(),
        Some(layer) => {
            let n = summary
                .and_then(|s| {
                    s.layers
                        .iter()
                        .find(|l| l.temporal == layer.temporal && l.embedded == layer.embedded)
                })
                .map_or(0, |l| l.count);
            format!(
                "tlayer {} · mlayer {} ({})",
                layer.temporal,
                layer.embedded,
                count(n)
            )
        }
    }
}

/// `5 rows`, or `2 of 8 elements` under a filter.
#[must_use]
pub fn rows_line(filter: RowFilter, total: Option<u64>, elements: u64) -> String {
    match total {
        None => "Loading rows…".to_owned(),
        Some(total) if filter.is_all() => plural(total, "element", "elements"),
        Some(0) => "No element matches the filters.".to_owned(),
        Some(total) => format!(
            "{} of {} match",
            count(total),
            plural(elements, "element", "elements")
        ),
    }
}

/// Why the selection is not in the list, when the filter hides it.
#[must_use]
pub fn hidden_note(selected: Option<&Selected>) -> String {
    match selected {
        Some(Selected {
            row,
            position: Position::Hidden,
        }) => format!(
            "The selection, {}, is hidden by the filters. It stays selected; clear the filters \
             to see it in the list.",
            row_title(row)
        ),
        _ => String::new(),
    }
}

/// The inspector's title and properties: the selection's, or the
/// document's when nothing is selected.
#[must_use]
pub fn properties(
    summary: &DocumentSummary,
    origin: &Origin,
    path: &std::path::Path,
    selected: Option<&Selected>,
) -> (String, Vec<Property>) {
    match selected {
        None => ("File".to_owned(), file_properties(summary, origin, path)),
        Some(selected) => (
            row_title(&selected.row),
            element_properties(summary, &selected.row),
        ),
    }
}

fn file_properties(
    summary: &DocumentSummary,
    origin: &Origin,
    path: &std::path::Path,
) -> Vec<Property> {
    let mut rows = vec![
        property("File", file_name(path), path.display().to_string()),
        property("Size", byte_count(summary.fingerprint.len), ""),
        property(
            "Wrapper",
            framing(summary.framing),
            "identified from the contents",
        ),
        property("Elements", count(summary.elements), ""),
    ];
    if summary.framing == Framing::Ivf {
        rows.push(property(
            "Packets",
            count(summary.packets),
            "IVF frame records",
        ));
    }
    rows.push(property("OBUs", count(summary.obus), ""));
    rows.push(property("Unparsed", count(summary.unparsed), "regions"));
    if let Origin::Result(result) = origin {
        rows.push(property(
            "Converted from",
            file_name(&result.source),
            result.source.display().to_string(),
        ));
        rows.push(property(
            "Verified size",
            byte_count(result.bytes),
            "when the conversion published it",
        ));
    }
    if let Some(header) = &summary.ivf_header {
        rows.extend(ivf_header_properties(header));
    }
    rows
}

fn ivf_header_properties(header: &IvfHeaderFact) -> Vec<Property> {
    let signaled = provenance(Provenance::Signaled);
    let fourcc = String::from_utf8_lossy(&header.fourcc).into_owned();
    vec![
        property("Codec", fourcc, "IVF fourcc, signaled"),
        property("Version", header.version.to_string(), signaled),
        property("Header", byte_count(u64::from(header.header_len)), signaled),
        property(
            "Frame size",
            format!("{} × {} pixels", header.width, header.height),
            "as the IVF header states; not checked against the sequence header",
        ),
        property(
            "Time base",
            time_base(header.timebase_numerator, header.timebase_denominator),
            signaled,
        ),
        property(
            "Frame count",
            count(u64::from(header.frame_count)),
            "declared by the writer; not a count of the records",
        ),
    ]
}

fn time_base(numerator: u32, denominator: u32) -> String {
    if denominator == 0 {
        format!("{numerator}/0 s: not a valid time base")
    } else {
        format!("{numerator}/{denominator} s")
    }
}

fn element_properties(summary: &DocumentSummary, row: &Row) -> Vec<Property> {
    let mut rows = vec![property(
        "Element",
        count(row.index),
        "index in this document",
    )];
    match &row.element {
        Element::IvfHeader { span: s } => {
            rows.push(property("Bytes", span(*s), provenance(s.provenance)));
            if let Some(header) = &summary.ivf_header {
                rows.extend(ivf_header_properties(header));
            }
        }
        Element::Packet { packet } => rows.extend(packet_properties(summary, packet)),
        Element::Obu { obu } => rows.extend(obu_properties(obu, row.parent)),
        Element::Unparsed {
            span: s,
            kind,
            message,
        } => {
            rows.push(property(
                "Kind",
                problem_kind(*kind),
                "nothing in it is interpreted",
            ));
            rows.push(property("Bytes", span(*s), provenance(s.provenance)));
            rows.push(property("Reason", message.clone(), ""));
            if let Some(parent) = row.parent {
                rows.push(property(
                    "In packet",
                    format!("element {}", count(parent)),
                    "",
                ));
            }
        }
    }
    rows
}

fn packet_properties(summary: &DocumentSummary, packet: &PacketFact) -> Vec<Property> {
    let mut rows = vec![
        property(
            "Record",
            count(packet.index),
            "IVF frame record, in file order",
        ),
        property(
            "Timestamp",
            format!("{} time base units", count(packet.pts)),
            "signaled",
        ),
    ];
    if let Some(header) = &summary.ivf_header
        && header.timebase_denominator > 0
    {
        // A display hint; the exact value is the line above.
        #[allow(clippy::cast_precision_loss)]
        let seconds = packet.pts as f64 * f64::from(header.timebase_numerator)
            / f64::from(header.timebase_denominator);
        rows.push(property(
            "Time",
            format!("≈ {seconds:.3} s"),
            "derived from the timestamp and the time base",
        ));
    }
    rows.push(property(
        "Record header",
        span(packet.header),
        provenance(packet.header.provenance),
    ));
    rows.push(property(
        "Payload",
        span(packet.payload),
        provenance(packet.payload.provenance),
    ));
    rows.push(property(
        "In the stream",
        span(packet.stream),
        "the payloads one after another; derived by Studio",
    ));
    rows
}

fn layer(id: LayerId) -> (String, &'static str) {
    (id.value.to_string(), provenance(id.provenance))
}

fn obu_properties(obu: &ObuFact, parent: Option<u64>) -> Vec<Property> {
    let mut rows = vec![
        property("OBU", count(obu.index), "index among the OBUs"),
        property(
            "Type",
            obu.type_name.clone(),
            format!(
                "obu_type {}{}",
                obu.obu_type,
                if obu.reserved { ", reserved" } else { "" }
            ),
        ),
        property(
            "Extension",
            if obu.has_extension {
                "present"
            } else {
                "absent"
            },
            "obu_header_extension_flag, signaled",
        ),
    ];
    for (label, id) in [
        ("tlayer", obu.temporal_layer_id),
        ("mlayer", obu.embedded_layer_id),
        ("xlayer", obu.extended_layer_id),
    ] {
        let (value, note) = layer(id);
        rows.push(property(label, value, note));
    }
    if let Some(record) = obu.packet {
        let place = parent.map_or_else(String::new, |p| format!(", element {}", count(p)));
        rows.push(property(
            "In packet",
            format!("IVF record {}{place}", count(record)),
            "",
        ));
    }
    for (label, s) in [
        ("Length prefix", obu.length_prefix),
        ("OBU", obu.obu),
        ("Header", obu.header),
        ("Payload", obu.payload),
    ] {
        rows.push(property(label, span(s), provenance(s.provenance)));
    }
    rows.push(property(
        "In the stream",
        span(obu.stream),
        "length prefix and OBU in the elementary stream; derived by Studio",
    ));
    rows
}

/// The text of one row of the property list for the clipboard.
#[must_use]
pub fn property_line(property: &Property) -> String {
    format!("{}: {}", property.label, property.value)
}

#[cfg(test)]
mod tests {
    use studio_domain::analysis::{LayerCount, ObuTypeCount, Problem};
    use studio_domain::job::SourceFingerprint;

    use super::*;

    fn summary(outcome: ScanOutcome, header: Option<IvfHeaderFact>) -> DocumentSummary {
        DocumentSummary {
            fingerprint: SourceFingerprint {
                len: 82,
                modified: None,
            },
            framing: if header.is_some() {
                Framing::Ivf
            } else {
                Framing::AnnexB
            },
            ivf_header: header,
            elements: 8,
            packets: 3,
            obus: 4,
            unparsed: 1,
            obu_types: vec![ObuTypeCount {
                obu_type: 2,
                type_name: "OBU_TEMPORAL_DELIMITER".into(),
                count: 2,
            }],
            layers: vec![LayerCount {
                temporal: 1,
                embedded: 2,
                count: 1,
            }],
            outcome,
        }
    }

    #[test]
    fn spans_and_offsets_are_exact_with_units() {
        let s = Span {
            offset: 0x2C,
            len: 7,
            provenance: Provenance::Located,
        };
        assert_eq!(span(s), "7 bytes at 44 (0x2C)");
        assert_eq!(span(Span { len: 1, ..s }), "1 byte at 44 (0x2C)");
        assert_eq!(span(Span { len: 0, ..s }), "none, at 44 (0x2C)");
        assert_eq!(
            offset(u64::MAX),
            "18,446,744,073,709,551,615 (0xFFFFFFFFFFFFFFFF)"
        );
    }

    #[test]
    fn the_outcome_says_where_reading_stopped() {
        let stopped = summary(
            ScanOutcome::Stopped {
                problem: Problem {
                    kind: ProblemKind::Malformed,
                    offset: 7,
                    message: "the OBU does not parse".into(),
                },
            },
            None,
        );
        let (text, problem) = outcome(&stopped);
        assert!(problem);
        assert_eq!(
            text,
            "Reading stopped at byte 7 (0x7) (malformed): the OBU does not parse. The rest of the \
             file is one unparsed region."
        );
        assert!(!outcome(&summary(ScanOutcome::Complete, None)).1);
        assert_eq!(
            summary_line(&summary(ScanOutcome::Complete, None)),
            "Annex B · 82 bytes · 4 OBUs, 1 unparsed region"
        );
    }

    /// A header that states zero says it states zero; nothing is filled in.
    #[test]
    fn ivf_header_values_are_what_the_header_states() {
        let header = IvfHeaderFact {
            version: 0,
            header_len: 32,
            fourcc: *b"AV02",
            width: 0,
            height: 0,
            timebase_numerator: 1,
            timebase_denominator: 0,
            frame_count: 0,
        };
        let rows = file_properties(
            &summary(ScanOutcome::Complete, Some(header)),
            &Origin::File,
            std::path::Path::new("/m/a.ivf"),
        );
        let find = |label: &str| rows.iter().find(|r| r.label == label).cloned();
        let size = find("Frame size").unwrap_or_else(|| unreachable!());
        assert_eq!(size.value, "0 × 0 pixels");
        assert!(size.note.contains("as the IVF header states"));
        assert_eq!(
            find("Time base").map(|r| r.value),
            Some("1/0 s: not a valid time base".into())
        );
        assert!(find("Frame count").is_some_and(|r| r.note.contains("not a count of the records")));
    }

    #[test]
    fn filter_labels_carry_their_counts() {
        let s = summary(ScanOutcome::Complete, None);
        assert_eq!(
            kind_label(KindFilter::Obu { obu_type: 2 }, Some(&s)),
            "OBU_TEMPORAL_DELIMITER (2)"
        );
        assert_eq!(
            kind_label(KindFilter::Unparsed, Some(&s)),
            "Unparsed regions (1)"
        );
        assert_eq!(
            layer_label(
                Some(LayerFilter {
                    temporal: 1,
                    embedded: 2
                }),
                Some(&s)
            ),
            "tlayer 1 · mlayer 2 (1)"
        );
        assert_eq!(rows_line(RowFilter::default(), Some(8), 8), "8 elements");
        assert_eq!(rows_line(RowFilter::default(), None, 8), "Loading rows…");
        let tds = RowFilter {
            kind: KindFilter::Obu { obu_type: 2 },
            layer: None,
        };
        assert_eq!(rows_line(tds, Some(2), 8), "2 of 8 elements match");
        assert_eq!(
            rows_line(tds, Some(0), 8),
            "No element matches the filters."
        );
    }
}
