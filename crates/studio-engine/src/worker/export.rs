// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Worker ownership of prepared export paths and publication results.

use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{MutexGuard, PoisonError};
use std::thread::{self, Scope, ScopedJoinHandle};
use std::time::SystemTime;

use studio_domain::export::{
    ExportArtifact, ExportArtifactKind, ExportDurability, ExportOutcome, ExportProgress,
    ExportRequest, ExportStage, ExportTemp,
};
use studio_domain::job::{Failure, FailureCode, WirePath};
use studio_domain::protocol::{Envelope, ErrorCode, Event};
use tempfile::NamedTempFile;

use super::{JobState, Outbox, State, refusal};
use crate::job::{JOB_STACK_BYTES, Stop};
use crate::output::{self, ExportControl, FileId, Publication};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Preparing,
    Waiting,
    Ready,
    AwaitingPublish(u64),
    Publishing(u64),
}

#[derive(Debug)]
struct Destination {
    kind: ExportArtifactKind,
    wire: WirePath,
    path: PathBuf,
    temp: Option<Prepared>,
}

#[derive(Debug)]
struct Prepared {
    partial: PathBuf,
    file: FileId,
    // Keep the inode allocated until the worker completes or abandons this export.
    open: fs::File,
    parent: FileId,
    parent_created: Option<SystemTime>,
}

impl Prepared {
    fn created(&self) -> Option<SystemTime> {
        self.open.metadata().and_then(|meta| meta.created()).ok()
    }

    fn matches_path(&self) -> bool {
        self.partial.parent().is_some_and(|dir| {
            FileId::of_path(dir).is_ok_and(|id| id == self.parent)
                && fs::metadata(dir).and_then(|meta| meta.created()).ok() == self.parent_created
        }) && fs::symlink_metadata(&self.partial)
            .is_ok_and(|meta| meta.is_file() && meta.created().ok() == self.created())
            && FileId::of_path(&self.partial).is_ok_and(|id| id == self.file)
    }

    fn remove(&self) {
        if self.file.export_identity(self.created()).is_some() && self.matches_path() {
            let _ = fs::remove_file(&self.partial);
        }
    }
}

impl Destination {
    fn registration(&self) -> Option<ExportTemp> {
        let temp = self.temp.as_ref()?;
        Some(ExportTemp {
            kind: self.kind,
            partial: WirePath::from_path(&temp.partial),
            destination: self.wire.clone(),
            identity: temp.file.export_identity(temp.created()),
            parent_identity: temp.parent.export_identity(temp.parent_created),
        })
    }
}

#[derive(Debug)]
pub(super) struct ExportRun {
    phase: Phase,
    destinations: Vec<Destination>,
    published: Vec<ExportArtifact>,
}

impl ExportRun {
    fn new(request: &ExportRequest) -> Result<Self, Failure> {
        request
            .validate()
            .map_err(|error| invalid_failure(error.to_string()))?;
        let mut destinations: Vec<Destination> = Vec::new();
        for (kind, wire) in request.format.destinations() {
            let path = wire
                .to_path_buf()
                .map_err(|error| invalid_failure(error.to_string()))?;
            if !path.is_absolute()
                || path.file_name().is_none()
                || path.components().any(|part| part == Component::ParentDir)
            {
                return Err(invalid_failure(
                    "Select an absolute destination path without parent components.",
                ));
            }
            #[cfg(windows)]
            if path.file_name().is_some_and(|name| {
                let name = name.as_encoded_bytes();
                name.ends_with(b" ") || name.ends_with(b".") || name.contains(&b':')
            }) {
                return Err(invalid_failure(
                    "Choose output names without trailing spaces, periods, or a colon.",
                ));
            }
            if destinations
                .iter()
                .any(|destination| destination.path == path)
            {
                return Err(invalid_failure(
                    "Select different paths for the samples and layout.",
                ));
            }
            destinations.push(Destination {
                kind,
                wire: wire.clone(),
                path,
                temp: None,
            });
        }
        Ok(Self {
            phase: Phase::Preparing,
            destinations,
            published: Vec::new(),
        })
    }

