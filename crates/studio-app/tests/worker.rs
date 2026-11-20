// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The worker client against the real engine binary.
//!
//! The engine is looked up next to the app binary Cargo builds for this test
//! (`target/<profile>/splot-studio-engine`). It is not built by
//! `cargo test -p studio-app`; when it is missing these tests fail and say so
//! instead of skipping.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use studio_app::activity::{Activity, Input, Phase};
use studio_app::engine::{Delivery, ENGINE_NAME, Ending, EngineStatus, Worker};
use studio_domain::job::{
    ChromaLayout, FailureCode, JobSpec, Rational, SourceFacts, Stage, WirePath,
};
use studio_domain::protocol::{BuildIdentity, Command, Event};
use studio_domain::settings::{Crf, EncoderSettings, OutputKind, SpeedPreset};

/// Long enough for a debug-build encode of a tiny clip.
const JOB_TIMEOUT: Duration = Duration::from_secs(600);
const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);

fn engine() -> PathBuf {
    // Next to the app binary Cargo built for this test, as at run time.
    let path = Path::new(env!("CARGO_BIN_EXE_splot-studio"))
        .with_file_name(format!("{ENGINE_NAME}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.is_file(),
        "build studio-engine first (`cargo build -p studio-engine`, same profile): \
         no engine at {}",
        path.display()
    );
    path
}

/// A fresh, empty directory for one test.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "studio-app-worker-{}-{}-{name}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap_or_else(|e| unreachable!("{e}"));
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn entries(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.0).map_or_else(
            |_| Vec::new(),
            |dir| {
                dir.filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            },
        );
        names.sort();
        names
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Writes a progressive 4:2:0 Y4M at 30000/1001 fps, as ffmpeg labels it,
/// with a moving pattern. Ten-bit samples use all ten bits.
fn write_y4m(path: &Path, width: usize, height: usize, frames: usize, ten_bit: bool) {
    let colour = if ten_bit {
        "C420p10 XYSCSS=420P10"
    } else {
        "C420jpeg XYSCSS=420JPEG"
    };
    let mut bytes =
        format!("YUV4MPEG2 W{width} H{height} F30000:1001 Ip A1:1 {colour}\n").into_bytes();
    let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
    for f in 0..frames {
        bytes.extend_from_slice(b"FRAME\n");
        let planes = [(width, height, 0), (cw, ch, 300), (cw, ch, 700)];
        for (w, h, offset) in planes {
            for y in 0..h {
                for x in 0..w {
                    let v = (x * 7 + y * 3 + f * 5 + offset) % 1024;
                    if ten_bit {
                        bytes.extend_from_slice(&u16::try_from(v).unwrap_or(0).to_le_bytes());
                    } else {
                        bytes.push(u8::try_from(v % 256).unwrap_or(0));
                    }
                }
            }
        }
    }
    fs::write(path, bytes).unwrap_or_else(|e| unreachable!("{e}"));
}

fn spawn() -> (Worker, Receiver<Delivery>) {
    let (tx, rx) = mpsc::channel();
    let worker = Worker::spawn(&engine(), move |d| {
        let _ = tx.send(d);
    })
    .unwrap_or_else(|e| unreachable!("{e}"));
    (worker, rx)
}

fn next(rx: &Receiver<Delivery>, timeout: Duration) -> Delivery {
    rx.recv_timeout(timeout).unwrap_or_else(|e| {
        unreachable!(
            "no delivery within {timeout:?} ({e}); if the engine was built before \
             STUDIO-02, rebuild it with `cargo build -p studio-engine`"
        )
    })
}

fn event(rx: &Receiver<Delivery>, id: u64, timeout: Duration) -> Event {
    match next(rx, timeout) {
        Delivery::Event(e) => {
            assert_eq!(e.request_id, id, "{e:?}");
            e.body
        }
        Delivery::Ended(ending) => unreachable!("worker ended early: {ending:?}"),
    }
}

fn expect_clean_end(rx: &Receiver<Delivery>) -> Ending {
    match next(rx, ANSWER_TIMEOUT) {
        Delivery::Ended(ending) => {
            assert!(ending.error.is_none(), "{ending:?}");
            ending
        }
        Delivery::Event(e) => unreachable!("expected the end, got {e:?}"),
    }
}

const fn is_terminal(event: &Event) -> bool {
    matches!(
        event,
        Event::Completed { .. } | Event::Cancelled | Event::Failed { .. }
    )
}

fn probe(source: &Path) -> Event {
    let (mut worker, rx) = spawn();
    let sent = worker.send(
        7,
        Command::Probe {
            source: WirePath::from_path(source),
        },
    );
    assert!(sent.is_ok(), "{sent:?}");
    worker.close_input();
    let answer = event(&rx, 7, ANSWER_TIMEOUT);
    expect_clean_end(&rx);
    answer
}

fn settings(output: OutputKind) -> EncoderSettings {
    EncoderSettings {
        output,
        preset: SpeedPreset::new(SpeedPreset::MAX).unwrap_or_else(|e| unreachable!("{e}")),
        crf: Crf::new(40).unwrap_or_else(|e| unreachable!("{e}")),
    }
}

fn start(
    source: &Path,
    destination: &Path,
    settings: EncoderSettings,
) -> (Worker, Receiver<Delivery>) {
    let (mut worker, rx) = spawn();
    let job = JobSpec {
        source: WirePath::from_path(source),
        destination: WirePath::from_path(destination),
        settings,
    };
    let sent = worker.send(1, Command::Start { job });
    assert!(sent.is_ok(), "{sent:?}");
    (worker, rx)
}

/// Collects a job's events up to and including its terminal event.
fn run_to_end(rx: &Receiver<Delivery>, activity: &mut Activity) -> Vec<Event> {
    let deadline = Instant::now() + JOB_TIMEOUT;
    let mut events = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let e = event(rx, 1, left);
        activity.apply(Input::Event(e.clone()), Instant::now());
        let done = is_terminal(&e);
        events.push(e);
        if done {
            return events;
        }
    }
}

