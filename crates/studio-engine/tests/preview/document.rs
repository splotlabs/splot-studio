// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Playback from independently opened AV2 documents.

use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use studio_domain::{
    job::WirePath,
    preview::{
        PictureBox, PictureFailure, PictureFrame, PictureHeader, PictureOrigin, PictureTarget,
    },
    protocol::{Command, Envelope, Event, read_event, write_message},
};
use studio_engine::inspect::document::Document;

use super::{BIG, Fallible, NATIVE, Preview, TestResult, play, rendered};

const I420: [(usize, usize); 3] = [(64, 64), (32, 32), (32, 32)];
const FRAME_SAMPLES: usize = 64 * 64 + 2 * 32 * 32;
const FRAME_BOX: PictureBox = PictureBox {
    width: 64,
    height: 64,
};

struct Fixture {
    name: &'static str,
    input: &'static [u8],
    avm: &'static [u8],
    depth: u8,
    outputs: u64,
}

macro_rules! fixture {
    ($name:literal, $depth:literal, $outputs:literal) => {
        Fixture {
            name: $name,
            input: include_bytes!(concat!("fixtures/", $name)),
            avm: include_bytes!(concat!("fixtures/", $name, ".yuv")),
            depth: $depth,
            outputs: $outputs,
        }
    };
}

const IVF_8: Fixture = fixture!("syn-frame-sef-families-64x64.ivf", 8, 4);
const IVF_10: Fixture = fixture!("syn-tlayer2-dependent-10bit.ivf", 10, 4);
const ANNEX_B_8: Fixture = fixture!("syn-mlayer2-derived-sef-8bit.obu", 8, 4);
const ANNEX_B_10: Fixture = fixture!("syn-mlayer2-crossref-10bit.obu", 10, 6);

impl Fixture {
    fn write(&self, dir: &Path, document: u64) -> Fallible<PictureTarget> {
        let path = dir.join(self.name);
        fs::write(&path, self.input)?;
        target(&path, document)
    }

    fn rgb(&self, ordinal: u64) -> Fallible<Vec<u8>> {
        let bytes_per_sample = if self.depth == 8 { 1 } else { 2 };
        let bytes_per_frame = FRAME_SAMPLES * bytes_per_sample;
        assert_eq!(
            self.avm.len(),
            bytes_per_frame * usize::try_from(self.outputs)?
        );
        let start = usize::try_from(ordinal)? * bytes_per_frame;
        let mut bytes = self.avm[start..start + bytes_per_frame].chunks_exact(bytes_per_sample);
        let planes = I420.map(|(w, h)| {
            bytes
                .by_ref()
                .take(w * h)
                .map(|sample| match sample {
                    [lo, hi] => u16::from_le_bytes([*lo, *hi]),
                    _ => u16::from(sample[0]),
                })
                .collect()
        });
        rendered(&planes, I420, self.depth, NATIVE, BIG)
    }
}

fn target(path: &Path, document: u64) -> Fallible<PictureTarget> {
    let path = WirePath::from_path(path);
    let inspected =
        Document::open(&path, &|| false).map_err(|e| format!("{:?}: {}", e.code, e.message))?;
    let summary = inspected.summary();
    Ok(PictureTarget::Document {
        document,
        path,
        fingerprint: summary.fingerprint,
        digest: summary
            .digest
            .clone()
            .ok_or("the inspection has no digest")?,
    })
}

fn picture(generation: u64, target: &PictureTarget, index: u64) -> Command {
    Command::Picture {
        generation,
        target: target.clone(),
        frame: PictureFrame::Index { index },
        view: NATIVE,
        size: BIG,
    }
}

fn check_picture(
    header: &PictureHeader,
    rgb: &[u8],
    fixture: &Fixture,
    ordinal: u64,
) -> TestResult {
    assert_eq!(header.origin, PictureOrigin::Document);
    assert_eq!(header.frame, ordinal);
    assert_eq!(header.output.ok_or("no output identity")?.ordinal, ordinal);
    assert_eq!((header.width, header.height), (64, 64));
    assert_eq!(
        rgb,
        fixture.rgb(ordinal)?,
        "{} output {ordinal}",
        fixture.name
    );
    if let Some(count) = header.frame_count {
        assert_eq!(count, fixture.outputs, "{}", fixture.name);
    }
    Ok(())
}

