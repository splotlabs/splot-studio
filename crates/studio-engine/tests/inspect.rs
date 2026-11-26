// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Inspect process tests: the real engine binary in `inspect` mode, reading
//! IVF and Annex B files that the job path encodes from generated Y4M.

#[path = "inspect/batch.rs"]
mod batch;
mod common;
#[path = "inspect/layers.rs"]
mod layers;

use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command as Process, Stdio};

use common::{Clip, TestResult, run_job, spec};
use studio_domain::analysis::{
    AnalysisFailure, DocumentSummary, Element, Framing, KindFilter, LocateTarget,
    MAX_PAGE_ELEMENTS, ObuFact, ProblemKind, Row, RowFilter, ScanOutcome, Span,
};
use studio_domain::identity::{IdentityCompletion, IdentityKind};
use studio_domain::job::WirePath;
use studio_domain::preview::{
    GrainPolicy, PictureBox, PictureFrame, PictureHeader, PictureOptions, PictureTarget,
    PictureView,
};
use studio_domain::protocol::{
    BuildIdentity, Command, Envelope, ErrorCode, Event, read_event, read_message, write_message,
};
use studio_domain::settings::OutputKind;
use studio_domain::syntax::{SYNTAX_SCHEMA, SyntaxEvent, SyntaxValue};
use studio_domain::timeline::{TimelineSelection, TimelineTarget, TimelineTime, TimelineView};
use studio_domain::validation::{VALIDATION_SCHEMA, ValidationCompletion, ValidationSettings};

const BIN: &str = env!("CARGO_BIN_EXE_splot-studio-engine");

/// An inspect process whose pipes the test drives.
struct Inspect {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    next_id: u64,
}

impl Inspect {
    fn spawn() -> Result<Self, Box<dyn Error>> {
        Self::spawn_mode("inspect")
    }

    fn spawn_mode(mode: &str) -> Result<Self, Box<dyn Error>> {
        let mut child = Process::new(BIN)
            .arg(mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or("no stdout")?;
        Ok(Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        })
    }

    /// Writes all `commands` at once and returns their request ids.
    fn send(&mut self, commands: Vec<Command>) -> Result<Vec<u64>, Box<dyn Error>> {
        let mut bytes = Vec::new();
        let mut ids = Vec::new();
        for command in commands {
            write_message(&mut bytes, &Envelope::new(self.next_id, command))?;
            ids.push(self.next_id);
            self.next_id += 1;
        }
        self.stdin
            .as_mut()
            .ok_or("stdin closed")?
            .write_all(&bytes)?;
        Ok(ids)
    }

    fn next(&mut self) -> Result<Envelope<Event>, Box<dyn Error>> {
        Ok(read_message::<Event>(&mut self.stdout)?.ok_or("inspect closed stdout")?)
    }

    /// Sends one command and returns its one answer.
    fn ask(&mut self, command: Command) -> Result<Event, Box<dyn Error>> {
        let [id] = self.send(vec![command])?[..] else {
            return Err("one id".into());
        };
        let answer = self.next()?;
        assert_eq!(answer.request_id, id);
        Ok(answer.body)
    }

    fn open(&mut self, document: u64, path: &Path) -> Result<DocumentSummary, Box<dyn Error>> {
        match self.ask(Command::AnalysisOpen {
            document,
            source: WirePath::from_path(path),
        })? {
            Event::AnalysisOpened {
                document: d,
                summary,
            } if d == document => Ok(summary),
            other => Err(format!("open answered {other:?}").into()),
        }
    }

    /// Every element of an open document, page by page.
    fn all(&mut self, document: u64, total: u64) -> Result<Vec<Element>, Box<dyn Error>> {
        let mut elements = Vec::new();
        while (elements.len() as u64) < total {
            let first = elements.len() as u64;
            match self.ask(Command::AnalysisPage {
                document,
                filter: RowFilter::default(),
                first,
                count: MAX_PAGE_ELEMENTS,
            })? {
                Event::AnalysisPage {
                    first: f,
                    total: t,
                    rows,
                    ..
                } if f == first && t == total && !rows.is_empty() => {
                    for (row, index) in rows.into_iter().zip(first..) {
                        assert_eq!(row.index, index);
                        elements.push(row.element);
                    }
                }
                other => return Err(format!("page answered {other:?}").into()),
            }
        }
        Ok(elements)
    }

    fn bytes(&mut self, document: u64, span: Span) -> Result<Vec<u8>, Box<dyn Error>> {
        let len = u32::try_from(span.len)?;
        match self.ask(Command::AnalysisBytes {
            document,
            offset: span.offset,
            len,
        })? {
            Event::AnalysisBytes { bytes, .. } => Ok(bytes),
            other => Err(format!("bytes answered {other:?}").into()),
        }
    }
}

impl Drop for Inspect {
    fn drop(&mut self) {
        self.stdin = None;
        let _ = self.child.wait();
    }
}

fn failure(event: &Event) -> Option<AnalysisFailure> {
    match event {
        Event::AnalysisFailed { code, .. } => Some(*code),
        _ => None,
    }
}

fn obus(elements: &[Element]) -> Vec<&ObuFact> {
    elements
        .iter()
        .filter_map(|e| match e {
            Element::Obu { obu } => Some(obu),
            _ => None,
        })
        .collect()
}

/// Encodes `clip` to both output kinds and returns the IVF and `.obu` paths.
fn encode(dir: &Path, clip: &Clip, name: &str) -> Result<(PathBuf, PathBuf), Box<dyn Error>> {
    let source = dir.join(format!("{name}.y4m"));
    clip.write(&source)?;
    let mut made = Vec::new();
    for kind in [OutputKind::Ivf, OutputKind::Av2AnnexB] {
        let out = dir.join(format!("{name}.{}", kind.extension()));
        let (_, end) = run_job(spec(&source, &out, kind), |_, _| {});
        assert!(matches!(end, Event::Completed { .. }), "{end:?}");
        made.push(out);
    }
    let [ivf, obu] = made.try_into().map_err(|_| "two outputs")?;
    Ok((ivf, obu))
}

