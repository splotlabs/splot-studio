// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! `splot-studio-engine`: a thin shell over the `studio_engine` library.

use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use studio_domain::job::{CheckOutcome, JobSpec, WirePath};
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
        }) => encode(&source, &destination, settings),
        Err(error) => {
            let hint = format!("{error}\n\n{}", cli::help_text());
            fail(&hint, USAGE)
        }
    }
}

/// Runs one job on a job thread, the same path the worker takes, and prints
/// its events as lines: progress at most once a second, everything else.
fn encode(source: &Path, destination: &Path, settings: EncoderSettings) -> ExitCode {
    let spec = JobSpec {
        source: WirePath::from_path(source),
        destination: WirePath::from_path(destination),
        settings,
    };
    let started = Instant::now();
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

/// One event as a line on stdout.
fn line(event: &Event, started: Instant) {
    let at = started.elapsed().as_secs_f64();
    let text = match event {
        Event::Stage { stage } => format!("{at:8.3}s stage {stage:?}"),
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