#[test]
fn linear_and_random_document_outputs_match_avm() -> TestResult {
    let dir = tempfile::tempdir()?;
    let mut preview = Preview::spawn()?;
    let mut generation = 0;
    for (document, fixture) in (1..).zip([IVF_8, IVF_10, ANNEX_B_8, ANNEX_B_10]) {
        let target = fixture.write(dir.path(), document)?;
        let mut linear = Vec::new();
        for ordinal in 0..fixture.outputs {
            generation += 1;
            let (header, rgb) = preview.frame(generation, &target, ordinal)?;
            check_picture(&header, &rgb, &fixture, ordinal)?;
            linear.push((header.output, header.time));
        }
        generation += 1;
        assert_eq!(
            preview.failure(generation, &target, fixture.outputs)?,
            PictureFailure::OutOfRange
        );
        for ordinal in [fixture.outputs - 1, 0, 2, 1, fixture.outputs - 1] {
            generation += 1;
            // A different box bypasses the rendered cache at the same pixel size.
            let (header, rgb) = preview
                .ask(
                    generation,
                    &target,
                    PictureFrame::Index { index: ordinal },
                    NATIVE,
                    FRAME_BOX,
                )?
                .map_err(|code| format!("{} output {ordinal}: {code:?}", fixture.name))?;
            check_picture(&header, &rgb, &fixture, ordinal)?;
            assert_eq!(header.frame_count, Some(fixture.outputs));
            assert_eq!(
                (header.output, header.time),
                linear[usize::try_from(ordinal)?]
            );
        }
    }
    preview.finish()
}

#[test]
fn derived_show_existing_keeps_the_coded_and_showing_bytes() -> TestResult {
    let dir = tempfile::tempdir()?;
    let target = ANNEX_B_8.write(dir.path(), 1)?;
    let mut preview = Preview::spawn()?;
    for (ordinal, coded, showing) in [(2, 44, 1324), (3, 787, 1328)] {
        let (header, rgb) = preview.frame(ordinal, &target, ordinal)?;
        check_picture(&header, &rgb, &ANNEX_B_8, ordinal)?;
        let output = header.output.ok_or("no output identity")?;
        assert_eq!((output.unit_offset, output.unit_len), (783, 548));
        assert_eq!(output.coded_offset, Some(coded));
        assert_eq!(output.showing_offset, Some(showing));
        assert_eq!(output.sequence_offset, Some(18));
        assert_eq!(
            (
                output.extended_layer,
                output.embedded_layer,
                output.temporal_layer
            ),
            (0, 0, 0)
        );
        assert_eq!(header.time, None);
        assert_eq!(output.interval, None);
    }
    preview.finish()
}

#[test]
fn document_timing_uses_ivf_ticks_and_keeps_invalid_timing_unknown() -> TestResult {
    let dir = tempfile::tempdir()?;
    let timed = IVF_10.write(dir.path(), 1)?;
    let mut preview = Preview::spawn()?;
    let (header, rgb) = preview.frame(1, &timed, 3)?;
    check_picture(&header, &rgb, &IVF_10, 3)?;
    let time = header.time.ok_or("the IVF packet declares a timestamp")?;
    assert_eq!(time.num * 30, time.den * 3);

    let path = dir.path().join("invalid-time-base.ivf");
    let mut bytes = IVF_10.input.to_vec();
    bytes[16..20].fill(0);
    fs::write(&path, bytes)?;
    let (header, rgb) = preview.frame(2, &target(&path, 2)?, 3)?;
    check_picture(&header, &rgb, &IVF_10, 3)?;
    assert_eq!(header.time, None);
    assert_eq!(header.output.ok_or("no output identity")?.interval, None);
    preview.finish()
}

