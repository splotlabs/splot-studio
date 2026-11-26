// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Collects the codec's state-free syntax observations for one indexed OBU.

use splot_core::Error;
use splot_core::obu::{
    PayloadStatus, dispatch_obu_payload_observed, read_obu_header_from_slice_observed,
};
use splot_core::span::ByteOffset;
use splot_core::syntax::{
    RecordedScope, SyntaxEvent as CodecEvent, SyntaxLimits, SyntaxRecorder, SyntaxTrace,
    SyntaxValue as CodecValue,
};
use studio_domain::analysis::ObuFact;
use studio_domain::syntax::{
    BitPosition, SYNTAX_SCHEMA, SyntaxError, SyntaxEvent, SyntaxEvidence, SyntaxSpan, SyntaxStatus,
    SyntaxSummary, SyntaxValue,
};

const MAX_ELEMENTS: usize = SyntaxLimits::DEFAULT_MAX_ELEMENTS;
const MAX_DEPTH: usize = SyntaxLimits::DEFAULT_MAX_DEPTH;

// The recorder limits retained memory. The input budget also bounds parser work.
pub(super) const MAX_OBU_BYTES: u64 = 256 * 1024;

/// Reads syntax from the indexed OBU bytes, including its header and excluding its length prefix.
/// For an OBU above the byte budget, only the header bytes are required.
pub(super) fn observe(obu: &ObuFact, bytes: &[u8]) -> SyntaxEvidence {
    let mut recorder = SyntaxRecorder::new(SyntaxLimits::new(MAX_ELEMENTS, MAX_DEPTH));
    let header = read_obu_header_from_slice_observed(
        bytes,
        ByteOffset::new(obu.header.offset),
        &mut recorder,
    );
    let header_trace = recorder.finish();
    let mut evidence = SyntaxEvidence {
        summary: SyntaxSummary {
            schema_version: SYNTAX_SCHEMA,
            spec_revision: "AV2 v1.0.0".to_owned(),
            status: SyntaxStatus::Malformed,
            error: None,
            retained_events: 0,
            observed_elements: 0,
            elements_truncated: false,
            depth_truncated: false,
        },
        events: Vec::new(),
    };
    append(
        &mut evidence,
        obu,
        obu.header.offset,
        obu.header.len,
        header_trace,
    );
    match header {
        Ok(_) if obu.obu.len > MAX_OBU_BYTES => {
            evidence.summary.status = SyntaxStatus::ResourceLimited;
        }
        Ok(header) => {
            let Some(payload) = bytes.get(usize::from(header.header_size_bytes)..) else {
                return evidence;
            };
            let remaining =
                MAX_ELEMENTS.saturating_sub(evidence.summary.observed_elements as usize);
            let mut recorder = SyntaxRecorder::new(SyntaxLimits::new(remaining, MAX_DEPTH));
            let result = dispatch_obu_payload_observed(
                header,
                payload,
                ByteOffset::new(obu.payload.offset),
                &mut recorder,
            );
            append(
                &mut evidence,
                obu,
                obu.payload.offset,
                obu.payload.len,
                recorder.finish(),
            );
            evidence.summary.status = match result {
                Ok(PayloadStatus::Parsed(_)) => SyntaxStatus::Parsed,
                Ok(PayloadStatus::Opaque(_)) => SyntaxStatus::Opaque,
                Ok(PayloadStatus::PrefixParsed {
                    blocked_on,
                    feature,
                    ..
                }) => SyntaxStatus::Prefix {
                    blocked_on: blocked_on.to_owned(),
                    feature: feature.to_owned(),
                },
                Ok(PayloadStatus::Unimplemented { feature, .. }) => SyntaxStatus::Unsupported {
                    feature: feature.to_owned(),
                },
                Err(error) => record_error(&mut evidence, error),
            };
        }
        Err(error) => {
            evidence.summary.status = record_error(&mut evidence, error);
        }
    }
    evidence
}

fn record_error(evidence: &mut SyntaxEvidence, error: Error) -> SyntaxStatus {
    let status = match error {
        Error::InvalidObuExtension { .. } => SyntaxStatus::UnknownExtension,
        Error::Unimplemented { feature, .. } => SyntaxStatus::Unsupported {
            feature: feature.to_owned(),
        },
        _ => SyntaxStatus::Malformed,
    };
    evidence.summary.error.get_or_insert_with(|| SyntaxError {
        message: error.to_string(),
        field: None,
        position: None,
    });
    status
}

