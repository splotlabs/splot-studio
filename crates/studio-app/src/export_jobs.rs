// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Frozen requests and durable state for exports from Analyze.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use studio_domain::analysis::IvfHeaderFact;
use studio_domain::export::{
    ExportArtifact, ExportDurability, ExportLayer, ExportOutcome, ExportProgress, ExportRange,
    ExportRequest, ExportSampleFormat, ExportSource as Source, ExportStage, ExportTemp,
};
use studio_domain::job::{Failure, FailureCode, WirePath};
use studio_domain::preview::GrainPolicy;

use crate::analysis::Document;
use crate::queue::PartialFate;
use crate::recovery;

/// An open Analyze document whose inspection read all source bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportSource {
    /// The current Analyze document ID.
    pub document: u64,
    /// The file's complete source identity.
    pub source: Source,
    /// The clock declared by the IVF header, when available.
    pub ivf_header: Option<IvfHeaderFact>,
}

impl ExportSource {
    pub(crate) fn from_document(document: &Document) -> Option<Self> {
        let summary = document.summary()?;
        let digest = summary
            .digest
            .as_deref()
            .filter(|digest| digest.bytes == summary.fingerprint.len)?;
        Some(Self {
            document: document.id(),
            source: Source {
                path: WirePath::from_path(document.path()),
                fingerprint: summary.fingerprint,
                digest: digest.clone(),
                framing: summary.framing,
            },
            ivf_header: summary.ivf_header,
        })
    }
}

/// The displayed Analyze output, captured before any file picker opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportFrameSource {
    /// The source of this picture.
    pub source: ExportSource,
    /// The accepted preview request generation.
    pub generation: u64,
    /// The zero-based output ordinal.
    pub ordinal: u64,
    /// The exact layer of this output.
    pub layer: ExportLayer,
    /// The sample format observed in this output.
    pub expected: ExportSampleFormat,
    /// The grain policy in use for this output.
    pub grain: GrainPolicy,
}

/// A row in Jobs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportJob {
    pub(crate) id: u64,
    pub(crate) request: ExportRequest,
    pub(crate) phase: Phase,
    pub(crate) temporary: Vec<ExportTemp>,
    pub(crate) published: Vec<ExportArtifact>,
    #[serde(default)]
    pub(crate) publication_possible: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    verified_frames: Option<u64>,
    #[serde(default)]
    pub(crate) cleanup: Vec<String>,
    #[serde(skip)]
    pub(crate) progress: Option<ExportProgress>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    Queued,
    Running(ExportStage),
    Cancelling,
    Finished(ExportOutcome),
    Interrupted(String),
}

impl Phase {
    pub(crate) const fn active(&self) -> bool {
        matches!(self, Self::Queued | Self::Running(_) | Self::Cancelling)
    }
}

impl ExportJob {
    fn queued(id: u64, request: ExportRequest) -> Self {
        Self {
            id,
            request,
            phase: Phase::Queued,
            temporary: Vec::new(),
            published: Vec::new(),
            publication_possible: false,
            verified_frames: None,
            cleanup: Vec::new(),
            progress: None,
        }
    }

    pub(crate) fn failure(&mut self, message: String) {
        let failure = Failure {
            code: FailureCode::Output,
            message,
        };
        self.phase = Phase::Finished(if self.published.is_empty() {
            ExportOutcome::Failed { failure }
        } else {
            ExportOutcome::Partial {
                frames: self.verified_frames.unwrap_or_default(),
                files: self.published.clone(),
                failure,
            }
        });
    }

