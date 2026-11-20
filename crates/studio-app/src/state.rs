// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Interaction state: what the user has chosen and what the engine said, as
//! typed values.
//!
//! UI callbacks and worker answers become [`Intent`]s; [`AppState::apply`]
//! validates them against the domain types and returns the [`Effect`] the
//! caller must perform. The window is then re-synchronised from [`AppState`],
//! so a rejected value snaps back instead of lingering in a widget. Nothing
//! here touches files, processes or the window.
//!
//! A job is only ever built from a successful probe of the current source:
//! its fingerprint and video stream come from those facts. When the output
//! leaves something out, Convert first asks for a [`Confirmation`]; only that
//! confirmation sets [`JobSpec::omissions_acknowledged`], for that one job.

use std::path::{Path, PathBuf};
use std::time::Instant;

use studio_domain::job::{
    Failure, FailureCode, JobSpec, Omissions, SourceFacts, SourceReader, VideoTrack, WirePath,
};
use studio_domain::protocol::Capabilities;
use studio_domain::settings::{Crf, EncoderSettings, OutputKind, SpeedPreset};

use crate::activity::{self, Activity, Phase};
use crate::simple::{Priority, Quality};

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
    /// Open the native file dialog.
    OpenRequested,
    /// The file dialog closed, or a path came from the command line; `None`
    /// means the dialog was cancelled.
    PickFinished(Option<PathBuf>),
    /// A probe worker answered.
    ProbeAnswered {
        /// The probe, as returned in [`Effect::Probe`].
        id: u64,
        /// The facts, or why the source cannot be read.
        result: Result<SourceFacts, ProbeFailure>,
    },
    /// A video track, by index into [`AppState::track_picker`]'s tracks.
    VideoTrackSelected(i32),
    /// Open the native Save dialog for the destination.
    ChooseDestinationRequested,
    /// The Save dialog closed; `None` means it was cancelled.
    DestinationPicked(Option<PathBuf>),
    /// An output kind, by index into [`OutputKind::ALL`].
    OutputSelected(i32),
    /// A speed preset as typed or stepped in the widget.
    PresetEdited(i32),
    /// A rate factor as typed or stepped in the widget.
    CrfEdited(i32),
    /// A Quality step, by index into [`Quality::ALL`].
    QualitySelected(i32),
    /// A Priority step, by index into [`Priority::ALL`].
    PrioritySelected(i32),
    /// Clear the source and the finished job to start over.
    ResetRequested,
    /// The startup handshake finished: what the engine can do, or why it
    /// cannot be used.
    EngineAnswered(Result<Capabilities, String>),
    /// Start encoding with the current choices. When the output leaves
    /// something out, this only asks for a [`Confirmation`].
    EncodeRequested,
    /// The user confirmed the video-only output in the confirmation.
    OmissionsConfirmed,
    /// The user dismissed the confirmation.
    OmissionsDeclined,
    /// Something happened to a job.
    Job {
        /// The job, as returned in [`Effect::Start`].
        id: u64,
        /// What happened.
        input: activity::Input,
    },
}

/// A probe to run with a new worker; any older probe is obsolete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeRequest {
    /// Echoed in [`Intent::ProbeAnswered`].
    pub id: u64,
    /// The source.
    pub path: PathBuf,
    /// The video stream to describe; `None` lets the engine pick.
    pub video_stream: Option<u32>,
}

/// What [`AppState::apply`] asks the caller to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Only re-synchronise the window.
    None,
    /// Show the native Open dialog.
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
    /// Probe a source.
    Probe(ProbeRequest),
    /// Start `job` with a new worker.
    Start {
        /// Echoed in [`Intent::Job`].
        id: u64,
        /// The immutable request.
        job: JobSpec,
    },
    /// Act on a job's worker and redraw what changed.
    Job {
        /// The job.
        id: u64,
        /// The step it took.
        step: activity::Step,
        /// The engine said the source changed since it was probed: probe it
        /// again, once.
        reprobe: Option<ProbeRequest>,
    },
}

/// Why a probe produced no facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeFailure {
    /// The engine's reason; `None` when the engine could not be asked or
    /// broke off.
    pub code: Option<FailureCode>,
    /// The explanation, safe to show.
    pub message: String,
}

impl ProbeFailure {
    /// A failure without an engine reason, such as a worker that did not start.
    #[must_use]
    pub fn other(message: impl Into<String>) -> Self {
        Self {
            code: None,
            message: message.into(),
        }
    }
}

impl From<Failure> for ProbeFailure {
    fn from(failure: Failure) -> Self {
        Self {
            code: Some(failure.code),
            message: failure.message,
        }
    }
}

/// What is known about the chosen source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Nothing chosen yet.
    None,
    /// The engine is reading its header.
    Probing {
        /// The file.
        path: PathBuf,
        /// The probe in flight.
        id: u64,
        /// The video stream asked for; `None` lets the engine pick.
        video_stream: Option<u32>,
        /// The facts of an earlier probe of this file while it is read again
        /// (another video track, or the file changed). Display only.
        previous: Option<Box<SourceFacts>>,
    },
    /// The engine read its header.
    Ready {
        /// The file.
        path: PathBuf,
        /// What the header says.
        facts: SourceFacts,
    },
    /// The engine could not read it, or could not be asked.
    Unreadable {
        /// The file.
        path: PathBuf,
        /// Why.
        failure: ProbeFailure,
        /// The video stream that was asked for.
        video_stream: Option<u32>,
        /// The facts of an earlier probe of this file, so another video
        /// track can still be chosen. Display only.
        previous: Option<Box<SourceFacts>>,
    },
}

impl Source {
    /// The chosen file, in any state.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::None => None,
            Self::Probing { path, .. }
            | Self::Ready { path, .. }
            | Self::Unreadable { path, .. } => Some(path),
        }
    }

    /// The facts to show: the current ones, or the previous ones while the
    /// same file is read again. Never used to build a job.
    #[must_use]
    pub fn shown_facts(&self) -> Option<&SourceFacts> {
        match self {
            Self::Ready { facts, .. } => Some(facts),
            Self::Probing { previous, .. } => previous.as_deref(),
            Self::None | Self::Unreadable { .. } => None,
        }
    }

    /// Every video track known for this file, attached pictures included.
    #[must_use]
    pub fn video_tracks(&self) -> &[VideoTrack] {
        match self {
            Self::Ready { facts, .. } => &facts.video_tracks,
            Self::Probing { previous, .. } | Self::Unreadable { previous, .. } => {
                previous.as_deref().map_or(&[], |f| &f.video_tracks)
            }
            Self::None => &[],
        }
    }

    /// The video stream described, or asked for while reading.
    #[must_use]
    pub fn video_stream(&self) -> Option<u32> {
        match self {
            Self::Ready { facts, .. } => Some(facts.video_stream),
            Self::Probing {
                video_stream,
                previous,
                ..
            }
            | Self::Unreadable {
                video_stream,
                previous,
                ..
            } => video_stream.or_else(|| previous.as_deref().map(|f| f.video_stream)),
            Self::None => None,
        }
    }
}

