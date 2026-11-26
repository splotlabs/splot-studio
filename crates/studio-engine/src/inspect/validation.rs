// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Native validation for an open analysis document.

mod context;

use std::fs::File;
use std::io::BufReader;

use splot_core::span::{BitOffset, ByteOffset};
use splot_validate::{
    StreamValidateError, ValidationCompletion as NativeCompletion, ValidationLimits, Validator,
};
use studio_domain::validation::{
    Finding, Severity, VALIDATION_SCHEMA, ValidationCompletion, ValidationEvidence,
    ValidationSettings, ValidationSummary,
};

use super::document::{Checked, MAX_RETAINED_ELEMENTS, MAX_UNIT_BYTES, Refusal};

const MAX_FINDINGS: u32 = 4096;

pub(super) fn validate(
    file: &mut File,
    settings: &ValidationSettings,
    cancelled: &dyn Fn() -> bool,
) -> Result<ValidationEvidence, Refusal> {
    let (options, context) = context::options(settings.external_hls.as_deref())?;
    let reader = BufReader::new(Checked::new(file, cancelled));
    let result = Validator::new(settings.strict).validate_reader_with_evidence(
        reader,
        &options,
        ValidationLimits {
            max_unit_bytes: MAX_UNIT_BYTES,
            max_diagnostics: MAX_FINDINGS as usize,
            max_obus: MAX_RETAINED_ELEMENTS as u64,
        },
        cancelled,
    );
    let completion = match result.completion {
        NativeCompletion::EndOfInput => ValidationCompletion::EndOfInput,
        NativeCompletion::MalformedInput => ValidationCompletion::MalformedInput,
        NativeCompletion::Cancelled => ValidationCompletion::Cancelled,
        NativeCompletion::ObuLimit { max_obus } => ValidationCompletion::ObuLimit { max_obus },
        NativeCompletion::InputError(StreamValidateError::Io(error)) => ValidationCompletion::Io {
            message: error.to_string(),
        },
        NativeCompletion::InputError(StreamValidateError::UnitTooLarge {
            offset,
            declared,
            cap,
        }) => ValidationCompletion::UnitTooLarge {
            offset: offset.get(),
            declared,
            cap: cap as u64,
        },
        _ => ValidationCompletion::Unsupported,
    };
    let mut scope = String::from(
        "Splot checks OBU syntax and stream relationships. Per-rule execution is unavailable. \
         These checks cover part of AV2 v1.0.0. They exclude independent decoding.",
    );
    if context.is_some() {
        scope.push_str(" Splot uses the supplied sequence headers and operating point sets as initial validation state. Random access checks cannot distinguish some in-band replacements of supplied objects.");
    }
    let counts = result.counts;
    let findings: Vec<_> = result
        .report
        .diagnostics
        .into_iter()
        .map(|d| Finding {
            rule_id: d.rule_id,
            message: d.message,
            severity: match d.severity {
                splot_validate::Severity::Error => Severity::Error,
                splot_validate::Severity::Warning => Severity::Warning,
                splot_validate::Severity::Info => Severity::Info,
            },
            spec_section: d.spec_section,
            file_offset: d.byte_offset.map(ByteOffset::get),
            bit_offset: d.bit_offset.map(BitOffset::get),
        })
        .collect();
    let coverage = result.coverage;
    let total = counts
        .errors
        .saturating_add(counts.warnings)
        .saturating_add(counts.info);
    Ok(ValidationEvidence {
        summary: ValidationSummary {
            schema: VALIDATION_SCHEMA,
            spec_revision: "AV2 v1.0.0".into(),
            codec_revision: env!("SPLOT_CODEC_REVISION").into(),
            engine_version: env!("CARGO_PKG_VERSION").into(),
            strict: settings.strict,
            context,
            scope,
            completion,
            checked_obus: coverage.checked_obus,
            last_checked_obu_end: coverage.last_checked_obu_end.map(ByteOffset::get),
            malformed_units: coverage.malformed_units,
            finalized: coverage.finalized,
            errors: counts.errors as u64,
            warnings: counts.warnings as u64,
            info: counts.info as u64,
            finding_limit: MAX_FINDINGS,
            omitted: total.saturating_sub(findings.len()) as u64,
        },
        findings,
    })
}

#[cfg(test)]
mod tests {
    use std::io::{Seek, Write};

    use super::*;
    use splot_core::ivf::{IvfHeader, write_ivf_frame, write_ivf_header};

    #[test]
    fn native_rule_retains_its_location_across_wrappers_and_policies() -> std::io::Result<()> {
        // This temporal delimiter explicitly signals extended layer 5.
        let obu = [0x02, 0x88, 0x05];
        for (ivf, header_offset) in [(false, 1), (true, 45)] {
            let mut file = tempfile::tempfile()?;
            if ivf {
                write_ivf_header(&mut file, &IvfHeader::new(*b"AV02", 16, 16, 24, 1, 1))?;
                write_ivf_frame(&mut file, 0, &obu)?;
            } else {
                file.write_all(&obu)?;
            }
            let mut previous = None;
            for strict in [false, true] {
                file.rewind()?;
                let options = ValidationSettings {
                    strict,
                    external_hls: None,
                };
                let report = validate(&mut file, &options, &|| false)
                    .map_err(|error| std::io::Error::other(error.message))?;
                let rule = report
                    .findings
                    .iter()
                    .find(|f| f.rule_id == "obu-header/global-xlayer-required")
                    .ok_or_else(|| std::io::Error::other("Missing layer diagnostic."))?;
                assert_eq!(rule.file_offset, Some(header_offset));
                assert_eq!(rule.bit_offset, None);
                assert_eq!(rule.spec_section.as_deref(), Some("6.2.2"));
                assert_eq!(rule.severity, Severity::Error);
                assert!(report.summary.policy_failed());
                assert_eq!(report.summary.checked_obus, 1);
                assert!(report.summary.finalized);
                if let Some(first) = previous {
                    assert_eq!(report.findings, first);
                }
                previous = Some(report.findings);
            }
        }
        Ok(())
    }
}
