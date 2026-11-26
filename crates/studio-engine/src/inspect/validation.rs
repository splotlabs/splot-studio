// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Native validation for an open analysis document.

mod context;

use std::fs::File;
use std::io::BufReader;

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
        scope.push_str(" Splot uses the supplied sequence headers and operating point sets as initial validation state.");
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
            file_offset: d.byte_offset.map(|offset| offset.get()),
            bit_offset: d.bit_offset.map(|bit| bit.get()),
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
            last_checked_obu_end: coverage.last_checked_obu_end.map(|end| end.get()),
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