/// The video tracks a user may choose: every track that is not an attached
/// picture.
#[must_use]
pub fn selectable_tracks(tracks: &[VideoTrack]) -> Vec<&VideoTrack> {
    tracks.iter().filter(|t| !t.attached_picture).collect()
}

/// The attached pictures in `facts` other than the selected stream; they are
/// part of [`Omissions::other_video`].
#[must_use]
pub fn attached_pictures(facts: &SourceFacts) -> u32 {
    let count = facts
        .video_tracks
        .iter()
        .filter(|t| t.attached_picture && t.stream != facts.video_stream)
        .count();
    u32::try_from(count)
        .unwrap_or(u32::MAX)
        .min(facts.omissions.other_video)
}

/// The Video track choice, shown only when a file has more than one track a
/// user may choose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackPicker {
    /// The choosable tracks, in stream order.
    pub tracks: Vec<VideoTrack>,
    /// The index of the selected or requested track in `tracks`.
    pub selected: Option<usize>,
    /// Attached pictures, which are never encoded.
    pub attached: Vec<VideoTrack>,
}

/// The question Convert asks before a job that leaves something out.
///
/// It holds the exact job it would start. Confirming starts that job with
/// [`JobSpec::omissions_acknowledged`] set, and only when the job built from
/// the current choices is still identical: same file and fingerprint, video
/// stream, destination and settings (output kind included). Any change
/// discards it. It is never kept for a later job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirmation {
    /// The job that confirming starts, still unacknowledged.
    pub job: JobSpec,
    /// What the output leaves out, as the confirmation repeats it.
    pub omissions: Omissions,
    /// How many of `omissions.other_video` are attached pictures.
    pub attached_pictures: u32,
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

/// Where the output goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    /// The file to create.
    pub path: PathBuf,
    /// The user picked it in the Save dialog; otherwise it is the default
    /// next to the source and follows the output kind's extension.
    pub chosen: bool,
    /// Something existed at `path` when last checked.
    pub exists: bool,
}

/// The user's choices and what the engine reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppState {
    source: Source,
    destination: Option<Destination>,
    settings: EncoderSettings,
    /// `None` until the startup handshake answers; `Err` when the engine
    /// cannot be used, with the reason for the user.
    engine: Option<Result<Capabilities, String>>,
    /// The current or last job and its id. A finished one is dismissed as
    /// soon as the user changes what a new job would be made from.
    job: Option<(u64, Activity)>,
    /// The open confirmation, if Convert is waiting for one.
    confirmation: Option<Confirmation>,
    /// The source file's size, when it could be read.
    source_size: Option<u64>,
    /// The Open dialog is showing; further Open requests are ignored.
    picking: bool,
    /// The Save dialog is showing.
    saving: bool,
    next_id: u64,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            source: Source::None,
            destination: None,
            settings: INITIAL_SETTINGS,
            engine: None,
            job: None,
            confirmation: None,
            source_size: None,
            picking: false,
            saving: false,
            next_id: 1,
        }
    }
}

impl AppState {
    /// Applies one intent at time `now`. Out-of-range values, stale answers
    /// and changes while a job runs leave the state unchanged.
    pub fn apply(&mut self, intent: Intent, now: Instant) -> Effect {
        let inputs = self.job_inputs();
        let effect = self.apply_inner(intent, now);
        if self.job.as_ref().is_some_and(|(_, a)| a.is_terminal()) && self.job_inputs() != inputs {
            self.job = None;
        }
        if let Some(confirmation) = &self.confirmation
            && !self.still_confirms(confirmation)
        {
            self.confirmation = None;
        }
        effect
    }

    /// What a new job would be made from.
    fn job_inputs(&self) -> (Option<PathBuf>, Option<PathBuf>, EncoderSettings) {
        (
            self.source.path().map(Path::to_path_buf),
            self.destination.as_ref().map(|d| d.path.clone()),
            self.settings,
        )
    }

    /// Whether `confirmation` still describes the job Convert would start.
    fn still_confirms(&self, confirmation: &Confirmation) -> bool {
        let omissions = match &self.source {
            Source::Ready { facts, .. } => facts.omissions,
            _ => return false,
        };
        omissions == confirmation.omissions && self.next_job().as_ref() == Some(&confirmation.job)
    }

    fn apply_inner(&mut self, intent: Intent, now: Instant) -> Effect {
        let locked = self.job_running();
        // The confirmation is modal: nothing new is opened behind it.
        let modal = locked || self.confirmation.is_some();
        match intent {
            Intent::OpenRequested if self.picking || modal => {}
            Intent::OpenRequested => {
                self.picking = true;
                return Effect::ShowOpenDialog;
            }
            Intent::PickFinished(path) => {
                self.picking = false;
                // Cancelling keeps the previous choice.
                if let Some(path) = path.filter(|_| !locked) {
                    return self.choose_source(path);
                }
            }
            Intent::ProbeAnswered { id, result } => {
                if let Source::Probing {
                    path,
                    id: current,
                    video_stream,
                    previous,
                } = &mut self.source
                    && *current == id
                {
                    let path = std::mem::take(path);
                    self.source = match result {
                        Ok(facts) => Source::Ready { path, facts },
                        Err(failure) => Source::Unreadable {
                            path,
                            failure,
                            video_stream: *video_stream,
                            previous: previous.take(),
                        },
                    };
                }
            }
            Intent::VideoTrackSelected(_) | Intent::ResetRequested if locked => {}
            Intent::VideoTrackSelected(index) => return self.select_track(index),
            Intent::ChooseDestinationRequested if self.saving || modal => {}
            Intent::ChooseDestinationRequested => {
                if let Some(destination) = &self.destination {
                    self.saving = true;
                    let path = &destination.path;
                    return Effect::ShowSaveDialog {
                        directory: path.parent().map(Path::to_path_buf),
                        file_name: path
                            .file_name()
                            .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
                        output: self.settings.output,
                    };
                }
            }
            Intent::DestinationPicked(path) => {
                self.saving = false;
                if let Some(path) = path.filter(|_| !locked) {
                    self.destination = Some(Destination {
                        path,
                        chosen: true,
                        exists: false,
                    });
                }
            }
            Intent::OutputSelected(_)
            | Intent::PresetEdited(_)
            | Intent::CrfEdited(_)
            | Intent::QualitySelected(_)
            | Intent::PrioritySelected(_)
                if !locked =>
            {
                self.edit_settings(&intent);
            }
            Intent::OutputSelected(_)
            | Intent::PresetEdited(_)
            | Intent::CrfEdited(_)
            | Intent::QualitySelected(_)
            | Intent::PrioritySelected(_) => {}
            Intent::ResetRequested => {
                self.source = Source::None;
                self.destination = None;
                self.source_size = None;
                self.job = None;
            }
            Intent::EngineAnswered(answer) => self.engine = Some(answer),
            Intent::EncodeRequested => return self.start(now),
            Intent::OmissionsConfirmed => return self.confirm(now),
            Intent::OmissionsDeclined => self.confirmation = None,
            Intent::Job { id, input } => return self.job_input(id, input, now),
        }
        Effect::None
    }