    fn prepare(
        &mut self,
        kind: ExportArtifactKind,
        destination: &Path,
        source: &FileId,
    ) -> Result<(NamedTempFile, ExportTemp), Stop> {
        if self.phase != Phase::Preparing {
            return Err(invalid(
                "The worker already announced the export preparation.",
            ));
        }
        if let [first, second] = self.destinations.as_slice()
            && output::same_destination_slot(&first.path, &second.path)
        {
            return Err(invalid(
                "Select different paths for the samples and layout.",
            ));
        }
        let parent = destination
            .parent()
            .ok_or_else(|| invalid("The destination has no parent folder."))?;
        let parent_id = FileId::of_path(parent)
            .map_err(|e| output::output(format!("Cannot identify the destination folder. {e}")))?;
        let Some(plan) = self
            .destinations
            .iter_mut()
            .find(|plan| plan.kind == kind && plan.path == destination && plan.temp.is_none())
        else {
            return Err(invalid(
                "The temporary file does not match the requested export.",
            ));
        };
        let mut temp = output::prepare(destination, source)?;
        let file = FileId::of_file(temp.as_file(), temp.path())
            .map_err(|e| output::output(format!("Cannot identify the temporary file. {e}")))?;
        let open = temp.as_file().try_clone().map_err(|e| {
            output::output(format!("Cannot retain the temporary file identity. {e}"))
        })?;
        let created = open.metadata().and_then(|meta| meta.created()).ok();
        if file.export_identity(created).is_some() {
            temp.disable_cleanup(true);
        }
        plan.temp = Some(Prepared {
            partial: temp.path().to_path_buf(),
            file,
            open,
            parent: parent_id,
            parent_created: fs::metadata(parent).and_then(|meta| meta.created()).ok(),
        });
        let registration = plan
            .registration()
            .ok_or_else(|| output::output("The worker did not register the temporary file."))?;
        Ok((temp, registration))
    }

    fn publication_path(
        &self,
        kind: ExportArtifactKind,
        destination: &Path,
        temp: &NamedTempFile,
    ) -> Result<WirePath, Stop> {
        if !matches!(self.phase, Phase::Publishing(_))
            || self.published.iter().any(|file| file.kind == kind)
        {
            return Err(invalid("The worker cannot publish this export file."));
        }
        let plan = self
            .destinations
            .iter()
            .find(|plan| plan.kind == kind && plan.path == destination)
            .ok_or_else(|| invalid("The output path does not match the requested export."))?;
        let original = plan
            .temp
            .as_ref()
            .filter(|original| original.partial == temp.path())
            .ok_or_else(|| invalid("The file does not match the prepared export."))?;
        if !original.matches_path()
            || !FileId::of_file(temp.as_file(), temp.path()).is_ok_and(|id| id == original.file)
        {
            return Err(output::output(
                "The output folder or temporary file changed.",
            ));
        }
        Ok(plan.wire.clone())
    }

    fn complete(self, result: Result<u64, Stop>) -> ExportOutcome {
        for plan in &self.destinations {
            if let Some(temp) = &plan.temp {
                temp.remove();
            }
        }
        let frames = match self.phase {
            Phase::Publishing(frames) => frames,
            _ => 0,
        };
        let failure = match result {
            Ok(reported)
                if frames != 0
                    && reported == frames
                    && self.published.len() == self.destinations.len()
                    && self
                        .published
                        .iter()
                        .all(|f| f.durability == ExportDurability::Confirmed) =>
            {
                return ExportOutcome::Completed {
                    frames,
                    files: self.published,
                };
            }
            Ok(_) => Failure {
                code: FailureCode::Output,
                message: "The worker did not publish every export file.".into(),
            },
            Err(Stop::Cancelled) if self.published.is_empty() => return ExportOutcome::Cancelled,
            Err(Stop::Cancelled) => Failure {
                code: FailureCode::Output,
                message: "The export stopped after it published a file.".into(),
            },
            Err(Stop::Failed(failure)) => failure,
        };
        if self.published.is_empty() {
            ExportOutcome::Failed { failure }
        } else {
            ExportOutcome::Partial {
                frames,
                files: self.published,
                failure,
            }
        }
    }

