// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The queue journal: one versioned JSON file, `queue.json`, in the app's
//! data folder, replaced atomically on every state change.
//!
//! The format is the DTOs below, not the app's types. They embed the
//! domain's transport values (`WirePath`, `JobSpec`, `SourceFacts`,
//! `JobReport`, `Failure`, settings, `SourceFingerprint`, and `RowFilter`
//! with its `KindFilter` and `LayerFilter`) as their serde forms; a change
//! to any of those forms needs a new [`VERSION`] and a migration.
//!
//! Version 2 adds the workspace and Analyze view. Version 3 adds typed
//! settings, frozen submissions and effective attempt policies. Version 4 adds
//! the job purpose and verified pass digest. Version 5 adds export jobs and
//! their publication receipts. Version 6 adds the Compare workspace and its
//! saved comparison. Older journals keep their items and reports. Legacy settings become Simple
//! requests, and unavailable policy and view data use their defaults. Waiting
//! items with incomplete source facts need a new probe before preflight.
//!
//! Load rules: a missing file is an empty queue; a newer [`VERSION`] (or a
//! file that cannot be read at all) is left untouched and this session does
//! not save. When only the view does not parse, the file is copied aside as
//! `queue.json.corrupt-<n>` and the queue and settings are restored with
//! the default view, so the next save loses nothing that is not in the
//! copy. Anything else that does not parse is moved aside the same way
//! and the queue starts empty.
//!
//! Writes go through [`Writer`], a thread that writes the latest submitted
//! journal (older unwritten ones are skipped), so a slow disk never holds up
//! the window.

use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};

use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use studio_domain::analysis::RowFilter;
use studio_domain::job::{
    Failure, FailureCode, JobReport, JobSpec, Omissions, SourceFacts, SourceFingerprint, WirePath,
};
use studio_domain::options::{EffectiveOption, OptionRequest};
use studio_domain::settings::EncoderSettings;

use crate::analysis::{Origin, Restore, ResultOf, Saved};
use crate::compare::SavedComparison;
use crate::queue::{
    Acknowledgement, Cause, Ending, Interruption, Item, ItemId, ItemState, PartialFate, Submission,
};
use crate::workspace::Workspace;

/// The journal format this build writes.
pub const VERSION: u32 = 6;

/// The oldest journal format this build reads and migrates.
pub const OLDEST_VERSION: u32 = 1;

/// The journal's file name in the data folder.
pub const FILE_NAME: &str = "queue.json";

/// The whole journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Journal {
    /// [`VERSION`] when written by this build.
    pub version: u32,
    /// The next item id; ids are never reused.
    pub next_id: ItemId,
    /// The current settings for editing jobs before submission.
    pub settings: OptionRequest,
    /// The items, in list order.
    pub items: Vec<JournalItem>,
    /// The workspace and the Analyze documents; absent before version 2.
    #[serde(default)]
    pub view: JournalView,
    /// Queued and completed exports, absent before version 5.
    #[serde(
        default,
        skip_serializing_if = "crate::export_jobs::ExportJobs::is_new"
    )]
    pub exports: crate::export_jobs::ExportJobs,
}

/// What the window showed: the workspace and the Analyze documents.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalView {
    /// The workspace shown.
    pub workspace: JournalWorkspace,
    /// The index of the document shown in Analyze.
    pub shown_document: usize,
    /// The Analyze documents, in the order they were opened.
    pub documents: Vec<JournalDocument>,
    /// The inputs and policy of the comparison in Compare, absent before
    /// version 6. The engine reads its files again; no fact about them is kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comparison: Option<SavedComparison>,
}

/// A [`Workspace`] that has a flow.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalWorkspace {
    /// Encode.
    #[default]
    Encode,
    /// Analyze.
    Analyze,
    /// Compare.
    Compare,
}

/// One Analyze document: where to read it and what to restore once it is
/// read again. No fact about the file is kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalDocument {
    /// The file.
    pub source: WirePath,
    /// Where it came from.
    pub origin: JournalOrigin,
    /// Its last good read and the view on it; `None` when it was never
    /// read.
    pub read: Option<JournalRead>,
}

/// An [`Origin`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JournalOrigin {
    /// Opened for analysis.
    File,
    /// A converted file from Encode.
    Result {
        /// The queue item that produced it.
        item: ItemId,
        /// Its source file.
        source: WirePath,
        /// Its size when the job verified it.
        bytes: u64,
    },
}

/// A [`Restore`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalRead {
    /// The file as it was read.
    pub fingerprint: SourceFingerprint,
    /// The selected element, by index.
    pub selected_element: Option<u64>,
    /// The navigator's filter.
    pub filter: RowFilter,
    /// The byte view was shown.
    pub bytes_shown: bool,
}

/// One item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalItem {
    /// Its id.
    pub id: ItemId,
    /// The source file.
    pub source: WirePath,
    /// The source file's size, for display.
    pub size: Option<u64>,
    /// The video stream asked for; `None` lets the engine pick.
    pub video_stream: Option<u32>,
    /// The last successful probe.
    pub facts: Option<SourceFacts>,
    /// Where its output goes.
    pub destination: WirePath,
    /// The user chose the destination.
    pub destination_chosen: bool,
    /// The recorded confirmation of what a video-only output leaves out.
    pub acknowledgement: Option<JournalAcknowledgement>,
    /// The frozen job and policy of a submitted item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submitted: Option<Submission>,
    /// The known effective policy of the last attempt.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempt_policy: Vec<EffectiveOption>,
    /// Its state and what the last attempt left.
    pub state: JournalState,
}

/// An [`Acknowledgement`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalAcknowledgement {
    /// The file that was looked at.
    pub fingerprint: SourceFingerprint,
    /// The video stream that is kept.
    pub video_stream: u32,
    /// What the output leaves out.
    pub omissions: Omissions,
    /// The settings it was given for.
    pub settings: EncoderSettings,
}

