// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Records the exact encoder revision from the workspace lockfile.

use std::{fs, io};

fn main() -> io::Result<()> {
    const LOCKFILE: &str = "../../Cargo.lock";
    println!("cargo:rerun-if-changed={LOCKFILE}");
    let lock = fs::read_to_string(LOCKFILE)?;
    let mut revisions = lock
        .split("[[package]]")
        .filter(|package| {
            package
                .lines()
                .any(|line| line == r#"name = "splot-encode""#)
        })
        .map(|package| {
            package
                .lines()
                .find_map(|line| {
                    line.strip_prefix(
                        r#"source = "git+https://github.com/splotlabs/splot?branch=main#"#,
                    )
                })
                .and_then(|source| source.strip_suffix('"'))
        });
    let revision = revisions.next().flatten().filter(|revision| {
        revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
    });
    match (revision, revisions.next()) {
        (Some(revision), None) => {
            println!("cargo:rustc-env=SPLOT_CODEC_REVISION={revision}");
            Ok(())
        }
        _ => Err(io::Error::other(
            "Use Cargo.lock with one pinned Splot encoder revision.",
        )),
    }
}
