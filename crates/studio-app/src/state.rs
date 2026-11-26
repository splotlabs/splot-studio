// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Interaction state: the queue of files, the current settings and what the
//! engine said, as typed values.
//!
//! UI callbacks and worker answers become [`Intent`]s; [`AppState::apply`]
//! validates them, moves items only through the [`crate::queue`] table and
//! returns the [`Effect`]s the caller must perform. The window is then
//! re-synchronised from [`AppState`], so a rejected value snaps back. Nothing
//! here starts processes or writes files; the disk is only looked at
//! through [`Disk`] (does a destination exist).
//!
//! Settings are global and apply to every waiting item; an item takes an
//! immutable [`JobSpec`] snapshot when it starts. One item runs at a time.
//! A run converts the waiting items in list order; a failure moves on to the
//! next, Cancel stops the run after the current item (the rest wait for
//! Continue). A video-only output that leaves something out needs a
//! [`Confirmation`], recorded per item as an [`Acknowledgement`] that holds
//! only while the file, stream and settings it was given for are unchanged.

mod files;
mod options;
mod related;
mod run;

pub use related::{RelatedAction, RelatedPreview, RelatedResolution};
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use studio_domain::job::{Failure, FailureCode, JobSpec, Omissions, SourceFacts};
use studio_domain::options::{EffectiveOption, OptionRequest, OptionValue};
use studio_domain::protocol::Capabilities;
use studio_domain::settings::{Crf, EncoderSettings, OutputKind, SpeedPreset};

use crate::activity::{self, Changed};
use crate::analysis::ResultOf;
use crate::queue::{Acknowledgement, Item, ItemId, ItemState, PartialFate};
use crate::recovery::{Disk, Recovered, RestoredView};
use crate::source::{ProbeFailure, ProbeRequest, TrackPicker};
use crate::workspace::Workspace;

use self::options::OptionHistory;

/// `splot_encode::Config::DEFAULT_CRF` at the pinned codec revision.
const CODEC_DEFAULT_CRF: u8 = 20;

/// `splot_encode::Preset::default()` at the pinned codec revision.
const CODEC_DEFAULT_PRESET: u8 = 6;

/// The settings shown before the user changes anything.
///
/// Both values mirror the pinned codec revision: `Config::DEFAULT_CRF` and
/// `Preset::default()`.
// The `unreachable!` arms run in const evaluation: an out-of-range value is a
// compile error, never a runtime panic.
pub const INITIAL_SETTINGS: EncoderSettings = EncoderSettings {
    output: OutputKind::ALL[0],
    preset: match SpeedPreset::new(CODEC_DEFAULT_PRESET) {
        Ok(preset) => preset,
        Err(_) => unreachable!(),
    },
    crf: match Crf::new(CODEC_DEFAULT_CRF) {
        Ok(crf) => crf,
        Err(_) => unreachable!(),
    },
};

