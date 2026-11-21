// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! `cargo xtask package`: an unsigned test package for the host target.
//!
//! Builds the app portable and the engine at the release CPU floor, each in
//! its own target directory so neither build's flags leak into the other,
//! stages the folder (a `.app` bundle on macOS), writes the notices and
//! `BUILD.json`, and archives it under `target/package/` with a SHA-256 file.
//!
//! The `FFmpeg` pair comes from `native/ffmpeg/out/<target>/` (built from
//! source by `native/ffmpeg/build.sh`). `--ffmpeg-dir` takes any other
//! directory holding a pair, for testing only: such a package is named and
//! recorded as a placeholder and must never be distributed.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

use crate::inspect::{self, Linked};
use crate::notices;
use crate::run::{capture, root};
use crate::sha256;

/// The operating system of a package target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    /// Linux, glibc.
    Linux,
    /// macOS.
    Macos,
    /// Windows, MSVC.
    Windows,
}

/// A target a package can be made for.
#[derive(Debug)]
pub struct Target {
    /// The Rust target triple.
    pub triple: &'static str,
    /// The name used by `native/ffmpeg/build.sh` and in archive names.
    pub name: &'static str,
    /// Its operating system.
    pub os: Os,
    /// The engine's `-C target-cpu`, `None` for the target's default.
    pub engine_cpu: Option<&'static str>,
}

impl Target {
    /// `name` plus `.exe` on Windows.
    pub fn exe(&self, name: &str) -> String {
        match self.os {
            Os::Windows => format!("{name}.exe"),
            Os::Linux | Os::Macos => name.to_owned(),
        }
    }

    /// The archive file extension.
    pub const fn archive_extension(&self) -> &'static str {
        match self.os {
            Os::Linux => "tar.gz",
            Os::Macos | Os::Windows => "zip",
        }
    }
}

/// The three release lanes. x86-64 engines require x86-64-v3 (owner
/// decision, 2026-11-21); the app checks it before starting one.
const TARGETS: [Target; 3] = [
    Target {
        triple: "x86_64-unknown-linux-gnu",
        name: "linux-x86_64",
        os: Os::Linux,
        engine_cpu: Some("x86-64-v3"),
    },
    Target {
        triple: "x86_64-pc-windows-msvc",
        name: "windows-x86_64",
        os: Os::Windows,
        engine_cpu: Some("x86-64-v3"),
    },
    Target {
        triple: "aarch64-apple-darwin",
        name: "macos-arm64",
        os: Os::Macos,
        engine_cpu: None,
    },
];

/// The oldest macOS the bundle declares and every executable is built for:
/// the oldest release Apple still ships security updates for in 2026-11 and
/// the version of the CI runner.
pub const MIN_MACOS: &str = "15.0";

/// The bundle folder inside a macOS package.
pub const MAC_APP: &str = "Splot Studio.app";

/// The target of the toolchain running this, which is what gets packaged.
pub fn host() -> Result<&'static Target, String> {
    let info = capture("rustc", &["-vV"])?;
    let triple = info
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .ok_or("rustc -vV printed no host")?;
    TARGETS
        .iter()
        .find(|t| t.triple == triple)
        .ok_or_else(|| format!("no package is defined for {triple}"))
}

/// Where the executables and the bundled tools sit inside a staged folder.
pub struct Layout {
    /// The app and engine executables.
    pub bin: PathBuf,
    /// `ffmpeg` and `ffprobe`.
    pub tools: PathBuf,
    /// Licences, notices and the `FFmpeg` manifest.
    pub resources: PathBuf,
}

impl Layout {
    /// The layout under the package folder `root`.
    pub fn of(target: &Target, root: &Path) -> Self {
        match target.os {
            Os::Macos => {
                let contents = root.join(MAC_APP).join("Contents");
                Self {
                    bin: contents.join("MacOS"),
                    // Beside the engine: Contents/MacOS must hold code only.
                    tools: contents.join("MacOS"),
                    resources: contents.join("Resources"),
                }
            }
            Os::Linux | Os::Windows => Self {
                bin: root.to_path_buf(),
                tools: root.join("ffmpeg"),
                resources: root.to_path_buf(),
            },
        }
    }
}

