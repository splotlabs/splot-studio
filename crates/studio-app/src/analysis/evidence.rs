// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Per-document syntax and validation reports, read in bounded protocol pages.

use studio_domain::analysis::{Element, LocateTarget, Row};
use studio_domain::syntax::{SyntaxEvent, SyntaxSpan, SyntaxSummary};
use studio_domain::validation::{Finding, Severity, ValidationSettings, ValidationSummary};

use super::document::{in_window, line_of};
use super::{Analysis, Asked, Command, Effect, Event, Position, Purpose, Step};

pub(crate) const VIEW_ROWS: usize = 8;
const WIRE_ROWS: u32 = 128;
const MAX_SYNTAX_EVENTS: u64 = 32_768;
const MAX_FINDINGS: u64 = 4_096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Request {
    Syntax(u64),
    SyntaxPage { trace: u64, first: u64 },
    Validate,
    Findings { report: u64, first: u64 },
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Pick {
    Syntax { element: u64, index: usize },
    Finding { report: u64, index: usize },
}

#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Evidence {
    pub(crate) tab: i32,
    pub(crate) syntax_query: String,
    pub(crate) finding_query: String,
    pub(crate) rule_query: String,
    pub(crate) severity: i32,
    pub(crate) layer: i32,
    pub(crate) first: usize,
    pub(crate) syntax: Option<(u64, SyntaxSummary)>,
    pub(crate) events: Vec<(i32, SyntaxEvent)>,
    pub(crate) validation: Option<(u64, ValidationSummary)>,
    pub(crate) findings: Vec<Finding>,
    pub(crate) running: Option<u64>,
    pub(crate) cancelling: bool,
    pub(crate) syntax_loading: bool,
    pub(crate) findings_loading: bool,
    pub(crate) syntax_error: Option<String>,
    pub(crate) findings_error: Option<String>,
    pub(super) selected: Option<Pick>,
    trace: Option<u64>,
    findings_total: u64,
    syntax_request: Option<(u64, u64)>,
    depth: i32,
}

impl Evidence {
    pub(super) fn has_finding(&self, pick: Pick) -> bool {
        matches!(pick, Pick::Finding { report, index } if self.validation.as_ref().is_some_and(|(current, _)| *current == report)
            && self.findings.get(index).is_some())
    }

    pub(super) fn fail(&mut self, id: u64, request: Request, message: String) {
        match request {
            Request::Syntax(element) if self.syntax_request == Some((id, element)) => {
                self.syntax_request = None;
                self.syntax_loading = false;
                self.syntax_error = Some(message);
            }
            Request::SyntaxPage { trace, .. } if self.trace == Some(trace) => {
                self.syntax_loading = false;
                self.syntax_error = Some(message);
            }
            Request::Validate if self.running == Some(id) => {
                self.running = None;
                self.cancelling = false;
                self.findings_error = Some(message);
            }
            Request::Findings { report, .. }
                if self
                    .validation
                    .as_ref()
                    .is_some_and(|(current, _)| *current == report) =>
            {
                self.findings_loading = false;
                self.findings_error = Some(message);
            }
            _ => {}
        }
    }

    pub(crate) fn query(&self) -> &str {
        if self.tab == 2 {
            &self.finding_query
        } else {
            &self.syntax_query
        }
    }

    pub(crate) fn matching(&self) -> Vec<usize> {
        let query = self.query().to_lowercase();
        if self.tab == 2 {
            let rule = self.rule_query.to_lowercase();
            self.findings
                .iter()
                .enumerate()
                .filter_map(|(index, finding)| {
                    let severity = match finding.severity {
                        Severity::Error => 1,
                        Severity::Warning => 2,
                        Severity::Info => 3,
                    };
                    let layer = finding.extended_layer_id.map_or(33, |id| i32::from(id) + 1);
                    let rule_id = finding.rule_id.to_lowercase();
                    ((self.severity == 0 || self.severity == severity)
                        && (self.layer == 0 || self.layer == layer)
                        && rule_id.contains(&rule)
                        && (rule_id.contains(&query)
                            || finding.message.to_lowercase().contains(&query)))
                    .then_some(index)
                })
                .collect()
        } else {
            self.events
                .iter()
                .enumerate()
                .filter_map(|(index, (_, event))| {
                    let label = match event {
                        SyntaxEvent::Structure { name, .. }
                        | SyntaxEvent::LoopItem { name, .. }
                        | SyntaxEvent::Signaled { name, .. }
                        | SyntaxEvent::Absent { name, .. } => name.as_str(),
                        SyntaxEvent::Condition { condition, .. } => condition.as_str(),
                        SyntaxEvent::ExitScope => return None,
                        SyntaxEvent::UnknownScope | SyntaxEvent::UnknownEvent => "Unknown syntax",
                    };
                    (query.is_empty() || label.to_lowercase().contains(&query)).then_some(index)
                })
                .collect()
        }
    }

