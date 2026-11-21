// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Third-party notices for exactly the Rust crates linked into the shipped
//! executables, generated from `cargo metadata` for the package target.
//!
//! The crate set is what `cargo tree -e normal` resolves for `studio-app`
//! and `studio-engine` on that target (no dev or build dependencies,
//! procedural macros included); `cargo metadata` supplies each crate's
//! licence, source and files. Every declared licence
//! expression must be satisfiable from [`ALLOWED`]; a crate without one, or
//! with a licence nobody reviewed, fails the package instead of being listed.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::run::capture_cargo;
use crate::sha256;

/// Licence identifiers reviewed for binary redistribution with notices.
/// `LicenseRef-Slint-Royalty-free-2.0` is the Slint option this project
/// chose (attribution through `AboutSlint`); `PolyForm-Noncommercial-1.0.0`
/// is the project's own and the Splot codec's (same owner). `MPL-2.0` is
/// file-level copyleft: its source stays available on crates.io.
const ALLOWED: [&str; 16] = [
    "MIT",
    "MIT-0",
    "Apache-2.0",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "ISC",
    "Zlib",
    "0BSD",
    "Unlicense",
    "CC0-1.0",
    "BSL-1.0",
    "Unicode-3.0",
    "Unicode-DFS-2016",
    "MPL-2.0",
    "LicenseRef-Slint-Royalty-free-2.0",
    "PolyForm-Noncommercial-1.0.0",
];

/// Licence exceptions (`X WITH exception`) that only add permissions.
const EXCEPTIONS: [&str; 1] = ["LLVM-exception"];

/// Files a crate embeds into the executable that its own licence
/// expression does not cover: (crate, path in the crate, what it is).
/// Each is checked to exist so the list cannot silently go stale.
const EMBEDDED: [(&str, &str, &str); 1] = [(
    "i-slint-common",
    "sharedfontique/Inter-VariableFont.ttf.license",
    "Inter variable font, embedded by Slint as its default font (SIL Open Font License 1.1)",
)];

/// What was written, for `BUILD.json` and the console.
pub struct Written {
    /// How many third-party crates are listed.
    pub crates: usize,
    /// Crates whose published package carries no licence file.
    pub without_text: Vec<String>,
}

struct Crate {
    name: String,
    version: String,
    license: String,
    source: String,
    repository: String,
    dir: PathBuf,
    license_file: Option<String>,
}

