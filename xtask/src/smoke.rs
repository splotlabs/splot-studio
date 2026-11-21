// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! `cargo xtask smoke [ARCHIVE]`: the package, unpacked and run the way a
//! user's machine would run it.
//!
//! The archive is checked against its `.sha256`, unpacked into a new
//! temporary directory with a non-ASCII name, and checked against
//! `BUILD.json`. Every program then runs with an emptied environment: only
//! the operating system's own directories on `PATH`, a fresh home, no
//! `SPLOT_STUDIO_SYSTEM_FFMPEG`, no Cargo or Rust variables and no library
//! path overrides. The engine must report the bundled `FFmpeg` and this
//! build's protocol, encode an 8-bit and a 10-bit Y4M and an MP4 to IVF and
//! OBU and verify each, and refuse the MP4 until its audio track is
//! acknowledged. The app must report its identity without a display. The
//! start-up libraries of all four executables must be the system's own.
//!
//! A placeholder package (`--ffmpeg-dir`) still runs every step, but its
//! `ffmpeg`/`ffprobe` libraries are reported, not judged.
//!
//! What it does not prove: a clean machine (this one has development tools
//! installed even though the programs cannot see them), a window opening,
//! or another processor or OS version.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::inspect;
use crate::package::{self, Layout, Os, Target};
use crate::run::root;
use crate::sha256;

/// Runs the smoke test on `archive`, or on the one package in
/// `target/package/` for the host target.
pub fn run(archive: Option<PathBuf>) -> Result<(), String> {
    let target = package::host()?;
    let archive = match archive {
        Some(path) => path,
        None => find_archive(target)?,
    };
    println!("\n==> smoke test of {}", archive.display());
    check_checksum(&archive)?;
    let base = std::env::temp_dir().join(format!("splot-studio-smoke-ü-{}", std::process::id()));
    if base.exists() {
        fs::remove_dir_all(&base).map_err(|e| format!("cannot clear {}: {e}", base.display()))?;
    }
    for dir in ["unpacked", "home/tmp", "media ä"] {
        fs::create_dir_all(base.join(dir))
            .map_err(|e| format!("cannot create {}: {e}", base.join(dir).display()))?;
    }
    let result = smoke(target, &archive, &base);
    match &result {
        Ok(()) => {
            let _ = fs::remove_dir_all(&base);
            println!("\nxtask: smoke test passed");
        }
        Err(_) => println!("\nsmoke test files kept in {}", base.display()),
    }
    result
}

fn smoke(target: &Target, archive: &Path, base: &Path) -> Result<(), String> {
    let unpacked = base.join("unpacked");
    unpack(target, archive, &unpacked)?;
    let name = archive
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let folder = unpacked.join(
        name.strip_suffix(&format!(".{}", target.archive_extension()))
            .ok_or("unexpected archive name")?,
    );
    let build = check_files(&folder)?;
    let layout = Layout::of(target, &folder);
    let app = layout.bin.join(target.exe("splot-studio"));
    let engine = layout.bin.join(target.exe("splot-studio-engine"));
    let env = CleanEnv::new(target.os, &base.join("home"));

    identity(target, &env, [&app, &engine], &build, base)?;
    encodes(&env, &engine, base)?;
    let placeholder = build.pointer("/ffmpeg/placeholder") != Some(&Value::Bool(false));
    binaries(target, &layout, &folder, placeholder)
}