    pub(crate) fn selected_index(&self) -> Option<usize> {
        match self.selected {
            Some(Pick::Syntax { element, index })
                if self.tab == 1
                    && self
                        .syntax
                        .as_ref()
                        .is_some_and(|(current, _)| *current == element) =>
            {
                Some(index)
            }
            Some(Pick::Finding { report, index })
                if self.tab == 2
                    && self
                        .validation
                        .as_ref()
                        .is_some_and(|(current, _)| *current == report) =>
            {
                Some(index)
            }
            Some(Pick::Finding { .. }) if self.tab == 1 => self.matched_index(),
            _ => None,
        }
    }

    fn append_syntax(&mut self, events: Vec<SyntaxEvent>) {
        for event in events {
            if event == SyntaxEvent::ExitScope {
                self.depth = self.depth.saturating_sub(1);
            } else {
                let scope = matches!(
                    event,
                    SyntaxEvent::Structure { .. }
                        | SyntaxEvent::LoopItem { .. }
                        | SyntaxEvent::Condition { .. }
                        | SyntaxEvent::UnknownScope
                );
                self.events.push((self.depth, event));
                if scope {
                    self.depth += 1;
                }
            }
        }
    }

    pub(super) fn reveal_selection(&mut self) {
        if let Some(index) = self.selected_index()
            && let Ok(position) = self.matching().binary_search(&index)
        {
            self.first = position / VIEW_ROWS * VIEW_ROWS;
        }
    }

    fn matched_index(&self) -> Option<usize> {
        let Pick::Finding { report, index } = self.selected? else {
            return None;
        };
        if self.validation.as_ref()?.0 != report {
            return None;
        }
        let finding = self.findings.get(index)?;
        let bit = finding
            .file_offset?
            .checked_mul(8)?
            .checked_add(u64::from(finding.bit_offset.filter(|bit| *bit < 8)?))?;
        self.events
            .iter()
            .enumerate()
            .filter_map(|(index, (_, event))| {
                let SyntaxEvent::Signaled {
                    span: Some(span), ..
                } = event
                else {
                    return None;
                };
                (bit >= span.start.file_bit && bit - span.start.file_bit < span.bit_len)
                    .then_some((index, span.bit_len))
            })
            .min_by_key(|(_, len)| *len)
            .map(|(index, _)| index)
    }

    pub(crate) fn matched_field(&self) -> Option<(&str, SyntaxSpan)> {
        match &self.events.get(self.matched_index()?)?.1 {
            SyntaxEvent::Signaled {
                name,
                span: Some(span),
                ..
            } => Some((name, *span)),
            _ => None,
        }
    }

    pub(crate) fn exact_span(&self) -> Option<SyntaxSpan> {
        let index = match self.selected? {
            Pick::Syntax { element, index } if self.syntax.as_ref()?.0 == element => index,
            Pick::Finding { .. } => return self.matched_field().map(|(_, span)| span),
            Pick::Syntax { .. } => return None,
        };
        match &self.events.get(index)?.1 {
            SyntaxEvent::Signaled { span, .. } => *span,
            _ => None,
        }
    }
}

impl Analysis {
    pub(super) fn ensure_syntax(&mut self, at: usize, out: &mut Vec<Effect>) {
        let doc = &mut self.documents[at];
        let document_id = doc.id;
        let need =
            doc.evidence.tab == 1 || matches!(doc.evidence.selected, Some(Pick::Finding { .. }));
        let element = doc
            .selected
            .as_ref()
            .filter(|s| matches!(s.row.element, Element::Obu { .. }))
            .map(|s| s.row.index);
        let same = element.is_some()
            && (doc
                .evidence
                .syntax_request
                .is_some_and(|(_, current)| Some(current) == element)
                || doc
                    .evidence
                    .syntax
                    .as_ref()
                    .is_some_and(|(current, _)| Some(*current) == element));
        if same {
            return;
        }
        doc.evidence.syntax = None;
        doc.evidence.events.clear();
        doc.evidence.trace = None;
        doc.evidence.syntax_request = None;
        doc.evidence.syntax_loading = false;
        doc.evidence.syntax_error = None;
        if doc.evidence.tab == 1 {
            doc.evidence.first = 0;
        }
        if let Some(element) = element.filter(|_| need) {
            let id = self.send(
                document_id,
                Asked::Evidence(Request::Syntax(element)),
                out,
                |document| Command::AnalysisSyntax { document, element },
            );
            let evidence = &mut self.documents[at].evidence;
            evidence.syntax_request = Some((id, element));
            evidence.syntax_loading = true;
        }
    }

