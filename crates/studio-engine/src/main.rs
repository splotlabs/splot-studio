// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! `splot-studio-engine`: a thin shell over the `studio_engine` library.

use std::io::{self, Write};
use std::process::ExitCode;

use studio_engine::cli::{self, Cli};
use studio_engine::session;

/// Exit status for a runtime failure; 2 is a usage error.
const FAILURE: u8 = 1;
const USAGE: u8 = 2;

fn main() -> ExitCode {
    match cli::parse(std::env::args_os().skip(1)) {
        Ok(Cli::Help) => print(&cli::help_text()),
        Ok(Cli::Version) => print(&cli::version_text()),
        Ok(Cli::Handshake) => match session::run(&mut io::stdin().lock(), &mut io::stdout().lock())
        {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(&error, FAILURE),
        },
        Err(error) => {
            let hint = format!("{error}\n\n{}", cli::help_text());
            fail(&hint, USAGE)
        }
    }
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
