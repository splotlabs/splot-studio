// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Complete structural indexes in the app-owned data directory.
//! Source identity requires a full content digest. The cache stores no decoded evidence.

#[cfg(unix)]
use std::ffi::OsStr;
#[cfg(any(not(unix), test))]
use std::fs;
#[cfg(not(unix))]
use std::fs::Metadata;
#[cfg(any(not(unix), test))]
use std::fs::OpenOptions;
use std::fs::{File, FileTimes, TryLockError};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use studio_domain::analysis::{
    DocumentSummary, Framing, INSPECT_DATA_DIR_VAR, ScanOutcome, SourceDigest,
};

use crate::artifact::hex;
#[cfg(not(unix))]
use crate::output::FileId;

#[cfg(unix)]
mod directory;

const DIRECTORY: &str = "analysis-index";
const ENTRY_PREFIX: &str = "index-";
const STAGING_PREFIX: &str = ".stage-";
const RANDOM_LEN: usize = 16;
const MAGIC: &[u8; 18] = b"SplotStudio/index\0";
// Increase this version when the structure or meaning of an index changes.
const SCHEMA: u32 = 1;
const SPEC: &str = "AV2 v1.0.0";
const POLICY: &str = "structural/all-layers/grain-not-applied";
const VERIFY_BYTES: u64 = 4 << 20;
const MAX_BYTES: u64 = 512 * 1024 * 1024;
const MAX_ENTRIES: usize = 32;
const MAX_SUMMARY: usize = 64 * 1024;
const SHA_BYTES: usize = 32;

#[derive(Debug)]
pub(super) struct Cache {
    root: PathBuf,
    #[cfg(unix)]
    directory: directory::Directory,
    #[cfg(not(unix))]
    root_id: FileId,
    compatibility: [u8; SHA_BYTES],
    max_bytes: u64,
}

struct Entry {
    path: PathBuf,
    bytes: u64,
    modified: SystemTime,
    owned: bool,
}

pub(super) struct EntryReader {
    file: Option<FileLease>,
    payload_end: u64,
    left: u64,
    hash: Sha256,
    header: Vec<u8>,
    framing: Framing,
    digest: SourceDigest,
}

pub(super) enum VerifiedEntry {
    Reading,
    Invalid,
    Complete(FileLease, Box<DocumentSummary>, u64),
}

/// Holds a lease until its owner releases the file, even if a fork inherited its handle.
#[derive(Debug)]
pub(super) struct FileLease {
    file: File,
    leased: bool,
}

impl FileLease {
    pub(super) const fn unleased(file: File) -> Self {
        Self {
            file,
            leased: false,
        }
    }

    const fn leased(file: File) -> Self {
        Self { file, leased: true }
    }
}

impl Deref for FileLease {
    type Target = File;

    fn deref(&self) -> &Self::Target {
        &self.file
    }
}

impl DerefMut for FileLease {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.file
    }
}

impl Drop for FileLease {
    fn drop(&mut self) {
        // A child may still have a copy of this descriptor before exec.
        if self.leased {
            let _ = self.file.unlock();
        }
    }
}

#[cfg(not(windows))]
type DirectoryLock = FileLease;

#[cfg(windows)]
struct DirectoryLock {
    _file: FileLease,
    _ancestry: Vec<File>,
}

impl Cache {
    pub(super) fn from_env() -> io::Result<Option<Self>> {
        let Some(path) = std::env::var_os(INSPECT_DATA_DIR_VAR).map(PathBuf::from) else {
            return Ok(None);
        };
        if !path.is_absolute() {
            return Err(invalid("Use an absolute path for the app data directory."));
        }
        Self::new(&path).map(Some)
    }

    /// Use the app data directory selected at startup.
    pub(super) fn new(app_data: &Path) -> io::Result<Self> {
        let app_data = app_data.canonicalize()?;
        let root = app_data.join(DIRECTORY);
        #[cfg(unix)]
        let directory = directory::Directory::create(&root)?;
        #[cfg(not(unix))]
        let root_id = {
            #[cfg(windows)]
            let _ancestry = pin_directories(&app_data)?;
            match fs::create_dir(&root) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
            directory_id(&root)?
        };
        Ok(Self {
            root,
            #[cfg(unix)]
            directory,
            #[cfg(not(unix))]
            root_id,
            compatibility: compatibility(
                SCHEMA,
                SPEC,
                env!("SPLOT_CODEC_REVISION"),
                env!("CARGO_PKG_VERSION"),
                POLICY,
            ),
            max_bytes: MAX_BYTES,
        })
    }

