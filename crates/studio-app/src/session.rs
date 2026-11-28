// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The app without its window: the [`AppState`], the engine workers it
//! asks for, the [`Preview`] and its engine process, the [`Analysis`] and
//! its inspect engine process, and the journal it is saved to.
//!
//! [`Session::dispatch`] applies an intent and carries out its effects:
//! starting probe and job workers, talking to them, removing a dead
//! worker's temporary file, and submitting the journal whenever the
//! durable state changed (never for progress counters). The journal also
//! records the workspace and the Analyze documents; documents restored from
//! it are read again only once Analyze is shown.
//!
//! The inspect process is started when a document needs it and let go as
//! soon as none does (every document closed, failed or not read), so an
//! idle Analyze holds no process, file handle or kept facts. What only a window
//! can do (dialogs, timers, Show in Folder) comes back as [`UiEffect`]s.
//! Worker events are handed to `post` on their reader threads; whoever owns
//! the session brings them back to its thread and calls [`Session::deliver`].
//! The window uses the Slint event loop for that, the tests a channel.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use studio_domain::analysis::RowFilter;
use studio_domain::job::{JobSpec, WirePath};
use studio_domain::protocol::{BuildIdentity, Command, Envelope, Event, ProtocolError};
use studio_domain::settings::OutputKind;

use crate::activity::{self, Changed};
use crate::analysis::{self, Analysis};
use crate::engine::{self, Delivery, Ending, EngineStatus, Role, Worker};
use crate::jobs::{self, Work};
use crate::journal::{self, JournalView, Writer};
use crate::preview::{self, Dirty, Preview};
use crate::recovery::{self, RealDisk};
use crate::source::ProbeFailure;
use crate::state::{AppState, Effect, Intent, JournalProblem};
use crate::workspace::Workspace;

mod advanced;
mod batch;
mod compare;
mod export;
mod preflight;
mod related;

/// The request id of the startup `Hello`; probes and jobs count from 1.
const HELLO_ID: u64 = 0;

/// Which request a worker serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// The startup handshake.
    Hello,
    /// A probe, by request id.
    Probe(u64),
    /// A job, by request id.
    Job(u64),
    /// The journal save that authorizes a prepared job, by job request ID.
    Journal(u64),
    /// The Advanced request generation.
    Advanced(u64),
    /// A related job preparation request.
    Related(u64),
    /// The preview process, by how many were started before it.
    Preview(u64),
    /// The inspect process, by how many were started before it.
    Inspect(u64),
    /// The batch validation process, by spawn number.
    Batch(u64),
    /// The process that reads a comparison, by spawn number.
    Compare(u64),
    /// The export worker for this job ID.
    Export(u64),
    /// The confirmed journal write for this export job.
    ExportSaved(u64),
}

/// How worker deliveries leave their reader threads.
pub type Post = Arc<dyn Fn(Route, Delivery) + Send + Sync>;

/// What only the window can do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiEffect {
    /// Show the native Open dialog, to add files to convert.
    ShowOpenDialog,
    /// Show the native Open dialog, to open AV2 streams in Analyze.
    ShowAnalyzeDialog,
    /// Show the native Open dialog for one AV2 file of a comparison. Answer
    /// with [`Session::compare_picked`].
    ShowCompareDialog,
    /// Show the native Open dialog for one file: where the shown missing
    /// document is now. Answer with [`Session::locate_picked`].
    ShowLocateDialog,
    /// Show the native Save dialog.
    ShowSaveDialog {
        /// The folder to start in.
        directory: Option<PathBuf>,
        /// The suggested file name.
        file_name: String,
        /// The output kind, for the file type filter.
        output: OutputKind,
    },
    /// Arm the [`activity::CANCEL_TIMEOUT`] timer for job `0`; when it
    /// fires, dispatch [`activity::Input::CancelTimedOut`] for it.
    CancelTimer(u64),
    /// Arm the cancellation deadline for this export job.
    ExportCancelTimer(u64),
    /// Show this file in the file manager.
    Reveal(PathBuf),
    /// The startup handshake answered.
    Engine(EngineStatus),
}

/// What one dispatch changed, for the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// What only the window can do, in order.
    pub ui: Vec<UiEffect>,
    /// What to redraw.
    pub changed: Changed,
    /// What of the picture area to redraw.
    pub preview: Dirty,
    /// The Analyze workspace changed.
    pub analysis: bool,
    /// The Advanced editor needs to redraw.
    pub advanced: bool,
    /// The Compare workspace changed.
    pub compare: bool,
}

impl Outcome {
    pub(crate) const fn nothing() -> Self {
        Self {
            ui: Vec::new(),
            changed: Changed::Nothing,
            preview: Dirty {
                picture: false,
                view: false,
                thumbs: false,
            },
            analysis: false,
            advanced: false,
            compare: false,
        }
    }

    /// Adds what `other` changed.
    fn merge(&mut self, other: Self) {
        self.ui.extend(other.ui);
        self.changed = more(self.changed, other.changed);
        self.preview.picture |= other.preview.picture;
        self.preview.view |= other.preview.view;
        self.preview.thumbs |= other.preview.thumbs;
        self.analysis |= other.analysis;
        self.advanced |= other.advanced;
        self.compare |= other.compare;
    }
}