    pub(crate) fn register(&mut self, file: ExportTemp) -> Result<(), String> {
        if !matches!(
            self.phase,
            Phase::Running(ExportStage::Checking) | Phase::Cancelling
        ) || !self
            .request
            .format
            .destinations()
            .any(|(kind, path)| file.kind == kind && &file.destination == path)
            || self
                .temporary
                .iter()
                .any(|temp| temp.kind == file.kind || temp.partial == file.partial)
            || file.partial == self.request.source.path
        {
            return Err("The worker sent an unexpected temporary file.".to_owned());
        }
        let partial = file
            .partial
            .to_path_buf()
            .map_err(|error| error.to_string())?;
        let destination = file
            .destination
            .to_path_buf()
            .map_err(|error| error.to_string())?;
        recovery::check_partial_path(&partial, &destination)?;
        self.temporary.push(file);
        Ok(())
    }

    pub(crate) fn prepared(&self, files: &[ExportTemp]) -> Result<(), String> {
        if !matches!(self.phase, Phase::Running(ExportStage::Checking))
            || self.publication_possible
            || files.len() != self.request.format.destinations().count()
            || files.len() != self.temporary.len()
            || !self.temporary.iter().all(|file| files.contains(file))
        {
            return Err(
                "The worker's output preparation did not match its temporary files.".to_owned(),
            );
        }
        Ok(())
    }

    pub(crate) fn set_stage(&mut self, stage: ExportStage) -> bool {
        use ExportStage::{Checking, Decoding, Publishing, Verifying};
        let allowed = match self.phase {
            Phase::Running(current) => {
                current == stage
                    || self.publication_possible
                        && matches!(
                            (current, stage),
                            (Checking, Decoding) | (Decoding, Verifying)
                        )
            }
            Phase::Cancelling => return stage != Publishing,
            _ => false,
        };
        if allowed {
            self.phase = Phase::Running(stage);
        }
        allowed
    }

    pub(crate) fn authorize_publication(&mut self, frames: u64) -> Result<bool, &'static str> {
        match self.phase {
            Phase::Cancelling => Ok(false),
            Phase::Running(ExportStage::Verifying) if self.frames_match(frames) => {
                // Remove kill eligibility before the worker can claim publication.
                self.verified_frames = Some(frames);
                self.phase = Phase::Running(ExportStage::Publishing);
                Ok(true)
            }
            _ => Err("The worker did not verify the requested output selection."),
        }
    }

    pub(crate) fn published(&mut self, file: ExportArtifact) -> bool {
        if self.phase != Phase::Running(ExportStage::Publishing)
            || !self
                .temporary
                .iter()
                .any(|temp| temp.kind == file.kind && temp.destination == file.path)
            || self.published.iter().any(|known| known.kind == file.kind)
        {
            return false;
        }
        self.published.push(file);
        true
    }

    pub(crate) fn finish(&mut self, outcome: ExportOutcome) {
        let reported = match &outcome {
            ExportOutcome::Completed { files, .. } | ExportOutcome::Partial { files, .. } => {
                files.as_slice()
            }
            ExportOutcome::Failed { .. } | ExportOutcome::Cancelled => &[],
        };
        let same_files = reported.len() == self.published.len()
            && self.published.iter().all(|file| reported.contains(file));
        let complete = match &outcome {
            ExportOutcome::Completed { frames, .. } => {
                self.verified_frames == Some(*frames)
                    && reported.len() == self.request.format.destinations().count()
                    && reported
                        .iter()
                        .all(|file| file.durability == ExportDurability::Confirmed)
            }
            ExportOutcome::Partial { frames, .. } => {
                !reported.is_empty() && self.verified_frames == Some(*frames)
            }
            ExportOutcome::Failed { .. } | ExportOutcome::Cancelled => true,
        };
        if same_files && complete {
            self.phase = Phase::Finished(outcome);
        } else {
            self.failure(
                "The worker's final report did not match the published outputs.".to_owned(),
            );
        }
    }

    fn frames_match(&self, frames: u64) -> bool {
        if frames == 0 {
            return false;
        }
        let ExportRange::Inclusive { first, last } = self.request.range else {
            return true;
        };
        let count = last
            .checked_sub(first)
            .and_then(|difference| difference.checked_add(1));
        if self.request.layer.is_some() {
            count.is_none_or(|count| frames <= count)
        } else {
            count == Some(frames)
        }
    }

    pub(crate) fn reveal_path(&self) -> Option<PathBuf> {
        self.published.first()?.path.to_path_buf().ok()
    }

    pub(crate) fn cleanup_partials(&mut self) {
        let format = &self.request.format;
        let source = &self.request.source.path;
        if self.publication_possible && matches!(self.phase, Phase::Interrupted(_)) {
            for file in &self.temporary {
                if self
                    .published
                    .iter()
                    .any(|artifact| artifact.kind == file.kind)
                    || !format
                        .destinations()
                        .any(|(kind, path)| file.kind == kind && &file.destination == path)
                {
                    continue;
                }
                if let Ok(path) = file.destination.to_path_buf()
                    && let Some(metadata) = identified_output(file, &path)
                {
                    self.published.push(ExportArtifact {
                        kind: file.kind,
                        path: file.destination.clone(),
                        bytes: metadata.len(),
                        durability: ExportDurability::Unconfirmed,
                    });
                }
            }
        }
        self.temporary.retain(|file| {
            if &file.partial == source
                || !format.destinations().any(|(kind, destination)| {
                    file.kind == kind && &file.destination == destination
                })
            {
                return false;
            }
            let (Ok(partial), Ok(destination)) =
                (file.partial.to_path_buf(), file.destination.to_path_buf())
            else {
                return false;
            };
            match remove_export_partial(file, &partial, &destination) {
                PartialFate::Kept { path, reason } => {
                    let detail = format!(
                        "Studio kept the temporary file at {}. {reason}",
                        path.display()
                    );
                    if !self.cleanup.contains(&detail) {
                        self.cleanup.push(detail);
                    }
                    true
                }
                _ => false,
            }
        });
    }
}