    /// A cheap test before a worker reads the complete source for verification.
    pub(super) fn has_candidate(&self, source_len: u64) -> io::Result<bool> {
        let prefix = format!("{ENTRY_PREFIX}{source_len:016x}-");
        Ok(self.entries()?.iter().any(|entry| {
            entry.owned
                && entry
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&prefix))
        }))
    }

    /// Open a candidate only after the worker verifies the complete source.
    pub(super) fn open_entry(
        &self,
        framing: Framing,
        digest: SourceDigest,
    ) -> io::Result<Option<EntryReader>> {
        let _lock = self.lock()?;
        let path = self.path(framing, &digest)?;
        let Some(file) = self.open_regular(&path)? else {
            return Ok(None);
        };
        if file.metadata()?.len() > self.max_bytes {
            return Ok(None);
        }
        self.check_file(&path, &file)?;
        file.try_lock_shared().map_err(io::Error::from)?;
        let mut file = FileLease::leased(file);
        let Some(payload_end) = file.metadata()?.len().checked_sub(SHA_BYTES as u64) else {
            return Ok(None);
        };
        let header = self.header();
        if payload_end > self.max_bytes || payload_end < header.len() as u64 + 4 {
            return Ok(None);
        }
        let mut read_header = vec![0; header.len()];
        file.read_exact(&mut read_header)?;
        if read_header != header {
            return Ok(None);
        }
        // The document retains this lease. Eviction must keep its bytes accounted for.
        Ok(Some(EntryReader {
            file: Some(file),
            payload_end,
            left: payload_end - header.len() as u64,
            hash: Sha256::new_with_prefix(&header),
            header,
            framing,
            digest,
        }))
    }

    #[cfg(test)]
    fn load(
        &self,
        framing: Framing,
        digest: &SourceDigest,
        cancelled: &dyn Fn() -> bool,
    ) -> io::Result<Option<(FileLease, DocumentSummary, u64)>> {
        let Some(mut entry) = self.open_entry(framing, digest.clone())? else {
            return Ok(None);
        };
        loop {
            match entry.advance(cancelled)? {
                VerifiedEntry::Reading => {}
                VerifiedEntry::Invalid => return Ok(None),
                VerifiedEntry::Complete(file, summary, end) => {
                    return Ok(Some((file, *summary, end)));
                }
            }
        }
    }

    /// Publish one complete entry. Evict old entries before reserving disk for staging.
    pub(super) fn store(
        &self,
        summary: &DocumentSummary,
        index_bytes: u64,
        write_index: impl FnOnce(&mut dyn Write) -> io::Result<()>,
        cancelled: &dyn Fn() -> bool,
    ) -> io::Result<()> {
        let Some(digest) = complete_digest(summary) else {
            return Ok(());
        };
        let path = self.path(summary.framing, digest)?;
        let header = self.header();
        let summary = serde_json::to_vec(summary)?;
        if summary.len() > MAX_SUMMARY {
            return Err(invalid("The cache summary exceeds its byte limit."));
        }
        let length = u32::try_from(summary.len())
            .map_err(|_| invalid("The cache summary exceeds its byte limit."))?;
        let overhead = header.len() + 4 + summary.len() + SHA_BYTES;
        let Some(required) = index_bytes.checked_add(overhead as u64) else {
            return Err(invalid("The cache entry exceeds its byte limit."));
        };
        if required > self.max_bytes {
            return Err(invalid("The cache entry exceeds its byte limit."));
        }
        let _lock = self.lock()?;
        check_cancel(cancelled)?;
        let staging_bytes = self.remove_staging()?;
        self.evict_for(&path, required, staging_bytes)?;

        self.check_root()?;
        let mut temp = self.stage()?;
        let write_entry = |temp: &mut tempfile::NamedTempFile| {
            let mut writer = CheckedWriter {
                file: temp.as_file_mut(),
                remaining: required - SHA_BYTES as u64,
                hash: Sha256::new(),
                cancelled,
            };
            writer.write_all(&header)?;
            writer.write_all(&length.to_le_bytes())?;
            writer.write_all(&summary)?;
            write_index(&mut writer)?;
            if writer.remaining != 0 {
                return Err(invalid(
                    "The cache writer returned an incorrect byte count.",
                ));
            }
            let checksum = writer.hash.finalize();
            temp.write_all(&checksum)?;
            temp.as_file().sync_all()?;
            check_cancel(cancelled)?;
            self.check_root()?;
            self.check_file(temp.path(), temp.as_file())
        };
        if let Err(error) = write_entry(&mut temp) {
            let _ = self.remove_file(temp.path(), temp.as_file());
            return Err(error);
        }
        self.publish(temp, &path)
    }

    fn stage(&self) -> io::Result<tempfile::NamedTempFile> {
        let mut builder = tempfile::Builder::new();
        builder
            .prefix(STAGING_PREFIX)
            .rand_bytes(RANDOM_LEN)
            .disable_cleanup(true);
        #[cfg(unix)]
        {
            builder.make_in(&self.root, |path| {
                self.directory.create_file(entry_name_of(path)?)
            })
        }
        #[cfg(not(unix))]
        {
            builder.tempfile_in(&self.root)
        }
    }

    fn publish(&self, temp: tempfile::NamedTempFile, path: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            let outcome = self.directory.publish(
                entry_name_of(temp.path())?,
                entry_name_of(path)?,
                temp.as_file(),
            );
            if outcome.is_err() {
                let _ = self.remove_file(temp.path(), temp.as_file());
            }
            drop(temp);
            outcome
        }
        #[cfg(not(unix))]
        {
            if let Err(error) = temp.persist_noclobber(path) {
                let _ = self.remove_file(error.file.path(), error.file.as_file());
                return Err(error.error);
            }
            Ok(())
        }
    }

    fn evict_for(&self, path: &Path, required: u64, staging_bytes: u64) -> io::Result<()> {
        let mut entries = self.entries()?;
        let mut used = entries
            .iter()
            .fold(staging_bytes, |sum, entry| sum.saturating_add(entry.bytes));
        // Reserve enough bytes before changing the directory. A lease keeps its entry
        // in the budget but prevents eviction.
        entries.sort_unstable_by_key(|entry| (entry.path != path, entry.modified));
        let mut count = entries.len();
        let mut evict = Vec::new();
        for entry in entries {
            let replacing = entry.path == path;
            if !replacing && used <= self.max_bytes - required && count < MAX_ENTRIES {
                break;
            }
            let file = if entry.owned {
                self.exclusive_entry(&entry.path)?
            } else {
                None
            };
            let Some(file) = file else {
                if replacing {
                    return Err(busy());
                }
                continue;
            };
            used -= entry.bytes;
            count -= 1;
            evict.push((entry.path, file));
        }
        if used > self.max_bytes - required || count >= MAX_ENTRIES {
            return Err(busy());
        }
        for (path, file) in evict {
            self.remove_file(&path, &file)?;
        }
        Ok(())
    }

    pub(super) fn clear(&self) -> io::Result<()> {
        let _lock = self.lock()?;
        let mut remove = Vec::new();
        for entry in self.entries()?.into_iter().filter(|entry| entry.owned) {
            let file = self.exclusive_entry(&entry.path)?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "Close the documents that use the cache. Try again.",
                )
            })?;
            remove.push((entry.path, file));
        }
        self.remove_staging()?;
        for (path, file) in remove {
            self.remove_file(&path, &file)?;
        }
        Ok(())
    }

    #[cfg(unix)]
    fn check_root(&self) -> io::Result<()> {
        self.directory.check_current(&self.root)
    }

    #[cfg(not(unix))]
    fn check_root(&self) -> io::Result<()> {
        if directory_id(&self.root)? != self.root_id {
            return Err(invalid("The cache directory changed."));
        }
        Ok(())
    }

    #[cfg(unix)]
    fn check_file(&self, path: &Path, file: &File) -> io::Result<()> {
        self.directory.check_file(entry_name_of(path)?, file)
    }

    #[cfg(not(unix))]
    fn check_file(&self, path: &Path, file: &File) -> io::Result<()> {
        self.check_root()?;
        if !owned_regular(&fs::symlink_metadata(path)?)
            || FileId::of_path(path)? != FileId::of_file(file, path)?
        {
            return Err(invalid("The cache file changed."));
        }
        self.check_root()
    }

    #[cfg(unix)]
    fn open_regular(&self, path: &Path) -> io::Result<Option<File>> {
        self.directory.regular(entry_name_of(path)?)
    }

    #[cfg(not(unix))]
    fn open_regular(&self, path: &Path) -> io::Result<Option<File>> {
        self.check_root()?;
        match fs::symlink_metadata(path) {
            Ok(metadata) if owned_regular(&metadata) => File::open(path).map(Some),
            Ok(_) => Ok(None),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn exclusive_entry(&self, path: &Path) -> io::Result<Option<FileLease>> {
        let Some(file) = self.open_regular(path)? else {
            return Err(invalid("The cache file changed."));
        };
        self.check_file(path, &file)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(error)) => return Err(error),
        }
        let mut file = FileLease::leased(file);
        if !has_magic(&mut file)? {
            return Err(invalid("The cache file changed."));
        }
        Ok(Some(file))
    }

    #[cfg(unix)]
    fn remove_file(&self, path: &Path, file: &File) -> io::Result<()> {
        self.directory.remove(entry_name_of(path)?, file)
    }

    #[cfg(not(unix))]
    fn remove_file(&self, path: &Path, file: &File) -> io::Result<()> {
        self.check_file(path, file)?;
        fs::remove_file(path)
    }

    fn header(&self) -> Vec<u8> {
        [MAGIC.as_slice(), &self.compatibility].concat()
    }

    fn path(&self, framing: Framing, digest: &SourceDigest) -> io::Result<PathBuf> {
        if !lower_hex(&digest.sha256, 64) {
            return Err(invalid("The source digest is invalid."));
        }
        let mut key = Sha256::new();
        key.update(self.compatibility);
        key.update(match framing {
            Framing::Ivf => b"ivf".as_slice(),
            Framing::AnnexB => b"annex-b".as_slice(),
        });
        key.update(digest.sha256.as_bytes());
        let key = hex(&key.finalize());
        Ok(self
            .root
            .join(format!("{ENTRY_PREFIX}{:016x}-{key}", digest.bytes)))
    }

    fn lock(&self) -> io::Result<DirectoryLock> {
        #[cfg(windows)]
        let ancestry = pin_directories(&self.root)?;
        self.check_root()?;
        let path = self.root.join("cache.lock");
        #[cfg(unix)]
        let file = self.directory.lock_file(entry_name_of(&path)?)?;
        #[cfg(not(unix))]
        let file = self.open_lock(&path)?;
        file.try_lock().map_err(io::Error::from)?;
        let file = FileLease::leased(file);
        #[cfg(windows)]
        {
            Ok(DirectoryLock {
                _file: file,
                _ancestry: ancestry,
            })
        }
        #[cfg(not(windows))]
        {
            Ok(file)
        }
    }

    #[cfg(not(unix))]
    fn open_lock(&self, path: &Path) -> io::Result<File> {
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;

            // Keep the locked directory nonempty while cleanup is active.
            const FILE_SHARE_READ_WRITE: u32 = 0x0003;
            options.share_mode(FILE_SHARE_READ_WRITE);
        }
        let file = match options.clone().create_new(true).open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                if !owned_regular(&fs::symlink_metadata(path)?) {
                    return Err(invalid("The cache lock is not a regular file."));
                }
                options.open(path)?
            }
            Err(e) => return Err(e),
        };
        self.check_file(path, &file)?;
        Ok(file)
    }

    #[cfg(unix)]
    fn names(&self) -> io::Result<impl Iterator<Item = io::Result<std::ffi::OsString>>> {
        self.directory.list()
    }

    #[cfg(not(unix))]
    fn names(&self) -> io::Result<impl Iterator<Item = io::Result<std::ffi::OsString>>> {
        Ok(fs::read_dir(&self.root)?.map(|entry| entry.map(|item| item.file_name())))
    }

    fn entries(&self) -> io::Result<Vec<Entry>> {
        self.check_root()?;
        let mut entries = Vec::new();
        for name in self.names()? {
            let name = name?;
            if !name.to_str().is_some_and(entry_name) {
                continue;
            }
            let path = self.root.join(name);
            let Some(mut file) = self.open_regular(&path)? else {
                continue;
            };
            self.check_file(&path, &file)?;
            let metadata = file.metadata()?;
            entries.push(Entry {
                path,
                bytes: metadata.len(),
                modified: metadata.modified().unwrap_or(UNIX_EPOCH),
                owned: has_magic(&mut file)?,
            });
        }
        Ok(entries)
    }

    fn remove_staging(&self) -> io::Result<u64> {
        self.check_root()?;
        let mut remaining = 0_u64;
        for name in self.names()? {
            let name = name?;
            if !name
                .to_str()
                .and_then(|name| name.strip_prefix(STAGING_PREFIX))
                .is_some_and(|tail| {
                    tail.len() == RANDOM_LEN && tail.bytes().all(|b| b.is_ascii_alphanumeric())
                })
            {
                continue;
            }
            let path = self.root.join(name);
            let Some(mut file) = self.open_regular(&path)? else {
                continue;
            };
            self.check_file(&path, &file)?;
            // A killed process may leave a short header, including an empty file.
            let mut prefix = Vec::new();
            (&mut file)
                .take(MAGIC.len() as u64)
                .read_to_end(&mut prefix)?;
            if MAGIC.starts_with(&prefix) {
                self.remove_file(&path, &file)?;
            } else {
                remaining = remaining.saturating_add(file.metadata()?.len());
            }
        }
        Ok(remaining)
    }
}