    pub(super) fn cancel(self) -> Event {
        Event::ExportFinished {
            outcome: self.complete(Err(Stop::Cancelled)),
        }
    }
}

fn invalid_failure(message: impl Into<String>) -> Failure {
    Failure {
        code: FailureCode::InvalidSettings,
        message: message.into(),
    }
}

fn invalid(message: impl Into<String>) -> Stop {
    Stop::Failed(invalid_failure(message))
}

pub(super) fn start<'scope>(
    scope: &'scope Scope<'scope, '_>,
    outbox: &'scope Outbox,
    id: u64,
    request: ExportRequest,
    job: &mut Option<ScopedJoinHandle<'scope, ()>>,
) {
    if job.is_some() {
        outbox.reply(
            id,
            refusal(ErrorCode::WrongState, "This worker already started a job."),
        );
        return;
    }
    let export = match ExportRun::new(&request) {
        Ok(export) => export,
        Err(failure) => {
            outbox.reply(
                id,
                Event::ExportFinished {
                    outcome: ExportOutcome::Failed { failure },
                },
            );
            return;
        }
    };
    {
        let mut state = outbox.lock();
        state.job = JobState::Running;
        state.job_id = id;
        state.export = Some(export);
    }
    let control = Worker { outbox, id };
    match thread::Builder::new()
        .name("studio-export".into())
        .stack_size(JOB_STACK_BYTES)
        .spawn_scoped(scope, move || {
            let result = catch_unwind(AssertUnwindSafe(|| crate::export::run(&request, &control)))
                .unwrap_or_else(|_| Err(output::output("The export stopped unexpectedly.")));
            control.finish(result);
        }) {
        Ok(thread) => *job = Some(thread),
        Err(error) => Worker { outbox, id }.finish(Err(output::output(format!(
            "Cannot start the export. {error}"
        )))),
    }
}

pub(super) fn proceed(outbox: &Outbox, id: u64) {
    let mut state = outbox.lock();
    let worker = Worker { outbox, id };
    if let Ok(export) = worker.active(&mut state)
        && export.phase == Phase::Waiting
    {
        export.phase = Phase::Ready;
        drop(state);
        outbox.changed.notify_all();
        return;
    }
    drop(state);
    outbox.reply(
        id,
        refusal(ErrorCode::WrongState, "Wait for the prepared export paths."),
    );
}

pub(super) fn permit(outbox: &Outbox, id: u64) {
    let mut state = outbox.lock();
    let worker = Worker { outbox, id };
    if let Ok(export) = worker.active(&mut state)
        && let Phase::AwaitingPublish(frames) = export.phase
    {
        export.phase = Phase::Publishing(frames);
        state.publishing = true;
        state.push(Envelope::new(
            id,
            Event::ExportStage {
                stage: ExportStage::Publishing,
            },
        ));
        drop(state);
        outbox.changed.notify_all();
        return;
    }
    drop(state);
    outbox.reply(
        id,
        refusal(
            ErrorCode::WrongState,
            "Wait for the verified export before publishing.",
        ),
    );
}

struct Worker<'a> {
    outbox: &'a Outbox,
    id: u64,
}

impl Worker<'_> {
    fn active<'a>(&self, state: &'a mut State) -> Result<&'a mut ExportRun, Stop> {
        if state.job != JobState::Running || state.job_id != self.id || self.cancelled() {
            return Err(Stop::Cancelled);
        }
        state.export.as_mut().ok_or(Stop::Cancelled)
    }

    fn wait(&self, mut state: MutexGuard<'_, State>, pending: Phase) -> Result<(), Stop> {
        while self.active(&mut state)?.phase == pending {
            state = self
                .outbox
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        Ok(())
    }

    fn finish(&self, result: Result<u64, Stop>) {
        let mut state = self.outbox.lock();
        if let Some(export) = state.export.take() {
            let outcome = export.complete(result);
            state.end_job(self.id, Event::ExportFinished { outcome });
        }
        drop(state);
        self.outbox.changed.notify_all();
    }

    fn record(
        &self,
        kind: ExportArtifactKind,
        path: WirePath,
        publication: Publication,
    ) -> Result<(), Stop> {
        let file = ExportArtifact {
            kind,
            path,
            bytes: publication.bytes,
            durability: if publication.sync.is_ok() {
                ExportDurability::Confirmed
            } else {
                ExportDurability::Unconfirmed
            },
        };
        let mut state = self.outbox.lock();
        self.active(&mut state)?.published.push(file.clone());
        state.push(Envelope::new(self.id, Event::ExportPublished { file }));
        drop(state);
        self.outbox.changed.notify_all();
        publication.sync
    }
}

