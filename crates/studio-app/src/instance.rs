// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Where the app keeps its files, and the lock that lets only one running
//! app use them.
//!
//! The folder is the OS's per-user local application data folder plus
//! `SplotStudio`: `$XDG_DATA_HOME/SplotStudio` (normally
//! `~/.local/share/SplotStudio`) on Linux, `~/Library/Application
//! Support/SplotStudio` on macOS and `%LOCALAPPDATA%\SplotStudio` on
//! Windows. [`DATA_DIR_VAR`] replaces it, for tests and development only.
//!
//! The lock is an OS file lock on `instance.lock` in that folder, held for
//! the life of the process and released by the OS when the process ends in
//! any way, so a crash never leaves a stale lock.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

/// Replaces the data folder when set (tests and development only). Read
/// once, at startup. A relative value (on Windows also one without a
/// drive) is made absolute against the current folder, once, so the app
/// keeps using the same folder whatever it does later.
pub const DATA_DIR_VAR: &str = "SPLOT_STUDIO_DATA_DIR";

/// The lock file's name in the data folder.
pub const LOCK_FILE: &str = "instance.lock";

/// The folder name under the OS's application data folder.
const FOLDER: &str = "SplotStudio";

/// The data folder: `overridden` when given and not empty, otherwise the
/// OS's local application data folder plus `SplotStudio`.
///
/// # Errors
/// When the OS reports no home or application data folder.
pub fn data_dir(overridden: Option<OsString>) -> Result<PathBuf, String> {
    if let Some(dir) = overridden.filter(|d| !d.is_empty()) {
        return std::path::absolute(PathBuf::from(dir))
            .map_err(|e| format!("{DATA_DIR_VAR} is not a usable path: {e}"));
    }
    directories::BaseDirs::new()
        .map(|dirs| dirs.data_local_dir().join(FOLDER))
        .ok_or_else(|| "this system reports no folder for application data".to_owned())
}

/// This process holds the data folder. Dropping it releases the lock.
#[derive(Debug)]
pub struct Instance {
    dir: PathBuf,
    // Held, never read: the OS lock lives as long as this handle.
    _lock: File,
}

impl Instance {
    /// The data folder this instance owns.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// The outcome of [`acquire`].
#[derive(Debug)]
pub enum Acquired {
    /// This process owns the folder.
    Owner(Instance),
    /// Another running app holds it.
    AlreadyOpen,
    /// The folder or its lock cannot be used, with why.
    Failed(String),
}

/// Creates `dir` if needed and takes its instance lock without waiting.
#[must_use]
pub fn acquire(dir: &Path) -> Acquired {
    if let Err(e) = fs::create_dir_all(dir) {
        return Acquired::Failed(format!("cannot create {}: {e}", dir.display()));
    }
    let path = dir.join(LOCK_FILE);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path);
    let file = match file {
        Ok(file) => file,
        Err(e) => return Acquired::Failed(format!("cannot open {}: {e}", path.display())),
    };
    match file.try_lock() {
        Ok(()) => Acquired::Owner(Instance {
            dir: dir.to_path_buf(),
            _lock: file,
        }),
        Err(TryLockError::WouldBlock) => Acquired::AlreadyOpen,
        Err(TryLockError::Error(e)) => {
            Acquired::Failed(format!("cannot lock {}: {e}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Scratch, abs};

    #[test]
    fn the_override_wins_and_the_default_is_per_user() {
        let absolute = abs("/tmp/studio-data");
        let dir = data_dir(Some(absolute.clone().into_os_string()));
        assert_eq!(dir, Ok(absolute));
        // A relative override (or, on Windows, one without a drive) is made
        // absolute against the current folder, as the OS would resolve it.
        let relative = data_dir(Some("studio-data/dev".into()));
        let expected = std::env::current_dir().map(|d| d.join("studio-data").join("dev"));
        assert_eq!(relative.ok(), expected.ok());
        let default = data_dir(Some(OsString::new()));
        assert!(
            default
                .as_ref()
                .is_ok_and(|d| d.is_absolute() && d.ends_with(FOLDER)),
            "{default:?}"
        );
        assert_eq!(default, data_dir(None));
    }

    #[test]
    fn only_one_holder_at_a_time_and_released_on_drop() {
        let scratch = Scratch::new("lock");
        let dir = scratch.path("data");
        let first = acquire(&dir);
        assert!(matches!(first, Acquired::Owner(_)), "{first:?}");
        // A second open file is a second holder, as another process is.
        assert!(matches!(acquire(&dir), Acquired::AlreadyOpen));
        drop(first);
        assert!(matches!(acquire(&dir), Acquired::Owner(_)));
    }

    #[test]
    fn an_unusable_folder_is_a_failure_not_a_second_owner() {
        let scratch = Scratch::new("lock-file");
        let file = scratch.path("plain-file");
        assert!(fs::write(&file, b"").is_ok());
        assert!(matches!(acquire(&file), Acquired::Failed(_)));
    }
}