/// An item's state with the data that state carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JournalState {
    /// Not read yet; read again on load.
    Probing,
    /// Read and waiting.
    Waiting,
    /// Cannot be read.
    Unreadable {
        /// The engine's reason, if it gave one.
        code: Option<FailureCode>,
        /// The explanation.
        message: String,
    },
    /// Converting.
    Running {
        /// The job as sent.
        job: JobSpec,
        /// The temporary file the engine announced.
        partial: Option<WirePath>,
    },
    /// Converting, and the engine said it is publishing.
    Publishing {
        /// The job as sent.
        job: JobSpec,
        /// The temporary file the engine announced.
        partial: Option<WirePath>,
    },
    /// Published.
    Completed {
        /// The job as sent, settings included.
        job: JobSpec,
        /// What was produced and checked.
        report: JobReport,
    },
    /// Stopped on request.
    Cancelled {
        /// The job as sent.
        job: JobSpec,
    },
    /// The engine reported a failure.
    Failed {
        /// The job as sent.
        job: JobSpec,
        /// Why.
        failure: Failure,
    },
    /// The worker went away without a terminal event.
    Lost {
        /// The job as sent.
        job: JobSpec,
        /// What happened.
        why: String,
        /// The engine had said it was publishing.
        publishing: bool,
        /// Its temporary file.
        partial: JournalPartial,
    },
    /// The app closed or stopped while it converted.
    Interrupted {
        /// The job as sent.
        job: JobSpec,
        /// The engine had said it was publishing.
        publishing: bool,
        /// How the app stopped.
        cause: JournalCause,
        /// Something was at the destination when last checked.
        destination_present: bool,
        /// Its temporary file.
        partial: JournalPartial,
    },
}

/// A [`PartialFate`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "fate", rename_all = "snake_case")]
pub enum JournalPartial {
    /// None was announced.
    None,
    /// Not dealt with yet.
    Pending {
        /// The file.
        path: WirePath,
    },
    /// Already gone.
    Gone,
    /// Removed by the app.
    Removed {
        /// The file.
        path: WirePath,
    },
    /// Left alone.
    Kept {
        /// The file.
        path: WirePath,
        /// Why.
        reason: String,
    },
}

/// A [`Cause`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalCause {
    /// Closed with a job running.
    AppClosed,
    /// Found still converting at the next start.
    AppStopped,
}

/// The journal of `items` with `settings`, and of `view`.
#[must_use]
pub fn snapshot(
    next_id: ItemId,
    settings: OptionRequest,
    items: &[Item],
    view: JournalView,
) -> Journal {
    Journal {
        version: VERSION,
        next_id,
        settings,
        items: items.iter().map(item).collect(),
        view,
        exports: crate::export_jobs::ExportJobs::default(),
    }
}

/// The view record of `workspace` and the Analyze documents `saved`, with
/// the one at `shown` shown. The caller adds the comparison.
#[must_use]
pub fn view(workspace: Workspace, saved: &[Saved], shown: usize) -> JournalView {
    JournalView {
        workspace: match workspace {
            Workspace::Encode => JournalWorkspace::Encode,
            Workspace::Analyze => JournalWorkspace::Analyze,
            Workspace::Compare => JournalWorkspace::Compare,
        },
        comparison: None,
        shown_document: shown,
        documents: saved
            .iter()
            .map(|saved| JournalDocument {
                source: WirePath::from_path(&saved.path),
                origin: match &saved.origin {
                    Origin::File => JournalOrigin::File,
                    Origin::Result(result) => JournalOrigin::Result {
                        item: result.item,
                        source: WirePath::from_path(&result.source),
                        bytes: result.bytes,
                    },
                },
                read: saved.restore.map(|r| JournalRead {
                    fingerprint: r.fingerprint,
                    selected_element: r.selected,
                    filter: r.filter,
                    bytes_shown: r.bytes_shown,
                }),
            })
            .collect(),
    }
}

/// The model's value of a journalled document, or `None` when a path in
/// it was written on another kind of system.
#[must_use]
pub fn saved(dto: &JournalDocument) -> Option<Saved> {
    let path = dto.source.to_path_buf().ok()?;
    let origin = match &dto.origin {
        JournalOrigin::File => Origin::File,
        JournalOrigin::Result {
            item,
            source,
            bytes,
        } => Origin::Result(ResultOf {
            item: *item,
            source: source.to_path_buf().ok()?,
            output: path.clone(),
            bytes: *bytes,
        }),
    };
    Some(Saved {
        path,
        origin,
        restore: dto.read.map(|r| Restore {
            fingerprint: r.fingerprint,
            selected: r.selected_element,
            filter: r.filter,
            bytes_shown: r.bytes_shown,
        }),
    })
}

/// The model's value of a journalled workspace.
#[must_use]
pub const fn workspace(dto: JournalWorkspace) -> Workspace {
    match dto {
        JournalWorkspace::Encode => Workspace::Encode,
        JournalWorkspace::Analyze => Workspace::Analyze,
        JournalWorkspace::Compare => Workspace::Compare,
    }
}

fn item(item: &Item) -> JournalItem {
    JournalItem {
        id: item.id,
        source: WirePath::from_path(&item.source),
        size: item.size,
        video_stream: item.stream,
        facts: item.facts.clone(),
        destination: WirePath::from_path(&item.destination.path),
        destination_chosen: item.destination.chosen,
        acknowledgement: item.ack.map(|a| JournalAcknowledgement {
            fingerprint: a.fingerprint,
            video_stream: a.video_stream,
            omissions: a.omissions,
            settings: a.settings,
        }),
        submitted: item.submitted.clone(),
        attempt_policy: item
            .attempt
            .as_ref()
            .map_or_else(Vec::new, |attempt| attempt.policy.clone()),
        state: state(item),
    }
}

