// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Streams a validation report and publishes it through the shared output transaction.

use std::ffi::OsStr;
use std::io::{self, BufWriter, Write};
use std::path::Path;

use serde::Serialize;
use studio_domain::analysis::{AnalysisFailure, SourceDigest};
use studio_domain::job::{FailureCode, SourceFingerprint};
use studio_domain::validation::{
    BatchOutcome, ContextIdentity, Finding, ReportFormat, Severity, VALIDATION_SCHEMA,
    ValidationCompletion, ValidationSummary,
};

use crate::job::Stop;
use crate::output::{self, FileId};

const REPORT_SCHEMA: u16 = 1;
const FINDING_SELECTION: &str = "all_retained";
const NO_VALIDATION_SCOPE: &str = "Validation scope and findings are unavailable.";

/// Report data from the original validation run.
pub(super) struct Report<'a> {
    pub source: &'a Path,
    pub source_id: Option<&'a FileId>,
    pub outcome: &'a BatchOutcome,
    pub strict: bool,
    pub context: Option<&'a ContextIdentity>,
    pub findings: &'a [Finding],
}

#[derive(Serialize)]
struct Metadata<'a> {
    schema: u16,
    source: String,
    finding_selection: &'static str,
    outcome: Outcome<'a>,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Outcome<'a> {
    Validated {
        fingerprint: SourceFingerprint,
        digest: &'a SourceDigest,
        validation: &'a ValidationSummary,
        policy_failed: bool,
        retained: u64,
    },
    Failed {
        code: AnalysisFailure,
        reason: &'static str,
        engine_version: &'static str,
        codec_revision: &'static str,
        validation_schema: u16,
        spec_revision: &'static str,
        scope: &'static str,
        strict: bool,
        context: Option<&'a ContextIdentity>,
    },
}

impl<'a> Metadata<'a> {
    fn new(report: &Report<'a>) -> Result<Self, Stop> {
        let outcome = match report.outcome {
            BatchOutcome::Validated {
                fingerprint,
                digest,
                summary,
                total,
            } if *total == report.findings.len() as u64 => Outcome::Validated {
                fingerprint: *fingerprint,
                digest,
                validation: summary,
                policy_failed: summary.policy_failed(),
                retained: *total,
            },
            BatchOutcome::Failed { code, .. } if report.findings.is_empty() => Outcome::Failed {
                code: *code,
                // Names and paths from native I/O errors stay in the app.
                reason: failure_reason(*code),
                engine_version: env!("CARGO_PKG_VERSION"),
                codec_revision: env!("SPLOT_CODEC_REVISION"),
                validation_schema: VALIDATION_SCHEMA,
                spec_revision: "AV2 v1.0.0",
                scope: NO_VALIDATION_SCOPE,
                strict: report.strict,
                context: report.context,
            },
            _ => {
                return Err(output::output(
                    "The validation report has inconsistent findings.",
                ));
            }
        };
        Ok(Self {
            schema: REPORT_SCHEMA,
            source: report.source.file_name().map_or_else(
                || "Unnamed source".into(),
                |name| name.to_string_lossy().into(),
            ),
            finding_selection: FINDING_SELECTION,
            outcome,
        })
    }
}

fn failure_reason(code: AnalysisFailure) -> &'static str {
    match code {
        AnalysisFailure::InvalidRequest => "The worker refused the request.",
        AnalysisFailure::UnknownDocument => "The source document is unavailable.",
        AnalysisFailure::Unsupported => "The worker cannot validate this source.",
        AnalysisFailure::SourceChanged => "The source changed during processing.",
        AnalysisFailure::Io => "The worker could not read the source.",
        AnalysisFailure::Cancelled => "Validation was interrupted before a report was available.",
        AnalysisFailure::LimitReached => "The worker reached a resource limit.",
    }
}

pub(super) fn export(
    destination: &Path,
    report: &Report<'_>,
    format: ReportFormat,
    prepared: &dyn Fn(&Path) -> Result<(), Stop>,
    cancelled: &dyn Fn() -> bool,
) -> Result<u64, Stop> {
    check_cancelled(cancelled)?;
    #[cfg(windows)]
    if destination.file_name().is_some_and(|name| {
        let name = name.to_string_lossy();
        name.ends_with([' ', '.']) || name.contains(':')
    }) {
        return Err(output::output(
            "Choose a report name without trailing spaces, periods, or a colon.",
        ));
    }
    check_source_slot(report.source, destination)?;
    let metadata = Metadata::new(report)?;
    let mut temp = match report.source_id {
        Some(id) => output::prepare(destination, id)?,
        None => output::prepare_new(destination)?,
    };
    prepared(temp.path())?;
    {
        let mut writer = BufWriter::new(temp.as_file_mut());
        match format {
            ReportFormat::Json => json(&mut writer, &metadata, report.findings, cancelled)?,
            ReportFormat::Text => readable(&mut writer, &metadata, report.findings, cancelled)?,
        }
        writer.flush().map_err(write_failed)?;
    }
    temp.as_file().sync_all().map_err(write_failed)?;
    check_cancelled(cancelled)?;
    #[cfg(windows)]
    if let Some(name) = destination.file_name() {
        let name = name.to_string_lossy();
        if name.ends_with([' ', '.']) || name.contains(':') {
            return Err(output::output(
                "Choose a report name without trailing spaces, periods, or a colon.",
            ));
        }
    }
    check_source_slot(report.source, destination)?;
    output::publish(temp, destination)
}