fn remove_export_partial(file: &ExportTemp, partial: &Path, destination: &Path) -> PartialFate {
    let planned = recovery::plan_partial(partial, destination, &recovery::RealDisk);
    if !matches!(planned, PartialFate::Pending(_)) {
        return planned;
    }
    if identified_output(file, partial).is_none() {
        return PartialFate::Kept {
            path: partial.to_path_buf(),
            reason: "Studio could not confirm the identity of the file or its folder.".to_owned(),
        };
    }
    recovery::remove_partial(partial, destination)
}

fn identified_output(file: &ExportTemp, path: &Path) -> Option<fs::Metadata> {
    let identity = file.identity.as_ref()?;
    let parent_identity = file.parent_identity.as_ref()?;
    identity.created?;
    parent_identity.created?;
    #[cfg(windows)]
    if ![identity, parent_identity]
        .iter()
        .all(|id| studio_domain::export::ExportTempIdentity::is_known_windows_index(id.inode))
    {
        return None;
    }
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let parent_path = path.parent()?;
    let parent = fs::metadata(parent_path).ok()?;
    if temp_identity(path, &metadata).as_ref() == Some(identity)
        && temp_identity(parent_path, &parent).as_ref() == Some(parent_identity)
    {
        Some(metadata)
    } else {
        None
    }
}

#[cfg(unix)]
fn temp_identity(
    _path: &Path,
    metadata: &fs::Metadata,
) -> Option<studio_domain::export::ExportTempIdentity> {
    use std::os::unix::fs::MetadataExt;

    Some(studio_domain::export::ExportTempIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        created: Some(metadata.created().ok()?),
    })
}

#[cfg(windows)]
fn temp_identity(
    path: &Path,
    _metadata: &fs::Metadata,
) -> Option<studio_domain::export::ExportTempIdentity> {
    let handle = winapi_util::Handle::from_path_any(path).ok()?;
    let info = winapi_util::file::information(&handle).ok()?;
    let inode = info.file_index();
    if !studio_domain::export::ExportTempIdentity::is_known_windows_index(inode) {
        return None;
    }
    info.creation_time()?;
    let metadata = handle.as_file().metadata().ok()?;
    Some(studio_domain::export::ExportTempIdentity {
        device: info.volume_serial_number(),
        inode,
        created: Some(metadata.created().ok()?),
    })
}