impl EntryReader {
    /// Check up to 4 MiB of cache data. Never return unchecked facts.
    pub(super) fn advance(&mut self, cancelled: &dyn Fn() -> bool) -> io::Result<VerifiedEntry> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| invalid("The cache entry was already read."))?;
        let amount = self.left.min(VERIFY_BYTES);
        let mut content = (&mut **file).take(amount);
        let mut bytes = [0; 16 * 1024];
        let mut read = 0;
        loop {
            check_cancel(cancelled)?;
            let count = content.read(&mut bytes)?;
            if count == 0 {
                break;
            }
            self.hash.update(&bytes[..count]);
            read += count as u64;
        }
        if read != amount {
            return Ok(VerifiedEntry::Invalid);
        }
        self.left -= read;
        if self.left != 0 {
            return Ok(VerifiedEntry::Reading);
        }
        let mut checksum = [0; SHA_BYTES];
        file.read_exact(&mut checksum)?;
        let computed: [u8; SHA_BYTES] = std::mem::take(&mut self.hash).finalize().into();
        if checksum != computed {
            return Ok(VerifiedEntry::Invalid);
        }
        file.seek(SeekFrom::Start(self.header.len() as u64))?;
        let mut length = [0; 4];
        file.read_exact(&mut length)?;
        let length = u32::from_le_bytes(length) as usize;
        if length > MAX_SUMMARY || file.stream_position()? + length as u64 > self.payload_end {
            return Ok(VerifiedEntry::Invalid);
        }
        let mut summary = vec![0; length];
        file.read_exact(&mut summary)?;
        let summary: DocumentSummary = serde_json::from_slice(&summary)?;
        if summary.framing != self.framing || complete_digest(&summary) != Some(&self.digest) {
            return Ok(VerifiedEntry::Invalid);
        }
        let _ = file.set_times(FileTimes::new().set_modified(SystemTime::now()));
        Ok(VerifiedEntry::Complete(
            self.file
                .take()
                .ok_or_else(|| invalid("The cache entry was already read."))?,
            Box::new(summary),
            self.payload_end,
        ))
    }
}