/// The IVF and Annex B outputs of one encode carry the same OBUs: the IVF
/// document's elementary-stream spans are the `.obu` document's file spans,
/// and the bytes the engine returns are the file's.
#[test]
fn real_outputs_in_both_framings_and_depths() -> TestResult {
    let dir = tempfile::tempdir()?;
    let mut inspect = Inspect::spawn()?;
    let mut document = 0;
    for (colorspace, frames) in [("420", 5), ("420p10", 4)] {
        let clip = Clip::new(32, 32, colorspace, frames);
        let (ivf, obu) = encode(dir.path(), &clip, colorspace)?;
        // Swapped names: framing comes from the contents.
        let (ivf_named_obu, obu_named_ivf) = (
            dir.path().join(format!("{colorspace}-ivf.obu")),
            dir.path().join(format!("{colorspace}-obu.ivf")),
        );
        fs::copy(&ivf, &ivf_named_obu)?;
        fs::copy(&obu, &obu_named_ivf)?;

        document += 1;
        let ivf_doc = document;
        let summary = inspect.open(ivf_doc, &ivf_named_obu)?;
        assert_eq!(summary.framing, Framing::Ivf);
        assert_eq!(summary.outcome, ScanOutcome::Complete);
        assert_eq!(summary.fingerprint.len, fs::metadata(&ivf)?.len());
        let header = summary.ivf_header.ok_or("no IVF header")?;
        assert_eq!(
            (header.fourcc, header.width, header.height),
            (*b"AV02", 32, 32)
        );
        assert_eq!(summary.packets, frames as u64);
        let ivf_elements = inspect.all(ivf_doc, summary.elements)?;
        let ivf_summary = summary;

        document += 1;
        let obu_doc = document;
        let summary = inspect.open(obu_doc, &obu_named_ivf)?;
        assert_eq!(summary.framing, Framing::AnnexB);
        assert_eq!(summary.outcome, ScanOutcome::Complete);
        assert!(summary.ivf_header.is_none());
        let obu_elements = inspect.all(obu_doc, summary.elements)?;

        let packets: Vec<_> = ivf_elements
            .iter()
            .filter_map(|e| match e {
                Element::Packet { packet } => Some((packet.index, packet.pts)),
                _ => None,
            })
            .collect();
        assert_eq!(
            packets,
            (0..frames as u64).map(|i| (i, i)).collect::<Vec<_>>()
        );
        let in_ivf = obus(&ivf_elements);
        let in_obu = obus(&obu_elements);
        assert_eq!(in_ivf.len(), in_obu.len());
        assert_eq!(
            in_obu.first().map(|o| o.type_name.as_str()),
            Some("OBU_TEMPORAL_DELIMITER")
        );
        assert!(in_obu.iter().any(|o| o.type_name == "OBU_SEQUENCE_HEADER"));
        let file = fs::read(&obu)?;
        for (a, b) in in_ivf.iter().zip(&in_obu) {
            assert_eq!(a.stream.offset, b.length_prefix.offset);
            assert_eq!(a.stream.len, b.length_prefix.len + b.obu.len);
            assert_eq!(
                (
                    a.obu_type,
                    a.temporal_layer_id,
                    a.embedded_layer_id,
                    a.extended_layer_id,
                    a.obu.len
                ),
                (
                    b.obu_type,
                    b.temporal_layer_id,
                    b.embedded_layer_id,
                    b.extended_layer_id,
                    b.obu.len
                )
            );
            // The type bits of the header byte in the file.
            let at = usize::try_from(b.header.offset)?;
            assert_eq!((file[at] >> 2) & 0x1F, b.obu_type);
        }
        let sequence = in_obu
            .iter()
            .find(|o| o.type_name == "OBU_SEQUENCE_HEADER")
            .ok_or("no sequence header")?;
        let span = sequence.obu;
        let start = usize::try_from(span.offset)?;
        assert_eq!(
            inspect.bytes(obu_doc, span)?,
            file[start..start + usize::try_from(span.len)?]
        );

        for (document, elements) in [(ivf_doc, &ivf_elements), (obu_doc, &obu_elements)] {
            check_sequence_syntax(&mut inspect, document, elements)?;
            check_validation_pages(&mut inspect, document)?;
        }

        locate_and_filter(&mut inspect, ivf_doc, &ivf_summary, &ivf_elements, frames)?;
    }
    Ok(())
}