/// Something the user asked for, or an answer from a worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// Open the native file dialog to add files to convert (Open…, Add…);
    /// shows Encode.
    AddRequested,
    /// Open the native file dialog to replace the one file (Change…).
    ReplaceRequested,
    /// The file dialog closed; empty means it was cancelled.
    PickFinished(Vec<PathBuf>),
    /// Files named on the command line: added, as if picked.
    FilesOpened(Vec<PathBuf>),
    /// A probe worker answered.
    ProbeAnswered {
        /// The probe, as returned in [`Effect::Probe`].
        id: u64,
        /// The facts, or why the source cannot be read.
        result: Result<SourceFacts, ProbeFailure>,
    },
    /// A row was selected, by index.
    RowSelected(i32),
    /// A row's action (Remove, Cancel, Retry, Show in Folder), by index.
    RowAction(i32),
    /// Remove a row, by index, unless it is converting.
    RemoveRequested(i32),
    /// Remove every converted file from the list.
    ClearFinished,
    /// A video track of the selected file, by index into its picker.
    VideoTrackSelected(i32),
    /// Open the native Save dialog for the selected file's destination.
    ChooseDestinationRequested,
    /// The Save dialog closed; `None` means it was cancelled.
    DestinationPicked(Option<PathBuf>),
    /// An output kind, by index into [`OutputKind::ALL`].
    OutputSelected(i32),
    /// A speed preset as typed or stepped in the widget.
    PresetEdited(i32),
    /// A rate factor as typed or stepped in the widget.
    CrfEdited(i32),
    /// A Quality step, by index into [`Quality::ALL`](crate::simple::Quality::ALL).
    QualitySelected(i32),
    /// A Priority step, by index into [`Priority::ALL`](crate::simple::Priority::ALL).
    PrioritySelected(i32),
    /// Sets one typed override. An absent value removes the override.
    OptionEdited {
        /// The canonical option key.
        key: String,
        /// An explicit option value, or absence to inherit.
        value: Option<OptionValue>,
    },
    /// Replaces the editable option draft with one undoable edit.
    ReplaceOptions(OptionRequest),
    /// Restores the previous option draft.
    UndoOptions,
    /// Restores the next option draft after an undo.
    RedoOptions,
    /// Clear the one file to start over (Convert Another).
    ResetRequested,
    /// Show another workspace. Refused while it has no flow in this build.
    WorkspaceRequested(Workspace),
    /// The startup handshake finished: what the engine can do, or why it
    /// cannot be used.
    EngineAnswered(Result<Capabilities, String>),
    /// Convert, Convert N Files or Continue. When something waiting leaves
    /// streams out, this only asks for a [`Confirmation`].
    ConvertRequested,
    /// The user confirmed the video-only output in the confirmation.
    OmissionsConfirmed,
    /// The user dismissed the confirmation.
    OmissionsDeclined,
    /// Prepare a job action with fresh source and policy checks.
    PrepareRelated {
        /// The source or completed job.
        item: ItemId,
        /// The requested operation.
        action: RelatedAction,
    },
    /// A fresh source and option check answered.
    RelatedPrepared {
        /// The preparation request.
        request: u64,
        /// The fresh source and policy or the compatibility error.
        result: RelatedResolution,
    },
    /// Approve the checked proposal.
    RelatedApproved,
    /// Close the related job proposal.
    RelatedDeclined,
    /// Stop the converting file; the run pauses.
    CancelRequested,
    /// Something happened to a job.
    Job {
        /// The job's request, as returned in [`Effect::Start`].
        id: u64,
        /// What happened.
        input: activity::Input,
    },
    /// Preflight returned the complete effective policy.
    JobPolicy {
        /// The job's request.
        id: u64,
        /// The policy in codec schema order.
        values: Vec<EffectiveOption>,
    },
    /// Preflight finished for a waiting submission.
    QueuedPolicy {
        /// The queue item.
        item: ItemId,
        /// The full policy, or why the worker refused the submission.
        result: Result<Vec<EffectiveOption>, Failure>,
    },
    /// [`Effect::CleanPartial`] was carried out.
    PartialCleaned {
        /// The item.
        item: ItemId,
        /// What became of the file.
        fate: PartialFate,
    },
    /// Saving the journal started failing (`Err`) or works again.
    JournalSaved(Result<(), String>),
}

/// What the caller must do for an [`AppState::apply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Show the native Open dialog (several files may be picked).
    ShowOpenDialog,
    /// Show the native Save dialog, suggesting this file.
    ShowSaveDialog {
        /// The folder to start in.
        directory: Option<PathBuf>,
        /// The suggested file name.
        file_name: String,
        /// The output kind, for the file type filter.
        output: OutputKind,
    },
    /// Probe a source with a new worker; any older probe is obsolete.
    Probe {
        /// The item it is for, so its size can be noted.
        item: ItemId,
        /// The request.
        request: ProbeRequest,
    },
    /// Validate a proposed action before adding an item to the queue.
    PrepareRelated {
        /// The preparation request.
        request: u64,
        /// The job proposed for validation.
        job: Box<JobSpec>,
    },
    /// Start `job` with a new worker.
    Start {
        /// Echoed in [`Intent::Job`].
        id: u64,
        /// The immutable request.
        job: JobSpec,
    },
    /// Act on a job's worker.
    Worker {
        /// The job's request.
        id: u64,
        /// What to do.
        effect: activity::Effect,
    },
    /// The worker of a job is gone and left `partial`: remove it with
    /// [`crate::recovery::remove_partial`], then send
    /// [`Intent::PartialCleaned`].
    CleanPartial {
        /// The item.
        item: ItemId,
        /// The announced temporary file.
        partial: PathBuf,
        /// The output it was for.
        destination: PathBuf,
    },
    /// Show this converted file in the file manager.
    Reveal(PathBuf),
}

