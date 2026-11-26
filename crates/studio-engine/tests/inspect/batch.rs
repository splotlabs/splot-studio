// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use std::fs;
use std::path::Path;

use studio_domain::analysis::AnalysisFailure;
use studio_domain::job::WirePath;
use studio_domain::protocol::{Command, ErrorCode, Event};
use studio_domain::validation::{
    BatchOutcome, MAX_BATCH_FILES, ReportFormat, ValidationCompletion, ValidationSettings,
};

use super::{Clip, Inspect, TestResult, encode, long_stream};

fn validate(source: &Path) -> Command {
    Command::BatchValidate {
        source: WirePath::from_path(source),
        settings: ValidationSettings::default(),
    }
}

fn outcome(inspect: &mut Inspect, id: u64) -> Result<BatchOutcome, Box<dyn std::error::Error>> {
    loop {
        let envelope = inspect.next()?;
        assert_eq!(envelope.request_id, id);
        match envelope.body {
            Event::BatchValidating { validation } => assert_eq!(validation, id),
            Event::BatchValidated { outcome } => return Ok(*outcome),
            event => return Err(format!("Expected validation evidence. Received {event:?}").into()),
        }
    }
}

fn findings(validation: u64, first: u64) -> Command {
    Command::BatchFindings {
        validation,
        first,
        count: 2,
    }
}

#[test]
fn mixed_inputs_retain_independent_evidence_and_export() -> TestResult {
    let dir = tempfile::tempdir()?;
    let (clean, _) = encode(dir.path(), &Clip::new(32, 32, "420", 1), "batch")?;
    let malformed = dir.path().join("truncated.obu");
    fs::write(&malformed, [0x01, 0x08, 0x02, 0x64])?;
    let unsupported = dir.path().join("unsupported.ivf");
    let mut wrong_codec = fs::read(&clean)?;
    wrong_codec[8..12].copy_from_slice(b"AV01");
    fs::write(&unsupported, wrong_codec)?;
    let missing = dir.path().join("missing.obu");

    let mut inspect = Inspect::spawn()?;
    let [clean_id, malformed_id, unsupported_id, missing_id] = inspect.send(vec![
        validate(&clean),
        validate(&malformed),
        validate(&unsupported),
        validate(&missing),
    ])?[..] else {
        return Err("Expected four validation IDs.".into());
    };
    let BatchOutcome::Validated { summary, .. } = outcome(&mut inspect, clean_id)? else {
        return Err("The valid IVF did not produce a report.".into());
    };
    assert!(matches!(
        summary.completion,
        ValidationCompletion::EndOfInput
    ));
    let BatchOutcome::Validated { summary, total, .. } = outcome(&mut inspect, malformed_id)?
    else {
        return Err("The truncated OBU did not produce a report.".into());
    };
    assert!(matches!(
        summary.completion,
        ValidationCompletion::MalformedInput
    ));
    assert!(summary.errors > 0 && total > 0);
    assert!(matches!(
        outcome(&mut inspect, unsupported_id)?,
        BatchOutcome::Failed {
            code: AnalysisFailure::Unsupported,
            ..
        }
    ));
    assert!(matches!(
        outcome(&mut inspect, missing_id)?,
        BatchOutcome::Failed {
            code: AnalysisFailure::Io,
            ..
        }
    ));
    let Event::BatchFindings {
        validation,
        first,
        total: available,
        findings: found,
    } = inspect.ask(findings(malformed_id, 0))?
    else {
        return Err("Expected retained findings.".into());
    };
    assert_eq!((validation, first, available), (malformed_id, 0, total));
    assert!(!found.is_empty() && found.len() <= 2);
    assert!(found.iter().all(|finding| !finding.rule_id.is_empty()));

    fs::write(&malformed, [0x01, 0x08])?;
    assert!(matches!(
        inspect.ask(findings(malformed_id, 0))?,
        Event::Error {
            code: ErrorCode::WrongState,
            ..
        }
    ));
    for validation in [malformed_id, missing_id] {
        check_export(
            &mut inspect,
            &dir.path().join(format!("{validation}.json")),
            validation,
        )?;
    }
    Ok(())
}

