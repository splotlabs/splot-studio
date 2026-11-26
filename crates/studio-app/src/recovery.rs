// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Restoring the queue from its journal, and removing what a dead worker
//! left behind.
//!
//! [`recover`] is pure: it looks at the disk only through [`Disk`] and
//! returns the model plus the [`Cleanup`]s to perform. A job that was still
//! converting is never resumed and never claimed as done; it becomes
//! [`ItemState::Interrupted`], and if the engine had announced its
//! temporary file, exactly that path is removed, and only when it is a
//! regular file in the destination's folder named the way the engine names
//! them (`.<destination name>.<random>.partial`). Nothing is globbed or
//! deleted recursively, and a symlink, a folder or a file named otherwise
//! is left alone and reported.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use studio_domain::job::{JobSpec, SourceFingerprint, WirePath};
use studio_domain::options::OptionRequest;

use crate::analysis::Saved;
use crate::journal::{self, Journal, JournalItem, JournalState, JournalView};
use crate::queue::{Attempt, Cause, Destination, Ending, Item, ItemId, ItemState, PartialFate};
use crate::source::ProbeFailure;
use crate::workspace::Workspace;

/// What is at a path, without following a symlink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Nothing.
    Missing,
    /// A regular file.
    File,
    /// A symbolic link, wherever it points.
    Symlink,
    /// A folder.
    Directory,
    /// Something else, or it could not be inspected.
    Other,
}

/// The questions recovery asks the file system.
pub trait Disk {
    /// What is at `path`.
    fn kind(&self, path: &Path) -> Kind;

    /// Whether anything is at `path`, a dangling symlink included.
    fn exists(&self, path: &Path) -> bool {
        self.kind(path) != Kind::Missing
    }

    /// The size and modification time of the file at `path`, following a
    /// symlink as the engine does; `None` when that cannot be told.
    fn fingerprint(&self, _path: &Path) -> Option<SourceFingerprint> {
        None
    }
}

/// The real file system.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealDisk;

impl Disk for RealDisk {
    fn kind(&self, path: &Path) -> Kind {
        match fs::symlink_metadata(path) {
            Ok(m) if m.file_type().is_symlink() => Kind::Symlink,
            Ok(m) if m.is_file() => Kind::File,
            Ok(m) if m.is_dir() => Kind::Directory,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Kind::Missing,
            Ok(_) | Err(_) => Kind::Other,
        }
    }

    fn fingerprint(&self, path: &Path) -> Option<SourceFingerprint> {
        fs::metadata(path)
            .ok()
            .filter(fs::Metadata::is_file)
            .map(|m| SourceFingerprint::of(&m))
    }
}

/// Why `partial` is not a temporary file the engine made for `destination`,
/// or `Ok` when its path is one: same folder, `.<name>.<random>.partial`.
///
/// # Errors
/// The reason, for the user.
pub fn check_partial_path(partial: &Path, destination: &Path) -> Result<(), String> {
    let plain = |p: &Path| {
        p.components().all(|c| {
            matches!(
                c,
                Component::Normal(_) | Component::RootDir | Component::Prefix(_)
            )
        })
    };
    if !partial.is_absolute() || !plain(partial) || !plain(destination) {
        return Err("its path is not a plain absolute path".to_owned());
    }
    if partial.parent() != destination.parent() {
        return Err("it is not in the output file's folder".to_owned());
    }
    let (Some(name), Some(target)) = (
        partial.file_name().and_then(|n| n.to_str()),
        destination.file_name().and_then(|n| n.to_str()),
    ) else {
        return Err("its name is not the engine's temporary file name".to_owned());
    };
    let random = name
        .strip_prefix('.')
        .and_then(|n| n.strip_prefix(target))
        .and_then(|n| n.strip_prefix('.'))
        .and_then(|n| n.strip_suffix(".partial"));
    match random {
        Some(r) if !r.is_empty() && r.bytes().all(|b| b.is_ascii_alphanumeric()) => Ok(()),
        _ => Err("its name is not the engine's temporary file name".to_owned()),
    }
}