/// The result of one [`AppState::apply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    /// What to do, in order.
    pub effects: Vec<Effect>,
    /// What to redraw. [`Changed::Phase`] also means the journal may have
    /// changed.
    pub changed: Changed,
}

/// Why Convert is unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocker {
    /// The reason, for the user.
    pub text: String,
    /// How it should read.
    pub severity: Severity,
}

/// How serious a [`Blocker`] is; the text always says it in words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// A next step, not a problem.
    Info,
    /// Fixable by the user here.
    Warning,
    /// Something is wrong with the engine or the file.
    Problem,
}

/// How the window shows the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// No file: the empty state.
    Empty,
    /// One file: the file card, exactly as before there was a queue.
    Single,
    /// Several files: a list of rows.
    List,
}

/// One file in the question Convert asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asked {
    /// The item.
    pub item: ItemId,
    /// What confirming records for it.
    pub ack: Acknowledgement,
    /// The source file.
    pub source: PathBuf,
    /// What its output leaves out.
    pub omissions: Omissions,
    /// How many of `omissions.other_video` are attached pictures.
    pub attached_pictures: u32,
}

/// The question Convert asks before converting files whose video-only
/// output leaves something out. Confirming records each file's
/// [`Acknowledgement`]; a change to any of them (file, stream, settings,
/// destination taken) discards the whole question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirmation {
    /// The files, in list order.
    pub asked: Vec<Asked>,
    /// The output kind the question names.
    pub output: OutputKind,
}

/// Why the journal is not in use, or what happened to it at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalProblem {
    /// It did not parse and was moved here; the list started empty.
    MovedAside(PathBuf),
    /// Its Analyze documents did not parse and were not restored; the list
    /// was. A copy of it as it was is here.
    ViewUnreadable(PathBuf),
    /// A newer Splot Studio wrote it; it is left untouched and not saved.
    TooNew {
        /// The file.
        path: PathBuf,
        /// The version it states.
        version: u32,
    },
    /// It cannot be read; it is left untouched and not saved.
    Unreadable {
        /// The file.
        path: PathBuf,
        /// Why.
        error: String,
    },
    /// No data folder can be used; nothing is saved.
    NoFolder(String),
}

/// The queue's view of startup: what was restored and what went wrong.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Startup {
    /// The reconciled journal.
    pub recovered: Option<Recovered>,
    /// A problem to tell the user about.
    pub problem: Option<JournalProblem>,
}

/// What the restored-queue notice says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Restored {
    /// Files restored.
    pub files: usize,
    /// Of those, interrupted while converting.
    pub interrupted: usize,
    /// Files the journal named that could not be restored.
    pub dropped: usize,
}

/// Which dialog is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialog {
    /// Open, adding the picked files.
    Add,
    /// Open, replacing the one file.
    Replace,
    /// Save, for this item.
    Save(ItemId),
}

/// A run: converting the waiting files one after another.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Run {
    /// Settings to use for files whose probe completes during this run.
    settings: OptionRequest,
    /// Items this run started or decided not to start, in order.
    attempted: Vec<ItemId>,
    /// Cancel was pressed: end the run when the current item ends.
    stop_after_current: bool,
}

/// The settings the window shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingsView {
    /// The values.
    pub settings: EncoderSettings,
    /// They are a file's snapshot (it is converting or converted), not the
    /// current settings, and cannot be changed.
    pub read_only: bool,
    quality_override: bool,
    priority_override: bool,
}