/// The finished package.
pub struct Package {
    /// The archive.
    pub archive: PathBuf,
}

/// Builds, stages and archives the package for the host target.
pub fn run(ffmpeg_dir: Option<PathBuf>) -> Result<Package, String> {
    let target = host()?;
    let root = root();
    let out = root.join("target").join("package");
    let version = env!("CARGO_PKG_VERSION");
    let commit = capture("git", &["rev-parse", "HEAD"])?.trim().to_owned();
    let dirty = !capture("git", &["status", "--porcelain"])?
        .trim()
        .is_empty();
    let stamp = capture("git", &["log", "-1", "--format=%ct"])?
        .trim()
        .to_owned();
    let rustc = capture("rustc", &["--version"])?.trim().to_owned();
    let protocol = protocol_version(&root)?;
    let placeholder = ffmpeg_dir.is_some();
    let tools_from = ffmpeg_dir.unwrap_or_else(|| root.join("native/ffmpeg/out").join(target.name));
    let ffmpeg = Ffmpeg::inspect(target, &tools_from, placeholder)?;

    let build_commit = if dirty {
        format!("{commit}-dirty")
    } else {
        commit.clone()
    };
    let app = build(target, "studio-app", "splot-studio", None, &build_commit)?;
    let engine = build(
        target,
        "studio-engine",
        "splot-studio-engine",
        target.engine_cpu,
        &build_commit,
    )?;
    check_cpu_list(target, &app)?;

    let name = format!(
        "splot-studio-{version}-unsigned-test{}-{}",
        if placeholder {
            "-placeholder-ffmpeg"
        } else {
            ""
        },
        target.name
    );
    let stage = out.join(&name);
    let (layout, written, signing) = stage_folder(target, &stage, [&app, &engine], &ffmpeg)?;
    let (executables, platform) = executables(target, &stage, &layout, placeholder)?;
    let files = hashes(&stage)?;
    let build = json!({
        "name": "Splot Studio",
        "version": version,
        "label": "unsigned test build",
        "commit": commit,
        "dirty": dirty,
        "commit_time_unix": stamp,
        "target": target.triple,
        "package_target": target.name,
        "rustc": rustc,
        "profile": "release",
        "protocol_version": protocol,
        "signing": signing,
        "executables": executables,
        "platform": platform,
        "ffmpeg": ffmpeg.record(&root),
        "rust_crates": written.crates,
        "rust_crates_without_licence_file": written.without_text,
        "files": files,
    });
    let text = serde_json::to_string_pretty(&build).map_err(|e| e.to_string())?;
    write(&stage.join("BUILD.json"), &format!("{text}\n"))?;

    let archive = archive(target, &out, &name, &stamp)?;
    if placeholder {
        println!(
            "WARNING: placeholder FFmpeg from {}; this package is for testing only and must \
             not be distributed",
            tools_from.display()
        );
    }
    Ok(Package { archive })
}