fn append(evidence: &mut SyntaxEvidence, obu: &ObuFact, base: u64, len: u64, trace: SyntaxTrace) {
    let position = |relative: u64| -> Option<BitPosition> {
        if relative > len.checked_mul(8)? {
            return None;
        }
        let stream_base = obu
            .stream
            .offset
            .checked_add(base.checked_sub(obu.length_prefix.offset)?)?;
        Some(BitPosition {
            file_bit: base.checked_mul(8)?.checked_add(relative)?,
            stream_bit: stream_base.checked_mul(8)?.checked_add(relative)?,
        })
    };

    evidence
        .events
        .extend(trace.events.into_iter().map(|event| match event {
            CodecEvent::EnterScope(scope) => match scope {
                RecordedScope::Structure { name, spec_section } => SyntaxEvent::Structure {
                    name: name.into_owned(),
                    spec_section: spec_section.map(std::borrow::Cow::into_owned),
                },
                RecordedScope::LoopItem { name, index } => SyntaxEvent::LoopItem {
                    name: name.into_owned(),
                    index: index as u64,
                },
                RecordedScope::Condition { condition, taken } => SyntaxEvent::Condition {
                    condition: condition.into_owned(),
                    taken,
                },
                _ => SyntaxEvent::UnknownScope,
            },
            CodecEvent::ExitScope => SyntaxEvent::ExitScope,
            CodecEvent::FieldRead(field) => {
                let span = field
                    .span
                    .start_bit
                    .checked_add(field.span.bit_len)
                    .and_then(position)
                    .and_then(|_| position(field.span.start_bit))
                    .map(|start| SyntaxSpan {
                        start,
                        bit_len: field.span.bit_len,
                    });
                SyntaxEvent::Signaled {
                    name: field.name.into_owned(),
                    descriptor: field.descriptor.to_string(),
                    value: value(field.value),
                    span,
                }
            }
            CodecEvent::FieldAbsent(field) => SyntaxEvent::Absent {
                name: field.name.into_owned(),
                condition: field.condition.into_owned(),
                inferred: field.inferred.map(value),
            },
            _ => SyntaxEvent::UnknownEvent,
        }));
    if let Some(error) = trace.error {
        evidence.summary.error = Some(SyntaxError {
            message: error.message,
            field: error.field.map(std::borrow::Cow::into_owned),
            position: position(error.bit_offset),
        });
    }
    evidence.summary.retained_events = evidence.events.len() as u32;
    evidence.summary.observed_elements += trace.total_observed as u64;
    evidence.summary.elements_truncated |= trace.elements_truncated;
    evidence.summary.depth_truncated |= trace.depth_truncated;
}

fn value(value: CodecValue) -> SyntaxValue {
    match value {
        CodecValue::Bool(value) => SyntaxValue::Bool(value),
        CodecValue::Unsigned(value) => SyntaxValue::Unsigned(value),
        CodecValue::Signed(value) => SyntaxValue::Signed(value),
        CodecValue::Bytes { len } => SyntaxValue::Bytes(len as u64),
        _ => SyntaxValue::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use studio_domain::analysis::{Element, LocateTarget, RowFilter};
    use studio_domain::job::WirePath;

    use super::*;
    use crate::inspect::document::Document;

    fn observed(file: &[u8], offset: u64, obu: &[u8]) -> SyntaxEvidence {
        let dir = tempfile::tempdir().expect("test directory");
        let path = dir.path().join("fixture");
        std::fs::write(&path, file).expect("write fixture");
        let document =
            Document::open(&WirePath::from_path(&path), &|| false).expect("open fixture");
        let (_, row) = document
            .locate(RowFilter::default(), LocateTarget::Offset { offset })
            .expect("locate OBU");
        let Element::Obu { obu: fact } = row.element else {
            panic!("expected an OBU");
        };
        observe(&fact, obu)
    }

    #[test]
    fn nested_metadata_fields_map_to_annex_b_and_ivf() {
        // Metadata short with the HDR content-light payload. max_cll follows
        // the two metadata bytes and holds the 16-bit value 1000.
        let metadata = [0x20, 0x01, 0x01, 0x03, 0xE8, 0x01, 0x90, 0x80];
        let mut annex = vec![0x01, 0x08, metadata.len() as u8];
        annex.extend_from_slice(&metadata);
        let mut ivf = b"DKIF\x00\x00\x20\x00AV02\x10\x00\x10\x00".to_vec();
        ivf.extend(30_u32.to_le_bytes());
        ivf.extend(1_u32.to_le_bytes());
        ivf.extend(1_u64.to_le_bytes());
        ivf.extend((annex.len() as u32).to_le_bytes());
        ivf.extend(0_u64.to_le_bytes());
        ivf.extend_from_slice(&annex);

        for (file, wrapper) in [(&annex, 0), (&ivf, 44)] {
            let evidence = observed(file, wrapper + 3, &metadata);
            assert_eq!(evidence.summary.status, SyntaxStatus::Parsed);
            assert!(evidence.events.iter().any(|event| matches!(
                event,
                SyntaxEvent::Absent { name, inferred: Some(SyntaxValue::Unsigned(0)), .. }
                    if name == "obu_mlayer_id"
            )));
            assert!(evidence.events.iter().any(|event| matches!(
                event,
                SyntaxEvent::Signaled { name, value: SyntaxValue::Unsigned(1000), span: Some(span), .. }
                    if name == "max_cll" && *span == SyntaxSpan {
                        start: BitPosition { file_bit: (wrapper + 6) * 8, stream_bit: 6 * 8 },
                        bit_len: 16,
                    }
            )));
        }
    }

    #[test]
    fn recorder_limits_preserve_scopes_and_report_later_errors() {
        // A temporal delimiter with a long tail. Its final non-zero bit
        // violates trailing_bits after the recorder exhausts its budget.
        let mut annex = vec![0x01, 0x08, 0x81, 0x20, 0x08, 0x80];
        annex.resize(4 + 4097, 0);
        *annex.last_mut().expect("last byte") = 1;
        let evidence = observed(&annex, 4, &annex[4..]);
        assert_eq!(evidence.summary.status, SyntaxStatus::Malformed);
        assert!(evidence.summary.elements_truncated);
        assert!(evidence.summary.observed_elements > MAX_ELEMENTS as u64);
        let error = evidence.summary.error.expect("tail error");
        assert_eq!(error.field.as_deref(), Some("trailing_zero_bit"));
        let bit = (annex.len() as u64) * 8 - 1;
        assert_eq!(
            error.position,
            Some(BitPosition {
                file_bit: bit,
                stream_bit: bit
            })
        );
        let mut depth = 0;
        for event in evidence.events {
            match event {
                SyntaxEvent::Structure { .. } => depth += 1,
                SyntaxEvent::ExitScope => {
                    assert!(depth > 0);
                    depth -= 1;
                }
                _ => {}
            }
        }
        assert_eq!(depth, 0);
    }
}
