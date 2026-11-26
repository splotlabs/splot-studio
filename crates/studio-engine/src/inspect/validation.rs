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
            stream_offset: None,
            extended_layer_id: None,
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

    fn validate_fixture(file: &mut File, strict: bool) -> std::io::Result<ValidationEvidence> {
        file.rewind()?;
        let settings = ValidationSettings {
            strict,
            external_hls: None,
        };
        validate(file, &settings, &|| false).map_err(|error| std::io::Error::other(error.message))
    }

    #[test]
    fn diagnostic_omission_retains_counts_and_complete_execution() -> std::io::Result<()> {
        let mut file = tempfile::tempfile()?;
        // A temporal delimiter must use the global layer. This one signals layer 5.
        let units = MAX_FINDINGS + 2;
        file.write_all(&[0x02, 0x88, 0x05].repeat(units as usize))?;
        let ValidationEvidence { summary, findings } = validate_fixture(&mut file, false)?;
        assert_eq!(summary.completion, ValidationCompletion::EndOfInput);
        assert!(summary.finalized);
        assert_eq!(summary.checked_obus, u64::from(units));
        assert_eq!(summary.finding_limit, MAX_FINDINGS);
        assert_eq!(findings.len(), MAX_FINDINGS as usize);
        assert_eq!(
            summary.omitted + u64::from(MAX_FINDINGS),
            summary.errors + summary.warnings + summary.info
        );
        assert!(summary.omitted >= 2);
        assert!(summary.policy_failed());
        Ok(())
    }

    fn ivf_with_one_delimiter() -> std::io::Result<File> {
        let mut file = tempfile::tempfile()?;
        write_ivf_header(&mut file, &IvfHeader::new(*b"AV02", 16, 16, 24, 1, 2))?;
        write_ivf_frame(&mut file, 0, &[0x01, 0x08])?;
        Ok(file)
    }

    #[test]
    fn an_oversize_ivf_record_preserves_checked_prefix() -> std::io::Result<()> {
        let mut file = ivf_with_one_delimiter()?;
        let declared = u32::try_from(MAX_UNIT_BYTES + 1).map_err(std::io::Error::other)?;
        let payload_start = file.stream_position()? + 12;
        file.write_all(&declared.to_le_bytes())?;
        file.write_all(&1_u64.to_le_bytes())?;
        let ValidationEvidence { summary, findings } = validate_fixture(&mut file, true)?;
        assert_eq!(
            summary.completion,
            ValidationCompletion::UnitTooLarge {
                offset: payload_start,
                declared: u64::from(declared),
                cap: MAX_UNIT_BYTES as u64,
            }
        );
        assert_eq!(summary.checked_obus, 1);
        assert_eq!(summary.last_checked_obu_end, Some(payload_start - 12));
        assert!(!summary.finalized);
        assert_eq!(findings.as_slice(), &[]);
        assert!(!summary.policy_failed());
        Ok(())
    }

    #[test]
    fn strict_policy_retains_the_native_warning() -> std::io::Result<()> {
        let mut file = ivf_with_one_delimiter()?;
        // A partial trailing record header emits a native warning.
        file.write_all(&[0x04, 0x00])?;
        let permissive = validate_fixture(&mut file, false)?;
        let strict = validate_fixture(&mut file, true)?;
        assert_eq!(
            permissive.summary.completion,
            ValidationCompletion::MalformedInput
        );
        assert_eq!(strict.summary.completion, permissive.summary.completion);
        assert_eq!(permissive.summary.errors, 0);
        assert_eq!(permissive.summary.warnings, 1);
        assert!(!permissive.summary.policy_failed());
        assert!(strict.summary.policy_failed());
        assert_eq!(permissive.findings, strict.findings);
        assert!(matches!(
            strict.findings.as_slice(),
            [finding]
              if finding.rule_id == "ivf/trailing-partial-frame-header"
                  && finding.severity == Severity::Warning
        ));
        Ok(())
    }

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
                let report = validate_fixture(&mut file, strict)?;
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
