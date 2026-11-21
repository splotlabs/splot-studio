// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Splot Studio development automation: `cargo xtask ci [--fast]`, `cargo xtask doctor`,
//! `cargo xtask run [--release]`, `cargo xtask package` and `cargo xtask smoke`.
//!
//! Every step runs a real command, stops at the first failure, and says which
//! command failed. Nothing here downloads or modifies the machine.

mod checks;
mod doctor;
mod inspect;
mod notices;
mod package;
mod run;
mod sha256;
mod smoke;

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
usage:
    cargo xtask ci            full gate: fmt, repo checks, clippy, all tests, doc tests
    cargo xtask ci --fast     fmt, repo checks, clippy, lib/bin unit tests only
    cargo xtask doctor        check the toolchain and native build prerequisites
    cargo xtask run           build the app and its engine together, then start the app
    cargo xtask run --release the same, optimized (encoding is far faster)
    cargo xtask package [--ffmpeg-dir DIR] [--smoke]
                              unsigned test package for this machine's target in
                              target/package/; FFmpeg from native/ffmpeg/out/<target>/,
                              or DIR for a placeholder package that must not be shipped;
                              --smoke runs the smoke test on it afterwards
    cargo xtask smoke [ARCHIVE] unpack a package and run it with a clean environment
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["ci"] => ci(false),
        ["ci", "--fast"] => ci(true),
        ["doctor"] => doctor::run(),
        ["run"] => run_app(false),
        ["run", "--release"] => run_app(true),
        ["package", rest @ ..] => {
            let Some((ffmpeg_dir, smoke)) = package_options(rest) else {
                eprintln!("{USAGE}");
                return ExitCode::from(2);
            };
            package::run(ffmpeg_dir).and_then(|built| {
                if smoke {
                    smoke::run(Some(built.archive))
                } else {
                    Ok(())
                }
            })
        }
        ["smoke"] => smoke::run(None),
        ["smoke", archive] => smoke::run(Some(PathBuf::from(archive))),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("\nxtask: FAILED: {message}");
            ExitCode::FAILURE
        }
    }
}

/// `package` flags in any order: `--ffmpeg-dir DIR` and `--smoke`.
fn package_options(args: &[&str]) -> Option<(Option<PathBuf>, bool)> {
    let (mut dir, mut smoke) = (None, false);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match *arg {
            "--smoke" if !smoke => smoke = true,
            "--ffmpeg-dir" if dir.is_none() => dir = Some(PathBuf::from(args.next()?)),
            _ => return None,
        }
    }
    Some((dir, smoke))
}

/// The CI gate. `fast` skips integration, process and doc tests; it does not
/// skip or soften any other step.
fn ci(fast: bool) -> Result<(), String> {
    run::cargo(&["fmt", "--all", "--", "--check"])?;
    checks::headers()?;
    checks::dependency_direction()?;
    run::cargo(&[
        "clippy",
        "--workspace",
        "--all-targets",
        "--locked",
        "--",
        "-D",
        "warnings",
    ])?;
    if fast {
        run::cargo(&["test", "--workspace", "--lib", "--bins", "--locked"])?;
    } else {
        run::cargo(&["test", "--workspace", "--all-targets", "--locked"])?;
        run::cargo(&["test", "--workspace", "--doc", "--locked"])?;
    }
    println!("\nxtask: ci{} passed", if fast { " --fast" } else { "" });
    Ok(())
}

/// Builds the app and the engine in one profile, then starts the app.
///
/// `cargo run -p studio-app` alone rebuilds only the app, so it can start an
/// engine left over from an older build that speaks another protocol version.
fn run_app(release: bool) -> Result<(), String> {
    let profile: &[&str] = if release { &["--release"] } else { &[] };
    let packages = ["-p", "studio-app", "-p", "studio-engine", "--locked"];
    run::cargo(&[&["build"], &packages[..], profile].concat())?;
    run::cargo(&[&["run", "-p", "studio-app", "--locked"], profile].concat())
}