fn state(item: &Item) -> JournalState {
    let attempt = item.attempt.as_ref();
    let job = attempt.map(|a| a.job.clone());
    let partial = || {
        attempt
            .and_then(|a| a.partial.as_deref())
            .map(WirePath::from_path)
    };
    match (item.state, job, item.ending()) {
        (ItemState::Waiting, ..) => JournalState::Waiting,
        (ItemState::Unreadable, ..) => {
            let failure = item.probe_failure.clone();
            JournalState::Unreadable {
                code: failure.as_ref().and_then(|f| f.code),
                message: failure.map_or_default(|f| f.message),
            }
        }
        (ItemState::Running, Some(job), _) => JournalState::Running {
            job,
            partial: partial(),
        },
        (ItemState::Publishing, Some(job), _) => JournalState::Publishing {
            job,
            partial: partial(),
        },
        (ItemState::Completed, Some(job), Some(Ending::Completed(report))) => {
            JournalState::Completed {
                job,
                report: report.clone(),
            }
        }
        (ItemState::Cancelled, Some(job), _) => JournalState::Cancelled { job },
        (ItemState::Failed, Some(job), Some(Ending::Failed(failure))) => JournalState::Failed {
            job,
            failure: failure.clone(),
        },
        (
            ItemState::Lost,
            Some(job),
            Some(Ending::Lost {
                why,
                publishing,
                partial,
            }),
        ) => JournalState::Lost {
            job,
            why: why.clone(),
            publishing: *publishing,
            partial: partial_dto(partial),
        },
        (ItemState::Interrupted, Some(job), Some(Ending::Interrupted(i))) => {
            JournalState::Interrupted {
                job,
                publishing: i.publishing,
                cause: match i.cause {
                    Cause::AppClosed => JournalCause::AppClosed,
                    Cause::AppStopped => JournalCause::AppStopped,
                },
                destination_present: i.destination_present,
                partial: partial_dto(&i.partial),
            }
        }
        // Probing, and any state whose attempt data is missing (which the
        // model never produces): read the file again on load.
        _ => JournalState::Probing,
    }
}

fn partial_dto(fate: &PartialFate) -> JournalPartial {
    match fate {
        PartialFate::None => JournalPartial::None,
        PartialFate::Pending(path) => JournalPartial::Pending {
            path: WirePath::from_path(path),
        },
        PartialFate::Gone => JournalPartial::Gone,
        PartialFate::Removed(path) => JournalPartial::Removed {
            path: WirePath::from_path(path),
        },
        PartialFate::Kept { path, reason } => JournalPartial::Kept {
            path: WirePath::from_path(path),
            reason: reason.clone(),
        },
    }
}

/// The model's value of a journalled acknowledgement.
#[must_use]
pub const fn acknowledgement(dto: &JournalAcknowledgement) -> Acknowledgement {
    Acknowledgement {
        fingerprint: dto.fingerprint,
        video_stream: dto.video_stream,
        omissions: dto.omissions,
        settings: dto.settings,
    }
}

/// The model's value of a journalled temporary-file fate. A path from
/// another platform family is reported as kept, never acted on.
#[must_use]
pub fn partial_fate(dto: &JournalPartial) -> PartialFate {
    let path = |wire: &WirePath| wire.to_path_buf().ok();
    match dto {
        JournalPartial::None => PartialFate::None,
        JournalPartial::Gone => PartialFate::Gone,
        JournalPartial::Pending { path: p } => path(p).map_or_else(foreign, PartialFate::Pending),
        JournalPartial::Removed { path: p } => path(p).map_or_else(foreign, PartialFate::Removed),
        JournalPartial::Kept { path: p, reason } => {
            path(p).map_or_else(foreign, |path| PartialFate::Kept {
                path,
                reason: reason.clone(),
            })
        }
    }
}

fn foreign() -> PartialFate {
    PartialFate::Kept {
        path: PathBuf::new(),
        reason: "its path was written on another kind of system".to_owned(),
    }
}

/// The model's value of a journalled interruption cause.
#[must_use]
pub const fn cause(dto: JournalCause) -> Cause {
    match dto {
        JournalCause::AppClosed => Cause::AppClosed,
        JournalCause::AppStopped => Cause::AppStopped,
    }
}

/// An interruption, with `partial` already converted.
#[must_use]
pub const fn interruption(
    publishing: bool,
    cause: Cause,
    destination_present: bool,
    partial: PartialFate,
) -> Interruption {
    Interruption {
        publishing,
        cause,
        destination_present,
        partial,
    }
}

/// The bytes written for `journal`.
///
/// # Errors
/// Only if serialization fails, which these DTOs never do.
pub fn encode(journal: &Journal) -> serde_json::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(journal)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// What [`load`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Loaded {
    /// No journal: an empty queue.
    Missing,
    /// A journal this build reads.
    Found(Journal),
    /// A journal from a newer build. It was left untouched and must not be
    /// overwritten.
    TooNew {
        /// The version it states.
        version: u32,
    },
    /// The queue and settings parsed and the view did not. The file was
    /// copied here first; the journal comes with the default view.
    ViewUnreadable {
        /// The journal without its view.
        journal: Journal,
        /// The copy of the file as it was.
        copy: PathBuf,
        /// What was wrong with the view.
        error: String,
    },
    /// The journal did not parse and was moved here; the queue starts empty.
    MovedAside {
        /// Where it is now.
        to: PathBuf,
        /// What was wrong with it.
        error: String,
    },
    /// The journal could not be read, or not moved aside. It was left
    /// untouched and must not be overwritten.
    Unreadable {
        /// Why.
        error: String,
    },
}

impl Loaded {
    /// Whether this session may write the journal.
    #[must_use]
    pub const fn may_save(&self) -> bool {
        matches!(
            self,
            Self::Missing | Self::Found(_) | Self::ViewUnreadable { .. } | Self::MovedAside { .. }
        )
    }
}