/// Both executables report this build's version, commit and protocol, the
/// engine the bundled `FFmpeg`, and the app a processor that passes.
fn identity(
    target: &Target,
    env: &CleanEnv,
    [app, engine]: [&Path; 2],
    build: &Value,
    base: &Path,
) -> Result<(), String> {
    let field = |key: &str| build.get(key).and_then(Value::as_str).unwrap_or_default();
    let version = field("version").to_owned();
    let commit = format!(
        "{}{}",
        field("commit"),
        if build.get("dirty").and_then(Value::as_bool) == Some(true) {
            "-dirty"
        } else {
            ""
        }
    );
    let protocol = build
        .get("protocol_version")
        .and_then(Value::as_u64)
        .ok_or("BUILD.json has no protocol_version")?;
    let ffmpeg_version = build
        .pointer("/ffmpeg/version")
        .and_then(Value::as_str)
        .ok_or("BUILD.json has no ffmpeg version")?
        .to_owned();
    let shown = env.run(engine, &["--version"], base)?;
    expect(
        &shown,
        &format!("splot-studio-engine {version}\n"),
        "engine version",
    )?;
    expect(
        &shown,
        &format!("protocol version: {protocol}\n"),
        "engine protocol",
    )?;
    expect(
        &shown,
        &format!("media files: FFmpeg {ffmpeg_version} (Bundled, "),
        "engine uses the bundled FFmpeg",
    )?;

    let shown = env.run(app, &["--version"], base)?;
    expect(&shown, &format!("splot-studio {version}\n"), "app version")?;
    expect(&shown, &format!("commit: {commit}\n"), "app commit")?;
    expect(
        &shown,
        &format!("protocol version: {protocol}\n"),
        "app protocol",
    )?;
    if let Some(floor) = target.engine_cpu {
        expect(
            &shown,
            &format!("engine CPU floor: {floor}; checked: "),
            "app CPU floor",
        )?;
        expect(
            &shown,
            "this CPU has all of them",
            "this CPU passes the floor",
        )?;
    }

    Ok(())
}

/// Encodes through the packaged engine: Y4M at both depths and an MP4 to
/// both outputs, after the MP4 is refused without an acknowledgement.
fn encodes(env: &CleanEnv, engine: &Path, base: &Path) -> Result<(), String> {
    let media = base.join("media ä");
    let y4m8 = media.join("clip 8-bit ü.y4m");
    let y4m10 = media.join("clip 10-bit ü.y4m");
    write_y4m(&y4m8, false)?;
    write_y4m(&y4m10, true)?;
    let mp4 = media.join("clip ü.mp4");
    fs::copy(root().join("packaging/smoke/tiny-h264-aac.mp4"), &mp4)
        .map_err(|e| format!("cannot copy the MP4 fixture: {e}"))?;

    let refused = env.output(
        engine,
        &[
            "encode".as_ref(),
            mp4.as_os_str(),
            media.join("refused.ivf").as_os_str(),
        ],
        base,
    )?;
    if refused.0 || !refused.1.contains("failed OmissionsNotAcknowledged") {
        return Err(format!(
            "the MP4 with an audio track was not refused without an acknowledgement:\n{}",
            refused.1
        ));
    }
    println!("ok: MP4 with audio refused until the omission is acknowledged");
    for (source, output, frames, acknowledge) in [
        (&y4m8, "ivf", 4, false),
        (&y4m10, "obu", 4, false),
        (&mp4, "ivf", 6, true),
        (&mp4, "obu", 6, true),
    ] {
        let stem = source
            .file_stem()
            .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
        let destination = media.join(format!("{stem} out.{output}"));
        encode(
            env,
            engine,
            base,
            (source, &destination),
            output,
            frames,
            acknowledge,
        )?;
    }
    let leftovers: Vec<String> = fs::read_dir(&media)
        .map_err(|e| format!("cannot list {}: {e}", media.display()))?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with('.') || n.ends_with(".partial") || n.starts_with("refused"))
        .collect();
    if !leftovers.is_empty() {
        return Err(format!("files left behind: {leftovers:?}"));
    }

    Ok(())
}