#[test]
fn document_close_and_replacement_supersede_playback() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("document.ivf");
    let mut bytes = IVF_10.input.to_vec();
    // A long interval keeps playback waiting when the next command arrives.
    bytes[20..24].copy_from_slice(&1200_u32.to_le_bytes());
    fs::write(&path, bytes)?;
    let old = target(&path, 1)?;
    let mut preview = Preview::spawn()?;
    preview.send(1, play(1, &old, 0))?;
    let (first, _) = preview.picture_answer(1)?;
    let Event::Picture { picture: first } = first.body else {
        return Err(format!("unexpected first answer {:?}", first.body).into());
    };
    assert_eq!((first.generation, first.frame), (1, 0));
    preview.send(2, Command::StopPlayback)?;
    assert_eq!(
        preview.picture_answer(1)?.0.body,
        Event::PlaybackEnded {
            generation: 1,
            frame: Some(0)
        }
    );

    preview.send(3, Command::CloseDocumentPlayback { document: 1 })?;
    fs::write(&path, ANNEX_B_8.input)?;
    let new = target(&path, 2)?;
    let (header, rgb) = preview.frame(4, &new, 3)?;
    check_picture(&header, &rgb, &ANNEX_B_8, 3)?;
    assert_eq!(preview.failure(5, &old, 0)?, PictureFailure::SourceChanged);
    preview.finish()
}

struct BlockPixels {
    pipe: io::PipeWriter,
    gate: Option<(Sender<()>, Receiver<()>)>,
}

impl Write for BlockPixels {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() == 64 * 64 * 3
            && let Some((entered, resume)) = self.gate.take()
        {
            entered
                .send(())
                .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e))?;
            resume
                .recv_timeout(Duration::from_secs(10))
                .map_err(|e| io::Error::new(io::ErrorKind::TimedOut, e))?;
        }
        self.pipe.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.pipe.flush()
    }
}

struct ObserveInput {
    pipe: io::PipeReader,
    remaining: usize,
    arrived: Option<Sender<()>>,
}

impl Read for ObserveInput {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let count = self.pipe.read(bytes)?;
        self.remaining = self.remaining.saturating_sub(count);
        if self.remaining == 0
            && let Some(arrived) = self.arrived.take()
        {
            let _ = arrived.send(());
        }
        Ok(count)
    }
}

#[test]
fn scrubbing_reaches_the_newest_document_output_with_a_blocked_writer() -> TestResult {
    let dir = tempfile::tempdir()?;
    let target = ANNEX_B_10.write(dir.path(), 1)?;
    let mut first = Vec::new();
    write_message(&mut first, &Envelope::new(1, picture(1, &target, 0)))?;
    let mut seeks = Vec::new();
    for generation in 2..=40 {
        let command = picture(generation, &target, generation % ANNEX_B_10.outputs);
        write_message(&mut seeks, &Envelope::new(generation, command))?;
    }
    let input_len = first.len() + seeks.len();
    thread::scope(|scope| -> TestResult {
        let (input, mut requests) = io::pipe()?;
        let (mut events, output) = io::pipe()?;
        let (entered, blocked) = mpsc::channel();
        let (resume, released) = mpsc::channel();
        let (arrived, received) = mpsc::channel();
        let worker = scope.spawn(move || {
            studio_engine::preview::run(
                &mut ObserveInput {
                    pipe: input,
                    remaining: input_len,
                    arrived: Some(arrived),
                },
                BlockPixels {
                    pipe: output,
                    gate: Some((entered, released)),
                },
            )
        });
        requests.write_all(&first)?;
        blocked.recv_timeout(Duration::from_secs(10))?;
        requests.write_all(&seeks)?;
        received.recv_timeout(Duration::from_secs(10))?;
        resume.send(())?;
        let mut previous = 0;
        loop {
            let (event, rgb) = read_event(&mut events)?.ok_or("preview closed its output")?;
            match event.body {
                Event::PictureProgress { .. } => {}
                Event::Picture { picture } => {
                    assert!(event.request_id > previous, "{picture:?}");
                    assert_eq!(picture.generation, event.request_id);
                    previous = event.request_id;
                    let ordinal = if previous == 1 {
                        0
                    } else {
                        previous % ANNEX_B_10.outputs
                    };
                    check_picture(&picture, &rgb.ok_or("no pixels")?, &ANNEX_B_10, ordinal)?;
                    if previous == 40 {
                        break;
                    }
                }
                other => return Err(format!("unexpected answer {other:?}").into()),
            }
        }
        drop(requests);
        worker.join().map_err(|_| "preview thread panicked")??;
        Ok(())
    })
}
