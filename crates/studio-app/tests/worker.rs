// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The worker client against the real engine binary.
//!
//! The engine is looked up next to the app binary Cargo builds for this test
//! (`target/<profile>/splot-studio-engine`). It is not built by
//! `cargo test -p studio-app`; when it is missing these tests fail and say so
//! instead of skipping.
//!
//! Ordinary media files are generated with the `ffmpeg` on `PATH`, which the
//! engine also uses here (`SPLOT_STUDIO_SYSTEM_FFMPEG=1` from
//! `.cargo/config.toml`). Without it those tests fail and say so.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use studio_app::activity::{Activity, Input, Phase};
use studio_app::engine::{Delivery, ENGINE_NAME, Ending, EngineStatus, Role, Worker};
use studio_domain::job::{
    ChromaLayout, FailureCode, JobSpec, Rational, SourceFacts, SourceReader, Stage, WirePath,
};
use studio_domain::protocol::{BuildIdentity, Command, Event, MediaToolOrigin};
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
    let worker = Worker::spawn(&engine(), Role::Worker, move |d| {
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
        Delivery::Pictures(_) => unreachable!("no live pictures were asked for"),
    }
}

fn expect_clean_end(rx: &Receiver<Delivery>) -> Ending {
    match next(rx, ANSWER_TIMEOUT) {
        Delivery::Ended(ending) => {
            assert!(ending.error.is_none(), "{ending:?}");
            ending
        }
        Delivery::Event(e) => unreachable!("expected the end, got {e:?}"),
        Delivery::Pictures(_) => unreachable!("no live pictures were asked for"),
    }
}

const fn is_terminal(event: &Event) -> bool {
    matches!(
        event,
        Event::Completed { .. } | Event::Cancelled | Event::Failed { .. }
    )
}

