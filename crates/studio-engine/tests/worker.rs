// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Process tests: spawn the real engine binary and speak the framed protocol.

mod common;

use std::error::Error;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command as Process, Output, Stdio};
use std::time::{Duration, Instant};

use common::{Clip, TestResult, leftovers, spec};
use studio_domain::job::{FailureCode, Stage, WirePath};
use studio_domain::protocol::{
    BuildIdentity, Command, Envelope, ErrorCode, Event, MAX_MESSAGE_BYTES, PROTOCOL_VERSION,
    read_message, write_message,
};
use studio_domain::settings::OutputKind;
use studio_engine::worker::CANCEL_GRACE;

const BIN: &str = env!("CARGO_BIN_EXE_splot-studio-engine");

/// Runs the engine with `args`, feeds it `stdin`, and waits for it to exit.
fn run(args: &[&str], stdin: &[u8]) -> Result<Output, Box<dyn Error>> {
    let mut child = Process::new(BIN)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // The engine may exit before reading everything (framing errors); that
    // is the behaviour under test, so a broken pipe here is not a failure.
    let _ = child.stdin.take().map(|mut pipe| pipe.write_all(stdin));
    Ok(child.wait_with_output()?)
}

fn worker(stdin: &[u8]) -> Result<Output, Box<dyn Error>> {
    run(&["worker"], stdin)
}

fn frame(request_id: u64, command: Command) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut bytes = Vec::new();
    write_message(&mut bytes, &Envelope::new(request_id, command))?;
    Ok(bytes)
}

fn hello() -> Command {
    let client = BuildIdentity {
        name: "test".into(),
        version: "0".into(),
    };
    Command::Hello { client }
}

fn hello_frame(request_id: u64) -> Result<Vec<u8>, Box<dyn Error>> {
    frame(request_id, hello())
}

fn raw_frame(body: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut bytes = u32::try_from(body.len())?.to_le_bytes().to_vec();
    bytes.extend_from_slice(body);
    Ok(bytes)
}

/// Decodes all of stdout as frames. Fails if any byte is not part of a frame,
/// which is how "stdout carries only protocol frames" is asserted.
fn events(output: &Output) -> Result<Vec<Envelope<Event>>, Box<dyn Error>> {
    let mut reader = output.stdout.as_slice();
    let mut events = Vec::new();
    while let Some(event) = read_message::<Event>(&mut reader)? {
        events.push(event);
    }
    Ok(events)
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A worker whose pipes the test drives step by step.
struct Live {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
}

impl Live {
    fn spawn() -> Result<Self, Box<dyn Error>> {
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
        let pipe = self.stdin.as_mut().ok_or("stdin closed")?;
        pipe.write_all(&frame(request_id, command)?)?;
        Ok(())
    }

    fn next(&mut self) -> Result<Envelope<Event>, Box<dyn Error>> {
        Ok(read_message::<Event>(&mut self.stdout)?.ok_or("worker closed stdout")?)
    }

    /// Reads events until one matches `until`, returning all of them.
    fn read_until(
        &mut self,
        until: impl Fn(&Event) -> bool,
    ) -> Result<Vec<Envelope<Event>>, Box<dyn Error>> {
        let mut seen = Vec::new();
        loop {
            let event = self.next()?;
            let done = until(&event.body);
            seen.push(event);
            if done {
                return Ok(seen);
            }
        }
    }
}

fn terminal(event: &Event) -> bool {
    matches!(
        event,
        Event::Completed { .. } | Event::Cancelled | Event::Failed { .. }
    )
}

/// A clip long enough that the job is still running when the test acts.
fn long_clip(dir: &Path) -> Result<std::path::PathBuf, Box<dyn Error>> {
    let src = dir.join("long.y4m");
    Clip::new(64, 64, "420", 400).write(&src)?;
    Ok(src)
}

#[test]
fn hello_round_trip() -> TestResult {
    let output = worker(&hello_frame(42)?)?;
    assert_eq!(output.status.code(), Some(0));
    let [reply] = events(&output)?
        .try_into()
        .map_err(|_| "expected one reply")?;
    assert_eq!(reply.protocol_version, PROTOCOL_VERSION);
    assert_eq!(reply.request_id, 42);
    let Event::Hello {
        engine,
        capabilities,
    } = reply.body
    else {
        return Err("expected Event::Hello".into());
    };
    assert_eq!(engine.name, "splot-studio-engine");
    assert_eq!(engine.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(capabilities.encode_outputs, OutputKind::ALL);
    Ok(())
}

#[test]
fn replies_follow_request_order() -> TestResult {
    let mut input = hello_frame(1)?;
    input.extend(hello_frame(2)?);
    let output = worker(&input)?;
    assert_eq!(output.status.code(), Some(0));
    let ids: Vec<u64> = events(&output)?.iter().map(|e| e.request_id).collect();
    assert_eq!(ids, [1, 2]);
    Ok(())
}

#[test]
fn clean_eof_exits_zero_with_no_output() -> TestResult {
    let output = worker(&[])?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, Vec::<u8>::new());
    Ok(())
}

#[test]
fn version_mismatch_is_answered_and_the_session_continues() -> TestResult {
    let mut input = raw_frame(
        br#"{"protocol_version":999,"request_id":9,"body":{"type":"hello","client":{"name":"x","version":"0"}}}"#,
    )?;
    input.extend(hello_frame(10)?);
    let output = worker(&input)?;
    assert_eq!(output.status.code(), Some(0));
    let got = events(&output)?;
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].request_id, 9);
    assert!(matches!(
        got[0].body,
        Event::Error {
            code: ErrorCode::IncompatibleVersion,
            ..
        }
    ));
    assert!(matches!(got[1].body, Event::Hello { .. }));
    Ok(())
}