/// The queue, the current settings and what the engine reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppState {
    items: Vec<Item>,
    next_id: ItemId,
    settings: OptionRequest,
    option_history: OptionHistory,
    /// `None` until the startup handshake answers; `Err` when the engine
    /// cannot be used, with the reason for the user.
    engine: Option<Result<Capabilities, String>>,
    /// The row the user selected; see [`AppState::selected`].
    selected: Option<ItemId>,
    run: Option<Run>,
    /// A run was cancelled, or the queue restored, with files left waiting:
    /// the primary action reads Continue.
    paused: bool,
    confirmation: Option<Confirmation>,
    related: Option<related::Related>,
    /// The probe in flight: its request and item.
    probing: Option<(u64, ItemId)>,
    dialog: Option<Dialog>,
    restored: Option<Restored>,
    journal_problem: Option<JournalProblem>,
    save_error: Option<String>,
    next_request: u64,
    /// The workspace the window shows: view state, journalled with the
    /// Analyze documents.
    workspace: Workspace,
    /// The Analyze documents the journal recorded, until the session takes
    /// them.
    restored_view: RestoredView,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new(Startup::default())
    }
}

impl AppState {
    /// The state at launch: the recovered queue, or an empty one. Nothing
    /// starts by itself; restored waiting files wait for Continue.
    #[must_use]
    pub fn new(startup: Startup) -> Self {
        let mut state = Self {
            items: Vec::new(),
            next_id: 1,
            settings: OptionRequest {
                base: INITIAL_SETTINGS,
                values: BTreeMap::new(),
            },
            option_history: OptionHistory::default(),
            engine: None,
            selected: None,
            run: None,
            paused: false,
            confirmation: None,
            related: None,
            probing: None,
            dialog: None,
            restored: None,
            journal_problem: startup.problem,
            save_error: None,
            next_request: 1,
            workspace: Workspace::Encode,
            restored_view: RestoredView::default(),
        };
        if let Some(recovered) = startup.recovered {
            let interrupted = recovered
                .items
                .iter()
                .filter(|i| i.state == ItemState::Interrupted)
                .count();
            if !recovered.items.is_empty() || recovered.dropped > 0 {
                state.restored = Some(Restored {
                    files: recovered.items.len(),
                    interrupted,
                    dropped: recovered.dropped,
                });
            }
            state.paused = recovered
                .items
                .iter()
                .any(|i| i.state == ItemState::Waiting);
            state.items = recovered.items;
            state.next_id = recovered.next_id;
            state.settings = recovered.settings;
            if recovered.view.workspace.available() {
                state.workspace = recovered.view.workspace;
            }
            state.restored_view = recovered.view;
        }
        state
    }

    /// Hands over the Analyze documents the journal recorded, once.
    pub fn take_restored_view(&mut self) -> RestoredView {
        std::mem::take(&mut self.restored_view)
    }

    /// Applies one intent at time `now`, judging destinations on `disk`.
    /// Out-of-range values, stale answers and edits of a converting file
    /// leave the state unchanged.
    pub fn apply(&mut self, intent: Intent, now: Instant, disk: &dyn Disk) -> Update {
        let mut update = Update {
            effects: Vec::new(),
            changed: Changed::Phase,
        };
        self.apply_inner(intent, now, disk, &mut update);
        if update.changed == Changed::Phase {
            self.settle(now, &mut update);
        }
        update
    }

