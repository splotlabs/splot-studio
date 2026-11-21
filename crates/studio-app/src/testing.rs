// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Helpers for unit tests: platform-absolute fixture paths and scratch
//! directories for tests that touch real files.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

/// A platform-absolute path from a Unix-looking one: `/out/clip.ivf`
/// stays as it is on Unix and becomes `C:\out\clip.ivf` on Windows, where
/// a path without a drive is not absolute. Fixtures that reach code which
/// requires absolute paths (recovery's temporary-file check) use this.
pub fn abs(unix: &str) -> PathBuf {
    debug_assert!(
        unix.starts_with('/'),
        "{unix} is not a Unix-looking absolute path"
    );
    if cfg!(windows) {
        PathBuf::from(format!("C:{}", unix.replace('/', "\\")))
    } else {
        PathBuf::from(unix)
    }
}

/// A fresh, empty directory for one test, removed afterwards.
pub struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new(name: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "studio-app-unit-{}-{}-{name}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap_or_else(|e| unreachable!("{e}"));
        Self(dir)
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    pub fn entries(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.0).map_or_else(
            |_| Vec::new(),
            |dir| {
                dir.filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            },
        );
        names.sort();
        names
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_paths_are_absolute_with_the_same_components() {
        let path = abs("/out/clip one.ivf");
        assert!(path.is_absolute(), "{}", path.display());
        let names: Vec<_> = path.iter().filter_map(|c| c.to_str()).collect();
        assert!(names.ends_with(&["out", "clip one.ivf"]), "{names:?}");
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("clip one.ivf")
        );
    }
}