/// Stages the package folder: executables, the `FFmpeg` pair, licences,
/// notices, the README and, on macOS, the signed bundle.
fn stage_folder(
    target: &Target,
    stage: &Path,
    [app, engine]: [&Path; 2],
    ffmpeg: &Ffmpeg,
) -> Result<(Layout, notices::Written, &'static str), String> {
    let root = root();
    let version = env!("CARGO_PKG_VERSION");
    if stage.exists() {
        fs::remove_dir_all(stage).map_err(|e| format!("cannot clear {}: {e}", stage.display()))?;
    }
    let layout = Layout::of(target, stage);
    for dir in [&layout.bin, &layout.tools, &layout.resources] {
        mkdir(dir)?;
    }
    copy(app, &layout.bin.join(target.exe("splot-studio")))?;
    copy(engine, &layout.bin.join(target.exe("splot-studio-engine")))?;
    ffmpeg.stage(target, &layout)?;
    copy(
        &root.join("LICENSE.md"),
        &layout.resources.join("LICENSE.md"),
    )?;
    mkdir(&layout.resources.join("licenses"))?;
    let written = notices::write(
        target.triple,
        &root.join("THIRD-PARTY-NOTICES.md"),
        &layout.resources.join("THIRD-PARTY-NOTICES.md"),
        &layout.resources.join("licenses/RUST-CRATES.txt"),
    )?;
    println!(
        "notices: {} third-party crates; {} ship no licence file",
        written.crates,
        written.without_text.len()
    );
    let signing = match target.os {
        Os::Macos => {
            bundle(stage, version)?;
            "unsigned: ad-hoc signature only (codesign --sign -); no Developer ID, not notarized"
        }
        Os::Linux | Os::Windows => "unsigned",
    };
    let readme = fs::read_to_string(root.join("packaging/README.txt"))
        .map_err(|e| format!("cannot read packaging/README.txt: {e}"))?
        .replace("@VERSION@", version)
        .replace("@TARGET@", target.name)
        .replace(
            "@SIGNING_NOTE@",
            if target.os == Os::Macos {
                " (only an ad-hoc signature) and not notarized"
            } else {
                ""
            },
        )
        .replace("@LAYOUT@", layout_text(target));
    if let Some(left) = placeholder_in(&readme) {
        return Err(format!("packaging/README.txt: {left} was not replaced"));
    }
    write(&stage.join("README.txt"), &readme)?;

    Ok((layout, written, signing))
}

/// What each executable needs from the machine, and the platform floor
/// that results, for `BUILD.json`. Fails on a library outside the OS.
fn executables(
    target: &Target,
    stage: &Path,
    layout: &Layout,
    placeholder: bool,
) -> Result<(Value, Value), String> {
    let mut executables = BTreeMap::new();
    let mut glibc = Vec::new();
    let mut minos = Vec::new();
    for (name, dir, baseline, extra) in [
        (
            "splot-studio",
            &layout.bin,
            app_baseline(target),
            &["libfontconfig.so.1"][..],
        ),
        (
            "splot-studio-engine",
            &layout.bin,
            engine_baseline(target),
            &[],
        ),
        (
            "ffmpeg",
            &layout.tools,
            "see the FFmpeg MANIFEST.json".to_owned(),
            &[],
        ),
        (
            "ffprobe",
            &layout.tools,
            "see the FFmpeg MANIFEST.json".to_owned(),
            &[],
        ),
    ] {
        let path = dir.join(target.exe(name));
        let tool = matches!(name, "ffmpeg" | "ffprobe");
        let Linked {
            libraries,
            glibc: needs_glibc,
            minos: built_for,
        } = match inspect::linked(target.os, &path, extra) {
            Ok(linked) => linked,
            Err(problem) if tool && placeholder => {
                println!("WARNING (placeholder FFmpeg, not checked further): {problem}");
                Linked::default()
            }
            Err(problem) => return Err(problem),
        };
        glibc.extend(needs_glibc.clone());
        minos.extend(built_for.clone());
        executables.insert(
            name,
            json!({
                "path": relative(stage, &path),
                "cpu_baseline": baseline,
                "libraries": libraries,
                "glibc_required": needs_glibc,
                "macos_minos": built_for,
            }),
        );
    }
    let platform = match target.os {
        Os::Linux => json!({ "glibc_required": newest(&glibc) }),
        Os::Macos => {
            let built = newest(&minos);
            if let Some(built) = &built
                && version_key(built) > version_key(MIN_MACOS)
            {
                return Err(format!(
                    "an executable is built for macOS {built}, newer than the declared \
                     minimum {MIN_MACOS}"
                ));
            }
            json!({ "minimum_system_version": MIN_MACOS, "newest_minos": built })
        }
        Os::Windows => json!({ "c_runtime": "static (+crt-static)" }),
    };
    Ok((json!(executables), platform))
}

