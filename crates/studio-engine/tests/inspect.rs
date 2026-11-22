// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Inspect process tests: the real engine binary in `inspect` mode, reading
//! IVF and Annex B files that the job path encodes from generated Y4M.

mod common;

use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command as Process, Stdio};

use common::{Clip, TestResult, run_job, spec};
use studio_domain::analysis::{
    AnalysisFailure, DocumentSummary, Element, Framing, KindFilter, LocateTarget,
    MAX_PAGE_ELEMENTS, ObuFact, RowFilter, ScanOutcome, Span,
};
use studio_domain::job::WirePath;
use studio_domain::protocol::{
    BuildIdentity, Command, Envelope, ErrorCode, Event, read_message, write_message,
};
use studio_domain::settings::OutputKind;

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
        let mut child = Process::new(BIN)
            .arg("inspect")
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

        locate_and_filter(&mut inspect, ivf_doc, &ivf_summary, &ivf_elements, frames)?;
    }
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
    let cancelled = match (&cancel, &open) {
        (
            Event::CancelRequested,
            Event::AnalysisFailed {
                code: AnalysisFailure::Cancelled,
                ..
            },
        ) => true,
        (
            Event::Error {
                code: ErrorCode::WrongState,
                ..
            },
            Event::AnalysisOpened { .. },
        ) => false,
        other => return Err(format!("inconsistent: {other:?}").into()),
    };
    let page = inspect.ask(Command::AnalysisPage {
        document: 1,
        filter: RowFilter::default(),
        first: 0,
        count: 1,
    })?;
    if cancelled {
        assert_eq!(failure(&page), Some(AnalysisFailure::UnknownDocument));
    } else {
        assert!(matches!(page, Event::AnalysisPage { .. }), "{page:?}");
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
            Some(Event::Error {
                code: ErrorCode::WrongState,
                ..
            }),
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
    fs::OpenOptions::new()
        .append(true)
        .open(&path)?
        .write_all(&[0x01, 0x08])?;
    let page = || Command::AnalysisPage {
        document: 1,
        filter: RowFilter::default(),
        first: 0,
        count: 1,
    };
    assert_eq!(
        failure(&inspect.ask(page())?),
        Some(AnalysisFailure::SourceChanged)
    );
    assert_eq!(
        failure(&inspect.ask(page())?),
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

/// After `CancelRequested`, the open is answered as cancelled even when it
/// would have failed for another reason (here: too many documents).
#[test]
fn a_cancelled_open_is_answered_cancelled_whatever_else_happened() -> TestResult {
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
            Some(Event::Error {
                code: ErrorCode::WrongState,
                ..
            }),
            Some(AnalysisFailure::LimitReached)
        )
    );
    assert!(consistent, "{answers:?}");
    Ok(())
}