#[test]
fn external_sequence_header_changes_validation_through_inspect() -> TestResult {
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("source.y4m");
    Clip::new(32, 32, "420", 3).write(&source)?;
    let encoded = dir.path().join("encoded.obu");
    let (_, end) = run_job(spec(&source, &encoded, OutputKind::Av2AnnexB), |_, _| {});
    assert!(matches!(end, Event::Completed { .. }), "{end:?}");

    let mut inspect = Inspect::spawn()?;
    let summary = inspect.open(1, &encoded)?;
    let elements = inspect.all(1, summary.elements)?;
    let mut stream = fs::read(&encoded)?;
    let mut context = None;
    for obu in obus(&elements).into_iter().rev() {
        if obu.type_name == "OBU_SEQUENCE_HEADER" {
            let start = usize::try_from(obu.length_prefix.offset)?;
            let end = usize::try_from(obu.obu.offset + obu.obu.len)?;
            let bytes: Vec<_> = stream.drain(start..end).collect();
            if let Some(previous) = &context {
                assert_eq!(&bytes, previous);
            }
            context = Some(bytes);
        }
    }
    let context = context.ok_or("no sequence header")?;
    let stripped = dir.path().join("without-sequence.obu");
    fs::write(&stripped, stream)?;
    inspect.open(2, &stripped)?;

    for external_hls in [None, Some(context)] {
        let supplied = external_hls.is_some();
        let validation = inspect.next_id;
        let event = inspect.ask(Command::AnalysisValidate {
            document: 2,
            settings: ValidationSettings {
                strict: false,
                external_hls,
            },
        })?;
        let Event::AnalysisValidated { summary, total, .. } = event else {
            return Err(format!("validation answered {event:?}").into());
        };
        assert_eq!(summary.completion, ValidationCompletion::EndOfInput);
        assert_eq!(summary.context.is_some(), supplied);
        if let Some(identity) = summary.context {
            assert_eq!(identity.sequence_headers.len(), 1);
            assert_eq!(identity.sha256.len(), 64);
        }

        let event = inspect.ask(Command::AnalysisFindings {
            document: 2,
            validation,
            first: 0,
            count: MAX_PAGE_ELEMENTS,
        })?;
        let Event::AnalysisFindings { findings, .. } = event else {
            return Err(format!("findings answered {event:?}").into());
        };
        assert_eq!(findings.len() as u64, total);
        let missing = findings
            .iter()
            .find(|finding| finding.rule_id == "hls/unavailable-sequence-header");
        assert_eq!(missing.is_none(), supplied, "{findings:?}");
        if let Some(finding) = missing {
            assert!(finding.file_offset.is_some());
            assert_eq!(finding.file_offset, finding.stream_offset);
            assert_eq!(finding.extended_layer_id, Some(0));
        }
    }
    Ok(())
}

fn check_sequence_syntax(inspect: &mut Inspect, document: u64, elements: &[Element]) -> TestResult {
    let (element, obu) = elements
        .iter()
        .enumerate()
        .find_map(|(index, element)| match element {
            Element::Obu { obu } if obu.type_name == "OBU_SEQUENCE_HEADER" => {
                Some((index as u64, obu))
            }
            _ => None,
        })
        .ok_or("no sequence header")?;
    let trace = inspect.next_id;
    let Event::AnalysisSyntaxReady {
        document: generation,
        element: selected,
        summary,
        total,
    } = inspect.ask(Command::AnalysisSyntax { document, element })?
    else {
        return Err("expected syntax evidence".into());
    };
    assert_eq!(
        (generation, selected, total),
        (document, element, summary.retained_events)
    );
    assert_eq!(summary.schema_version, SYNTAX_SCHEMA);
    assert!(total > 3);
    let mut retained = Vec::new();
    while (retained.len() as u64) < total {
        let first = retained.len() as u64;
        let Event::AnalysisSyntaxPage {
            document: generation,
            trace: identity,
            element: selected,
            first: at,
            total: available,
            events,
        } = inspect.ask(Command::AnalysisSyntaxPage {
            document,
            trace,
            first,
            count: 7,
        })?
        else {
            return Err("expected syntax page".into());
        };
        assert_eq!(
            (generation, identity, selected, at, available),
            (document, trace, element, first, total)
        );
        assert!(!events.is_empty() && events.len() <= 7);
        retained.extend(events);
    }
    assert_eq!(retained.len() as u64, total);
    assert!(retained.iter().any(|event| matches!(
        event,
        SyntaxEvent::Signaled { name, value: SyntaxValue::Unsigned(value), span: Some(span), .. }
        if name == "obu_type" && *value == u64::from(obu.obu_type)
            && span.bit_len == 5
            && span.start.file_bit == obu.header.offset * 8 + 1
            && span.start.stream_bit == (obu.stream.offset + obu.length_prefix.len) * 8 + 1
    )));
    assert!(
        retained
            .iter()
            .any(|event| matches!(event, SyntaxEvent::Absent { .. }))
    );

    assert!(matches!(
        inspect.ask(Command::AnalysisSyntax { document, element })?,
        Event::AnalysisSyntaxReady { .. }
    ));
    assert_eq!(
        failure(&inspect.ask(Command::AnalysisSyntaxPage {
            document,
            trace,
            first: 0,
            count: 1
        })?),
        Some(AnalysisFailure::InvalidRequest),
    );
    Ok(())
}

fn check_validation_pages(inspect: &mut Inspect, document: u64) -> TestResult {
    let start = || Command::AnalysisValidate {
        document,
        settings: ValidationSettings::default(),
    };
    let validation = inspect.next_id;
    let Event::AnalysisValidated {
        document: generation,
        total,
        ..
    } = inspect.ask(start())?
    else {
        return Err("expected a validation report".into());
    };
    assert_eq!(generation, document);
    let findings = |validation, first, count| Command::AnalysisFindings {
        document,
        validation,
        first,
        count,
    };
    let mut first = 0;
    loop {
        let Event::AnalysisFindings {
            document: generation,
            validation: identity,
            first: at,
            total: available,
            findings: entries,
        } = inspect.ask(findings(validation, first, 2))?
        else {
            return Err("expected validation findings".into());
        };
        assert_eq!(
            (generation, identity, at, available),
            (document, validation, first, total)
        );
        assert!(entries.len() <= 2);
        assert_eq!(entries.is_empty(), total == 0);
        first += entries.len() as u64;
        if first >= total {
            assert_eq!(first, total);
            break;
        }
    }
    assert!(
        matches!(inspect.ask(start())?, Event::AnalysisValidated { total: next, .. } if next == total)
    );
    assert_eq!(
        failure(&inspect.ask(findings(validation, 0, 1))?),
        Some(AnalysisFailure::InvalidRequest),
    );
    Ok(())
}

