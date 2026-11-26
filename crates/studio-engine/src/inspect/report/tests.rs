// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use std::cell::Cell;
use std::error::Error;
use std::fs;

use serde_json::Value;

use super::*;

type TestResult = Result<(), Box<dyn Error>>;

struct Fixture {
    directory: tempfile::TempDir,
    source: std::path::PathBuf,
    id: FileId,
    outcome: BatchOutcome,
    finding: Finding,
}

impl Fixture {
    fn new() -> Result<Self, Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("sample.av2");
        fs::write(&source, b"source bytes")?;
        let id = FileId::of_path(&source)?;
        let fingerprint = SourceFingerprint::of(&fs::metadata(&source)?);
        let outcome = BatchOutcome::Validated {
            fingerprint,
            digest: SourceDigest {
                sha256: "b".repeat(64),
                bytes: 9,
            },
            summary: Box::new(ValidationSummary {
                schema: VALIDATION_SCHEMA,
                spec_revision: "AV2 v1.0.0".into(),
                codec_revision: "codec revision".into(),
                engine_version: "engine version".into(),
                strict: true,
                context: None,
                scope: "Stream syntax only.".into(),
                completion: ValidationCompletion::Cancelled,
                checked_obus: 5,
                last_checked_obu_end: Some(9),
                malformed_units: 0,
                finalized: false,
                errors: 0,
                warnings: 2,
                info: 0,
                finding_limit: 1,
                omitted: 1,
            }),
            total: 1,
        };
        let finding: Finding = serde_json::from_value(serde_json::json!({
            "rule_id": "rule-a",
            "severity": "warning",
            "message": "Line 1\n\"<tag>\"",
            "spec_section": "5.2.2",
            "file_offset": 8,
            "stream_offset": 4,
            "bit_offset": 1,
            "extended_layer_id": 3
        }))?;
        Ok(Self {
            directory,
            source,
            id,
            outcome,
            finding,
        })
    }

    fn report(&self) -> Report<'_> {
        Report {
            source: &self.source,
            source_id: Some(&self.id),
            outcome: &self.outcome,
            strict: true,
            context: None,
            findings: std::slice::from_ref(&self.finding),
        }
    }
}

#[test]
fn exports_scope_and_all_retained_findings() -> TestResult {
    let fixture = Fixture::new()?;
    let directory = fixture.directory.path();
    let json_file = directory.join("report.json");
    let text_file = directory.join("report.txt");
    for (path, format) in [
        (&json_file, ReportFormat::Json),
        (&text_file, ReportFormat::Text),
    ] {
        let bytes = export(path, &fixture.report(), format, &|_| Ok(()), &|| false)?;
        assert_eq!(fs::metadata(path)?.len(), bytes);
    }
    let json_text = fs::read_to_string(&json_file)?;
    let data: Value = serde_json::from_str(&json_text)?;
    assert_eq!(data["metadata"]["schema"], REPORT_SCHEMA);
    assert_eq!(data["metadata"]["source"], "sample.av2");
    assert_eq!(data["metadata"]["finding_selection"], FINDING_SELECTION);
    let outcome = &data["metadata"]["outcome"];
    assert_eq!(outcome["policy_failed"], true);
    assert_eq!(outcome["digest"]["sha256"], "b".repeat(64));
    assert_eq!(outcome["digest"]["bytes"], 9);
    assert_eq!(outcome["validation"]["completion"]["kind"], "cancelled");
    assert_eq!(outcome["validation"]["warnings"], 2);
    assert_eq!(outcome["retained"], 1);
    assert_eq!(outcome["validation"]["omitted"], 1);
    assert_eq!(data["findings"], serde_json::json!([fixture.finding]));

    let readable = fs::read_to_string(text_file)?;
    assert!(readable.contains("Source name                \"sample.av2\""));
    assert!(readable.contains("Splot revision             \"codec revision\""));
    assert!(readable.contains("Stopped                    Cancelled"));
    assert!(readable.contains("Finding selection          All retained findings"));
    assert!(readable.contains("Hashed source bytes        9"));
    assert!(readable.contains("Warnings                   2"));
    assert!(readable.contains("Omitted findings           1"));
    assert!(
        readable
            .contains("layer 3 | file byte 8 bit 1 | stream byte 4 | \"Line 1\\n\\\"<tag>\\\"\"")
    );
    let ancestor = directory.display().to_string();
    assert!(!json_text.contains(&ancestor));
    assert!(!readable.contains(&ancestor));
    Ok(())
}