/// The start-up libraries of all four executables, the Windows subsystems
/// and the macOS signature.
fn binaries(
    target: &Target,
    layout: &Layout,
    folder: &Path,
    placeholder: bool,
) -> Result<(), String> {
    for (dir, name, extra) in [
        (&layout.bin, "splot-studio", &["libfontconfig.so.1"][..]),
        (&layout.bin, "splot-studio-engine", &[]),
        (&layout.tools, "ffmpeg", &[]),
        (&layout.tools, "ffprobe", &[]),
    ] {
        let linked = match inspect::linked(target.os, &dir.join(target.exe(name)), extra) {
            Ok(linked) => linked,
            Err(problem) if placeholder && matches!(name, "ffmpeg" | "ffprobe") => {
                println!("NOT CHECKED (placeholder FFmpeg): {problem}");
                continue;
            }
            Err(problem) => return Err(problem),
        };
        println!(
            "ok: {name} loads only system libraries: {}{}",
            linked.libraries.join(", "),
            linked
                .glibc
                .map_or(String::new(), |v| format!(" (glibc {v} or newer)"))
        );
    }
    match target.os {
        Os::Windows => {
            for (name, want, label) in [
                ("splot-studio", 2, "window (no console)"),
                ("splot-studio-engine", 3, "console"),
            ] {
                let path = layout.bin.join(target.exe(name));
                let bytes =
                    fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
                let got = inspect::pe_subsystem(&bytes);
                if got != Some(want) {
                    return Err(format!(
                        "{name}: PE subsystem {got:?}, want {want} ({label})"
                    ));
                }
                println!("ok: {name} is a {label} program");
            }
        }
        Os::Macos => {
            let app = folder.join(package::MAC_APP);
            let status = Command::new("codesign")
                .args(["--verify", "--deep", "--strict"])
                .arg(&app)
                .status()
                .map_err(|e| format!("cannot run codesign: {e}"))?;
            if !status.success() {
                return Err(format!("codesign --verify failed for {}", app.display()));
            }
            println!("ok: the bundle's ad-hoc signature verifies");
        }
        Os::Linux => {}
    }
    Ok(())
}

/// One encode through the packaged engine, verified.
fn encode(
    env: &CleanEnv,
    engine: &Path,
    cwd: &Path,
    (source, destination): (&Path, &Path),
    output: &str,
    frames: u32,
    acknowledge: bool,
) -> Result<(), String> {
    let mut args = vec![
        "encode".as_ref(),
        source.as_os_str(),
        destination.as_os_str(),
        "--output".as_ref(),
        output.as_ref(),
        "--preset".as_ref(),
        "10".as_ref(),
        "--crf".as_ref(),
        "40".as_ref(),
    ];
    if acknowledge {
        args.push("--acknowledge-omissions".as_ref());
    }
    let (ok, text) = env.output(engine, &args, cwd)?;
    let what = format!(
        "{} -> {output}",
        source.file_name().unwrap_or_default().to_string_lossy()
    );
    if !ok || text.contains("FAILED") {
        return Err(format!("{what} failed:\n{text}"));
    }
    if source.extension().is_some_and(|e| e == "mp4") {
        expect(&text, "(Bundled)", "MP4 decoded by the bundled FFmpeg")?;
    }
    for check in ["check Structure: passed", "check SplotDecode: passed"] {
        expect(&text, check, &what)?;
    }
    let bytes: u64 = text
        .lines()
        .find_map(|l| {
            let rest = l.split("completed frames=").nth(1)?;
            let (got, rest) = rest.split_once(' ')?;
            (got == frames.to_string()).then_some(())?;
            rest.strip_prefix("bytes=")?.split(' ').next()?.parse().ok()
        })
        .ok_or_else(|| format!("{what}: no completion with {frames} frames:\n{text}"))?;
    let written = fs::read(destination)
        .map_err(|e| format!("{what}: cannot read {}: {e}", destination.display()))?;
    if written.len() as u64 != bytes || written.is_empty() {
        return Err(format!(
            "{what}: the file has {} bytes, the engine reported {bytes}",
            written.len()
        ));
    }
    if output == "ivf" && !written.starts_with(b"DKIF") {
        return Err(format!("{what}: not an IVF file"));
    }
    println!("ok: {what}: {frames} frames, {bytes} bytes, structure and Splot decode passed");
    Ok(())
}