/// Every OBU header byte of a real IVF document locates that OBU, inside
/// its packet; a type filter lists as many rows as the summary counts.
fn locate_and_filter(
    inspect: &mut Inspect,
    ivf_doc: u64,
    ivf_summary: &DocumentSummary,
    ivf_elements: &[Element],
    frames: usize,
) -> TestResult {
    let tiles = ivf_elements
        .iter()
        .enumerate()
        .filter_map(|(at, e)| match e {
            Element::Obu { obu } => Some((at as u64, obu)),
            _ => None,
        });
    for (index, obu) in tiles {
        match inspect.ask(Command::AnalysisLocate {
            document: ivf_doc,
            filter: RowFilter::default(),
            target: LocateTarget::Offset {
                offset: obu.header.offset,
            },
        })? {
            Event::AnalysisLocated {
                row: Some(row),
                found,
                ..
            } => {
                assert_eq!((row, found.index), (index, index));
                let parent = found.parent.ok_or("an IVF OBU has a packet")?;
                let packet = &ivf_elements[usize::try_from(parent)?];
                assert!(
                    matches!(packet, Element::Packet { packet } if Some(packet.index) == obu.packet)
                );
            }
            other => return Err(format!("locate answered {other:?}").into()),
        }
    }
    let counted = ivf_summary
        .obu_types
        .iter()
        .find(|t| t.type_name == "OBU_TEMPORAL_DELIMITER")
        .ok_or("no temporal delimiters counted")?;
    assert_eq!(counted.count, frames as u64);
    match inspect.ask(Command::AnalysisPage {
        document: ivf_doc,
        filter: RowFilter {
            kind: KindFilter::Obu {
                obu_type: counted.obu_type,
            },
            layer: None,
        },
        first: 0,
        count: MAX_PAGE_ELEMENTS,
    })? {
        Event::AnalysisPage { total, rows, .. } => {
            assert_eq!((total, rows.len() as u64), (counted.count, counted.count));
        }
        other => return Err(format!("page answered {other:?}").into()),
    }
    Ok(())
}

/// A 32 MiB Annex B stream: a temporal delimiter, then padding OBUs of
/// 1 MiB, so reading it takes long enough to be cancelled.
fn long_stream(path: &Path) -> std::io::Result<()> {
    let mut bytes = vec![0x01, 0x08];
    for _ in 0..32 {
        // leb128 of 2^20 + 1 (the padding header and its payload), the
        // padding header, the payload.
        bytes.extend([0x81, 0x80, 0x40, 0x64]);
        bytes.resize(bytes.len() + (1 << 20), 0);
    }
    fs::write(path, bytes)
}

#[test]
fn progressive_open_serves_pages_before_its_terminal_reply() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("long.obu");
    long_stream(&path)?;
    let mut inspect = Inspect::spawn()?;
    let [open, page] = inspect.send(vec![
        Command::AnalysisOpenProgressive {
            document: 1,
            source: WirePath::from_path(&path),
        },
        Command::AnalysisPage {
            document: 1,
            filter: RowFilter::default(),
            first: 0,
            count: 1,
        },
    ])?[..] else {
        return Err("Expected two request IDs.".into());
    };

    let partial = inspect.next()?;
    assert_eq!(partial.request_id, open);
    assert!(matches!(
        partial.body,
        Event::AnalysisOpenProgress { document: 1, summary }
            if summary.outcome == ScanOutcome::Scanning && summary.elements > 0 &&
                summary.digest.as_ref().is_some_and(|digest| digest.bytes < summary.fingerprint.len)
    ));
    let response = inspect.next()?;
    assert_eq!(response.request_id, page);
    assert!(matches!(
        response.body,
        Event::AnalysisPage { document: 1, total, rows, .. }
            if total > 0 && rows.len() == 1
    ));
    let complete = loop {
        let response = inspect.next()?;
        assert_eq!(response.request_id, open);
        match response.body {
            Event::AnalysisOpenProgress { .. } => {}
            Event::AnalysisOpened {
                document: 1,
                summary,
            } => break summary,
            other => return Err(format!("Unexpected open reply {other:?}.").into()),
        }
    };
    assert_eq!(complete, inspect.open(2, &path)?);
    assert_eq!(complete.outcome, ScanOutcome::Complete);
    Ok(())
}

/// Whether the document is open after a cancel answered `cancel` and its
/// open answered `open`.
fn kept_after_cancel(cancel: &Event, open: &Event) -> Result<bool, String> {
    // The order is a real race: cancelled before reading (nothing kept),
    // while reading (what was read is kept and the outcome says why it
    // stopped), or too late (the whole document).
    Ok(match (cancel, open) {
        (
            Event::CancelRequested,
            Event::AnalysisFailed {
                code: AnalysisFailure::Cancelled,
                ..
            },
        ) => false,
        (Event::CancelRequested, Event::AnalysisOpened { summary, .. }) => {
            match &summary.outcome {
                ScanOutcome::Stopped { problem } => {
                    assert_eq!(problem.kind, ProblemKind::Cancelled, "{summary:?}");
                    assert!(summary.obus >= 1 && summary.unparsed == 1, "{summary:?}");
                }
                other => assert_eq!(*other, ScanOutcome::Complete),
            }
            true
        }
        (
            Event::Error {
                code: ErrorCode::WrongState,
                ..
            },
            Event::AnalysisOpened { summary, .. },
        ) => {
            assert_eq!(summary.outcome, ScanOutcome::Complete);
            true
        }
        other => return Err(format!("inconsistent: {other:?}")),
    })
}