#[test]
fn redacts_paths_from_failed_reports() -> TestResult {
    let fixture = Fixture::new()?;
    let outcome = BatchOutcome::Failed {
        code: AnalysisFailure::Io,
        message: format!("Cannot open {}", fixture.source.display()),
    };
    let report = Report {
        source_id: None,
        outcome: &outcome,
        findings: &[],
        ..fixture.report()
    };
    let destination = fixture.directory.path().join("failed.json");
    export(
        &destination,
        &report,
        ReportFormat::Json,
        &|_| Ok(()),
        &|| false,
    )?;
    let text = fs::read_to_string(destination)?;
    let data: Value = serde_json::from_str(&text)?;
    assert_eq!(data["metadata"]["outcome"]["code"], "io");
    assert_eq!(data["metadata"]["outcome"]["strict"], true);
    assert_eq!(data["metadata"]["outcome"]["spec_revision"], "AV2 v1.0.0");
    assert_eq!(data["metadata"]["outcome"]["scope"], NO_VALIDATION_SCOPE);
    assert_eq!(data["findings"], serde_json::json!([]));
    assert!(!text.contains(&fixture.source.display().to_string()));
    Ok(())
}

#[test]
fn refuses_collisions_and_removes_partial_files_after_cancel() -> TestResult {
    let fixture = Fixture::new()?;
    let directory = fixture.directory.path();
    let output = directory.join("report.json");
    fs::write(&output, b"existing")?;
    let stop = export(
        &output,
        &fixture.report(),
        ReportFormat::Json,
        &|_| Ok(()),
        &|| false,
    )
    .err();
    assert!(matches!(
        stop,
        Some(Stop::Failed(failure)) if failure.code == FailureCode::DestinationExists
    ));
    assert_eq!(fs::read(&output)?, b"existing");
    let stop = export(
        &fixture.source,
        &fixture.report(),
        ReportFormat::Json,
        &|_| Ok(()),
        &|| false,
    )
    .err();
    assert!(matches!(
        stop,
        Some(Stop::Failed(failure)) if failure.code == FailureCode::SourceIsDestination
    ));
    assert_eq!(fs::read(&fixture.source)?, b"source bytes");
    fs::remove_file(&output)?;
    let calls = Cell::new(0);
    let announced = Cell::new(false);
    let stop = export(
        &output,
        &fixture.report(),
        ReportFormat::Text,
        &|partial| {
            assert!(partial.starts_with(directory));
            assert_eq!(fs::metadata(partial).map(|meta| meta.len()).ok(), Some(0));
            announced.set(true);
            Ok(())
        },
        &|| calls.replace(calls.get() + 1) > 0,
    );
    assert!(announced.get());
    assert!(matches!(stop, Err(Stop::Cancelled)));
    assert_eq!(fs::read_dir(directory)?.count(), 1);
    Ok(())
}

#[test]
fn refuses_the_source_path_after_removal() -> TestResult {
    let fixture = Fixture::new()?;
    fs::remove_file(&fixture.source)?;
    let nested = fixture.directory.path().join("nested");
    fs::create_dir(&nested)?;
    let same_location = nested.join("../sample.av2");
    let outcome = BatchOutcome::Failed {
        code: AnalysisFailure::Io,
        message: "Source removed.".into(),
    };
    let failed = Report {
        source_id: None,
        outcome: &outcome,
        findings: &[],
        ..fixture.report()
    };
    for report in [fixture.report(), failed] {
        let result = export(
            &same_location,
            &report,
            ReportFormat::Json,
            &|_| Ok(()),
            &|| false,
        );
        assert!(matches!(
            result,
            Err(Stop::Failed(failure)) if failure.code == FailureCode::SourceIsDestination
        ));
    }
    assert!(!fixture.source.exists());
    Ok(())
}

#[test]
fn disk_write_failure_stops_both_renderers() -> TestResult {
    struct Full;
    impl Write for Full {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::StorageFull.into())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let fixture = Fixture::new()?;
    let report = fixture.report();
    let metadata = Metadata::new(&report)?;
    for result in [
        json(&mut Full, &metadata, report.findings, &|| false),
        readable(&mut Full, &metadata, report.findings, &|| false),
    ] {
        assert!(matches!(
            result,
            Err(Stop::Failed(failure)) if failure.code == FailureCode::Output
        ));
    }
    Ok(())
}

#[cfg(windows)]
#[test]
fn refuses_win32_report_aliases_of_an_absent_source() -> TestResult {
    let fixture = Fixture::new()?;
    fs::remove_file(&fixture.source)?;
    let outcome = BatchOutcome::Failed {
        code: AnalysisFailure::Io,
        message: "Source removed.".into(),
    };
    let failed = Report {
        source_id: None,
        outcome: &outcome,
        findings: &[],
        ..fixture.report()
    };
    for name in ["sample.av2.", "sample.av2 ", "sample.av2:$DATA"] {
        let alias = fixture.directory.path().join(name);
        let result = export(&alias, &failed, ReportFormat::Json, &|_| Ok(()), &|| false);
        assert!(matches!(
            result,
            Err(Stop::Failed(failure)) if failure.code == FailureCode::Output
        ));
        assert!(!fixture.source.exists());
    }
    assert_eq!(fs::read_dir(fixture.directory.path())?.count(), 0);
    Ok(())
}