impl ExportControl for Worker<'_> {
    fn cancelled(&self) -> bool {
        self.outbox.cancel.load(Ordering::Relaxed)
    }

    fn create_temp(
        &self,
        kind: ExportArtifactKind,
        destination: &Path,
        source: &FileId,
    ) -> Result<NamedTempFile, Stop> {
        let mut state = self.outbox.lock();
        let (temp, file) = self
            .active(&mut state)?
            .prepare(kind, destination, source)?;
        state.push(Envelope::new(self.id, Event::ExportTempCreated { file }));
        drop(state);
        self.outbox.changed.notify_all();
        Ok(temp)
    }

    fn await_prepared(&self) -> Result<(), Stop> {
        let mut state = self.outbox.lock();
        let export = self.active(&mut state)?;
        if export.phase != Phase::Preparing {
            return Err(invalid(
                "The worker already announced the export preparation.",
            ));
        }
        let files = export
            .destinations
            .iter()
            .map(Destination::registration)
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| invalid("The worker must prepare all export files first."))?;
        export.phase = Phase::Waiting;
        state.push(Envelope::new(self.id, Event::ExportPrepared { files }));
        self.outbox.changed.notify_all();
        self.wait(state, Phase::Waiting)
    }

    fn stage(&self, stage: ExportStage) {
        self.outbox.job_event(self.id, Event::ExportStage { stage });
    }

    fn progress(&self, progress: ExportProgress) {
        self.outbox
            .progress(self.id, Event::ExportProgress { progress });
    }

    fn claim_publish(&self, frames: u64) -> Result<(), Stop> {
        let mut state = self.outbox.lock();
        let export = self.active(&mut state)?;
        if export.phase != Phase::Ready || frames == 0 {
            return Err(invalid(
                "Verify at least one selected frame before publishing.",
            ));
        }
        let pending = Phase::AwaitingPublish(frames);
        export.phase = pending;
        state.flush_progress();
        state.push(Envelope::new(self.id, Event::ExportPublishReady { frames }));
        self.outbox.changed.notify_all();
        self.wait(state, pending)
    }

    fn publish(
        &self,
        kind: ExportArtifactKind,
        mut temp: NamedTempFile,
        destination: &Path,
    ) -> Result<(), Stop> {
        let path =
            match self
                .active(&mut self.outbox.lock())?
                .publication_path(kind, destination, &temp)
            {
                Ok(path) => path,
                Err(error) => {
                    temp.disable_cleanup(true);
                    return Err(error);
                }
            };
        self.record(kind, path, output::publish_recorded(temp, destination)?)
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used)]
mod tests {
    use std::io::Write;
    use std::time::Duration;

    use studio_domain::analysis::{Framing, SourceDigest};
    use studio_domain::export::{ExportFormat, ExportRange, ExportSampleFormat, ExportSource};
    use studio_domain::job::{ChromaLayout, SourceFingerprint};
    use studio_domain::preview::GrainPolicy;

    use super::*;
    use crate::worker::Cancel;

    const ID: u64 = 4;

    struct Fixture {
        folder: tempfile::TempDir,
        source: FileId,
    }

    impl Fixture {
        fn new() -> Self {
            let folder = tempfile::tempdir().unwrap();
            let path = folder.path().join("source.obu");
            fs::write(&path, b"source").unwrap();
            let source = FileId::of_path(&path).unwrap();
            Self { folder, source }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.folder.path().join(name)
        }

