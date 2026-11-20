// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! "Show in folder": hands a published file to the platform's file manager.
//!
//! Linux opens the containing folder with `xdg-open` (there is no portable
//! way to select a file there); Windows selects the file in Explorer; macOS
//! reveals it in the Finder. Blocking until the helper exits, so callers run
//! it off the Slint thread.

use std::io;
use std::path::Path;
use std::process::Command;

/// Shows `file` in the platform file manager.
///
/// # Errors
/// Returns an error when the helper cannot be started or, except on Windows
/// where Explorer's exit code carries no meaning, when it reports failure.
pub fn show_in_folder(file: &Path) -> io::Result<()> {
    let mut command = command(file)?;
    let status = command.status()?;
    if status.success() || cfg!(windows) {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{} reported {status}",
            command.get_program().display()
        )))
    }
}

#[cfg(target_os = "macos")]
// Fallible only on Linux; one signature keeps the caller platform-free.
#[allow(clippy::unnecessary_wraps)]
fn command(file: &Path) -> io::Result<Command> {
    let mut command = Command::new("open");
    command.arg("-R").arg(file);
    Ok(command)
}

#[cfg(windows)]
// Fallible only on Linux; one signature keeps the caller platform-free.
#[allow(clippy::unnecessary_wraps)]
fn command(file: &Path) -> io::Result<Command> {
    use std::os::windows::process::CommandExt;
    // Explorer parses its own command line: `/select,"C:\a b\c.ivf"`.
    let mut arg = std::ffi::OsString::from("/select,\"");
    arg.push(file.as_os_str());
    arg.push("\"");
    let mut command = Command::new("explorer");
    command.raw_arg(arg);
    Ok(command)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn command(file: &Path) -> io::Result<Command> {
    let folder = file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| io::Error::other("the file has no containing folder"))?;
    let mut command = Command::new("xdg-open");
    command.arg(folder);
    Ok(command)
}

#[cfg(all(test, not(any(windows, target_os = "macos"))))]
mod tests {
    use super::*;

    #[test]
    fn linux_opens_the_containing_folder() {
        let command =
            command(Path::new("/out/dir/clip.ivf")).unwrap_or_else(|e| unreachable!("{e}"));
        assert_eq!(command.get_program(), "xdg-open");
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, ["/out/dir"]);
        assert!(command_has_no_folder_error());
    }

    fn command_has_no_folder_error() -> bool {
        command(Path::new("clip.ivf")).is_err()
    }
}