fn check_export(inspect: &mut Inspect, destination: &Path, validation: u64) -> TestResult {
    let export = || Command::BatchExport {
        validation,
        destination: WirePath::from_path(destination),
        format: ReportFormat::Json,
    };
    let request = inspect.next_id;
    inspect.send(vec![export()])?;
    let prepared = inspect.next()?;
    assert_eq!(prepared.request_id, request);
    let Event::BatchExportPrepared {
        validation: id,
        partial,
    } = prepared.body
    else {
        return Err("Expected the temporary report path before publication.".into());
    };
    assert_eq!(id, validation);
    let partial = partial.to_path_buf()?;
    assert_eq!(partial.parent(), destination.parent());
    assert_ne!(partial, destination);
    let complete = inspect.next()?;
    assert_eq!(complete.request_id, request);
    assert!(matches!(
        complete.body,
        Event::BatchExported { validation: id, bytes } if id == validation && bytes > 0
    ));
    assert!(!partial.exists());
    let original = fs::read(destination)?;
    assert!(matches!(inspect.ask(export())?, Event::Error { .. }));
    assert_eq!(fs::read(destination)?, original);
    Ok(())
}

#[test]
fn archive_caps_retained_reports_and_only_forgets_on_request() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("missing.obu");
    let mut inspect = Inspect::spawn()?;
    let first = inspect.next_id;
    for _ in 0..MAX_BATCH_FILES {
        let id = inspect.next_id;
        inspect.send(vec![validate(&path)])?;
        assert!(matches!(
            outcome(&mut inspect, id)?,
            BatchOutcome::Failed { .. }
        ));
    }
    assert!(matches!(
        inspect.ask(validate(&path))?,
        Event::Error {
            code: ErrorCode::Busy,
            ..
        }
    ));
    assert!(matches!(
        inspect.ask(findings(first, 0))?,
        Event::BatchFindings { validation, total: 0, .. } if validation == first
    ));
    assert!(matches!(
        inspect.ask(Command::BatchForget { validation: first })?,
        Event::BatchForgotten { validation } if validation == first
    ));
    assert!(matches!(
        inspect.ask(findings(first, 0))?,
        Event::Error { .. }
    ));
    let id = inspect.next_id;
    inspect.send(vec![validate(&path)])?;
    assert!(matches!(
        outcome(&mut inspect, id)?,
        BatchOutcome::Failed { .. }
    ));
    Ok(())
}

#[test]
fn preflight_rejects_invalid_context() -> TestResult {
    let mut inspect = Inspect::spawn()?;
    assert!(matches!(
        inspect.ask(Command::BatchCheckSettings {
            settings: ValidationSettings {
                strict: true,
                external_hls: Some(Vec::new())
            },
        })?,
        Event::Error {
            code: ErrorCode::Malformed,
            ..
        }
    ));
    assert!(matches!(
        inspect.ask(Command::BatchCheckSettings {
            settings: ValidationSettings::default()
        })?,
        Event::BatchSettingsChecked
    ));
    Ok(())
}

#[test]
fn cancelling_one_validation_preserves_other_reports() -> TestResult {
    let dir = tempfile::tempdir()?;
    let long = dir.path().join("long.obu");
    long_stream(&long)?;
    let mut inspect = Inspect::spawn()?;
    let earlier = inspect.next_id;
    let missing = dir.path().join("missing.obu");
    inspect.send(vec![validate(&missing)])?;
    assert!(matches!(
        outcome(&mut inspect, earlier)?,
        BatchOutcome::Failed { .. }
    ));
    let target = inspect.next_id;
    let [_, cancel, later] = inspect.send(vec![
        validate(&long),
        Command::BatchCancel { validation: target },
        validate(&missing),
    ])?[..] else {
        return Err("Expected three request IDs.".into());
    };
    let mut done = false;
    let mut ack = false;
    loop {
        let envelope = inspect.next()?;
        match (envelope.request_id, envelope.body) {
            (id, Event::CancelRequested) if id == cancel => {
                assert!(!done);
                ack = true;
            }
            (
                id,
                Event::Error {
                    code: ErrorCode::WrongState,
                    ..
                },
            ) if id == cancel => {
                assert!(done);
                ack = true;
            }
            (id, Event::BatchValidating { .. }) if id == target => {}
            (id, Event::BatchValidated { .. }) if id == target => done = true,
            (id, Event::BatchValidated { outcome }) if id == later => {
                assert!(matches!(
                    *outcome,
                    BatchOutcome::Failed {
                        code: AnalysisFailure::Io,
                        ..
                    }
                ));
                break;
            }
            (_, event) => return Err(format!("Unexpected batch event {event:?}.").into()),
        }
    }
    assert!(done && ack);
    assert!(matches!(
        inspect.ask(findings(earlier, 0))?,
        Event::BatchFindings { validation, .. } if validation == earlier
    ));
    Ok(())
}

