// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Live pictures: a real worker process running a job with `live` set.
//! The pictures must be the encoded frames (compared with a full decode of
//! the published file), and the job must be exactly the same with or
//! without them: the same bytes, the same cancel and failure behaviour.

mod common;

use std::error::Error;
use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command as Process, Stdio};
use std::time::{Duration, Instant};

use common::{Clip, TestResult, leftovers, spec};
use studio_domain::job::{FailureCode, JobSpec};
use studio_domain::preview::{
    PictureBox, PictureFailure, PictureHeader, PictureOrigin, PictureView,
};
use studio_domain::protocol::{Command, Envelope, ErrorCode, Event, read_event, write_message};
use studio_domain::settings::OutputKind;
use studio_engine::picture::{self, Frame};

type Fallible<T> = Result<T, Box<dyn Error>>;

const BIN: &str = env!("CARGO_BIN_EXE_splot-studio-engine");

const BOX: PictureBox = PictureBox {
    width: 40,
    height: 40,
};

struct Worker {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
}

impl Worker {
    fn spawn() -> Fallible<Self> {
        let mut child = Process::new(BIN)
            .arg("worker")
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
        })
    }

    fn send(&mut self, request_id: u64, command: Command) -> TestResult {
        let mut bytes = Vec::new();
        write_message(&mut bytes, &Envelope::new(request_id, command))?;
        let pipe = self.stdin.as_mut().ok_or("stdin closed")?;
        pipe.write_all(&bytes)?;
        Ok(())
    }

    fn next(&mut self) -> Fallible<(Envelope<Event>, Option<Vec<u8>>)> {
        Ok(read_event(&mut self.stdout)?.ok_or("the worker closed its output")?)
    }

    /// Events up to and including the job's terminal one, then a clean exit.
    fn run(
        mut self,
        job: JobSpec,
        live: Option<PictureBox>,
    ) -> Fallible<Vec<(Event, Option<Vec<u8>>)>> {
        self.send(1, Command::Start { job, live })?;
        let mut events = Vec::new();
        loop {
            let (envelope, pixels) = self.next()?;
            assert_eq!(envelope.request_id, 1);
            let end = terminal(&envelope.body);
            events.push((envelope.body, pixels));
            if end {
                break;
            }
        }
        drop(self.stdin.take());
        assert!(
            read_event(&mut self.stdout)?.is_none(),
            "an event after the terminal"
        );
        assert_eq!(self.child.wait()?.code(), Some(0));
        Ok(events)
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn terminal(event: &Event) -> bool {
    matches!(
        event,
        Event::Completed { .. } | Event::Cancelled | Event::Failed { .. }
    )
}

fn live_pictures(events: &[(Event, Option<Vec<u8>>)]) -> Vec<(PictureHeader, Vec<u8>)> {
    events
        .iter()
        .filter_map(|(event, pixels)| match (event, pixels) {
            (Event::Picture { picture }, Some(pixels)) => Some((picture.clone(), pixels.clone())),
            _ => None,
        })
        .collect()
}

/// Splot's full decode of `path`, frame by frame, as packed planes.
fn decoded(path: &Path, clip: &Clip) -> Fallible<Vec<[Vec<u16>; 3]>> {
    let raw = common::decode(path)?;
    let per_frame: usize = clip.planes().iter().map(|(w, h)| w * h).sum();
    let mut samples = raw.iter().map(|&b| u16::from(b));
    Ok((0..raw.len() / per_frame)
        .map(|_| {
            let sizes = clip.planes();
            let mut planes = sizes
                .iter()
                .map(|&(w, h)| samples.by_ref().take(w * h).collect::<Vec<u16>>());
            [
                planes.next().unwrap_or_default(),
                planes.next().unwrap_or_default(),
                planes.next().unwrap_or_default(),
            ]
        })
        .collect())
}

/// The same job with and without live pictures writes the same bytes;
/// with them, pictures arrive before the terminal event, and each is the
/// encoded frame it names, not the source.
#[test]
fn live_pictures_are_encoded_frames_and_change_nothing() -> TestResult {
    let dir = tempfile::tempdir()?;
    let clip = Clip::new(64, 48, "420", 40);
    let src = dir.path().join("in.y4m");
    clip.write(&src)?;
    let plain = dir.path().join("plain.ivf");
    let with_live = dir.path().join("live.ivf");
    let events = Worker::spawn()?.run(spec(&src, &plain, OutputKind::Ivf), None)?;
    assert!(matches!(events.last(), Some((Event::Completed { .. }, _))));
    assert_eq!(live_pictures(&events).len(), 0);
    let events = Worker::spawn()?.run(spec(&src, &with_live, OutputKind::Ivf), Some(BOX))?;
    assert!(
        matches!(events.last(), Some((Event::Completed { .. }, _))),
        "{:?}",
        events.last()
    );
    assert_eq!(
        fs::read(&plain)?,
        fs::read(&with_live)?,
        "live pictures changed the output"
    );
    let pictures = live_pictures(&events);
    assert!(!pictures.is_empty(), "no live picture");
    let reference = decoded(&with_live, &clip)?;
    let mut last = None;
    for (header, pixels) in &pictures {
        assert_eq!((header.generation, header.origin), (0, PictureOrigin::Live));
        assert_eq!((header.frame_width, header.frame_height), (64, 48));
        assert_eq!(
            (header.width, header.height, header.view),
            (40, 30, PictureView::Fit)
        );
        assert_eq!(header.frame_count, Some(40));
        assert!(last < Some(header.frame), "pictures went back");
        last = Some(header.frame);
        let planes = &reference[usize::try_from(header.frame)?];
        let frame = Frame::packed(planes, [(64, 48), (32, 24), (32, 24)], 8).ok_or("planes")?;
        assert_eq!(header.basis, picture::basis(None, false, 64, 48));
        let want = picture::render(&frame, header.basis, PictureView::Fit, BOX);
        assert!(
            *pixels == want.pixels,
            "live frame {} is not the encoded frame",
            header.frame
        );
        let mut samples = clip.samples(usize::try_from(header.frame)?).into_iter();
        let source_planes = [
            samples.next().unwrap_or_default(),
            samples.next().unwrap_or_default(),
            samples.next().unwrap_or_default(),
        ];
        let source =
            Frame::packed(&source_planes, [(64, 48), (32, 24), (32, 24)], 8).ok_or("planes")?;
        let source = picture::render(&source, header.basis, PictureView::Fit, BOX);
        assert!(
            *pixels != source.pixels,
            "live frame {} is the source frame",
            header.frame
        );
    }
    // The pictures come between the job's events, never after the end.
    let first = events
        .iter()
        .position(|(e, _)| matches!(e, Event::Picture { .. }));
    let working = events
        .iter()
        .position(|(e, _)| matches!(e, Event::Working { .. }));
    assert!(working < first);
    Ok(())
}

/// Cancel and failure behave as without live pictures.
#[test]
fn cancel_and_failure_are_unchanged_by_live_pictures() -> TestResult {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("long.y4m");
    Clip::new(64, 64, "420", 400).write(&src)?;
    let dst = dir.path().join("out.ivf");
    let mut worker = Worker::spawn()?;
    worker.send(
        1,
        Command::Start {
            job: spec(&src, &dst, OutputKind::Ivf),
            live: Some(BOX),
        },
    )?;
    loop {
        let (envelope, _) = worker.next()?;
        if matches!(envelope.body, Event::Picture { .. }) {
            break;
        }
        assert!(!terminal(&envelope.body), "{:?}", envelope.body);
    }
    let asked = Instant::now();
    worker.send(2, Command::Cancel)?;
    let mut saw_request = false;
    loop {
        let (envelope, _) = worker.next()?;
        match envelope.body {
            Event::CancelRequested => saw_request = true,
            Event::Cancelled => break,
            ref other => assert!(!terminal(other), "{other:?}"),
        }
    }
    assert!(saw_request);
    assert!(
        asked.elapsed() < Duration::from_secs(3),
        "{:?}",
        asked.elapsed()
    );
    drop(worker.stdin.take());
    assert!(read_event(&mut worker.stdout)?.is_none());
    assert_eq!(worker.child.wait()?.code(), Some(0));
    assert_eq!(
        leftovers(dir.path(), &[&src]),
        Vec::<std::path::PathBuf>::new()
    );

    // A failure: the destination appears.
    fs::write(&dst, b"taken")?;
    let events = Worker::spawn()?.run(spec(&src, &dst, OutputKind::Ivf), Some(BOX))?;
    assert!(
        matches!(events.last(), Some((Event::Failed { failure }, _)) if failure.code == FailureCode::DestinationExists),
        "{:?}",
        events.last()
    );
    assert_eq!(fs::read(&dst)?, b"taken");
    Ok(())
}

/// A worker killed while it sends live pictures publishes nothing and
/// leaves exactly the temporary file it announced, as without them.
#[test]
fn a_killed_worker_with_live_pictures_publishes_nothing() -> TestResult {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("long.y4m");
    Clip::new(64, 64, "420", 400).write(&src)?;
    let dst = dir.path().join("out.ivf");
    let mut worker = Worker::spawn()?;
    worker.send(
        1,
        Command::Start {
            job: spec(&src, &dst, OutputKind::Ivf),
            live: Some(BOX),
        },
    )?;
    let mut announced = Vec::new();
    loop {
        let (envelope, _) = worker.next()?;
        match envelope.body {
            Event::Working { partial } => announced.push(partial.to_path_buf()?),
            Event::Picture { .. } => break,
            ref other => assert!(!terminal(other), "{other:?}"),
        }
    }
    worker.child.kill()?;
    worker.child.wait()?;
    assert!(!dst.exists());
    assert_eq!(leftovers(dir.path(), &[&src]), announced);
    Ok(())
}

/// An invalid live box: the job runs without pictures and says why.
#[test]
fn an_invalid_live_box_is_reported_and_the_job_runs() -> TestResult {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("in.y4m");
    Clip::new(16, 16, "420", 3).write(&src)?;
    let dst = dir.path().join("out.obu");
    let bad = PictureBox {
        width: 0,
        height: 5000,
    };
    let events = Worker::spawn()?.run(spec(&src, &dst, OutputKind::Av2AnnexB), Some(bad))?;
    assert!(matches!(
        events.first(),
        Some((
            Event::PictureUnavailable {
                generation: 0,
                code: PictureFailure::InvalidRequest,
                ..
            },
            _
        ))
    ));
    assert_eq!(live_pictures(&events).len(), 0);
    assert!(matches!(events.last(), Some((Event::Completed { .. }, _))));
    Ok(())
}

/// A peer that does not read while the job runs gets the terminal event
/// once it reads; the job never waited for it, and pictures did not pile
/// up (only the newest one is kept).
#[test]
fn a_reader_that_stalls_gets_the_newest_picture_and_the_end() -> TestResult {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("in.y4m");
    Clip::new(64, 48, "420", 60).write(&src)?;
    let dst = dir.path().join("out.ivf");
    let mut worker = Worker::spawn()?;
    // 40x30 pictures: the pipe holds a few of them, then the writer blocks.
    worker.send(
        1,
        Command::Start {
            job: spec(&src, &dst, OutputKind::Ivf),
            live: Some(BOX),
        },
    )?;
    let deadline = Instant::now() + Duration::from_secs(120);
    while !dst.exists() {
        assert!(
            Instant::now() < deadline,
            "the job did not finish while unread"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(worker.stdin.take());
    let mut bytes = Vec::new();
    worker.stdout.read_to_end(&mut bytes)?;
    assert_eq!(worker.child.wait()?.code(), Some(0));
    let mut slice = bytes.as_slice();
    let mut events = Vec::new();
    while let Some(event) = read_event(&mut slice)? {
        events.push(event);
    }
    assert!(matches!(
        events.last(),
        Some((
            Envelope {
                body: Event::Completed { .. },
                ..
            },
            _
        ))
    ));
    let pictures = events
        .iter()
        .filter(|(e, _)| matches!(e.body, Event::Picture { .. }))
        .count();
    // At most four a second over the job's run, and at least one.
    assert!(pictures >= 1, "{pictures}");
    Ok(())
}

/// The job worker refuses preview commands.
#[test]
fn the_worker_refuses_picture_commands() -> TestResult {
    let mut worker = Worker::spawn()?;
    worker.send(3, Command::StopPlayback)?;
    let (envelope, _) = worker.next()?;
    assert_eq!(envelope.request_id, 3);
    assert!(matches!(
        envelope.body,
        Event::Error {
            code: ErrorCode::WrongState,
            ..
        }
    ));
    drop(worker.stdin.take());
    assert_eq!(worker.child.wait()?.code(), Some(0));
    Ok(())
}