    /// Re-probes for the chosen track, unless it is the current one.
    fn select_track(&mut self, index: i32) -> Effect {
        let stream = step(&selectable_tracks(self.source.video_tracks()), index)
            .map(|t| t.stream)
            .filter(|&s| Some(s) != self.source.video_stream());
        stream
            .and_then(|s| self.reprobe(Some(s)))
            .map_or(Effect::None, Effect::Probe)
    }

    /// Hands `input` to job `id`, if it is the current one.
    fn job_input(&mut self, id: u64, input: activity::Input, now: Instant) -> Effect {
        let Some((current, activity)) = &mut self.job else {
            return Effect::None;
        };
        if *current != id {
            return Effect::None;
        }
        let was_terminal = activity.is_terminal();
        let step = activity.apply(input, now);
        let changed = !was_terminal
            && matches!(activity.phase(), Phase::Failed(f) if f.code == FailureCode::SourceChanged);
        // The engine refused a file that changed since it was probed. Read
        // it again, once per refusal; what it contains now needs a new
        // Convert (and a new confirmation).
        let reprobe = if changed {
            let stream = self.source.video_stream();
            self.reprobe(stream)
        } else {
            None
        };
        Effect::Job { id, step, reprobe }
    }

    /// Applies a settings edit; out-of-range values change nothing.
    fn edit_settings(&mut self, intent: &Intent) {
        match *intent {
            Intent::OutputSelected(index) => {
                if let Some(output) = step(&OutputKind::ALL, index) {
                    self.settings.output = output;
                    self.follow_output_kind();
                }
            }
            Intent::PresetEdited(value) => {
                if let Some(preset) = to_u8(value).and_then(|v| SpeedPreset::new(v).ok()) {
                    self.settings.preset = preset;
                }
            }
            Intent::CrfEdited(value) => {
                if let Some(crf) = to_u8(value).and_then(|v| Crf::new(v).ok()) {
                    self.settings.crf = crf;
                }
            }
            Intent::QualitySelected(index) => {
                if let Some(quality) = step(&Quality::ALL, index) {
                    self.settings.crf = quality.crf();
                }
            }
            Intent::PrioritySelected(index) => {
                if let Some(priority) = step(&Priority::ALL, index) {
                    self.settings.preset = priority.preset();
                }
            }
            _ => {}
        }
    }

    fn choose_source(&mut self, path: PathBuf) -> Effect {
        let id = self.take_id();
        self.source_size = None;
        self.destination = Some(Destination {
            path: default_destination(&path, self.settings.output),
            chosen: false,
            exists: false,
        });
        self.source = Source::Probing {
            path: path.clone(),
            id,
            video_stream: None,
            previous: None,
        };
        Effect::Probe(ProbeRequest {
            id,
            path,
            video_stream: None,
        })
    }

    /// Reads the current file again for `video_stream`, keeping the
    /// destination. The last facts stay visible until the answer arrives.
    fn reprobe(&mut self, video_stream: Option<u32>) -> Option<ProbeRequest> {
        let path = self.source.path()?.to_path_buf();
        let previous = match std::mem::replace(&mut self.source, Source::None) {
            Source::Ready { facts, .. } => Some(Box::new(facts)),
            Source::Probing { previous, .. } | Source::Unreadable { previous, .. } => previous,
            Source::None => None,
        };
        let id = self.take_id();
        self.source = Source::Probing {
            path: path.clone(),
            id,
            video_stream,
            previous,
        };
        Some(ProbeRequest {
            id,
            path,
            video_stream,
        })
    }

    /// A default destination keeps following the output kind's extension.
    fn follow_output_kind(&mut self) {
        if let (Some(destination), Some(source)) = (&mut self.destination, self.source.path())
            && !destination.chosen
        {
            destination.path = default_destination(source, self.settings.output);
            destination.exists = false;
        }
    }

    /// The job Convert would start now, unacknowledged; `None` while
    /// anything blocks it. Its fingerprint and stream come from the current
    /// probe.
    fn next_job(&self) -> Option<JobSpec> {
        if self.blocker().is_some() {
            return None;
        }
        let (Source::Ready { path, facts }, Some(destination)) = (&self.source, &self.destination)
        else {
            return None;
        };
        Some(JobSpec {
            source: WirePath::from_path(path),
            fingerprint: facts.fingerprint,
            video_stream: facts.video_stream,
            omissions_acknowledged: false,
            destination: WirePath::from_path(&destination.path),
            settings: self.settings,
        })
    }

    fn start(&mut self, now: Instant) -> Effect {
        let (Some(job), Source::Ready { facts, .. }) = (self.next_job(), &self.source) else {
            return Effect::None;
        };
        if facts.omissions.any() {
            self.confirmation = Some(Confirmation {
                job,
                omissions: facts.omissions,
                attached_pictures: attached_pictures(facts),
            });
            return Effect::None;
        }
        self.launch(job, now)
    }

    fn confirm(&mut self, now: Instant) -> Effect {
        match self.confirmation.take() {
            Some(confirmation) if self.still_confirms(&confirmation) => self.launch(
                JobSpec {
                    omissions_acknowledged: true,
                    ..confirmation.job
                },
                now,
            ),
            _ => Effect::None,
        }
    }

    fn launch(&mut self, job: JobSpec, now: Instant) -> Effect {
        let (Source::Ready { facts, .. }, Some(destination)) = (&self.source, &self.destination)
        else {
            return Effect::None;
        };
        let activity = Activity::new(
            destination.path.clone(),
            facts.frame_count,
            facts.reader == SourceReader::Ffmpeg,
            now,
        );
        let id = self.take_id();
        self.job = Some((id, activity));
        Effect::Start { id, job }
    }

    const fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Records whether something exists at the destination now. The engine
    /// refuses to replace a file either way; this only says so earlier.
    pub fn note_destination_exists(&mut self, exists: bool) {
        if let Some(destination) = &mut self.destination {
            destination.exists = exists;
        }
        // A destination that is now taken blocks the confirmed job.
        if let Some(confirmation) = &self.confirmation
            && !self.still_confirms(confirmation)
        {
            self.confirmation = None;
        }
    }