#[test]
fn malformed_messages_are_answered() -> TestResult {
    let v = PROTOCOL_VERSION;
    let mut input = raw_frame(
        format!(r#"{{"protocol_version":{v},"request_id":5,"body":{{"type":"encode"}}}}"#)
            .as_bytes(),
    )?;
    input.extend(raw_frame(b"not json")?);
    // A Start whose settings are out of range does not decode either.
    input.extend(raw_frame(
        format!(r#"{{"protocol_version":{v},"request_id":6,"body":{{"type":"start","job":{{"source":{{"kind":"utf8","value":"a"}},"destination":{{"kind":"utf8","value":"b"}},"settings":{{"output":"ivf","preset":11,"crf":20}}}}}}}}"#).as_bytes(),
    )?);
    let output = worker(&input)?;
    assert_eq!(output.status.code(), Some(0));
    let got = events(&output)?;
    let ids: Vec<u64> = got.iter().map(|e| e.request_id).collect();
    assert_eq!(ids, [5, 0, 6]);
    for reply in got {
        assert!(matches!(
            reply.body,
            Event::Error {
                code: ErrorCode::Malformed,
                ..
            }
        ));
    }
    Ok(())
}

#[test]
fn oversize_length_prefix_is_fatal() -> TestResult {
    let output = worker(&(MAX_MESSAGE_BYTES + 1).to_le_bytes())?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, Vec::<u8>::new());
    assert!(
        stderr(&output).contains("outside 1..="),
        "stderr: {}",
        stderr(&output)
    );
    Ok(())
}

#[test]
fn zero_length_is_fatal() -> TestResult {
    let output = worker(&0_u32.to_le_bytes())?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, Vec::<u8>::new());
    assert!(
        stderr(&output).contains("length 0"),
        "stderr: {}",
        stderr(&output)
    );
    Ok(())
}

#[test]
fn truncated_message_is_fatal() -> TestResult {
    let frame = hello_frame(1)?;
    // Inside the body, and inside the length prefix.
    for cut in [frame.len() - 1, 2] {
        let output = worker(&frame[..cut])?;
        assert_eq!(output.status.code(), Some(1), "cut at {cut}");
        assert_eq!(output.stdout, Vec::<u8>::new(), "cut at {cut}");
        assert!(
            stderr(&output).contains("ended inside"),
            "stderr: {}",
            stderr(&output)
        );
    }
    Ok(())
}

#[test]
fn a_fatal_frame_after_a_good_one_keeps_the_earlier_reply() -> TestResult {
    let mut input = hello_frame(1)?;
    input.extend(0_u32.to_le_bytes());
    let output = worker(&input)?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(events(&output)?.len(), 1);
    Ok(())
}

#[test]
fn probe_answers_facts_or_failure() -> TestResult {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("in.y4m");
    Clip::new(16, 8, "420p10", 2).write(&src)?;
    let mut input = frame(
        1,
        Command::Probe {
            source: WirePath::from_path(&src),
            video_stream: None,
        },
    )?;
    input.extend(frame(
        2,
        Command::Probe {
            source: WirePath::from_path(&dir.path().join("no")),
            video_stream: None,
        },
    )?);
    let output = worker(&input)?;
    let got = events(&output)?;
    assert!(
        matches!(&got[0].body, Event::Probed { facts } if facts.bit_depth == 10 && facts.frame_count == Some(2)),
        "{got:?}"
    );
    assert!(matches!(
        &got[1].body,
        Event::Failed { failure } if failure.code == FailureCode::InvalidSource
    ));
    Ok(())
}