/// Only the field that must be readable at every version.
#[derive(Deserialize)]
struct Header {
    version: u32,
}

/// Reads the journal at `path`, applying the load rules.
#[must_use]
pub fn load(path: &Path) -> Loaded {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Loaded::Missing,
        Err(e) => {
            return Loaded::Unreadable {
                error: e.to_string(),
            };
        }
    };
    let error = match serde_json::from_slice::<Header>(&bytes) {
        Ok(Header { version }) if version > VERSION => return Loaded::TooNew { version },
        Ok(Header { version }) if version < OLDEST_VERSION => {
            format!("unknown version {version}")
        }
        // Serde supplies additions and converts legacy Simple requests.
        Ok(_) => match serde_json::from_slice::<Journal>(&bytes) {
            Ok(journal) => return Loaded::Found(migrate(journal, &bytes)),
            Err(e) => match without_view(&bytes) {
                Some(journal) => {
                    // The copy is synced before any save can replace the file.
                    return match free_name(path).and_then(|copy| {
                        fs::copy(path, &copy)?;
                        // Writable: Windows flushes only a handle that can write.
                        OpenOptions::new().write(true).open(&copy)?.sync_all()?;
                        Ok(copy)
                    }) {
                        Ok(copy) => Loaded::ViewUnreadable {
                            journal: migrate(journal, &bytes),
                            copy,
                            error: e.to_string(),
                        },
                        Err(c) => Loaded::Unreadable {
                            error: format!("{e}; it could not be copied aside: {c}"),
                        },
                    };
                }
                None => e.to_string(),
            },
        },
        Err(e) => e.to_string(),
    };
    match move_aside(path) {
        Ok(to) => Loaded::MovedAside { to, error },
        Err(e) => Loaded::Unreadable {
            error: format!("{error}; it could not be moved aside: {e}"),
        },
    }
}

#[derive(Deserialize)]
struct LegacySources {
    items: Vec<LegacySource>,
}

#[derive(Deserialize)]
struct LegacySource {
    facts: Option<LegacyColor>,
}

#[derive(Deserialize)]
struct LegacyColor {
    color: Option<IgnoredAny>,
}

fn migrate(mut journal: Journal, bytes: &[u8]) -> Journal {
    if journal.version < 3 {
        // SourceFacts defaults an absent colour record. Check the original
        // journal so explicitly unknown values from newer probes stay valid.
        let sources = serde_json::from_slice::<LegacySources>(bytes).ok();
        for (index, item) in journal.items.iter_mut().enumerate() {
            let has_color = sources
                .as_ref()
                .and_then(|sources| sources.items.get(index))
                .and_then(|item| item.facts.as_ref())
                .is_some_and(|facts| facts.color.is_some());
            if matches!(item.state, JournalState::Waiting) && !has_color {
                if let Some(facts) = &item.facts {
                    item.video_stream = item.video_stream.or(Some(facts.video_stream));
                }
                item.state = JournalState::Probing;
            }
        }
    }
    journal.version = VERSION;
    journal
}

/// The journal in `bytes` with its view left out, when the rest parses.
fn without_view(bytes: &[u8]) -> Option<Journal> {
    let mut value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    value.as_object_mut()?.remove("view")?;
    serde_json::from_value(value).ok()
}

/// Renames `path` to the first free `<name>.corrupt-<n>` beside it.
fn move_aside(path: &Path) -> io::Result<PathBuf> {
    let to = free_name(path)?;
    fs::rename(path, &to)?;
    Ok(to)
}

/// The first free `<name>.corrupt-<n>` beside `path`.
fn free_name(path: &Path) -> io::Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("the journal path has no file name"))?
        .to_string_lossy()
        .into_owned();
    for n in 1..=1000 {
        let to = path.with_file_name(format!("{name}.corrupt-{n}"));
        // Only this instance touches the folder (it holds the lock), so the
        // name is still free when it is used.
        match fs::symlink_metadata(&to) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(to),
            Err(e) => return Err(e),
            Ok(_) => {}
        }
    }
    Err(io::Error::other("no free name to move it to"))
}

/// Replaces `path` with `bytes` atomically: a temporary file in the same
/// folder is written, flushed and synced, then renamed over `path`; on Unix
/// the folder is synced too, so the new name survives a power loss.
///
/// # Errors
/// Any I/O error; `path` then still holds its previous contents.
pub fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("the journal path has no file name"))?;
    let mut temp_name = name.to_owned();
    temp_name.push(".tmp");
    let temp = path.with_file_name(temp_name);
    let written = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temp)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            file.flush()?;
            file.sync_all()
        })
        .and_then(|()| fs::rename(&temp, path));
    if let Err(e) = written {
        let _ = fs::remove_file(&temp);
        return Err(e);
    }
    #[cfg(unix)]
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

/// The state shared between [`Writer`] and its thread.
type SaveNotice = Box<dyn FnOnce(Result<(), String>) + Send>;

struct ConfirmedWrite {
    bytes: Vec<u8>,
    generation: u64,
    notify: SaveNotice,
}

#[derive(Default)]
struct Slot {
    /// The newest ordinary journal not yet written.
    pending: Option<Vec<u8>>,
    /// A snapshot for which the session waits before it starts a job.
    confirmed: Option<ConfirmedWrite>,
    /// How many journals were submitted, and how many of those are settled
    /// (written, or skipped for a newer one).
    submitted: u64,
    settled: u64,
    closed: bool,
}

type Shared = Arc<(Mutex<Slot>, Condvar)>;

fn lock(shared: &Shared) -> MutexGuard<'_, Slot> {
    // A panic while holding the lock leaves at worst an unwritten journal.
    shared.0.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Writes journals on its own thread. Only the newest submitted journal is