    /// Records the source file's size as read from its metadata.
    pub const fn note_source_size(&mut self, size: Option<u64>) {
        self.source_size = size;
    }

    /// The source file's size, when it could be read.
    #[must_use]
    pub const fn source_size(&self) -> Option<u64> {
        self.source_size
    }

    /// Why Encode is unavailable right now, or `None` when it can start.
    #[must_use]
    pub fn encode_blocker(&self) -> Option<String> {
        self.blocker().map(|b| b.text)
    }

    /// Why Encode is unavailable and how serious that is. Engine problems
    /// come first: no source choice can fix them.
    #[must_use]
    pub fn blocker(&self) -> Option<Blocker> {
        let blocker = |text: &str, severity| {
            Some(Blocker {
                text: text.to_owned(),
                severity,
            })
        };
        if self.job_running() {
            return blocker("Converting. Closing the window stops it.", Severity::Info);
        }
        match &self.engine {
            Some(Err(problem)) => return blocker(problem, Severity::Problem),
            Some(Ok(c)) if c.encode_outputs.is_empty() => {
                return blocker("This engine build cannot convert.", Severity::Problem);
            }
            _ => {}
        }
        let outputs = self
            .engine
            .as_ref()
            .map(|e| e.as_ref().map(|c| &c.encode_outputs));
        match (&self.source, &self.destination, outputs) {
            (Source::None, ..) => blocker("Open a video to convert it.", Severity::Info),
            (Source::Probing { .. }, ..) => blocker("Reading the source…", Severity::Info),
            (Source::Unreadable { failure, .. }, ..)
                if failure.code == Some(FailureCode::MediaToolMissing) =>
            {
                blocker("This build converts Y4M files only.", Severity::Problem)
            }
            (Source::Unreadable { .. }, ..) => {
                blocker("This file cannot be converted.", Severity::Problem)
            }
            (_, None, _) => blocker("Choose an output file.", Severity::Info),
            (_, Some(d), _) if d.exists => {
                blocker("The output file already exists.", Severity::Warning)
            }
            (_, _, None) => blocker("Checking the engine…", Severity::Info),
            (_, _, Some(Ok(outputs))) if !outputs.contains(&self.settings.output) => blocker(
                &format!("This engine cannot write {}.", self.settings.output.label()),
                Severity::Problem,
            ),
            (Source::Ready { .. }, Some(_), Some(_)) => None,
        }
    }

    /// A job is running: the source, destination and settings are locked.
    #[must_use]
    pub fn job_running(&self) -> bool {
        self.job.as_ref().is_some_and(|(_, a)| !a.is_terminal())
    }