/// A whole job over the protocol, and a second `Start` refused meanwhile.
/// Input stays open until the terminal event: closing it would cancel.
#[test]
fn a_job_runs_once_with_ordered_events() -> TestResult {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("in.y4m");
    Clip::new(16, 8, "420", 10).write(&src)?;
    let dst = dir.path().join("out.obu");
    let job = spec(&src, &dst, OutputKind::Av2AnnexB);
    let mut live = Live::spawn()?;
    live.send(
        7,
        Command::Start {
            job: job.clone(),
            live: None,
        },
    )?;
    live.send(8, Command::Start { job, live: None })?;
    let got = live.read_until(terminal)?;
    drop(live.stdin.take());
    assert!(read_message::<Event>(&mut live.stdout)?.is_none());
    assert_eq!(live.child.wait()?.code(), Some(0));
    let refused = got.iter().filter(|e| e.request_id == 8).collect::<Vec<_>>();
    assert!(
        matches!(
            refused.as_slice(),
            [Envelope {
                body: Event::Error {
                    code: ErrorCode::WrongState,
                    ..
                },
                ..
            }]
        ),
        "{refused:?}"
    );
    let job: Vec<&Event> = got
        .iter()
        .filter(|e| e.request_id == 7)
        .map(|e| &e.body)
        .collect();
    let stages: Vec<Stage> = job
        .iter()
        .filter_map(|e| match e {
            Event::Stage { stage } => Some(*stage),
            _ => None,
        })
        .collect();
    assert_eq!(
        stages,
        [
            Stage::Encoding,
            Stage::Finalizing,
            Stage::Verifying,
            Stage::Publishing
        ]
    );
    assert!(matches!(job.last(), Some(Event::Completed { report }) if report.frames == 10));
    let working: Vec<usize> = (0..job.len())
        .filter(|&i| matches!(job[i], Event::Working { .. }))
        .collect();
    let first_progress = job.iter().position(|e| matches!(e, Event::Progress { .. }));
    assert!(
        matches!(working.as_slice(), [at] if Some(*at) < first_progress && *at > 0),
        "{job:?}"
    );
    let mut last = (0, 0);
    for event in &job {
        if let Event::Progress { progress } = event {
            let now = (progress.frames_read, progress.frames_encoded);
            assert!(now >= last, "progress went backwards");
            last = now;
        }
    }
    assert!(dst.exists());
    Ok(())
}

#[test]
fn cancel_is_acknowledged_then_the_job_stops_clean() -> TestResult {
    let dir = tempfile::tempdir()?;
    let src = long_clip(dir.path())?;
    let dst = dir.path().join("out.ivf");
    let mut live = Live::spawn()?;
    live.send(
        1,
        Command::Start {
            job: spec(&src, &dst, OutputKind::Ivf),
            live: None,
        },
    )?;
    live.read_until(|e| matches!(e, Event::Progress { .. }))?;
    let asked = Instant::now();
    live.send(2, Command::Cancel)?;
    let rest = live.read_until(terminal)?;
    let latency = asked.elapsed();
    let acknowledged = rest
        .iter()
        .position(|e| e.request_id == 2 && e.body == Event::CancelRequested);
    let ended = rest.iter().position(|e| e.body == Event::Cancelled);
    assert!(
        matches!((acknowledged, ended), (Some(a), Some(c)) if a < c),
        "{rest:?}"
    );
    eprintln!("cancel latency (debug build, 64x64): {latency:?}");
    // The worker stays up after its job; a second cancel is refused.
    live.send(3, Command::Cancel)?;
    assert!(matches!(
        live.next()?.body,
        Event::Error {
            code: ErrorCode::WrongState,
            ..
        }
    ));
    drop(live.stdin.take());
    assert_eq!(live.child.wait()?.code(), Some(0));
    assert_eq!(
        leftovers(dir.path(), &[&src]),
        Vec::<std::path::PathBuf>::new()
    );
    Ok(())
}

