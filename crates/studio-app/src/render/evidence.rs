// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Display values for the bounded Analyze evidence list.

use std::fmt::Write as _;

use studio_domain::syntax::{SyntaxEvent, SyntaxSpan, SyntaxStatus, SyntaxValue};
use studio_domain::validation::{Finding, Severity, ValidationCompletion};

use super::model;
use crate::analysis::evidence::{Evidence, VIEW_ROWS};
use crate::ui::{Analysis, EvidenceRow};

pub(super) fn render(global: &Analysis<'_>, evidence: &Evidence) {
    global.set_evidence_tab(evidence.tab);
    global.set_evidence_query(evidence.query().into());
    global.set_evidence_rule_query(evidence.rule_query.as_str().into());
    global.set_severity_index(evidence.severity);
    global.set_finding_layer_index(evidence.layer);
    global.set_finding_layers(model(
        std::iter::once("All layers".into())
            .chain((0..32).map(|layer| format!("Extended layer {layer}").into()))
            .chain(std::iter::once("Layer unavailable".into()))
            .collect(),
    ));
    global.set_validation_running(evidence.running.is_some());
    global.set_syntax_status(syntax_status(evidence).into());
    global.set_findings_status(findings_status(evidence).into());

    let indices = if evidence.tab == 0 {
        Vec::new()
    } else {
        evidence.matching()
    };
    let selected = evidence.selected_index();
    let selected_position = selected.and_then(|index| indices.binary_search(&index).ok());
    let start = evidence.first.min(indices.len());
    let end = (start + VIEW_ROWS).min(indices.len());
    global.set_evidence_can_prev(start > 0);
    global.set_evidence_can_next(end < indices.len());
    global.set_finding_can_prev(evidence.tab == 2 && selected_position.is_some_and(|p| p > 0));
    global.set_finding_can_next(
        evidence.tab == 2
            && selected_position.map_or(!indices.is_empty(), |p| p + 1 < indices.len()),
    );
    let page = if end > start {
        format!("{}–{} of {}", start + 1, end, indices.len())
    } else if evidence.tab != 0 && (!evidence.events.is_empty() || !evidence.findings.is_empty()) {
        "No matching rows.".to_owned()
    } else {
        String::new()
    };
    global.set_evidence_page_label(page.into());
    global.set_evidence_rows(model(
        indices[start..end]
            .iter()
            .map(|&index| {
                let (depth, label, detail) = if evidence.tab == 2 {
                    let finding = &evidence.findings[index];
                    (
                        0,
                        format!("{} · {}", severity(finding.severity), finding.rule_id),
                        finding.message.clone(),
                    )
                } else {
                    let (depth, event) = &evidence.events[index];
                    let (label, detail) = syntax_row(event);
                    (*depth, label, detail)
                };
                EvidenceRow {
                    depth,
                    selected: selected == Some(index),
                    accessible: format!("{label}. {detail}").into(),
                    label: label.into(),
                    detail: detail.into(),
                }
            })
            .collect(),
    ));
    let (detail, note) = selected.map_or_else(
        || (String::new(), String::new()),
        |index| {
            let hidden = selected_position
                .is_none()
                .then_some("The current filters hide this selection.");
            if evidence.tab == 2 {
                finding_detail(&evidence.findings[index], evidence, hidden)
            } else {
                let (_, event) = &evidence.events[index];
                let (name, value) = syntax_row(event);
                let location = evidence.exact_span().map_or_else(
                    || "No signaled span is available for this row.".to_owned(),
                    span_text,
                );
                (format!("{name}\n{value}"), append_note(&location, hidden))
            }
        },
    );
    let copy = if note.is_empty() {
        detail.clone()
    } else {
        format!("{detail}\n{note}")
    };
    global.set_evidence_detail(detail.into());
    global.set_evidence_note(note.into());
    global.set_evidence_copy(copy.into());
}

