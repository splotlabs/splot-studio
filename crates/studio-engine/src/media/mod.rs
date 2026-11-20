// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Ordinary media files, read through the `FFmpeg` command-line tools.
//!
//! No `FFmpeg` library is linked: `ffprobe` and `ffmpeg` run as child
//! processes with restricted arguments, and decoded frames arrive as a Y4M
//! stream on a pipe, read by the same upstream reader as a Y4M file.
//!
//! - [`tool`] finds the two executables: beside the engine (bundled), or on
//!   `PATH` only when [`SYSTEM_OPT_IN`] is `1` (a development opt-in).
//! - [`probe`] reads the stream inventory and applies the input contract.
//! - [`scan`] decodes the selected stream once without its pixels and proves
//!   constant frame rate, constant geometry, format and colour, before any
//!   output exists.
//! - [`decode`] pipes the frames.
//!
//! Every invocation reads one local file (`file:` protocol only) through
//! one of [`FORMAT_WHITELIST`]'s demuxers, so playlists, concatenation
//! scripts, image sequences and network sources are refused by the tool
//! itself. The `mov` demuxer's external data references stay at their
//! default, off. This is a restriction, not a sandbox.

pub mod decode;
pub mod probe;
pub mod process;
pub mod scan;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use studio_domain::job::FailureCode;
use studio_domain::protocol::{MediaTool, MediaToolOrigin};

use crate::job::Stop;

pub use process::kill_all;

/// Setting this environment variable to `1` lets the engine use the
/// `ffmpeg` and `ffprobe` found on `PATH`. Cargo sets it for development
/// commands (`.cargo/config.toml`); a packaged engine never relies on it.
pub const SYSTEM_OPT_IN: &str = "SPLOT_STUDIO_SYSTEM_FFMPEG";

/// The demuxers a source may be read with: `mov` (MP4, MOV, M4V, 3GP),
/// `matroska` (`.mkv`, `.webm`), `mpegts` and `avi`. `FFmpeg` matches each token
/// against every alias of a demuxer's name.
pub const FORMAT_WHITELIST: &str = "mov,matroska,mpegts,avi";

/// How long `-version` may take.
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// The two executables and what they report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tool {
    /// The `ffmpeg` executable.
    pub ffmpeg: PathBuf,
    /// The `ffprobe` executable.
    pub ffprobe: PathBuf,
    /// The version both report, and where they were found.
    pub identity: MediaTool,
}

/// The tool this process uses, found and identified on first use.
///
/// # Errors
/// [`FailureCode::MediaToolMissing`] with the reason.
pub fn tool() -> Result<&'static Tool, Stop> {
    static TOOL: OnceLock<Result<Tool, String>> = OnceLock::new();
    TOOL.get_or_init(discover)
        .as_ref()
        .map_err(|reason| Stop::failed(FailureCode::MediaToolMissing, reason.clone()))
}

/// The identity for the handshake; `None` when there is no usable tool.
#[must_use]
pub fn identity() -> Option<MediaTool> {
    tool().ok().map(|tool| tool.identity.clone())
}