#[test]
fn hello_reports_what_the_engine_can_encode() {
    let (mut worker, rx) = spawn();
    let client = BuildIdentity {
        name: "studio-app-test".into(),
        version: "0".into(),
    };
    assert!(worker.send(0, Command::Hello { client }).is_ok());
    worker.close_input();
    let status = EngineStatus::from_hello(next(&rx, ANSWER_TIMEOUT));
    let EngineStatus::Ready { encode_outputs, .. } = &status else {
        unreachable!("{status:?}")
    };
    assert!(
        encode_outputs.contains(&OutputKind::Ivf)
            && encode_outputs.contains(&OutputKind::Av2AnnexB),
        "{status:?}"
    );
    expect_clean_end(&rx);
}

#[test]
fn probe_reads_the_header_at_both_depths() {
    let dir = Scratch::new("probe");
    for ten_bit in [false, true] {
        let source = dir.path(if ten_bit { "ten.y4m" } else { "eight.y4m" });
        write_y4m(&source, 64, 36, 3, ten_bit);
        let Event::Probed { facts } = probe(&source) else {
            unreachable!("probe of {} failed", source.display())
        };
        let SourceFacts {
            width,
            height,
            frame_rate,
            bit_depth,
            chroma,
            frame_count,
            ..
        } = facts;
        assert_eq!((width, height), (64, 36));
        assert_eq!(
            frame_rate,
            Rational {
                num: 30000,
                den: 1001
            }
        );
        assert_eq!(bit_depth, if ten_bit { 10 } else { 8 });
        assert_eq!(chroma, ChromaLayout::Yuv420);
        // Unknown is allowed; a wrong count is not.
        assert!(frame_count.is_none_or(|n| n == 3), "{frame_count:?}");
    }
    // A probe writes nothing.
    assert_eq!(dir.entries(), ["eight.y4m", "ten.y4m"]);
}

