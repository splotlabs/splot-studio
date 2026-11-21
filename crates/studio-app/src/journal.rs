// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The queue journal: one versioned JSON file, `queue.json`, in the app's
//! data folder, replaced atomically on every state change.
//!
//! The format is the DTOs below, not the app's types. They embed the
//! domain's transport values (`WirePath`, `JobSpec`, `SourceFacts`,
//! `JobReport`, `Failure`, settings) as their serde forms; a change to any of
//! those forms needs a new [`VERSION`] and a migration.
//!
//! Load rules: a missing file is an empty queue; a newer [`VERSION`] (or a
//! file that cannot be read at all) is left untouched and this session does
//! not save; anything else that does not parse is moved aside as
//! `queue.json.corrupt-<n>` and the queue starts empty.
//!
//! Writes go through [`Writer`], a thread that writes the latest submitted
//! journal (older unwritten ones are skipped), so a slow disk never holds up
//! the window.

use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};

use serde::{Deserialize, Serialize};
use studio_domain::job::{
    Failure, FailureCode, JobReport, JobSpec, Omissions, SourceFacts, SourceFingerprint, WirePath,
};
use studio_domain::settings::EncoderSettings;

use crate::queue::{
    Acknowledgement, Cause, Ending, Interruption, Item, ItemId, ItemState, PartialFate,
};

/// The journal format this build reads and writes.
pub const VERSION: u32 = 1;

/// The journal's file name in the data folder.
pub const FILE_NAME: &str = "queue.json";

/// The whole journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Journal {
    /// [`VERSION`] when written by this build.
    pub version: u32,
    /// The next item id; ids are never reused.
    pub next_id: ItemId,
    /// The current settings, which apply to every waiting item.
    pub settings: EncoderSettings,
    /// The items, in list order.
    pub items: Vec<JournalItem>,
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

/// The journal of `items` with `settings`.
#[must_use]
pub fn snapshot(next_id: ItemId, settings: EncoderSettings, items: &[Item]) -> Journal {
    Journal {
        version: VERSION,
        next_id,
        settings,
        items: items.iter().map(item).collect(),
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
            Self::Missing | Self::Found(_) | Self::MovedAside { .. }
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
        Ok(Header { version }) if version < VERSION => format!("unknown version {version}"),
        Ok(_) => match serde_json::from_slice::<Journal>(&bytes) {
            Ok(journal) => return Loaded::Found(journal),
            Err(e) => e.to_string(),
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

/// Renames `path` to the first free `<name>.corrupt-<n>` beside it.
fn move_aside(path: &Path) -> io::Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("the journal path has no file name"))?
        .to_string_lossy()
        .into_owned();
    for n in 1..=1000 {
        let to = path.with_file_name(format!("{name}.corrupt-{n}"));
        // Only this instance touches the folder (it holds the lock), so the
        // name is still free when the rename runs.
        match fs::symlink_metadata(&to) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::rename(path, &to)?;
                return Ok(to);
            }
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
#[derive(Default)]
struct Slot {
    /// The newest journal not yet written.
    pending: Option<Vec<u8>>,
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
                    let (bytes, upto) = {
                        let mut slot = lock(&worker);
                        while slot.pending.is_none() && !slot.closed {
                            slot = worker.1.wait(slot).unwrap_or_else(PoisonError::into_inner);
                        }
                        match slot.pending.take() {
                            Some(bytes) => (bytes, slot.submitted),
                            None => return,
                        }
                    };
                    let result = write_atomically(&path, &bytes).map_err(|e| e.to_string());
                    if result != last {
                        report(result.clone());
                        last = result;
                    }
                    lock(&worker).settled = upto;
                    worker.1.notify_all();
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
    use crate::testing::Scratch;

    fn job() -> JobSpec {
        JobSpec {
            source: WirePath::Utf8("/m/a.mp4".into()),
            fingerprint: SourceFingerprint {
                len: 10,
                modified: Some((1, 2)),
            },
            video_stream: 0,
            omissions_acknowledged: true,
            destination: WirePath::Utf8("/m/a.ivf".into()),
            settings: EncoderSettings {
                output: OutputKind::Av2AnnexB,
                preset: SpeedPreset::new(9).unwrap_or_else(|e| unreachable!("{e}")),
                crf: Crf::new(40).unwrap_or_else(|e| unreachable!("{e}")),
            },
        }
    }

    fn journal() -> Journal {
        let item = |id, state| JournalItem {
            id,
            source: WirePath::Utf8(format!("/m/{id}.mp4")),
            size: Some(1234),
            video_stream: None,
            facts: None,
            destination: WirePath::Utf8(format!("/m/{id}.ivf")),
            destination_chosen: false,
            acknowledgement: None,
            state,
        };
        Journal {
            version: VERSION,
            next_id: 9,
            settings: INITIAL_SETTINGS,
            items: vec![
                item(1, JournalState::Waiting),
                item(
                    2,
                    JournalState::Running {
                        job: job(),
                        partial: Some(WirePath::Utf8("/m/.2.ivf.Ab12Cd.partial".into())),
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
        assert!(text.starts_with("{\n  \"version\": 1,"), "{text}");
        assert!(text.contains("\"kind\": \"running\""), "{text}");
    }

    #[test]
    fn a_newer_version_is_left_untouched_and_not_saved_over() {
        let dir = Scratch::new("too-new");
        let path = dir.path(FILE_NAME);
        let future = br#"{"version": 7, "anything": ["from", "the", "future"]}"#;
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
