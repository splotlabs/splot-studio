// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! What a shipped executable needs from the machine it runs on.
//!
//! Each OS's own tool lists the libraries an executable loads at start:
//! `objdump -p` and `ldd` on Linux, `otool -L`/`otool -l` on macOS,
//! `dumpbin /dependents` on Windows. Anything outside the operating system's
//! own libraries fails: a package must not depend on a developer's machine.

use std::path::Path;
use std::process::Command;

use crate::package::Os;

/// Libraries every Linux distribution this package targets provides with
/// glibc itself, plus the compiler runtime.
const LINUX_SYSTEM: [&str; 7] = [
    "libc.so.6",
    "libm.so.6",
    "libgcc_s.so.1",
    "ld-linux-x86-64.so.2",
    "libpthread.so.0",
    "libdl.so.2",
    "librt.so.1",
];

/// Windows system DLLs (all in `System32` on Windows 10 and 11). The C
/// runtime is linked statically (`+crt-static`), so `vcruntime*.dll` and
/// `msvcp*.dll` are deliberately absent: they are not on a clean machine.
const WINDOWS_SYSTEM: [&str; 42] = [
    "kernel32.dll",
    "kernelbase.dll",
    "ntdll.dll",
    "user32.dll",
    "gdi32.dll",
    "advapi32.dll",
    "shell32.dll",
    "shcore.dll",
    "ole32.dll",
    "oleaut32.dll",
    "comctl32.dll",
    "comdlg32.dll",
    "uxtheme.dll",
    "dwmapi.dll",
    "imm32.dll",
    "opengl32.dll",
    "ws2_32.dll",
    "bcrypt.dll",
    "bcryptprimitives.dll",
    "userenv.dll",
    "shlwapi.dll",
    "uiautomationcore.dll",
    "propsys.dll",
    "winmm.dll",
    "version.dll",
    "secur32.dll",
    "crypt32.dll",
    "ncrypt.dll",
    "msvcrt.dll",
    "ucrtbase.dll",
    "dxgi.dll",
    "d3d11.dll",
    "d3d12.dll",
    "setupapi.dll",
    "cfgmgr32.dll",
    "powrprof.dll",
    "hid.dll",
    "dbghelp.dll",
    "synchronization.dll",
    "msimg32.dll",
    "psapi.dll",
    "winspool.drv",
];

/// What the tools said about one executable.
#[derive(Debug, Default)]
pub struct Linked {
    /// The libraries it loads at start, as the tool names them.
    pub libraries: Vec<String>,
    /// Linux: the newest `GLIBC_x.y` symbol version it requires.
    pub glibc: Option<String>,
    /// macOS: the newest `minos` among its load commands.
    pub minos: Option<String>,
}

/// Lists and checks the start-up libraries of `exe`. `extra` are further
/// library names this one executable may need (the GUI needs fontconfig on
/// Linux).
pub fn linked(os: Os, exe: &Path, extra: &[&str]) -> Result<Linked, String> {
    match os {
        Os::Linux => linux(exe, extra),
        Os::Macos => macos(exe),
        Os::Windows => windows(exe),
    }
}

fn tool(program: &str, args: &[&str], exe: &Path) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .arg(exe)
        // No inherited library path may hide a missing library.
        .env_remove("LD_LIBRARY_PATH")
        .env_remove("DYLD_LIBRARY_PATH")
        .env_remove("DYLD_FALLBACK_LIBRARY_PATH")
        .output()
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    if output.status.success() {
        Ok(text)
    } else {
        Err(format!(
            "{program} {} {} exited with {}: {}{}",
            args.join(" "),
            exe.display(),
            output.status,
            text.trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn linux(exe: &Path, extra: &[&str]) -> Result<Linked, String> {
    let dump = tool("objdump", &["-p"], exe)?;
    let mut linked = Linked::default();
    let mut problems = Vec::new();
    let mut newest: Option<(u32, u32, u32)> = None;
    for line in dump.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            ["NEEDED", name] => {
                if !LINUX_SYSTEM.contains(name) && !extra.contains(name) {
                    problems.push(format!(
                        "needs {name}, which is not an expected system library"
                    ));
                }
                linked.libraries.push((*name).to_owned());
            }
            ["RPATH" | "RUNPATH", path] if !path.starts_with("$ORIGIN") => {
                problems.push(format!("searches {path} for libraries"));
            }
            // `hash flags index GLIBC_x.y` under "Version References". A weak
            // reference (flag 0x2) only warns when missing: std looks those
            // functions up and falls back, so they set no floor.
            [_, flags, _, version] if version.starts_with("GLIBC_") => {
                let weak = u32::from_str_radix(flags.trim_start_matches("0x"), 16)
                    .is_ok_and(|f| f & 0x2 != 0);
                if let (false, Some(parsed)) = (weak, glibc_version(version)) {
                    newest = newest.max(Some(parsed));
                }
            }
            _ => {}
        }
    }
    linked.glibc = newest.map(|(a, b, c)| {
        if c == 0 {
            format!("{a}.{b}")
        } else {
            format!("{a}.{b}.{c}")
        }
    });
    if !linked.libraries.is_empty() {
        // `ldd` resolves the whole tree; "not found" anywhere is fatal.
        let resolved = tool("ldd", &[], exe)?;
        for line in resolved.lines().filter(|l| l.contains("not found")) {
            problems.push(format!("cannot load {}", line.trim()));
        }
    }
    verdict(exe, linked, &problems)
}

/// `GLIBC_2.34` as (2, 34, 0); `GLIBC_PRIVATE` is not a version.
fn glibc_version(word: &str) -> Option<(u32, u32, u32)> {
    let mut parts = word.strip_prefix("GLIBC_")?.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next().map_or(Some(0), |p| p.parse().ok())?;
    Some((major, minor, patch))
}

fn macos(exe: &Path) -> Result<Linked, String> {
    let mut linked = Linked::default();
    let mut problems = Vec::new();
    // The first line names the file itself.
    for line in tool("otool", &["-L"], exe)?.lines().skip(1) {
        let path = line
            .trim()
            .split(" (compatibility")
            .next()
            .unwrap_or_default();
        if path.is_empty() {
            continue;
        }
        if !(path.starts_with("/usr/lib/") || path.starts_with("/System/Library/")) {
            problems.push(format!("loads {path}, which is not part of macOS"));
        }
        linked.libraries.push(path.to_owned());
    }
    let commands = tool("otool", &["-l"], exe)?;
    let mut newest: Option<(u32, u32)> = None;
    for line in commands.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            ["path", path, ..]
                if !(path.starts_with("@executable_path") || path.starts_with("@loader_path")) =>
            {
                problems.push(format!("has the run path {path}"));
            }
            ["minos", version] => {
                let mut parts = version.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
                let parsed = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
                newest = newest.max(Some(parsed));
            }
            _ => {}
        }
    }
    linked.minos = newest.map(|(major, minor)| format!("{major}.{minor}"));
    verdict(exe, linked, &problems)
}