/// Archives the staged folder and writes its `.sha256` file.
fn archive(target: &Target, out: &Path, name: &str, stamp: &str) -> Result<PathBuf, String> {
    let archive = out.join(format!("{name}.{}", target.archive_extension()));
    archive_folder(target, out, name, &archive, stamp)?;
    let digest = sha256::file(&archive)?;
    let sums = out.join(format!("{name}.{}.sha256", target.archive_extension()));
    write(
        &sums,
        &format!(
            "{digest}  {}\n",
            archive.file_name().unwrap_or_default().to_string_lossy()
        ),
    )?;
    let size = fs::metadata(&archive).map_or(0, |m| m.len());
    println!(
        "\npackage: {} ({size} bytes)\nsha256:  {digest}",
        archive.display()
    );
    Ok(archive)
}

/// `PROTOCOL_VERSION` from the domain crate's source; the smoke test then
/// checks that both executables report it.
fn protocol_version(root: &Path) -> Result<u32, String> {
    let path = root.join("crates/studio-domain/src/protocol.rs");
    let text =
        fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    text.lines()
        .find_map(|line| {
            line.strip_prefix("pub const PROTOCOL_VERSION: u32 = ")?
                .strip_suffix(';')?
                .parse()
                .ok()
        })
        .ok_or_else(|| format!("no PROTOCOL_VERSION in {}", path.display()))
}

/// `cargo build --release` of one binary for `target`, with its own target
/// directory and exactly the flags given: an inherited `RUSTFLAGS` or a
/// `build.rustflags` setting does not apply (`CARGO_ENCODED_RUSTFLAGS` wins
/// over both). With `--target` given, the flags reach target code only, not
/// build scripts. Paths are remapped so the builder's home is not embedded.
fn build(
    target: &Target,
    package: &str,
    bin: &str,
    cpu: Option<&str>,
    commit: &str,
) -> Result<PathBuf, String> {
    let root = root();
    let flavour = cpu.unwrap_or("portable");
    let dir = root
        .join("target/package/cargo")
        .join(format!("{}-{flavour}", target.triple));
    let mut flags = vec![
        format!("--remap-path-prefix={}=/splot-studio", root.display()),
        format!("--remap-path-prefix={}=/cargo", cargo_home().display()),
    ];
    if let Some(cpu) = cpu {
        flags.push(format!("-Ctarget-cpu={cpu}"));
    }
    if target.os == Os::Windows {
        // No vcruntime140.dll: it is not on a clean Windows machine.
        flags.push("-Ctarget-feature=+crt-static".to_owned());
    }
    let args = [
        "build",
        "--release",
        "--locked",
        "-p",
        package,
        "--bin",
        bin,
        "--target",
        target.triple,
    ];
    println!(
        "\n==> cargo {} --target-dir {} (rustflags: {})",
        args.join(" "),
        dir.display(),
        flags.join(" ")
    );
    let mut command = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    command
        .args(args)
        .arg("--target-dir")
        .arg(&dir)
        .current_dir(&root)
        .env_remove("RUSTFLAGS")
        .env("CARGO_ENCODED_RUSTFLAGS", flags.join("\u{1f}"))
        .env("SPLOT_STUDIO_BUILD_COMMIT", commit);
    match target.engine_cpu {
        Some(floor) => command.env("SPLOT_STUDIO_ENGINE_CPU", floor),
        None => command.env_remove("SPLOT_STUDIO_ENGINE_CPU"),
    };
    if target.os == Os::Macos {
        command.env("MACOSX_DEPLOYMENT_TARGET", MIN_MACOS);
    }
    let status = command
        .status()
        .map_err(|e| format!("cannot start cargo: {e}"))?;
    if !status.success() {
        return Err(format!("building {bin} exited with {status}"));
    }
    Ok(dir
        .join(target.triple)
        .join("release")
        .join(target.exe(bin)))
}

