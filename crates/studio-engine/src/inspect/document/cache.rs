// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Complete structural indexes in the app-owned data directory.
//! Source identity requires a full content digest. The cache stores no decoded evidence.

use std::fs::{self, File, FileTimes, Metadata, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use studio_domain::analysis::{
    DocumentSummary, Framing, INSPECT_DATA_DIR_VAR, ScanOutcome, SourceDigest,
};

use crate::artifact::hex;

const DIRECTORY: &str = "analysis-index";
const ENTRY_PREFIX: &str = "index-";
const STAGING_PREFIX: &str = ".stage-";
const RANDOM_LEN: usize = 16;
const MAGIC: &[u8] = b"SplotStudio/index\0";
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
    compatibility: [u8; SHA_BYTES],
    max_bytes: u64,
}

struct Entry {
    path: PathBuf,
    bytes: u64,
    modified: SystemTime,
}

pub(super) struct EntryReader {
    file: File,
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
    Complete(File, DocumentSummary, u64),
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
        let root = app_data.canonicalize()?.join(DIRECTORY);
        match fs::create_dir(&root) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        check_directory(&root)?;
        Ok(Self {
            root,
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
            entry
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
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if !owned_regular(&metadata) || metadata.len() > self.max_bytes {
            return Ok(None);
        }
        let mut file = File::open(path)?;
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
        // An open file stays valid when the app evicts its directory entry.
        Ok(Some(EntryReader {
            file,
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
    ) -> io::Result<Option<(File, DocumentSummary, u64)>> {
        let Some(mut entry) = self.open_entry(framing, digest.clone())? else {
            return Ok(None);
        };
        loop {
            match entry.advance(cancelled)? {
                VerifiedEntry::Reading => {}
                VerifiedEntry::Invalid => return Ok(None),
                VerifiedEntry::Complete(file, summary, end) => {
                    return Ok(Some((file, summary, end)));
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
        let overhead = header.len() + 4 + summary.len() + SHA_BYTES;
        let Some(required) = index_bytes.checked_add(overhead as u64) else {
            return Err(invalid("The cache entry exceeds its byte limit."));
        };
        if required > self.max_bytes {
            return Err(invalid("The cache entry exceeds its byte limit."));
        }
        let _lock = self.lock()?;
        check_cancel(cancelled)?;
        self.remove_staging()?;
        let mut entries = self.entries()?;
        let mut used = entries
            .iter()
            .fold(0_u64, |sum, entry| sum.saturating_add(entry.bytes));
        // Remove the old copy first. The staging file counts towards the same limit.
        entries.sort_unstable_by_key(|entry| (entry.path != path, entry.modified));
        let mut count = entries.len();
        for entry in entries {
            if entry.path != path && used <= self.max_bytes - required && count < MAX_ENTRIES {
                break;
            }
            fs::remove_file(entry.path)?;
            used -= entry.bytes;
            count -= 1;
        }

        let mut temp = tempfile::Builder::new()
            .prefix(STAGING_PREFIX)
            .rand_bytes(RANDOM_LEN)
            .tempfile_in(&self.root)?;
        let mut writer = CheckedWriter {
            file: temp.as_file_mut(),
            remaining: required - SHA_BYTES as u64,
            hash: Sha256::new(),
            cancelled,
        };
        writer.write_all(&header)?;
        writer.write_all(&(summary.len() as u32).to_le_bytes())?;
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
        temp.persist_noclobber(path)?;
        Ok(())
    }

    pub(super) fn clear(&self) -> io::Result<()> {
        let _lock = self.lock()?;
        self.remove_staging()?;
        for entry in self.entries()? {
            fs::remove_file(entry.path)?;
        }
        Ok(())
    }

    fn header(&self) -> Vec<u8> {
        [MAGIC, &self.compatibility].concat()
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

    fn lock(&self) -> io::Result<File> {
        check_directory(&self.root)?;
        let path = self.root.join("cache.lock");
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                if !owned_regular(&fs::symlink_metadata(&path)?) {
                    return Err(invalid("The cache lock is not a regular file."));
                }
                OpenOptions::new().read(true).write(true).open(path)?
            }
            Err(e) => return Err(e),
        };
        file.try_lock().map_err(io::Error::from)?;
        Ok(file)
    }

    fn entries(&self) -> io::Result<Vec<Entry>> {
        check_directory(&self.root)?;
        let mut entries = Vec::new();
        for item in fs::read_dir(&self.root)? {
            let item = item?;
            if !item.file_name().to_str().is_some_and(entry_name) {
                continue;
            }
            let metadata = fs::symlink_metadata(item.path())?;
            if owned_regular(&metadata) {
                entries.push(Entry {
                    path: item.path(),
                    bytes: metadata.len(),
                    modified: metadata.modified().unwrap_or(UNIX_EPOCH),
                });
            }
        }
        Ok(entries)
    }

    fn remove_staging(&self) -> io::Result<()> {
        for item in fs::read_dir(&self.root)? {
            let item = item?;
            let name = item.file_name();
            let matches = name
                .to_str()
                .and_then(|name| name.strip_prefix(STAGING_PREFIX))
                .is_some_and(|tail| {
                    tail.len() == RANDOM_LEN && tail.bytes().all(|b| b.is_ascii_alphanumeric())
                });
            if matches && owned_regular(&fs::symlink_metadata(item.path())?) {
                fs::remove_file(item.path())?;
            }
        }
        Ok(())
    }
}

impl EntryReader {
    /// Check up to 4 MiB of cache data. Never return unchecked facts.
    pub(super) fn advance(&mut self, cancelled: &dyn Fn() -> bool) -> io::Result<VerifiedEntry> {
        let amount = self.left.min(VERIFY_BYTES);
        let mut content = (&mut self.file).take(amount);
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
        self.file.read_exact(&mut checksum)?;
        let computed: [u8; SHA_BYTES] = std::mem::take(&mut self.hash).finalize().into();
        if checksum != computed {
            return Ok(VerifiedEntry::Invalid);
        }
        self.file.seek(SeekFrom::Start(self.header.len() as u64))?;
        let mut length = [0; 4];
        self.file.read_exact(&mut length)?;
        let length = u32::from_le_bytes(length) as usize;
        if length > MAX_SUMMARY || self.file.stream_position()? + length as u64 > self.payload_end {
            return Ok(VerifiedEntry::Invalid);
        }
        let mut summary = vec![0; length];
        self.file.read_exact(&mut summary)?;
        let summary: DocumentSummary = serde_json::from_slice(&summary)?;
        if summary.framing != self.framing || complete_digest(&summary) != Some(&self.digest) {
            return Ok(VerifiedEntry::Invalid);
        }
        let _ = self
            .file
            .set_times(FileTimes::new().set_modified(SystemTime::now()));
        Ok(VerifiedEntry::Complete(
            self.file.try_clone()?,
            summary,
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

fn check_directory(root: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(root)?;
    if !metadata.is_dir() || is_link(&metadata) {
        return Err(invalid("The app cache path is not a local directory."));
    }
    Ok(())
}

fn owned_regular(metadata: &Metadata) -> bool {
    metadata.is_file() && !is_link(metadata)
}

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

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use studio_domain::job::SourceFingerprint;

    const DATA: &[u8] = b"paged index bytes";

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
        let digest = summary.digest.as_deref().unwrap();
        assert!(!cache.has_candidate(digest.bytes)?);
        let mut partial = summary.clone();
        partial.digest.as_mut().unwrap().bytes -= 1;
        store(&cache, &partial)?;
        assert!(!cache.has_candidate(digest.bytes)?);
        store(&cache, &summary)?;
        let mut next_process = Cache::new(dir.path())?;
        assert!(next_process.has_candidate(digest.bytes)?);
        let (file, cached, end) = next_process
            .load(summary.framing, digest, &|| false)?
            .unwrap();
        assert_eq!(cached, summary);
        let mut file = file;
        let bytes = end - file.stream_position()?;
        let mut index = Vec::new();
        file.take(bytes).read_to_end(&mut index)?;
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
        let digest = summary.digest.as_deref().unwrap();
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
        assert_eq!(failed.unwrap_err().kind(), io::ErrorKind::StorageFull);
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
        let stale = cache.root.join(".stage-0123456789abcdef");
        fs::write(&unrelated, b"source")?;
        fs::write(&outside, b"output")?;
        fs::write(&stale, b"interrupted index")?;
        store(&cache, &second)?;
        assert!(!stale.exists());
        assert!(
            cache
                .load(first.framing, first.digest.as_deref().unwrap(), &|| false)?
                .is_none()
        );
        assert!(
            cache
                .load(second.framing, second.digest.as_deref().unwrap(), &|| false)?
                .is_some()
        );
        assert_eq!(cache.entries()?.len(), 1);
        cache.clear()?;
        assert!(cache.entries()?.is_empty());
        assert_eq!(fs::read(unrelated)?, b"source");
        assert_eq!(fs::read(outside)?, b"output");
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
        let digest = summary.digest.as_deref().unwrap();
        let entry = cache.path(summary.framing, digest)?;
        symlink(&media, &entry)?;
        assert!(cache.load(summary.framing, digest, &|| false)?.is_none());
        assert!(store(&cache, &summary).is_err());
        cache.clear()?;
        assert!(entry.is_symlink());
        assert_eq!(fs::read(media)?, b"media");
        Ok(())
    }
}
