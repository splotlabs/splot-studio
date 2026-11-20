// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! `cargo xtask doctor`: read-only report of what the build needs.

use std::fs;

use crate::run::{capture, root};

/// Checks the pinned toolchain, on Linux the native packages the GUI build
/// script probes, and the `FFmpeg` the engine tests need. Reports everything,
/// then fails if anything required is missing.
pub fn run() -> Result<(), String> {
    let mut problems = 0_usize;
    let mut report = |ok: bool, line: String| {
        println!("{} {line}", if ok { "ok     " } else { "PROBLEM" });
        problems += usize::from(!ok);
    };

    let pinned = pinned_channel()?;
    println!("pinned toolchain (rust-toolchain.toml): {pinned}");
    for tool in ["rustc", "cargo"] {
        match capture(tool, &["--version"]) {
            Ok(out) => {
                let version = out.split_whitespace().nth(1).unwrap_or("?");
                report(
                    version == pinned,
                    format!("{tool} {version} (want {pinned})"),
                );
            }
            Err(e) => report(false, format!("{tool}: {e}")),
        }
    }
    report(
        capture("git", &["--version"]).is_ok(),
        "git on PATH (needed by the header check)".into(),
    );

    if cfg!(target_os = "linux") {
        let has_pkg_config = capture("pkg-config", &["--version"]).is_ok();
        report(has_pkg_config, "pkg-config on PATH".into());
        // The Slint build pulls yeslogic-fontconfig-sys, whose build script
        // requires fontconfig.pc even though the library is dlopen'd at run
        // time. Other GUI libraries (wayland, X11, xkbcommon, GL) are dlopen'd
        // and need no development package to build.
        if has_pkg_config {
            let found = capture("pkg-config", &["--exists", "fontconfig"]).is_ok();
            let hint = "Debian/Ubuntu: apt install libfontconfig-dev; Fedora: fontconfig-devel";
            report(
                found,
                format!("pkg-config module fontconfig (GUI build; {hint})"),
            );
        }
    } else {
        println!("no extra native build prerequisites are checked on this OS");
    }

    ffmpeg(&mut report);

    if problems == 0 {
        Ok(())
    } else {
        Err(format!("doctor found {problems} problem(s)"))
    }
}

fn pinned_channel() -> Result<String, String> {
    let path = root().join("rust-toolchain.toml");
    let text =
        fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    text.lines()
        .find_map(|line| {
            let value = line
                .trim()
                .strip_prefix("channel")?
                .trim_start()
                .strip_prefix('=')?;
            Some(value.trim().trim_matches('"').to_owned())
        })
        .ok_or_else(|| "rust-toolchain.toml has no channel".into())
}

/// The opt-in the engine tests run with (`.cargo/config.toml` sets it for
/// every Cargo command).
const SYSTEM_OPT_IN: &str = "SPLOT_STUDIO_SYSTEM_FFMPEG";

/// Encoders the engine tests make their fixtures with.
const FIXTURE_ENCODERS: [&str; 8] = [
    "libx264",
    "libvpx-vp9",
    "ffv1",
    "prores",
    "rawvideo",
    "flac",
    "png",
    "srt",
];

/// `ffmpeg` and `ffprobe` on `PATH`, at one version, with the fixture
/// encoders. The engine tests fail without them; they never skip.
fn ffmpeg(report: &mut impl FnMut(bool, String)) {
    let install = "Debian/Ubuntu: apt install ffmpeg; macOS: brew install ffmpeg; \
                   Windows: choco install ffmpeg";
    let opt_in = std::env::var(SYSTEM_OPT_IN).ok();
    report(
        opt_in.as_deref() == Some("1"),
        format!(
            "{SYSTEM_OPT_IN}={} (the engine tests use the FFmpeg on PATH; set in .cargo/config.toml)",
            opt_in.as_deref().unwrap_or("<unset>")
        ),
    );
    let mut versions = Vec::new();
    for tool in ["ffmpeg", "ffprobe"] {
        match capture(tool, &["-version"]) {
            Ok(out) => {
                let line = out.lines().next().unwrap_or_default().to_owned();
                let version = line
                    .strip_prefix(tool)
                    .and_then(|rest| rest.strip_prefix(" version "))
                    .and_then(|rest| rest.split_whitespace().next())
                    .unwrap_or("?")
                    .to_owned();
                report(
                    true,
                    format!(
                        "{tool} {version} on PATH (engine tests; development only, never shipped)"
                    ),
                );
                versions.push(version);
            }
            Err(e) => report(
                false,
                format!("{tool} on PATH, needed by the engine tests ({install}): {e}"),
            ),
        }
    }
    if let [ffmpeg, ffprobe] = versions.as_slice() {
        report(
            ffmpeg == ffprobe,
            format!("ffmpeg and ffprobe report the same version ({ffmpeg}, {ffprobe})"),
        );
        match capture("ffmpeg", &["-hide_banner", "-encoders"]) {
            Ok(list) => {
                for encoder in FIXTURE_ENCODERS {
                    let found = list
                        .lines()
                        .any(|line| line.split_whitespace().nth(1) == Some(encoder));
                    report(found, format!("ffmpeg encoder {encoder} (test fixtures)"));
                }
            }
            Err(e) => report(false, format!("ffmpeg -encoders: {e}")),
        }
    }
}