/// Closing the worker's input cancels the job; it still reports and cleans up.
#[test]
fn stdin_eof_mid_job_cancels() -> TestResult {
    let dir = tempfile::tempdir()?;
    let src = long_clip(dir.path())?;
    let dst = dir.path().join("out.ivf");
    let mut live = Live::spawn()?;
    live.send(
        1,
        Command::Start {
            job: spec(&src, &dst, OutputKind::Ivf),
            live: None,
        },
    )?;
    live.read_until(|e| matches!(e, Event::Progress { .. }))?;
    drop(live.stdin.take());
    let rest = live.read_until(terminal)?;
    assert!(
        matches!(rest.last().map(|e| &e.body), Some(Event::Cancelled)),
        "{rest:?}"
    );
    assert!(read_message::<Event>(&mut live.stdout)?.is_none());
    assert_eq!(live.child.wait()?.code(), Some(0));
    assert_eq!(
        leftovers(dir.path(), &[&src]),
        Vec::<std::path::PathBuf>::new()
    );
    Ok(())
}

/// A killed worker cannot clean up: nothing is published, and its
/// temporary file `.<name>.<random>.partial` stays beside the destination.
#[test]
fn a_killed_worker_publishes_nothing_and_leaves_its_temporary_file() -> TestResult {
    let dir = tempfile::tempdir()?;
    let src = long_clip(dir.path())?;
    let dst = dir.path().join("out.ivf");
    let mut live = Live::spawn()?;
    live.send(
        1,
        Command::Start {
            job: spec(&src, &dst, OutputKind::Ivf),
            live: None,
        },
    )?;
    let seen = live.read_until(|e| matches!(e, Event::Progress { .. }))?;
    live.child.kill()?;
    live.child.wait()?;
    assert!(!dst.exists());
    let announced: Vec<std::path::PathBuf> = seen
        .iter()
        .filter_map(|e| match &e.body {
            Event::Working { partial } => partial.to_path_buf().ok(),
            _ => None,
        })
        .collect();
    let left = leftovers(dir.path(), &[&src]);
    assert_eq!(
        left, announced,
        "the leftover is not what Working announced"
    );
    let names: Vec<String> = left
        .iter()
        .filter_map(|p| Some(p.file_name()?.to_string_lossy().into_owned()))
        .collect();
    assert!(
        matches!(names.as_slice(), [name] if name.starts_with(".out.ivf.") && name.ends_with(".partial")),
        "{names:?}"
    );
    Ok(())
}