    fn apply_inner(&mut self, intent: Intent, now: Instant, disk: &dyn Disk, update: &mut Update) {
        // The confirmation is modal: nothing new is opened behind it.
        let modal = self.confirmation.is_some();
        match intent {
            Intent::AddRequested
            | Intent::ReplaceRequested
            | Intent::ChooseDestinationRequested
                if self.dialog.is_some() || modal => {}
            Intent::AddRequested => {
                // Opening to convert shows where the files will be.
                self.workspace = Workspace::Encode;
                self.dialog = Some(Dialog::Add);
                update.effects.push(Effect::ShowOpenDialog);
            }
            Intent::ReplaceRequested => {
                if self.items.len() == 1 && !self.items[0].state.is_active() {
                    self.dialog = Some(Dialog::Replace);
                    update.effects.push(Effect::ShowOpenDialog);
                }
            }
            Intent::PickFinished(paths) => {
                let replace = self.dialog == Some(Dialog::Replace);
                if matches!(self.dialog, Some(Dialog::Add | Dialog::Replace)) {
                    self.dialog = None;
                }
                if !paths.is_empty() {
                    if replace {
                        self.replace(paths, disk);
                    } else {
                        self.add(paths, disk);
                    }
                }
            }
            Intent::FilesOpened(paths) => self.add(paths, disk),
            Intent::ProbeAnswered { id, result } => self.probe_answered(id, result),
            Intent::RowSelected(index) => {
                if let Some(item) = self.at(index) {
                    self.selected = Some(item.id);
                }
            }
            Intent::RowAction(index) => self.row_action(index, now, disk, update),
            Intent::RemoveRequested(index) => {
                if let Some(id) = self.at(index).map(|i| i.id) {
                    self.remove(id);
                }
            }
            Intent::ClearFinished => {
                let done: Vec<ItemId> = self
                    .items
                    .iter()
                    .filter(|i| i.state == ItemState::Completed)
                    .map(|i| i.id)
                    .collect();
                for id in done {
                    self.remove(id);
                }
            }
            Intent::VideoTrackSelected(index) => self.select_track(index, disk),
            Intent::ChooseDestinationRequested => {
                if let Some(effect) = self.save_dialog() {
                    update.effects.push(effect);
                }
            }
            Intent::DestinationPicked(path) => self.destination_picked(path),
            Intent::OutputSelected(_)
            | Intent::PresetEdited(_)
            | Intent::CrfEdited(_)
            | Intent::QualitySelected(_)
            | Intent::PrioritySelected(_)
            | Intent::OptionEdited { .. }
            | Intent::ReplaceOptions(_)
            | Intent::UndoOptions
            | Intent::RedoOptions => self.edit_options(intent, disk),
            Intent::ResetRequested => {
                if self.items.len() == 1 && !self.items[0].state.is_active() {
                    self.remove(self.items[0].id);
                }
            }
            Intent::WorkspaceRequested(workspace) => {
                if workspace.available() {
                    self.workspace = workspace;
                }
            }
            Intent::EngineAnswered(answer) => self.engine = Some(answer),
            Intent::ConvertRequested if self.related.is_none() => self.convert(disk),
            Intent::ConvertRequested => {}
            Intent::PrepareRelated { item, action } => {
                update.effects.extend(self.prepare_related(item, action));
            }
            Intent::RelatedPrepared { request, result } => self.related_prepared(request, result),
            Intent::RelatedApproved => self.approve_related(disk),
            Intent::RelatedDeclined => self.decline_related(),
            Intent::OmissionsConfirmed => self.confirm(),
            Intent::OmissionsDeclined => self.decline(),
            Intent::CancelRequested => self.cancel(now, update),
            Intent::Job { id, input } => self.job_input(id, input, now, disk, update),
            Intent::JobPolicy { id, values } => self.record_policy(id, values),
            Intent::QueuedPolicy { item, result } => self.queued_policy(item, result),
            Intent::PartialCleaned { item, fate } => self.partial_cleaned(item, fate),
            Intent::JournalSaved(result) => self.save_error = result.err(),
        }
    }

    fn destination_picked(&mut self, path: Option<PathBuf>) {
        if let Some(Dialog::Save(id)) = self.dialog {
            self.dialog = None;
            if let Some(path) = path {
                self.choose_destination(id, path);
            }
        }
    }

    fn record_policy(&mut self, id: u64, values: Vec<EffectiveOption>) {
        if let Some(attempt) = self
            .items
            .iter_mut()
            .filter(|item| item.state.is_active())
            .filter_map(|item| item.attempt.as_mut())
            .find(|attempt| attempt.request == id)
        {
            attempt.policy = values;
        }
    }

    /// Brings derived state up to date after a change: acknowledgements
    /// and the confirmation that no longer hold are dropped, the next probe
    /// is sent and the run moves on.
    fn settle(&mut self, now: Instant, update: &mut Update) {
        let settings = self.draft_settings().base;
        for item in &mut self.items {
            let judged = item.facts.is_some() && item.state != ItemState::Probing;
            if !item.state.is_active() && judged && !item.acknowledged_for(settings) {
                item.ack = None;
            }
        }
        if let Some(confirmation) = &self.confirmation
            && !self.still_confirms(confirmation)
        {
            self.confirmation = None;
        }
        if let Some(effect) = self.next_probe() {
            update.effects.push(effect);
        }
        self.advance(now, update);
        if !self.items.iter().any(|i| i.state == ItemState::Waiting) {
            self.paused = false;
        }
        if self.items.is_empty() {
            self.restored = None;
        }
    }