fn executable(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

fn holds_both(dir: &Path) -> bool {
    executable(dir, "ffmpeg").is_file() && executable(dir, "ffprobe").is_file()
}

/// Looks beside the engine executable, then in its `ffmpeg/` directory,
/// then (only with the opt-in) in each `PATH` directory. The first
/// directory holding both tools is used; nothing else is searched.
fn discover() -> Result<Tool, String> {
    let exe =
        std::env::current_exe().map_err(|e| format!("cannot locate the engine executable: {e}"))?;
    let dir = exe.parent().unwrap_or_else(|| Path::new("."));
    for candidate in [dir.to_path_buf(), dir.join("ffmpeg")] {
        if holds_both(&candidate) {
            return identify(&candidate, MediaToolOrigin::Bundled);
        }
    }
    let bundled = format!(
        "no FFmpeg is bundled with this engine (looked for ffmpeg and ffprobe in {} and {})",
        dir.display(),
        dir.join("ffmpeg").display()
    );
    if std::env::var_os(SYSTEM_OPT_IN).is_none_or(|value| value != "1") {
        return Err(bundled);
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .find(|dir| dir.is_absolute() && holds_both(dir))
        .map_or_else(
            || {
                Err(format!(
                    "{bundled}, and {SYSTEM_OPT_IN}=1 is set but no PATH directory holds both"
                ))
            },
            |dir| identify(&dir, MediaToolOrigin::System),
        )
}

fn identify(dir: &Path, origin: MediaToolOrigin) -> Result<Tool, String> {
    let ffmpeg = executable(dir, "ffmpeg");
    let ffprobe = executable(dir, "ffprobe");
    let ffmpeg_version = version_of(&ffmpeg, "ffmpeg")?;
    let ffprobe_version = version_of(&ffprobe, "ffprobe")?;
    if ffmpeg_version != ffprobe_version {
        return Err(format!(
            "{} reports version {ffmpeg_version} but {} reports {ffprobe_version}",
            ffmpeg.display(),
            ffprobe.display()
        ));
    }
    Ok(Tool {
        ffmpeg,
        ffprobe,
        identity: MediaTool {
            version: ffmpeg_version,
            origin,
        },
    })
}

fn version_of(program: &Path, name: &str) -> Result<String, String> {
    let mut command = Command::new(program);
    command.arg("-version");
    let output = process::run_bounded(command, 64 * 1024, VERSION_TIMEOUT)
        .map_err(|e| format!("{} -version failed: {e}", program.display()))?;
    if !output.ended.status.success() {
        return Err(format!(
            "{} -version {}",
            program.display(),
            output.ended.describe()
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    version_line(text.lines().next().unwrap_or_default(), name).ok_or_else(|| {
        format!(
            "{} -version printed no version line for {name}",
            program.display()
        )
    })
}

/// The version from `"<name> version <version> Copyright ..."`.
#[must_use]
pub fn version_line(line: &str, name: &str) -> Option<String> {
    let version = line
        .strip_prefix(name)?
        .strip_prefix(" version ")?
        .split_whitespace()
        .next()?;
    Some(version.chars().take(128).collect())
}

/// The input URL for `path`: the `file:` protocol, so no name (a leading
/// dash, a colon) is read as an option or another protocol. Built from the
/// native string; on Unix any bytes pass through unchanged.
///
/// # Errors
/// [`FailureCode::UnsupportedSource`] on Windows for a path that is not
/// valid Unicode, which the tools cannot receive.
pub fn file_url(path: &Path) -> Result<OsString, Stop> {
    if cfg!(windows) && path.to_str().is_none() {
        return Err(Stop::failed(
            FailureCode::UnsupportedSource,
            "FFmpeg cannot open a path that is not valid Unicode on Windows",
        ));
    }
    let mut url = OsString::from("file:");
    url.push(path.as_os_str());
    Ok(url)
}

/// Which tool a command runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Program {
    Ffprobe,
    Ffmpeg,
}

/// A command with quiet logging and the input restrictions for `path`, up
/// to and including the input itself; `before_input` are further input
/// options. Output options follow.
fn command(
    tool: &Tool,
    program: Program,
    path: &Path,
    before_input: &[&str],
) -> Result<Command, Stop> {
    let mut command = Command::new(match program {
        Program::Ffprobe => &tool.ffprobe,
        Program::Ffmpeg => &tool.ffmpeg,
    });
    command
        .args(["-v", "error", "-hide_banner"])
        .args(["-protocol_whitelist", "file"])
        .args(["-format_whitelist", FORMAT_WHITELIST])
        .args(before_input);
    if program == Program::Ffmpeg {
        command.arg("-i");
    }
    command.arg(file_url(path)?);
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_lines_parse() {
        assert_eq!(
            version_line(
                "ffmpeg version 8.0.1-3ubuntu2 Copyright (c) 2000-2025 the FFmpeg developers",
                "ffmpeg"
            )
            .as_deref(),
            Some("8.0.1-3ubuntu2")
        );
        assert_eq!(
            version_line("ffprobe version n7.1 Copyright", "ffprobe").as_deref(),
            Some("n7.1")
        );
        assert_eq!(version_line("ffprobe version n7.1", "ffmpeg"), None);
        assert_eq!(version_line("", "ffmpeg"), None);
    }

    #[test]
    fn the_url_keeps_odd_names_as_files() {
        let url = file_url(Path::new("-i x:y.mkv"));
        assert_eq!(url.ok(), Some(OsString::from("file:-i x:y.mkv")));
    }
}