fn cargo_home() -> PathBuf {
    std::env::var_os("CARGO_HOME").map_or_else(
        || {
            let home = std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .unwrap_or_default();
            Path::new(&home).join(".cargo")
        },
        PathBuf::from,
    )
}

/// The app's list of checked features must be rustc's `x86-64-v3` set.
fn check_cpu_list(target: &Target, app: &Path) -> Result<(), String> {
    let Some(cpu) = target.engine_cpu else {
        return Ok(());
    };
    let text = capture(app.as_os_str(), &["--version"])?;
    let prefix = format!("engine CPU floor: {cpu}; checked: ");
    let checked: Vec<&str> = text
        .lines()
        .find_map(|line| line.strip_prefix(prefix.as_str()))
        .and_then(|rest| rest.split(';').next())
        .ok_or_else(|| format!("the app does not report checking {cpu}:\n{text}"))?
        .split(", ")
        .collect();
    let cfg = capture(
        "rustc",
        &[
            "--print",
            "cfg",
            "--target",
            target.triple,
            &format!("-Ctarget-cpu={cpu}"),
        ],
    )?;
    let mut wanted: Vec<&str> = cfg
        .lines()
        .filter_map(|l| l.strip_prefix("target_feature=\"")?.strip_suffix('"'))
        .collect();
    wanted.sort_unstable();
    let mut got = checked.clone();
    got.sort_unstable();
    if got == wanted {
        println!(
            "ok: the app checks exactly rustc's {cpu} features: {}",
            checked.join(", ")
        );
        Ok(())
    } else {
        Err(format!(
            "the app checks {got:?} but rustc's {cpu} enables {wanted:?}; update \
             crates/studio-app/src/cpu.rs"
        ))
    }
}

fn app_baseline(target: &Target) -> String {
    match target.os {
        Os::Macos => "aarch64-apple-darwin default (apple-m1)".to_owned(),
        Os::Linux | Os::Windows => format!(
            "x86-64 (portable); checks for {} before starting the engine",
            target.engine_cpu.unwrap_or("nothing")
        ),
    }
}

fn engine_baseline(target: &Target) -> String {
    target.engine_cpu.map_or_else(
        || "aarch64-apple-darwin default (apple-m1)".to_owned(),
        |cpu| format!("{cpu} (-C target-cpu={cpu})"),
    )
}

/// The bundled tools and what they say about themselves.
struct Ffmpeg {
    dir: PathBuf,
    placeholder: bool,
    version: String,
    configuration: String,
    manifest: Option<PathBuf>,
    licenses: Option<PathBuf>,
}