    /// Records whether something exists at each destination now. The
    /// engine refuses to replace a file either way; this only says so
    /// earlier. Converting and converted files are not looked at.
    pub fn refresh_destinations(&mut self, disk: &dyn Disk) {
        for item in &mut self.items {
            if !item.state.is_active() && item.state != ItemState::Completed {
                item.destination.exists = disk.exists(&item.destination.path);
            }
        }
        if let Some(confirmation) = &self.confirmation
            && !self.still_confirms(confirmation)
        {
            self.confirmation = None;
        }
    }

    /// Records a source file's size as read from its metadata.
    pub fn note_size(&mut self, item: ItemId, size: Option<u64>) {
        if let Some(item) = self.item_mut(item) {
            item.size = size;
        }
    }

    /// The app is closing: the converting file, if any, is recorded as
    /// interrupted (with its temporary file still to be dealt with) before
    /// its worker is stopped, so even a kill during shutdown is recovered.
    pub fn interrupt_for_shutdown(&mut self) {
        for item in &mut self.items {
            if item.state.is_active() {
                let publishing = item.state == ItemState::Publishing;
                if item.transition(crate::queue::Transition::Interrupt).is_ok()
                    && let Some(attempt) = &mut item.attempt
                {
                    attempt.ending = Some(crate::queue::Ending::Interrupted(
                        crate::queue::Interruption {
                            publishing,
                            cause: crate::queue::Cause::AppClosed,
                            destination_present: false,
                            partial: PartialFate::pending(attempt.partial.as_deref()),
                        },
                    ));
                }
            }
        }
        self.run = None;
    }

    // Views.

    /// How the window shows the queue.
    #[must_use]
    pub const fn mode(&self) -> Mode {
        match self.items.len() {
            0 => Mode::Empty,
            1 => Mode::Single,
            _ => Mode::List,
        }
    }

    /// Every item, in list order.
    #[must_use]
    pub fn items(&self) -> &[Item] {
        &self.items
    }

    /// The next item id, for the journal.
    #[must_use]
    pub const fn next_id(&self) -> ItemId {
        self.next_id
    }

    /// The item whose details Advanced shows: the one the user selected,
    /// otherwise the first that can still be edited, the converting one, or
    /// the first.
    #[must_use]
    pub fn selected(&self) -> Option<&Item> {
        self.selected
            .and_then(|id| self.item(id))
            .or_else(|| {
                self.items
                    .iter()
                    .find(|i| matches!(i.state, ItemState::Waiting | ItemState::Probing))
            })
            .or_else(|| self.active())
            .or_else(|| self.items.first())
    }

    /// The index of [`Self::selected`] in [`Self::items`].
    #[must_use]
    pub fn selected_index(&self) -> Option<usize> {
        let id = self.selected()?.id;
        self.items.iter().position(|i| i.id == id)
    }

    /// The index of the row the user selected, if they selected one.
    #[must_use]
    pub fn chosen_index(&self) -> Option<usize> {
        let id = self.selected?;
        self.items.iter().position(|i| i.id == id)
    }

    /// The workspace the window shows.
    #[must_use]
    pub const fn workspace(&self) -> Workspace {
        self.workspace
    }

    /// The shown file's converted result, as Analyze takes it over: the
    /// selected file, once it converted and its result was verified.
    #[must_use]
    pub fn analyzable(&self) -> Option<ResultOf> {
        let item = self
            .selected()
            .filter(|i| i.state == ItemState::Completed)?;
        if item.attempt()?.job.purpose != studio_domain::job::JobPurpose::Encode {
            return None;
        }
        match item.ending() {
            Some(crate::queue::Ending::Completed(report)) => Some(ResultOf {
                item: item.id,
                source: item.source().to_path_buf(),
                output: item.destination().path.clone(),
                bytes: report.bytes,
            }),
            _ => None,
        }
    }