    /// The current or last job.
    #[must_use]
    pub fn job(&self) -> Option<(u64, &Activity)> {
        self.job.as_ref().map(|(id, a)| (*id, a))
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

    /// The Video track choice, when the file has more than one track a user
    /// may choose.
    #[must_use]
    pub fn track_picker(&self) -> Option<TrackPicker> {
        let all = self.source.video_tracks();
        let tracks: Vec<VideoTrack> = selectable_tracks(all).into_iter().cloned().collect();
        if tracks.len() < 2 {
            return None;
        }
        let stream = self.source.video_stream();
        Some(TrackPicker {
            selected: tracks.iter().position(|t| Some(t.stream) == stream),
            attached: all.iter().filter(|t| t.attached_picture).cloned().collect(),
            tracks,
        })
    }

    /// What is known about the chosen source.
    #[must_use]
    pub const fn source(&self) -> &Source {
        &self.source
    }

    /// Where the output goes.
    #[must_use]
    pub const fn destination(&self) -> Option<&Destination> {
        self.destination.as_ref()
    }

    /// The requested encoder settings.
    #[must_use]
    pub const fn settings(&self) -> EncoderSettings {
        self.settings
    }

    /// The index of the current output kind in [`OutputKind::ALL`].
    #[must_use]
    pub fn output_index(&self) -> i32 {
        OutputKind::ALL
            .iter()
            .position(|&kind| kind == self.settings.output)
            .and_then(|i| i32::try_from(i).ok())
            .unwrap_or(0)
    }
}

/// `<source stem>.<ext>` next to the source, never the source itself.
#[must_use]
pub fn default_destination(source: &Path, output: OutputKind) -> PathBuf {
    let path = source.with_extension(output.extension());
    if path != source {
        return path;
    }
    let stem = source
        .file_stem()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    source.with_file_name(format!("{stem}-av2.{}", output.extension()))
}

fn step<T: Copy>(all: &[T], index: i32) -> Option<T> {
    usize::try_from(index)
        .ok()
        .and_then(|i| all.get(i).copied())
}

fn to_u8(value: i32) -> Option<u8> {
    u8::try_from(value).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::{Effect as JobEffect, Input};
    use studio_domain::job::{ChromaLayout, Rational, SourceFingerprint};
    use studio_domain::protocol::{Event, MediaTool, MediaToolOrigin};

    const PRINT: SourceFingerprint = SourceFingerprint {
        len: 1000,
        modified: Some((1_700_000_000, 5)),
    };

    fn track(stream: u32, attached_picture: bool) -> VideoTrack {
        VideoTrack {
            stream,
            codec: if attached_picture { "mjpeg" } else { "h264" }.into(),
            width: 640,
            height: 360,
            attached_picture,
        }
    }

    fn facts(frame_count: Option<u64>) -> SourceFacts {
        SourceFacts {
            reader: SourceReader::Y4m,
            fingerprint: PRINT,
            video_stream: 0,
            video_tracks: vec![track(0, false)],
            omissions: Omissions::default(),
            width: 640,
            height: 360,
            frame_rate: Rational {
                num: 30000,
                den: 1001,
            },
            pixel_aspect: None,
            bit_depth: 10,
            chroma: ChromaLayout::Yuv420,
            frame_count,
            disclosures: Vec::new(),
        }
    }

    /// An MP4 with two movie tracks, cover art, audio, subtitles and
    /// chapters, described for `stream`.
    fn rich(stream: u32) -> SourceFacts {
        SourceFacts {
            reader: SourceReader::Ffmpeg,
            video_stream: stream,
            video_tracks: vec![track(0, false), track(1, false), track(4, true)],
            omissions: Omissions {
                other_video: 2,
                audio: 2,
                subtitles: 3,
                chapters: 4,
                ..Omissions::default()
            },
            frame_count: None,
            ..facts(None)
        }
    }

    fn capabilities(media_tool: Option<MediaTool>) -> Capabilities {
        Capabilities {
            encode_outputs: OutputKind::ALL.to_vec(),
            media_tool,
        }
    }

    fn system_ffmpeg() -> MediaTool {
        MediaTool {
            version: "8.0.1".into(),
            origin: MediaToolOrigin::System,
        }
    }

    fn probe_id(effect: &Effect) -> u64 {
        match effect {
            Effect::Probe(request) => request.id,
            other => unreachable!("expected a probe, got {other:?}"),
        }
    }

    /// An app whose engine encodes both kinds and whose `path` probed as
    /// `facts`.
    fn ready_with(path: &str, facts: SourceFacts) -> (AppState, Instant) {
        let now = Instant::now();
        let mut state = AppState::default();
        state.apply(
            Intent::EngineAnswered(Ok(capabilities(Some(system_ffmpeg())))),
            now,
        );
        let id = probe_id(&state.apply(Intent::PickFinished(Some(path.into())), now));
        state.apply(
            Intent::ProbeAnswered {
                id,
                result: Ok(facts),
            },
            now,
        );
        (state, now)
    }

    fn ready() -> (AppState, Instant) {
        ready_with("/media/clip.y4m", facts(Some(90)))
    }

    fn start(state: &mut AppState, now: Instant) -> (u64, JobSpec) {
        match state.apply(Intent::EncodeRequested, now) {
            Effect::Start { id, job } => (id, job),
            other => unreachable!("expected Start, got {other:?}"),
        }
    }

    /// Presses Convert on a source with omissions and confirms.
    fn confirm(state: &mut AppState, now: Instant) -> (u64, JobSpec) {
        assert_eq!(state.apply(Intent::EncodeRequested, now), Effect::None);
        assert!(state.confirmation().is_some(), "Convert must ask first");
        match state.apply(Intent::OmissionsConfirmed, now) {
            Effect::Start { id, job } => (id, job),
            other => unreachable!("expected Start, got {other:?}"),
        }
    }

    fn end_job(state: &mut AppState, id: u64, now: Instant, event: Event) -> Effect {
        state.apply(
            Intent::Job {
                id,
                input: Input::Event(event),
            },
            now,
        )
    }

    #[test]
    fn open_is_not_reentrant_while_a_dialog_shows() {
        let now = Instant::now();
        let mut state = AppState::default();
        assert_eq!(
            state.apply(Intent::OpenRequested, now),
            Effect::ShowOpenDialog
        );
        assert_eq!(state.apply(Intent::OpenRequested, now), Effect::None);
        state.apply(Intent::PickFinished(None), now);
        assert_eq!(
            state.apply(Intent::OpenRequested, now),
            Effect::ShowOpenDialog
        );
    }

    #[test]
    fn picking_probes_and_sets_a_default_destination() {
        let now = Instant::now();
        let mut state = AppState::default();
        let effect = state.apply(
            Intent::PickFinished(Some("/media/clip one.mkv".into())),
            now,
        );
        assert_eq!(
            effect,
            Effect::Probe(ProbeRequest {
                id: 1,
                path: "/media/clip one.mkv".into(),
                video_stream: None,
            })
        );
        assert_eq!(
            state.destination().map(|d| (d.path.as_path(), d.chosen)),
            Some((Path::new("/media/clip one.ivf"), false))
        );
        assert_eq!(
            state.encode_blocker().as_deref(),
            Some("Reading the source…")
        );
        // Cancelling a later dialog keeps the choice.
        state.apply(Intent::OpenRequested, now);
        state.apply(Intent::PickFinished(None), now);
        assert_eq!(
            state.source().path(),
            Some(Path::new("/media/clip one.mkv"))
        );
    }

    #[test]
    fn a_stale_probe_answer_is_ignored() {
        let now = Instant::now();
        let mut state = AppState::default();
        state.apply(Intent::PickFinished(Some("/a.y4m".into())), now);
        state.apply(Intent::PickFinished(Some("/b.y4m".into())), now);
        state.apply(
            Intent::ProbeAnswered {
                id: 1,
                result: Ok(facts(None)),
            },
            now,
        );
        assert!(matches!(state.source(), Source::Probing { id: 2, .. }));
        state.apply(
            Intent::ProbeAnswered {
                id: 2,
                result: Err(ProbeFailure {
                    code: Some(FailureCode::InvalidSource),
                    message: "not a video".into(),
                }),
            },
            now,
        );
        assert!(
            matches!(state.source(), Source::Unreadable { failure, .. } if failure.message == "not a video")
        );
        assert_eq!(
            state.encode_blocker().as_deref(),
            Some("This file cannot be converted.")
        );
    }

    #[test]
    fn the_default_destination_follows_the_output_kind_but_a_chosen_one_stays() {
        let (mut state, now) = ready();
        state.apply(Intent::OutputSelected(1), now);
        assert_eq!(
            state.destination().map(|d| d.path.clone()),
            Some("/media/clip.obu".into())
        );
        let effect = state.apply(Intent::ChooseDestinationRequested, now);
        assert_eq!(
            effect,
            Effect::ShowSaveDialog {
                directory: Some("/media".into()),
                file_name: "clip.obu".into(),
                output: OutputKind::Av2AnnexB
            }
        );
        state.apply(Intent::DestinationPicked(Some("/out/mine.obu".into())), now);
        state.apply(Intent::OutputSelected(0), now);
        assert_eq!(
            state.destination().map(|d| (d.path.clone(), d.chosen)),
            Some(("/out/mine.obu".into(), true))
        );
        // A new source resets to a default next to it.
        state.apply(Intent::PickFinished(Some("/media/two.y4m".into())), now);
        assert_eq!(
            state.destination().map(|d| (d.path.clone(), d.chosen)),
            Some(("/media/two.ivf".into(), false))
        );
    }

    #[test]
    fn the_default_destination_is_never_the_source() {
        assert_eq!(
            default_destination(Path::new("/m/clip.ivf"), OutputKind::Ivf),
            PathBuf::from("/m/clip-av2.ivf")
        );
        assert_eq!(
            default_destination(Path::new("/m/clip"), OutputKind::Av2AnnexB),
            PathBuf::from("/m/clip.obu")
        );
    }

    #[test]
    fn encode_needs_a_probe_a_free_destination_and_engine_support() {
        let now = Instant::now();
        let mut state = AppState::default();
        assert_eq!(
            state.encode_blocker().as_deref(),
            Some("Open a video to convert it.")
        );
        assert_eq!(state.apply(Intent::EncodeRequested, now), Effect::None);

        let (mut state, now) = ready();
        assert_eq!(state.encode_blocker(), None);
        state.note_destination_exists(true);
        assert_eq!(
            state.encode_blocker().as_deref(),
            Some("The output file already exists.")
        );
        assert_eq!(state.apply(Intent::EncodeRequested, now), Effect::None);
        state.note_destination_exists(false);

        let ivf_only = Capabilities {
            encode_outputs: vec![OutputKind::Ivf],
            media_tool: None,
        };
        state.apply(Intent::EngineAnswered(Ok(ivf_only)), now);
        state.apply(Intent::OutputSelected(1), now);
        assert_eq!(
            state.encode_blocker(),
            Some(format!(
                "This engine cannot write {}.",
                OutputKind::Av2AnnexB.label()
            ))
        );
        state.apply(Intent::EngineAnswered(Ok(Capabilities::default())), now);
        state.apply(Intent::OutputSelected(0), now);
        assert_eq!(state.blocker().map(|b| b.severity), Some(Severity::Problem));
    }

    #[test]
    fn an_engine_problem_shows_before_anything_else() {
        let now = Instant::now();
        let mut state = AppState::default();
        state.apply(
            Intent::EngineAnswered(Err("The engine is missing.".into())),
            now,
        );
        assert_eq!(
            state.blocker(),
            Some(Blocker {
                text: "The engine is missing.".into(),
                severity: Severity::Problem
            })
        );
        assert_eq!(state.capabilities(), None);
        let (mut state, now) = ready();
        state.apply(Intent::EngineAnswered(Err("Incompatible.".into())), now);
        assert_eq!(state.encode_blocker().as_deref(), Some("Incompatible."));
        assert_eq!(state.apply(Intent::EncodeRequested, now), Effect::None);
    }

    #[test]
    fn an_unanswered_engine_blocks_encode() {
        let now = Instant::now();
        let mut state = AppState::default();
        let id = probe_id(&state.apply(Intent::PickFinished(Some("/m/a.y4m".into())), now));
        state.apply(
            Intent::ProbeAnswered {
                id,
                result: Ok(facts(None)),
            },
            now,
        );
        assert_eq!(
            state.encode_blocker().as_deref(),
            Some("Checking the engine…")
        );
    }

    #[test]
    fn encode_builds_the_job_from_the_probe_and_the_current_choices_and_locks_them() {
        let (mut state, now) = ready();
        state.apply(Intent::PresetEdited(9), now);
        state.apply(Intent::CrfEdited(33), now);
        let (id, job) = start(&mut state, now);
        assert_eq!(job.source, WirePath::Utf8("/media/clip.y4m".into()));
        assert_eq!(job.destination, WirePath::Utf8("/media/clip.ivf".into()));
        assert_eq!(job.fingerprint, PRINT);
        assert_eq!(job.video_stream, 0);
        // Nothing to leave out: one click, nothing acknowledged.
        assert!(!job.omissions_acknowledged);
        assert_eq!(job.settings.preset.get(), 9);
        assert_eq!(job.settings.crf.get(), 33);
        assert!(state.job_running());
        assert_eq!(state.job().map(|(_, a)| a.total_frames()), Some(Some(90)));

        // Everything the job was built from is locked while it runs.
        let before = state.clone();
        for intent in [
            Intent::OpenRequested,
            Intent::PickFinished(Some("/other.y4m".into())),
            Intent::VideoTrackSelected(1),
            Intent::ChooseDestinationRequested,
            Intent::DestinationPicked(Some("/other.ivf".into())),
            Intent::OutputSelected(1),
            Intent::PresetEdited(1),
            Intent::CrfEdited(1),
            Intent::EncodeRequested,
            Intent::OmissionsConfirmed,
        ] {
            assert_eq!(state.apply(intent.clone(), now), Effect::None, "{intent:?}");
            assert_eq!(state, before, "{intent:?} must not change a running job");
        }

        // The job's own events reach it; another job id's do not.
        let cancelled = || Input::Event(Event::Cancelled);
        assert_eq!(
            state.apply(
                Intent::Job {
                    id: id + 100,
                    input: cancelled()
                },
                now
            ),
            Effect::None
        );
        let effect = state.apply(
            Intent::Job {
                id,
                input: cancelled(),
            },
            now,
        );
        assert!(
            matches!(effect, Effect::Job { step, reprobe: None, .. } if step.effect == JobEffect::CloseInput)
        );
        assert!(!state.job_running());
        assert_eq!(state.job().map(|(_, a)| a.phase()), Some(&Phase::Cancelled));
        // Retry is pressing Encode again.
        assert!(matches!(
            state.apply(Intent::EncodeRequested, now),
            Effect::Start { .. }
        ));
    }

    #[test]
    fn simple_steps_and_raw_values_are_one_setting() {
        let now = Instant::now();
        let mut state = AppState::default();
        state.apply(Intent::QualitySelected(0), now);
        state.apply(Intent::PrioritySelected(2), now);
        assert_eq!(state.settings().crf, Quality::Compact.crf());
        assert_eq!(state.settings().preset, Priority::SmallestFile.preset());
        // A raw edit that matches no step is Custom, not a stale step.
        state.apply(Intent::CrfEdited(37), now);
        assert_eq!(Quality::of(state.settings().crf), None);
        state.apply(Intent::CrfEdited(28), now);
        assert_eq!(Quality::of(state.settings().crf), Some(Quality::Good));
        for bad in [-1, 4, i32::MAX] {
            state.apply(Intent::QualitySelected(bad), now);
            state.apply(Intent::PrioritySelected(bad), now);
        }
        assert_eq!(state.settings().crf.get(), 28);
        assert_eq!(state.settings().preset, Priority::SmallestFile.preset());
    }

    #[test]
    fn a_finished_job_is_dismissed_when_its_inputs_change() {
        let (mut state, now) = ready();
        let (id, _) = start(&mut state, now);
        end_job(&mut state, id, now, Event::Cancelled);
        // Answers and checks do not dismiss it.
        state.note_destination_exists(false);
        state.apply(Intent::EngineAnswered(Ok(capabilities(None))), now);
        assert!(state.job().is_some());
        // Changing what a new job would be made from does.
        state.apply(Intent::QualitySelected(0), now);
        assert!(state.job().is_none());
    }

    #[test]
    fn convert_another_starts_over_but_keeps_the_settings() {
        let (mut state, now) = ready();
        state.apply(Intent::QualitySelected(3), now);
        let (id, _) = start(&mut state, now);
        assert_eq!(state.apply(Intent::ResetRequested, now), Effect::None);
        assert!(state.job_running(), "a running job is not reset");
        state.apply(
            Intent::Job {
                id,
                input: activity::Input::WorkerEnded {
                    detail: "exit status: 1".into(),
                },
            },
            now,
        );
        state.note_source_size(Some(10));
        state.apply(Intent::ResetRequested, now);
        assert_eq!(state.source(), &Source::None);
        assert_eq!(state.destination(), None);
        assert_eq!(state.source_size(), None);
        assert!(state.job().is_none());
        assert_eq!(state.settings().crf, Quality::Highest.crf());
    }

    #[test]
    fn settings_follow_the_domain_ranges() {
        let now = Instant::now();
        let mut state = AppState::default();
        state.apply(Intent::PresetEdited(i32::from(SpeedPreset::MAX)), now);
        state.apply(Intent::CrfEdited(i32::from(Crf::MIN)), now);
        state.apply(Intent::OutputSelected(1), now);
        let expected = EncoderSettings {
            output: OutputKind::ALL[1],
            preset: SpeedPreset::new(SpeedPreset::MAX).unwrap_or_else(|e| unreachable!("{e}")),
            crf: Crf::new(Crf::MIN).unwrap_or_else(|e| unreachable!("{e}")),
        };
        assert_eq!(state.settings(), expected);
        assert_eq!(state.output_index(), 1);

        for bad in [
            Intent::PresetEdited(i32::from(SpeedPreset::MAX) + 1),
            Intent::PresetEdited(-1),
            Intent::CrfEdited(i32::from(Crf::MIN) - 1),
            Intent::CrfEdited(i32::from(Crf::MAX) + 1),
            Intent::CrfEdited(i32::MAX),
            Intent::OutputSelected(-1),
            Intent::OutputSelected(2),
        ] {
            state.apply(bad.clone(), now);
            assert_eq!(state.settings(), expected, "{bad:?} must be rejected");
        }
    }

    #[test]
    fn initial_settings_are_valid_and_first_output() {
        let state = AppState::default();
        assert_eq!(state.settings(), INITIAL_SETTINGS);
        assert_eq!(state.output_index(), 0);
        assert_eq!(state.source(), &Source::None);
        assert_eq!(state.destination(), None);
    }

    // Video-only acknowledgement.

    #[test]
    fn convert_asks_first_and_only_the_confirmation_acknowledges() {
        let (mut state, now) = ready_with("/media/trip.mp4", rich(0));
        assert_eq!(state.encode_blocker(), None);
        assert_eq!(state.apply(Intent::EncodeRequested, now), Effect::None);
        let confirmation = state.confirmation().cloned();
        let Some(confirmation) = confirmation else {
            unreachable!("Convert must ask first")
        };
        assert!(!confirmation.job.omissions_acknowledged);
        assert_eq!(confirmation.omissions, rich(0).omissions);
        assert_eq!(confirmation.attached_pictures, 1);
        assert!(!state.job_running());
        // Pressing Convert again does not start anything behind the question.
        assert_eq!(state.apply(Intent::EncodeRequested, now), Effect::None);
        // Nothing new opens behind it.
        assert_eq!(state.apply(Intent::OpenRequested, now), Effect::None);
        assert_eq!(
            state.apply(Intent::ChooseDestinationRequested, now),
            Effect::None
        );

        let Effect::Start { job, .. } = state.apply(Intent::OmissionsConfirmed, now) else {
            unreachable!("confirming starts the job")
        };
        assert_eq!(
            job,
            JobSpec {
                omissions_acknowledged: true,
                ..confirmation.job
            }
        );
        assert_eq!(state.confirmation(), None);
    }

    #[test]
    fn declining_starts_nothing_and_a_stray_confirm_is_ignored() {
        let (mut state, now) = ready_with("/media/trip.mp4", rich(0));
        assert_eq!(state.apply(Intent::OmissionsConfirmed, now), Effect::None);
        state.apply(Intent::EncodeRequested, now);
        assert_eq!(state.apply(Intent::OmissionsDeclined, now), Effect::None);
        assert_eq!(state.confirmation(), None);
        assert_eq!(state.apply(Intent::OmissionsConfirmed, now), Effect::None);
        assert!(state.job().is_none());
    }

    #[test]
    fn an_acknowledgement_is_never_kept_for_the_next_job() {
        let (mut state, now) = ready_with("/media/trip.mp4", rich(0));
        let (id, job) = confirm(&mut state, now);
        assert!(job.omissions_acknowledged);
        end_job(&mut state, id, now, Event::Cancelled);
        // The same file, stream and settings: Convert asks again.
        assert_eq!(state.apply(Intent::EncodeRequested, now), Effect::None);
        assert!(state.confirmation().is_some());
    }

    #[test]
    fn any_change_to_the_job_discards_the_confirmation() {
        type Change = fn(&mut AppState, Instant);
        let changes: [(&str, Change); 6] = [
            ("settings", |s, now| {
                s.apply(Intent::QualitySelected(0), now);
            }),
            ("output kind", |s, now| {
                s.apply(Intent::OutputSelected(1), now);
            }),
            ("destination", |s, now| {
                s.apply(Intent::DestinationPicked(Some("/out/x.ivf".into())), now);
            }),
            ("destination taken", |s, _| s.note_destination_exists(true)),
            ("video track", |s, now| {
                s.apply(Intent::VideoTrackSelected(1), now);
            }),
            ("source", |s, now| {
                s.apply(Intent::PickFinished(Some("/media/other.mp4".into())), now);
            }),
        ];
        for (what, change) in changes {
            let (mut state, now) = ready_with("/media/trip.mp4", rich(0));
            state.apply(Intent::EncodeRequested, now);
            assert!(state.confirmation().is_some());
            change(&mut state, now);
            assert_eq!(state.confirmation(), None, "{what} must discard it");
            assert_eq!(
                state.apply(Intent::OmissionsConfirmed, now),
                Effect::None,
                "{what}"
            );
            assert!(!state.job_running(), "{what}");
        }
    }

    #[test]
    fn the_confirmed_job_carries_the_probed_fingerprint_and_stream() {
        let (mut state, now) = ready_with("/media/trip.mp4", rich(0));
        state.apply(Intent::EncodeRequested, now);
        // Re-reading the file (here: another track, then back) discards the
        // question; the answer carries a new fingerprint.
        let effect = state.apply(Intent::VideoTrackSelected(1), now);
        assert_eq!(state.confirmation(), None);
        state.apply(
            Intent::ProbeAnswered {
                id: probe_id(&effect),
                result: Ok(SourceFacts {
                    fingerprint: SourceFingerprint { len: 2000, ..PRINT },
                    ..rich(1)
                }),
            },
            now,
        );
        assert_eq!(state.apply(Intent::OmissionsConfirmed, now), Effect::None);
        let (_, job) = confirm(&mut state, now);
        assert_eq!(job.fingerprint.len, 2000);
        assert_eq!(job.video_stream, 1);
    }

    #[test]
    fn sources_without_omissions_convert_in_one_click() {
        let video_only = SourceFacts {
            reader: SourceReader::Ffmpeg,
            ..facts(None)
        };
        let (mut state, now) = ready_with("/media/screen.webm", video_only);
        let (_, job) = start(&mut state, now);
        assert!(!job.omissions_acknowledged);
        assert_eq!(state.confirmation(), None);
    }

    // Video track choice.

    #[test]
    fn the_track_picker_lists_movie_tracks_only_and_only_when_there_is_a_choice() {
        let (state, _) = ready();
        assert_eq!(state.track_picker(), None, "one track is no choice");
        let one_and_art = SourceFacts {
            video_tracks: vec![track(0, false), track(1, true)],
            ..rich(0)
        };
        let (state, _) = ready_with("/m/song.mp4", one_and_art);
        assert_eq!(state.track_picker(), None, "cover art is not a choice");

        let (state, _) = ready_with("/m/trip.mp4", rich(1));
        let picker = state.track_picker();
        assert_eq!(
            picker,
            Some(TrackPicker {
                tracks: vec![track(0, false), track(1, false)],
                selected: Some(1),
                attached: vec![track(4, true)],
            })
        );
    }

    #[test]
    fn choosing_a_track_re_probes_that_stream_and_keeps_the_destination() {
        let (mut state, now) = ready_with("/m/trip.mp4", rich(0));
        state.apply(Intent::DestinationPicked(Some("/out/t.ivf".into())), now);
        // The selected track, or one out of range, changes nothing.
        for same in [0, 2, -1] {
            assert_eq!(
                state.apply(Intent::VideoTrackSelected(same), now),
                Effect::None
            );
        }
        let effect = state.apply(Intent::VideoTrackSelected(1), now);
        let Effect::Probe(request) = effect else {
            unreachable!("{effect:?}")
        };
        assert_eq!(request.video_stream, Some(1));
        assert_eq!(request.path, PathBuf::from("/m/trip.mp4"));
        // While reading, the last facts stay visible and the picker shows
        // the requested track, but Convert waits.
        assert_eq!(
            state.source().shown_facts().map(|f| f.video_stream),
            Some(0)
        );
        assert_eq!(state.track_picker().and_then(|p| p.selected), Some(1));
        assert_eq!(
            state.encode_blocker().as_deref(),
            Some("Reading the source…")
        );
        assert_eq!(
            state.destination().map(|d| d.path.clone()),
            Some("/out/t.ivf".into())
        );
        state.apply(
            Intent::ProbeAnswered {
                id: request.id,
                result: Ok(rich(1)),
            },
            now,
        );
        let (_, job) = confirm(&mut state, now);
        assert_eq!(job.video_stream, 1);
    }

    #[test]
    fn a_track_that_cannot_be_read_still_lets_the_user_choose_another() {
        let (mut state, now) = ready_with("/m/trip.mp4", rich(0));
        let request = match state.apply(Intent::VideoTrackSelected(1), now) {
            Effect::Probe(request) => request,
            other => unreachable!("{other:?}"),
        };
        state.apply(
            Intent::ProbeAnswered {
                id: request.id,
                result: Err(ProbeFailure {
                    code: Some(FailureCode::UnsupportedSource),
                    message: "Interlaced video is not supported".into(),
                }),
            },
            now,
        );
        assert!(matches!(state.source(), Source::Unreadable { .. }));
        assert_eq!(state.track_picker().and_then(|p| p.selected), Some(1));
        let back = state.apply(Intent::VideoTrackSelected(0), now);
        assert!(
            matches!(
                back,
                Effect::Probe(ProbeRequest {
                    video_stream: Some(0),
                    ..
                })
            ),
            "{back:?}"
        );
    }

    // FFmpeg availability and a changed source.

    #[test]
    fn a_missing_media_tool_blocks_only_files_that_need_it() {
        let now = Instant::now();
        let mut state = AppState::default();
        state.apply(Intent::EngineAnswered(Ok(capabilities(None))), now);
        assert_eq!(
            state.capabilities().map(|c| c.media_tool.is_none()),
            Some(true)
        );
        let id = probe_id(&state.apply(Intent::PickFinished(Some("/m/a.mp4".into())), now));
        state.apply(
            Intent::ProbeAnswered {
                id,
                result: Err(ProbeFailure {
                    code: Some(FailureCode::MediaToolMissing),
                    message: "this build has no FFmpeg".into(),
                }),
            },
            now,
        );
        assert_eq!(
            state.blocker(),
            Some(Blocker {
                text: "This build converts Y4M files only.".into(),
                severity: Severity::Problem
            })
        );
        // Y4M still works without FFmpeg.
        let id = probe_id(&state.apply(Intent::PickFinished(Some("/m/a.y4m".into())), now));
        state.apply(
            Intent::ProbeAnswered {
                id,
                result: Ok(facts(Some(3))),
            },
            now,
        );
        assert!(matches!(
            state.apply(Intent::EncodeRequested, now),
            Effect::Start { .. }
        ));
    }

    #[test]
    fn a_changed_source_is_read_again_once_and_needs_a_new_confirmation() {
        let (mut state, now) = ready_with("/media/trip.mp4", rich(1));
        let (id, _) = confirm(&mut state, now);
        let changed = Event::Failed {
            failure: Failure {
                code: FailureCode::SourceChanged,
                message: "The source changed since it was probed".into(),
            },
        };
        let effect = end_job(&mut state, id, now, changed.clone());
        let Effect::Job {
            reprobe: Some(request),
            step,
            ..
        } = effect
        else {
            unreachable!("{effect:?}")
        };
        assert_eq!(step.effect, JobEffect::CloseInput);
        assert_eq!(request.video_stream, Some(1));
        // The failure stays visible while the file is read again.
        assert!(matches!(
            state.job().map(|(_, a)| a.phase()),
            Some(Phase::Failed(f)) if f.code == FailureCode::SourceChanged
        ));
        // A duplicate terminal event does not read it again.
        assert!(matches!(
            end_job(&mut state, id, now, changed),
            Effect::Job { reprobe: None, .. }
        ));
        state.apply(
            Intent::ProbeAnswered {
                id: request.id,
                result: Ok(SourceFacts {
                    fingerprint: SourceFingerprint {
                        len: 5,
                        modified: None,
                    },
                    ..rich(1)
                }),
            },
            now,
        );
        let (_, job) = confirm(&mut state, now);
        assert_eq!(job.fingerprint.len, 5);
    }

    #[test]
    fn other_job_failures_do_not_re_probe() {
        let (mut state, now) = ready();
        let (id, _) = start(&mut state, now);
        let effect = end_job(
            &mut state,
            id,
            now,
            Event::Failed {
                failure: Failure {
                    code: FailureCode::UnsupportedSource,
                    message: "Variable frame rate".into(),
                },
            },
        );
        assert!(matches!(effect, Effect::Job { reprobe: None, .. }));
        assert!(matches!(state.source(), Source::Ready { .. }));
    }
}
