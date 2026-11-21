// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! `splot-studio-engine`: a thin shell over the `studio_engine` library.

use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use studio_domain::job::{CheckOutcome, JobSpec, SourceFacts, SourceReader, WirePath};
use studio_domain::protocol::Event;
use studio_domain::settings::EncoderSettings;
use studio_engine::cli::{self, Cli};
use studio_engine::{job, worker};

/// Exit status for a runtime failure; 2 is a usage error.
const FAILURE: u8 = 1;
const USAGE: u8 = 2;

fn main() -> ExitCode {
    match cli::parse(std::env::args_os().skip(1)) {
        Ok(Cli::Help) => print(&cli::help_text()),
        Ok(Cli::Version) => print(&cli::version_text()),
        Ok(Cli::Worker) => match worker::run(&mut io::stdin().lock(), io::stdout()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(&error, FAILURE),
        },
        Ok(Cli::Encode {
            source,
            destination,
            settings,
            video_stream,
            acknowledge_omissions,
        }) => encode(
            &source,
            &destination,
            settings,
            video_stream,
            acknowledge_omissions,
        ),
        Err(error) => {
            let hint = format!("{error}\n\n{}", cli::help_text());
            fail(&hint, USAGE)
        }
    }
}

/// Probes the source and prints its inventory, then runs one job on a job
/// thread, the same path the worker takes, and prints its events as lines:
/// progress at most once a second, everything else.
fn encode(
    source: &Path,
    destination: &Path,
    settings: EncoderSettings,
    video_stream: Option<u32>,
    omissions_acknowledged: bool,
) -> ExitCode {
    let started = Instant::now();
    let facts = match studio_engine::source::probe(source, video_stream) {
        Ok(facts) => facts,
        Err(stop) => {
            let end = match stop {
                job::Stop::Failed(failure) => Event::Failed { failure },
                job::Stop::Cancelled => Event::Cancelled,
            };
            line(&end, started);
            return ExitCode::from(FAILURE);
        }
    };
    print_inventory(&facts, started, omissions_acknowledged);
    let spec = JobSpec {
        source: WirePath::from_path(source),
        fingerprint: facts.fingerprint,
        video_stream: facts.video_stream,
        omissions_acknowledged,
        destination: WirePath::from_path(destination),
        settings,
    };
    let thread = job::spawn(move || {
        let mut last_progress: Option<Instant> = None;
        let mut report = |event: Event| {
            if matches!(event, Event::Progress { .. })
                && last_progress.is_some_and(|at| at.elapsed() < Duration::from_secs(1))
            {
                return;
            }
            if matches!(event, Event::Progress { .. }) {
                last_progress = Some(Instant::now());
            }
            line(&event, started);
        };
        job::run(&spec, &AtomicBool::new(false), &mut report)
    });
    let end = match thread.map(std::thread::JoinHandle::join) {
        Ok(Ok(end)) => end,
        Ok(Err(_)) => return fail(&"the job panicked", FAILURE),
        Err(error) => return fail(&format!("cannot start the job thread: {error}"), FAILURE),
    };
    line(&end, started);
    match end {
        Event::Completed { .. } => ExitCode::SUCCESS,
        _ => ExitCode::from(FAILURE),
    }
}

/// What the probe found, as the app would show it before a job.
fn print_inventory(facts: &SourceFacts, started: Instant, acknowledged: bool) {
    let at = started.elapsed().as_secs_f64();
    let mut text = match facts.reader {
        SourceReader::Y4m => format!("{at:8.3}s source: Y4M"),
        SourceReader::Ffmpeg => match studio_engine::media::identity() {
            Some(tool) => format!(
                "{at:8.3}s source: decoded by FFmpeg {} ({:?})",
                tool.version, tool.origin
            ),
            None => format!("{at:8.3}s source: decoded by FFmpeg"),
        },
    };
    for track in &facts.video_tracks {
        let _ = write!(
            text,
            "\n          video stream {}: {} {}x{}{}{}",
            track.stream,
            track.codec,
            track.width,
            track.height,
            if track.attached_picture {
                " (attached picture)"
            } else {
                ""
            },
            if track.stream == facts.video_stream {
                " <- encoded"
            } else {
                ""
            }
        );
    }
    let _ = write!(
        text,
        "\n          {}x{} {}-bit {:?}, {}/{} fps, pixel aspect {}, frames {}",
        facts.width,
        facts.height,
        facts.bit_depth,
        facts.chroma,
        facts.frame_rate.num,
        facts.frame_rate.den,
        facts
            .pixel_aspect
            .map_or_else(|| "unstated".to_owned(), |r| format!("{}:{}", r.num, r.den)),
        facts
            .frame_count
            .map_or_else(|| "counted when encoding".to_owned(), |n| n.to_string())
    );
    let o = facts.omissions;
    if o.any() {
        let _ = write!(
            text,
            "\n          left out (video-only output): {} other video, {} audio, {} subtitle, \
             {} attachment, {} other stream(s), {} chapter(s){}",
            o.other_video,
            o.audio,
            o.subtitles,
            o.attachments,
            o.other,
            o.chapters,
            if acknowledged {
                ", acknowledged"
            } else {
                "; pass --acknowledge-omissions to encode without them"
            }
        );
    }
    let _ = write!(text, "\n          disclosures: {:?}", facts.disclosures);
    let _ = writeln!(io::stdout(), "{text}");
}

/// One event as a line on stdout.
fn line(event: &Event, started: Instant) {
    let at = started.elapsed().as_secs_f64();
    let text = match event {
        Event::Stage { stage } => format!("{at:8.3}s stage {stage:?}"),
        Event::Working { partial } => match partial.to_path_buf() {
            Ok(path) => format!("{at:8.3}s working {}", path.display()),
            Err(error) => format!("{at:8.3}s working ({error})"),
        },
        Event::Progress { progress } => format!(
            "{at:8.3}s progress read={} encoded={} bytes={}",
            progress.frames_read, progress.frames_encoded, progress.bytes_written
        ),
        Event::Completed { report } => {
            let mut text = format!(
                "{at:8.3}s completed frames={} bytes={} disclosures={:?}",
                report.frames, report.bytes, report.disclosures
            );
            for check in &report.checks {
                let outcome = match &check.outcome {
                    CheckOutcome::Passed => "passed".to_owned(),
                    CheckOutcome::Failed { detail } => format!("FAILED {detail}"),
                    CheckOutcome::NotRun { reason } => format!("not run: {reason}"),
                };
                let _ = write!(text, "\n          check {:?}: {outcome}", check.kind);
            }
            text
        }
        Event::Cancelled => format!("{at:8.3}s cancelled"),
        Event::Failed { failure } => {
            format!("{at:8.3}s failed {:?}: {}", failure.code, failure.message)
        }
        other => format!("{at:8.3}s {other:?}"),
    };
    // A closed stdout must not stop the job; the exit status still reports it.
    let _ = writeln!(io::stdout(), "{text}");
}

fn print(text: &str) -> ExitCode {
    match io::stdout().write_all(text.as_bytes()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => fail(&error, FAILURE),
    }
}

/// Reports on stderr: stdout is reserved for protocol frames.
fn fail(message: &dyn std::fmt::Display, status: u8) -> ExitCode {
    // Nothing useful is left to do if stderr itself is gone.
    let _ = writeln!(
        io::stderr(),
        "{ENGINE}: {message}",
        ENGINE = studio_engine::ENGINE_NAME
    );
    ExitCode::from(status)
}