/// A peer that stops reading stalls only the writer: the job finishes and
/// publishes, command replies back up, and once the peer reads again every
/// reply and the terminal event arrive.
#[test]
fn a_non_reading_peer_neither_deadlocks_nor_loses_the_terminal() -> TestResult {
    const HELLOS: u64 = 3000;
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("in.y4m");
    Clip::new(16, 8, "420", 4).write(&src)?;
    let dst = dir.path().join("out.ivf");
    let mut live = Live::spawn()?;
    live.send(
        1,
        Command::Start {
            job: spec(&src, &dst, OutputKind::Ivf),
            live: None,
        },
    )?;
    let mut stdin = live.stdin.take().ok_or("no stdin")?;
    // Far more replies than the pipe holds; written from another thread
    // because the worker stops reading once its reply backlog is full.
    let flood = std::thread::spawn(move || -> std::io::Result<ChildStdin> {
        for id in 0..HELLOS {
            let client = BuildIdentity {
                name: "flood".into(),
                version: "0".into(),
            };
            let mut bytes = Vec::new();
            let _ = write_message(
                &mut bytes,
                &Envelope::new(100 + id, Command::Hello { client }),
            );
            stdin.write_all(&bytes)?;
        }
        Ok(stdin)
    });
    let deadline = Instant::now() + Duration::from_secs(120);
    while !dst.exists() {
        assert!(
            Instant::now() < deadline,
            "the job did not finish while stdout was unread"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut stdout = Vec::new();
    let reader = std::thread::spawn(move || live.stdout.read_to_end(&mut stdout).map(|_| stdout));
    let stdin = flood.join().map_err(|_| "flood panicked")??;
    drop(stdin);
    let bytes = reader.join().map_err(|_| "reader panicked")??;
    assert_eq!(live.child.wait()?.code(), Some(0));
    let mut events = Vec::new();
    let mut slice = bytes.as_slice();
    while let Some(event) = read_message::<Event>(&mut slice)? {
        events.push(event);
    }
    let hellos = events
        .iter()
        .filter(|e| matches!(e.body, Event::Hello { .. }))
        .count();
    assert_eq!(hellos as u64, HELLOS);
    let ends: Vec<_> = events.iter().filter(|e| terminal(&e.body)).collect();
    assert!(
        matches!(
            ends.as_slice(),
            [Envelope {
                request_id: 1,
                body: Event::Completed { .. },
                ..
            }]
        ),
        "{ends:?}"
    );
    Ok(())
}

#[test]
fn help_is_truthful() -> TestResult {
    let output = run(&["--help"], &[])?;
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8(output.stdout)?;
    assert!(
        text.contains("worker") && text.contains("encode") && text.contains("preview"),
        "{text}"
    );
    Ok(())
}

#[test]
fn version_reports_build_and_protocol_identity() -> TestResult {
    let output = run(&["--version"], &[])?;
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8(output.stdout)?;
    assert!(
        text.contains(&format!(
            "splot-studio-engine {}",
            env!("CARGO_PKG_VERSION")
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!("protocol version: {PROTOCOL_VERSION}")),
        "{text}"
    );
    assert!(
        text.contains("AV2 Annex B length-delimited OBU stream"),
        "{text}"
    );
    Ok(())
}

#[test]
fn unknown_argument_is_a_usage_error() -> TestResult {
    for args in [&["--bogus"][..], &[], &["worker", "extra"], &["handshake"]] {
        let output = run(args, &[])?;
        assert_eq!(output.status.code(), Some(2), "args {args:?}");
        assert_eq!(output.stdout, Vec::<u8>::new(), "args {args:?}");
        assert!(stderr(&output).contains("USAGE"), "args {args:?}");
    }
    Ok(())
}

/// The text `encode` command runs the same job path.
#[test]
fn encode_command_prints_the_job() -> TestResult {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("in.y4m");
    Clip::new(16, 8, "420p10", 3).write(&src)?;
    let dst = dir.path().join("out.obu");
    let (Some(src_arg), Some(dst_arg)) = (src.to_str(), dst.to_str()) else {
        return Err("non-UTF-8 temp path".into());
    };
    let output = run(
        &[
            "encode", src_arg, dst_arg, "--output", "obu", "--preset", "10",
        ],
        &[],
    )?;
    let text = String::from_utf8(output.stdout)?;
    assert_eq!(output.status.code(), Some(0), "{text}");
    assert!(text.contains("completed frames=3"), "{text}");
    assert_eq!(text.matches(" working ").count(), 1, "{text}");
    assert!(text.contains(".out.obu."), "{text}");
    assert!(text.contains("check Structure: passed"), "{text}");
    let again = run(&["encode", src_arg, dst_arg], &[])?;
    assert_eq!(again.status.code(), Some(1));
    assert!(String::from_utf8(again.stdout)?.contains("DestinationExists"));
    Ok(())
}

/// Events after the job's terminal event, once the worker has exited.
fn rest_after_exit(live: &mut Live) -> Result<Vec<Envelope<Event>>, Box<dyn Error>> {
    let mut rest = Vec::new();
    while let Some(event) = read_message::<Event>(&mut live.stdout)? {
        rest.push(event);
    }
    Ok(rest)
}

/// Waits for the worker to exit, killing it after `limit`.
fn exits_within(live: &mut Live, limit: Duration) -> Result<Option<i32>, Box<dyn Error>> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = live.child.try_wait()? {
            return Ok(status.code());
        }
        if Instant::now() > deadline {
            live.child.kill()?;
            return Err("the worker did not exit in time".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A cancel during one long codec call (a whole mini-GOP of 640x360 frames)
/// stops it cooperatively through the encoder's token: `Cancelled` well
/// before [`CANCEL_GRACE`] (the forced path cannot emit it earlier), the
/// temporary file removed, and the worker still serving until the end of
/// its input. The same through the end of input.
#[test]
fn a_cancel_inside_a_long_codec_call_is_bounded() -> TestResult {
    for by_eof in [false, true] {
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("big.y4m");
        Clip::new(640, 360, "420", 12).write(&src)?;
        let dst = dir.path().join("out.ivf");
        let mut live = Live::spawn()?;
        live.send(
            1,
            Command::Start {
                job: spec(&src, &dst, OutputKind::Ivf),
                live: None,
            },
        )?;
        // The first unit is out; the next call codes the whole mini-GOP.
        live.read_until(
            |e| matches!(e, Event::Progress { progress } if progress.frames_encoded >= 1),
        )?;
        let asked = Instant::now();
        let stdin = if by_eof {
            drop(live.stdin.take());
            None
        } else {
            live.send(2, Command::Cancel)?;
            live.stdin.take()
        };
        let got = live.read_until(terminal)?;
        let latency = asked.elapsed();
        eprintln!("cooperative cancel (eof: {by_eof}) after {latency:?}");
        assert!(latency < CANCEL_GRACE / 2, "{latency:?}");
        assert!(
            matches!(got.last().map(|e| &e.body), Some(Event::Cancelled)),
            "{got:?}"
        );
        assert!(!dst.exists());
        assert_eq!(
            leftovers(dir.path(), &[&src]),
            Vec::<std::path::PathBuf>::new()
        );
        if let Some(stdin) = stdin {
            // Not abandoned: the reader still answers, and the worker exits
            // only once its input ends.
            live.stdin = Some(stdin);
            live.send(3, hello())?;
            assert!(matches!(live.next()?.body, Event::Hello { .. }));
            assert_eq!(live.child.try_wait()?, None);
            drop(live.stdin.take());
        }
        assert_eq!(exits_within(&mut live, Duration::from_secs(3))?, Some(0));
        assert_eq!(rest_after_exit(&mut live)?, Vec::new());
        assert!(!dst.exists());
        assert_eq!(
            leftovers(dir.path(), &[&src]),
            Vec::<std::path::PathBuf>::new()
        );
    }
    Ok(())
}

/// Cancels racing the end of a tiny job: always exactly one terminal event,
/// nothing after it, and the files match it.
#[test]
fn a_cancel_racing_completion_has_one_outcome() -> TestResult {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("in.y4m");
    let clip = Clip::new(16, 8, "420", 2);
    clip.write(&src)?;
    let (mut completed, mut cancelled) = (0, 0);
    for round in 0..25_u64 {
        let dst = dir.path().join(format!("out-{round}.ivf"));
        let mut live = Live::spawn()?;
        live.send(
            1,
            Command::Start {
                job: spec(&src, &dst, OutputKind::Ivf),
                live: None,
            },
        )?;
        std::thread::sleep(Duration::from_millis(round));
        live.send(2, Command::Cancel)?;
        let mut events = live.read_until(terminal)?;
        drop(live.stdin.take());
        events.extend(rest_after_exit(&mut live)?);
        assert_eq!(live.child.wait()?.code(), Some(0));
        let ends: Vec<usize> = (0..events.len())
            .filter(|&i| terminal(&events[i].body))
            .collect();
        let [end] = ends.as_slice() else {
            return Err(format!("round {round}: {events:?}").into());
        };
        assert!(
            events[end + 1..].iter().all(|e| e.request_id != 1),
            "round {round}: job events after the terminal: {events:?}"
        );
        match &events[*end].body {
            Event::Completed { report } => {
                completed += 1;
                assert_eq!(report.frames, 2);
                assert_eq!(common::decode(&dst)?.len(), 2 * 16 * 8 * 3 / 2);
            }
            Event::Cancelled => {
                cancelled += 1;
                assert!(!dst.exists(), "round {round}");
            }
            other => return Err(format!("round {round}: {other:?}").into()),
        }
        let published: Vec<std::path::PathBuf> = (0..=round)
            .map(|r| dir.path().join(format!("out-{r}.ivf")))
            .filter(|p| p.exists())
            .collect();
        let mut keep: Vec<&Path> = vec![&src];
        keep.extend(published.iter().map(std::path::PathBuf::as_path));
        assert_eq!(
            leftovers(dir.path(), &keep),
            Vec::<std::path::PathBuf>::new(),
            "round {round}"
        );
    }
    eprintln!("completed {completed}, cancelled {cancelled}");
    Ok(())
}

/// A worker with `SPLOT_STUDIO_SYSTEM_FFMPEG` removed (or set, as Cargo
/// sets it for tests).
fn worker_with(opt_in: bool, stdin: &[u8]) -> Result<Output, Box<dyn Error>> {
    let mut command = Process::new(BIN);
    command.arg("worker");
    if !opt_in {
        command.env_remove(studio_engine::media::SYSTEM_OPT_IN);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let _ = child.stdin.take().map(|mut pipe| pipe.write_all(stdin));
    Ok(child.wait_with_output()?)
}

/// A small MP4 made by the `ffmpeg` on `PATH`.
fn mp4(
    dir: &Path,
    width: usize,
    height: usize,
    frames: usize,
) -> Result<std::path::PathBuf, Box<dyn Error>> {
    let src = dir.join("in.mp4");
    common::make(
        &Clip::new(width, height, "420", frames),
        dir,
        &["-c:v", "libx264"],
        &src,
    )?;
    Ok(src)
}

#[test]
fn hello_reports_the_media_tool() -> TestResult {
    let output = worker_with(true, &hello_frame(1)?)?;
    let got = events(&output)?;
    let Some(Event::Hello { capabilities, .. }) = got.first().map(|e| &e.body) else {
        return Err(format!("{got:?}").into());
    };
    let tool = capabilities.media_tool.as_ref().ok_or("no media tool")?;
    assert_eq!(
        tool.origin,
        studio_domain::protocol::MediaToolOrigin::System
    );
    assert_ne!(tool.version, "");
    Ok(())
}

/// Without the opt-in and with nothing bundled, ordinary media files are
/// refused with `MediaToolMissing`, and Y4M still works.
#[test]
fn without_a_tool_only_y4m_is_read() -> TestResult {
    let dir = tempfile::tempdir()?;
    let media = mp4(dir.path(), 16, 8, 2)?;
    let y4m = dir.path().join("in.y4m");
    Clip::new(16, 8, "420", 2).write(&y4m)?;
    let probe = |id, path: &Path| {
        frame(
            id,
            Command::Probe {
                source: WirePath::from_path(path),
                video_stream: None,
            },
        )
    };
    let mut input = hello_frame(1)?;
    input.extend(probe(2, &media)?);
    input.extend(probe(3, &y4m)?);
    let output = worker_with(false, &input)?;
    let got = events(&output)?;
    assert!(
        matches!(&got[0].body, Event::Hello { capabilities, .. } if capabilities.media_tool.is_none()),
        "{got:?}"
    );
    assert!(
        matches!(&got[1].body, Event::Failed { failure } if failure.code == FailureCode::MediaToolMissing),
        "{got:?}"
    );
    assert!(matches!(&got[2].body, Event::Probed { .. }), "{got:?}");
    // A job on the media file fails the same way.
    let dst = dir.path().join("out.ivf");
    let output = worker_with(
        false,
        &frame(
            4,
            Command::Start {
                job: spec(&media, &dst, OutputKind::Ivf),
                live: None,
            },
        )?,
    )?;
    let got = events(&output)?;
    assert!(
        matches!(&got.last().map(|e| &e.body), Some(Event::Failed { failure }) if failure.code == FailureCode::MediaToolMissing),
        "{got:?}"
    );
    assert!(!dst.exists());
    Ok(())
}

/// The first `PATH` directory holding `name`.
fn on_path(name: &str) -> Option<std::path::PathBuf> {
    let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(&file))
        .find(|path| path.is_file())
}

/// Tools in the engine's `ffmpeg/` directory are used without the opt-in
/// and reported as bundled.
#[test]
fn tools_beside_the_engine_are_bundled() -> TestResult {
    let dir = tempfile::tempdir()?;
    let engine = dir
        .path()
        .join(format!("engine{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(BIN, &engine)?;
    let tools = dir.path().join("ffmpeg");
    std::fs::create_dir(&tools)?;
    for name in ["ffmpeg", "ffprobe"] {
        let found = on_path(name).ok_or(format!("{name} is not on PATH"))?;
        let found = std::fs::canonicalize(found)?;
        let link = tools.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        #[cfg(unix)]
        std::os::unix::fs::symlink(&found, &link)?;
        #[cfg(not(unix))]
        if std::fs::hard_link(&found, &link).is_err() {
            std::fs::copy(&found, &link)?;
        }
    }
    let output = Process::new(&engine)
        .arg("--version")
        .env_remove(studio_engine::media::SYSTEM_OPT_IN)
        .output()?;
    let text = String::from_utf8(output.stdout)?;
    assert!(text.contains("(Bundled,"), "{text}");
    Ok(())
}

/// The live `ffmpeg`/`ffprobe` children of `live` reading `needle`.
#[cfg(target_os = "linux")]
fn tools_of(live: &Live, needle: &Path) -> Vec<u32> {
    common::tool_children(live.child.id(), &needle.to_string_lossy())
}

/// A cancel inside a long codec call on the `FFmpeg` route stops
/// cooperatively and reaps the decoding `ffmpeg` before `Cancelled`;
/// nothing is published or left, and the worker exits on the end of input.
#[test]
fn a_cancel_leaves_no_ffmpeg_behind() -> TestResult {
    let dir = tempfile::tempdir()?;
    let src = mp4(dir.path(), 640, 360, 12)?;
    let dst = dir.path().join("out.ivf");
    let mut live = Live::spawn()?;
    live.send(
        1,
        Command::Start {
            job: spec(&src, &dst, OutputKind::Ivf),
            live: None,
        },
    )?;
    live.read_until(|e| matches!(e, Event::Progress { progress } if progress.frames_encoded >= 1))?;
    #[cfg(target_os = "linux")]
    let decoders = tools_of(&live, &src);
    #[cfg(target_os = "linux")]
    assert_eq!(decoders.len(), 1, "one ffmpeg decodes: {decoders:?}");
    let asked = Instant::now();
    live.send(2, Command::Cancel)?;
    let got = live.read_until(terminal)?;
    assert!(asked.elapsed() < CANCEL_GRACE / 2, "{:?}", asked.elapsed());
    assert!(
        matches!(got.last().map(|e| &e.body), Some(Event::Cancelled)),
        "{got:?}"
    );
    #[cfg(target_os = "linux")]
    for pid in decoders {
        assert!(!common::alive_tool(pid), "ffmpeg {pid} outlived its job");
    }
    assert_eq!(live.child.try_wait()?, None);
    drop(live.stdin.take());
    assert_eq!(exits_within(&mut live, Duration::from_secs(3))?, Some(0));
    assert!(!dst.exists());
    assert_eq!(
        leftovers(dir.path(), &[&src]),
        Vec::<std::path::PathBuf>::new()
    );
    Ok(())
}

/// `kill -9` of the worker runs no code; its `ffmpeg` stops at its next
/// write to the closed pipe and is reaped by the system.
#[cfg(target_os = "linux")]
#[test]
fn a_killed_worker_leaves_no_ffmpeg_running() -> TestResult {
    let dir = tempfile::tempdir()?;
    let src = mp4(dir.path(), 640, 360, 12)?;
    let dst = dir.path().join("out.ivf");
    let mut live = Live::spawn()?;
    live.send(
        1,
        Command::Start {
            job: spec(&src, &dst, OutputKind::Ivf),
            live: None,
        },
    )?;
    live.read_until(|e| matches!(e, Event::Progress { .. }))?;
    let decoders = tools_of(&live, &src);
    assert_eq!(decoders.len(), 1, "{decoders:?}");
    live.child.kill()?;
    live.child.wait()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while decoders.iter().any(|&pid| common::alive_tool(pid)) {
        assert!(
            Instant::now() < deadline,
            "ffmpeg kept running after the worker was killed"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!dst.exists());
    Ok(())
}

/// `encode` on a media file prints the inventory, and needs the
/// acknowledgement when something is left out.
#[test]
fn encode_command_reads_media_files() -> TestResult {
    let dir = tempfile::tempdir()?;
    let clip = Clip::new(32, 16, "420", 3);
    let raw = dir.path().join("v.yuv");
    std::fs::write(&raw, clip.raw())?;
    let audio = dir.path().join("a.raw");
    std::fs::write(&audio, vec![0_u8; 3200])?;
    let src = dir.path().join("with audio.mkv");
    let mut args = clip.raw_input("yuv420p", &raw);
    args.extend(["-f", "s16le", "-ar", "8000", "-ac", "1", "-i"].map(str::to_owned));
    args.push(audio.to_string_lossy().into_owned());
    args.extend(["-map", "0", "-map", "1", "-c:v", "ffv1", "-c:a", "flac"].map(str::to_owned));
    args.push(src.to_string_lossy().into_owned());
    common::ffmpeg(&args.iter().map(String::as_str).collect::<Vec<_>>())?;
    let dst = dir.path().join("out.obu");
    let (Some(src_arg), Some(dst_arg)) = (src.to_str(), dst.to_str()) else {
        return Err("non-UTF-8 temp path".into());
    };
    let refused = run(&["encode", src_arg, dst_arg, "--output", "obu"], &[])?;
    let text = String::from_utf8(refused.stdout)?;
    assert_eq!(refused.status.code(), Some(1), "{text}");
    assert!(
        text.contains("left out (video-only output)") && text.contains("1 audio"),
        "{text}"
    );
    assert!(text.contains("OmissionsNotAcknowledged"), "{text}");
    assert!(!dst.exists());
    let output = run(
        &[
            "encode",
            src_arg,
            dst_arg,
            "--output",
            "obu",
            "--stream",
            "0",
            "--acknowledge-omissions",
            "--preset",
            "10",
        ],
        &[],
    )?;
    let text = String::from_utf8(output.stdout)?;
    assert_eq!(output.status.code(), Some(0), "{text}");
    assert!(
        text.contains("decoded by FFmpeg") && text.contains("<- encoded"),
        "{text}"
    );
    assert!(text.contains("completed frames=3"), "{text}");
    let wrong = run(&["encode", src_arg, dst_arg, "--stream", "1"], &[])?;
    assert!(String::from_utf8(wrong.stdout)?.contains("UnsupportedSource"));
    Ok(())
}
