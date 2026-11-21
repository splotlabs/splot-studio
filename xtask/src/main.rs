// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Splot Studio development automation: `cargo xtask ci [--fast]`, `cargo xtask doctor`
//! and `cargo xtask run [--release]`.
//!
//! Every step runs a real command, stops at the first failure, and says which
//! command failed. Nothing here downloads or modifies the machine.

mod checks;
mod doctor;
mod run;

use std::process::ExitCode;

const USAGE: &str = "\
usage:
    cargo xtask ci            full gate: fmt, repo checks, clippy, all tests, doc tests
    cargo xtask ci --fast     fmt, repo checks, clippy, lib/bin unit tests only
    cargo xtask doctor        check the toolchain and native build prerequisites
    cargo xtask run           build the app and its engine together, then start the app
    cargo xtask run --release the same, optimized (encoding is far faster)
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