/// What to do about an announced temporary file, judged through `disk`.
#[must_use]
pub fn plan_partial(partial: &Path, destination: &Path, disk: &dyn Disk) -> PartialFate {
    let kept = |reason: &str| PartialFate::Kept {
        path: partial.to_path_buf(),
        reason: reason.to_owned(),
    };
    if let Err(reason) = check_partial_path(partial, destination) {
        return kept(&reason);
    }
    match disk.kind(partial) {
        Kind::Missing => PartialFate::Gone,
        Kind::File => PartialFate::Pending(partial.to_path_buf()),
        Kind::Symlink => kept("it is a symbolic link"),
        Kind::Directory => kept("it is a folder"),
        Kind::Other => kept("it is not a regular file"),
    }
}

/// Removes `partial` if, right now, it is a regular file with the
/// engine's name for a temporary file of `destination`.
#[must_use]
pub fn remove_partial(partial: &Path, destination: &Path) -> PartialFate {
    match plan_partial(partial, destination, &RealDisk) {
        PartialFate::Pending(path) => match fs::remove_file(&path) {
            Ok(()) => PartialFate::Removed(path),
            Err(e) if e.kind() == io::ErrorKind::NotFound => PartialFate::Gone,
            Err(e) => PartialFate::Kept {
                path,
                reason: format!("it could not be removed: {e}"),
            },
        },
        other => other,
    }
}

/// A temporary file to remove with [`remove_partial`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cleanup {
    /// The item it belongs to.
    pub item: ItemId,
    /// The temporary file.
    pub partial: PathBuf,
    /// The output it was for.
    pub destination: PathBuf,
}

/// The queue as the journal left it, reconciled with the disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recovered {
    /// The items, in list order.
    pub items: Vec<Item>,
    /// The next item id.
    pub next_id: ItemId,
    /// The settings.
    pub settings: OptionRequest,
    /// Temporary files to remove now.
    pub cleanups: Vec<Cleanup>,
    /// Items that could not be restored: a duplicate id, or a path written
    /// on another kind of system.
    pub dropped: usize,
    /// The workspace and documents the window showed.
    pub view: RestoredView,
}

/// The view the journal recorded. Its documents are only where to read
/// them again; nothing about their files is restored before that.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoredView {
    /// The workspace shown.
    pub workspace: Workspace,
    /// The Analyze documents.
    pub documents: Vec<Saved>,
    /// The index of the document shown.
    pub shown: usize,
    /// Documents whose path was written on another kind of system.
    pub dropped: usize,
    /// The view did not parse and nothing of it was restored; a copy of
    /// the journal as it was is here.
    pub unreadable: Option<PathBuf>,
}

/// The model's values of a journalled view.
#[must_use]
pub fn view(dto: &JournalView) -> RestoredView {
    let kept: Vec<(usize, Saved)> = dto
        .documents
        .iter()
        .enumerate()
        .filter_map(|(at, d)| journal::saved(d).map(|saved| (at, saved)))
        .collect();
    // The shown one, counted among those kept.
    let shown = kept
        .iter()
        .position(|(at, _)| *at == dto.shown_document)
        .unwrap_or(0);
    RestoredView {
        workspace: journal::workspace(dto.workspace),
        dropped: dto.documents.len() - kept.len(),
        documents: kept.into_iter().map(|(_, saved)| saved).collect(),
        shown,
        unreadable: None,
    }
}

/// Rebuilds the queue from `journal`. Items that were converting become
/// Interrupted; temporary files that are still there are listed in
/// [`Recovered::cleanups`] (and marked [`PartialFate::Pending`]).
#[must_use]
pub fn recover(journal: Journal, disk: &dyn Disk) -> Recovered {
    let view = view(&journal.view);
    let mut items: Vec<Item> = Vec::with_capacity(journal.items.len());
    let mut dropped = 0;
    for dto in journal.items {
        match item(dto, disk) {
            Some(item) if items.iter().all(|i| i.id != item.id) => items.push(item),
            _ => dropped += 1,
        }
    }
    let cleanups = items
        .iter()
        .filter_map(
            |item| match (item.ending().and_then(Ending::partial), item.attempt()) {
                (Some(PartialFate::Pending(partial)), Some(attempt)) => Some(Cleanup {
                    item: item.id,
                    partial: partial.clone(),
                    destination: attempt.job.destination.to_path_buf().ok()?,
                }),
                _ => None,
            },
        )
        .collect();
    let next_id = items
        .iter()
        .map(|i| i.id.saturating_add(1))
        .fold(journal.next_id.max(1), u64::max);
    Recovered {
        items,
        next_id,
        settings: journal.settings,
        cleanups,
        dropped,
        view,
    }
}

