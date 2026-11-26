// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Verify source and cache bytes in bounded worker steps.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use studio_domain::analysis::{AnalysisFailure, DocumentSummary, ScanOutcome};
use studio_domain::job::SourceFingerprint;

use super::cache::{Cache, EntryReader, VerifiedEntry};
use super::index::Index;
use super::{FileId, HashReader, Refusal};

const SOURCE_STEP_BYTES: u64 = 4 << 20;
static CACHE_MUTATION: AtomicBool = AtomicBool::new(false);

pub(super) struct CacheSession {
    cache: Arc<Cache>,
    verification: Option<Verification>,
    cancelled: Arc<AtomicBool>,
}

enum Verification {
    Source(HashReader<File>),
    Entry(Box<EntryReader>),
}

pub(super) enum Progress {
    Cold,
    Reading,
    Warm(Index, DocumentSummary),
}

impl fmt::Debug for CacheSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CacheSession").finish_non_exhaustive()
    }
}

impl CacheSession {
    pub(super) fn new(path: &Path, id: &FileId, summary: &DocumentSummary) -> Option<Self> {
        let cache = Cache::from_env().ok().flatten()?;
        let verification = source_candidate(&cache, path, id, summary)
            .ok()
            .flatten()
            .map(Verification::Source);
        Some(Self {
            cache: Arc::new(cache),
            verification,
            cancelled: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Read at most one verification chunk. A failed cache check permits a cold scan.
    /// The document checks source identity before and after this call.
    pub(super) fn advance(
        &mut self,
        summary: &DocumentSummary,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Progress, Refusal> {
        let result = self.verify(summary, cancelled);
        if cancelled() {
            return Err(Refusal::new(
                AnalysisFailure::Cancelled,
                "The scan was cancelled.",
            ));
        }
        Ok(result.unwrap_or(Progress::Cold))
    }

    fn verify(
        &mut self,
        summary: &DocumentSummary,
        cancelled: &dyn Fn() -> bool,
    ) -> io::Result<Progress> {
        let Some(verification) = self.verification.take() else {
            return Ok(Progress::Cold);
        };
        let next = match verification {
            Verification::Source(mut reader) => {
                let mut bytes = [0; 16 * 1024];
                let mut read = 0;
                while read < SOURCE_STEP_BYTES && reader.bytes < summary.fingerprint.len {
                    if cancelled() {
                        return Ok(Progress::Cold);
                    }
                    let count = reader.read(&mut bytes)?;
                    if count == 0 {
                        return Ok(Progress::Cold);
                    }
                    read += count as u64;
                }
                if reader.bytes < summary.fingerprint.len {
                    Verification::Source(reader)
                } else {
                    let digest = reader.finish();
                    if digest.bytes != summary.fingerprint.len {
                        return Ok(Progress::Cold);
                    }
                    let Some(entry) = self.cache.open_entry(summary.framing, digest)? else {
                        return Ok(Progress::Cold);
                    };
                    Verification::Entry(Box::new(entry))
                }
            }
            Verification::Entry(mut entry) => match entry.advance(cancelled)? {
                VerifiedEntry::Reading => Verification::Entry(entry),
                VerifiedEntry::Invalid => return Ok(Progress::Cold),
                VerifiedEntry::Complete(file, mut cached, end) => {
                    cached.fingerprint = summary.fingerprint;
                    let index = Index::open_cache(file, &cached, end)?;
                    return Ok(Progress::Warm(index, cached));
                }
            },
        };
        self.verification = Some(next);
        Ok(Progress::Reading)
    }

    /// Publish an optional index without occupying the request loop.
    pub(super) fn publish(&self, summary: &DocumentSummary, index: &Index) {
        if !matches!(summary.outcome, ScanOutcome::Complete)
            || CACHE_MUTATION
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }
        let permission = PublishPermission;
        let Ok(export) = index.export() else {
            return;
        };
        let summary = summary.clone();
        let cache = Arc::clone(&self.cache);
        let cancelled = Arc::clone(&self.cancelled);
        // One optional writer per worker. Dropping the closure after a spawn error
        // releases the slot as well.
        let _ = std::thread::Builder::new()
            .name("analysis-index".into())
            .spawn(move || {
                let _permission = permission;
                let _ = cache.store(
                    &summary,
                    export.len(),
                    |output| export.write(output),
                    &|| cancelled.load(Ordering::Relaxed),
                );
            });
    }
}

impl Drop for CacheSession {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

struct PublishPermission;

impl Drop for PublishPermission {
    fn drop(&mut self) {
        CACHE_MUTATION.store(false, Ordering::Release);
    }
}

fn source_candidate(
    cache: &Cache,
    path: &Path,
    id: &FileId,
    summary: &DocumentSummary,
) -> io::Result<Option<HashReader<File>>> {
    if !cache.has_candidate(summary.fingerprint.len)? {
        return Ok(None);
    }
    if !fs::metadata(path)?.is_file() {
        return Ok(None);
    }
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || FileId::of_file(&file, path)? != *id
        || SourceFingerprint::of(&metadata) != summary.fingerprint
    {
        return Ok(None);
    }
    Ok(Some(HashReader::new(file)))
}

pub(crate) fn clear_cache() -> Result<(), Refusal> {
    let operation = || {
        CACHE_MUTATION
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "The cache is busy."))?;
        let _permission = PublishPermission;
        if let Some(cache) = Cache::from_env()? {
            cache.clear()?;
        }
        Ok::<(), io::Error>(())
    };
    operation().map_err(|error| {
        Refusal::new(
            AnalysisFailure::Io,
            format!("Cannot clear the analysis index cache. {error}"),
        )
    })
}