/// written; one submitted while a write is under way waits for it.
pub struct Writer {
    shared: Shared,
    thread: Option<JoinHandle<()>>,
}

impl Writer {
    /// Starts the writer for `path`. `report` is called on the writer thread
    /// whenever saving starts failing, fails differently or works again,
    /// with `Err(reason)` or `Ok(())`.
    ///
    /// # Errors
    /// When the thread cannot be started.
    pub fn spawn(
        path: PathBuf,
        report: impl Fn(Result<(), String>) + Send + 'static,
    ) -> io::Result<Self> {
        let shared: Shared = Arc::default();
        let worker = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("queue-journal".to_owned())
            .spawn(move || {
                let mut last: Result<(), String> = Ok(());
                loop {
                    let (bytes, upto, notify) = {
                        let mut slot = lock(&worker);
                        while slot.pending.is_none() && slot.confirmed.is_none() && !slot.closed {
                            slot = worker.1.wait(slot).unwrap_or_else(PoisonError::into_inner);
                        }
                        if let Some(confirmed) = slot.confirmed.take() {
                            (
                                confirmed.bytes,
                                confirmed.generation,
                                Some(confirmed.notify),
                            )
                        } else if let Some(bytes) = slot.pending.take() {
                            (bytes, slot.submitted, None)
                        } else {
                            return;
                        }
                    };
                    let result = write_atomically(&path, &bytes).map_err(|e| e.to_string());
                    if result != last {
                        report(result.clone());
                        last.clone_from(&result);
                    }
                    lock(&worker).settled = upto;
                    worker.1.notify_all();
                    if let Some(notify) = notify {
                        notify(result);
                    }
                }
            })?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// Queues `bytes` as the newest journal, replacing any unwritten one.
    pub fn submit(&self, bytes: Vec<u8>) {
        let mut slot = lock(&self.shared);
        slot.pending = Some(bytes);
        slot.submitted += 1;
        drop(slot);
        self.shared.1.notify_all();
    }

    /// Writes this exact snapshot before it writes any later ordinary snapshot.
    ///
    /// The writer calls `notify` with the result of this write. If another
    /// confirmed snapshot is waiting, this call immediately reports an error
    /// through `notify`.
    pub fn submit_confirmed(
        &self,
        bytes: Vec<u8>,
        notify: impl FnOnce(Result<(), String>) + Send + 'static,
    ) {
        let mut slot = lock(&self.shared);
        if slot.confirmed.is_some() {
            drop(slot);
            notify(Err("Wait for the previous queue save.".to_owned()));
            return;
        }
        slot.submitted += 1;
        slot.pending = None;
        slot.confirmed = Some(ConfirmedWrite {
            bytes,
            generation: slot.submitted,
            notify: Box::new(notify),
        });
        drop(slot);
        self.shared.1.notify_all();
    }

    /// Blocks until everything submitted so far is written (or failed).
    pub fn flush(&self) {
        let mut slot = lock(&self.shared);
        let target = slot.submitted;
        while slot.settled < target {
            slot = self
                .shared
                .1
                .wait(slot)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

impl Drop for Writer {
    /// Writes what is pending, then stops the thread.
    fn drop(&mut self) {
        lock(&self.shared).closed = true;
        self.shared.1.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    use studio_domain::settings::{Crf, OutputKind, SpeedPreset};

    use crate::state::INITIAL_SETTINGS;
    use crate::testing::{Scratch, abs};

    fn job() -> JobSpec {
        JobSpec {
            source: WirePath::from_path(&abs("/m/a.mp4")),
            fingerprint: SourceFingerprint {
                len: 10,
                modified: Some((1, 2)),
            },
            video_stream: 0,
            omissions_acknowledged: true,
            destination: WirePath::from_path(&abs("/m/a.ivf")),
            settings: EncoderSettings {
                output: OutputKind::Av2AnnexB,
                preset: SpeedPreset::new(9).unwrap_or_else(|e| unreachable!("{e}")),
                crf: Crf::new(40).unwrap_or_else(|e| unreachable!("{e}")),
            }
            .into(),
            identity: None,
            purpose: studio_domain::job::JobPurpose::Encode,
            pass_digest: None,
            pass_artifact: None,
        }
    }

    /// A result paired with the source of its job, under a user assumption.
    fn comparison() -> SavedComparison {
        use studio_domain::compare::{
            AlignmentBasis, ComparisonSetup, EncodeProvenance, Reference, StreamInput,
        };
        let job = job();
        let report = JobReport {
            destination: job.destination.clone(),
            pass_digest: None,
            frames: 5,
            bytes: 82,
            checks: Vec::new(),
            disclosures: Vec::new(),
        };
        let mut setup = ComparisonSetup::new(
            Reference::EncodeSource,
            StreamInput {
                path: job.destination.clone(),
                selection: studio_domain::layers::LayerDecodeSelection::Default,
                encode: EncodeProvenance::of(&job, &report, &[]).map(Box::new),
            },
        );
        setup.basis = AlignmentBasis::AssumedOffset { offset: -2 };
        setup.grain = studio_domain::preview::GrainPolicy::Skip;
        SavedComparison {
            setup,
            identity: None,
        }
    }

    fn journal() -> Journal {
        let item = |id, state| JournalItem {
            id,
            source: WirePath::from_path(&abs(&format!("/m/{id}.mp4"))),
            size: Some(1234),
            video_stream: None,
            facts: None,
            destination: WirePath::from_path(&abs(&format!("/m/{id}.ivf"))),
            destination_chosen: false,
            acknowledgement: None,
            submitted: None,
            attempt_policy: Vec::new(),
            state,
        };
        Journal {
            version: VERSION,
            next_id: 9,
            exports: crate::export_jobs::ExportJobs::default(),
            settings: INITIAL_SETTINGS.into(),
            view: JournalView {
                workspace: JournalWorkspace::Analyze,
                shown_document: 1,
                documents: vec![
                    JournalDocument {
                        source: WirePath::from_path(&abs("/m/never read.obu")),
                        origin: JournalOrigin::File,
                        read: None,
                    },
                    JournalDocument {
                        source: WirePath::from_path(&abs("/m/1.ivf")),
                        origin: JournalOrigin::Result {
                            item: 1,
                            source: WirePath::from_path(&abs("/m/1.mp4")),
                            bytes: 82,
                        },
                        read: Some(JournalRead {
                            fingerprint: SourceFingerprint {
                                len: 82,
                                modified: Some((5, 6)),
                            },
                            selected_element: Some(3),
                            filter: RowFilter {
                                kind: studio_domain::analysis::KindFilter::Obu { obu_type: 2 },
                                layer: None,
                            },
                            bytes_shown: false,
                        }),
                    },
                ],
                comparison: Some(comparison()),
            },
            items: vec![
                item(1, JournalState::Waiting),
                item(
                    2,
                    JournalState::Running {
                        job: job(),
                        partial: Some(WirePath::from_path(&abs("/m/.2.ivf.Ab12Cd.partial"))),
                    },
                ),
                item(
                    3,
                    JournalState::Interrupted {
                        job: job(),
                        publishing: true,
                        cause: JournalCause::AppClosed,
                        destination_present: false,
                        partial: JournalPartial::Kept {
                            path: WirePath::Unix(vec![0xff]),
                            reason: "a folder".into(),
                        },
                    },
                ),
                item(
                    4,
                    JournalState::Unreadable {
                        code: Some(FailureCode::UnsupportedSource),
                        message: "HDR".into(),
                    },
                ),
            ],
        }
    }

    #[test]
    fn a_journal_round_trips_through_its_file() {
        let dir = Scratch::new("round-trip");
        let path = dir.path(FILE_NAME);
        assert_eq!(load(&path), Loaded::Missing);
        let bytes = encode(&journal()).unwrap_or_default();
        assert!(write_atomically(&path, &bytes).is_ok());
        assert_eq!(load(&path), Loaded::Found(journal()));
        // The temporary file does not stay behind.
        assert_eq!(dir.entries(), [FILE_NAME]);
        // The format is readable and names the version first.
        let text = String::from_utf8(bytes).unwrap_or_default();
        assert!(
            text.starts_with(&format!("{{\n  \"version\": {VERSION},")),
            "{text}"
        );
        assert!(text.contains("\"kind\": \"running\""), "{text}");
    }

    /// A journal as a version 1 build wrote it: a completed conversion with
    /// its settings, report and acknowledgement, a chosen destination, and
    /// one interrupted while publishing.
    const VERSION_1: &str = r#"{
  "version": 1,
  "next_id": 7,
  "settings": {"output": "ivf", "preset": 6, "crf": 20},
  "items": [
    {
      "id": 5,
      "source": {"kind": "utf8", "value": "/m/harbour.mp4"},
      "size": 4096,
      "video_stream": 1,
      "facts": null,
      "destination": {"kind": "utf8", "value": "/out/harbour.obu"},
      "destination_chosen": true,
      "acknowledgement": {
        "fingerprint": {"len": 4096, "modified": [1795352441, 421141523]},
        "video_stream": 1,
        "omissions": {"other_video": 0, "audio": 2, "subtitles": 1, "attachments": 0, "other": 0, "chapters": 3},
        "settings": {"output": "av2_annex_b", "preset": 9, "crf": 40}
      },
      "state": {
        "kind": "completed",
        "job": {
          "source": {"kind": "utf8", "value": "/m/harbour.mp4"},
          "fingerprint": {"len": 4096, "modified": [1795352441, 421141523]},
          "video_stream": 1,
          "omissions_acknowledged": true,
          "destination": {"kind": "utf8", "value": "/out/harbour.obu"},
          "settings": {"output": "av2_annex_b", "preset": 9, "crf": 40}
        },
        "report": {
          "destination": {"kind": "utf8", "value": "/out/harbour.obu"},
          "frames": 6,
          "bytes": 178,
          "checks": [],
          "disclosures": []
        }
      }
    },
    {
      "id": 6,
      "source": {"kind": "unix", "value": [47, 109, 47, 255]},
      "size": null,
      "video_stream": null,
      "facts": null,
      "destination": {"kind": "utf8", "value": "/out/raw.ivf"},
      "destination_chosen": false,
      "acknowledgement": null,
      "state": {
        "kind": "interrupted",
        "job": {
          "source": {"kind": "unix", "value": [47, 109, 47, 255]},
          "fingerprint": {"len": 10, "modified": null},
          "video_stream": 0,
          "omissions_acknowledged": false,
          "destination": {"kind": "utf8", "value": "/out/raw.ivf"},
          "settings": {"output": "ivf", "preset": 6, "crf": 20}
        },
        "publishing": true,
        "cause": "app_stopped",
        "destination_present": false,
        "partial": {"fate": "removed", "path": {"kind": "utf8", "value": "/out/.raw.ivf.Ab12.partial"}}
      }
    }
  ]
}"#;

    #[test]
    fn legacy_journals_keep_jobs_settings_and_view() -> serde_json::Result<()> {
        use serde_json::{Value, json};

        let dir = Scratch::new("migrate");
        let path = dir.path(FILE_NAME);
        let mut legacy: Value = serde_json::from_str(VERSION_1)?;
        let analyze = JournalView {
            comparison: None,
            ..journal().view
        };
        for (version, view) in [(1, JournalView::default()), (2, analyze)] {
            legacy["version"] = json!(version);
            if version == 2 {
                legacy["view"] = json!(view);
            }
            let original = serde_json::to_vec(&legacy)?;
            assert!(fs::write(&path, &original).is_ok());
            let Loaded::Found(migrated) = load(&path) else {
                unreachable!("{:?}", load(&path))
            };
            assert_eq!(migrated.version, VERSION);
            assert_eq!(migrated.view, view);
            assert_eq!(fs::read(&path).ok(), Some(original));

            let mut expected = legacy.clone();
            expected["version"] = json!(VERSION);
            expected["view"] = json!(view);
            expected["settings"] = json!({"base": legacy["settings"], "values": {}});
            for i in 0..migrated.items.len() {
                let item = &migrated.items[i];
                assert_eq!(item.submitted, None);
                assert_eq!(item.attempt_policy.as_slice(), &[]);
                let job = &mut expected["items"][i]["state"]["job"];
                job["settings"] = json!({"base": job["settings"], "values": {}});
                job["purpose"] = json!("encode");
            }
            let bytes = encode(&migrated)?;
            let mut actual: Value = serde_json::from_slice(&bytes)?;
            for i in 0..migrated.items.len() {
                let job = actual["items"][i]["state"]["job"]
                    .as_object_mut()
                    .unwrap_or_else(|| unreachable!());
                for field in ["identity", "pass_artifact"] {
                    assert!(job.remove(field).is_none_or(|value| value.is_null()));
                }
            }
            assert_eq!(expected, actual);
            assert!(write_atomically(&path, &bytes).is_ok());
            assert_eq!(load(&path), Loaded::Found(migrated));
        }
        Ok(())
    }

    #[test]
    fn legacy_waiting_jobs_refresh_only_incomplete_source_facts() -> serde_json::Result<()> {
        use serde_json::{Value, json};

        let dir = Scratch::new("refresh-legacy");
        let path = dir.path(FILE_NAME);
        let mut legacy: Value = serde_json::from_str(VERSION_1)?;
        let mut waiting = legacy["items"][0].clone();
        waiting["state"] = json!({"kind": "waiting"});
        waiting["video_stream"] = Value::Null;
        waiting["facts"] = json!({
            "reader": "ffmpeg",
            "fingerprint": {"len": 4096, "modified": [1_795_352_441, 421_141_523]},
            "video_stream": 1,
            "video_tracks": [],
            "omissions": {"other_video": 0, "audio": 2, "subtitles": 1, "attachments": 0, "other": 0, "chapters": 3},
            "width": 64,
            "height": 36,
            "frame_rate": {"num": 25, "den": 1},
            "pixel_aspect": null,
            "bit_depth": 8,
            "chroma": "yuv420",
            "frame_count": null,
            "disclosures": []
        });
        let mut with_color = waiting.clone();
        with_color["id"] = json!(7);
        with_color["facts"]["color"] = json!({});
        legacy["items"] = json!([waiting, with_color]);
        for version in [1, 2, 3, 4, 5, VERSION] {
            legacy["version"] = json!(version);
            assert!(fs::write(&path, serde_json::to_vec(&legacy)?).is_ok());
            let Loaded::Found(loaded) = load(&path) else {
                unreachable!("expected a readable journal");
            };
            let first = &loaded.items[0];
            let second = &loaded.items[1];
            if version >= 3 {
                assert_eq!(first.state, JournalState::Waiting);
                assert_eq!(first.video_stream, None);
            } else {
                assert_eq!(first.state, JournalState::Probing);
                assert_eq!(first.video_stream, Some(1));
            }
            assert_eq!(second.state, JournalState::Waiting);
            assert_eq!(second.video_stream, None);
            assert_eq!(first.destination, second.destination);
            assert!(first.destination_chosen);
            assert!(first.acknowledgement.is_some());
            assert_eq!(first.acknowledgement, second.acknowledgement);
            assert_eq!(first.facts, second.facts);
            assert_eq!(loaded.settings, INITIAL_SETTINGS.into());
            let restored = crate::recovery::recover(loaded, &crate::recovery::RealDisk);
            assert_eq!(
                restored.items[0].state(),
                if version >= 3 {
                    ItemState::Waiting
                } else {
                    ItemState::Probing
                }
            );
            assert_eq!(restored.items[1].state(), ItemState::Waiting);
            assert!(restored.items.iter().all(|item| item.attempt().is_none()));
        }
        Ok(())
    }

    #[test]
    fn an_unreadable_view_keeps_the_queue_and_a_copy_of_the_file() {
        let dir = Scratch::new("bad-view");
        let path = dir.path(FILE_NAME);
        let mut text: serde_json::Value =
            serde_json::from_slice(&encode(&journal()).unwrap_or_default()).unwrap_or_default();
        text["view"]["workspace"] = "decode".into();
        let bytes = serde_json::to_vec(&text).unwrap_or_default();
        assert!(fs::write(&path, &bytes).is_ok());
        let loaded = load(&path);
        assert!(loaded.may_save());
        let Loaded::ViewUnreadable {
            journal: kept,
            copy,
            error,
        } = loaded
        else {
            unreachable!("{loaded:?}")
        };
        assert!(error.contains("decode"), "{error}");
        // The queue and settings are all there; the view is the default.
        assert_eq!(
            kept,
            Journal {
                view: JournalView::default(),
                ..journal()
            }
        );
        // The file stays, and a copy of it as it was is beside it.
        assert_eq!(copy, dir.path("queue.json.corrupt-1"));
        assert_eq!(fs::read(&copy).ok(), Some(bytes.clone()));
        assert_eq!(fs::read(&path).ok(), Some(bytes));
        // Items that do not parse still move the whole file aside.
        text["items"] = "renamed".into();
        assert!(fs::write(&path, serde_json::to_vec(&text).unwrap_or_default()).is_ok());
        assert!(matches!(load(&path), Loaded::MovedAside { .. }));
    }

    #[test]
    fn invalid_attempt_policy_moves_the_unmodified_journal_aside() -> serde_json::Result<()> {
        let dir = Scratch::new("bad-policy");
        let path = dir.path(FILE_NAME);
        let mut text = serde_json::to_value(journal())?;
        text["items"][1]["attempt_policy"] = "invalid".into();
        let bytes = serde_json::to_vec(&text)?;
        assert!(fs::write(&path, &bytes).is_ok());
        let Loaded::MovedAside { to, error } = load(&path) else {
            unreachable!("expected the malformed journal to move aside");
        };
        assert!(error.contains("invalid"), "{error}");
        assert_eq!(fs::read(&to).ok(), Some(bytes));
        assert!(!path.exists());
        Ok(())
    }

    #[test]
    fn a_newer_version_is_left_untouched_and_not_saved_over() {
        let dir = Scratch::new("too-new");
        let path = dir.path(FILE_NAME);
        let future = br#"{"version": 7, "items": "renamed", "view": {"tabs": []}}"#;
        assert!(fs::write(&path, future).is_ok());
        let loaded = load(&path);
        assert_eq!(loaded, Loaded::TooNew { version: 7 });
        assert!(!loaded.may_save());
        assert_eq!(fs::read(&path).ok().as_deref(), Some(&future[..]));
    }

    #[test]
    fn an_unparsable_journal_is_moved_aside_with_a_free_number() {
        let dir = Scratch::new("corrupt");
        let path = dir.path(FILE_NAME);
        assert!(fs::write(dir.path("queue.json.corrupt-1"), b"older").is_ok());
        for (n, bad) in [
            &b"{\"version\": 1, \"next_id\": "[..],
            b"not json",
            b"{\"version\": 0}",
            b"{\"version\": 1, \"next_id\": 1, \"settings\": {}, \"items\": []}",
        ]
        .iter()
        .enumerate()
        {
            assert!(fs::write(&path, bad).is_ok());
            let loaded = load(&path);
            let expected = dir.path(&format!("queue.json.corrupt-{}", n + 2));
            assert!(
                matches!(&loaded, Loaded::MovedAside { to, .. } if *to == expected),
                "{loaded:?}"
            );
            assert!(loaded.may_save());
            assert_eq!(fs::read(&expected).ok().as_deref(), Some(*bad));
            assert!(!path.exists());
        }
        assert_eq!(
            fs::read(dir.path("queue.json.corrupt-1")).ok().as_deref(),
            Some(&b"older"[..])
        );
    }

    #[test]
    fn a_failed_write_leaves_the_old_journal_and_says_why() {
        let dir = Scratch::new("write-fails");
        // The journal's folder is a regular file: nothing can be created in it.
        let blocker = dir.path("not-a-folder");
        assert!(fs::write(&blocker, b"").is_ok());
        let path = blocker.join(FILE_NAME);
        assert!(write_atomically(&path, b"{}").is_err());

        let (tx, rx) = mpsc::channel();
        let writer = Writer::spawn(path, move |result| {
            let _ = tx.send(result);
        });
        let Ok(writer) = writer else {
            unreachable!("no writer thread")
        };
        writer.submit(b"{}".to_vec());
        writer.flush();
        let reported = rx.recv_timeout(Duration::from_secs(10));
        assert!(
            matches!(reported, Ok(Err(ref e)) if !e.is_empty()),
            "{reported:?}"
        );
        // The same failure again is not reported again.
        writer.submit(b"{}".to_vec());
        writer.flush();
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn confirmed_writes_report_the_exact_snapshot_or_error() {
        let dir = Scratch::new("confirmed-writer");
        let folder = dir.path("blocked");
        let path = folder.join(FILE_NAME);
        let writer = Writer::spawn(path.clone(), |_| {}).unwrap_or_else(|e| unreachable!("{e}"));
        let (tx, rx) = mpsc::channel();
        let failed = tx.clone();
        writer.submit_confirmed(b"before".to_vec(), move |result| {
            let _ = failed.send(result.map(|()| Vec::new()));
        });
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(10)),
            Ok(Err(_))
        ));
        fs::create_dir(&folder).unwrap_or_else(|e| unreachable!("{e}"));
        let checked_path = path.clone();
        writer.submit_confirmed(b"confirmed".to_vec(), move |result| {
            let _ =
                tx.send(result.and_then(|()| fs::read(checked_path).map_err(|e| e.to_string())));
        });
        writer.submit(b"after".to_vec());
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(10)),
            Ok(Ok(b"confirmed".to_vec()))
        );
        writer.flush();
        assert_eq!(fs::read(&path).ok().as_deref(), Some(&b"after"[..]));
    }