/// Writes `notices` (the repository's notices plus the generated crate
/// list) and `texts` (every crate licence file, identical texts once).
pub fn write(
    target: &str,
    repo_notices: &Path,
    notices: &Path,
    texts: &Path,
) -> Result<Written, String> {
    let json = capture_cargo(&[
        "metadata",
        "--format-version",
        "1",
        "--locked",
        "--filter-platform",
        target,
    ])?;
    let crates = shipped(&json, &linked(target)?)?;
    let mut table = String::new();
    let mut body = String::new();
    let mut seen: HashMap<String, String> = HashMap::new();
    let mut without_text = Vec::new();
    for krate in &crates {
        let _ = writeln!(
            table,
            "| `{}` | {} | {} | {} |",
            krate.name, krate.version, krate.license, krate.source
        );
        let files = licence_files(krate)?;
        let _ = writeln!(
            body,
            "\n==============================================================================\n\
             {} {} ({})\nLicence: {}\nSource: {}\n",
            krate.name, krate.version, krate.repository, krate.license, krate.source
        );
        if files.is_empty() {
            without_text.push(format!("{} {}", krate.name, krate.version));
            let _ = writeln!(
                body,
                "The published package contains no licence file; the declared licence is \
                 {}.",
                krate.license
            );
        }
        for (name, text) in files {
            let digest = sha256::hex(text.as_bytes());
            let label = format!("{} {} {name}", krate.name, krate.version);
            if let Some(first) = seen.get(&digest) {
                let _ = writeln!(body, "--- {name}: identical to the text of {first} above.");
            } else {
                let _ = writeln!(body, "--- {name}:\n\n{}", text.trim_end());
                seen.insert(digest, label);
            }
        }
    }
    let mut assets = String::new();
    for (owner, path, what) in EMBEDDED {
        let Some(krate) = crates.iter().find(|c| c.name == owner) else {
            continue;
        };
        let file = krate.dir.join(path);
        let text = fs::read_to_string(&file)
            .map_err(|e| format!("embedded asset notice {} is missing: {e}", file.display()))?;
        let _ = writeln!(
            assets,
            "- {what}: from `{owner}` {}, `{path}`:\n\n```text\n{}\n```",
            krate.version,
            text.trim_end()
        );
    }
    let base = fs::read_to_string(repo_notices)
        .map_err(|e| format!("cannot read {}: {e}", repo_notices.display()))?;
    let generated = format!(
        "{}\n\n## Rust crates in this build (generated)\n\n\
         Generated by `cargo xtask package` from `cargo tree --target {target} -e normal` and \
         `cargo metadata`: every crate linked into `splot-studio` or `splot-studio-engine` \
         (procedural macros included; build tools and test-only crates are not shipped). \
         Licences are the crates' declared SPDX expressions; the full licence texts each \
         published crate carries are in `licenses/RUST-CRATES.txt`.\n\n\
         | Crate | Version | Declared licence | Source |\n|---|---|---|---|\n{table}\n\
         ## Embedded assets not covered by a crate licence\n\n{assets}",
        base.trim_end()
    );
    write_file(notices, &generated)?;
    write_file(
        texts,
        &format!(
            "Licence texts of the Rust crates linked into Splot Studio {}.\n\
             Generated by cargo xtask package for {target}.\n{body}",
            env!("CARGO_PKG_VERSION")
        ),
    )?;
    Ok(Written {
        crates: crates.len(),
        without_text,
    })
}

fn write_file(path: &Path, text: &str) -> Result<(), String> {
    fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// The third-party crates reachable from the two executables through
/// normal edges, sorted by name and version.
fn shipped(json: &str, linked: &BTreeSet<(String, String)>) -> Result<Vec<Crate>, String> {
    let meta: Value = serde_json::from_str(json).map_err(|e| format!("bad metadata: {e}"))?;
    let key = |p: &Value, field: &str| {
        p.get(field)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let packages: Vec<&Value> = meta
        .get("packages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|p| linked.contains(&(key(p, "name"), key(p, "version"))))
        .collect();
    let found: BTreeSet<(String, String)> = packages
        .iter()
        .map(|p| (key(p, "name"), key(p, "version")))
        .collect();
    if let Some((name, version)) = linked.difference(&found).next() {
        return Err(format!(
            "cargo tree lists {name} {version}, cargo metadata does not"
        ));
    }
    let mut crates = BTreeMap::new();
    let mut problems = Vec::new();
    for package in packages {
        // Workspace members are this project's own material (LICENSE.md).
        if package.get("source").is_none_or(Value::is_null) {
            continue;
        }
        match accept(package) {
            Ok(krate) => {
                crates.insert((krate.name.clone(), krate.version.clone()), krate);
            }
            Err(problem) => problems.push(problem),
        }
    }
    if problems.is_empty() {
        Ok(crates.into_values().collect())
    } else {
        Err(format!(
            "the licence of {} shipped crate(s) cannot be accepted:\n  {}",
            problems.len(),
            problems.join("\n  ")
        ))
    }
}

/// (name, version) of every package `cargo tree` resolves for the two
/// executables on `target` through normal edges. Unlike the metadata
/// graph, this applies the feature resolver for that one target, so an
/// optional dependency enabled only elsewhere is not counted.
fn linked(target: &str) -> Result<BTreeSet<(String, String)>, String> {
    let text = capture_cargo(&[
        "tree",
        "--locked",
        "--target",
        target,
        "-e",
        "normal",
        "-p",
        "studio-app",
        "-p",
        "studio-engine",
        "--prefix",
        "none",
        "-f",
        "{p}",
    ])?;
    Ok(tree_packages(&text))
}

/// Parses `cargo tree --prefix none -f {p}` lines: `name vX.Y.Z [suffix]`.
fn tree_packages(text: &str) -> BTreeSet<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let name = words.next()?;
            let version = words.next()?.strip_prefix('v')?;
            Some((name.to_owned(), version.to_owned()))
        })
        .collect()
}