/// The state, its workers and its journal.
pub struct Session {
    state: AppState,
    engine: Result<PathBuf, String>,
    post: Post,
    hello: Option<Worker>,
    probe: Option<(u64, Worker)>,
    job: Option<(u64, Worker)>,
    preflight: Option<preflight::Preflight>,
    advanced: advanced::Advanced,
    preflight_queue: VecDeque<preflight::Request>,
    prepared: Option<(u64, JobSpec)>,
    encode_save: Option<u64>,
    confirming: Option<(u64, usize)>,
    preview: Preview,
    /// The open inspection read whose playback panel is visible.
    playback_document: Option<u64>,
    /// The preview process, started on first need, with its spawn number.
    previewer: Option<(u64, Worker)>,
    previewers_started: u64,
    analysis: Analysis,
    batch: crate::batch::Batch,
    exports: crate::export_jobs::ExportJobs,
    export: Option<export::Active>,
    export_notice: Option<String>,
    batch_worker: Option<(u64, Worker)>,
    batch_workers_started: u64,
    compare: crate::compare::Compare,
    /// The inspect process that reads a comparison, with its spawn number.
    compare_worker: Option<(u64, Worker)>,
    compare_workers_started: u64,
    /// The Compare Open dialog is showing for this side.
    compare_dialog: Option<studio_domain::compare::Side>,
    /// The inspect process, started on first need, with its spawn number.
    inspector: Option<(u64, Worker)>,
    inspectors_started: u64,
    inspect_data_dir: Option<PathBuf>,
    /// The Analyze Open dialog is showing.
    analyze_dialog: bool,
    /// The Locate dialog is showing for the missing document of this path.
    locating: Option<PathBuf>,
    journal: Option<Writer>,
    /// The journal bytes last submitted.
    saved: Option<Vec<u8>>,
    /// The view record in them.
    saved_view: Option<JournalView>,
    shut_down: bool,
}

impl Session {
    /// A session over `state`, starting workers from the `engine`
    /// executable (or reporting why there is none) and saving to `journal`
    /// when given. Saves once right away, so a recovered queue is recorded.
    #[must_use]
    pub fn new(
        state: AppState,
        engine: Result<PathBuf, String>,
        post: Post,
        journal: Option<Writer>,
    ) -> Self {
        Self::with_inspect_data_dir(state, engine, post, journal, None)
    }

    pub(crate) fn with_inspect_data_dir(
        mut state: AppState,
        engine: Result<PathBuf, String>,
        post: Post,
        journal: Option<Writer>,
        inspect_data_dir: Option<PathBuf>,
    ) -> Self {
        let mut view = state.take_restored_view();
        let comparison = view.comparison.take();
        let exports = state.take_restored_exports();
        let mut session = Self {
            state,
            engine,
            post,
            hello: None,
            probe: None,
            job: None,
            preflight: None,
            advanced: advanced::Advanced::default(),
            preflight_queue: VecDeque::new(),
            prepared: None,
            encode_save: None,
            confirming: None,
            preview: Preview::new(),
            playback_document: None,
            previewer: None,
            previewers_started: 0,
            analysis: Analysis::restored(view),
            batch: crate::batch::Batch::default(),
            exports,
            export: None,
            export_notice: None,
            batch_worker: None,
            batch_workers_started: 0,
            compare: crate::compare::Compare::restored(comparison),
            compare_worker: None,
            compare_workers_started: 0,
            compare_dialog: None,
            inspector: None,
            inspectors_started: 0,
            inspect_data_dir,
            analyze_dialog: false,
            locating: None,
            journal,
            saved: None,
            saved_view: None,
            shut_down: false,
        };
        session.note_storage();
        session.read_if_shown();
        session.persist();
        session
    }

    /// Tells Analyze when its documents are not saved, as Encode's notices
    /// say for the list.
    fn note_storage(&mut self) {
        let problem = if let Some(reason) = self.state.save_error() {
            Some(format!(
                "The documents cannot be saved: {reason}. They are kept until the app closes."
            ))
        } else if matches!(
            self.state.journal_problem(),
            Some(
                JournalProblem::TooNew { .. }
                    | JournalProblem::Unreadable { .. }
                    | JournalProblem::NoFolder(_)
            )
        ) {
            Some("The documents are not saved in this session; Encode says why.".to_owned())
        } else {
            None
        };
        self.analysis.set_storage_problem(problem);
    }

    /// Reads the shown document if it is not read yet, once Analyze is
    /// shown, and a restored comparison once Compare is shown.
    fn read_if_shown(&mut self) {
        if self.state.workspace() == Workspace::Analyze {
            let effects = self.analysis.read_shown(&RealDisk);
            self.send_inspect(effects);
        }
        self.read_compare_if_shown();
    }

    /// Whether an inspect process runs, for tests of its lifecycle.
    #[must_use]
    pub const fn inspecting(&self) -> bool {
        self.inspector.is_some()
    }

    /// The state.
    #[must_use]
    pub const fn state(&self) -> &AppState {
        &self.state
    }

    /// The picture area.
    #[must_use]
    pub const fn preview(&self) -> &Preview {
        &self.preview
    }

    /// The Analyze workspace.
    #[must_use]
    pub const fn analysis(&self) -> &Analysis {
        &self.analysis
    }