#[test]
fn probe_rejects_a_file_that_is_not_y4m() {
    let dir = Scratch::new("garbage");
    let source = dir.path("garbage.y4m");
    fs::write(&source, b"RIFF\0\0\0\0WAVEfmt ").unwrap_or_else(|e| unreachable!("{e}"));
    let answer = probe(&source);
    assert!(
        matches!(&answer, Event::Failed { failure } if failure.code == FailureCode::InvalidSource),
        "{answer:?}"
    );
    let missing = probe(&dir.path("missing.y4m"));
    assert!(matches!(missing, Event::Failed { .. }), "{missing:?}");
}

#[test]
fn encodes_both_depths_to_both_outputs_and_publishes_only_the_result() {
    for (ten_bit, output) in [(false, OutputKind::Ivf), (true, OutputKind::Av2AnnexB)] {
        let dir = Scratch::new("encode");
        let source = dir.path("clip.y4m");
        write_y4m(&source, 64, 36, 4, ten_bit);
        let destination = dir.path(&format!("clip.{}", output.extension()));
        let (mut worker, rx) = start(&source, &destination, settings(output));
        let mut activity = Activity::new(destination.clone(), Some(4), Instant::now());
        let events = run_to_end(&rx, &mut activity);

        let Phase::Completed(report) = activity.phase() else {
            unreachable!("{output:?}: {events:?}")
        };
        assert_eq!(report.destination, WirePath::from_path(&destination));
        assert_eq!(report.frames, 4);
        let len = fs::metadata(&destination).map_or(0, |m| m.len());
        assert!(len > 0);
        assert_eq!(report.bytes, len);
        assert_ne!(report.checks.len(), 0);
        // Stages arrive in order and before the result.
        let stages: Vec<Stage> = events
            .iter()
            .filter_map(|e| match e {
                Event::Stage { stage } => Some(*stage),
                _ => None,
            })
            .collect();
        assert!(stages.is_sorted_by_key(|s| *s as u8), "{stages:?}");
        assert_eq!(stages.first(), Some(&Stage::Encoding), "{stages:?}");
        // Counters never go backwards.
        let mut last = (0, 0, 0);
        for e in &events {
            if let Event::Progress { progress: p } = e {
                let now = (p.frames_read, p.frames_encoded, p.bytes_written);
                assert!(
                    now.0 >= last.0 && now.1 >= last.1 && now.2 >= last.2,
                    "{events:?}"
                );
                last = now;
            }
        }
        worker.close_input();
        expect_clean_end(&rx);
        // No temporary file is left next to the published one.
        assert_eq!(
            dir.entries(),
            // Sorted: both extensions sort before "y4m".
            [
                format!("clip.{}", output.extension()),
                "clip.y4m".to_owned()
            ]
        );
    }
}

#[test]
fn an_existing_destination_is_refused_and_left_alone() {
    let dir = Scratch::new("exists");
    let source = dir.path("clip.y4m");
    write_y4m(&source, 64, 36, 2, false);
    let destination = dir.path("clip.ivf");
    fs::write(&destination, b"precious").unwrap_or_else(|e| unreachable!("{e}"));
    let (mut worker, rx) = start(&source, &destination, settings(OutputKind::Ivf));
    let mut activity = Activity::new(destination.clone(), Some(2), Instant::now());
    let events = run_to_end(&rx, &mut activity);
    assert!(
        matches!(activity.phase(), Phase::Failed(f) if f.code == FailureCode::DestinationExists),
        "{events:?}"
    );
    assert_eq!(
        fs::read(&destination).ok().as_deref(),
        Some(&b"precious"[..])
    );
    worker.close_input();
    expect_clean_end(&rx);
    assert_eq!(dir.entries(), ["clip.ivf", "clip.y4m"]);
}

/// A job long enough that Cancel normally lands before it finishes.
fn long_job(dir: &Scratch) -> (PathBuf, PathBuf) {
    let source = dir.path("long.y4m");
    write_y4m(&source, 320, 180, 240, false);
    (source, dir.path("long.ivf"))
}

fn slow() -> EncoderSettings {
    EncoderSettings {
        preset: SpeedPreset::new(0).unwrap_or_else(|e| unreachable!("{e}")),
        ..settings(OutputKind::Ivf)
    }
}

