// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Evidence retained for one generation of an open source.

use std::fs::File;

use serde::Serialize;
use studio_domain::analysis::{AnalysisFailure, Element, Framing, MAX_PAGE_ELEMENTS, SourceDigest};
use studio_domain::identity::{IdentityKind, IdentityPage, IdentitySummary};
use studio_domain::preview::GrainPolicy;
use studio_domain::syntax::{SyntaxEvent, SyntaxSummary};
use studio_domain::validation::{
    Finding, ValidationEvidence, ValidationSettings, ValidationSummary,
};

use super::{Document, Item, PAGE_BUDGET, Refusal, Seek, SeekFrom, read_file_range};
use crate::inspect::{ValidationResult, identity::IdentityIndex, syntax, validation};
use crate::output::FileId;

#[derive(Debug, Default)]
pub(super) struct Evidence {
    trace: Option<(u64, u64, Vec<SyntaxEvent>)>,
    report: Option<(u64, Vec<Finding>)>,
    identity: Option<(u64, IdentityIndex)>,
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
        let index = usize::try_from(element).unwrap_or(usize::MAX);
        let entry = self.index()?.item(index)?;
        let Some(Element::Obu { obu }) = entry.as_ref().map(Item::element) else {
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

    pub(in crate::inspect) fn prepare_validation(&self, request: u64) -> Result<File, Refusal> {
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
        self.open_evidence_source()
    }

    pub(in crate::inspect) fn open_evidence_source(&self) -> Result<File, Refusal> {
        self.check()?;
        self.require_av2()?;
        let opened = File::open(&self.path);
        self.check()?;
        let file = opened.map_err(|error| {
            Refusal::new(
                AnalysisFailure::Io,
                format!("Cannot open the source. {error}"),
            )
        })?;
        let id = FileId::of_file(&file, &self.path).map_err(|error| {
            Refusal::new(
                AnalysisFailure::Io,
                format!("Cannot identify the source. {error}"),
            )
        })?;
        if id != self.id {
            return Err(Refusal::new(
                AnalysisFailure::SourceChanged,
                "The source changed before the evidence pass started.",
            ));
        }
        Ok(file)
    }

    pub(in crate::inspect) fn begin_identity(
        &mut self,
        request: u64,
        grain: GrainPolicy,
    ) -> Result<IdentityIndex, Refusal> {
        if self
            .evidence
            .identity
            .as_ref()
            .is_some_and(|(id, _)| request <= *id)
        {
            return Err(invalid(
                "Use a request ID greater than the last identity scan request ID.",
            ));
        }
        let index = IdentityIndex::new(grain, self.summary.fingerprint)?;
        self.evidence.identity = Some((request, index.clone()));
        Ok(index)
    }

    pub(in crate::inspect) fn drop_identity(&mut self, request: u64) {
        if self
            .evidence
            .identity
            .as_ref()
            .is_some_and(|(id, _)| request == *id)
        {
            self.evidence.identity = None;
        }
    }

    fn identity_index(&self, request: u64) -> Result<&IdentityIndex, Refusal> {
        self.check()?;
        match &self.evidence.identity {
            Some((id, index)) if *id == request => Ok(index),
            _ => Err(invalid("The requested identity scan is unavailable.")),
        }
    }

    pub(in crate::inspect) fn complete_identity(
        &mut self,
        request: u64,
        result: Result<(), Refusal>,
    ) -> Result<IdentitySummary, Refusal> {
        self.check()?;
        if let Err(error) = result {
            self.drop_identity(request);
            return Err(error);
        }
        let mut summary = self.identity_index(request)?.summary();
        self.verify_identity(&mut summary)?;
        Ok(summary)
    }

    pub(in crate::inspect) fn identity_page(
        &self,
        request: u64,
        kind: IdentityKind,
        first: u64,
        count: u32,
    ) -> Result<IdentityPage, Refusal> {
        let mut page = self.identity_index(request)?.page(kind, first, count)?;
        self.check()?;
        self.verify_identity(&mut page.summary)?;
        Ok(page)
    }

    fn verify_identity(&self, summary: &mut IdentitySummary) -> Result<(), Refusal> {
        let Some((structure, decoded)) =
            self.summary.digest.as_deref().zip(summary.digest.as_ref())
        else {
            return Ok(());
        };
        if structure.bytes == decoded.bytes {
            if structure != decoded {
                return Err(Refusal::new(
                    AnalysisFailure::SourceChanged,
                    "The source bytes changed between structure and identity scans.",
                ));
            }
            summary.matches_structure = decoded.bytes == self.summary.fingerprint.len;
        }
        Ok(())
    }

    pub(in crate::inspect) fn complete_validation(
        &mut self,
        request: u64,
        result: ValidationResult,
    ) -> Result<(ValidationSummary, u64), Refusal> {
        self.check()?;
        let (report, _) = self.enrich_validation(result?)?;
        let total = report.findings.len() as u64;
        self.evidence.report = Some((request, report.findings));
        Ok((report.summary, total))
    }

    pub(in crate::inspect) fn collect_validation(
        &mut self,
        settings: &ValidationSettings,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(ValidationEvidence, SourceDigest), Refusal> {
        self.check()?;
        self.validate_source(settings, cancelled)
    }

    fn require_av2(&self) -> Result<(), Refusal> {
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
        Ok(())
    }

    fn validate_source(
        &mut self,
        settings: &ValidationSettings,
        cancelled: &dyn Fn() -> bool,
    ) -> ValidationResult {
        self.require_av2()?;
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
        self.enrich_validation(result?)
    }

    fn enrich_validation(
        &self,
        (mut report, digest): (ValidationEvidence, SourceDigest),
    ) -> ValidationResult {
        let Some(indexed) = self
            .summary
            .digest
            .as_deref()
            .filter(|indexed| indexed.bytes == digest.bytes)
        else {
            report.summary.scope.push_str(
                " Inspection and validation read different byte ranges. Derived stream offsets and layer IDs are unavailable.",
            );
            return Ok((report, digest));
        };
        if indexed != &digest {
            return Err(Refusal::new(
                AnalysisFailure::SourceChanged,
                "The source bytes changed between inspection and validation.",
            ));
        }
        for finding in &mut report.findings {
            if let Some(byte) = finding.file_offset {
                let index = self.index()?;
                let item = match index.preceding(byte)? {
                    Some(at) => index.item(at)?,
                    None => None,
                };
                let (stream, layer) = item_location(item.as_ref(), byte);
                finding.stream_offset = if self.summary.framing == Framing::AnnexB {
                    (byte < self.summary.fingerprint.len).then_some(byte)
                } else {
                    stream
                };
                finding.extended_layer_id = layer;
            }
        }
        Ok((report, digest))
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

#[cfg(test)]
fn indexed_location(items: &[Item], byte: u64) -> (Option<u64>, Option<u8>) {
    let end = items.partition_point(|item| item.start() <= byte);
    item_location(items[..end].last(), byte)
}

fn item_location(item: Option<&Item>, byte: u64) -> (Option<u64>, Option<u8>) {
    let Some(item) = item.filter(|item| byte < item.end()) else {
        return (None, None);
    };
    match item {
        Item::Obu {
            prefix,
            offset,
            header,
            stream_offset,
            ..
        } => (
            stream_offset.checked_add(byte - prefix),
            (byte >= *offset).then(|| header.extended_layer_id.get()),
        ),
        Item::Unparsed {
            offset,
            stream_offset: Some(stream),
            ..
        } => (stream.checked_add(byte - offset), None),
        _ => (None, None),
    }
}

fn invalid(message: impl Into<String>) -> Refusal {
    Refusal::new(AnalysisFailure::InvalidRequest, message)
}

pub(in crate::inspect) fn page<T: Clone + Serialize>(
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
    use super::{PAGE_BUDGET, indexed_location, page};
    use crate::inspect::document::Refusal;
    use splot_core::types::GLOBAL_XLAYER_ID;
    use studio_domain::analysis::{AnalysisFailure, MAX_PAGE_ELEMENTS};

    #[test]
    fn finding_coordinates_require_a_known_payload_byte() -> Result<(), Refusal> {
        let mut stream = b"DKIF\x00\x00\x20\x00AV02".to_vec();
        stream.resize(32, 0);
        // One IVF record: an extended OBU on layer 5, then unparsed bytes.
        stream.extend_from_slice(&5_u32.to_le_bytes());
        stream.extend_from_slice(&0_u64.to_le_bytes());
        stream.extend_from_slice(&[3, 0x9D, 0x45, 0x11, 0x80]);
        stream.extend_from_slice(&2_u32.to_le_bytes());
        stream.extend_from_slice(&1_u64.to_le_bytes());
        stream.extend_from_slice(&[1, 0x08]);
        // The next IVF record header ends early. These bytes are unclassified.
        stream.extend_from_slice(&[4, 0]);
        let scanned = super::super::scan(stream.as_slice(), stream.len() as u64, &|| false)?;

        assert_eq!(indexed_location(&scanned.items, 44), (Some(0), None));
        for byte in [45, 46, 47] {
            assert_eq!(
                indexed_location(&scanned.items, byte),
                (Some(byte - 44), Some(5))
            );
        }
        assert_eq!(indexed_location(&scanned.items, 48), (Some(4), None));
        assert_eq!(indexed_location(&scanned.items, 61), (Some(5), None));
        assert_eq!(
            indexed_location(&scanned.items, 62),
            (Some(6), Some(GLOBAL_XLAYER_ID.get()))
        );
        for byte in [0, 32, 49, 60, 63, 64, 65] {
            assert_eq!(indexed_location(&scanned.items, byte), (None, None));
        }
        Ok(())
    }

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