impl Ffmpeg {
    fn inspect(target: &Target, dir: &Path, placeholder: bool) -> Result<Self, String> {
        let ffmpeg = dir.join(target.exe("ffmpeg"));
        let ffprobe = dir.join(target.exe("ffprobe"));
        for tool in [&ffmpeg, &ffprobe] {
            if !tool.is_file() {
                return Err(format!(
                    "{} is missing; build it with `native/ffmpeg/build.sh {}`, or pass \
                     --ffmpeg-dir DIR to make a placeholder test package",
                    tool.display(),
                    target.name
                ));
            }
        }
        let manifest = dir.join("MANIFEST.json");
        let licenses = dir.join("licenses");
        if !placeholder {
            let text = fs::read_to_string(&manifest)
                .map_err(|e| format!("cannot read {}: {e}", manifest.display()))?;
            serde_json::from_str::<Value>(&text)
                .map_err(|e| format!("{} is not JSON: {e}", manifest.display()))?;
            let empty = fs::read_dir(&licenses)
                .map_err(|e| format!("cannot list {}: {e}", licenses.display()))?
                .next()
                .is_none();
            if empty {
                return Err(format!("{} is empty", licenses.display()));
            }
        }
        let first = |tool: &Path, name: &str| -> Result<(String, String), String> {
            let text = capture(tool.as_os_str(), &["-version"])?;
            let version = text
                .lines()
                .find_map(|l| l.strip_prefix(&format!("{name} version ")))
                .and_then(|rest| rest.split_whitespace().next())
                .ok_or_else(|| format!("{} -version printed no version", tool.display()))?
                .to_owned();
            let configuration = text
                .lines()
                .find_map(|l| l.trim().strip_prefix("configuration:"))
                .unwrap_or_default()
                .trim()
                .to_owned();
            Ok((version, configuration))
        };
        let (version, configuration) = first(&ffmpeg, "ffmpeg")?;
        let (probe_version, _) = first(&ffprobe, "ffprobe")?;
        if version != probe_version {
            return Err(format!(
                "ffmpeg reports {version} but ffprobe reports {probe_version}"
            ));
        }
        let unwanted: Vec<&str> = ["--enable-gpl", "--enable-nonfree"]
            .into_iter()
            .filter(|flag| configuration.split_whitespace().any(|w| w == *flag))
            .collect();
        if !unwanted.is_empty() {
            let message = format!(
                "the FFmpeg in {} is configured with {}; a package ships LGPL builds only",
                dir.display(),
                unwanted.join(" ")
            );
            if !placeholder {
                return Err(message);
            }
            println!("WARNING: {message} (accepted only because this is a placeholder)");
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            placeholder,
            version,
            configuration,
            manifest: manifest.is_file().then_some(manifest),
            licenses: licenses.is_dir().then_some(licenses),
        })
    }

    fn stage(&self, target: &Target, layout: &Layout) -> Result<(), String> {
        for tool in ["ffmpeg", "ffprobe"] {
            let name = target.exe(tool);
            copy(&self.dir.join(&name), &layout.tools.join(&name))?;
        }
        let resources = layout.resources.join("ffmpeg");
        mkdir(&resources)?;
        if let Some(manifest) = &self.manifest {
            copy(manifest, &resources.join("MANIFEST.json"))?;
        }
        if let Some(licenses) = &self.licenses {
            copy_tree(licenses, &resources.join("licenses"))?;
        }
        if self.placeholder {
            write(
                &resources.join("PLACEHOLDER.txt"),
                "These FFmpeg executables are a placeholder given with --ffmpeg-dir, not the \
                 project's controlled build. This package is for testing only and must not be \
                 distributed.\n",
            )?;
        }
        let known = |name: &str| {
            matches!(name, "MANIFEST.json" | "licenses")
                || name == target.exe("ffmpeg")
                || name == target.exe("ffprobe")
        };
        if let Ok(entries) = fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !known(&name) && !self.placeholder {
                    println!(
                        "WARNING: {} is not packaged (only ffmpeg, ffprobe, MANIFEST.json and \
                         licenses/ are)",
                        entry.path().display()
                    );
                }
            }
        }
        Ok(())
    }

    fn record(&self, root: &Path) -> Value {
        let digest = |path: &Option<PathBuf>| path.as_ref().and_then(|p| sha256::file(p).ok());
        json!({
            "placeholder": self.placeholder,
            "from": self.dir.strip_prefix(root).map_or_else(
                |_| "a directory given with --ffmpeg-dir".to_owned(),
                |p| p.display().to_string().replace('\\', "/"),
            ),
            "version": self.version,
            "configuration": self.configuration,
            "manifest_sha256": digest(&self.manifest),
        })
    }
}