    /// The user did something to the picture area or the window. The
    /// picture area is visible when the window is visible.
    pub fn preview_input(&mut self, input: preview::Input) -> Outcome {
        if self.shut_down {
            return Outcome::nothing();
        }
        let effects = self.preview.input(input);
        self.send_preview(effects);
        if matches!(input, preview::Input::Surface(_) | preview::Input::Grain(_)) {
            self.sync_reference_preview();
        }
        self.preview_outcome()
    }

    /// Sends the startup `Hello`.
    pub fn start_hello(&mut self, client: BuildIdentity, now: Instant) -> Outcome {
        let started = self.spawn(Route::Hello).and_then(|mut worker| {
            worker
                .send(HELLO_ID, Command::Hello { client })
                .map_err(|e| format!("could not send the handshake: {e}"))?;
            // End of input tells the worker to exit once it has answered.
            worker.close_input();
            Ok(worker)
        });
        match started {
            Ok(worker) => {
                self.hello = Some(worker);
                Outcome::nothing()
            }
            Err(e) => {
                let status = match &self.engine {
                    Ok(path) if !path.exists() => EngineStatus::NotFound(path.clone()),
                    _ => EngineStatus::Failed(e),
                };
                self.engine_answered(status, now)
            }
        }
    }

    fn engine_answered(&mut self, status: EngineStatus, now: Instant) -> Outcome {
        let answer = match &status {
            EngineStatus::Ready { capabilities, .. } => Ok(capabilities.clone()),
            EngineStatus::NotFound(path) => Err(format!(
                "The encoding engine is missing ({}). Reinstall Splot Studio.",
                path.display()
            )),
            EngineStatus::Incompatible(why) => Err(format!(
                "The encoding engine does not match this app: {why}. Reinstall Splot Studio."
            )),
            EngineStatus::Failed(why) => Err(format!("The encoding engine did not start: {why}.")),
        };
        let mut outcome = self.dispatch(Intent::EngineAnswered(answer), now);
        outcome.ui.insert(0, UiEffect::Engine(status));
        outcome
    }

    /// Applies `intent` and everything it leads to.
    pub fn dispatch(&mut self, intent: Intent, now: Instant) -> Outcome {
        let mut outcome = Outcome::nothing();
        if self.shut_down {
            return outcome;
        }
        let workspace = self.state.workspace();
        let batch_policy = matches!(intent, Intent::QueuedPolicy { .. });
        let mut pending = VecDeque::from([intent]);
        while let Some(intent) = pending.pop_front() {
            // Destinations are looked at before anything can start, but not
            // for every progress counter.
            let counters = matches!(
                intent,
                Intent::Job {
                    input: activity::Input::Event(Event::Progress { .. }),
                    ..
                }
            );
            if !counters && !batch_policy {
                self.state.refresh_destinations(&RealDisk);
            }
            let worker_available =
                self.job.is_none() && self.encode_save.is_none() && self.export.is_none();
            let worker_ended = matches!(
                intent,
                Intent::Job {
                    input: activity::Input::WorkerEnded { .. },
                    ..
                }
            );
            let update = self
                .state
                .apply_with_worker(intent, now, &RealDisk, worker_available);
            if worker_ended && worker_available && update.changed != Changed::Phase {
                pending.push_back(Intent::WorkerIdle);
            }
            outcome.changed = more(outcome.changed, update.changed);
            for effect in update.effects {
                self.perform(effect, &mut pending, &mut outcome.ui);
            }
        }
        if outcome.changed == Changed::Phase {
            self.note_storage();
            outcome.analysis |= self.analysis.take_dirty();
            if !batch_policy {
                self.persist();
            }
        }
        if self.state.workspace() != workspace {
            self.read_if_shown();
            outcome.analysis |= self.analysis.take_dirty();
            outcome.compare |= self.compare.take_dirty();
        }
        if outcome.changed == Changed::Phase || self.state.workspace() != workspace {
            self.sync_preview();
            self.sync_block_overlay();
            outcome.analysis |= self.analysis.take_dirty();
        }
        self.advance_export(now);
        outcome.preview = self.preview.take_dirty();
        outcome
    }

    fn sync_preview(&mut self) {
        let scene = if self.state.workspace() == Workspace::Encode {
            self.playback_document = None;
            preview::scene(&self.state)
        } else if self.state.workspace() == Workspace::Compare {
            // Compare shows no picture.
            self.playback_document = None;
            preview::Scene {
                stage: None,
                thumbs: Vec::new(),
            }
        } else {
            let stage = self
                .analysis
                .shown()
                .filter(|doc| Some(doc.id()) == self.playback_document)
                .and_then(document_subject);
            if stage.is_none() {
                self.playback_document = None;
            }
            preview::Scene {
                stage,
                thumbs: Vec::new(),
            }
        };
        let effects = self.preview.sync(scene);
        self.send_preview(effects);
        self.sync_reference_preview();
    }

    fn sync_reference_preview(&mut self) {
        let source = self
            .analysis
            .shown()
            .filter(|_| {
                self.state.workspace() == Workspace::Analyze
                    && self.analysis.motion_preview_position().is_some()
            })
            .and_then(|doc| {
                let (frame, index) = doc.reference_preview_picture()?;
                let target = document_subject(doc)?.target()?;
                Some((target, frame, index))
            });
        let effects = match source {
            Some((target, frame, index)) => {
                self.preview.request_reference_preview(target, frame, index)
            }
            None => self.preview.clear_reference_preview(),
        };
        self.send_preview(effects);
    }

