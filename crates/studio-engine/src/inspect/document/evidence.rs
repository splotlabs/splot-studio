// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Evidence retained for one generation of an open source.

use serde::Serialize;
use studio_domain::analysis::{AnalysisFailure, Element, MAX_PAGE_ELEMENTS};
use studio_domain::syntax::{SyntaxEvent, SyntaxSummary};
use studio_domain::validation::{Finding, ValidationSettings, ValidationSummary};

use super::{Document, PAGE_BUDGET, Refusal, Seek, SeekFrom, read_file_range};
use crate::inspect::{syntax, validation};

#[derive(Debug, Default)]
pub(super) struct Evidence {
    trace: Option<(u64, u64, Vec<SyntaxEvent>)>,
    report: Option<(u64, Vec<Finding>)>,
}

pub(in crate::inspect) struct EvidencePage<T> {
    pub total: u64,
    pub entries: Vec<T>,
}

impl Document {
    pub(in crate::inspect) fn syntax(
        &mut self,
        request: u64,
        element: u64,
    ) -> Result<(SyntaxSummary, u64), Refusal> {
        self.check()?;
        if self
            .evidence
            .trace
            .as_ref()
            .is_some_and(|(id, ..)| request <= *id)
        {
            return Err(invalid(
                "Use a request ID greater than the last syntax request ID.",
            ));
        }
        let entry = usize::try_from(element)
            .ok()
            .and_then(|index| self.items.get(index));
        let Some(Element::Obu { obu }) = entry.map(super::Item::element) else {
            return Err(invalid("Select an indexed OBU for syntax observation."));
        };
        let len = if obu.obu.len <= syntax::MAX_OBU_BYTES {
            obu.obu.len
        } else {
            obu.header.len
        };
        let len = u32::try_from(len).map_err(|_| invalid("The OBU size is out of range."))?;
        let read = read_file_range(
            &mut self.file,
            self.summary.fingerprint.len,
            obu.obu.offset,
            len,
        );
        self.check()?;
        let trace = syntax::observe(&obu, &read?);
        self.check()?;
        let total = trace.events.len() as u64;
        self.evidence.trace = Some((request, element, trace.events));
        Ok((trace.summary, total))
    }

    pub(in crate::inspect) fn syntax_page(
        &self,
        request: u64,
        first: u64,
        count: u32,
    ) -> Result<(u64, EvidencePage<SyntaxEvent>), Refusal> {
        self.check()?;
        let Some((id, element, events)) = &self.evidence.trace else {
            return Err(invalid(
                "Collect a syntax trace before requesting its events.",
            ));
        };
        if request != *id {
            return Err(invalid(
                "The requested syntax trace is no longer available.",
            ));
        }
        Ok((*element, page(events, first, count)?))
    }

    pub(in crate::inspect) fn validate(
        &mut self,
        request: u64,
        settings: &ValidationSettings,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(ValidationSummary, u64), Refusal> {
        self.check()?;
        if self
            .evidence
            .report
            .as_ref()
            .is_some_and(|(id, _)| request <= *id)
        {
            return Err(invalid(
                "Use a request ID greater than the last validation request ID.",
            ));
        }
        if self
            .summary
            .ivf_header
            .as_ref()
            .is_some_and(|header| header.fourcc != *b"AV02")
        {
            return Err(Refusal::new(
                AnalysisFailure::Unsupported,
                "Validation requires an AV2 source.",
            ));
        }
        let result = self
            .file
            .seek(SeekFrom::Start(0))
            .map_err(|error| {
                Refusal::new(
                    AnalysisFailure::Io,
                    format!("Cannot read the file from the start. {error}"),
                )
            })
            .and_then(|_| validation::validate(&mut self.file, settings, cancelled));
        self.check()?;
        let report = result?;
        let total = report.findings.len() as u64;
        self.evidence.report = Some((request, report.findings));
        Ok((report.summary, total))
    }

    pub(in crate::inspect) fn findings(
        &self,
        request: u64,
        first: u64,
        count: u32,
    ) -> Result<EvidencePage<Finding>, Refusal> {
        self.check()?;
        let Some((id, findings)) = &self.evidence.report else {
            return Err(invalid("Run a validation before requesting its findings."));
        };
        if request != *id {
            return Err(invalid(
                "The requested validation report is no longer available.",
            ));
        }
        page(findings, first, count)
    }
}

fn invalid(message: impl Into<String>) -> Refusal {
    Refusal::new(AnalysisFailure::InvalidRequest, message)
}

fn page<T: Clone + Serialize>(
    entries: &[T],
    first: u64,
    count: u32,
) -> Result<EvidencePage<T>, Refusal> {
    let total = entries.len() as u64;
    if count == 0 || count > MAX_PAGE_ELEMENTS {
        return Err(invalid(format!(
            "Request 1 to {MAX_PAGE_ELEMENTS} entries per page."
        )));
    }
    let index = usize::try_from(first)
        .ok()
        .filter(|&index| index < entries.len() || (index == 0 && entries.is_empty()))
        .ok_or_else(|| {
            invalid(format!(
                "Entry {first} is outside the report of {total} entries."
            ))
        })?;
    let mut items = Vec::new();
    let mut remaining = PAGE_BUDGET;
    for item in entries[index..].iter().take(count as usize) {
        let encoded = serde_json::to_vec(item).map_err(|error| {
            Refusal::new(
                AnalysisFailure::Io,
                format!("Cannot encode report evidence. {error}"),
            )
        })?;
        // Reserve one byte for the comma between entries.
        let size = encoded.len() + 1;
        if size > remaining {
            if items.is_empty() {
                return Err(Refusal::new(
                    AnalysisFailure::LimitReached,
                    "The evidence entry exceeds the protocol message limit.",
                ));
            }
            break;
        }
        remaining -= size;
        items.push(item.clone());
    }
    Ok(EvidencePage {
        total,
        entries: items,
    })
}

#[cfg(test)]
mod tests {
    use super::{PAGE_BUDGET, page};
    use crate::inspect::document::Refusal;
    use studio_domain::analysis::{AnalysisFailure, MAX_PAGE_ELEMENTS};

    #[test]
    fn pages_limit_bytes_after_json_escaping() -> Result<(), Refusal> {
        let quote_heavy = "\"".repeat(PAGE_BUDGET / 3);
        let entries = vec![quote_heavy; 3];
        let first = page(&entries, 0, MAX_PAGE_ELEMENTS)?;
        assert_eq!(first.total, 3);
        assert_eq!(first.entries.len(), 1);
        let second = page(&entries, 1, MAX_PAGE_ELEMENTS)?;
        assert_eq!(second.total, 3);
        assert_eq!(second.entries.len(), 1);
        let error = page(&["\"".repeat(PAGE_BUDGET)], 0, 1).err();
        assert!(error.is_some_and(|error| error.code == AnalysisFailure::LimitReached));
        Ok(())
    }
}