fn layout_text(target: &Target) -> &'static str {
    match target.os {
        Os::Linux => {
            "  splot-studio           the app: start this one
  splot-studio-engine    the engine the app starts (x86-64-v3 processors only;
                         the app checks this, the engine run directly cannot)
  ffmpeg/                ffmpeg and ffprobe, used to read MP4, MKV, MOV, TS, AVI,
                         with their build manifest (source, configuration) and licences
  BUILD.json             exact build identity: version, commit, compiler, CPU
                         baseline per program, FFmpeg build, file checksums
  LICENSE.md             licence of Splot Studio itself (PolyForm Noncommercial 1.0.0)
  THIRD-PARTY-NOTICES.md every third-party component and its licence
  licenses/              licence texts of the Rust crates
"
        }
        Os::Windows => {
            "  splot-studio.exe       the app: start this one
  splot-studio-engine.exe
                         the engine the app starts (x86-64-v3 processors only;
                         the app checks this, the engine run directly cannot)
  ffmpeg\\                ffmpeg.exe and ffprobe.exe, used to read MP4, MKV, MOV, TS,
                         AVI, with their build manifest and licences
  BUILD.json             exact build identity: version, commit, compiler, CPU
                         baseline per program, FFmpeg build, file checksums
  LICENSE.md             licence of Splot Studio itself (PolyForm Noncommercial 1.0.0)
  THIRD-PARTY-NOTICES.md every third-party component and its licence
  licenses\\              licence texts of the Rust crates
"
        }
        Os::Macos => {
            "  Splot Studio.app       the app; move it to Applications or anywhere else.
                         Contents/MacOS holds the app, its engine, ffmpeg and
                         ffprobe; Contents/Resources holds LICENSE.md,
                         THIRD-PARTY-NOTICES.md, the Rust crate licence texts
                         (licenses/) and the FFmpeg manifest and licences (ffmpeg/)
  BUILD.json             exact build identity: version, commit, compiler, CPU
                         baseline per program, FFmpeg build, file checksums
"
        }
    }
}

/// Writes `Info.plist`, then signs ad hoc from the inside out: every nested
/// executable, then the bundle, which seals the main executable and the
/// resources. Apple silicon runs no unsigned code at all; an ad-hoc
/// signature is not an identity and does not satisfy Gatekeeper.
fn bundle(stage: &Path, version: &str) -> Result<(), String> {
    let app = stage.join(MAC_APP);
    let template = fs::read_to_string(root().join("packaging/macos/Info.plist"))
        .map_err(|e| format!("cannot read packaging/macos/Info.plist: {e}"))?;
    let plist = template
        .replace("@VERSION@", version)
        .replace("@MIN_MACOS@", MIN_MACOS);
    if let Some(left) = placeholder_in(&plist) {
        return Err(format!("Info.plist template: {left} was not replaced"));
    }
    let path = app.join("Contents/Info.plist");
    write(&path, &plist)?;
    capture("plutil", &["-lint", &path.to_string_lossy()])?;
    let macos = app.join("Contents/MacOS");
    for nested in ["ffmpeg", "ffprobe", "splot-studio-engine"] {
        let file = macos.join(nested);
        capture(
            "codesign",
            &[
                "--force",
                "--sign",
                "-",
                "--timestamp=none",
                &file.to_string_lossy(),
            ],
        )?;
    }
    let app_text = app.to_string_lossy();
    capture(
        "codesign",
        &["--force", "--sign", "-", "--timestamp=none", &app_text],
    )?;
    capture(
        "codesign",
        &["--verify", "--deep", "--strict", "--verbose=2", &app_text],
    )?;
    println!("ok: {MAC_APP} signed ad hoc and verified");
    Ok(())
}