fn expect(text: &str, wanted: &str, what: &str) -> Result<(), String> {
    if text.contains(wanted) {
        Ok(())
    } else {
        Err(format!("{what}: expected {wanted:?} in:\n{text}"))
    }
}

fn find_archive(target: &Target) -> Result<PathBuf, String> {
    let dir = root().join("target/package");
    let suffix = format!("-{}.{}", target.name, target.archive_extension());
    let mut found: Vec<PathBuf> = fs::read_dir(&dir)
        .map_err(|e| {
            format!(
                "cannot list {}: {e}; run cargo xtask package",
                dir.display()
            )
        })?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .is_some_and(|n| n.starts_with("splot-studio-") && n.ends_with(&suffix))
        })
        .collect();
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => Err(format!(
            "no package in {}; run cargo xtask package",
            dir.display()
        )),
        _ => Err(format!(
            "several packages in {}; name one: cargo xtask smoke <archive>",
            dir.display()
        )),
    }
}

fn check_checksum(archive: &Path) -> Result<(), String> {
    let mut sums = archive.as_os_str().to_owned();
    sums.push(".sha256");
    let sums = PathBuf::from(sums);
    let text =
        fs::read_to_string(&sums).map_err(|e| format!("cannot read {}: {e}", sums.display()))?;
    let wanted = text.split_whitespace().next().unwrap_or_default();
    let got = sha256::file(archive)?;
    if got == wanted {
        println!("ok: SHA-256 matches {}", sums.display());
        Ok(())
    } else {
        Err(format!(
            "{} has SHA-256 {got}, {} says {wanted}",
            archive.display(),
            sums.display()
        ))
    }
}