#[cfg(not(any(unix, windows)))]
fn temp_identity(
    _path: &Path,
    _metadata: &fs::Metadata,
) -> Option<studio_domain::export::ExportTempIdentity> {
    None
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
/// The export jobs stored in the queue journal.
pub struct ExportJobs {
    rows: Vec<ExportJob>,
    next_id: u64,
}

impl ExportJobs {
    pub(crate) fn is_new(&self) -> bool {
        self.next_id == 0 && self.rows.is_empty()
    }

    pub(crate) fn rows(&self) -> &[ExportJob] {
        &self.rows
    }

    pub(crate) fn active(&self) -> bool {
        self.rows.iter().any(|job| job.phase.active())
    }

    pub(crate) fn next_queued(&self) -> Option<u64> {
        self.rows
            .iter()
            .find(|job| job.phase == Phase::Queued)
            .map(|job| job.id)
    }

    pub(crate) fn job(&mut self, id: u64) -> Option<&mut ExportJob> {
        self.rows.iter_mut().find(|job| job.id == id)
    }

    pub(crate) fn remove(&mut self, id: u64) {
        self.rows
            .retain(|job| job.id != id || job.phase.active() || !job.temporary.is_empty());
    }

    pub(crate) fn cancel_queued(&mut self) {
        for job in &mut self.rows {
            if job.phase == Phase::Queued {
                job.phase = Phase::Finished(ExportOutcome::Cancelled);
            }
        }
    }

    pub(crate) fn add(&mut self, requests: Vec<ExportRequest>) -> Result<(), String> {
        if requests.is_empty() {
            return Err("Select one or more AV2 files to export.".to_owned());
        }
        if self.active() {
            return Err("Wait for the current export batch or stop it.".to_owned());
        }

        let mut paths = Vec::new();
        let count = u64::try_from(requests.len()).map_err(|error| error.to_string())?;
        self.next_id
            .checked_add(count)
            .ok_or_else(|| "The export queue has no free job IDs.".to_owned())?;
        for request in &requests {
            request.validate().map_err(|error| error.to_string())?;
            for (_, output) in request.format.destinations() {
                let path = output.to_path_buf().map_err(|error| error.to_string())?;
                let key = destination_key(&path)?;
                if paths.contains(&key) {
                    return Err(format!(
                        "Several export outputs use {}. Choose distinct paths.",
                        path.display()
                    ));
                }
                paths.push(key);
            }
        }
        for request in requests {
            self.next_id += 1;
            self.rows.push(ExportJob::queued(self.next_id, request));
        }
        Ok(())
    }

    /// Never resume external writes after shutdown. The user can submit the
    /// source again after checking its interrupted output paths.
    pub(crate) fn interrupt(&mut self, why: &str) {
        for job in &mut self.rows {
            if job.phase == Phase::Queued {
                job.phase =
                    Phase::Interrupted("Studio stopped before this export started.".to_owned());
            } else if job.phase.active() {
                job.phase = Phase::Interrupted(why.to_owned());
            }
        }
    }

    pub(crate) fn recover(&mut self) {
        self.interrupt("Studio closed before this export finished. Its outputs may be incomplete.");
        self.next_id = self
            .rows
            .iter()
            .map(|row| row.id)
            .fold(self.next_id, u64::max);
    }

    pub(crate) fn clean_up(&mut self) {
        for job in &mut self.rows {
            job.cleanup_partials();
        }
    }
}

fn destination_key(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("Select an absolute path for each export output.".to_owned());
    }
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err("Select an output file name.".to_owned());
    };
    let directory = parent.canonicalize().map_err(|error| {
        format!(
            "Cannot open the output folder {}. {error}",
            parent.display()
        )
    })?;
    Ok(directory.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use studio_domain::analysis::{Framing, SourceDigest};
    use studio_domain::export::{ExportArtifactKind, ExportFormat, ExportRange};
    use studio_domain::job::{ChromaLayout, SourceFingerprint};

    use crate::jobs::{State, export};
    use crate::testing::Scratch;

    fn request(format: ExportFormat, scratch: &Scratch) -> ExportRequest {
        ExportRequest {
            source: Source {
                path: WirePath::from_path(&scratch.path("source.ivf")),
                fingerprint: SourceFingerprint {
                    len: 32,
                    modified: None,
                },
                digest: SourceDigest {
                    sha256: "a".repeat(64),
                    bytes: 32,
                },
                framing: Framing::Ivf,
            },
            range: ExportRange::All,
            layer: None,
            grain: GrainPolicy::Skip,
            frame_rate: None,
            expected: ExportSampleFormat {
                bit_depth: 8,
                chroma: ChromaLayout::Yuv420,
            },
            format,
        }
    }

    #[test]
    fn batch_rejects_collisions_before_any_job_is_queued() -> Result<(), String> {
        let scratch = Scratch::new("export-collision");
        let path = WirePath::from_path(&scratch.path("out.y4m"));
        let request = request(ExportFormat::Y4m { path }, &scratch);
        let mut jobs = ExportJobs::default();
        assert!(jobs.add(vec![request.clone(), request.clone()]).is_err());
        assert_eq!(jobs.rows(), []);
        jobs.add(vec![request])?;
        assert_eq!(jobs.next_queued(), Some(1));
        jobs.cancel_queued();
        assert_eq!(export(&jobs.rows()[0]).state, State::Cancelled);
        assert_eq!(jobs.next_queued(), None);
        Ok(())
    }

    #[test]
    fn invalid_terminal_counts_do_not_claim_completion() {
        let scratch = Scratch::new("export-outcome");
        let path = WirePath::from_path(&scratch.path("out.y4m"));
        let mut request = request(ExportFormat::Y4m { path: path.clone() }, &scratch);
        request.range = ExportRange::Inclusive { first: 4, last: 4 };
        let mut job = ExportJob::queued(1, request);
        job.phase = Phase::Running(ExportStage::Checking);
        assert!(!job.set_stage(ExportStage::Decoding));
        job.publication_possible = true;
        assert!(job.set_stage(ExportStage::Decoding));
        assert!(!job.set_stage(ExportStage::Publishing));
        assert!(job.set_stage(ExportStage::Verifying));
        assert!(!job.set_stage(ExportStage::Publishing));
        assert!(job.authorize_publication(2).is_err());
        assert_eq!(job.authorize_publication(1), Ok(true));
        assert!(job.set_stage(ExportStage::Publishing));
        let failure = Failure {
            code: FailureCode::Output,
            message: "test error".to_owned(),
        };
        job.finish(ExportOutcome::Partial {
            frames: 1,
            files: Vec::new(),
            failure,
        });
        assert_eq!(export(&job).state, State::Failed);

        let file = ExportArtifact {
            kind: ExportArtifactKind::Video,
            path,
            bytes: 100,
            durability: ExportDurability::Confirmed,
        };
        let mut request = job.request.clone();
        request.range = ExportRange::All;
        let mut job = ExportJob::queued(2, request);
        job.phase = Phase::Running(ExportStage::Verifying);
        assert_eq!(job.authorize_publication(1), Ok(true));
        job.published.push(file.clone());
        job.finish(ExportOutcome::Completed {
            frames: 2,
            files: vec![file],
        });
        assert_eq!(export(&job).state, State::Partial);
    }

    #[test]
    fn publication_permission_respects_an_outstanding_cancel() {
        let scratch = Scratch::new("export-publish-cancel");
        let path = WirePath::from_path(&scratch.path("out.y4m"));
        let request = request(ExportFormat::Y4m { path }, &scratch);
        let mut job = ExportJob::queued(1, request);
        job.publication_possible = true;
        job.phase = Phase::Cancelling;

        assert_eq!(job.authorize_publication(1), Ok(false));
        assert_eq!(job.phase, Phase::Cancelling);
        assert!(!job.set_stage(ExportStage::Publishing));

        job.phase = Phase::Running(ExportStage::Verifying);
        assert_eq!(job.authorize_publication(1), Ok(true));
        assert_eq!(job.phase, Phase::Running(ExportStage::Publishing));
        assert!(!export(&job).can_cancel());
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_path_check_accepts_native_file_names() -> Result<(), String> {
        use std::os::unix::ffi::OsStrExt;

        let path = |name: &[u8]| Path::new("/tmp").join(std::ffi::OsStr::from_bytes(name));
        recovery::check_partial_path(
            &path(b".out-\xff.json.abc123.partial"),
            &path(b"out-\xff.json"),
        )
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn interrupted_export_preserves_replacement_and_published_output()
    -> Result<(), Box<dyn std::error::Error>> {
        let scratch = Scratch::new("export-restore");
        let layout = scratch.path("out.json");
        let samples = scratch.path("out.raw");
        let layout_partial = scratch.path(".out.json.abc123.partial");
        let samples_partial = scratch.path(".out.raw.def456.partial");
        let request = request(
            ExportFormat::Raw {
                samples: WirePath::from_path(&samples),
                layout: WirePath::from_path(&layout),
            },
            &scratch,
        );
        let mut jobs = ExportJobs::default();
        jobs.add(vec![request])?;
        let job = jobs.job(1).ok_or("export was not queued")?;
        job.phase = Phase::Running(ExportStage::Checking);
        let temp = |kind, partial: &Path, destination: &Path| ExportTemp {
            kind,
            partial: WirePath::from_path(partial),
            destination: WirePath::from_path(destination),
            identity: fs::symlink_metadata(partial)
                .ok()
                .and_then(|metadata| temp_identity(partial, &metadata)),
            parent_identity: fs::metadata(&scratch.0)
                .ok()
                .and_then(|metadata| temp_identity(&scratch.0, &metadata)),
        };
        fs::write(&layout_partial, b"layout temp")?;
        fs::write(&samples_partial, b"raw temp")?;
        job.register(temp(ExportArtifactKind::Layout, &layout_partial, &layout))?;
        job.register(temp(
            ExportArtifactKind::Samples,
            &samples_partial,
            &samples,
        ))?;
        job.prepared(&job.temporary)?;
        let layout_has_identity = identified_output(&job.temporary[0], &layout_partial).is_some();
        #[cfg(windows)]
        assert!(layout_has_identity);
        job.publication_possible = true;
        fs::hard_link(&layout_partial, &layout)?;
        let moved = scratch.path("moved");
        fs::rename(&samples_partial, &moved)?;
        fs::write(&samples_partial, b"a different file")?;

        let bytes = serde_json::to_vec(&jobs)?;
        let mut restored: ExportJobs = serde_json::from_slice(&bytes)?;
        restored.recover();
        restored.clean_up();
        let job = &restored.rows()[0];
        if layout_has_identity {
            assert_eq!(export(job).state, State::Partial);
            assert_eq!(job.published[0].durability, ExportDurability::Unconfirmed);
        } else {
            assert_eq!(export(job).state, State::Interrupted);
            assert_eq!(job.published, []);
        }
        assert_eq!(layout_partial.exists(), !layout_has_identity);
        assert_eq!(fs::read(&samples_partial)?, b"a different file");
        assert_eq!(fs::read(&moved)?, b"raw temp");
        assert_eq!(fs::read(&layout)?, b"layout temp");
        assert!(job.cleanup.iter().any(
            |line| line.contains("out.raw") && line.contains("could not confirm the identity")
        ));
        Ok(())
    }
}