struct CheckedWriter<'a> {
    file: &'a mut File,
    remaining: u64,
    hash: Sha256,
    cancelled: &'a dyn Fn() -> bool,
}

impl Write for CheckedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        check_cancel(self.cancelled)?;
        if bytes.len() as u64 > self.remaining {
            return Err(invalid("The cache writer exceeded its byte limit."));
        }
        let count = self.file.write(bytes)?;
        self.hash.update(&bytes[..count]);
        self.remaining -= count as u64;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

fn compatibility(
    schema: u32,
    spec: &str,
    codec: &str,
    engine: &str,
    policy: &str,
) -> [u8; SHA_BYTES] {
    let mut hash = Sha256::new_with_prefix(schema.to_le_bytes());
    for value in [spec, codec, engine, policy] {
        hash.update(value.as_bytes());
        hash.update(b"\0");
    }
    hash.finalize().into()
}

fn complete_digest(summary: &DocumentSummary) -> Option<&SourceDigest> {
    summary.digest.as_deref().filter(|digest| {
        matches!(summary.outcome, ScanOutcome::Complete) && digest.bytes == summary.fingerprint.len
    })
}

fn lower_hex(text: &str, len: usize) -> bool {
    text.len() == len
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn entry_name(name: &str) -> bool {
    name.strip_prefix(ENTRY_PREFIX)
        .and_then(|tail| tail.split_once('-'))
        .is_some_and(|(len, digest)| lower_hex(len, 16) && lower_hex(digest, 64))
}

fn has_magic(file: &mut File) -> io::Result<bool> {
    let mut bytes = [0; MAGIC.len()];
    match file.read_exact(&mut bytes) {
        Ok(()) => Ok(&bytes == MAGIC),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(unix)]
fn entry_name_of(path: &Path) -> io::Result<&OsStr> {
    path.file_name()
        .ok_or_else(|| invalid("The cache entry has no name."))
}

#[cfg(windows)]
fn pin_directories(path: &Path) -> io::Result<Vec<File>> {
    use std::os::windows::fs::OpenOptionsExt as _;

    const FILE_LIST_DIRECTORY: u32 = 0x0001;
    const FILE_READ_ATTRIBUTES: u32 = 0x0080;
    const FILE_SHARE_READ: u32 = 0x0001;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

    // Hold the path from the volume root. The handles deny rename and keep
    // its ancestors nonempty, which prevents conversion to reparse points.
    let mut directories = Vec::new();
    let mut ancestry: Vec<_> = path.ancestors().filter(|part| part.is_absolute()).collect();
    ancestry.reverse();
    for dir in ancestry {
        let file = OpenOptions::new()
            .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(dir)?;
        let metadata = file.metadata()?;
        if !metadata.is_dir() || is_link(&metadata) {
            return Err(invalid(
                "The cache directory path contains a reparse point.",
            ));
        }
        directories.push(file);
    }
    Ok(directories)
}

#[cfg(not(unix))]
fn directory_id(root: &Path) -> io::Result<FileId> {
    let metadata = fs::symlink_metadata(root)?;
    if !metadata.is_dir() || is_link(&metadata) {
        return Err(invalid("The app cache path is not a local directory."));
    }
    let id = FileId::of_path(root)?;
    let after = fs::symlink_metadata(root)?;
    if !after.is_dir() || is_link(&after) {
        return Err(invalid("The cache directory changed."));
    }
    Ok(id)
}

#[cfg(not(unix))]
fn owned_regular(metadata: &Metadata) -> bool {
    metadata.is_file() && !is_link(metadata)
}

#[cfg(not(unix))]
fn is_link(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // FILE_ATTRIBUTE_REPARSE_POINT also covers Windows junctions.
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    metadata.file_type().is_symlink()
}

fn check_cancel(cancelled: &dyn Fn() -> bool) -> io::Result<()> {
    if cancelled() {
        return Err(io::Error::other("The cache operation was cancelled."));
    }
    Ok(())
}

fn busy() -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        "The cache has files in use or cannot reserve space.",
    )
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use studio_domain::job::SourceFingerprint;

    const DATA: &[u8] = b"paged index bytes";

    fn present<T>(value: Option<T>) -> io::Result<T> {
        value.ok_or_else(|| io::Error::other("The test value is missing."))
    }

    fn error_is<T>(result: &io::Result<T>, kind: io::ErrorKind) -> bool {
        result.as_ref().is_err_and(|error| error.kind() == kind)
    }

    fn summary(content: &[u8]) -> DocumentSummary {
        DocumentSummary {
            fingerprint: SourceFingerprint {
                len: content.len() as u64,
                modified: None,
            },
            digest: Some(Box::new(SourceDigest {
                sha256: hex(&Sha256::digest(content)),
                bytes: content.len() as u64,
            })),
            framing: Framing::AnnexB,
            ivf_header: None,
            elements: 1,
            packets: 0,
            obus: 1,
            unparsed: 0,
            obu_types: vec![],
            layers: vec![],
            outcome: ScanOutcome::Complete,
        }
    }

    fn store(cache: &Cache, summary: &DocumentSummary) -> io::Result<()> {
        cache.store(
            summary,
            DATA.len() as u64,
            |writer| writer.write_all(DATA),
            &|| false,
        )
    }

    #[test]
    fn warm_index_requires_complete_matching_content_and_engine() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let cache = Cache::new(dir.path())?;
        let summary = summary(b"media");
        let digest = present(summary.digest.as_deref())?;
        assert!(!cache.has_candidate(digest.bytes)?);
        let mut partial = summary.clone();
        present(partial.digest.as_mut())?.bytes -= 1;
        store(&cache, &partial)?;
        assert!(!cache.has_candidate(digest.bytes)?);
        store(&cache, &summary)?;
        let mut next_process = Cache::new(dir.path())?;
        assert!(next_process.has_candidate(digest.bytes)?);
        let (file, cached, end) = present(next_process.load(summary.framing, digest, &|| false)?)?;
        assert_eq!(cached, summary);
        let mut file = file;
        let bytes = end - file.stream_position()?;
        let mut index = Vec::new();
        (&mut *file).take(bytes).read_to_end(&mut index)?;
        assert_eq!(index, DATA);
        let replacement = SourceDigest {
            sha256: hex(&Sha256::digest(b"other")),
            bytes: digest.bytes,
        };
        assert!(
            next_process
                .load(summary.framing, &replacement, &|| false)?
                .is_none()
        );
        assert!(
            next_process
                .load(Framing::Ivf, digest, &|| false)?
                .is_none()
        );
        next_process.compatibility = compatibility(
            SCHEMA + 1,
            SPEC,
            env!("SPLOT_CODEC_REVISION"),
            env!("CARGO_PKG_VERSION"),
            POLICY,
        );
        assert!(
            next_process
                .load(summary.framing, digest, &|| false)?
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn compatibility_covers_spec_codec_engine_and_policy() {
        let original = compatibility(1, SPEC, "codec", "engine", POLICY);
        for changed in [
            compatibility(1, "another specification", "codec", "engine", POLICY),
            compatibility(1, SPEC, "new codec", "engine", POLICY),
            compatibility(1, SPEC, "codec", "new engine", POLICY),
            compatibility(1, SPEC, "codec", "engine", "new policy"),
        ] {
            assert_ne!(changed, original);
        }
    }

    #[test]
    fn corrupt_or_failed_writes_cannot_load() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let cache = Cache::new(dir.path())?;
        let summary = summary(b"media");
        let digest = present(summary.digest.as_deref())?;
        let path = cache.path(summary.framing, digest)?;
        store(&cache, &summary)?;
        let mut file = OpenOptions::new().write(true).open(&path)?;
        let len = file.metadata()?.len();
        file.seek(SeekFrom::End(-1))?;
        file.write_all(b"\0")?;
        file.set_len(len - 1)?;
        drop(file);
        assert!(cache.load(summary.framing, digest, &|| false)?.is_none());

        let failed = cache.store(
            &summary,
            DATA.len() as u64,
            |writer| {
                writer.write_all(&DATA[..1])?;
                Err(io::Error::new(
                    io::ErrorKind::StorageFull,
                    "The disk is full.",
                ))
            },
            &|| false,
        );
        assert!(error_is(&failed, io::ErrorKind::StorageFull));
        assert!(!path.exists());
        assert_eq!(fs::read_dir(&cache.root)?.count(), 1);
        let cancelled = cache.store(&summary, DATA.len() as u64, |_| unreachable!(), &|| true);
        assert!(cancelled.is_err());
        assert!(!path.exists());
        store(&cache, &summary)?;
        assert!(cache.load(summary.framing, digest, &|| false)?.is_some());
        Ok(())
    }

    #[test]
    fn eviction_and_clear_only_remove_owned_entries() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut cache = Cache::new(dir.path())?;
        let first = summary(b"media");
        let second = summary(b"other");
        store(&cache, &first)?;
        cache.max_bytes = cache.entries()?[0].bytes;
        let unrelated = cache.root.join("source.obu");
        let outside = dir.path().join("output.obu");
        let looks_owned = cache
            .root
            .join(format!("{ENTRY_PREFIX}{:016x}-{}", 1, "a".repeat(64)));
        fs::write(&looks_owned, b"media")?;
        cache.max_bytes += 5;
        let stale = cache.root.join(".stage-0123456789abcdef");
        let noncache_stage = cache.root.join(".stage-abcdef0123456789");
        fs::write(&noncache_stage, b"media")?;
        fs::write(&unrelated, b"source")?;
        fs::write(&outside, b"output")?;
        fs::write(&stale, &MAGIC[..6])?;
        assert!(error_is(&store(&cache, &second), io::ErrorKind::WouldBlock));
        cache.max_bytes += 5;
        store(&cache, &second)?;
        assert!(!stale.exists());
        assert!(
            cache
                .load(first.framing, present(first.digest.as_deref())?, &|| false)?
                .is_none()
        );
        assert!(
            cache
                .load(second.framing, present(second.digest.as_deref())?, &|| {
                    false
                })?
                .is_some()
        );
        assert_eq!(
            cache.entries()?.iter().filter(|entry| entry.owned).count(),
            1
        );
        cache.clear()?;
        assert_eq!(
            cache.entries()?.iter().filter(|entry| entry.owned).count(),
            0
        );
        assert_eq!(fs::read(unrelated)?, b"source");
        assert_eq!(fs::read(outside)?, b"output");
        assert_eq!(fs::read(looks_owned)?, b"media");
        assert_eq!(fs::read(noncache_stage)?, b"media");
        Ok(())
    }

    #[test]
    fn verification_yields_between_chunks_and_clear_refuses_active_writer() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let cache = Cache::new(dir.path())?;
        let summary = summary(b"media");
        let digest = present(summary.digest.as_deref())?;
        let bytes = [0; 16 * 1024];
        let copies = VERIFY_BYTES / bytes.len() as u64 + 1;
        cache.store(
            &summary,
            copies * bytes.len() as u64,
            |writer| {
                assert!(error_is(&cache.clear(), io::ErrorKind::WouldBlock));
                for _ in 0..copies {
                    writer.write_all(&bytes)?;
                }
                Ok(())
            },
            &|| false,
        )?;
        let mut pending = present(cache.open_entry(summary.framing, digest.clone())?)?;
        assert!(matches!(
            pending.advance(&|| false)?,
            VerifiedEntry::Reading
        ));
        assert!(pending.advance(&|| true).is_err());
        let mut pending = present(cache.open_entry(summary.framing, digest.clone())?)?;
        assert!(matches!(
            pending.advance(&|| false)?,
            VerifiedEntry::Reading
        ));
        assert!(matches!(
            pending.advance(&|| false)?,
            VerifiedEntry::Complete(_, _, _)
        ));
        Ok(())
    }

    #[test]
    fn warm_leases_count_against_budget_and_block_clear() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut cache = Cache::new(dir.path())?;
        let first = summary(b"first");
        let second = summary(b"other");
        store(&cache, &first)?;
        cache.max_bytes = cache.entries()?[0].bytes;
        let digest = present(first.digest.as_deref())?;
        let reader = present(cache.open_entry(first.framing, digest.clone())?)?;
        assert!(error_is(&store(&cache, &second), io::ErrorKind::WouldBlock));
        assert!(error_is(&cache.clear(), io::ErrorKind::WouldBlock));
        drop(reader);
        let (leased, _, _) = present(cache.load(first.framing, digest, &|| false)?)?;
        let another_worker = Cache::new(dir.path())?;
        assert!(error_is(&another_worker.clear(), io::ErrorKind::WouldBlock));
        assert!(error_is(&store(&cache, &first), io::ErrorKind::WouldBlock));
        assert_eq!(cache.entries()?.len(), 1);
        drop(leased);
        store(&cache, &second)?;
        assert!(cache.load(first.framing, digest, &|| false)?.is_none());
        cache.clear()?;
        assert_eq!(cache.entries()?.len(), 0);
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn an_active_cache_operation_pins_its_windows_ancestors() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let app_data = dir.path().join("app");
        let moved_app_data = dir.path().join("app-moved");
        fs::create_dir(&app_data)?;
        let cache = Cache::new(&app_data)?;
        let summary = summary(b"media");
        let lock = cache.lock()?;
        assert!(fs::rename(&app_data, &moved_app_data).is_err());
        drop(lock);

        cache.store(
            &summary,
            DATA.len() as u64,
            |writer| {
                assert!(fs::rename(&app_data, &moved_app_data).is_err());
                writer.write_all(DATA)
            },
            &|| false,
        )?;
        assert_eq!(cache.entries()?.len(), 1);
        fs::rename(&app_data, &moved_app_data)?;
        assert!(cache.clear().is_err());
        assert!(moved_app_data.join(DIRECTORY).is_dir());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn cache_unlocks_even_if_a_child_has_the_file_description() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let cache = Cache::new(dir.path())?;
        let lock = cache.lock()?;
        let inherited_lock = lock.try_clone()?;
        drop(lock);
        // The duplicate models a child that has not executed its new process.
        let next = cache.lock()?;
        drop(next);
        drop(inherited_lock);

        let summary = summary(b"media");
        store(&cache, &summary)?;
        let digest = present(summary.digest.as_deref())?;
        let (lease, _, _) = present(cache.load(summary.framing, digest, &|| false)?)?;
        let inherited_lease = lease.try_clone()?;
        drop(lease);
        cache.clear()?;
        drop(inherited_lease);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn replaced_root_does_not_redirect_publication_or_cleanup() -> io::Result<()> {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir()?;

        let app_data = dir.path().join("app");
        let archived_app_data = dir.path().join("old-app");
        let outside = dir.path().join("outside");
        fs::create_dir(&app_data)?;
        fs::create_dir_all(outside.join(DIRECTORY))?;
        let cache = Cache::new(&app_data)?;
        let summary = summary(b"media");
        let mut replacement_stage = None;
        let result = cache.store(
            &summary,
            DATA.len() as u64,
            |writer| {
                writer.write_all(DATA)?;
                let name = present(fs::read_dir(&cache.root)?.find_map(|item| {
                    let item = item.ok()?;
                    let name = item.file_name();
                    name.to_string_lossy()
                        .starts_with(STAGING_PREFIX)
                        .then_some(name)
                }))?;
                fs::rename(&app_data, &archived_app_data)?;
                symlink(&outside, &app_data)?;
                let path = outside.join(DIRECTORY).join(name);
                fs::write(&path, b"replacement media")?;
                replacement_stage = Some(path);
                Ok(())
            },
            &|| false,
        );
        assert!(result.is_err());
        assert!(cache.clear().is_err());
        assert_eq!(fs::read(present(replacement_stage)?)?, b"replacement media");
        assert_eq!(fs::read_dir(archived_app_data.join(DIRECTORY))?.count(), 1);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn cache_operations_reject_links() -> io::Result<()> {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        let media = outside.path().join("source.obu");
        fs::write(&media, b"media")?;
        let root = dir.path().join(DIRECTORY);
        symlink(outside.path(), &root)?;
        assert!(Cache::new(dir.path()).is_err());
        fs::remove_file(root)?;
        let cache = Cache::new(dir.path())?;
        let summary = summary(b"media");
        let digest = present(summary.digest.as_deref())?;
        let entry = cache.path(summary.framing, digest)?;
        symlink(&media, &entry)?;
        assert!(cache.load(summary.framing, digest, &|| false)?.is_none());
        assert!(store(&cache, &summary).is_err());
        cache.clear()?;
        assert!(entry.is_symlink());
        assert_eq!(fs::read(&media)?, b"media");
        let lock = cache.root.join("cache.lock");
        fs::remove_file(&lock)?;
        symlink(&media, &lock)?;
        assert!(cache.clear().is_err());
        assert_eq!(fs::read(&media)?, b"media");
        Ok(())
    }
}