#[test]
fn strict_policy_keeps_warning_evidence() -> TestResult {
    let dir = tempfile::tempdir()?;
    let (path, _) = encode(dir.path(), &Clip::new(32, 32, "420", 1), "strict")?;
    // A partial record header after a complete IVF record produces a warning.
    let mut bytes = fs::read(&path)?;
    bytes.push(0);
    fs::write(&path, bytes)?;

    let mut inspect = Inspect::spawn()?;
    let mut findings_by_policy = Vec::new();
    for strict in [false, true] {
        let id = inspect.next_id;
        inspect.send(vec![Command::BatchValidate {
            source: WirePath::from_path(&path),
            settings: ValidationSettings {
                strict,
                external_hls: None,
            },
        }])?;
        let BatchOutcome::Validated { summary, total, .. } = outcome(&mut inspect, id)? else {
            return Err("Expected warning evidence for the partial IVF header.".into());
        };
        assert_eq!(summary.errors, 0);
        assert!(summary.warnings > 0);
        assert_eq!(summary.policy_failed(), strict);
        let mut retained = Vec::new();
        while (retained.len() as u64) < total {
            let Event::BatchFindings { findings, .. } =
                inspect.ask(findings(id, retained.len() as u64))?
            else {
                return Err("Expected retained warnings.".into());
            };
            assert_ne!(findings, []);
            retained.extend(findings);
        }
        assert!(
            retained
                .iter()
                .any(|finding| finding.rule_id == "ivf/trailing-partial-frame-header")
        );
        findings_by_policy.push(retained);
    }
    assert_eq!(findings_by_policy[0], findings_by_policy[1]);
    Ok(())
}

#[test]
fn reports_belong_to_the_worker_that_validated_them() -> TestResult {
    let dir = tempfile::tempdir()?;
    let missing = dir.path().join("missing.obu");
    let mut inspect = Inspect::spawn()?;
    let id = inspect.next_id;
    inspect.send(vec![validate(&missing)])?;
    assert!(matches!(
        outcome(&mut inspect, id)?,
        BatchOutcome::Failed { .. }
    ));
    inspect.child.kill()?;
    drop(inspect);

    let mut replacement = Inspect::spawn()?;
    assert!(matches!(
        replacement.ask(findings(id, 0))?,
        Event::Error {
            code: ErrorCode::WrongState,
            ..
        }
    ));
    Ok(())
}

#[test]
fn a_rewritten_source_with_the_same_metadata_has_a_new_digest() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("source.obu");
    let original = [0x01, 0x08, 0x02, 0x64, 0];
    let replacement = [0x01, 0x08, 0x02, 0x64, 0x80];
    fs::write(&path, original)?;
    let modified = fs::metadata(&path)?.modified()?;
    let mut inspect = Inspect::spawn()?;

    let id = inspect.next_id;
    inspect.send(vec![validate(&path)])?;
    let BatchOutcome::Validated {
        fingerprint,
        digest,
        ..
    } = outcome(&mut inspect, id)?
    else {
        return Err("Expected source validation evidence.".into());
    };
    assert_eq!(digest.bytes, original.len() as u64);
    assert_eq!(digest.sha256, sha256(&original));

    fs::write(&path, replacement)?;
    fs::File::options()
        .write(true)
        .open(&path)?
        .set_times(fs::FileTimes::new().set_modified(modified))?;
    let reopened = inspect.open(1, &path)?;
    assert_eq!(reopened.fingerprint, fingerprint);
    let reopened = reopened.digest.ok_or("Expected reopened source digest.")?;
    assert_eq!(reopened.bytes, digest.bytes);
    assert_eq!(reopened.sha256, sha256(&replacement));
    assert_ne!(reopened.sha256, digest.sha256);
    Ok(())
}

#[test]
fn a_stopped_scan_digests_only_the_bytes_read() -> TestResult {
    use studio_domain::analysis::{ProblemKind, ScanOutcome};

    let dir = tempfile::tempdir()?;
    let path = dir.path().join("large-unit.obu");
    // Temporal delimiter, then an Annex B length of 64 MiB plus one byte.
    let mut contents = vec![0x01, 0x08, 0x81, 0x80, 0x80, 0x20];
    contents.resize(128 * 1024, 0);
    fs::write(&path, &contents)?;
    let summary = Inspect::spawn()?.open(1, &path)?;
    assert!(matches!(summary.outcome, ScanOutcome::Stopped { problem }
        if problem.kind == ProblemKind::LimitReached));
    let digest = summary
        .digest
        .ok_or("Expected a digest for the scanned prefix.")?;
    let prefix = usize::try_from(digest.bytes)?;
    assert!((6..contents.len()).contains(&prefix));
    assert_eq!(digest.sha256, sha256(&contents[..prefix]));
    Ok(())
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;

    let mut hex = String::new();
    for byte in Sha256::digest(bytes) {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}