    pub(super) fn evidence_event(
        &mut self,
        at: usize,
        id: u64,
        request: Request,
        event: Event,
        out: &mut Vec<Effect>,
    ) {
        let doc = &mut self.documents[at];
        let evidence = &mut doc.evidence;
        match (request, event) {
            (
                Request::Syntax(expected),
                Event::AnalysisSyntaxReady {
                    element,
                    summary,
                    total,
                    ..
                },
            ) if evidence.syntax_request == Some((id, expected)) && expected == element => {
                self.syntax_ready(at, id, element, *summary, total, out);
            }
            (
                Request::SyntaxPage {
                    trace,
                    first: expected,
                },
                Event::AnalysisSyntaxPage {
                    trace: actual,
                    element,
                    first,
                    total,
                    events,
                    ..
                },
            ) if evidence.trace == Some(trace)
                && actual == trace
                && expected == first
                && evidence.syntax.as_ref().is_some_and(|(selected, summary)| {
                    *selected == element && summary.retained_events == total
                })
                && first < total
                && !events.is_empty()
                && events.len() as u64 <= total - first =>
            {
                let next = first + events.len() as u64;
                evidence.append_syntax(events);
                // Exits also count in the stream. The separate cursor is the
                // number of received events, not the number of displayed rows.
                evidence.syntax_loading = next < total;
                if next < total {
                    self.syntax_page(at, trace, next, out);
                } else {
                    evidence.reveal_selection();
                }
            }
            (Request::Validate, Event::AnalysisValidated { summary, total, .. })
                if evidence.running == Some(id) =>
            {
                self.validation_ready(at, id, *summary, total, out);
            }
            (
                Request::Findings {
                    report,
                    first: expected,
                },
                Event::AnalysisFindings {
                    validation,
                    first,
                    total,
                    findings,
                    ..
                },
            ) if evidence
                .validation
                .as_ref()
                .is_some_and(|(current, _)| *current == report)
                && report == validation
                && expected == first
                && total == evidence.findings_total
                && first == evidence.findings.len() as u64
                && first < total
                && !findings.is_empty()
                && findings.len() as u64 <= total - first =>
            {
                evidence.findings.extend(findings);
                let next = evidence.findings.len() as u64;
                evidence.findings_loading = next < total;
                if next < total {
                    self.findings_page(at, report, next, out);
                }
            }
            (Request::Cancel, Event::CancelRequested) => {}
            _ => evidence.fail(
                id,
                request,
                "The engine supplied an invalid evidence reply.".to_owned(),
            ),
        }
    }

    fn syntax_ready(
        &mut self,
        at: usize,
        id: u64,
        element: u64,
        summary: SyntaxSummary,
        total: u64,
        out: &mut Vec<Effect>,
    ) {
        let evidence = &mut self.documents[at].evidence;
        evidence.syntax_request = None;
        if total > MAX_SYNTAX_EVENTS || total != summary.retained_events {
            evidence.syntax_loading = false;
            evidence.syntax_error = Some("The engine supplied an invalid syntax count.".to_owned());
            return;
        }
        evidence.syntax_loading = total > 0;
        evidence.syntax = Some((element, summary));
        evidence.trace = Some(id);
        evidence.depth = 0;
        if evidence.syntax_loading {
            self.syntax_page(at, id, 0, out);
        }
    }

    fn validation_ready(
        &mut self,
        at: usize,
        id: u64,
        summary: ValidationSummary,
        total: u64,
        out: &mut Vec<Effect>,
    ) {
        let evidence = &mut self.documents[at].evidence;
        evidence.running = None;
        evidence.cancelling = false;
        evidence.validation = Some((id, summary));
        evidence.findings_total = total;
        if total > MAX_FINDINGS {
            evidence.findings_error =
                Some("The engine supplied an invalid finding count.".to_owned());
            return;
        }
        evidence.findings_loading = total > 0;
        if evidence.findings_loading {
            self.findings_page(at, id, 0, out);
        }
    }

    fn syntax_page(&mut self, at: usize, trace: u64, first: u64, out: &mut Vec<Effect>) {
        self.send(
            self.documents[at].id,
            Asked::Evidence(Request::SyntaxPage { trace, first }),
            out,
            |document| Command::AnalysisSyntaxPage {
                document,
                trace,
                first,
                count: WIRE_ROWS,
            },
        );
    }

    fn findings_page(&mut self, at: usize, report: u64, first: u64, out: &mut Vec<Effect>) {
        self.send(
            self.documents[at].id,
            Asked::Evidence(Request::Findings { report, first }),
            out,
            |document| Command::AnalysisFindings {
                document,
                validation: report,
                first,
                count: WIRE_ROWS,
            },
        );
    }