    fn sync_analysis_preview(&mut self) {
        let navigation = self.analysis.take_timeline_picture();
        if let Some((document, output)) = navigation {
            self.playback_document = output.map(|_| document);
        }
        self.sync_preview();
        if let Some((document, view)) = self.analysis.take_reference_view() {
            self.preview.restore_history_view(document, view);
        }
        if let Some((_, Some(index))) = navigation
            && let Some(output) = index.checked_add(1)
        {
            let effects = self.preview.pause_document();
            self.send_preview(effects);
            let effects = self.preview.input(preview::Input::SeekOutput(output));
            self.send_preview(effects);
        }
    }

    fn sync_block_overlay(&mut self) {
        let picture = if self.state.workspace() == Workspace::Analyze
            && self.analysis.overlay().mode != analysis::block_overlay::Mode::Off
        {
            self.preview.block_picture()
        } else {
            None
        };
        let mut effects = self.analysis.sync_overlay(picture);
        self.analysis.sync_entropy(&mut effects);
        if !effects.is_empty() {
            self.send_inspect(effects);
        }
        self.analysis.discard_other_motion_preview();
        self.sync_reference_preview();
    }

    /// Applies an action for the displayed block overlay.
    pub fn block_overlay_input(&mut self, input: analysis::BlockOverlayInput) -> Outcome {
        if self.shut_down || self.state.workspace() != Workspace::Analyze {
            return Outcome::nothing();
        }
        let picture = self.preview.block_picture();
        let view = picture.and_then(|picture| self.preview.history_view(picture.document));
        let point = if let analysis::BlockOverlayInput::Pick {
            x,
            y,
            width,
            height,
        } = input
        {
            self.preview.block_point(x, y, width, height)
        } else {
            None
        };
        let mut effects = self.analysis.change_overlay(input, picture, point, view);
        self.analysis.sync_entropy(&mut effects);
        self.send_inspect(effects);
        self.preview_outcome()
    }

    /// Selects a byte range linked to the displayed output.
    pub fn preview_show_bytes(&mut self, link: i32) -> Outcome {
        if let Some((document, offset)) = self.preview.output_byte(link) {
            let effects = self.preview.pause_document();
            self.send_preview(effects);
            return self.analysis_input(analysis::Input::PlaybackByte { document, offset });
        }
        Outcome::nothing()
    }

    /// The user did something in the Analyze workspace.
    pub fn analysis_input(&mut self, input: analysis::Input) -> Outcome {
        let mut outcome = Outcome::nothing();
        if self.shut_down {
            return outcome;
        }
        if input == analysis::Input::TogglePlayback {
            self.playback_document = if self.playback_document.is_some() {
                None
            } else {
                self.analysis.shown().map(analysis::Document::id)
            };
            self.sync_preview();
            return self.preview_outcome();
        }
        if input == analysis::Input::Resolve
            && let Some(doc) = self.analysis.shown()
            && *doc.phase() == analysis::Phase::Missing
        {
            // The answer goes to this document, whichever is shown then.
            if self.locating.replace(doc.path().to_path_buf()).is_none() {
                outcome.ui.push(UiEffect::ShowLocateDialog);
            }
            return outcome;
        }
        if matches!(
            input,
            analysis::Input::FollowReference(_)
                | analysis::Input::BlockMotion(analysis::BlockMotionInput::Follow(_))
                | analysis::Input::Back
                | analysis::Input::Forward
        ) {
            let view = self
                .analysis
                .shown()
                .and_then(|document| self.preview.history_view(document.id()));
            self.analysis.save_reference_view(view);
        }
        let effects = self.analysis.input(input);
        self.send_inspect(effects);
        // A document shown now, or relinked, may not be read yet.
        self.read_if_shown();
        self.persist_view();
        self.sync_analysis_preview();
        self.sync_block_overlay();
        outcome.analysis = self.analysis.take_dirty();
        outcome.preview = self.preview.take_dirty();
        outcome
    }

    /// The Locate dialog closed; `None` means it was cancelled.
    pub fn locate_picked(&mut self, path: Option<PathBuf>) -> Outcome {
        match (self.locating.take(), path) {
            (Some(missing), Some(to)) => {
                self.analysis_input(analysis::Input::Relink { missing, to })
            }
            _ => Outcome::nothing(),
        }
    }