fn append_note(note: &str, extra: Option<&str>) -> String {
    extra.map_or_else(|| note.to_owned(), |extra| format!("{note}\n{extra}"))
}

fn syntax_status(evidence: &Evidence) -> String {
    if let Some(error) = &evidence.syntax_error {
        return format!("Syntax read failed. {error}");
    }
    let Some((_, summary)) = &evidence.syntax else {
        return if evidence.syntax_loading {
            "Reading OBU syntax…"
        } else {
            "Select an OBU to inspect its syntax."
        }
        .to_owned();
    };
    let mut status = match &summary.status {
        SyntaxStatus::Parsed => "The codec parsed the modeled syntax.".to_owned(),
        SyntaxStatus::Prefix { blocked_on, .. } => {
            format!("The codec parsed a prefix. The remainder requires {blocked_on}.")
        }
        SyntaxStatus::Opaque => "The payload is opaque. Header fields remain available.".to_owned(),
        SyntaxStatus::Unsupported { feature } => format!("The codec does not support {feature}."),
        SyntaxStatus::UnknownExtension => {
            "The codec could not interpret the OBU extension.".to_owned()
        }
        SyntaxStatus::ResourceLimited => {
            "This OBU exceeds the syntax read limit. Header fields remain available.".to_owned()
        }
        SyntaxStatus::Malformed => {
            "A syntax error stopped parsing. Earlier fields remain available.".to_owned()
        }
    };
    if summary.elements_truncated || summary.depth_truncated {
        status.push_str(" The trace omits some observations.");
    }
    if evidence.syntax_loading {
        status.push_str(" Reading more rows…");
    }
    if let Some(error) = &summary.error {
        status.push(' ');
        status.push_str(&error.message);
    }
    status
}

fn findings_status(evidence: &Evidence) -> String {
    if evidence.running.is_some() {
        return if evidence.cancelling {
            "Stopping validation…"
        } else {
            "Waiting for the validation result…"
        }
        .to_owned();
    }
    let Some((_, report)) = &evidence.validation else {
        return evidence.findings_error.as_ref().map_or_else(
            || "Validate this stream to inspect findings.".to_owned(),
            |error| format!("Validation failed. {error}"),
        );
    };
    let status = match &report.completion {
        ValidationCompletion::EndOfInput if report.malformed_units != 0 => {
            "Validation finished with incomplete framing coverage.".to_owned()
        }
        ValidationCompletion::EndOfInput if report.errors > 0 => {
            "Validation completed with errors.".to_owned()
        }
        ValidationCompletion::EndOfInput if report.warnings > 0 => {
            "Validation completed with warnings.".to_owned()
        }
        ValidationCompletion::EndOfInput => "No errors found in the completed checks.".to_owned(),
        ValidationCompletion::MalformedInput => "Malformed framing stopped validation.".to_owned(),
        ValidationCompletion::Cancelled => "Validation stopped at your request.".to_owned(),
        ValidationCompletion::Io { message } => {
            format!("A read error stopped validation. {message}")
        }
        ValidationCompletion::UnitTooLarge { offset, .. } => {
            format!("A unit at byte {offset} exceeded the size limit.")
        }
        ValidationCompletion::ObuLimit { max_obus } => {
            format!("Validation reached the limit of {max_obus} OBUs.")
        }
        ValidationCompletion::Unsupported => {
            "The engine returned an unsupported completion state.".to_owned()
        }
    };
    let end = if report.finalized {
        "End checks ran."
    } else {
        "End checks did not run."
    };
    let mut text = format!(
        "{status} {} errors, {} warnings, {} informational findings. {} OBUs checked. {end} {}",
        report.errors, report.warnings, report.info, report.checked_obus, report.scope
    );
    if report.strict && report.warnings > 0 {
        text.push_str(" Strict policy treats warnings as a failure.");
    }
    if report.omitted > 0 {
        let _ = write!(text, " The report omits {} findings.", report.omitted);
    }
    if evidence.findings_loading {
        text.push_str(" Reading more rows…");
    }
    if let Some(error) = &evidence.findings_error {
        let _ = write!(
            text,
            " The engine could not supply all retained findings. {error}"
        );
    }
    text
}