    pub(super) fn start_validation(
        &mut self,
        at: usize,
        settings: ValidationSettings,
        out: &mut Vec<Effect>,
    ) {
        let doc = &mut self.documents[at];
        let document_id = doc.id;
        if doc.evidence.running.is_some() {
            return;
        }
        doc.evidence.tab = 2;
        doc.evidence.first = 0;
        doc.evidence.validation = None;
        doc.evidence.findings.clear();
        doc.evidence.findings_loading = false;
        doc.evidence.findings_error = None;
        doc.evidence.selected = None;
        let id = self.send(
            document_id,
            Asked::Evidence(Request::Validate),
            out,
            |document| Command::AnalysisValidate { document, settings },
        );
        self.documents[at].evidence.running = Some(id);
    }

    pub(super) fn stop_validation(&mut self, at: usize, out: &mut Vec<Effect>) {
        let doc = &mut self.documents[at];
        let document_id = doc.id;
        if let Some(validation) = doc.evidence.running.filter(|_| !doc.evidence.cancelling) {
            doc.evidence.cancelling = true;
            self.send(
                document_id,
                Asked::Evidence(Request::Cancel),
                out,
                |document| Command::AnalysisCancelValidation {
                    document,
                    validation,
                },
            );
        }
    }

    pub(super) fn move_evidence(&mut self, at: usize, step: Step, out: &mut Vec<Effect>) {
        let evidence = &self.documents[at].evidence;
        let matches = evidence.matching();
        let Some(last) = matches.len().checked_sub(1) else {
            return;
        };
        let current = evidence
            .selected_index()
            .and_then(|i| matches.binary_search(&i).ok());
        let index = match (step, current) {
            (Step::End, _) => last,
            (Step::Home, _) => 0,
            (Step::Left | Step::Right, _) => return,
            (_, None) => evidence.first.min(last),
            (Step::Up, Some(i)) => i.saturating_sub(1),
            (Step::Down, Some(i)) => (i + 1).min(last),
            (Step::PageUp, Some(i)) => i.saturating_sub(VIEW_ROWS),
            (Step::PageDown, Some(i)) => (i + VIEW_ROWS).min(last),
        };
        self.documents[at].evidence.first = index / VIEW_ROWS * VIEW_ROWS;
        self.pick_evidence(at, matches[index], out);
    }

    pub(super) fn pick_evidence(&mut self, at: usize, index: usize, out: &mut Vec<Effect>) {
        let doc = &self.documents[at];
        let evidence = &doc.evidence;
        if evidence.tab == 2 {
            let Some((report, _)) = evidence.validation.as_ref() else {
                return;
            };
            let Some(finding) = evidence.findings.get(index) else {
                return;
            };
            let pick = Pick::Finding {
                report: *report,
                index,
            };
            if let Some(offset) = finding
                .file_offset
                .filter(|_| doc.len().is_some_and(|len| len > 0))
            {
                let offset = offset.min(doc.len().unwrap_or_default().saturating_sub(1));
                self.locate(
                    at,
                    LocateTarget::Offset { offset },
                    Purpose::Evidence { pick, offset },
                    out,
                );
            } else {
                self.documents[at].evidence.selected = Some(pick);
            }
        } else if evidence.tab == 1
            && let Some((element, _)) = evidence.syntax.as_ref()
            && let Some((_, event)) = evidence.events.get(index)
            && let Some(selected) = doc.selected.clone()
        {
            let offset = match event {
                SyntaxEvent::Signaled {
                    span: Some(span), ..
                } => span.start.file_bit / 8,
                _ => doc.bytes.cursor,
            };
            let pick = Pick::Syntax {
                element: *element,
                index,
            };
            self.select_evidence(at, selected.row, selected.position, pick, offset, out);
        }
    }

    pub(super) fn select_evidence(
        &mut self,
        at: usize,
        row: Row,
        position: Position,
        pick: Pick,
        offset: u64,
        out: &mut Vec<Effect>,
    ) {
        let doc = &mut self.documents[at];
        if doc.evidence.selected == Some(pick)
            && doc
                .selected
                .as_ref()
                .is_some_and(|selected| selected.row.index == row.index)
        {
            return;
        }
        if let Some(place) = doc.history.get_mut(doc.at) {
            place.window = doc.bytes.offset;
            place.cursor = doc.bytes.cursor;
            place.evidence = doc.evidence.selected;
        }
        doc.bytes.shown = true;
        doc.bytes.cursor = offset;
        if !in_window(doc.bytes.offset, offset) {
            doc.bytes.offset = line_of(offset);
            doc.bytes.data = None;
        }
        // A finding and the OBU are one selection, even if both resolve
        // to the same navigator row.
        doc.selected = None;
        self.select(at, row, position, false, out);
        let doc = &mut self.documents[at];
        doc.evidence.selected = Some(pick);
        if let Some(place) = doc.history.get_mut(doc.at) {
            place.evidence = Some(pick);
        }
        self.ensure_syntax(at, out);
    }
}