#[test]
fn a_cancel_and_the_open_it_cancels_agree() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("long.obu");
    long_stream(&path)?;
    let mut inspect = Inspect::spawn()?;
    let source = WirePath::from_path(&path);
    let ids = inspect.send(vec![
        Command::AnalysisOpen {
            document: 1,
            source: source.clone(),
        },
        Command::AnalysisCancel { document: 1 },
    ])?;
    let (mut open, mut cancel) = (None, None);
    for _ in 0..2 {
        let answer = inspect.next()?;
        if answer.request_id == ids[0] {
            open = Some(answer.body);
        } else {
            cancel = Some(answer.body);
        }
    }
    let (Some(open), Some(cancel)) = (open, cancel) else {
        return Err("both answered".into());
    };
    let kept = kept_after_cancel(&cancel, &open)?;
    let page = inspect.ask(Command::AnalysisPage {
        document: 1,
        filter: RowFilter::default(),
        first: 0,
        count: 1,
    })?;
    if kept {
        assert!(matches!(page, Event::AnalysisPage { .. }), "{page:?}");
        assert!(matches!(
            inspect.ask(Command::AnalysisClose { document: 1 })?,
            Event::AnalysisClosed { document: 1 }
        ));
    } else {
        assert_eq!(failure(&page), Some(AnalysisFailure::UnknownDocument));
    }
    // Nothing left to cancel, and the inspect process refuses the job
    // cancel, which names no document.
    for again in [Command::AnalysisCancel { document: 1 }, Command::Cancel] {
        let again = inspect.ask(again)?;
        assert!(
            matches!(
                again,
                Event::Error {
                    code: ErrorCode::WrongState,
                    ..
                }
            ),
            "{again:?}"
        );
    }
    // An id is never reused: stale requests cannot reach a newer document.
    let reused = inspect.ask(Command::AnalysisOpen {
        document: 1,
        source: source.clone(),
    })?;
    assert_eq!(failure(&reused), Some(AnalysisFailure::InvalidRequest));
    let summary = inspect.open(2, &path)?;
    assert_eq!((summary.obus, summary.outcome), (33, ScanOutcome::Complete));
    assert!(matches!(
        inspect.ask(Command::AnalysisClose { document: 2 })?,
        Event::AnalysisClosed { document: 2 }
    ));
    for stale in [
        Command::AnalysisPage {
            document: 2,
            filter: RowFilter::default(),
            first: 0,
            count: 1,
        },
        Command::AnalysisBytes {
            document: 2,
            offset: 0,
            len: 1,
        },
        Command::AnalysisClose { document: 2 },
    ] {
        assert_eq!(
            failure(&inspect.ask(stale)?),
            Some(AnalysisFailure::UnknownDocument)
        );
    }
    Ok(())
}

#[test]
fn versions_foreign_commands_and_the_document_limit() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("td.obu");
    fs::write(&path, [0x01, 0x08])?;
    let mut inspect = Inspect::spawn()?;
    let hello = inspect.ask(Command::Hello {
        client: BuildIdentity {
            name: "test".into(),
            version: "0".into(),
        },
    })?;
    assert!(
        matches!(hello, Event::Hello { ref capabilities, .. } if capabilities.analysis_framings == [Framing::Ivf, Framing::AnnexB]),
        "{hello:?}"
    );
    let Event::Hello { capabilities, .. } = hello else {
        return Err("expected inspect capabilities".into());
    };
    let identity = capabilities
        .analysis_evidence
        .ok_or("expected evidence schema")?;
    assert_eq!(identity.syntax_schema, SYNTAX_SCHEMA);
    assert_eq!(identity.validation_schema, VALIDATION_SCHEMA);
    assert_eq!(identity.codec_revision, env!("SPLOT_CODEC_REVISION"));
    let body = br#"{"protocol_version":999,"request_id":77,"body":{"type":"analysis_close","document":1}}"#;
    let mut frame = u32::try_from(body.len())?.to_le_bytes().to_vec();
    frame.extend_from_slice(body);
    inspect.stdin.as_mut().ok_or("stdin")?.write_all(&frame)?;
    let mismatch = inspect.next()?;
    assert_eq!(mismatch.request_id, 77);
    assert!(matches!(
        mismatch.body,
        Event::Error {
            code: ErrorCode::IncompatibleVersion,
            ..
        }
    ));
    let probe = inspect.ask(Command::Probe {
        source: WirePath::from_path(&path),
        video_stream: None,
    })?;
    assert!(
        matches!(
            probe,
            Event::Error {
                code: ErrorCode::WrongState,
                ..
            }
        ),
        "{probe:?}"
    );
    for document in 1..=4 {
        inspect.open(document, &path)?;
    }
    let fifth = inspect.ask(Command::AnalysisOpen {
        document: 5,
        source: WirePath::from_path(&path),
    })?;
    assert_eq!(failure(&fifth), Some(AnalysisFailure::LimitReached));
    assert!(matches!(
        inspect.ask(Command::AnalysisClose { document: 1 })?,
        Event::AnalysisClosed { .. }
    ));
    inspect.open(6, &path)?;
    assert!(matches!(
        inspect.ask(Command::AnalysisClose { document: 6 })?,
        Event::AnalysisClosed { .. }
    ));
    let empty = dir.path().join("empty.ivf");
    fs::write(&empty, [])?;
    let refused = inspect.ask(Command::AnalysisOpen {
        document: 7,
        source: WirePath::from_path(&empty),
    })?;
    assert_eq!(failure(&refused), Some(AnalysisFailure::Unsupported));
    Ok(())
}