    /// Every job of the workspaces; see [`jobs::list`].
    #[must_use]
    pub fn jobs(&self) -> Vec<jobs::Job<'_>> {
        let mut jobs = jobs::list(&self.state, &self.analysis);
        jobs.extend(self.batch.files.iter().map(jobs::validation));
        jobs.extend(self.exports.rows().iter().map(jobs::export));
        jobs.extend(jobs::comparison(&self.compare));
        jobs
    }

    /// Shows the job at `index` of [`Self::jobs`] where it belongs: its
    /// file in Encode, or its document in Analyze.
    pub fn show_job(&mut self, index: usize, now: Instant) -> Outcome {
        let jobs = self.jobs();
        let target = jobs
            .get(index)
            .filter(|job| job.can_show())
            .map(|job| job.work);
        let target = match target {
            Some(Work::Validate(file)) => return self.show_batch(file.id, now),
            Some(Work::Export(job)) => {
                let mut outcome = Outcome::nothing();
                if let Some(path) = job.reveal_path() {
                    outcome.ui.push(UiEffect::Reveal(path));
                }
                return outcome;
            }
            Some(Work::Compare(_)) => {
                return self.dispatch(Intent::WorkspaceRequested(Workspace::Compare), now);
            }
            Some(Work::Convert(item)) => Some((Workspace::Encode, self.index_of_item(item.id()))),
            Some(Work::Inspect(at, _)) => Some((Workspace::Analyze, Some(at))),
            None => None,
        };
        let mut outcome = Outcome::nothing();
        match target {
            Some((Workspace::Analyze, Some(at))) => {
                outcome.merge(self.analysis_input(analysis::Input::Show(int(at))));
                outcome.merge(self.dispatch(Intent::WorkspaceRequested(Workspace::Analyze), now));
            }
            Some((_, row)) => {
                outcome.merge(self.dispatch(Intent::WorkspaceRequested(Workspace::Encode), now));
                if let Some(row) = row {
                    outcome.merge(self.dispatch(Intent::RowSelected(int(row)), now));
                }
            }
            None => {}
        }
        outcome
    }

    /// Cancels the job at `index` of [`Self::jobs`], if it can be.
    pub fn cancel_job(&mut self, index: usize, now: Instant) -> Outcome {
        let target = self
            .jobs()
            .get(index)
            .filter(|job| job.can_cancel())
            .map(|job| job.work);
        match target {
            Some(Work::Convert(_)) => self.dispatch(Intent::CancelRequested, now),
            Some(Work::Inspect(at, _)) => {
                self.analysis_input(analysis::Input::CancelDocument(int(at)))
            }
            Some(Work::Validate(file)) => self.cancel_batch_file(file.id, now),
            Some(Work::Export(job)) => self.cancel_export_file(job.id, now),
            Some(Work::Compare(_)) => self.compare_input(crate::compare::Input::Close, now),
            None => Outcome::nothing(),
        }
    }

    fn index_of_item(&self, id: crate::queue::ItemId) -> Option<usize> {
        self.state.items().iter().position(|i| i.id() == id)
    }

    /// Open to Analyze: shows Analyze and its Open dialog.
    pub fn analyze_open_requested(&mut self, now: Instant) -> Outcome {
        let mut outcome = self.dispatch(Intent::WorkspaceRequested(Workspace::Analyze), now);
        if !self.shut_down && !self.analyze_dialog {
            self.analyze_dialog = true;
            outcome.ui.push(UiEffect::ShowAnalyzeDialog);
        }
        outcome
    }

    /// The Analyze Open dialog closed; empty means it was cancelled.
    pub fn analyze_picked(&mut self, paths: Vec<PathBuf>) -> Outcome {
        self.analyze_dialog = false;
        if paths.is_empty() {
            return Outcome::nothing();
        }
        self.analysis_input(analysis::Input::Open(paths))
    }

    /// Open in Analyze: the shown file's converted result, as it was
    /// verified. Nothing in the queue changes.
    pub fn analyze_result(&mut self, now: Instant) -> Outcome {
        let Some(result) = self.state.analyzable() else {
            return Outcome::nothing();
        };
        let mut outcome = self.analysis_input(analysis::Input::OpenResult(result));
        outcome.merge(self.dispatch(Intent::WorkspaceRequested(Workspace::Analyze), now));
        outcome
    }

    /// Sends inspect commands, starting the inspect process first if none
    /// runs. A process that cannot be written to ends by itself; its end of
    /// stream is what reports it.
    fn send_inspect(&mut self, effects: Vec<analysis::Effect>) {
        for effect in effects {
            if self.inspector.is_none() {
                let spawn = self.inspectors_started;
                self.inspectors_started += 1;
                match self.spawn_as(Role::Inspect, Route::Inspect(spawn)) {
                    Ok(worker) => self.inspector = Some((spawn, worker)),
                    Err(why) => {
                        self.analysis.engine_failed(&why);
                        return;
                    }
                }
            }
            if let Some((_, worker)) = &mut self.inspector
                && worker.send(effect.request_id, effect.command).is_err()
            {
                worker.close_input();
            }
        }
        if self.inspector.is_some() && !self.analysis.needs_engine() {
            // Dropping it ends its input; it exits and is reaped without
            // blocking. Its late answers are for an older spawn.
            self.inspector = None;
            self.analysis.engine_released();
        }
    }

    /// One delivery from inspect process number `spawn`; older processes
    /// are ignored.
    fn deliver_inspect(&mut self, spawn: u64, delivery: Delivery) -> Outcome {
        if self.inspector.as_ref().is_none_or(|(n, _)| *n != spawn) {
            if let Delivery::Pictures(slot) = delivery {
                let _ = slot.take();
            }
            return Outcome::nothing();
        }
        let effects = match delivery {
            Delivery::Advanced(_) | Delivery::JournalSaved(_) | Delivery::Related(_) => {
                return Outcome::nothing();
            }
            Delivery::Event(envelope) => self.analysis.event(envelope.request_id, envelope.body),
            Delivery::Pictures(slot) => {
                // The inspect process sends no pictures.
                let _ = slot.take();
                Vec::new()
            }
            Delivery::Ended(ending) => {
                self.inspector = None;
                self.analysis.process_ended(&ending_text(&ending));
                Vec::new()
            }
        };
        self.send_inspect(effects);
        self.persist_view();
        let analysis = self.analysis.take_dirty();
        if analysis {
            self.sync_analysis_preview();
            self.sync_block_overlay();
        }
        Outcome {
            analysis: self.analysis.take_dirty() || analysis,
            preview: self.preview.take_dirty(),
            ..Outcome::nothing()
        }
    }

    /// Routes one worker delivery.
    pub fn deliver(&mut self, route: Route, delivery: Delivery, now: Instant) -> Outcome {
        let nothing = Outcome::nothing();
        if self.shut_down {
            return nothing;
        }
        match route {
            Route::Hello => {
                // Only the first delivery answers; the stream ending after
                // it is the worker exiting as asked. Dropping it reaps it.
                if self.hello.take().is_some() {
                    self.engine_answered(EngineStatus::from_hello(delivery), now)
                } else {
                    nothing
                }
            }
            Route::Probe(id) => {
                let result = match delivery {
                    Delivery::Advanced(_) | Delivery::JournalSaved(_) | Delivery::Related(_) => {
                        return nothing;
                    }
                    Delivery::Event(e) if e.request_id != id => return nothing,
                    Delivery::Pictures(slot) => {
                        let _ = slot.take();
                        return nothing;
                    }
                    Delivery::Event(e) => match e.body {
                        Event::Probed { facts } => Ok(facts),
                        Event::Failed { failure } => Err(ProbeFailure::from(failure)),
                        Event::Error { message, .. } => Err(ProbeFailure::other(format!(
                            "The engine refused: {message}"
                        ))),
                        _ => return nothing,
                    },
                    Delivery::Ended(ending) => {
                        if self
                            .probe
                            .as_ref()
                            .is_some_and(|(current, _)| *current == id)
                        {
                            self.probe = None;
                        }
                        Err(ProbeFailure::other(format!(
                            "The engine stopped without answering ({}).",
                            ending_text(&ending)
                        )))
                    }
                };
                // A probe that already answered ignores this one.
                self.dispatch(Intent::ProbeAnswered { id, result }, now)
            }
            Route::Job(id) => self.deliver_job(id, delivery, now),
            Route::Journal(id) => {
                let Delivery::JournalSaved(result) = delivery else {
                    return nothing;
                };
                if self.encode_save != Some(id) {
                    return nothing;
                }
                self.encode_save = None;
                if self
                    .prepared
                    .as_ref()
                    .is_none_or(|(current, _)| *current != id)
                {
                    return self.dispatch(Intent::WorkerIdle, now);
                }
                match result {
                    Ok(()) => self.start_prepared(id, now),
                    Err(message) => self.journal_failed(id, &message, now),
                }
            }
            Route::Advanced(generation) => self.deliver_advanced(generation, delivery),
            Route::Related(request) => match delivery {
                Delivery::Related(result) => {
                    self.dispatch(Intent::RelatedPrepared { request, result }, now)
                }
                _ => nothing,
            },
            Route::Preview(spawn) => self.deliver_preview(spawn, delivery),
            Route::Inspect(spawn) => self.deliver_inspect(spawn, delivery),
            Route::Batch(spawn) => self.deliver_batch(spawn, delivery, now),
            Route::Compare(spawn) => self.deliver_compare(spawn, delivery),
            Route::Export(id) => self.deliver_export(id, delivery, now),
            Route::ExportSaved(id) => self.export_saved(id, delivery, now),
        }
    }

    fn deliver_job(&mut self, id: u64, delivery: Delivery, now: Instant) -> Outcome {
        let nothing = Outcome::nothing();
        let reaped = matches!(delivery, Delivery::Ended(_))
            && self.job.as_ref().is_some_and(|(current, _)| *current == id);
        if reaped {
            self.job = None;
        }
        if self
            .preflight
            .as_ref()
            .is_some_and(|preflight| preflight.id == id)
        {
            return self.deliver_preflight(id, delivery, now);
        }
        let input = match delivery {
            Delivery::Advanced(_) | Delivery::JournalSaved(_) | Delivery::Related(_) => {
                return nothing;
            }
            Delivery::Event(e) if e.request_id != id => {
                eprintln!(
                    "{}: ignoring an event for request {} during job {id}",
                    crate::APP_NAME,
                    e.request_id
                );
                return nothing;
            }
            Delivery::Event(Envelope {
                body:
                    Event::JobPolicy {
                        first,
                        total,
                        values,
                    },
                ..
            }) => return self.confirm_policy(id, first, total, &values, now),
            Delivery::Event(Envelope {
                body: Event::PictureUnavailable { message, .. },
                ..
            }) => {
                self.preview.live_unavailable(id, &message);
                return self.preview_outcome();
            }
            Delivery::Event(e) => activity::Input::Event(e.body),
            Delivery::Pictures(slot) => {
                if let Some(picture) = slot.take().filter(|p| p.request_id == id) {
                    self.preview.live(id, picture);
                }
                return self.preview_outcome();
            }
            Delivery::Ended(ending) => {
                if reaped {
                    self.clear_preflight();
                }
                activity::Input::WorkerEnded {
                    detail: ending_text(&ending),
                }
            }
        };
        self.dispatch(Intent::Job { id, input }, now)
    }

    fn preview_outcome(&mut self) -> Outcome {
        if let Some((document, selection, status)) = self.preview.take_layer_decode_feedback() {
            self.analysis
                .update_layer_decode_status(document, selection, status);
        }
        self.sync_block_overlay();
        Outcome {
            analysis: self.analysis.take_dirty(),
            preview: self.preview.take_dirty(),
            ..Outcome::nothing()
        }
    }

    /// One delivery from preview process number `spawn`; older processes
    /// are ignored.
    fn deliver_preview(&mut self, spawn: u64, delivery: Delivery) -> Outcome {
        let current = self.previewer.as_ref().is_some_and(|(n, _)| *n == spawn);
        let effects = match delivery {
            Delivery::Advanced(_) | Delivery::JournalSaved(_) | Delivery::Related(_) => {
                return Outcome::nothing();
            }
            Delivery::Pictures(slot) => match slot.take() {
                Some(picture) if current => self.preview.picture(picture),
                _ => Vec::new(),
            },
            _ if !current => Vec::new(),
            Delivery::Event(envelope) => match envelope.body {
                Event::PictureSample {
                    generation,
                    ordinal,
                    grain,
                    point,
                    value,
                } => {
                    self.preview
                        .sample_answer(generation, ordinal, grain, point, value);
                    Vec::new()
                }
                Event::PictureProgress {
                    generation,
                    stage,
                    completed,
                    total,
                } => {
                    self.preview.progress(generation, stage, completed, total);
                    Vec::new()
                }
                Event::PictureUnavailable {
                    generation,
                    code,
                    message,
                } => self.preview.unavailable(generation, code, &message),
                Event::PlaybackEnded { generation, frame } => {
                    self.preview.playback_ended(generation, frame)
                }
                Event::Error { message, .. } => {
                    self.previewer = None;
                    self.preview.refused(&message)
                }
                // Nothing else is sent to the preview process's requests.
                _ => Vec::new(),
            },
            Delivery::Ended(ending) => {
                self.previewer = None;
                let incompatible = matches!(
                    ending.error,
                    Some(ProtocolError::IncompatibleVersion { .. })
                );
                self.preview
                    .process_ended(&ending_text(&ending), incompatible)
            }
        };
        self.send_preview(effects);
        self.preview_outcome()
    }

    /// Sends preview commands, starting the preview process first if none
    /// runs. A process that cannot be written to ends by itself; its end
    /// of stream is what reports it.
    fn send_preview(&mut self, effects: Vec<preview::Effect>) {
        for effect in effects {
            if self.previewer.is_none()
                && matches!(effect.command, Command::CloseDocumentPlayback { .. })
            {
                continue;
            }
            if self.previewer.is_none() {
                let spawn = self.previewers_started;
                self.previewers_started += 1;
                match self.spawn_as(Role::Preview, Route::Preview(spawn)) {
                    Ok(worker) => self.previewer = Some((spawn, worker)),
                    Err(why) => {
                        // Gives up, so nothing further is returned.
                        let _ = self.preview.spawn_failed(&why);
                        return;
                    }
                }
            }
            if let Some((_, worker)) = &mut self.previewer
                && worker.send(effect.request_id, effect.command).is_err()
            {
                worker.close_input();
            }
        }
    }

    fn perform(&mut self, effect: Effect, pending: &mut VecDeque<Intent>, ui: &mut Vec<UiEffect>) {
        match effect {
            Effect::ShowOpenDialog => ui.push(UiEffect::ShowOpenDialog),
            Effect::ShowSaveDialog {
                directory,
                file_name,
                output,
            } => ui.push(UiEffect::ShowSaveDialog {
                directory,
                file_name,
                output,
            }),
            Effect::Reveal(path) => ui.push(UiEffect::Reveal(path)),
            Effect::Probe { item, request } => {
                // One metadata read for the row; the engine reads the rest.
                let size = std::fs::metadata(&request.path).ok().map(|m| m.len());
                self.state.note_size(item, size);
                let id = request.id;
                // Replacing an older probe drops its worker, which then exits.
                let started = self.spawn(Route::Probe(id)).and_then(|mut worker| {
                    let command = Command::Probe {
                        source: WirePath::from_path(&request.path),
                        video_stream: request.video_stream,
                    };
                    worker
                        .send(id, command)
                        .map_err(|e| format!("could not ask the engine: {e}"))?;
                    worker.close_input();
                    Ok(worker)
                });
                match started {
                    Ok(worker) => self.probe = Some((id, worker)),
                    Err(message) => {
                        self.probe = None;
                        pending.push_back(Intent::ProbeAnswered {
                            id,
                            result: Err(ProbeFailure::other(message)),
                        });
                    }
                }
            }
            Effect::PrepareRelated { request, job } => self.prepare_related(request, *job, pending),
            Effect::Start { id, job } => self.begin_preflight(id, job, pending),
            Effect::Worker { id, effect } => {
                if effect == activity::Effect::SendCancel
                    && self.confirming.is_some_and(|(current, _)| current == id)
                {
                    self.confirming = None;
                }
                if effect == activity::Effect::SendCancel
                    && (self
                        .preflight
                        .as_ref()
                        .is_some_and(|preflight| preflight.id == id)
                        || self
                            .prepared
                            .as_ref()
                            .is_some_and(|(current, _)| *current == id))
                {
                    self.clear_preflight();
                    self.stop_preflight_worker(id);
                    pending.push_back(Intent::Job {
                        id,
                        input: activity::Input::Event(Event::Cancelled),
                    });
                    return;
                }
                let Some((_, worker)) = self.job.as_mut().filter(|(current, _)| *current == id)
                else {
                    return;
                };
                match effect {
                    activity::Effect::None => {}
                    activity::Effect::SendCancel => {
                        // Cancel carries the job's request id: it is about that job.
                        if worker.send(id, Command::Cancel).is_err() {
                            // The worker is already gone; its end of stream decides.
                            worker.close_input();
                        }
                        ui.push(UiEffect::CancelTimer(id));
                    }
                    activity::Effect::CloseInput => worker.close_input(),
                    activity::Effect::Kill => worker.kill(),
                }
            }
            Effect::CleanPartial {
                item,
                partial,
                destination,
            } => {
                let fate = recovery::remove_partial(&partial, &destination);
                pending.push_back(Intent::PartialCleaned { item, fate });
            }
        }
    }

    fn spawn(&self, route: Route) -> Result<Worker, String> {
        self.spawn_as(Role::Worker, route)
    }

    fn spawn_as(&self, role: Role, route: Route) -> Result<Worker, String> {
        let engine = self.engine.as_ref().map_err(Clone::clone)?;
        let post = Arc::clone(&self.post);
        Worker::spawn_with_data_dir(
            engine,
            role,
            self.inspect_data_dir.as_deref(),
            move |delivery| post(route, delivery),
        )
        .map_err(|e| e.to_string())
    }

    /// The view record of the workspace and the Analyze documents.
    fn view(&self) -> JournalView {
        let (saved, shown) = self.analysis.saved();
        JournalView {
            comparison: self.compare.saved(),
            ..journal::view(self.state.workspace(), &saved, shown)
        }
    }

    /// Submits the journal when the documents or the workspace changed.
    /// A selection, filter or byte view alone waits for the next save (any
    /// change of the queue, or quit), so moving through rows costs no
    /// write each.
    fn persist_view(&mut self) {
        let outline = |view: &JournalView| {
            let mut view = view.clone();
            for doc in &mut view.documents {
                if let Some(read) = &mut doc.read {
                    read.selected_element = None;
                    read.filter = RowFilter::default();
                    read.bytes_shown = true;
                }
            }
            view
        };
        let changed = self
            .saved_view
            .as_ref()
            .is_none_or(|saved| outline(saved) != outline(&self.view()));
        if self.journal.is_some() && changed {
            self.persist();
        }
    }

    /// Submits the journal if it differs from the last one submitted.
    fn persist(&mut self) {
        let view = self.view();
        let Some(writer) = &self.journal else {
            return;
        };
        let journal = self.journal_snapshot(view.clone());
        self.saved_view = Some(view);
        let Ok(bytes) = journal::encode(&journal) else {
            return;
        };
        if self.saved.as_ref() != Some(&bytes) {
            writer.submit(bytes.clone());
            self.saved = Some(bytes);
        }
    }

    fn journal_snapshot(&self, view: JournalView) -> journal::Journal {
        let mut snapshot = journal::snapshot(
            self.state.next_id(),
            self.state.option_request().clone(),
            self.state.items(),
            view,
        );
        snapshot.exports = self.exports.clone();
        snapshot
    }

    /// Stops everything: the converting file is journalled as interrupted
    /// and the journal written, then every worker is told to stop (a
    /// running job cancels and removes its temporary file) and killed if it
    /// has not exited within [`engine::EXIT_GRACE`]. Blocks until done.
    pub fn shutdown(&mut self) {
        if self.shut_down {
            return;
        }
        self.close_advanced();
        self.state.interrupt_for_shutdown();
        self.interrupt_exports();
        self.persist();
        if let Some(writer) = &self.journal {
            writer.flush();
        }
        self.shut_down = true;
        let workers = [
            self.job.take().map(|(_, w)| w),
            self.probe.take().map(|(_, w)| w),
            self.hello.take(),
            self.previewer.take().map(|(_, w)| w),
            self.inspector.take().map(|(_, w)| w),
            self.batch_worker.take().map(|(_, w)| w),
            self.compare_worker.take().map(|(_, w)| w),
            self.take_export_worker(),
        ];
        for worker in workers.into_iter().flatten() {
            worker.shutdown(engine::EXIT_GRACE);
        }
        let _ = self.cleanup_batch_partial();
        self.exports.clean_up();
        self.persist();
        if let Some(writer) = &self.journal {
            writer.flush();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shutdown();
    }
}

const fn more(a: Changed, b: Changed) -> Changed {
    match (a, b) {
        (Changed::Phase, _) | (_, Changed::Phase) => Changed::Phase,
        (Changed::Counters, _) | (_, Changed::Counters) => Changed::Counters,
        (Changed::Nothing, Changed::Nothing) => Changed::Nothing,
    }
}

fn document_subject(doc: &analysis::Document) -> Option<preview::Subject> {
    let summary = doc.summary()?;
    Some(preview::Subject::Document {
        document: doc.id(),
        path: WirePath::from_path(doc.path()),
        fingerprint: summary.fingerprint,
        digest: summary.digest.clone()?,
        selection: doc.layer_decode_selection(),
    })
}

fn int(index: usize) -> i32 {
    i32::try_from(index).unwrap_or(i32::MAX)
}

fn ending_text(ending: &Ending) -> String {
    match &ending.error {
        None => ending.exit.clone(),
        Some(e) => format!("{e}; {}", ending.exit),
    }
}