    /// The converting item, if any.
    #[must_use]
    pub fn active(&self) -> Option<&Item> {
        self.items.iter().find(|i| i.state.is_active())
    }

    /// The item shown in the activity area: the converting one, or in
    /// [`Mode::Single`] the one file.
    #[must_use]
    pub fn focus(&self) -> Option<&Item> {
        self.active().or_else(|| match self.mode() {
            Mode::Single => self.items.first(),
            Mode::Empty | Mode::List => None,
        })
    }

    /// A run is under way (a file converting, or about to).
    #[must_use]
    pub const fn running(&self) -> bool {
        self.run.is_some()
    }

    /// The primary action reads Continue.
    #[must_use]
    pub const fn paused(&self) -> bool {
        self.paused
    }

    /// `(position, total)` of the converting file in the run: "2 of 5".
    #[must_use]
    pub fn run_position(&self) -> Option<(usize, usize)> {
        let run = self.run.as_ref()?;
        self.active()?;
        let started = run
            .attempted
            .iter()
            .filter(|id| self.item(**id).is_some_and(|i| i.attempt.is_some()))
            .count();
        let ahead = self
            .items
            .iter()
            .filter(|i| !run.attempted.contains(&i.id))
            .filter(|i| i.state == ItemState::Probing || self.startable(i))
            .count();
        Some((started, started + ahead))
    }

    /// Whether the run converts `item` after the converting file: it has
    /// not reached it, is not stopping, and could start it now.
    #[must_use]
    pub fn queued(&self, item: &Item) -> bool {
        self.run
            .as_ref()
            .is_some_and(|run| !run.stop_after_current && !run.attempted.contains(&item.id))
            && self.startable(item)
    }

    /// How many files Convert would start now.
    #[must_use]
    pub fn startable_count(&self) -> usize {
        self.items.iter().filter(|i| self.startable(i)).count()
    }

    /// The confirmation Convert is waiting for, if any.
    #[must_use]
    pub const fn confirmation(&self) -> Option<&Confirmation> {
        self.confirmation.as_ref()
    }

    /// What the engine can do; `None` until it answered or when it cannot be
    /// used.
    #[must_use]
    pub fn capabilities(&self) -> Option<&Capabilities> {
        self.engine.as_ref().and_then(|e| e.as_ref().ok())
    }

    /// The restored-queue notice, until a run starts or the list empties.
    #[must_use]
    pub const fn restored(&self) -> Option<&Restored> {
        self.restored.as_ref()
    }

    /// What happened to the journal at startup.
    #[must_use]
    pub const fn journal_problem(&self) -> Option<&JournalProblem> {
        self.journal_problem.as_ref()
    }

    /// Why the journal cannot be saved right now.
    #[must_use]
    pub fn save_error(&self) -> Option<&str> {
        self.save_error.as_deref()
    }

    /// The Simple controls for new submissions.
    #[must_use]
    pub const fn settings(&self) -> EncoderSettings {
        self.settings.base
    }

    /// The typed settings for a new submission.
    #[must_use]
    pub const fn option_request(&self) -> &OptionRequest {
        &self.settings
    }

    fn draft_settings(&self) -> &OptionRequest {
        self.run
            .as_ref()
            .map_or(&self.settings, |run| &run.settings)
    }

    /// The index of `kind` in [`OutputKind::ALL`].
    #[must_use]
    pub fn output_index(kind: OutputKind) -> i32 {
        OutputKind::ALL
            .iter()
            .position(|&k| k == kind)
            .and_then(|i| i32::try_from(i).ok())
            .unwrap_or(0)
    }

    /// The Video track choice for the selected file, when it has more than
    /// one track a user may choose.
    #[must_use]
    pub fn track_picker(&self) -> Option<TrackPicker> {
        let item = self.selected()?;
        TrackPicker::of(
            item.facts.as_ref().map_or(&[], |f| &f.video_tracks),
            item.video_stream(),
        )
    }

    /// The selected file's destination can be changed now.
    #[must_use]
    pub fn destination_editable(&self) -> bool {
        self.selected().is_some_and(Self::editable)
    }