/// Waits for the first stage or progress event, so the job is really running.
fn wait_until_running(rx: &Receiver<Delivery>) {
    loop {
        match event(rx, 1, JOB_TIMEOUT) {
            Event::Stage { .. } | Event::Progress { .. } => return,
            other => assert!(!is_terminal(&other), "ended before running: {other:?}"),
        }
    }
}

#[test]
fn cancel_is_acknowledged_then_ends_with_nothing_published() {
    let dir = Scratch::new("cancel");
    let (source, destination) = long_job(&dir);
    let (mut worker, rx) = start(&source, &destination, slow());
    let mut activity = Activity::new(destination.clone(), None, Instant::now());
    wait_until_running(&rx);
    activity.apply(Input::CancelPressed, Instant::now());
    assert!(worker.send(1, Command::Cancel).is_ok());
    let events = run_to_end(&rx, &mut activity);
    worker.close_input();
    expect_clean_end(&rx);
    match activity.phase() {
        Phase::Cancelled => {
            assert!(events.contains(&Event::CancelRequested), "{events:?}");
            assert!(!destination.exists());
            assert_eq!(dir.entries(), ["long.y4m"], "temporary file left behind");
        }
        // Cancel raced completion: the file is real, and that is the truth.
        Phase::Completed(_) => assert!(destination.exists()),
        other => unreachable!("{other:?}: {events:?}"),
    }
}

#[test]
fn closing_the_input_mid_job_cancels_it() {
    let dir = Scratch::new("eof");
    let (source, destination) = long_job(&dir);
    let (mut worker, rx) = start(&source, &destination, slow());
    let mut activity = Activity::new(destination.clone(), None, Instant::now());
    wait_until_running(&rx);
    worker.close_input();
    let events = run_to_end(&rx, &mut activity);
    expect_clean_end(&rx);
    match activity.phase() {
        Phase::Cancelled => {
            assert!(!destination.exists());
            assert_eq!(dir.entries(), ["long.y4m"], "temporary file left behind");
        }
        Phase::Completed(_) => assert!(destination.exists()),
        other => unreachable!("{other:?}: {events:?}"),
    }
}

#[test]
fn shutdown_mid_job_stops_and_reaps_the_worker() {
    let dir = Scratch::new("shutdown");
    let (source, destination) = long_job(&dir);
    let (worker, rx) = start(&source, &destination, slow());
    wait_until_running(&rx);
    let started = Instant::now();
    worker.shutdown(Duration::from_secs(10));
    assert!(started.elapsed() < Duration::from_secs(15));
    // Whatever arrives, the stream ends: the process was reaped.
    let deadline = Instant::now() + ANSWER_TIMEOUT;
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Delivery::Event(_)) => {}
            Ok(Delivery::Ended(_)) => break,
            Err(e) => unreachable!("worker not reaped: {e}"),
        }
    }
    assert!(!destination.exists() || dir.entries().len() == 2);
}

#[test]
fn a_killed_worker_ends_without_a_terminal_event() {
    let dir = Scratch::new("kill");
    let (source, destination) = long_job(&dir);
    let (worker, rx) = start(&source, &destination, slow());
    let mut activity = Activity::new(destination.clone(), None, Instant::now());
    wait_until_running(&rx);
    worker.kill();
    let deadline = Instant::now() + ANSWER_TIMEOUT;
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Delivery::Event(e)) => {
                assert!(!is_terminal(&e.body), "a killed worker sent {e:?}");
                activity.apply(Input::Event(e.body), Instant::now());
            }
            Ok(Delivery::Ended(ending)) => {
                assert!(ending.exit.contains("signal"), "{ending:?}");
                activity.apply(
                    Input::WorkerEnded {
                        detail: ending.exit,
                    },
                    Instant::now(),
                );
                break;
            }
            Err(e) => unreachable!("worker not reaped: {e}"),
        }
    }
    assert!(matches!(activity.phase(), Phase::Lost(_)));
    assert!(!destination.exists());
}