/// Cancelling one pending open stops it alone: an open queued before it,
/// still reading, completes.
#[test]
fn cancelling_one_open_leaves_another_alone() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("long.obu");
    long_stream(&path)?;
    let mut inspect = Inspect::spawn()?;
    let source = WirePath::from_path(&path);
    let ids = inspect.send(vec![
        Command::AnalysisOpen {
            document: 1,
            source: source.clone(),
        },
        Command::AnalysisOpen {
            document: 2,
            source,
        },
        Command::AnalysisCancel { document: 2 },
    ])?;
    let mut answers = std::collections::HashMap::new();
    for _ in 0..3 {
        let answer = inspect.next()?;
        answers.insert(answer.request_id, answer.body);
    }
    assert!(
        matches!(answers.get(&ids[0]), Some(Event::AnalysisOpened { document: 1, summary }) if summary.outcome == ScanOutcome::Complete),
        "{answers:?}"
    );
    let consistent = matches!(
        (answers.get(&ids[2]), answers.get(&ids[1])),
        (
            Some(Event::CancelRequested),
            Some(Event::AnalysisFailed {
                document: 2,
                code: AnalysisFailure::Cancelled,
                ..
            })
        ) | (
            Some(
                Event::CancelRequested
                    | Event::Error {
                        code: ErrorCode::WrongState,
                        ..
                    }
            ),
            Some(Event::AnalysisOpened { document: 2, .. })
        )
    );
    assert!(consistent, "{answers:?}");
    Ok(())
}

/// A request that finds the file changed closes the document.
#[test]
fn a_changed_source_closes_its_document() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("td.obu");
    fs::write(&path, [0x01, 0x08])?;
    let mut inspect = Inspect::spawn()?;
    inspect.open(1, &path)?;
    let validation = inspect.next_id;
    assert!(matches!(
        inspect.ask(Command::AnalysisValidate {
            document: 1,
            settings: ValidationSettings::default()
        })?,
        Event::AnalysisValidated { .. },
    ));
    let append = || {
        fs::OpenOptions::new()
            .append(true)
            .open(&path)?
            .write_all(&[0x01, 0x08])
    };
    let findings = || Command::AnalysisFindings {
        document: 1,
        validation,
        first: 0,
        count: 1,
    };
    #[cfg(windows)]
    {
        let error = append().err().ok_or("A verified reader allowed a write.")?;
        assert_eq!(error.raw_os_error(), Some(32)); // ERROR_SHARING_VIOLATION
        assert!(matches!(
            inspect.ask(findings())?,
            Event::AnalysisFindings { document: 1, .. },
        ));
        assert!(matches!(
            inspect.ask(Command::AnalysisClose { document: 1 })?,
            Event::AnalysisClosed { document: 1 },
        ));
        append()?;
    }
    #[cfg(not(windows))]
    {
        append()?;
        assert_eq!(
            failure(&inspect.ask(findings())?),
            Some(AnalysisFailure::SourceChanged)
        );
    }
    let page = Command::AnalysisPage {
        document: 1,
        filter: RowFilter::default(),
        first: 0,
        count: 1,
    };
    assert_eq!(
        failure(&inspect.ask(page)?),
        Some(AnalysisFailure::UnknownDocument)
    );
    let locate = Command::AnalysisLocate {
        document: 1,
        filter: RowFilter::default(),
        target: LocateTarget::Offset { offset: 0 },
    };
    assert_eq!(
        failure(&inspect.ask(locate)?),
        Some(AnalysisFailure::UnknownDocument)
    );
    let summary = inspect.open(2, &path)?;
    assert_eq!(summary.obus, 2);
    Ok(())
}

/// A cancel does not hide why an open failed: an open cancelled before it
/// started is answered cancelled, one that already failed (here: too many
/// documents) keeps its reason.
#[test]
fn a_cancel_does_not_hide_why_an_open_failed() -> TestResult {
    let dir = tempfile::tempdir()?;
    let small = dir.path().join("td.obu");
    fs::write(&small, [0x01, 0x08])?;
    let long = dir.path().join("long.obu");
    long_stream(&long)?;
    let mut inspect = Inspect::spawn()?;
    for document in 1..=3 {
        inspect.open(document, &small)?;
    }
    // The fourth document fills the limit while the fifth waits behind it.
    let ids = inspect.send(vec![
        Command::AnalysisOpen {
            document: 4,
            source: WirePath::from_path(&long),
        },
        Command::AnalysisOpen {
            document: 5,
            source: WirePath::from_path(&small),
        },
        Command::AnalysisCancel { document: 5 },
    ])?;
    let mut answers = std::collections::HashMap::new();
    for _ in 0..3 {
        let answer = inspect.next()?;
        answers.insert(answer.request_id, answer.body);
    }
    assert!(
        matches!(
            answers.get(&ids[0]),
            Some(Event::AnalysisOpened { document: 4, .. })
        ),
        "{answers:?}"
    );
    let consistent = matches!(
        (answers.get(&ids[2]), answers.get(&ids[1]).and_then(failure)),
        (
            Some(Event::CancelRequested),
            Some(AnalysisFailure::Cancelled)
        ) | (
            Some(
                Event::CancelRequested
                    | Event::Error {
                        code: ErrorCode::WrongState,
                        ..
                    }
            ),
            Some(AnalysisFailure::LimitReached)
        )
    );
    assert!(consistent, "{answers:?}");
    Ok(())
}