/// A third-party package whose licence expression is acceptable.
fn accept(package: &Value) -> Result<Crate, String> {
    let text = |key: &str| package.get(key).and_then(Value::as_str).map(str::to_owned);
    let name = text("name").unwrap_or_default();
    let version = text("version").unwrap_or_default();
    let license_file = text("license_file");
    let Some(license) = text("license") else {
        return Err(format!(
            "{name} {version}: no SPDX licence expression{}",
            license_file.map_or(String::new(), |f| format!(
                " (only a licence file, {f}, which needs a review)"
            ))
        ));
    };
    match allowed(&license) {
        Ok(true) => {}
        Ok(false) => return Err(format!("{name} {version}: unreviewed licence {license:?}")),
        Err(why) => return Err(format!("{name} {version}: licence {license:?}: {why}")),
    }
    let manifest = text("manifest_path").unwrap_or_default();
    let dir = Path::new(&manifest)
        .parent()
        .map_or_else(PathBuf::new, Path::to_path_buf);
    let source = text("source").unwrap_or_default();
    let source = source
        .split_once('#')
        .map_or(source.as_str(), |(url, _)| url)
        .replace(
            "registry+https://github.com/rust-lang/crates.io-index",
            "crates.io",
        );
    Ok(Crate {
        repository: text("repository").unwrap_or_else(|| "no repository".into()),
        name,
        version,
        license,
        source,
        dir,
        license_file,
    })
}

/// The licence files a crate's package carries: `LICENSE*`, `LICENCE*`,
/// `COPYING*`, `COPYRIGHT*`, `NOTICE*`, `UNLICENSE*` at its root, everything
/// in its `LICENSES/` directory, and its declared `license-file`.
fn licence_files(krate: &Crate) -> Result<Vec<(String, String)>, String> {
    const PREFIXES: [&str; 6] = [
        "LICENSE",
        "LICENCE",
        "COPYING",
        "COPYRIGHT",
        "NOTICE",
        "UNLICENSE",
    ];
    let mut paths = BTreeSet::new();
    let list = |dir: &Path| -> Result<Vec<PathBuf>, String> {
        let entries =
            fs::read_dir(dir).map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
        let mut files = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
            if entry.file_type().is_ok_and(|t| t.is_file()) {
                files.push(entry.path());
            }
        }
        Ok(files)
    };
    for path in list(&krate.dir)? {
        let name = path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().to_uppercase());
        if PREFIXES.iter().any(|p| name.starts_with(p)) {
            paths.insert(path);
        }
    }
    let licenses = krate.dir.join("LICENSES");
    if licenses.is_dir() {
        paths.extend(list(&licenses)?);
    }
    if let Some(file) = &krate.license_file {
        paths.insert(krate.dir.join(file));
    }
    paths
        .into_iter()
        .map(|path| {
            let bytes =
                fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            let name = path
                .strip_prefix(&krate.dir)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            Ok((name, String::from_utf8_lossy(&bytes).into_owned()))
        })
        .collect()
}

/// Whether an SPDX expression can be satisfied from [`ALLOWED`]: `OR` needs
/// one acceptable side, `AND` both, `X WITH e` an acceptable `X` and a known
/// exception. The legacy `/` separator means `OR`.
fn allowed(expression: &str) -> Result<bool, String> {
    let spaced = expression
        .replace('(', " ( ")
        .replace(')', " ) ")
        .replace('/', " OR ");
    let tokens: Vec<&str> = spaced.split_whitespace().collect();
    let mut at = 0;
    let value = or_expr(&tokens, &mut at)?;
    if at == tokens.len() {
        Ok(value)
    } else {
        Err(format!("unexpected {:?}", tokens[at]))
    }
}

