// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! `splot-studio [FILE]...`: opens the Splot Studio window with its list of
//! files from last time, adding each FILE exactly as if it had been picked
//! in the Open dialog. Only one instance runs per user; a second one says
//! so and quits.
//!
//! `SPLOT_STUDIO_DATA_DIR` replaces the per-user data folder (tests and
//! development only).

// Keep a console window from opening next to the GUI on Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::process::ExitCode;

use studio_app::instance::{self, DATA_DIR_VAR};
use studio_app::{APP_NAME, Opening};

fn main() -> ExitCode {
    // Absolute, so the default destinations and the display name the same
    // files the user meant; no filesystem access is involved.
    let files: Result<Vec<PathBuf>, _> = std::env::args_os()
        .skip(1)
        .map(|arg| std::path::absolute(PathBuf::from(arg)))
        .collect();
    let files = match files {
        Ok(files) => files,
        Err(e) => {
            eprintln!("{APP_NAME}: {e}");
            return ExitCode::from(2);
        }
    };
    let dir = instance::data_dir(std::env::var_os(DATA_DIR_VAR));
    let prepared = match studio_app::prepare(dir) {
        Opening::Owner(prepared) => prepared,
        Opening::AlreadyOpen(dir) => {
            eprintln!(
                "{APP_NAME}: Splot Studio is already open (it owns {}); not opening another",
                dir.display()
            );
            if let Err(error) = studio_app::show_already_open() {
                eprintln!("{APP_NAME}: {error}");
            }
            return ExitCode::from(3);
        }
    };
    match studio_app::build(prepared, files).and_then(studio_app::Studio::run) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{APP_NAME}: {error}");
            ExitCode::FAILURE
        }
    }
}