/// Archives `out/name` as `archive`. Linux: a tar with sorted names,
/// numeric root ownership, normalized modes and the commit's time, gzip
/// without a timestamp. macOS: `ditto`, which keeps the bundle's signature
/// and modes. Windows: the system `tar.exe` (bsdtar) writing a zip.
fn archive_folder(
    target: &Target,
    out: &Path,
    name: &str,
    archive: &Path,
    stamp: &str,
) -> Result<(), String> {
    if archive.exists() {
        fs::remove_file(archive)
            .map_err(|e| format!("cannot replace {}: {e}", archive.display()))?;
    }
    let mut command = match target.os {
        Os::Linux => {
            let mut c = Command::new("tar");
            c.args([
                "--sort=name",
                "--owner=0",
                "--group=0",
                "--numeric-owner",
                "--format=gnu",
                "--mode=u+rwX,go+rX,go-w",
                &format!("--mtime=@{stamp}"),
                "--use-compress-program=gzip -n -9",
                "-cf",
            ])
            .arg(archive)
            .arg("-C")
            .arg(out)
            .arg(name);
            c
        }
        Os::Macos => {
            let mut c = Command::new("ditto");
            c.args(["-c", "-k", "--sequesterRsrc", "--keepParent"])
                .arg(out.join(name))
                .arg(archive);
            c
        }
        Os::Windows => {
            let mut c = Command::new(system_tar());
            c.args(["-a", "-c", "-f"])
                .arg(archive)
                .arg("-C")
                .arg(out)
                .arg(name);
            c
        }
    };
    let status = command
        .status()
        .map_err(|e| format!("cannot start the archiver: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("the archiver exited with {status}"))
    }
}

/// Windows' own bsdtar, not a GNU tar that a Git installation may put first
/// on `PATH` (GNU tar cannot write or read zip files).
pub fn system_tar() -> PathBuf {
    let system = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    Path::new(&system).join("System32").join("tar.exe")
}

/// SHA-256 of every file under `dir`, by forward-slash relative path.
fn hashes(dir: &Path) -> Result<BTreeMap<String, String>, String> {
    let mut files = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let entries =
            fs::read_dir(&next).map_err(|e| format!("cannot list {}: {e}", next.display()))?;
        for entry in entries {
            let path = entry
                .map_err(|e| format!("cannot list {}: {e}", next.display()))?
                .path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.insert(relative(dir, &path), sha256::file(&path)?);
            }
        }
    }
    Ok(files)
}

/// `path` under `base`, with forward slashes.
pub fn relative(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The first `@NAME@` template placeholder left in `text`.
fn placeholder_in(text: &str) -> Option<&str> {
    text.match_indices('@').find_map(|(start, _)| {
        let rest = &text[start + 1..];
        let end = rest.find('@')?;
        let name = &rest[..end];
        (!name.is_empty() && name.chars().all(|c| c.is_ascii_uppercase() || c == '_'))
            .then(|| &text[start..start + end + 2])
    })
}

/// The newest of dotted versions such as `2.34` or `14.0`.
fn newest(versions: &[String]) -> Option<String> {
    versions.iter().max_by_key(|v| version_key(v)).cloned()
}

fn version_key(version: &str) -> Vec<u32> {
    version
        .split('.')
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

fn mkdir(dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))
}

fn copy(from: &Path, to: &Path) -> Result<(), String> {
    fs::copy(from, to)
        .map(|_| ())
        .map_err(|e| format!("cannot copy {} to {}: {e}", from.display(), to.display()))
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    mkdir(to)?;
    let entries = fs::read_dir(from).map_err(|e| format!("cannot list {}: {e}", from.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot list {}: {e}", from.display()))?;
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            copy(&entry.path(), &target)?;
        }
    }
    Ok(())
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        let list = ["2.9".to_owned(), "2.34".to_owned(), "2.10".to_owned()];
        assert_eq!(newest(&list).as_deref(), Some("2.34"));
        assert!(version_key("15.0") > version_key("14.9"));
    }

    #[test]
    fn leftover_placeholders_are_found_but_addresses_are_not() {
        assert_eq!(placeholder_in("a @VERSION@ b"), Some("@VERSION@"));
        assert_eq!(placeholder_in("mail <x@example.com> 1.0"), None);
        assert_eq!(placeholder_in("x@y @MIN_MACOS@"), Some("@MIN_MACOS@"));
    }

    #[test]
    fn every_target_has_a_distinct_name() {
        let mut names: Vec<&str> = TARGETS.iter().map(|t| t.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), TARGETS.len());
    }
}
