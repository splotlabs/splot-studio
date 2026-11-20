// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! `splot-studio [FILE]`: opens the Splot Studio window, optionally with FILE
//! chosen as the source exactly as if it had been picked in the Open dialog.

// Keep a console window from opening next to the GUI on Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let source = args.next().map(PathBuf::from);
    if args.next().is_some() {
        eprintln!("usage: {} [FILE]", studio_app::APP_NAME);
        return ExitCode::from(2);
    }
    // Absolute, so the default destination and the display name the same
    // file the user meant; no filesystem access is involved.
    let source = match source.map(std::path::absolute).transpose() {
        Ok(source) => source,
        Err(e) => {
            eprintln!("{}: {e}", studio_app::APP_NAME);
            return ExitCode::from(2);
        }
    };
    match studio_app::build(source).and_then(studio_app::Studio::run) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}: {error}", studio_app::APP_NAME);
            ExitCode::FAILURE
        }
    }
}