// The source can disappear after validation. Its directory entry still
// identifies a location that a report must not occupy.
fn check_source_slot(source: &Path, destination: &Path) -> Result<(), Stop> {
    fn parent_identity(path: &Path) -> io::Result<FileId> {
        let parent = path.parent().filter(|dir| !dir.as_os_str().is_empty());
        FileId::of_path(parent.unwrap_or_else(|| Path::new(".")))
    }

    let same_slot = source
        .file_name()
        .zip(destination.file_name())
        .is_some_and(|(left, right)| same_name(left, right))
        && parent_identity(source)
            .is_ok_and(|left| parent_identity(destination).is_ok_and(|right| left == right));
    if source == destination || same_slot {
        return Err(Stop::failed(
            FailureCode::SourceIsDestination,
            "Select a destination different from the source file.",
        ));
    }
    Ok(())
}

fn same_name(left: &OsStr, right: &OsStr) -> bool {
    if cfg!(any(windows, target_os = "macos")) {
        left.to_string_lossy().to_lowercase() == right.to_string_lossy().to_lowercase()
    } else {
        left == right
    }
}

fn check_cancelled(cancelled: &dyn Fn() -> bool) -> Result<(), Stop> {
    if cancelled() {
        return Err(Stop::Cancelled);
    }
    Ok(())
}

#[allow(clippy::needless_pass_by_value)] // Result::map_err takes the error.
fn write_failed(error: io::Error) -> Stop {
    output::output(format!("Cannot write the validation report. {error}"))
}

fn json(
    writer: &mut impl Write,
    metadata: &Metadata<'_>,
    findings: &[Finding],
    cancelled: &dyn Fn() -> bool,
) -> Result<(), Stop> {
    writer.write_all(b"{\"metadata\":").map_err(write_failed)?;
    serialize(writer, metadata)?;
    writer.write_all(b",\"findings\":[").map_err(write_failed)?;
    for (index, finding) in findings.iter().enumerate() {
        check_cancelled(cancelled)?;
        if index > 0 {
            writer.write_all(b",").map_err(write_failed)?;
        }
        serialize(writer, finding)?;
    }
    writer.write_all(b"]}\n").map_err(write_failed)
}

fn serialize(writer: &mut impl Write, value: &impl Serialize) -> Result<(), Stop> {
    serde_json::to_writer(writer, value)
        .map_err(|error| output::output(format!("Cannot write the validation report. {error}")))
}

fn readable(
    writer: &mut impl Write,
    metadata: &Metadata<'_>,
    findings: &[Finding],
    cancelled: &dyn Fn() -> bool,
) -> Result<(), Stop> {
    write_metadata(writer, metadata).map_err(write_failed)?;
    for finding in findings {
        check_cancelled(cancelled)?;
        write_finding(writer, finding).map_err(write_failed)?;
    }
    Ok(())
}

fn write_metadata(writer: &mut impl Write, metadata: &Metadata<'_>) -> io::Result<()> {
    writeln!(writer, "AV2 validation report")?;
    writeln!(writer, "Report schema              {}", metadata.schema)?;
    writeln!(writer, "Source name                {:?}", metadata.source)?;
    writeln!(writer, "Finding selection          All retained findings")?;
    match &metadata.outcome {
        Outcome::Validated {
            fingerprint,
            digest,
            validation,
            policy_failed,
            retained,
        } => {
            writeln!(writer, "Source SHA-256             {}", digest.sha256)?;
            writeln!(writer, "Hashed source bytes        {}", digest.bytes)?;
            write_validation(writer, *fingerprint, validation, *policy_failed, *retained)
        }
        Outcome::Failed {
            code,
            reason,
            engine_version,
            codec_revision,
            validation_schema,
            spec_revision,
            scope,
            strict,
            context,
        } => {
            writeln!(writer, "Studio engine version      {engine_version}")?;
            writeln!(writer, "Splot revision             {codec_revision}")?;
            writeln!(writer, "Validation schema          {validation_schema}")?;
            writeln!(writer, "Specification              {spec_revision}")?;
            write_options(writer, *strict, *context)?;
            writeln!(writer, "\nFailed                     {code:?}")?;
            writeln!(writer, "{reason}")?;
            writeln!(writer, "{scope}")
        }
    }
}

