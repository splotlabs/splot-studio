// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Running child commands from the repository root.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The repository root: `xtask/`'s parent.
pub fn root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap_or(manifest).to_path_buf()
}

fn cargo_path() -> OsString {
    std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into())
}

/// Runs `cargo <args>` with inherited output; a non-zero exit is an error.
pub fn cargo(args: &[&str]) -> Result<(), String> {
    let shown = format!("cargo {}", args.join(" "));
    println!("\n==> {shown}");
    let status = Command::new(cargo_path())
        .args(args)
        .current_dir(root())
        .status()
        .map_err(|e| format!("could not start `{shown}`: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("`{shown}` exited with {status}"))
    }
}

/// Runs `program <args>` and returns its stdout; a non-zero exit is an error.
pub fn capture(program: impl Into<OsString>, args: &[&str]) -> Result<String, String> {
    let program = program.into();
    let shown = format!("{} {}", program.to_string_lossy(), args.join(" "));
    let output = Command::new(&program)
        .args(args)
        .current_dir(root())
        .output()
        .map_err(|e| format!("could not start `{shown}`: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "`{shown}` exited with {}: {}",
            output.status,
            stderr.trim()
        ));
    }
    String::from_utf8(output.stdout).map_err(|e| format!("`{shown}` printed non-UTF-8: {e}"))
}

/// Same as [`capture`] for `cargo`.
pub fn capture_cargo(args: &[&str]) -> Result<String, String> {
    capture(cargo_path(), args)
}