/// Carries out `recovered`'s cleanups with [`remove_partial`] and records
/// what became of each file in its item.
pub fn clean_up(recovered: &mut Recovered) {
    for cleanup in std::mem::take(&mut recovered.cleanups) {
        let fate = remove_partial(&cleanup.partial, &cleanup.destination);
        let ending = recovered
            .items
            .iter_mut()
            .find(|i| i.id == cleanup.item)
            .and_then(|i| i.attempt.as_mut())
            .and_then(|a| a.ending.as_mut());
        if let Some(ending) = ending {
            ending.set_partial(fate);
        }
    }
}

fn item(dto: JournalItem, disk: &dyn Disk) -> Option<Item> {
    let source = dto.source.to_path_buf().ok()?;
    let destination = dto.destination.to_path_buf().ok()?;
    let mut item = Item::new(dto.id, source, destination.clone());
    item.size = dto.size;
    item.stream = dto.video_stream;
    item.facts = dto.facts;
    item.destination = Destination {
        path: destination,
        chosen: dto.destination_chosen,
        exists: false,
    };
    item.ack = dto.acknowledgement.as_ref().map(journal::acknowledgement);
    item.submitted = dto.submitted;
    let (state, mut attempt) = match dto.state {
        // A waiting item is only ever made from facts; without them, read it.
        JournalState::Waiting if item.facts.is_some() => (ItemState::Waiting, None),
        JournalState::Probing | JournalState::Waiting => (ItemState::Probing, None),
        JournalState::Unreadable { code, message } => {
            item.probe_failure = Some(ProbeFailure { code, message });
            (ItemState::Unreadable, None)
        }
        JournalState::Running { job, partial } => {
            interrupted(job, false, Cause::AppStopped, partial.as_ref(), disk)?
        }
        JournalState::Publishing { job, partial } => {
            interrupted(job, true, Cause::AppStopped, partial.as_ref(), disk)?
        }
        JournalState::Interrupted {
            job,
            publishing,
            cause,
            partial,
            ..
        } => {
            // A temporary file not dealt with yet (the app was closed while
            // converting) is judged now; an earlier decision is kept.
            let fate = journal::partial_fate(&partial);
            let pending = match &fate {
                PartialFate::Pending(path) => Some(WirePath::from_path(path)),
                _ => None,
            };
            let (state, mut attempt) = interrupted(
                job,
                publishing,
                journal::cause(cause),
                pending.as_ref(),
                disk,
            )?;
            if pending.is_none()
                && let Some(ending) = attempt.as_mut().and_then(|a| a.ending.as_mut())
            {
                ending.set_partial(fate);
            }
            (state, attempt)
        }
        JournalState::Completed { job, report } => {
            ended(ItemState::Completed, job, Ending::Completed(report))
        }
        JournalState::Cancelled { job } => ended(ItemState::Cancelled, job, Ending::Cancelled),
        JournalState::Failed { job, failure } => {
            ended(ItemState::Failed, job, Ending::Failed(failure))
        }
        JournalState::Lost {
            job,
            why,
            publishing,
            partial,
        } => {
            let destination = job.destination.to_path_buf().ok()?;
            let partial = match journal::partial_fate(&partial) {
                PartialFate::Pending(path) => plan_partial(&path, &destination, disk),
                other => other,
            };
            ended(
                ItemState::Lost,
                job,
                Ending::Lost {
                    why,
                    publishing,
                    partial,
                },
            )
        }
    };
    if let Some(attempt) = &mut attempt {
        attempt.policy = dto.attempt_policy;
    }
    item.state = state;
    item.attempt = attempt;
    Some(item)
}

fn ended(state: ItemState, job: JobSpec, ending: Ending) -> (ItemState, Option<Attempt>) {
    (
        state,
        Some(Attempt {
            job,
            request: 0,
            activity: None,
            partial: None,
            ending: Some(ending),
            policy: Vec::new(),
        }),
    )
}

