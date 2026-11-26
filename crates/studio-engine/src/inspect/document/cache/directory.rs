// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Unix directory operations. Only the cache directory handle resolves entry names.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

use rustix::fs::{self, AtFlags, Dir, FileType, Mode, OFlags};
use rustix::io::Errno;

use super::invalid;

const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const ENTRY: OFlags = OFlags::NOFOLLOW
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC);

#[derive(Debug)]
pub(super) struct Directory(File);

impl Directory {
    pub(super) fn create(path: &Path) -> io::Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| invalid("The cache directory has no parent."))?;
        let name = path
            .file_name()
            .ok_or_else(|| invalid("The cache directory has no name."))?;
        let parent = open_absolute(parent)?;
        match fs::mkdirat(&parent, name, Mode::RWXU) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(error) => return Err(error.into()),
        }
        let file = File::from(fs::openat(&parent, name, DIRECTORY, Mode::empty())?);
        let attributes = fs::fstat(&file)?;
        if Mode::from_bits_truncate(attributes.st_mode).intersects(Mode::WGRP | Mode::WOTH) {
            return Err(invalid(
                "The cache directory permits writes from other users.",
            ));
        }
        Ok(Self(file))
    }

    pub(super) fn check_current(&self, path: &Path) -> io::Result<()> {
        let current = fs::fstat(open_absolute(path)?)?;
        let existing = fs::fstat(&self.0)?;
        if current.st_dev != existing.st_dev || current.st_ino != existing.st_ino {
            return Err(invalid("The cache directory changed."));
        }
        Ok(())
    }

    pub(super) fn list(&self) -> io::Result<impl Iterator<Item = io::Result<OsString>>> {
        Ok(Dir::read_from(&self.0)?.map(|item| {
            item.map(|entry| OsStr::from_bytes(entry.file_name().to_bytes()).to_owned())
                .map_err(Into::into)
        }))
    }

    pub(super) fn regular(&self, name: &OsStr) -> io::Result<Option<File>> {
        // The nonblocking open below also protects against replacement of a
        // regular entry with a named pipe between these operations.
        match fs::statat(&self.0, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile => {}
            Ok(_) | Err(Errno::NOENT) => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        match fs::openat(&self.0, name, ENTRY | OFlags::RDONLY, Mode::empty()) {
            Ok(file) => {
                let file = File::from(file);
                Ok(file.metadata()?.is_file().then_some(file))
            }
            Err(Errno::LOOP | Errno::NOENT) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn lock_file(&self, name: &OsStr) -> io::Result<File> {
        let file = File::from(fs::openat(
            &self.0,
            name,
            ENTRY | OFlags::RDWR | OFlags::CREATE,
            Mode::RUSR | Mode::WUSR,
        )?);
        self.check_file(name, &file)?;
        Ok(file)
    }

    pub(super) fn create_file(&self, name: &OsStr) -> io::Result<File> {
        fs::openat(
            &self.0,
            name,
            ENTRY | OFlags::RDWR | OFlags::CREATE | OFlags::EXCL,
            Mode::RUSR | Mode::WUSR,
        )
        .map(File::from)
        .map_err(Into::into)
    }

    pub(super) fn check_file(&self, name: &OsStr, file: &File) -> io::Result<()> {
        let current = fs::statat(&self.0, name, AtFlags::SYMLINK_NOFOLLOW)?;
        let opened = fs::fstat(file)?;
        if FileType::from_raw_mode(current.st_mode) != FileType::RegularFile
            || current.st_dev != opened.st_dev
            || current.st_ino != opened.st_ino
        {
            return Err(invalid("The cache file changed."));
        }
        Ok(())
    }

    pub(super) fn remove(&self, name: &OsStr, file: &File) -> io::Result<()> {
        self.check_file(name, file)?;
        fs::unlinkat(&self.0, name, AtFlags::empty())?;
        Ok(())
    }

    pub(super) fn publish(&self, source: &OsStr, dest: &OsStr, file: &File) -> io::Result<()> {
        self.check_file(source, file)?;
        fs::linkat(&self.0, source, &self.0, dest, AtFlags::empty())?;
        self.remove(source, file)
    }
}

fn open_absolute(path: &Path) -> io::Result<File> {
    let mut components = path.components();
    if components.next() != Some(Component::RootDir) {
        return Err(invalid("Use an absolute path for the cache directory."));
    }
    let mut directory = File::from(fs::open("/", DIRECTORY, Mode::empty())?);
    for component in components {
        let Component::Normal(name) = component else {
            return Err(invalid("The cache directory path is invalid."));
        };
        directory = File::from(fs::openat(&directory, name, DIRECTORY, Mode::empty())?);
    }
    Ok(directory)
}
