// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The two repository checks that run inside `ci`.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::run::{capture, capture_cargo, root};

const HEADER: [&str; 2] = [
    "// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0",
    "// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>",
];

/// Every `.rs` and `.slint` file under `crates/` and `xtask/` that is tracked
/// or not yet ignored must start with the two SPDX header lines.
pub fn headers() -> Result<(), String> {
    println!("\n==> check: SPDX headers");
    let listing = capture(
        "git",
        &[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            "crates",
            "xtask",
        ],
    )?;
    let mut checked = 0_usize;
    let mut bad = Vec::new();
    for path in listing.lines().filter(|p| is_source(p)) {
        let full = root().join(path);
        if !full.exists() {
            continue; // tracked but deleted in the working tree
        }
        let text = fs::read_to_string(&full).map_err(|e| format!("cannot read {path}: {e}"))?;
        let mut lines = text.lines();
        if lines.next() != Some(HEADER[0]) || lines.next() != Some(HEADER[1]) {
            bad.push(path);
        }
        checked += 1;
    }
    if checked == 0 {
        return Err("no .rs/.slint files found to check; is git listing the repository?".into());
    }
    if bad.is_empty() {
        println!("ok: {checked} files carry the SPDX header");
        Ok(())
    } else {
        Err(format!(
            "missing the two-line SPDX header: {}",
            bad.join(", ")
        ))
    }
}

fn is_source(path: &str) -> bool {
    let extension = Path::new(path).extension();
    extension.is_some_and(|e| e == "rs" || e == "slint")
}

/// The resolved package graph from `cargo metadata`.
struct Graph {
    /// package id -> name
    names: HashMap<String, String>,
    /// package id -> ids of its direct dependencies (all kinds, all targets)
    deps: HashMap<String, Vec<String>>,
}

impl Graph {
    fn parse(json: &str) -> Result<Self, String> {
        let meta: Value = serde_json::from_str(json).map_err(|e| format!("bad metadata: {e}"))?;
        let strings =
            |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
        let mut names = HashMap::new();
        for package in meta
            .get("packages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let (Some(id), Some(name)) = (strings(package, "id"), strings(package, "name")) {
                names.insert(id, name);
            }
        }
        let mut deps = HashMap::new();
        let nodes = meta.pointer("/resolve/nodes").and_then(Value::as_array);
        for node in nodes.ok_or("cargo metadata has no resolve graph")? {
            let Some(id) = strings(node, "id") else {
                continue;
            };
            let children = node.get("dependencies").and_then(Value::as_array);
            let children = children.into_iter().flatten().filter_map(Value::as_str);
            deps.insert(id, children.map(str::to_owned).collect());
        }
        Ok(Self { names, deps })
    }

    fn id_of(&self, name: &str) -> Result<&String, String> {
        let mut found = self
            .names
            .iter()
            .filter(|(id, n)| *n == name && id.starts_with("path+"));
        found
            .next()
            .map(|(id, _)| id)
            .ok_or_else(|| format!("workspace package {name} not found"))
    }

    /// Names of every package reachable from `name`, excluding itself.
    fn closure(&self, name: &str) -> Result<BTreeSet<&str>, String> {
        let mut seen: BTreeSet<&String> = BTreeSet::new();
        let mut stack = vec![self.id_of(name)?];
        while let Some(id) = stack.pop() {
            for child in self.deps.get(id).into_iter().flatten() {
                if seen.insert(child) {
                    stack.push(child);
                }
            }
        }
        Ok(seen
            .into_iter()
            .filter_map(|id| self.names.get(id).map(String::as_str))
            .collect())
    }
}

/// Enforces the crate boundaries from AGENTS.md on the resolved graph.
pub fn dependency_direction() -> Result<(), String> {
    println!("\n==> check: dependency direction");
    let json = capture_cargo(&["metadata", "--format-version", "1", "--locked"])?;
    let violations = violations(&Graph::parse(&json)?)?;
    if violations.is_empty() {
        println!("ok: dependency direction holds");
        Ok(())
    } else {
        Err(format!(
            "dependency direction violated: {}",
            violations.join("; ")
        ))
    }
}

fn violations(graph: &Graph) -> Result<Vec<String>, String> {
    let mut violations = Vec::new();
    for lib in ["studio-domain", "studio-engine"] {
        for dep in graph.closure(lib)? {
            if dep == "slint" || dep.starts_with("slint-") {
                violations.push(format!("{lib} depends on {dep}"));
            }
        }
    }
    for dep in graph.closure("studio-app")? {
        if dep.starts_with("splot-") || dep.contains("ffmpeg") || dep == "rsmpeg" {
            violations.push(format!("studio-app depends on {dep}"));
        }
    }
    let app = graph.id_of("studio-app")?;
    for (id, children) in &graph.deps {
        if id != app && children.contains(app) {
            let name = graph.names.get(id).map_or(id.as_str(), String::as_str);
            violations.push(format!("{name} depends on studio-app"));
        }
    }
    violations.sort();
    Ok(violations)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A workspace package `id` named `name` with the given direct dependencies.
    fn graph(edges: &[(&str, &[&str])]) -> Graph {
        let pkg = |name: &str| {
            let source = if name.starts_with("studio") {
                "path+file:///w"
            } else {
                "registry+x"
            };
            format!("{source}#{name}@1")
        };
        let mut names = HashMap::new();
        let mut deps = HashMap::new();
        for (name, children) in edges {
            names.insert(pkg(name), (*name).to_owned());
            deps.insert(pkg(name), children.iter().map(|c| pkg(c)).collect());
            for child in *children {
                names.insert(pkg(child), (*child).to_owned());
            }
        }
        Graph { names, deps }
    }

    #[test]
    fn allowed_graph_has_no_violations() {
        let g = graph(&[
            ("studio-domain", &["serde"]),
            ("studio-engine", &["studio-domain"]),
            ("studio-app", &["studio-domain", "slint"]),
        ]);
        assert_eq!(violations(&g), Ok(vec![]));
    }

    #[test]
    fn forbidden_edges_are_reported_transitively() {
        let g = graph(&[
            ("studio-domain", &["helper"]),
            ("helper", &["slint-build"]),
            ("studio-engine", &["studio-app"]),
            ("studio-app", &["glue"]),
            ("glue", &["splot-encode"]),
        ]);
        assert_eq!(
            violations(&g),
            Ok(vec![
                "studio-app depends on splot-encode".to_owned(),
                "studio-domain depends on slint-build".to_owned(),
                "studio-engine depends on studio-app".to_owned(),
            ])
        );
    }

    #[test]
    fn a_missing_package_is_an_error_not_a_pass() {
        assert!(violations(&graph(&[("studio-domain", &[])])).is_err());
    }
}