/// A document closed while it is read gives its slot back before a later
/// open, whether the cancel stopped the read or came too late: the close
/// waits behind the open in the queue.
#[test]
fn a_document_closed_while_read_frees_its_slot() -> TestResult {
    let dir = tempfile::tempdir()?;
    let small = dir.path().join("td.obu");
    fs::write(&small, [0x01, 0x08])?;
    let long = dir.path().join("long.obu");
    long_stream(&long)?;
    let mut inspect = Inspect::spawn()?;
    for document in 1..=3 {
        inspect.open(document, &small)?;
    }
    let ids = inspect.send(vec![
        Command::AnalysisOpen {
            document: 4,
            source: WirePath::from_path(&long),
        },
        Command::AnalysisClose { document: 4 },
        Command::AnalysisOpen {
            document: 5,
            source: WirePath::from_path(&small),
        },
    ])?;
    let mut answers = std::collections::HashMap::new();
    for _ in 0..3 {
        let answer = inspect.next()?;
        answers.insert(answer.request_id, answer.body);
    }
    assert!(
        matches!(
            answers.get(&ids[2]),
            Some(Event::AnalysisOpened { document: 5, .. })
        ),
        "{answers:?}"
    );
    Ok(())
}

#[test]
fn identity_pages_require_the_selected_scan_and_source() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("identity.ivf");
    fs::write(
        &path,
        include_bytes!("inspect/fixtures/syn-bridge-celu-64x64.ivf"),
    )?;
    let mut inspect = Inspect::spawn()?;
    inspect.open(1, &path)?;

    let scan = inspect.next_id;
    assert!(matches!(
        inspect.ask(Command::AnalysisIdentityScan {
            document: 1,
            grain: GrainPolicy::Skip,
            selection: studio_domain::layers::LayerDecodeSelection::Default,
        })?,
        Event::AnalysisIdentityScanned { document: 1, summary }
            if summary.completion == IdentityCompletion::Complete && summary.matches_structure
    ));
    let output = |scan| Command::AnalysisIdentityPage {
        document: 1,
        scan,
        first: 0,
        count: 1,
        kind: IdentityKind::Output,
    };
    assert!(matches!(
        inspect.ask(output(scan))?,
        Event::AnalysisIdentityPage { document: 1, scan: current, page }
            if current == scan && page.rows.len() == 1
    ));
    assert_eq!(
        failure(&inspect.ask(output(scan - 1))?),
        Some(AnalysisFailure::InvalidRequest)
    );
    let replace = || fs::write(&path, [0x01, 0x08]);
    #[cfg(windows)]
    {
        let error = replace()
            .err()
            .ok_or("A verified reader allowed a write.")?;
        assert_eq!(error.raw_os_error(), Some(32)); // ERROR_SHARING_VIOLATION
        assert!(matches!(
            inspect.ask(Command::AnalysisClose { document: 1 })?,
            Event::AnalysisClosed { document: 1 },
        ));
        replace()?;
    }
    #[cfg(not(windows))]
    {
        replace()?;
        assert_eq!(
            failure(&inspect.ask(output(scan))?),
            Some(AnalysisFailure::SourceChanged)
        );
    }
    assert_eq!(
        failure(&inspect.ask(output(scan))?),
        Some(AnalysisFailure::UnknownDocument)
    );
    let reopened = inspect.open(2, &path)?;
    assert_eq!(reopened.framing, Framing::AnnexB);
    assert_eq!(reopened.obus, 1);
    Ok(())
}

#[test]
fn timeline_output_links_match_native_pictures_after_backward_seek() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("timeline.ivf");
    fs::write(
        &path,
        include_bytes!("inspect/fixtures/syn-bridge-celu-64x64.ivf"),
    )?;
    let mut inspect = Inspect::spawn()?;
    let opened = inspect.open(1, &path)?;
    assert_eq!(opened.packets, 1);
    let scan = inspect.next_id;
    let scanned = inspect.ask(Command::AnalysisIdentityScan {
        document: 1,
        grain: GrainPolicy::Skip,
        selection: studio_domain::layers::LayerDecodeSelection::Default,
    })?;
    assert!(
        matches!(scanned, Event::AnalysisIdentityScanned { summary, .. }
        if summary.coded == 3 && summary.outputs == 2 && summary.matches_structure)
    );

    let coded = inspect.ask(Command::AnalysisTimeline {
        document: 1,
        scan,
        view: TimelineView::Coded,
        first: 0,
        limit: 3,
        buckets: 3,
    })?;
    let Event::AnalysisTimeline { page, .. } = coded else {
        return Err(format!("Expected coded timeline. {coded:?}").into());
    };
    assert!(page.verified);
    let hidden = page
        .buckets
        .iter()
        .filter_map(|bucket| bucket.entry)
        .find(|row| row.output_ordinal.is_none())
        .ok_or("Expected a hidden coded frame.")?;
    let selected = timeline_location(
        &mut inspect,
        scan,
        TimelineTarget::Coded {
            index: hidden.ordinal,
        },
    )?;
    assert_eq!(selected.coded_offset, hidden.coded_offset);
    assert_eq!(selected.output_ordinal, None);
    assert_eq!(
        timeline_obu(&mut inspect, selected.coded_offset)?.parent,
        Some(1)
    );

    let time_search = inspect.ask(Command::AnalysisTimelineLocate {
        document: 1,
        scan,
        target: TimelineTarget::Time {
            time: TimelineTime { num: 0, den: 1 },
        },
    })?;
    assert_eq!(failure(&time_search), Some(AnalysisFailure::InvalidRequest));

    let target = PictureTarget::Document {
        document: 1,
        path: WirePath::from_path(&path),
        fingerprint: opened.fingerprint,
        digest: opened.digest.ok_or("Expected source digest.")?,
        selection: studio_domain::layers::LayerDecodeSelection::Default,
    };
    let mut preview = Inspect::spawn_mode("preview")?;
    for (generation, index) in [(1, 1), (2, 0)] {
        let selection = timeline_location(&mut inspect, scan, TimelineTarget::Output { index })?;
        let showing = selection.showing_offset.ok_or("Expected a display OBU.")?;
        assert_eq!(timeline_obu(&mut inspect, showing)?.parent, Some(1));
        assert_eq!(
            timeline_obu(&mut inspect, selection.coded_offset)?.parent,
            Some(1)
        );
        let picture = timeline_picture(&mut preview, target.clone(), generation, index)?;
        let native = picture.output.ok_or("Expected native output evidence.")?;
        assert_eq!(selection.output_ordinal, Some(native.ordinal));
        assert_eq!(Some(selection.coded_offset), native.coded_offset);
        assert_eq!(selection.showing_offset, native.showing_offset);
    }
    Ok(())
}

