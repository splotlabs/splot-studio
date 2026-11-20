// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! `splot-studio`: opens the Splot Studio window.

// Keep a console window from opening next to the GUI on Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::process::ExitCode;

use slint::ComponentHandle;

fn main() -> ExitCode {
    match studio_app::build().and_then(|window| window.run()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}: {error}", studio_app::APP_NAME);
            ExitCode::FAILURE
        }
    }
}