fn severity(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "Error",
        Severity::Warning => "Warning",
        Severity::Info => "Info",
    }
}

fn finding_detail(
    finding: &Finding,
    evidence: &Evidence,
    hidden: Option<&str>,
) -> (String, String) {
    let spec = evidence
        .validation
        .as_ref()
        .map_or("AV2", |(_, report)| report.spec_revision.as_str());
    let reference = finding.spec_section.as_ref().map_or_else(
        || spec.to_owned(),
        |section| format!("{spec} § {}", section.trim_start_matches('§').trim()),
    );
    let detail = format!(
        "{} · {}\n{}\n{reference}",
        severity(finding.severity),
        finding.rule_id,
        finding.message
    );
    let location = match (finding.file_offset, finding.bit_offset) {
        (Some(byte), Some(bit)) => format!("File byte {byte}, bit {bit}."),
        (Some(byte), None) => format!("File byte {byte}. Field-level evidence is unavailable."),
        (None, _) => "The validator supplied no location.".to_owned(),
    };
    let location = if let Some((name, span)) = evidence.matched_field() {
        if finding.file_offset == Some(span.start.file_bit / 8)
            && finding.bit_offset.map(u64::from) == Some(span.start.file_bit % 8)
        {
            format!("{name}. {}", span_text(span))
        } else {
            format!("{location}\n{name}. {}", span_text(span))
        }
    } else if finding.file_offset.is_some() && finding.bit_offset.is_some() {
        let status = if evidence.syntax_loading {
            "Reading field evidence…"
        } else {
            "Field-level evidence is unavailable."
        };
        format!("{location} {status}")
    } else {
        location
    };
    (detail, append_note(&location, hidden))
}

fn span_text(span: SyntaxSpan) -> String {
    format!(
        "File byte {}, bit {}. Stream byte {}, bit {}. Length {} {}.",
        span.start.file_bit / 8,
        span.start.file_bit % 8,
        span.start.stream_bit / 8,
        span.start.stream_bit % 8,
        span.bit_len,
        if span.bit_len == 1 { "bit" } else { "bits" }
    )
}

fn syntax_row(event: &SyntaxEvent) -> (String, String) {
    match event {
        SyntaxEvent::Structure { name, spec_section } => (
            name.clone(),
            spec_section
                .as_ref()
                .map_or_else(String::new, |spec| format!("AV2 § {spec}")),
        ),
        SyntaxEvent::LoopItem { name, index } => {
            (format!("{name}[{index}]"), "Loop item".to_owned())
        }
        SyntaxEvent::Condition { condition, taken } => (
            condition.clone(),
            if *taken {
                "Branch taken"
            } else {
                "Branch not taken"
            }
            .to_owned(),
        ),
        SyntaxEvent::Signaled {
            name,
            descriptor,
            value,
            ..
        } => (
            name.clone(),
            format!("{} · {descriptor}", value_text(*value)),
        ),
        SyntaxEvent::Absent {
            name,
            condition,
            inferred,
        } => (
            name.clone(),
            inferred.map_or_else(
                || format!("Absent. {condition}"),
                |value| format!("Inferred {}. {condition}", value_text(value)),
            ),
        ),
        _ => ("Syntax unavailable".to_owned(), String::new()),
    }
}

fn value_text(value: SyntaxValue) -> String {
    match value {
        SyntaxValue::Bool(value) => format!("{} ({value})", u8::from(value)),
        SyntaxValue::Unsigned(value) => value.to_string(),
        SyntaxValue::Signed(value) => value.to_string(),
        SyntaxValue::Bytes(len) => format!("{len} opaque bytes"),
        SyntaxValue::Unavailable => "Value unavailable".to_owned(),
    }
}