fn timeline_obu(inspect: &mut Inspect, offset: u64) -> Result<Row, Box<dyn Error>> {
    let reply = inspect.ask(Command::AnalysisLocate {
        document: 1,
        filter: RowFilter::default(),
        target: LocateTarget::Offset { offset },
    })?;
    let Event::AnalysisLocated { found, .. } = reply else {
        return Err(format!("Expected a linked OBU. {reply:?}").into());
    };
    assert!(matches!(&found.element, Element::Obu { obu } if obu.obu.offset == offset));
    Ok(*found)
}

fn timeline_location(
    inspect: &mut Inspect,
    scan: u64,
    target: TimelineTarget,
) -> Result<TimelineSelection, Box<dyn Error>> {
    let reply = inspect.ask(Command::AnalysisTimelineLocate {
        document: 1,
        scan,
        target,
    })?;
    let Event::AnalysisTimelineLocated {
        selection,
        scan: answered,
        ..
    } = reply
    else {
        return Err(format!("Expected a timeline selection. {reply:?}").into());
    };
    assert_eq!(answered, scan);
    Ok(*selection)
}

fn timeline_picture(
    preview: &mut Inspect,
    target: PictureTarget,
    generation: u64,
    index: u64,
) -> Result<PictureHeader, Box<dyn Error>> {
    preview.send(vec![Command::Picture {
        generation,
        target,
        frame: PictureFrame::Index { index },
        view: PictureView::Fit,
        options: PictureOptions {
            grain: GrainPolicy::Skip,
            ..PictureOptions::default()
        },
        size: PictureBox {
            width: 64,
            height: 64,
        },
    }])?;
    loop {
        let (reply, pixels) =
            read_event(&mut preview.stdout)?.ok_or("The preview process stopped.")?;
        match reply.body {
            Event::Picture { picture } => {
                assert_eq!(picture.generation, generation);
                assert!(pixels.is_some());
                return Ok(picture);
            }
            Event::PictureProgress { .. } => {}
            other => return Err(format!("Expected a picture. {other:?}").into()),
        }
    }
}

#[test]
fn validation_and_foreground_pages_share_one_inspector() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("long.obu");
    long_stream(&path)?;
    let mut inspect = Inspect::spawn()?;
    let opened = inspect.open(1, &path)?;
    let [validation, page] = inspect.send(vec![
        Command::AnalysisValidate {
            document: 1,
            settings: ValidationSettings::default(),
        },
        Command::AnalysisPage {
            document: 1,
            filter: RowFilter::default(),
            first: 0,
            count: 1,
        },
    ])?[..] else {
        return Err("Expected two request IDs.".into());
    };

    let response = inspect.next()?;
    assert_eq!(response.request_id, page);
    assert!(matches!(
        response.body,
        Event::AnalysisPage { document: 1, total, rows, .. }
            if total == opened.elements && rows.len() == 1
    ));
    let response = inspect.next()?;
    assert_eq!(response.request_id, validation);
    assert!(matches!(
        response.body,
        Event::AnalysisValidated { document: 1, .. }
    ));
    Ok(())
}

#[test]
fn cancellation_targets_one_validation_request() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("long.obu");
    long_stream(&path)?;
    let mut inspect = Inspect::spawn()?;
    inspect.open(1, &path)?;
    let validate = |document| Command::AnalysisValidate {
        document,
        settings: ValidationSettings::default(),
    };
    let validation = inspect.next_id;
    let [request, wrong_generation, cancel, other] = inspect.send(vec![
        validate(1),
        Command::AnalysisCancelValidation {
            document: 2,
            validation,
        },
        Command::AnalysisCancelValidation {
            document: 1,
            validation,
        },
        validate(1),
    ])?[..] else {
        return Err("expected four request IDs".into());
    };
    let mut answers = Vec::new();
    for _ in 0..4 {
        answers.push(inspect.next()?);
    }
    let event = |id| {
        answers
            .iter()
            .find(|answer| answer.request_id == id)
            .map(|answer| &answer.body)
    };
    assert!(matches!(
        event(wrong_generation),
        Some(Event::Error {
            code: ErrorCode::WrongState,
            ..
        })
    ));
    let Some(Event::AnalysisValidated { summary, .. }) = event(request) else {
        return Err(format!("expected first validation report: {answers:?}").into());
    };
    match event(cancel) {
        Some(Event::CancelRequested) => {
            // If collection ended before the cancellation reached it, the completed
            // result can follow the acknowledgement.
            let response = answers
                .iter()
                .position(|answer| answer.request_id == request);
            let ack = answers
                .iter()
                .position(|answer| answer.request_id == cancel);
            assert!(ack < response);
        }
        Some(Event::Error {
            code: ErrorCode::WrongState,
            ..
        }) => {
            assert!(!matches!(
                summary.completion,
                ValidationCompletion::Cancelled
            ));
        }
        _ => return Err(format!("unexpected cancellation response: {answers:?}").into()),
    }
    assert!(matches!(
        event(other),
        Some(Event::AnalysisValidated { summary, .. })
          if !matches!(summary.completion, ValidationCompletion::Cancelled),
    ));
    Ok(())
}