fn or_expr(tokens: &[&str], at: &mut usize) -> Result<bool, String> {
    let mut value = and_expr(tokens, at)?;
    while tokens.get(*at) == Some(&"OR") {
        *at += 1;
        value |= and_expr(tokens, at)?;
    }
    Ok(value)
}

fn and_expr(tokens: &[&str], at: &mut usize) -> Result<bool, String> {
    let mut value = with_expr(tokens, at)?;
    while tokens.get(*at) == Some(&"AND") {
        *at += 1;
        value &= with_expr(tokens, at)?;
    }
    Ok(value)
}

fn with_expr(tokens: &[&str], at: &mut usize) -> Result<bool, String> {
    let token = *tokens.get(*at).ok_or("ends early")?;
    *at += 1;
    let value = match token {
        "(" => {
            let inner = or_expr(tokens, at)?;
            if tokens.get(*at) != Some(&")") {
                return Err("unbalanced parenthesis".into());
            }
            *at += 1;
            inner
        }
        "OR" | "AND" | "WITH" | ")" => return Err(format!("unexpected {token:?}")),
        // `MIT+` ("or later") is not in the list, so it is refused too.
        id => ALLOWED.contains(&id),
    };
    if tokens.get(*at) == Some(&"WITH") {
        let exception = *tokens.get(*at + 1).ok_or("WITH without an exception")?;
        *at += 2;
        return Ok(value && EXCEPTIONS.contains(&exception));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expressions_evaluate() {
        for ok in [
            "MIT",
            "MIT OR Apache-2.0",
            "MIT/Apache-2.0",
            "Apache-2.0 / MIT",
            "Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT",
            "(MIT OR Apache-2.0) AND Unicode-3.0",
            "GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0",
        ] {
            assert_eq!(allowed(ok), Ok(true), "{ok}");
        }
        for refused in [
            "GPL-3.0-only",
            "MIT AND GPL-2.0-or-later",
            "LicenseRef-Unknown",
            "Apache-2.0 WITH Some-exception",
            "MIT+",
        ] {
            assert_eq!(allowed(refused), Ok(false), "{refused}");
        }
        for broken in ["", "MIT OR", "(MIT", "MIT)", "OR MIT", "MIT WITH"] {
            assert!(allowed(broken).is_err(), "{broken}");
        }
    }

    const METADATA: &str = r#"{"packages":[
      {"id":"app","name":"studio-app","version":"0.1.0","source":null,"license":"x","manifest_path":"/w/a/Cargo.toml"},
      {"id":"lib","name":"lib","version":"1.0.0","source":"registry+https://github.com/rust-lang/crates.io-index","license":"MIT","manifest_path":"/r/lib/Cargo.toml"},
      {"id":"gpl","name":"gpl","version":"1.0.0","source":"registry+https://github.com/rust-lang/crates.io-index","license":"GPL-3.0-only","manifest_path":"/r/gpl/Cargo.toml"}
    ]}"#;

    fn set(lines: &str) -> BTreeSet<(String, String)> {
        tree_packages(lines)
    }

    #[test]
    fn only_what_cargo_tree_lists_ships_and_workspace_members_are_skipped() {
        let linked = set("studio-app v0.1.0 (/w/a)\nlib v1.0.0\nlib v1.0.0 (*)\n");
        let names: Vec<String> = shipped(METADATA, &linked)
            .unwrap_or_else(|e| unreachable!("{e}"))
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, ["lib"]);
    }

    #[test]
    fn a_shipped_unreviewed_licence_fails() {
        let error = shipped(METADATA, &set("gpl v1.0.0\n"))
            .err()
            .unwrap_or_default();
        assert!(error.contains("gpl 1.0.0: unreviewed licence"), "{error}");
    }

    #[test]
    fn a_crate_missing_from_the_metadata_fails() {
        let error = shipped(METADATA, &set("ghost v2.0.0 (proc-macro)\n"))
            .err()
            .unwrap_or_default();
        assert!(error.contains("ghost 2.0.0"), "{error}");
    }
}
