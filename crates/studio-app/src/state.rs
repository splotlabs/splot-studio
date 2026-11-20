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

use std::path::{Path, PathBuf};
use std::time::Instant;

use studio_domain::job::{JobSpec, SourceFacts, WirePath};
use studio_domain::settings::{Crf, EncoderSettings, OutputKind, SpeedPreset};

use crate::activity::{self, Activity};
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
        /// The facts, or a message saying why the source cannot be read.
        result: Result<SourceFacts, String>,
    },
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
    /// The startup handshake finished: the output kinds the engine can
    /// encode, or why the engine cannot be used.
    EngineAnswered(Result<Vec<OutputKind>, String>),
    /// Start encoding with the current choices.
    EncodeRequested,
    /// Something happened to a job.
    Job {
        /// The job, as returned in [`Effect::Start`].
        id: u64,
        /// What happened.
        input: activity::Input,
    },
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
    /// Probe `path` with a new worker; any older probe is obsolete.
    Probe {
        /// Echoed in [`Intent::ProbeAnswered`].
        id: u64,
        /// The source.
        path: PathBuf,
    },
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
    },
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
        /// Why, for the user.
        message: String,
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
    encode_outputs: Option<Result<Vec<OutputKind>, String>>,
    /// The current or last job and its id. A finished one is dismissed as
    /// soon as the user changes what a new job would be made from.
    job: Option<(u64, Activity)>,
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
            encode_outputs: None,
            job: None,
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

    fn apply_inner(&mut self, intent: Intent, now: Instant) -> Effect {
        let locked = self.job_running();
        match intent {
            Intent::OpenRequested if self.picking || locked => {}
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
                if let Source::Probing { path, id: current } = &self.source
                    && *current == id
                {
                    let path = path.clone();
                    self.source = match result {
                        Ok(facts) => Source::Ready { path, facts },
                        Err(message) => Source::Unreadable { path, message },
                    };
                }
            }
            Intent::ChooseDestinationRequested if self.saving || locked => {}
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
            Intent::ResetRequested if locked => {}
            Intent::ResetRequested => {
                self.source = Source::None;
                self.destination = None;
                self.source_size = None;
                self.job = None;
            }
            Intent::EngineAnswered(outputs) => self.encode_outputs = Some(outputs),
            Intent::EncodeRequested => return self.start(now),
            Intent::Job { id, input } => {
                if let Some((current, activity)) = &mut self.job
                    && *current == id
                {
                    let step = activity.apply(input, now);
                    return Effect::Job { id, step };
                }
            }
        }
        Effect::None
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
        };
        Effect::Probe { id, path }
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

    fn start(&mut self, now: Instant) -> Effect {
        if self.encode_blocker().is_some() {
            return Effect::None;
        }
        let (Source::Ready { path, facts }, Some(destination)) = (&self.source, &self.destination)
        else {
            return Effect::None;
        };
        let job = JobSpec {
            source: WirePath::from_path(path),
            destination: WirePath::from_path(&destination.path),
            settings: self.settings,
        };
        let activity = Activity::new(destination.path.clone(), facts.frame_count, now);
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
        match &self.encode_outputs {
            Some(Err(problem)) => return blocker(problem, Severity::Problem),
            Some(Ok(outputs)) if outputs.is_empty() => {
                return blocker("This engine build cannot convert.", Severity::Problem);
            }
            _ => {}
        }
        match (&self.source, &self.destination, &self.encode_outputs) {
            (Source::None, ..) => blocker("Open a video to convert it.", Severity::Info),
            (Source::Probing { .. }, ..) => blocker("Reading the source…", Severity::Info),
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
    use crate::activity::{Effect as JobEffect, Input, Phase};
    use studio_domain::job::{ChromaLayout, Rational};
    use studio_domain::protocol::Event;

    fn facts(frame_count: Option<u64>) -> SourceFacts {
        SourceFacts {
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

    /// An app whose engine encodes both kinds and whose source probed fine.
    fn ready() -> (AppState, Instant) {
        let now = Instant::now();
        let mut state = AppState::default();
        state.apply(Intent::EngineAnswered(Ok(OutputKind::ALL.to_vec())), now);
        let Effect::Probe { id, .. } =
            state.apply(Intent::PickFinished(Some("/media/clip.y4m".into())), now)
        else {
            unreachable!("picking a file probes it")
        };
        state.apply(
            Intent::ProbeAnswered {
                id,
                result: Ok(facts(Some(90))),
            },
            now,
        );
        (state, now)
    }

    fn start(state: &mut AppState, now: Instant) -> (u64, JobSpec) {
        match state.apply(Intent::EncodeRequested, now) {
            Effect::Start { id, job } => (id, job),
            other => unreachable!("expected Start, got {other:?}"),
        }
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
            Intent::PickFinished(Some("/media/clip one.y4m".into())),
            now,
        );
        assert_eq!(
            effect,
            Effect::Probe {
                id: 1,
                path: "/media/clip one.y4m".into()
            }
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
            Some(Path::new("/media/clip one.y4m"))
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
                result: Err("not a Y4M file".into()),
            },
            now,
        );
        assert!(
            matches!(state.source(), Source::Unreadable { message, .. } if message == "not a Y4M file")
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

        state.apply(Intent::EngineAnswered(Ok(vec![OutputKind::Ivf])), now);
        state.apply(Intent::OutputSelected(1), now);
        assert_eq!(
            state.encode_blocker(),
            Some(format!(
                "This engine cannot write {}.",
                OutputKind::Av2AnnexB.label()
            ))
        );
        state.apply(Intent::EngineAnswered(Ok(Vec::new())), now);
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
        let (mut state, now) = ready();
        state.apply(Intent::EngineAnswered(Err("Incompatible.".into())), now);
        assert_eq!(state.encode_blocker().as_deref(), Some("Incompatible."));
        assert_eq!(state.apply(Intent::EncodeRequested, now), Effect::None);
    }

    #[test]
    fn an_unanswered_engine_blocks_encode() {
        let now = Instant::now();
        let mut state = AppState::default();
        let Effect::Probe { id, .. } =
            state.apply(Intent::PickFinished(Some("/m/a.y4m".into())), now)
        else {
            unreachable!()
        };
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
    fn encode_builds_the_job_from_the_current_choices_and_locks_them() {
        let (mut state, now) = ready();
        state.apply(Intent::PresetEdited(9), now);
        state.apply(Intent::CrfEdited(33), now);
        let (id, job) = start(&mut state, now);
        assert_eq!(job.source, WirePath::Utf8("/media/clip.y4m".into()));
        assert_eq!(job.destination, WirePath::Utf8("/media/clip.ivf".into()));
        assert_eq!(job.settings.preset.get(), 9);
        assert_eq!(job.settings.crf.get(), 33);
        assert!(state.job_running());
        assert_eq!(state.job().map(|(_, a)| a.total_frames()), Some(Some(90)));

        // Everything the job was built from is locked while it runs.
        let before = state.clone();
        for intent in [
            Intent::OpenRequested,
            Intent::PickFinished(Some("/other.y4m".into())),
            Intent::ChooseDestinationRequested,
            Intent::DestinationPicked(Some("/other.ivf".into())),
            Intent::OutputSelected(1),
            Intent::PresetEdited(1),
            Intent::CrfEdited(1),
            Intent::EncodeRequested,
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
        assert!(matches!(effect, Effect::Job { step, .. } if step.effect == JobEffect::CloseInput));
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
        state.apply(
            Intent::Job {
                id,
                input: Input::Event(Event::Cancelled),
            },
            now,
        );
        // Answers and checks do not dismiss it.
        state.note_destination_exists(false);
        state.apply(Intent::EngineAnswered(Ok(OutputKind::ALL.to_vec())), now);
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
}