/// Runs the `ffmpeg` on `PATH` with `args` in `dir`; fails the test, with
/// the reason, when it is missing or refuses.
fn ffmpeg(dir: &Scratch, args: &[&str]) {
    let output = Process::new("ffmpeg")
        .current_dir(&dir.0)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
        .args(args)
        .output()
        .unwrap_or_else(|e| {
            unreachable!(
                "these tests need FFmpeg: install it so that `ffmpeg` and `ffprobe` are on \
                 PATH ({e})"
            )
        });
    assert!(
        output.status.success(),
        "ffmpeg {args:?} failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A tiny MP4: six 64×48 frames of MPEG-4 Part 2 video at 25 fps and, when
/// `audio`, one AAC track. Both codecs are `FFmpeg`'s own, so any build has them.
fn write_mp4(dir: &Scratch, name: &str, audio: bool) -> PathBuf {
    let mut args = vec!["-f", "lavfi", "-i", "testsrc2=size=64x48:rate=25"];
    if audio {
        args.extend(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"]);
        args.extend(["-c:a", "aac", "-shortest"]);
    }
    args.extend([
        "-frames:v",
        "6",
        "-c:v",
        "mpeg4",
        "-pix_fmt",
        "yuv420p",
        name,
    ]);
    ffmpeg(dir, &args);
    dir.path(name)
}

fn probe(source: &Path) -> Event {
    let (mut worker, rx) = spawn();
    let sent = worker.send(
        7,
        Command::Probe {
            source: WirePath::from_path(source),
            video_stream: None,
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

fn facts(source: &Path) -> SourceFacts {
    match probe(source) {
        Event::Probed { facts } => facts,
        other => unreachable!("probe of {} failed: {other:?}", source.display()),
    }
}

/// Starts a job built, as the app builds it, from a fresh probe of `source`.
fn start(
    source: &Path,
    destination: &Path,
    settings: EncoderSettings,
) -> (Worker, Receiver<Delivery>) {
    let facts = facts(source);
    start_with(source, &facts, false, destination, settings)
}

fn start_with(
    source: &Path,
    facts: &SourceFacts,
    omissions_acknowledged: bool,
    destination: &Path,
    settings: EncoderSettings,
) -> (Worker, Receiver<Delivery>) {
    let (mut worker, rx) = spawn();
    let job = JobSpec {
        source: WirePath::from_path(source),
        fingerprint: facts.fingerprint,
        video_stream: facts.video_stream,
        omissions_acknowledged,
        destination: WirePath::from_path(destination),
        settings,
    };
    let sent = worker.send(1, Command::Start { job, live: None });
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
    let EngineStatus::Ready { capabilities, .. } = &status else {
        unreachable!("{status:?}")
    };
    assert!(
        capabilities.encode_outputs.contains(&OutputKind::Ivf)
            && capabilities.encode_outputs.contains(&OutputKind::Av2AnnexB),
        "{status:?}"
    );
    // Cargo sets SPLOT_STUDIO_SYSTEM_FFMPEG=1, so the engine uses the FFmpeg
    // on PATH and says so.
    let tool = capabilities.media_tool.as_ref().unwrap_or_else(|| {
        unreachable!(
            "the engine found no FFmpeg: install it so that `ffmpeg` and `ffprobe` are on \
             PATH ({status:?})"
        )
    });
    assert_eq!(tool.origin, MediaToolOrigin::System, "{status:?}");
    assert!(!tool.version.trim().is_empty(), "{status:?}");
    expect_clean_end(&rx);
}

#[test]
fn probe_reads_the_header_at_both_depths() {
    let dir = Scratch::new("probe");
    for ten_bit in [false, true] {
        let source = dir.path(if ten_bit { "ten.y4m" } else { "eight.y4m" });
        write_y4m(&source, 64, 36, 3, ten_bit);
        let facts = facts(&source);
        assert_eq!(facts.reader, SourceReader::Y4m);
        assert!(!facts.omissions.any(), "{facts:?}");
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
fn probe_refuses_a_file_that_is_not_video() {
    let dir = Scratch::new("garbage");
    // A truncated WAV header: not Y4M, and nothing FFmpeg is allowed to read
    // as video. Invalid or unsupported, it is refused either way.
    let source = dir.path("garbage.y4m");
    fs::write(&source, b"RIFF\0\0\0\0WAVEfmt ").unwrap_or_else(|e| unreachable!("{e}"));
    let answer = probe(&source);
    assert!(
        matches!(&answer, Event::Failed { failure }
            if matches!(failure.code, FailureCode::InvalidSource | FailureCode::UnsupportedSource)),
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
        let mut activity = Activity::new(destination.clone(), Some(4), false, Instant::now());
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
    let mut activity = Activity::new(destination.clone(), Some(2), false, Instant::now());
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
    let mut activity = Activity::new(destination.clone(), None, false, Instant::now());
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
    let mut activity = Activity::new(destination.clone(), None, false, Instant::now());
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
            Ok(Delivery::Event(_) | Delivery::Pictures(_)) => {}
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
    let mut activity = Activity::new(destination.clone(), None, false, Instant::now());
    wait_until_running(&rx);
    worker.kill();
    let deadline = Instant::now() + ANSWER_TIMEOUT;
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Delivery::Pictures(_)) => {}
            Ok(Delivery::Event(e)) => {
                assert!(!is_terminal(&e.body), "a killed worker sent {e:?}");
                activity.apply(Input::Event(e.body), Instant::now());
            }
            Ok(Delivery::Ended(ending)) => {
                // Windows reports a killed process as a plain exit code.
                #[cfg(unix)]
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

#[test]
fn probe_inventories_an_ordinary_file() {
    let dir = Scratch::new("probe-mp4");
    let source = write_mp4(&dir, "clip.mp4", true);
    let facts = facts(&source);
    assert_eq!(facts.reader, SourceReader::Ffmpeg, "{facts:?}");
    assert_eq!((facts.width, facts.height), (64, 48));
    assert_eq!(facts.frame_rate, Rational { num: 25, den: 1 });
    assert_eq!(facts.bit_depth, 8);
    assert_eq!(facts.chroma, ChromaLayout::Yuv420);
    assert!(facts.frame_count.is_none_or(|n| n == 6), "{facts:?}");
    let selected = facts
        .video_tracks
        .iter()
        .find(|t| t.stream == facts.video_stream);
    assert!(
        selected.is_some_and(|t| !t.attached_picture && t.codec == "mpeg4"),
        "{facts:?}"
    );
    assert_eq!(facts.omissions.audio, 1, "{facts:?}");
    assert_eq!(facts.omissions.other_video, 0, "{facts:?}");
    assert!(facts.omissions.any());
    assert_eq!(dir.entries(), ["clip.mp4"], "a probe writes nothing");
}

#[test]
fn an_ordinary_file_with_audio_converts_only_once_acknowledged() {
    let dir = Scratch::new("mp4-ack");
    let source = write_mp4(&dir, "clip.mp4", true);
    let facts = facts(&source);
    assert!(facts.omissions.any(), "{facts:?}");
    let destination = dir.path("clip.ivf");
    let fast = settings(OutputKind::Ivf);

    // Not acknowledged: refused, nothing written.
    let (mut worker, rx) = start_with(&source, &facts, false, &destination, fast);
    let mut activity = Activity::new(destination.clone(), facts.frame_count, true, Instant::now());
    let events = run_to_end(&rx, &mut activity);
    assert!(
        matches!(activity.phase(), Phase::Failed(f) if f.code == FailureCode::OmissionsNotAcknowledged),
        "{events:?}"
    );
    worker.close_input();
    expect_clean_end(&rx);
    assert_eq!(dir.entries(), ["clip.mp4"]);

    // Acknowledged: video only, every frame.
    let (mut worker, rx) = start_with(&source, &facts, true, &destination, fast);
    let mut activity = Activity::new(destination.clone(), facts.frame_count, true, Instant::now());
    let events = run_to_end(&rx, &mut activity);
    let Phase::Completed(report) = activity.phase() else {
        unreachable!("{events:?}")
    };
    assert_eq!(report.frames, 6, "{events:?}");
    assert_eq!(
        fs::metadata(&destination).map_or(0, |m| m.len()),
        report.bytes
    );
    worker.close_input();
    expect_clean_end(&rx);
    assert_eq!(dir.entries(), ["clip.ivf", "clip.mp4"]);
}

#[test]
fn a_video_only_file_needs_no_acknowledgement() {
    let dir = Scratch::new("mp4-video-only");
    let source = write_mp4(&dir, "silent.mp4", false);
    let facts = facts(&source);
    assert_eq!(facts.reader, SourceReader::Ffmpeg);
    assert!(!facts.omissions.any(), "{facts:?}");
    let destination = dir.path("silent.obu");
    let output = settings(OutputKind::Av2AnnexB);
    let (mut worker, rx) = start_with(&source, &facts, false, &destination, output);
    let mut activity = Activity::new(destination.clone(), facts.frame_count, true, Instant::now());
    let events = run_to_end(&rx, &mut activity);
    assert!(
        matches!(activity.phase(), Phase::Completed(r) if r.frames == 6),
        "{events:?}"
    );
    worker.close_input();
    expect_clean_end(&rx);
}

#[test]
fn a_source_that_changed_since_its_probe_is_refused() {
    let dir = Scratch::new("changed");
    let source = dir.path("clip.y4m");
    write_y4m(&source, 64, 36, 2, false);
    let facts = facts(&source);
    // Same name, different length: another file as far as the job knows.
    write_y4m(&source, 64, 36, 3, false);
    let destination = dir.path("clip.ivf");
    let output = settings(OutputKind::Ivf);
    let (mut worker, rx) = start_with(&source, &facts, false, &destination, output);
    let mut activity = Activity::new(destination.clone(), Some(2), false, Instant::now());
    let events = run_to_end(&rx, &mut activity);
    assert!(
        matches!(activity.phase(), Phase::Failed(f) if f.code == FailureCode::SourceChanged),
        "{events:?}"
    );
    worker.close_input();
    expect_clean_end(&rx);
    assert_eq!(dir.entries(), ["clip.y4m"]);
}

/// The app reaches the inspect process through the same client. A killed
/// inspect process ends its stream, and its documents die with it: a new
/// process knows none of them, so a stale id cannot answer with facts.
#[test]
fn inspect_documents_end_with_their_process() {
    use studio_domain::analysis::{AnalysisFailure, Framing};
    let dir = Scratch::new("inspect");
    let source = dir.path("td.obu");
    fs::write(&source, [0x01, 0x08]).unwrap_or_else(|e| unreachable!("{e}"));
    let spawn_inspect = || {
        let (tx, rx) = mpsc::channel();
        let worker = Worker::spawn(&engine(), Role::Inspect, move |d| {
            let _ = tx.send(d);
        })
        .unwrap_or_else(|e| unreachable!("{e}"));
        (worker, rx)
    };
    let (mut first, rx) = spawn_inspect();
    let open = Command::AnalysisOpen {
        document: 1,
        source: WirePath::from_path(&source),
    };
    assert!(first.send(1, open).is_ok());
    let opened = event(&rx, 1, ANSWER_TIMEOUT);
    assert!(
        matches!(opened, Event::AnalysisOpened { document: 1, ref summary } if summary.framing == Framing::AnnexB && summary.obus == 1),
        "{opened:?}"
    );
    first.kill();
    assert!(
        matches!(next(&rx, ANSWER_TIMEOUT), Delivery::Ended(_)),
        "a killed process ends its stream"
    );

    let (mut second, rx) = spawn_inspect();
    let page = Command::AnalysisPage {
        document: 1,
        first: 0,
        count: 1,
    };
    assert!(second.send(2, page).is_ok());
    let stale = event(&rx, 2, ANSWER_TIMEOUT);
    assert!(
        matches!(
            stale,
            Event::AnalysisFailed {
                document: 1,
                code: AnalysisFailure::UnknownDocument,
                ..
            }
        ),
        "{stale:?}"
    );
    second.close_input();
    expect_clean_end(&rx);
}