fn windows(exe: &Path) -> Result<Linked, String> {
    let dumpbin = dumpbin()?;
    let text = tool(&dumpbin, &["/nologo", "/dependents"], exe)?;
    let mut linked = Linked::default();
    let mut problems = Vec::new();
    for line in text.lines() {
        let name = line.trim().to_ascii_lowercase();
        // `name` is already lowercase.
        let is_library = Path::new(&name)
            .extension()
            .is_some_and(|e| e == "dll" || e == "drv");
        if !is_library || name.contains(' ') {
            continue;
        }
        let system = name.starts_with("api-ms-win-")
            || name.starts_with("ext-ms-win-")
            || WINDOWS_SYSTEM.contains(&name.as_str());
        if !system {
            problems.push(format!("loads {name}, which is not a Windows system DLL"));
        }
        linked.libraries.push(name);
    }
    if linked.libraries.is_empty() {
        problems.push("dumpbin listed no DLL at all; its output was not understood".to_owned());
    }
    verdict(exe, linked, &problems)
}

/// `dumpbin.exe` from the newest Visual Studio with the C++ tools, found
/// with `vswhere`, which every Visual Studio installation provides.
fn dumpbin() -> Result<String, String> {
    let base = std::env::var("ProgramFiles(x86)")
        .map_err(|_| "ProgramFiles(x86) is not set".to_owned())?;
    let vswhere = Path::new(&base).join("Microsoft Visual Studio/Installer/vswhere.exe");
    let output = Command::new(&vswhere)
        .args(["-latest", "-products", "*", "-find"])
        .arg("VC\\Tools\\MSVC\\**\\bin\\Hostx64\\x64\\dumpbin.exe")
        .output()
        .map_err(|e| format!("cannot run {}: {e}", vswhere.display()))?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "vswhere found no dumpbin.exe (Visual Studio C++ tools)".to_owned())
}

fn verdict(exe: &Path, linked: Linked, problems: &[String]) -> Result<Linked, String> {
    if problems.is_empty() {
        Ok(linked)
    } else {
        Err(format!("{}: {}", exe.display(), problems.join("; ")))
    }
}

/// The PE optional header's `Subsystem`: 2 is a window program, 3 a console
/// program. `None` when `bytes` is not a PE image.
pub fn pe_subsystem(bytes: &[u8]) -> Option<u16> {
    let at = |offset: usize, len: usize| bytes.get(offset..offset + len);
    if at(0, 2)? != b"MZ" {
        return None;
    }
    let pe = u32::from_le_bytes(at(0x3c, 4)?.try_into().ok()?) as usize;
    if at(pe, 4)? != b"PE\0\0" {
        return None;
    }
    // Signature (4) + COFF header (20); Subsystem sits at the same offset (68)
    // in PE32 and PE32+ optional headers.
    let optional = pe + 24;
    let magic = u16::from_le_bytes(at(optional, 2)?.try_into().ok()?);
    if magic != 0x10b && magic != 0x20b {
        return None;
    }
    Some(u16::from_le_bytes(at(optional + 68, 2)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glibc_versions_order_numerically() {
        assert_eq!(glibc_version("GLIBC_2.34"), Some((2, 34, 0)));
        assert_eq!(glibc_version("GLIBC_2.2.5"), Some((2, 2, 5)));
        assert_eq!(glibc_version("GLIBC_PRIVATE"), None);
        assert!(glibc_version("GLIBC_2.9") < glibc_version("GLIBC_2.10"));
    }

    #[test]
    fn pe_subsystem_is_read_from_the_optional_header() {
        let mut image = vec![0_u8; 0x200];
        image[..2].copy_from_slice(b"MZ");
        image[0x3c..0x40].copy_from_slice(&0x80_u32.to_le_bytes());
        image[0x80..0x84].copy_from_slice(b"PE\0\0");
        image[0x98..0x9a].copy_from_slice(&0x20b_u16.to_le_bytes());
        image[0x98 + 68..0x98 + 70].copy_from_slice(&2_u16.to_le_bytes());
        assert_eq!(pe_subsystem(&image), Some(2));
        image[0x98..0x9a].copy_from_slice(&0x999_u16.to_le_bytes());
        assert_eq!(pe_subsystem(&image), None);
        assert_eq!(pe_subsystem(b"\x7fELF"), None);
        assert_eq!(pe_subsystem(&image[..0x90]), None);
    }
}