    /// A file whose source, track and destination may still change.
    const fn editable(item: &Item) -> bool {
        item.submitted.is_none()
            && !item.state.is_active()
            && !matches!(item.state, ItemState::Completed)
    }

    /// Why Convert is unavailable and how serious that is. Engine problems
    /// come first: no file choice can fix them.
    #[must_use]
    pub fn blocker(&self) -> Option<Blocker> {
        let blocker = |text: &str, severity| {
            Some(Blocker {
                text: text.to_owned(),
                severity,
            })
        };
        if self.running() && self.mode() == Mode::Single {
            return blocker("Converting. Closing the window stops it.", Severity::Info);
        }
        match &self.engine {
            Some(Err(problem)) => return blocker(problem, Severity::Problem),
            Some(Ok(c)) if c.encode_outputs.is_empty() => {
                return blocker("This engine build cannot convert.", Severity::Problem);
            }
            _ => {}
        }
        let supported = self
            .capabilities()
            .is_none_or(|c| c.encode_outputs.contains(&self.settings.base.output));
        if !supported {
            return blocker(
                &format!(
                    "This engine cannot write {}.",
                    self.settings.base.output.label()
                ),
                Severity::Problem,
            );
        }
        match self.mode() {
            Mode::Empty => blocker("Open a video to convert it.", Severity::Info),
            Mode::Single => self.single_blocker(&self.items[0]),
            Mode::List => self.list_blocker(),
        }
    }

    fn single_blocker(&self, item: &Item) -> Option<Blocker> {
        let blocker = |text: &str, severity| {
            Some(Blocker {
                text: text.to_owned(),
                severity,
            })
        };
        match item.state {
            ItemState::Probing => blocker("Reading the source…", Severity::Info),
            ItemState::Unreadable
                if item
                    .probe_failure
                    .as_ref()
                    .is_some_and(|f| f.code == Some(FailureCode::MediaToolMissing)) =>
            {
                blocker("This build converts Y4M files only.", Severity::Problem)
            }
            ItemState::Unreadable => blocker("This file cannot be converted.", Severity::Problem),
            ItemState::Waiting if item.source_changed() => blocker(
                "The source changed after submission. Add the source again.",
                Severity::Warning,
            ),
            ItemState::Waiting if item.destination.exists => {
                blocker("The output file already exists.", Severity::Warning)
            }
            _ if self.engine.is_none() => blocker("Checking the engine…", Severity::Info),
            ItemState::Completed => blocker("Converted.", Severity::Info),
            _ => None,
        }
    }

    fn list_blocker(&self) -> Option<Blocker> {
        let blocker = |text: &str, severity| {
            Some(Blocker {
                text: text.to_owned(),
                severity,
            })
        };
        if self.running() || self.startable_count() > 0 {
            return if self.engine.is_none() {
                blocker("Checking the engine…", Severity::Info)
            } else {
                None
            };
        }
        if self.items.iter().any(|i| i.state == ItemState::Probing) {
            return blocker("Reading the files…", Severity::Info);
        }
        if self
            .items
            .iter()
            .any(|i| i.state == ItemState::Waiting && i.source_changed())
        {
            return blocker(
                "A source changed after submission. Add it again.",
                Severity::Warning,
            );
        }
        let taken = self
            .items
            .iter()
            .filter(|i| i.state == ItemState::Waiting && i.destination.exists)
            .count();
        match taken {
            0 => blocker("Nothing is waiting to convert.", Severity::Info),
            1 => blocker(
                "The output file of 1 waiting file already exists.",
                Severity::Warning,
            ),
            n => blocker(
                &format!("The output files of {n} waiting files already exist."),
                Severity::Warning,
            ),
        }
    }

    // Lookups.

    fn item(&self, id: ItemId) -> Option<&Item> {
        self.items.iter().find(|i| i.id == id)
    }

    fn item_mut(&mut self, id: ItemId) -> Option<&mut Item> {
        self.items.iter_mut().find(|i| i.id == id)
    }

    fn at(&self, index: i32) -> Option<&Item> {
        usize::try_from(index).ok().and_then(|i| self.items.get(i))
    }

    const fn take_request(&mut self) -> u64 {
        let id = self.next_request;
        self.next_request += 1;
        id
    }
}