        fn request(&self) -> ExportRequest {
            let path = self.path("source.obu");
            let fingerprint = SourceFingerprint::of(&fs::metadata(&path).unwrap());
            ExportRequest {
                source: ExportSource {
                    path: WirePath::from_path(&path),
                    fingerprint,
                    digest: SourceDigest {
                        bytes: fingerprint.len,
                        sha256: "a".repeat(64),
                    },
                    framing: Framing::AnnexB,
                },
                range: ExportRange::All,
                layer: None,
                grain: GrainPolicy::Skip,
                frame_rate: None,
                expected: ExportSampleFormat {
                    bit_depth: 10,
                    chroma: ChromaLayout::Yuv422,
                },
                format: ExportFormat::Raw {
                    samples: WirePath::from_path(&self.path("samples.yuv")),
                    layout: WirePath::from_path(&self.path("layout.json")),
                },
            }
        }

        fn control<'a>(&self, outbox: &'a Outbox) -> Worker<'a> {
            let request = self.request();
            let mut state = outbox.lock();
            state.job = JobState::Running;
            state.job_id = ID;
            state.export = Some(ExportRun::new(&request).unwrap());
            Worker { outbox, id: ID }
        }

        fn stage(
            &self,
            control: &Worker<'_>,
            kind: ExportArtifactKind,
            name: &str,
        ) -> NamedTempFile {
            control
                .create_temp(kind, &self.path(name), &self.source)
                .unwrap()
        }
    }

    fn fill(temp: &mut NamedTempFile) {
        temp.write_all(b"verified contents").unwrap();
        temp.as_file().sync_all().unwrap();
    }

    fn proceed_after_registration(control: &Worker<'_>) {
        thread::scope(|scope| {
            let waiting = scope.spawn(|| control.await_prepared());
            let mut state = control.outbox.lock();
            while !matches!(state.queue.back().map(|envelope| &envelope.body),
                Some(Event::ExportPrepared { files }) if files.len() == 2)
            {
                state = control.outbox.changed.wait(state).unwrap();
            }
            drop(state);
            proceed(control.outbox, ID + 1);
            assert_eq!(
                control.outbox.lock().export.as_ref().unwrap().phase,
                Phase::Waiting
            );
            proceed(control.outbox, ID);
            assert!(waiting.join().unwrap().is_ok());
        });
    }

    fn after_verified(
        control: &Worker<'_>,
        frames: u64,
        action: impl FnOnce(),
    ) -> Result<(), Stop> {
        thread::scope(|scope| {
            let waiting = scope.spawn(|| control.claim_publish(frames));
            let mut state = control.outbox.lock();
            while !matches!(state.queue.back().map(|envelope| &envelope.body),
                Some(Event::ExportPublishReady { frames: count }) if *count == frames)
            {
                state = control.outbox.changed.wait(state).unwrap();
            }
            assert!(!state.publishing);
            assert!(
                state
                    .export
                    .as_ref()
                    .unwrap()
                    .destinations
                    .iter()
                    .all(|plan| !plan.path.exists())
            );
            drop(state);
            action();
            waiting.join().unwrap()
        })
    }

    fn permit_after_verified(control: &Worker<'_>, frames: u64) {
        after_verified(control, frames, || {
            permit(control.outbox, ID + 1);
            assert!(!control.outbox.lock().publishing);
            permit(control.outbox, ID);
        })
        .unwrap();
    }

    #[test]
    fn cancellation_at_publish_barrier_never_renames() {
        let fixture = Fixture::new();
        let outbox = Outbox::default();
        let control = fixture.control(&outbox);
        let mut layout = fixture.stage(&control, ExportArtifactKind::Layout, "layout.json");
        let mut samples = fixture.stage(&control, ExportArtifactKind::Samples, "samples.yuv");
        proceed_after_registration(&control);
        fill(&mut layout);
        fill(&mut samples);
        let result = after_verified(&control, 6, || {
            assert!(matches!(outbox.cancel(ID), Cancel::Accepted));
            permit(&outbox, ID);
            assert!(!outbox.lock().publishing);
        });
        assert!(matches!(result, Err(Stop::Cancelled)));
        drop(layout);
        drop(samples);
        control.finish(result);
        assert!(!fixture.path("samples.yuv").exists());
        assert!(!fixture.path("layout.json").exists());
        assert!(matches!(
            outbox.lock().queue.back().map(|envelope| &envelope.body),
            Some(Event::ExportFinished {
                outcome: ExportOutcome::Cancelled
            })
        ));
    }

    #[test]
    fn second_output_collision_reports_the_published_first_file() {
        let fixture = Fixture::new();
        let outbox = Outbox::default();
        let control = fixture.control(&outbox);
        let mut layout = fixture.stage(&control, ExportArtifactKind::Layout, "layout.json");
        let mut samples = fixture.stage(&control, ExportArtifactKind::Samples, "samples.yuv");
        let partial = samples.path().to_path_buf();
        proceed_after_registration(&control);
        fill(&mut layout);
        fill(&mut samples);
        permit_after_verified(&control, 6);
        assert!(matches!(outbox.cancel(ID), Cancel::Publishing));
        outbox.stop(&outbox.lock());
        assert!(!control.cancelled());
        control
            .publish(
                ExportArtifactKind::Layout,
                layout,
                &fixture.path("layout.json"),
            )
            .unwrap();
        fs::write(fixture.path("samples.yuv"), b"other user's output").unwrap();
        let result = control.publish(
            ExportArtifactKind::Samples,
            samples,
            &fixture.path("samples.yuv"),
        );
        control.finish(result.map(|()| 6));
        assert_eq!(
            fs::read(fixture.path("samples.yuv")).unwrap(),
            b"other user's output"
        );
        assert!(!partial.exists());
        let state = outbox.lock();
        assert!(
            matches!(state.queue.back().map(|envelope| &envelope.body), Some(Event::ExportFinished {
            outcome: ExportOutcome::Partial { frames: 6, files, failure: Failure { code: FailureCode::DestinationExists, .. } }
        }) if files.len() == 1 && files[0].kind == ExportArtifactKind::Layout && files[0].durability == ExportDurability::Confirmed)
        );
    }

    #[test]
    fn a_post_publish_sync_error_keeps_the_file_in_the_partial_result() {
        let fixture = Fixture::new();
        let outbox = Outbox::default();
        let control = fixture.control(&outbox);
        let mut layout = fixture.stage(&control, ExportArtifactKind::Layout, "layout.json");
        let mut samples = fixture.stage(&control, ExportArtifactKind::Samples, "samples.yuv");
        proceed_after_registration(&control);
        fill(&mut layout);
        fill(&mut samples);
        permit_after_verified(&control, 6);
        let path = fixture.path("layout.json");
        let published = output::publish_recorded(layout, &path).unwrap();
        let result = control.record(
            ExportArtifactKind::Layout,
            WirePath::from_path(&path),
            Publication {
                bytes: published.bytes,
                sync: Err(output::output("Injected directory sync failure.")),
            },
        );
        drop(samples);
        control.finish(result.map(|()| 6));
        assert!(path.is_file());
        assert!(!fixture.path("samples.yuv").exists());
        let state = outbox.lock();
        let mut events = state.queue.iter().rev();
        assert!(
            matches!(events.next().map(|envelope| &envelope.body), Some(Event::ExportFinished {
            outcome: ExportOutcome::Partial { frames: 6, files, .. }
        }) if files.len() == 1 && files[0].durability == ExportDurability::Unconfirmed)
        );
        assert!(
            matches!(events.next().map(|envelope| &envelope.body), Some(Event::ExportPublished { file })
            if file.path == WirePath::from_path(&path) && file.durability == ExportDurability::Unconfirmed)
        );
    }

    #[test]
    fn a_failed_second_preparation_reports_and_cleans_the_first_temp() {
        let fixture = Fixture::new();
        let outbox = Outbox::default();
        let control = fixture.control(&outbox);
        let layout = fixture.stage(&control, ExportArtifactKind::Layout, "layout.json");
        let partial = layout.path().to_path_buf();
        fs::write(fixture.path("samples.yuv"), b"already exists").unwrap();
        let result = control.create_temp(
            ExportArtifactKind::Samples,
            &fixture.path("samples.yuv"),
            &fixture.source,
        );
        drop(layout);
        control.finish(result.map(|_| 1));
        assert!(!partial.exists());
        let state = outbox.lock();
        assert!(matches!(state.queue.front().map(|envelope| &envelope.body),
            Some(Event::ExportTempCreated { file }) if file.partial == WirePath::from_path(&partial)));
        assert!(matches!(
            state.queue.back().map(|envelope| &envelope.body),
            Some(Event::ExportFinished {
                outcome: ExportOutcome::Failed {
                    failure: Failure {
                        code: FailureCode::DestinationExists,
                        ..
                    }
                }
            })
        ));
    }

    #[test]
    fn cancellation_before_prepared_drops_registered_output() {
        let fixture = Fixture::new();
        let outbox = Outbox::default();
        let control = fixture.control(&outbox);
        let layout = fixture.stage(&control, ExportArtifactKind::Layout, "layout.json");
        let partial = layout.path().to_path_buf();
        assert!(matches!(outbox.cancel(ID + 1), Cancel::WrongExport));
        assert!(!control.cancelled());
        assert!(matches!(outbox.cancel(ID), Cancel::Accepted));
        assert!(matches!(control.await_prepared(), Err(Stop::Cancelled)));
        assert!(outbox.settle(Duration::ZERO));
        drop(layout);
        assert!(!partial.exists());
        assert!(matches!(
            outbox.lock().queue.back().map(|envelope| &envelope.body),
            Some(Event::ExportFinished {
                outcome: ExportOutcome::Cancelled
            })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn raw_paths_with_aliased_parents_fail_before_creating_a_temp() {
        let fixture = Fixture::new();
        let alias = fixture.path("alias");
        std::os::unix::fs::symlink(fixture.folder.path(), &alias).unwrap();
        let real_output = fixture.path("same-output");
        let mut request = fixture.request();
        request.format = ExportFormat::Raw {
            samples: WirePath::from_path(&real_output),
            layout: WirePath::from_path(&alias.join("same-output")),
        };
        let mut export = ExportRun::new(&request).unwrap();
        let result = export.prepare(ExportArtifactKind::Samples, &real_output, &fixture.source);
        assert!(matches!(
            result,
            Err(Stop::Failed(Failure {
                code: FailureCode::InvalidSettings,
                ..
            }))
        ));
        assert!(export.destinations.iter().all(|plan| plan.temp.is_none()));
    }

    #[cfg(windows)]
    #[test]
    fn raw_paths_that_only_differ_in_case_fail_before_creating_a_temp() {
        let fixture = Fixture::new();
        let path = fixture.path("Output");
        let mut request = fixture.request();
        request.format = ExportFormat::Raw {
            samples: WirePath::from_path(&path),
            layout: WirePath::from_path(&fixture.path("output")),
        };
        let mut export = ExportRun::new(&request).unwrap();
        let result = export.prepare(ExportArtifactKind::Samples, &path, &fixture.source);
        assert!(matches!(
            result,
            Err(Stop::Failed(Failure {
                code: FailureCode::InvalidSettings,
                ..
            }))
        ));
        assert!(export.destinations.iter().all(|plan| plan.temp.is_none()));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn cancellation_preserves_a_replacement_at_a_registered_path() {
        let fixture = Fixture::new();
        let outbox = Outbox::default();
        let control = fixture.control(&outbox);
        let layout = fixture.stage(&control, ExportArtifactKind::Layout, "layout.json");
        let partial = layout.path().to_path_buf();
        fs::rename(&partial, fixture.path("moved")).unwrap();
        fs::write(&partial, b"replacement").unwrap();
        assert!(matches!(outbox.cancel(ID), Cancel::Accepted));
        assert!(outbox.settle(Duration::ZERO));
        drop(layout);
        assert_eq!(fs::read(&partial).unwrap(), b"replacement");
    }
}