fn write_validation(
    writer: &mut impl Write,
    fingerprint: SourceFingerprint,
    validation: &ValidationSummary,
    policy_failed: bool,
    retained: u64,
) -> io::Result<()> {
    writeln!(writer, "Source bytes               {}", fingerprint.len)?;
    if let Some((seconds, nanoseconds)) = fingerprint.modified {
        writeln!(
            writer,
            "Source modification time   {seconds}s + {nanoseconds}ns from Unix epoch"
        )?;
    } else {
        writeln!(writer, "Source modification time   Unavailable")?;
    }
    writeln!(writer, "Validation schema          {}", validation.schema)?;
    writeln!(
        writer,
        "Studio engine version      {:?}",
        validation.engine_version
    )?;
    writeln!(
        writer,
        "Splot revision             {:?}",
        validation.codec_revision
    )?;
    writeln!(
        writer,
        "Specification              {:?}",
        validation.spec_revision
    )?;
    writeln!(
        writer,
        "Observed policy failure    {}",
        yes_no(policy_failed)
    )?;
    write_options(writer, validation.strict, validation.context.as_ref())?;
    writeln!(writer, "\nScope")?;
    writeln!(writer, "{:?}", validation.scope)?;
    write_completion(writer, &validation.completion)?;
    writeln!(
        writer,
        "Checked OBUs               {}",
        validation.checked_obus
    )?;
    if let Some(offset) = validation.last_checked_obu_end {
        writeln!(
            writer,
            "Last checked OBU ends at   byte {offset} (exclusive)"
        )?;
    } else {
        writeln!(writer, "Last checked OBU ends at   Unavailable")?;
    }
    writeln!(
        writer,
        "Malformed units            {}",
        validation.malformed_units
    )?;
    writeln!(
        writer,
        "Stream finalization checks {}",
        yes_no(validation.finalized)
    )?;
    writeln!(writer, "\nFindings")?;
    writeln!(writer, "Errors                     {}", validation.errors)?;
    writeln!(writer, "Warnings                   {}", validation.warnings)?;
    writeln!(writer, "Informational findings     {}", validation.info)?;
    writeln!(writer, "Retained findings          {retained}")?;
    writeln!(
        writer,
        "Finding retention limit    {}",
        validation.finding_limit
    )?;
    writeln!(
        writer,
        "Omitted findings           {}\n",
        validation.omitted
    )
}

fn write_options(
    writer: &mut impl Write,
    strict: bool,
    context: Option<&ContextIdentity>,
) -> io::Result<()> {
    writeln!(writer, "Strict warnings policy     {}", yes_no(strict))?;
    if let Some(context) = context {
        writeln!(writer, "External HLS SHA-256       {:?}", context.sha256)?;
        writeln!(
            writer,
            "Sequence header IDs        {:?}",
            context.sequence_headers
        )?;
        writeln!(
            writer,
            "Operating point sets       {:?}",
            context.operating_point_sets
        )
    } else {
        writeln!(writer, "External HLS               None")
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "Yes" } else { "No" }
}

fn write_completion(writer: &mut impl Write, completion: &ValidationCompletion) -> io::Result<()> {
    let reason = match completion {
        ValidationCompletion::EndOfInput => "End of input",
        ValidationCompletion::MalformedInput => "Malformed input",
        ValidationCompletion::Cancelled => "Cancelled",
        ValidationCompletion::Io { message } => {
            return writeln!(writer, "Stopped                    Input error {message:?}");
        }
        ValidationCompletion::UnitTooLarge {
            offset,
            declared,
            cap,
        } => {
            return writeln!(
                writer,
                "Stopped                    Unit at byte {offset} declares {declared} bytes. Limit {cap}."
            );
        }
        ValidationCompletion::ObuLimit { max_obus } => {
            return writeln!(writer, "Stopped                    OBU limit {max_obus}");
        }
        ValidationCompletion::Unsupported => "Unsupported completion reason",
    };
    writeln!(writer, "Stopped                    {reason}")
}

fn write_finding(writer: &mut impl Write, finding: &Finding) -> io::Result<()> {
    let severity = match finding.severity {
        Severity::Error => "Error",
        Severity::Warning => "Warning",
        Severity::Info => "Info",
    };
    write!(writer, "{severity} {:?}", finding.rule_id)?;
    if let Some(section) = &finding.spec_section {
        write!(writer, " | section {section:?}")?;
    }
    if let Some(layer) = finding.extended_layer_id {
        write!(writer, " | layer {layer}")?;
    }
    if let Some(offset) = finding.file_offset {
        write!(writer, " | file byte {offset}")?;
        if let Some(bit) = finding.bit_offset {
            write!(writer, " bit {bit}")?;
        }
    }
    if let Some(offset) = finding.stream_offset {
        write!(writer, " | stream byte {offset}")?;
    }
    writeln!(writer, " | {:?}", finding.message)
}

#[cfg(test)]
mod tests;