/// An interrupted attempt of `job`: whether its destination is there now
/// and what to do about its temporary file.
fn interrupted(
    job: JobSpec,
    publishing: bool,
    cause: Cause,
    partial: Option<&WirePath>,
    disk: &dyn Disk,
) -> Option<(ItemState, Option<Attempt>)> {
    let destination = job.destination.to_path_buf().ok()?;
    let fate = match partial.map(WirePath::to_path_buf) {
        None => PartialFate::None,
        Some(Ok(path)) => plan_partial(&path, &destination, disk),
        Some(Err(_)) => PartialFate::Kept {
            path: PathBuf::new(),
            reason: "its path was written on another kind of system".to_owned(),
        },
    };
    let ending = Ending::Interrupted(journal::interruption(
        publishing,
        cause,
        disk.exists(&destination),
        fate,
    ));
    Some(ended(ItemState::Interrupted, job, ending))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use studio_domain::job::{
        ChromaLayout, Failure, FailureCode, JobReport, Omissions, Rational, SourceColor,
        SourceFacts, SourceFingerprint, SourceReader,
    };

    use super::*;
    use crate::journal::{JournalCause, JournalPartial, VERSION};
    use crate::queue::Interruption;
    use crate::state::INITIAL_SETTINGS;
    use crate::testing::{Scratch, abs};

    fn destination_path() -> PathBuf {
        abs("/out/clip.ivf")
    }

    fn partial_path() -> PathBuf {
        abs("/out/.clip.ivf.x7Qp2a.partial")
    }

    /// A disk that holds exactly the listed paths.
    #[derive(Default)]
    struct FakeDisk(HashMap<PathBuf, Kind>);

    impl Disk for FakeDisk {
        fn kind(&self, path: &Path) -> Kind {
            self.0.get(path).copied().unwrap_or(Kind::Missing)
        }
    }

    fn job() -> JobSpec {
        JobSpec {
            source: WirePath::from_path(&abs("/in/clip.y4m")),
            fingerprint: SourceFingerprint {
                len: 1,
                modified: None,
            },
            video_stream: 0,
            omissions_acknowledged: false,
            destination: WirePath::from_path(&destination_path()),
            settings: INITIAL_SETTINGS.into(),
            identity: None,
            pass_artifact: None,
        }
    }

    fn facts() -> SourceFacts {
        SourceFacts {
            reader: SourceReader::Y4m,
            fingerprint: SourceFingerprint {
                len: 1,
                modified: None,
            },
            video_stream: 0,
            video_tracks: Vec::new(),
            omissions: Omissions::default(),
            width: 64,
            height: 36,
            frame_rate: Rational { num: 25, den: 1 },
            pixel_aspect: None,
            color: SourceColor::default(),
            bit_depth: 8,
            chroma: ChromaLayout::Yuv420,
            frame_count: Some(3),
            disclosures: Vec::new(),
        }
    }

    fn journal(state: JournalState) -> Journal {
        Journal {
            version: VERSION,
            next_id: 2,
            settings: INITIAL_SETTINGS.into(),
            view: JournalView::default(),
            items: vec![JournalItem {
                id: 1,
                source: WirePath::from_path(&abs("/in/clip.y4m")),
                size: Some(10),
                video_stream: None,
                facts: Some(facts()),
                destination: WirePath::from_path(&destination_path()),
                destination_chosen: false,
                acknowledgement: None,
                submitted: None,
                attempt_policy: Vec::new(),
                state,
            }],
        }
    }

    /// The temporary-file situations, each with the path the engine
    /// announced (if any) and what is on disk there.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Partial {
        NotAnnounced,
        Present,
        Absent,
        Symlink,
        Directory,
        OutsideFolder,
        WrongName,
    }

    const PARTIALS: [Partial; 7] = [
        Partial::NotAnnounced,
        Partial::Present,
        Partial::Absent,
        Partial::Symlink,
        Partial::Directory,
        Partial::OutsideFolder,
        Partial::WrongName,
    ];

    impl Partial {
        fn path(self) -> Option<PathBuf> {
            match self {
                Self::NotAnnounced => None,
                Self::OutsideFolder => Some(abs("/elsewhere/.clip.ivf.x7Qp2a.partial")),
                Self::WrongName => Some(abs("/out/clip.ivf.partial")),
                _ => Some(partial_path()),
            }
        }

        fn on_disk(self) -> Option<Kind> {
            match self {
                Self::NotAnnounced | Self::Absent => None,
                Self::Symlink => Some(Kind::Symlink),
                Self::Directory => Some(Kind::Directory),
                Self::Present | Self::OutsideFolder | Self::WrongName => Some(Kind::File),
            }
        }

        /// What recovery must decide.
        fn expected(self) -> PartialFate {
            let kept = |reason: &str| PartialFate::Kept {
                path: self.path().unwrap_or_default(),
                reason: reason.to_owned(),
            };
            match self {
                Self::NotAnnounced => PartialFate::None,
                Self::Present => PartialFate::Pending(partial_path()),
                Self::Absent => PartialFate::Gone,
                Self::Symlink => kept("it is a symbolic link"),
                Self::Directory => kept("it is a folder"),
                Self::OutsideFolder => kept("it is not in the output file's folder"),
                Self::WrongName => kept("its name is not the engine's temporary file name"),
            }
        }
    }

    /// Every journalled state, given the temporary file a converting one
    /// announced.
    fn states(partial: Option<&Path>) -> Vec<JournalState> {
        let wire = partial.map(WirePath::from_path);
        let pending = wire
            .clone()
            .map_or(JournalPartial::None, |path| JournalPartial::Pending {
                path,
            });
        vec![
            JournalState::Probing,
            JournalState::Waiting,
            JournalState::Unreadable {
                code: Some(FailureCode::InvalidSource),
                message: "not a video".into(),
            },
            JournalState::Running {
                job: job(),
                partial: wire.clone(),
            },
            JournalState::Publishing {
                job: job(),
                partial: wire,
            },
            JournalState::Completed {
                job: job(),
                report: JobReport {
                    destination: WirePath::from_path(&destination_path()),
                    frames: 3,
                    bytes: 99,
                    checks: Vec::new(),
                    disclosures: Vec::new(),
                },
            },
            JournalState::Cancelled { job: job() },
            JournalState::Failed {
                job: job(),
                failure: Failure {
                    code: FailureCode::Output,
                    message: "disk full".into(),
                },
            },
            JournalState::Lost {
                job: job(),
                why: "killed".into(),
                publishing: false,
                partial: pending.clone(),
            },
            JournalState::Interrupted {
                job: job(),
                publishing: false,
                cause: JournalCause::AppClosed,
                destination_present: false,
                partial: pending,
            },
        ]
    }

    #[test]
    fn every_state_with_every_destination_and_temporary_file() {
        for destination_present in [false, true] {
            for partial in PARTIALS {
                for dto in states(partial.path().as_deref()) {
                    let mut disk = FakeDisk::default();
                    if destination_present {
                        disk.0.insert(destination_path(), Kind::File);
                    }
                    if let (Some(path), Some(kind)) = (partial.path(), partial.on_disk()) {
                        disk.0.insert(path, kind);
                    }
                    let case = format!("{dto:?} / {partial:?} / destination {destination_present}");
                    let recovered = recover(journal(dto.clone()), &disk);
                    assert_eq!(recovered.dropped, 0, "{case}");
                    assert_eq!(recovered.next_id, 2, "{case}");
                    let [item] = recovered.items.as_slice() else {
                        unreachable!("{case}: {recovered:?}")
                    };
                    let converting = matches!(
                        dto,
                        JournalState::Running { .. }
                            | JournalState::Publishing { .. }
                            | JournalState::Interrupted { .. }
                    );
                    let expected_state = match dto {
                        JournalState::Probing => ItemState::Probing,
                        JournalState::Waiting => ItemState::Waiting,
                        JournalState::Unreadable { .. } => ItemState::Unreadable,
                        JournalState::Completed { .. } => ItemState::Completed,
                        JournalState::Cancelled { .. } => ItemState::Cancelled,
                        JournalState::Failed { .. } => ItemState::Failed,
                        JournalState::Lost { .. } => ItemState::Lost,
                        _ => ItemState::Interrupted,
                    };
                    assert_eq!(item.state(), expected_state, "{case}");
                    // Only a converting or lost job's temporary file is judged.
                    let judged = converting || matches!(dto, JournalState::Lost { .. });
                    let fate = item.ending().and_then(Ending::partial).cloned();
                    let expected_fate = judged.then(|| partial.expected());
                    assert_eq!(fate, expected_fate, "{case}");
                    let cleanups: Vec<_> = recovered.cleanups.iter().map(|c| &c.partial).collect();
                    if judged && partial == Partial::Present {
                        assert_eq!(cleanups, [&partial_path()], "{case}");
                        assert_eq!(recovered.cleanups[0].destination, destination_path());
                    } else {
                        assert_eq!(cleanups.len(), 0, "{case}");
                    }
                    if converting {
                        let Some(Ending::Interrupted(Interruption {
                            publishing,
                            destination_present: present,
                            cause,
                            ..
                        })) = item.ending()
                        else {
                            unreachable!("{case}")
                        };
                        assert_eq!(*present, destination_present, "{case}");
                        assert_eq!(
                            *publishing,
                            matches!(dto, JournalState::Publishing { .. }),
                            "{case}"
                        );
                        let closed = matches!(dto, JournalState::Interrupted { .. });
                        let expected_cause = if closed {
                            Cause::AppClosed
                        } else {
                            Cause::AppStopped
                        };
                        assert_eq!(*cause, expected_cause, "{case}");
                        // Never claimed as done, never resumable as such.
                        assert!(item.state().can_retry(), "{case}");
                    }
                }
            }
        }
    }

    #[test]
    fn an_earlier_decision_about_a_temporary_file_is_kept() {
        let dto = JournalState::Interrupted {
            job: job(),
            publishing: true,
            cause: JournalCause::AppStopped,
            destination_present: false,
            partial: JournalPartial::Removed {
                path: WirePath::from_path(&partial_path()),
            },
        };
        let mut disk = FakeDisk::default();
        disk.0.insert(destination_path(), Kind::File);
        let recovered = recover(journal(dto), &disk);
        assert_eq!(recovered.cleanups, []);
        let fate = recovered.items[0]
            .ending()
            .and_then(Ending::partial)
            .cloned();
        assert_eq!(fate, Some(PartialFate::Removed(partial_path())));
        // Whether the destination is there is looked at again.
        assert!(matches!(
            recovered.items[0].ending(),
            Some(Ending::Interrupted(Interruption {
                destination_present: true,
                publishing: true,
                ..
            }))
        ));
    }

    #[test]
    fn frozen_submission_and_interrupted_policy_survive_recovery() -> serde_json::Result<()> {
        use studio_domain::options::{EffectiveOption, OPTIONS_SCHEMA_VERSION, OptionValue};
        use studio_domain::protocol::OptionsIdentity;

        use crate::queue::Submission;

        let mut frozen = job();
        frozen
            .settings
            .values
            .insert("fast_decode".into(), OptionValue::Bool(false));
        frozen.identity = Some(OptionsIdentity {
            schema: OPTIONS_SCHEMA_VERSION,
            codec_revision: "0".repeat(40),
        });
        frozen.pass_artifact = Some(WirePath::from_path(&abs("/in/clip.pass")));
        let policy = vec![EffectiveOption {
            key: "fast_decode".into(),
            requested: Some(OptionValue::Bool(false)),
            value: OptionValue::Bool(false),
            origin: OptionValue::Choice("explicit".into()),
            screen: None,
            screen_origin: None,
        }];

        let mut stored = journal(JournalState::Waiting);
        stored.items[0].submitted = Some(Submission {
            job: frozen.clone(),
            policy: policy.clone(),
        });
        let mut running = stored.items[0].clone();
        running.id = 2;
        running.submitted = None;
        running.state = JournalState::Running {
            job: frozen.clone(),
            partial: None,
        };
        running.attempt_policy = policy.clone();
        stored.items.push(running);
        let bytes = journal::encode(&stored)?;
        let restored = recover(serde_json::from_slice(&bytes)?, &FakeDisk::default());
        assert_eq!(restored.items[0].state(), ItemState::Waiting);
        assert!(restored.items[0].attempt().is_none());
        assert_eq!(restored.items[0].submitted, stored.items[0].submitted);
        assert_eq!(restored.items[1].state(), ItemState::Interrupted);
        assert_eq!(restored.cleanups.as_slice(), &[]);
        let attempt = restored.items[1]
            .attempt()
            .unwrap_or_else(|| unreachable!());
        assert_eq!(attempt.job, frozen);
        assert_eq!(attempt.policy, policy);
        assert_eq!(attempt.request, 0);

        let saved = journal::snapshot(
            restored.next_id,
            restored.settings,
            &restored.items,
            JournalView::default(),
        );
        assert_eq!(saved.items[0].submitted, stored.items[0].submitted);
        assert_eq!(saved.items[1].attempt_policy, policy);
        assert!(
            matches!(&saved.items[1].state, JournalState::Interrupted { job, .. } if job == &frozen)
        );
        Ok(())
    }

    #[test]
    fn duplicate_ids_and_foreign_paths_are_dropped_and_ids_stay_unique() {
        let mut j = journal(JournalState::Waiting);
        let mut twin = j.items[0].clone();
        twin.source = WirePath::from_path(&abs("/in/twin.y4m"));
        let mut foreign = j.items[0].clone();
        foreign.id = 5;
        // A non-Unicode path from the other platform family.
        #[cfg(not(windows))]
        let other = WirePath::Windows(vec![0xD800]);
        #[cfg(windows)]
        let other = WirePath::Unix(vec![0xff]);
        foreign.source = other;
        let mut late = j.items[0].clone();
        late.id = 40;
        j.items.extend([twin, foreign, late]);
        j.next_id = 3;
        let recovered = recover(j, &FakeDisk::default());
        assert_eq!(recovered.dropped, 2);
        assert_eq!(recovered.items[0].source(), abs("/in/clip.y4m"));
        // Never below an id already used.
        assert_eq!(recovered.next_id, 41);
    }

    #[test]
    fn the_view_comes_back_as_documents_to_read_again() {
        use crate::analysis::Origin;
        use crate::journal::{JournalDocument, JournalOrigin, JournalWorkspace};
        let mut j = journal(JournalState::Waiting);
        #[cfg(not(windows))]
        let foreign = WirePath::Windows(vec![0xD800]);
        #[cfg(windows)]
        let foreign = WirePath::Unix(vec![0xff]);
        let document = |source: WirePath, origin| JournalDocument {
            source,
            origin,
            read: None,
        };
        j.view = JournalView {
            workspace: JournalWorkspace::Analyze,
            // The last one, the second kept.
            shown_document: 3,
            documents: vec![
                document(WirePath::from_path(&abs("/in/a.obu")), JournalOrigin::File),
                document(foreign.clone(), JournalOrigin::File),
                document(
                    WirePath::from_path(&abs("/out/clip.ivf")),
                    JournalOrigin::Result {
                        item: 1,
                        source: foreign,
                        bytes: 9,
                    },
                ),
                document(
                    WirePath::from_path(&abs("/out/clip.ivf")),
                    JournalOrigin::Result {
                        item: 1,
                        source: WirePath::from_path(&abs("/in/clip.y4m")),
                        bytes: 9,
                    },
                ),
            ],
        };
        let view = recover(j, &FakeDisk::default()).view;
        assert_eq!(view.workspace, Workspace::Analyze);
        assert_eq!(view.shown, 1);
        // A path from the other platform family, in the file or its origin.
        assert_eq!(view.dropped, 2);
        let paths: Vec<_> = view.documents.iter().map(|d| d.path.clone()).collect();
        assert_eq!(paths, [abs("/in/a.obu"), abs("/out/clip.ivf")]);
        assert!(matches!(
            &view.documents[1].origin,
            Origin::Result(r) if r.output == abs("/out/clip.ivf") && r.bytes == 9
        ));
    }

    #[test]
    fn a_waiting_item_without_facts_is_read_again() {
        let mut j = journal(JournalState::Waiting);
        j.items[0].facts = None;
        let recovered = recover(j, &FakeDisk::default());
        assert_eq!(recovered.items[0].state(), ItemState::Probing);
    }

    #[test]
    fn partial_names_must_be_the_engines() {
        let ok = |p: &Path| check_partial_path(p, &destination_path()).is_ok();
        assert!(ok(&partial_path()));
        assert!(ok(&abs("/out/.clip.ivf.A.partial")));
        for bad in [
            "/out/.clip.ivf..partial",
            "/out/.clip.ivf.x7-Qp.partial",
            "/out/.clip.ivf.x7Qp2a.partial.more",
            "/out/clip.ivf.x7Qp2a.partial",
            "/out/.clip.ivf.x7Qp2a",
            "/out/.other.ivf.x7Qp2a.partial",
            "/out/sub/.clip.ivf.x7Qp2a.partial",
            "/out/../out/.clip.ivf.x7Qp2a.partial",
            "/",
        ] {
            assert!(!ok(&abs(bad)), "{bad}");
        }
        // Relative, and on Windows also rooted without a drive: never absolute.
        assert!(!ok(Path::new("out/.clip.ivf.x7Qp2a.partial")));
        #[cfg(windows)]
        assert!(!ok(Path::new("/out/.clip.ivf.x7Qp2a.partial")));
    }

    #[test]
    fn only_a_regular_file_with_the_right_name_is_removed() {
        let dir = Scratch::new("partials");
        let destination = dir.path("clip.ivf");
        let partial = dir.path(".clip.ivf.x7Qp2a.partial");
        let precious = dir.path("precious.mov");
        assert!(fs::write(&precious, b"keep").is_ok());

        // A folder with the right name: left alone, with what is inside.
        assert!(fs::create_dir(&partial).is_ok());
        assert!(fs::write(partial.join("inside"), b"x").is_ok());
        assert!(matches!(
            remove_partial(&partial, &destination),
            PartialFate::Kept { .. }
        ));
        assert!(partial.join("inside").exists());
        assert!(fs::remove_dir_all(&partial).is_ok());

        // A regular file with another name: left alone.
        assert!(matches!(
            remove_partial(&precious, &destination),
            PartialFate::Kept { .. }
        ));
        assert!(precious.exists());

        // The real thing: removed, exactly that file.
        assert!(fs::write(&partial, b"half a movie").is_ok());
        assert_eq!(
            remove_partial(&partial, &destination),
            PartialFate::Removed(partial.clone())
        );
        assert_eq!(dir.entries(), ["precious.mov"]);
        // Already gone.
        assert_eq!(remove_partial(&partial, &destination), PartialFate::Gone);
    }

    // Unix only: creating a symlink on Windows needs a privilege CI
    // runners do not grant.
    #[cfg(unix)]
    #[test]
    fn a_fingerprint_follows_a_symlink_to_a_file_only() {
        let dir = Scratch::new("fingerprint");
        let file = dir.path("clip.obu");
        assert!(fs::write(&file, b"twelve bytes").is_ok());
        let link = dir.path("link.obu");
        assert!(std::os::unix::fs::symlink(&file, &link).is_ok());
        let of_file = fs::metadata(&file).ok().map(|m| SourceFingerprint::of(&m));
        assert_eq!(of_file.map(|f| f.len), Some(12));
        assert_eq!(RealDisk.fingerprint(&link), of_file);
        assert_eq!(RealDisk.fingerprint(&file), of_file);
        // A folder, and a link to nothing: no fingerprint.
        assert_eq!(RealDisk.fingerprint(&dir.path("")), None);
        let dangling = dir.path("dangling.obu");
        assert!(std::os::unix::fs::symlink(dir.path("gone.obu"), &dangling).is_ok());
        assert_eq!(RealDisk.fingerprint(&dangling), None);
    }

    // Unix only: creating a symlink on Windows needs a privilege CI
    // runners do not grant.
    #[cfg(unix)]
    #[test]
    fn a_symlink_named_like_a_temporary_file_is_left_alone() {
        let dir = Scratch::new("partials");
        let destination = dir.path("clip.ivf");
        let partial = dir.path(".clip.ivf.x7Qp2a.partial");
        let precious = dir.path("precious.mov");
        assert!(fs::write(&precious, b"keep").is_ok());

        // A symlink with the right name: left alone, its target untouched.
        assert!(std::os::unix::fs::symlink(&precious, &partial).is_ok());
        let kept = remove_partial(&partial, &destination);
        assert!(
            matches!(kept, PartialFate::Kept { ref reason, .. } if reason.contains("symbolic"))
        );
        assert!(fs::symlink_metadata(&partial).is_ok());
        assert_eq!(fs::read(&precious).ok().as_deref(), Some(&b"keep"[..]));
    }
}