fn unpack(target: &Target, archive: &Path, into: &Path) -> Result<(), String> {
    let mut command = match target.os {
        Os::Linux => {
            let mut c = Command::new("tar");
            c.arg("-xzf").arg(archive).arg("-C").arg(into);
            c
        }
        Os::Macos => {
            let mut c = Command::new("ditto");
            c.args(["-x", "-k"]).arg(archive).arg(into);
            c
        }
        Os::Windows => {
            let mut c = Command::new(package::system_tar());
            c.arg("-xf").arg(archive).arg("-C").arg(into);
            c
        }
    };
    let status = command
        .status()
        .map_err(|e| format!("cannot start the unpacker: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("unpacking exited with {status}"))
    }
}

/// Every file `BUILD.json` lists has its recorded hash, and no other file
/// is present.
fn check_files(folder: &Path) -> Result<Value, String> {
    let path = folder.join("BUILD.json");
    let text =
        fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let build: Value =
        serde_json::from_str(&text).map_err(|e| format!("BUILD.json is not JSON: {e}"))?;
    let listed = build
        .get("files")
        .and_then(Value::as_object)
        .ok_or("BUILD.json lists no files")?;
    let mut present = Vec::new();
    let mut stack = vec![folder.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in
            fs::read_dir(&dir).map_err(|e| format!("cannot list {}: {e}", dir.display()))?
        {
            let entry = entry.map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
            if entry.path().is_dir() {
                stack.push(entry.path());
            } else {
                present.push(package::relative(folder, &entry.path()));
            }
        }
    }
    let mut problems = Vec::new();
    for file in &present {
        if file == "BUILD.json" {
            continue;
        }
        match listed.get(file).and_then(Value::as_str) {
            None => problems.push(format!("{file} is not in BUILD.json")),
            Some(hash) if sha256::file(&folder.join(file))? != hash => {
                problems.push(format!("{file} does not match its BUILD.json hash"));
            }
            Some(_) => {}
        }
    }
    for file in listed.keys() {
        if !present.contains(file) {
            problems.push(format!("{file} is missing"));
        }
    }
    if problems.is_empty() {
        println!("ok: {} files match BUILD.json", listed.len());
        Ok(build)
    } else {
        Err(problems.join("; "))
    }
}

/// An environment with nothing from the developer's: the OS's own `PATH`
/// directories, a fresh home and temporary directory, nothing else.
struct CleanEnv {
    vars: Vec<(String, String)>,
}

impl CleanEnv {
    fn new(os: Os, home: &Path) -> Self {
        let home_text = home.display().to_string();
        let tmp = home.join("tmp").display().to_string();
        let mut vars = vec![("HOME".to_owned(), home_text.clone())];
        match os {
            Os::Linux => {
                vars.push(("PATH".into(), "/usr/bin:/bin".into()));
                vars.push(("TMPDIR".into(), tmp));
                vars.push(("LANG".into(), "C.UTF-8".into()));
            }
            Os::Macos => {
                vars.push(("PATH".into(), "/usr/bin:/bin:/usr/sbin:/sbin".into()));
                vars.push(("TMPDIR".into(), tmp));
            }
            Os::Windows => {
                let system = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
                vars.push((
                    "PATH".into(),
                    format!("{system}\\System32;{system};{system}\\System32\\Wbem"),
                ));
                for key in ["SystemRoot", "SystemDrive", "windir", "ComSpec", "PATHEXT"] {
                    if let Ok(value) = std::env::var(key) {
                        vars.push((key.to_owned(), value));
                    }
                }
                vars.push(("TEMP".into(), tmp.clone()));
                vars.push(("TMP".into(), tmp));
                vars.push(("USERPROFILE".into(), home_text.clone()));
                vars.push((
                    "LOCALAPPDATA".into(),
                    format!("{home_text}\\AppData\\Local"),
                ));
                vars.push(("APPDATA".into(), format!("{home_text}\\AppData\\Roaming")));
            }
        }
        Self { vars }
    }

    /// Runs `program` and returns (success, stdout and stderr).
    fn output(
        &self,
        program: &Path,
        args: &[&std::ffi::OsStr],
        cwd: &Path,
    ) -> Result<(bool, String), String> {
        let output = Command::new(program)
            .args(args)
            .env_clear()
            .envs(self.vars.iter().map(|(k, v)| (k, v)))
            .current_dir(cwd)
            .output()
            .map_err(|e| format!("cannot start {}: {e}", program.display()))?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        Ok((output.status.success(), text))
    }

    /// Runs `program` and returns its output; failure is an error.
    fn run(&self, program: &Path, args: &[&str], cwd: &Path) -> Result<String, String> {
        let args: Vec<&std::ffi::OsStr> = args.iter().map(std::convert::AsRef::as_ref).collect();
        let (ok, text) = self.output(program, &args, cwd)?;
        print!("{text}");
        if ok {
            Ok(text)
        } else {
            Err(format!("{} failed:\n{text}", program.display()))
        }
    }
}

/// A 64x64, four-frame, 30 fps 4:2:0 Y4M with a moving gradient in limited
/// range, 8-bit or 10-bit (little-endian samples).
fn write_y4m(path: &Path, ten_bit: bool) -> Result<(), String> {
    const SIZE: u16 = 64;
    let colour = if ten_bit { "420p10" } else { "420jpeg" };
    let mut data = format!("YUV4MPEG2 W{SIZE} H{SIZE} F30:1 Ip A1:1 C{colour}\n").into_bytes();
    let sample = |data: &mut Vec<u8>, value: u16| {
        if ten_bit {
            data.extend_from_slice(&value.to_le_bytes());
        } else {
            data.push(u8::try_from(value).unwrap_or(u8::MAX));
        }
    };
    for frame in 0..4_u16 {
        data.extend_from_slice(b"FRAME\n");
        for y in 0..SIZE {
            for x in 0..SIZE {
                let ramp = (x + 2 * y + 8 * frame) % 200;
                sample(&mut data, if ten_bit { 64 + ramp * 4 } else { 16 + ramp });
            }
        }
        for _ in 0..2 * (SIZE / 2) * (SIZE / 2) {
            sample(&mut data, if ten_bit { 512 } else { 128 });
        }
    }
    let mut file =
        fs::File::create_new(path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    file.write_all(&data)
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}