    #[test]
    fn the_writer_writes_the_newest_journal_and_reports_recovery() {
        let dir = Scratch::new("writer");
        let path = dir.path(FILE_NAME);
        let blocked = dir.path("blocked");
        let (tx, rx) = mpsc::channel();
        let writer = Writer::spawn(blocked.join(FILE_NAME), {
            let tx = tx.clone();
            move |result| {
                let _ = tx.send(result);
            }
        });
        let Ok(writer) = writer else {
            unreachable!("no writer thread")
        };
        // The folder does not exist yet: failing, then working once it does.
        writer.submit(b"1".to_vec());
        writer.flush();
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(10)),
            Ok(Err(_))
        ));
        assert!(fs::create_dir(&blocked).is_ok());
        writer.submit(b"2".to_vec());
        writer.flush();
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)), Ok(Ok(())));
        assert_eq!(
            fs::read(blocked.join(FILE_NAME)).ok().as_deref(),
            Some(&b"2"[..])
        );
        drop(writer);

        // Many quick submissions end with the last one on disk.
        let writer = Writer::spawn(path.clone(), move |result| {
            let _ = tx.send(result);
        });
        let Ok(writer) = writer else {
            unreachable!("no writer thread")
        };
        for n in 0..200 {
            writer.submit(n.to_string().into_bytes());
        }
        drop(writer);
        assert_eq!(fs::read(&path).ok().as_deref(), Some(&b"199"[..]));
    }
}
