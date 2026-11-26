// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The state machine, without files or processes: the disk is a list of
//! paths and the engine's answers are written out.

use std::path::{Path, PathBuf};
use std::time::Instant;

use studio_domain::job::{
    ChromaLayout, Failure, FailureCode, JobReport, JobSpec, Omissions, Rational, SourceColor,
    SourceFacts, SourceFingerprint, SourceReader, Stage, VideoTrack, WirePath,
};
use studio_domain::protocol::{Capabilities, Event, MediaTool, MediaToolOrigin};
use studio_domain::settings::{Crf, EncoderSettings, OutputKind, SpeedPreset};

use super::*;
use crate::activity::{Effect as JobEffect, Input};
use crate::journal;
use crate::queue::{Cause, Ending, Interruption, PartialFate};
use crate::recovery::{self, Kind};
use crate::simple::{Priority, Quality};
use crate::source::ProbeFailure;
use crate::testing::abs;

const PRINT: SourceFingerprint = SourceFingerprint {
    len: 1000,
    modified: Some((1_700_000_000, 5)),
};

/// A disk holding exactly these files.
#[derive(Default)]
struct Files(Vec<PathBuf>);

impl Disk for Files {
    fn kind(&self, path: &Path) -> Kind {
        if self.0.iter().any(|p| p == path) {
            Kind::File
        } else {
            Kind::Missing
        }
    }
}

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
        color: SourceColor::default(),
        bit_depth: 10,
        chroma: ChromaLayout::Yuv420,
        frame_count,
        disclosures: Vec::new(),
    }
}

/// An MP4 with two movie tracks, cover art, audio, subtitles and chapters,
/// described for `stream`.
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

fn capabilities(media_tool: bool) -> Capabilities {
    Capabilities {
        encode_outputs: OutputKind::ALL.to_vec(),
        media_tool: media_tool.then(|| MediaTool {
            version: "8.0.1".into(),
            origin: MediaToolOrigin::System,
        }),
        analysis_framings: Vec::new(),
        options: None,
    }
}

/// The state, a clock and a disk, driven as the session drives them.
struct App {
    state: AppState,
    now: Instant,
    disk: Files,
}

impl App {
    /// An app whose engine encodes both kinds and has `FFmpeg`.
    fn new() -> Self {
        let mut app = Self {
            state: AppState::default(),
            now: Instant::now(),
            disk: Files::default(),
        };
        app.apply(Intent::EngineAnswered(Ok(capabilities(true))));
        app
    }

    fn apply(&mut self, intent: Intent) -> Update {
        self.state.refresh_destinations(&self.disk);
        self.state.apply(intent, self.now, &self.disk)
    }

    /// Adds `paths` and answers every probe with `answer`; returns the
    /// effects that were not probes.
    fn add(
        &mut self,
        paths: &[&str],
        answer: impl Fn(&Path) -> Result<SourceFacts, ProbeFailure>,
    ) -> Vec<Effect> {
        let update = self.apply(Intent::FilesOpened(paths.iter().map(|p| abs(p)).collect()));
        self.answer(update, answer)
    }

    /// Answers the probes `update` asks for, and those that follow.
    fn answer(
        &mut self,
        mut update: Update,
        answer: impl Fn(&Path) -> Result<SourceFacts, ProbeFailure>,
    ) -> Vec<Effect> {
        let mut others = Vec::new();
        loop {
            let mut next = None;
            for effect in update.effects {
                match effect {
                    Effect::Probe { request, .. } => next = Some(request),
                    other => others.push(other),
                }
            }
            match next {
                Some(request) => {
                    let result = answer(&request.path);
                    update = self.apply(Intent::ProbeAnswered {
                        id: request.id,
                        result,
                    });
                }
                None => return others,
            }
        }
    }

    fn event(&mut self, id: u64, event: Event) -> Update {
        self.apply(Intent::Job {
            id,
            input: Input::Event(event),
        })
    }

    /// Publishes and completes job `id`.
    fn complete(&mut self, id: u64) -> Update {
        self.event(
            id,
            Event::Stage {
                stage: Stage::Publishing,
            },
        );
        let destination = self
            .state
            .items()
            .iter()
            .find(|i| i.attempt().is_some_and(|a| a.request == id))
            .map(|i| WirePath::from_path(&i.destination().path));
        self.event(
            id,
            Event::Completed {
                report: JobReport {
                    pass_digest: None,
                    destination: destination.unwrap_or(WirePath::Utf8(String::new())),
                    frames: 3,
                    bytes: 100,
                    checks: Vec::new(),
                    disclosures: Vec::new(),
                },
            },
        )
    }

    fn fail(&mut self, id: u64, code: FailureCode) -> Update {
        self.event(
            id,
            Event::Failed {
                failure: Failure {
                    code,
                    message: "it went wrong".into(),
                },
            },
        )
    }

    fn states(&self) -> Vec<ItemState> {
        self.state.items().iter().map(Item::state).collect()
    }
}

#[allow(clippy::unnecessary_wraps)] // the shape of a probe answer
fn ready(_: &Path) -> Result<SourceFacts, ProbeFailure> {
    Ok(facts(Some(90)))
}

#[allow(clippy::unnecessary_wraps)] // the shape of a probe answer
fn with_audio(_: &Path) -> Result<SourceFacts, ProbeFailure> {
    Ok(rich(0))
}

fn starts(effects: &[Effect]) -> Vec<(u64, JobSpec)> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Start { id, job } => Some((*id, job.clone())),
            _ => None,
        })
        .collect()
}

fn only_start(effects: &[Effect]) -> (u64, JobSpec) {
    match starts(effects).as_slice() {
        [one] => one.clone(),
        other => unreachable!("expected one Start, got {other:?} in {effects:?}"),
    }
}

/// Presses Convert and returns the job it starts.
fn convert(app: &mut App) -> (u64, JobSpec) {
    let effects = app.apply(Intent::ConvertRequested).effects;
    only_start(&effects)
}

fn source_of(job: &JobSpec) -> PathBuf {
    job.source.to_path_buf().unwrap_or_default()
}

fn dest_of(job: &JobSpec) -> PathBuf {
    job.destination.to_path_buf().unwrap_or_default()
}

// The one-file window, as before the queue.

#[test]
fn open_is_not_reentrant_while_a_dialog_shows() {
    let mut app = App::new();
    assert_eq!(
        app.apply(Intent::AddRequested).effects,
        [Effect::ShowOpenDialog]
    );
    assert_eq!(app.apply(Intent::AddRequested).effects, []);
    app.apply(Intent::PickFinished(Vec::new()));
    assert_eq!(
        app.apply(Intent::AddRequested).effects,
        [Effect::ShowOpenDialog]
    );
}

#[test]
fn picking_probes_and_sets_a_default_destination() {
    let mut app = App::new();
    app.apply(Intent::AddRequested);
    let update = app.apply(Intent::PickFinished(vec![abs("/media/clip one.mkv")]));
    assert!(matches!(
        update.effects.as_slice(),
        [Effect::Probe { request, .. }] if request.path == abs("/media/clip one.mkv")
            && request.video_stream.is_none()
    ));
    assert_eq!(app.state.mode(), Mode::Single);
    let item = &app.state.items()[0];
    assert_eq!(item.destination().path, abs("/media/clip one.ivf"));
    assert!(!item.destination().chosen);
    assert_eq!(
        app.state.blocker().map(|b| b.text).as_deref(),
        Some("Reading the source…")
    );
    // Cancelling a later dialog keeps the file.
    app.apply(Intent::AddRequested);
    app.apply(Intent::PickFinished(Vec::new()));
    assert_eq!(app.state.items().len(), 1);
}

#[test]
fn change_replaces_the_one_file_and_add_adds() {
    let mut app = App::new();
    app.add(&["/m/a.y4m"], ready);
    assert_eq!(
        app.apply(Intent::ReplaceRequested).effects,
        [Effect::ShowOpenDialog]
    );
    let update = app.apply(Intent::PickFinished(vec![abs("/m/b.y4m")]));
    app.answer(update, ready);
    let sources: Vec<_> = app
        .state
        .items()
        .iter()
        .map(|i| i.source().to_path_buf())
        .collect();
    assert_eq!(sources, [abs("/m/b.y4m")]);
    app.apply(Intent::AddRequested);
    let update = app.apply(Intent::PickFinished(vec![abs("/m/c.y4m"), abs("/m/d.y4m")]));
    app.answer(update, ready);
    assert_eq!(app.state.mode(), Mode::List);
    assert_eq!(app.state.items().len(), 3);
}

#[test]
fn a_stale_probe_answer_is_ignored() {
    let mut app = App::new();
    let first = app.apply(Intent::FilesOpened(vec![abs("/a.y4m")]));
    let Some(Effect::Probe { request, .. }) = first.effects.first().cloned() else {
        unreachable!("{first:?}")
    };
    app.apply(Intent::RemoveRequested(0));
    app.apply(Intent::FilesOpened(vec![abs("/b.y4m")]));
    app.apply(Intent::ProbeAnswered {
        id: request.id,
        result: Ok(facts(None)),
    });
    assert_eq!(app.states(), [ItemState::Probing]);
}

#[test]
fn an_unreadable_file_says_why_convert_is_unavailable() {
    let mut app = App::new();
    app.add(&["/a.y4m"], |_| {
        Err(ProbeFailure {
            code: Some(FailureCode::InvalidSource),
            message: "not a video".into(),
        })
    });
    assert_eq!(app.states(), [ItemState::Unreadable]);
    assert_eq!(
        app.state.blocker().map(|b| b.text).as_deref(),
        Some("This file cannot be converted.")
    );
}

#[test]
fn the_default_destination_follows_the_output_kind_but_a_chosen_one_stays() {
    let mut app = App::new();
    app.add(&["/media/clip.y4m"], ready);
    app.apply(Intent::OutputSelected(1));
    let dest = |app: &App| app.state.items()[0].destination().clone();
    assert_eq!(dest(&app).path, abs("/media/clip.obu"));
    let effect = app.apply(Intent::ChooseDestinationRequested).effects;
    assert_eq!(
        effect,
        [Effect::ShowSaveDialog {
            directory: Some(abs("/media")),
            file_name: "clip.obu".into(),
            output: OutputKind::Av2AnnexB
        }]
    );
    app.apply(Intent::DestinationPicked(Some(abs("/out/mine.obu"))));
    app.apply(Intent::OutputSelected(0));
    assert_eq!(
        (dest(&app).path, dest(&app).chosen),
        (abs("/out/mine.obu"), true)
    );
}

#[test]
fn encode_needs_a_probe_a_free_destination_and_engine_support() {
    let mut app = App::new();
    assert_eq!(
        app.state.blocker().map(|b| b.text).as_deref(),
        Some("Open a video to convert it.")
    );
    assert_eq!(app.apply(Intent::ConvertRequested).effects, []);

    app.add(&["/media/clip.y4m"], ready);
    assert_eq!(app.state.blocker(), None);
    // A file that appears at a chosen destination blocks it.
    app.apply(Intent::ChooseDestinationRequested);
    app.apply(Intent::DestinationPicked(Some(abs("/out/x.ivf"))));
    app.disk.0.push(abs("/out/x.ivf"));
    app.apply(Intent::RowSelected(0));
    assert_eq!(
        app.state.blocker().map(|b| b.text).as_deref(),
        Some("The output file already exists.")
    );
    assert_eq!(app.apply(Intent::ConvertRequested).effects, []);
    app.disk.0.clear();
    app.apply(Intent::RowSelected(0));

    let ivf_only = Capabilities {
        encode_outputs: vec![OutputKind::Ivf],
        media_tool: None,
        analysis_framings: Vec::new(),
        options: None,
    };
    app.apply(Intent::EngineAnswered(Ok(ivf_only)));
    app.apply(Intent::OutputSelected(1));
    assert_eq!(
        app.state.blocker().map(|b| b.text),
        Some(format!(
            "This engine cannot write {}.",
            OutputKind::Av2AnnexB.label()
        ))
    );
    app.apply(Intent::EngineAnswered(Ok(Capabilities::default())));
    app.apply(Intent::OutputSelected(0));
    assert_eq!(
        app.state.blocker().map(|b| b.severity),
        Some(Severity::Problem)
    );
}

#[test]
fn an_engine_problem_shows_before_anything_else() {
    let mut app = App::new();
    app.add(&["/m/a.y4m"], ready);
    app.apply(Intent::EngineAnswered(Err("Incompatible.".into())));
    assert_eq!(
        app.state.blocker(),
        Some(Blocker {
            text: "Incompatible.".into(),
            severity: Severity::Problem
        })
    );
    assert_eq!(app.state.capabilities(), None);
    assert_eq!(app.apply(Intent::ConvertRequested).effects, []);
}

#[test]
fn an_unanswered_engine_blocks_encode() {
    let mut app = App::new();
    app.state = AppState::default();
    app.add(&["/m/a.y4m"], ready);
    assert_eq!(
        app.state.blocker().map(|b| b.text).as_deref(),
        Some("Checking the engine…")
    );
}

#[test]
fn convert_builds_the_job_from_the_probe_and_the_current_settings() {
    let mut app = App::new();
    app.add(&["/media/clip.y4m"], ready);
    app.apply(Intent::PresetEdited(9));
    app.apply(Intent::CrfEdited(33));
    let (id, job) = convert(&mut app);
    assert_eq!(job.source, WirePath::from_path(&abs("/media/clip.y4m")));
    assert_eq!(
        job.destination,
        WirePath::from_path(&abs("/media/clip.ivf"))
    );
    assert_eq!(job.fingerprint, PRINT);
    assert_eq!(job.video_stream, 0);
    // Nothing to leave out: one click, nothing acknowledged.
    assert!(!job.omissions_acknowledged);
    assert_eq!(job.settings.base.preset.get(), 9);
    assert_eq!(job.settings.base.crf.get(), 33);
    assert!(app.state.running());
    assert_eq!(app.states(), [ItemState::Running]);

    // With one file, its settings are locked while it converts.
    let view = app.state.settings_view();
    assert!(view.read_only);
    let before = app.state.clone();
    let replacement = OptionRequest::from(INITIAL_SETTINGS);
    assert!(!app.state.can_undo_options());
    assert_eq!(app.state.visible_option_request(), &job.settings);
    for intent in [
        Intent::ReplaceOptions(replacement),
        Intent::UndoOptions,
        Intent::RedoOptions,
        Intent::ReplaceRequested,
        Intent::VideoTrackSelected(1),
        Intent::ChooseDestinationRequested,
        Intent::OutputSelected(1),
        Intent::PresetEdited(1),
        Intent::CrfEdited(1),
        Intent::QualitySelected(0),
        Intent::ConvertRequested,
        Intent::OmissionsConfirmed,
        Intent::ResetRequested,
        Intent::RemoveRequested(0),
    ] {
        assert_eq!(app.apply(intent.clone()).effects, [], "{intent:?}");
        assert_eq!(
            app.state, before,
            "{intent:?} must not change a running job"
        );
    }

    // Its own events reach it; another request's do not.
    let other = app.event(id + 100, Event::Cancelled);
    assert_eq!(other.changed, Changed::Nothing);
    app.apply(Intent::CancelRequested);
    let update = app.event(id, Event::Cancelled);
    assert!(update.effects.contains(&Effect::Worker {
        id,
        effect: JobEffect::CloseInput
    }));
    assert_eq!(app.states(), [ItemState::Cancelled]);
    assert!(!app.state.running());
    // Convert again retries it: read first, then started.
    let update = app.apply(Intent::ConvertRequested);
    let effects = app.answer(update, ready);
    assert_eq!(starts(&effects).len(), 1);
}

#[test]
fn simple_steps_and_raw_values_are_one_setting() {
    let mut app = App::new();
    app.apply(Intent::QualitySelected(0));
    app.apply(Intent::PrioritySelected(2));
    assert_eq!(app.state.settings().crf, Quality::Compact.crf());
    assert_eq!(app.state.settings().preset, Priority::SmallestFile.preset());
    app.apply(Intent::CrfEdited(37));
    assert_eq!(Quality::of(app.state.settings().crf), None);
    app.apply(Intent::CrfEdited(28));
    assert_eq!(Quality::of(app.state.settings().crf), Some(Quality::Good));
    for bad in [-1, 4, i32::MAX] {
        app.apply(Intent::QualitySelected(bad));
        app.apply(Intent::PrioritySelected(bad));
    }
    assert_eq!(app.state.settings().crf.get(), 28);
}

#[test]
fn simple_and_advanced_edits_share_one_history() {
    let mut app = App::new();
    app.add(&["/m/clip.y4m"], ready);
    let initial = app.state.option_request().clone();
    let mut explicit = initial.clone();
    explicit
        .values
        .insert("preset.tcq".into(), OptionValue::Bool(false));
    explicit.values.insert(
        "rate_control".into(),
        OptionValue::Record(BTreeMap::from([
            ("mode".into(), OptionValue::Choice("crf".into())),
            ("crf".into(), OptionValue::Integer(20)),
        ])),
    );
    explicit.values.insert("crop".into(), OptionValue::None);
    app.apply(Intent::ReplaceOptions(explicit.clone()));
    let view = app.state.settings_view();
    assert_eq!(view.quality(), None);
    assert_eq!(view.priority(), None);

    let intent = Intent::PrioritySelected(0);
    let expected = app.state.proposed_options(&intent);
    assert_eq!(app.state.option_request(), &explicit);
    app.apply(intent);
    assert_eq!(expected.as_ref(), Some(app.state.option_request()));
    assert_eq!(app.state.option_request().values, explicit.values);

    app.apply(Intent::OutputSelected(1));
    assert_eq!(app.state.items()[0].destination().path, abs("/m/clip.obu"));
    app.apply(Intent::UndoOptions);
    assert_eq!(expected.as_ref(), Some(app.state.option_request()));
    assert_eq!(app.state.items()[0].destination().path, abs("/m/clip.ivf"));
    app.apply(Intent::UndoOptions);
    assert_eq!(app.state.option_request(), &explicit);
    app.apply(Intent::UndoOptions);
    assert_eq!(app.state.option_request(), &initial);
    assert!(!app.state.can_undo_options());
    assert!(app.state.can_redo_options());

    app.apply(Intent::RedoOptions);
    assert_eq!(app.state.option_request(), &explicit);
    app.apply(Intent::ReplaceOptions(explicit));
    assert!(app.state.can_redo_options(), "A no-op must keep redo.");
    app.apply(Intent::QualitySelected(0));
    assert!(
        !app.state.can_redo_options(),
        "A different edit replaces the redo branch."
    );
    assert_eq!(app.state.settings_view().quality(), None);
    assert_eq!(app.state.options_generation(), 8);
}

#[test]
fn draft_history_has_a_fixed_limit() {
    let mut app = App::new();
    for change in 0..40 {
        app.apply(Intent::CrfEdited(if change % 2 == 0 { 21 } else { 22 }));
    }
    let mut restored = 0;
    while app.state.can_undo_options() {
        app.apply(Intent::UndoOptions);
        restored += 1;
        assert!(restored <= 32);
    }
    assert_eq!(restored, 32);
    assert_eq!(app.state.settings().crf.get(), 22);
}

#[test]
fn settings_follow_the_domain_ranges() {
    let mut app = App::new();
    app.apply(Intent::PresetEdited(i32::from(SpeedPreset::MAX)));
    app.apply(Intent::CrfEdited(i32::from(Crf::MIN)));
    app.apply(Intent::OutputSelected(1));
    let expected = EncoderSettings {
        output: OutputKind::ALL[1],
        preset: SpeedPreset::new(SpeedPreset::MAX).unwrap_or_else(|e| unreachable!("{e}")),
        crf: Crf::new(Crf::MIN).unwrap_or_else(|e| unreachable!("{e}")),
    };
    assert_eq!(app.state.settings(), expected);
    assert_eq!(AppState::output_index(app.state.settings().output), 1);
    for bad in [
        Intent::PresetEdited(i32::from(SpeedPreset::MAX) + 1),
        Intent::PresetEdited(-1),
        Intent::CrfEdited(i32::from(Crf::MIN) - 1),
        Intent::CrfEdited(i32::from(Crf::MAX) + 1),
        Intent::OutputSelected(-1),
        Intent::OutputSelected(2),
    ] {
        app.apply(bad.clone());
        assert_eq!(app.state.settings(), expected, "{bad:?} must be rejected");
    }
}

#[test]
fn convert_another_clears_the_file_but_keeps_the_settings() {
    let mut app = App::new();
    app.add(&["/m/a.y4m"], ready);
    app.apply(Intent::QualitySelected(3));
    let (id, _) = convert(&mut app);
    app.apply(Intent::ResetRequested);
    assert_eq!(app.state.items().len(), 1, "a running file is not cleared");
    app.complete(id);
    assert_eq!(app.states(), [ItemState::Completed]);
    app.apply(Intent::ResetRequested);
    assert_eq!(app.state.mode(), Mode::Empty);
    assert_eq!(app.state.settings().crf, Quality::Highest.crf());
}

// Video-only acknowledgement.

#[test]
fn convert_asks_first_and_only_the_confirmation_acknowledges() {
    let mut app = App::new();
    app.add(&["/media/trip.mp4"], with_audio);
    assert_eq!(app.state.blocker(), None);
    assert_eq!(app.apply(Intent::ConvertRequested).effects, []);
    let Some(confirmation) = app.state.confirmation().cloned() else {
        unreachable!("Convert must ask first")
    };
    assert_eq!(confirmation.asked.len(), 1);
    assert_eq!(confirmation.asked[0].omissions, rich(0).omissions);
    assert_eq!(confirmation.asked[0].attached_pictures, 1);
    assert!(!app.state.running());
    // Pressing Convert again starts nothing behind the question, and
    // nothing new opens behind it.
    assert_eq!(app.apply(Intent::ConvertRequested).effects, []);
    assert_eq!(app.apply(Intent::AddRequested).effects, []);
    assert_eq!(app.apply(Intent::ChooseDestinationRequested).effects, []);

    let (_, job) = only_start(&app.apply(Intent::OmissionsConfirmed).effects);
    assert!(job.omissions_acknowledged);
    assert_eq!(app.state.confirmation(), None);
}

#[test]
fn declining_starts_nothing_and_a_stray_confirm_is_ignored() {
    let mut app = App::new();
    app.add(&["/media/trip.mp4"], with_audio);
    assert_eq!(app.apply(Intent::OmissionsConfirmed).effects, []);
    app.apply(Intent::ConvertRequested);
    assert_eq!(app.apply(Intent::OmissionsDeclined).effects, []);
    assert_eq!(app.state.confirmation(), None);
    assert_eq!(app.apply(Intent::OmissionsConfirmed).effects, []);
    assert!(!app.state.running());
    assert_eq!(app.state.items()[0].acknowledgement(), None);
}

#[test]
fn an_acknowledgement_holds_for_its_file_while_nothing_changes() {
    let mut app = App::new();
    app.add(&["/media/trip.mp4"], with_audio);
    app.apply(Intent::ConvertRequested);
    let (id, _) = only_start(&app.apply(Intent::OmissionsConfirmed).effects);
    app.apply(Intent::CancelRequested);
    app.event(id, Event::Cancelled);
    // The same file, stream and settings: it is still acknowledged, and
    // converting again does not ask.
    assert!(app.state.items()[0].acknowledgement().is_some());
    let update = app.apply(Intent::ConvertRequested);
    let effects = app.answer(update, with_audio);
    let (_, job) = only_start(&effects);
    assert!(job.omissions_acknowledged);
}

#[test]
fn changing_track_requires_a_new_omissions_acknowledgement() {
    let mut app = App::new();
    app.add(&["/media/trip.mp4"], with_audio);
    app.apply(Intent::ConvertRequested);
    let (id, _) = only_start(&app.apply(Intent::OmissionsConfirmed).effects);
    app.apply(Intent::CancelRequested);
    app.event(id, Event::Cancelled);
    assert!(app.state.items()[0].acknowledgement().is_some());
    let update = app.apply(Intent::VideoTrackSelected(1));
    app.answer(update, |_| Ok(rich(1)));
    assert_eq!(app.state.items()[0].acknowledgement(), None);
    assert_eq!(app.apply(Intent::ConvertRequested).effects, []);
    assert!(app.state.confirmation().is_some());
}

#[test]
fn retry_uses_the_original_acknowledged_settings() {
    let mut app = App::new();
    app.add(&["/media/trip.mp4"], with_audio);
    app.apply(Intent::ConvertRequested);
    let (id, original) = only_start(&app.apply(Intent::OmissionsConfirmed).effects);
    app.apply(Intent::CancelRequested);
    app.event(id, Event::Cancelled);
    app.apply(Intent::QualitySelected(0));
    app.apply(Intent::PresetEdited(1));
    app.apply(Intent::OutputSelected(1));
    let retry = app.apply(Intent::RowAction(0));
    app.answer(retry, with_audio);
    let (_, job) = only_start(&app.apply(Intent::ConvertRequested).effects);
    assert_eq!(job.settings, original.settings);
    assert_eq!(job.destination, original.destination);
    assert!(job.omissions_acknowledged);
}

#[test]
fn convert_after_failure_uses_the_corrected_draft_and_confirms_omissions() {
    let mut app = App::new();
    app.add(&["/media/trip.mp4"], with_audio);
    app.apply(Intent::ConvertRequested);
    let (id, _) = only_start(&app.apply(Intent::OmissionsConfirmed).effects);
    app.fail(id, FailureCode::InvalidSettings);
    app.apply(Intent::PresetEdited(1));
    app.apply(Intent::OutputSelected(1));
    app.apply(Intent::OptionEdited {
        key: "fast_decode".into(),
        value: Some(OptionValue::Bool(true)),
    });
    let convert = app.apply(Intent::ConvertRequested);
    let effects = app.answer(convert, with_audio);
    assert_eq!(starts(&effects), []);
    let (_, job) = only_start(&app.apply(Intent::OmissionsConfirmed).effects);
    assert_eq!(&job.settings, app.state.option_request());
    assert_eq!(dest_of(&job), abs("/media/trip.obu"));
    assert!(job.omissions_acknowledged);
}

#[test]
fn retry_cannot_start_after_its_source_changed() {
    let mut app = App::new();
    app.add(&["/media/trip.mp4"], with_audio);
    app.apply(Intent::ConvertRequested);
    let (id, _) = only_start(&app.apply(Intent::OmissionsConfirmed).effects);
    app.apply(Intent::CancelRequested);
    app.event(id, Event::Cancelled);
    let retry = app.apply(Intent::RowAction(0));
    app.answer(retry, |_| {
        Ok(SourceFacts {
            fingerprint: SourceFingerprint { len: 7, ..PRINT },
            ..rich(0)
        })
    });
    assert_eq!(app.state.items()[0].acknowledgement(), None);
    assert_eq!(app.apply(Intent::ConvertRequested).effects, []);
    assert!(
        app.state
            .blocker()
            .is_some_and(|blocker| blocker.text.contains("Add the source again."))
    );
}

#[test]
fn an_open_question_is_discarded_by_any_change() {
    type Change = fn(&mut App);
    let changes: [(&str, Change); 4] = [
        ("settings", |app| {
            app.apply(Intent::QualitySelected(0));
        }),
        ("output kind", |app| {
            app.apply(Intent::OutputSelected(1));
        }),
        ("destination taken", |app| {
            let path = app.state.items()[0].destination().path.clone();
            app.disk.0.push(path);
            app.apply(Intent::RowSelected(0));
        }),
        ("removed", |app| {
            // The confirmation is modal, so the row action waits; a
            // programmatic removal still invalidates it.
            let id = app.state.items()[0].id();
            app.state.remove(id);
            app.apply(Intent::RowSelected(0));
        }),
    ];
    for (what, change) in changes {
        let mut app = App::new();
        app.add(&["/media/trip.mp4"], with_audio);
        app.apply(Intent::ConvertRequested);
        assert!(app.state.confirmation().is_some());
        change(&mut app);
        assert_eq!(app.state.confirmation(), None, "{what} must discard it");
        assert_eq!(app.apply(Intent::OmissionsConfirmed).effects, [], "{what}");
        assert!(!app.state.running(), "{what}");
    }
}

#[test]
fn an_acknowledgement_is_never_moved_to_another_file() {
    let mut app = App::new();
    app.add(&["/media/trip.mp4"], with_audio);
    app.apply(Intent::ConvertRequested);
    let (id, _) = only_start(&app.apply(Intent::OmissionsConfirmed).effects);
    app.complete(id);
    // The same file added again is a new item: it is asked about again.
    app.add(&["/media/trip.mp4"], with_audio);
    assert_eq!(app.state.items().len(), 2);
    assert_eq!(app.state.items()[1].acknowledgement(), None);
    assert_eq!(app.apply(Intent::ConvertRequested).effects, []);
    let asked: Vec<ItemId> = app
        .state
        .confirmation()
        .map_or_default(|c| c.asked.iter().map(|a| a.item).collect());
    assert_eq!(asked, [app.state.items()[1].id()]);
}

#[test]
fn one_question_lists_every_file_that_leaves_something_out() {
    let mut app = App::new();
    app.add(&["/m/a.mp4", "/m/b.y4m", "/m/c.mp4"], |p| {
        if p.extension().is_some_and(|e| e == "mp4") {
            Ok(rich(0))
        } else {
            Ok(facts(Some(3)))
        }
    });
    assert_eq!(app.apply(Intent::ConvertRequested).effects, []);
    let asked: Vec<PathBuf> = app
        .state
        .confirmation()
        .map_or_default(|c| c.asked.iter().map(|a| a.source.clone()).collect());
    assert_eq!(asked, [abs("/m/a.mp4"), abs("/m/c.mp4")]);
    let (_, job) = only_start(&app.apply(Intent::OmissionsConfirmed).effects);
    assert_eq!(source_of(&job), abs("/m/a.mp4"));
    assert!(job.omissions_acknowledged);
    for item in app.state.items() {
        assert_eq!(
            item.acknowledgement().is_some(),
            item.leaves_out_something(),
            "{:?}",
            item.source()
        );
    }
}

// Runs.

#[test]
fn a_run_converts_every_waiting_file_in_order_and_one_at_a_time() {
    let mut app = App::new();
    app.add(&["/m/a.y4m", "/m/b.y4m", "/m/c.y4m"], ready);
    assert_eq!(app.state.startable_count(), 3);
    let (a, job) = convert(&mut app);
    assert_eq!(source_of(&job), abs("/m/a.y4m"));
    assert_eq!(app.state.run_position(), Some((1, 3)));
    // While one runs, nothing else starts, whatever arrives.
    for intent in [Intent::ConvertRequested, Intent::RowSelected(2)] {
        assert_eq!(starts(&app.apply(intent).effects), []);
    }
    let (b, job) = only_start(&app.complete(a).effects);
    assert_eq!(source_of(&job), abs("/m/b.y4m"));
    assert_eq!(app.state.run_position(), Some((2, 3)));
    let (c, job) = only_start(&app.fail(b, FailureCode::Encoder).effects);
    assert_eq!(source_of(&job), abs("/m/c.y4m"), "a failure moves on");
    let update = app.apply(Intent::Job {
        id: c,
        input: Input::WorkerEnded {
            detail: "signal: 9".into(),
        },
    });
    assert_eq!(starts(&update.effects), []);
    assert!(!app.state.running(), "the run ends when nothing waits");
    assert_eq!(
        app.states(),
        [ItemState::Completed, ItemState::Failed, ItemState::Lost]
    );
    assert_eq!(
        crate::present::queue::summary(app.state.items()),
        "1 converted, 2 failed"
    );
}

#[test]
fn cancel_pauses_the_run_and_continue_resumes_it() {
    let mut app = App::new();
    app.add(&["/m/a.y4m", "/m/b.y4m", "/m/c.y4m"], ready);
    let (a, _) = convert(&mut app);
    let update = app.apply(Intent::RowAction(0));
    assert_eq!(
        update.effects,
        [Effect::Worker {
            id: a,
            effect: JobEffect::SendCancel
        }]
    );
    let update = app.event(a, Event::Cancelled);
    assert_eq!(starts(&update.effects), [], "Cancel pauses the run");
    assert!(!app.state.running());
    assert!(app.state.paused());
    assert_eq!(
        crate::present::queue::primary(false, app.state.startable_count(), app.state.paused()),
        "Continue"
    );
    assert_eq!(
        app.states(),
        [ItemState::Cancelled, ItemState::Waiting, ItemState::Waiting]
    );
    let (_, job) = convert(&mut app);
    assert_eq!(source_of(&job), abs("/m/b.y4m"));
    assert!(!app.state.paused());
}

#[test]
fn a_completion_that_races_cancel_still_pauses() {
    let mut app = App::new();
    app.add(&["/m/a.y4m", "/m/b.y4m"], ready);
    let (a, _) = convert(&mut app);
    app.apply(Intent::CancelRequested);
    let update = app.complete(a);
    assert_eq!(starts(&update.effects), []);
    assert_eq!(app.states(), [ItemState::Completed, ItemState::Waiting]);
    assert!(app.state.paused());
}

#[test]
fn files_added_during_a_run_join_it() {
    let mut app = App::new();
    app.add(&["/m/a.y4m", "/m/b.y4m"], ready);
    let (a, _) = convert(&mut app);
    let effects = app.add(&["/m/c.y4m"], ready);
    assert_eq!(starts(&effects), []);
    let (b, _) = only_start(&app.complete(a).effects);
    assert_eq!(app.state.run_position(), Some((2, 3)));
    let (_, job) = only_start(&app.complete(b).effects);
    assert_eq!(source_of(&job), abs("/m/c.y4m"));
}

#[test]
fn a_file_that_joins_unacknowledged_is_asked_about_at_the_end() {
    let mut app = App::new();
    app.add(&["/m/a.y4m", "/m/b.y4m"], ready);
    let (a, _) = convert(&mut app);
    app.add(&["/m/c.mp4"], with_audio);
    let (b, _) = only_start(&app.complete(a).effects);
    // Everything that can run runs first, then the question.
    assert_eq!(starts(&app.complete(b).effects), []);
    assert!(app.state.confirmation().is_some());
    assert!(app.state.running());
    let (_, job) = only_start(&app.apply(Intent::OmissionsConfirmed).effects);
    assert!(job.omissions_acknowledged);
}

#[test]
fn settings_edits_do_not_change_submitted_jobs() {
    let mut app = App::new();
    app.add(&["/m/a.y4m", "/m/b.y4m"], ready);
    let key = "fast_decode".to_owned();
    app.apply(Intent::OptionEdited {
        key: key.clone(),
        value: Some(OptionValue::Bool(false)),
    });
    let (a, first) = convert(&mut app);
    // In the list, the running row shows its own settings, read-only;
    // selecting a waiting row shows the current ones, editable.
    app.apply(Intent::RowSelected(0));
    assert!(app.state.settings_view().read_only);
    app.apply(Intent::QualitySelected(0));
    assert_eq!(app.state.settings(), INITIAL_SETTINGS, "read-only view");
    app.apply(Intent::RowSelected(1));
    assert!(!app.state.settings_view().read_only);
    app.apply(Intent::QualitySelected(0));
    app.apply(Intent::OutputSelected(1));
    app.apply(Intent::OptionEdited {
        key: key.clone(),
        value: Some(OptionValue::Bool(true)),
    });
    let running = app.state.items()[0].attempt().map(|a| a.job.clone());
    assert_eq!(running.as_ref(), Some(&first), "the snapshot is immutable");
    assert_eq!(first.settings.base, INITIAL_SETTINGS);
    assert_eq!(
        first.settings.values.get(&key),
        Some(&OptionValue::Bool(false))
    );
    assert_eq!(
        app.state.option_request().values.get(&key),
        Some(&OptionValue::Bool(true))
    );
    let (_, second) = only_start(&app.complete(a).effects);
    assert_eq!(second.settings, first.settings);
    assert_eq!(dest_of(&second), abs("/m/b.ivf"));
    // The converted row remembers what it ran with.
    app.apply(Intent::RowSelected(0));
    let view = app.state.settings_view();
    assert!(view.read_only);
    assert_eq!(view.settings, INITIAL_SETTINGS);
}

#[test]
fn a_changed_source_is_read_again_and_its_acknowledgement_dropped() {
    let mut app = App::new();
    app.add(&["/m/trip.mp4", "/m/b.y4m"], |p| {
        if p.extension().is_some_and(|e| e == "mp4") {
            Ok(rich(1))
        } else {
            Ok(facts(Some(3)))
        }
    });
    app.apply(Intent::ConvertRequested);
    let (id, _) = only_start(&app.apply(Intent::OmissionsConfirmed).effects);
    let update = app.fail(id, FailureCode::SourceChanged);
    // Read again, the same track, once; the run moves on meanwhile.
    let probes: Vec<_> = update
        .effects
        .iter()
        .filter_map(|e| match e {
            Effect::Probe { request, .. } => Some(request.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(probes.len(), 1);
    assert_eq!(probes[0].video_stream, Some(1));
    assert_eq!(starts(&update.effects).len(), 1);
    // A duplicate terminal event does not read it again.
    let again = app.fail(id, FailureCode::SourceChanged);
    assert!(
        !again
            .effects
            .iter()
            .any(|e| matches!(e, Effect::Probe { .. }))
    );
    app.answer(update, |_| {
        Ok(SourceFacts {
            fingerprint: SourceFingerprint { len: 5, ..PRINT },
            ..rich(1)
        })
    });
    let item = &app.state.items()[0];
    assert_eq!(item.state(), ItemState::Waiting);
    assert_eq!(item.acknowledgement(), None);
    // The failure stays in view while it waits.
    assert!(
        matches!(item.ending(), Some(Ending::Failed(f)) if f.code == FailureCode::SourceChanged)
    );
}

#[test]
fn other_job_failures_do_not_re_probe() {
    let mut app = App::new();
    app.add(&["/m/a.y4m"], ready);
    let (id, _) = convert(&mut app);
    let update = app.fail(id, FailureCode::UnsupportedSource);
    assert!(
        !update
            .effects
            .iter()
            .any(|e| matches!(e, Effect::Probe { .. }))
    );
    assert_eq!(app.states(), [ItemState::Failed]);
}

#[test]
fn the_publication_intent_is_recorded_before_completion() {
    let mut app = App::new();
    app.add(&["/m/a.y4m"], ready);
    let (id, _) = convert(&mut app);
    app.event(
        id,
        Event::Stage {
            stage: Stage::Verifying,
        },
    );
    assert_eq!(app.states(), [ItemState::Running]);
    let update = app.event(
        id,
        Event::Stage {
            stage: Stage::Publishing,
        },
    );
    assert_eq!(update.changed, Changed::Phase);
    assert_eq!(app.states(), [ItemState::Publishing]);
    // Progress never changes the durable state.
    let progress = app.event(
        id,
        Event::Progress {
            progress: studio_domain::job::Progress {
                frames_read: 1,
                frames_encoded: 1,
                bytes_written: 1,
                frames_coded_milli: 1000,
            },
        },
    );
    assert_eq!(progress.changed, Changed::Counters);
}

#[test]
fn a_dead_workers_temporary_file_is_cleaned_up_once_it_is_gone() {
    let mut app = App::new();
    app.add(&["/m/a.y4m", "/m/b.y4m"], ready);
    let (id, _) = convert(&mut app);
    app.event(
        id,
        Event::Working {
            partial: WirePath::from_path(&abs("/m/.a.ivf.Xy12Z9.partial")),
        },
    );
    // Recorded once; a second announcement changes nothing.
    app.event(
        id,
        Event::Working {
            partial: WirePath::from_path(&abs("/elsewhere/x")),
        },
    );
    let partial = app.state.items()[0]
        .attempt()
        .and_then(|a| a.partial.clone());
    assert_eq!(partial, Some(abs("/m/.a.ivf.Xy12Z9.partial")));
    let update = app.apply(Intent::Job {
        id,
        input: Input::WorkerEnded {
            detail: "signal: 9 (SIGKILL)".into(),
        },
    });
    assert!(update.effects.contains(&Effect::CleanPartial {
        item: app.state.items()[0].id(),
        partial: abs("/m/.a.ivf.Xy12Z9.partial"),
        destination: abs("/m/a.ivf"),
    }));
    assert_eq!(
        starts(&update.effects).len(),
        1,
        "a lost file does not stop the run"
    );
    let item = app.state.items()[0].id();
    app.apply(Intent::PartialCleaned {
        item,
        fate: PartialFate::Removed(abs("/m/.a.ivf.Xy12Z9.partial")),
    });
    assert!(matches!(
        app.state.items()[0].ending(),
        Some(Ending::Lost {
            partial: PartialFate::Removed(_),
            ..
        })
    ));
}

#[test]
fn shutdown_records_the_converting_file_as_interrupted_with_its_partial() {
    let mut app = App::new();
    app.add(&["/m/a.y4m", "/m/b.y4m"], ready);
    let (id, _) = convert(&mut app);
    app.event(
        id,
        Event::Working {
            partial: WirePath::from_path(&abs("/m/.a.ivf.Q1.partial")),
        },
    );
    app.state.interrupt_for_shutdown();
    assert_eq!(app.states(), [ItemState::Interrupted, ItemState::Waiting]);
    assert!(!app.state.running());
    assert_eq!(
        app.state.items()[0].ending(),
        Some(&Ending::Interrupted(Interruption {
            publishing: false,
            cause: Cause::AppClosed,
            destination_present: false,
            partial: PartialFate::Pending(abs("/m/.a.ivf.Q1.partial")),
        }))
    );
}

// The queue itself.

#[test]
fn files_are_read_one_at_a_time_in_order() {
    let mut app = App::new();
    let update = app.apply(Intent::FilesOpened(vec![abs("/m/a.y4m"), abs("/m/b.y4m")]));
    let probes = |u: &Update| {
        u.effects
            .iter()
            .filter_map(|e| match e {
                Effect::Probe { request, .. } => Some(request.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let first = probes(&update);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].path, abs("/m/a.y4m"));
    assert_eq!(probes(&app.apply(Intent::RowSelected(1))), []);
    let next = app.apply(Intent::ProbeAnswered {
        id: first[0].id,
        result: Ok(facts(None)),
    });
    assert_eq!(probes(&next)[0].path, abs("/m/b.y4m"));
}

#[test]
fn a_file_already_waiting_is_not_added_twice() {
    let mut app = App::new();
    app.add(&["/m/a.y4m", "/m/a.y4m", "/m/b.y4m"], ready);
    assert_eq!(app.state.items().len(), 2);
    app.add(&["/m/b.y4m"], ready);
    assert_eq!(app.state.items().len(), 2);
}

#[test]
fn destinations_are_unique_in_the_queue_and_on_disk() {
    let mut app = App::new();
    app.disk.0.push(abs("/m/c.ivf"));
    app.add(&["/m/a.mp4", "/m/a.mkv", "/m/a.mov", "/m/c.y4m"], ready);
    let dests: Vec<PathBuf> = app
        .state
        .items()
        .iter()
        .map(|i| i.destination().path.clone())
        .collect();
    assert_eq!(
        dests,
        [
            abs("/m/a.ivf"),
            abs("/m/a (2).ivf"),
            abs("/m/a (3).ivf"),
            abs("/m/c (2).ivf"),
        ]
    );
    // A chosen destination never collides with another file's either.
    app.apply(Intent::RowSelected(3));
    app.apply(Intent::ChooseDestinationRequested);
    app.apply(Intent::DestinationPicked(Some(abs("/m/a.ivf"))));
    assert_eq!(app.state.items()[3].destination().path, abs("/m/a (4).ivf"));
    // Switching the kind keeps them unique.
    app.apply(Intent::RowSelected(0));
    app.apply(Intent::OutputSelected(1));
    let dests: Vec<PathBuf> = app
        .state
        .items()
        .iter()
        .map(|i| i.destination().path.clone())
        .collect();
    assert_eq!(
        dests,
        [
            abs("/m/a.obu"),
            abs("/m/a (2).obu"),
            abs("/m/a (3).obu"),
            abs("/m/a (4).ivf"),
        ]
    );
}

#[test]
fn retry_picks_a_new_name_when_the_destination_now_exists() {
    let mut app = App::new();
    app.add(&["/m/a.y4m"], ready);
    let (id, original) = convert(&mut app);
    app.fail(id, FailureCode::DestinationExists);
    app.apply(Intent::PresetEdited(1));
    app.disk.0.push(abs("/m/a.ivf"));
    let retry = app.apply(Intent::RowAction(0));
    app.answer(retry, ready);
    let (_, job) = only_start(&app.apply(Intent::ConvertRequested).effects);
    assert_eq!(dest_of(&job), abs("/m/a (2).ivf"));
    assert_eq!(job.settings, original.settings);
    assert_eq!(job.fingerprint, original.fingerprint);
}

#[test]
fn removing_and_clearing_never_touch_the_converting_file() {
    let mut app = App::new();
    app.add(&["/m/a.y4m", "/m/b.y4m", "/m/c.y4m"], ready);
    let (a, _) = convert(&mut app);
    let (b, _) = only_start(&app.complete(a).effects);
    app.apply(Intent::RemoveRequested(1));
    app.apply(Intent::ClearFinished);
    assert_eq!(app.states(), [ItemState::Running, ItemState::Waiting]);
    app.apply(Intent::RemoveRequested(1));
    assert_eq!(app.states(), [ItemState::Running]);
    assert_eq!(app.state.mode(), Mode::Single);
    app.complete(b);
    assert!(!app.state.running());
}

#[test]
fn a_restored_queue_waits_for_continue() {
    let mut app = App::new();
    app.add(&["/m/a.y4m", "/m/b.y4m", "/m/c.y4m"], ready);
    let (a, _) = convert(&mut app);
    app.event(
        a,
        Event::Working {
            partial: WirePath::from_path(&abs("/m/.a.ivf.R4nd0m.partial")),
        },
    );
    let saved = journal::snapshot(
        app.state.next_id(),
        app.state.option_request().clone(),
        app.state.items(),
        journal::JournalView::default(),
    );
    // The app stops; at the next start, the temporary file is still there.
    let mut disk = Files::default();
    disk.0.push(abs("/m/.a.ivf.R4nd0m.partial"));
    let recovered = recovery::recover(saved, &disk);
    assert_eq!(recovered.cleanups.len(), 1);
    let mut state = AppState::new(Startup {
        recovered: Some(recovered),
        problem: None,
    });
    assert_eq!(
        state.items().iter().map(Item::state).collect::<Vec<_>>(),
        [
            ItemState::Interrupted,
            ItemState::Waiting,
            ItemState::Waiting
        ]
    );
    assert_eq!(
        state.restored(),
        Some(&Restored {
            files: 3,
            interrupted: 1,
            dropped: 0
        })
    );
    assert!(state.paused());
    assert!(!state.running());
    // Nothing starts by itself, whatever the engine and the disk say.
    let now = Instant::now();
    let none = Files::default();
    let update = state.apply(Intent::EngineAnswered(Ok(capabilities(true))), now, &none);
    assert_eq!(starts(&update.effects), []);
    assert_eq!(update.effects, []);
    // Ids continue after the old ones.
    let added = state.apply(Intent::FilesOpened(vec![abs("/m/d.y4m")]), now, &none);
    assert!(matches!(added.effects.as_slice(), [Effect::Probe { .. }]));
    assert_eq!(state.items()[3].id(), 4);
    state.apply(Intent::RemoveRequested(3), now, &none);
    let update = state.apply(Intent::ConvertRequested, now, &none);
    let (_, job) = only_start(&update.effects);
    assert_eq!(source_of(&job), abs("/m/b.y4m"));
    assert_eq!(state.restored(), None);
    assert_eq!(state.items()[0].state(), ItemState::Interrupted);
}

#[test]
fn the_journal_round_trips_every_settled_state() {
    let mut app = App::new();
    app.add(&["/m/a.mp4", "/m/b.y4m", "/m/c.y4m", "/m/d.y4m"], |p| {
        if p.extension().is_some_and(|e| e == "mp4") {
            Ok(rich(0))
        } else {
            Ok(facts(Some(3)))
        }
    });
    app.add(&["/m/e.y4m"], |_| {
        Err(ProbeFailure {
            code: Some(FailureCode::UnsupportedSource),
            message: "HDR".into(),
        })
    });
    app.apply(Intent::QualitySelected(1));
    app.apply(Intent::ConvertRequested);
    let (a, _) = only_start(&app.apply(Intent::OmissionsConfirmed).effects);
    let (b, _) = only_start(&app.complete(a).effects);
    let (c, _) = only_start(&app.fail(b, FailureCode::Output).effects);
    app.apply(Intent::CancelRequested);
    app.event(c, Event::Cancelled);
    let saved = journal::snapshot(
        app.state.next_id(),
        app.state.option_request().clone(),
        app.state.items(),
        journal::JournalView::default(),
    );
    let bytes = journal::encode(&saved).unwrap_or_default();
    let parsed: journal::Journal =
        serde_json::from_slice(&bytes).unwrap_or_else(|e| unreachable!("{e}"));
    assert_eq!(parsed, saved);
    let recovered = recovery::recover(parsed, &Files::default());
    assert_eq!(recovered.cleanups, []);
    assert_eq!(&recovered.settings, app.state.option_request());
    let restored = AppState::new(Startup {
        recovered: Some(recovered),
        problem: None,
    });
    for (before, after) in app.state.items().iter().zip(restored.items()) {
        assert_eq!(after.state(), before.state());
        assert_eq!(after.id(), before.id());
        assert_eq!(after.destination(), before.destination());
        assert_eq!(after.facts(), before.facts());
        assert_eq!(after.acknowledgement(), before.acknowledgement());
        assert_eq!(after.ending(), before.ending());
        assert_eq!(
            after.attempt().map(|a| &a.job),
            before.attempt().map(|a| &a.job)
        );
    }
    assert_eq!(
        restored.items().iter().map(Item::state).collect::<Vec<_>>(),
        [
            ItemState::Completed,
            ItemState::Failed,
            ItemState::Cancelled,
            ItemState::Waiting,
            ItemState::Unreadable
        ]
    );
}

// Video track choice and FFmpeg availability.

#[test]
fn the_track_picker_lists_movie_tracks_only_and_only_when_there_is_a_choice() {
    let mut app = App::new();
    app.add(&["/m/a.y4m"], ready);
    assert_eq!(app.state.track_picker(), None, "one track is no choice");
    let mut app = App::new();
    app.add(&["/m/trip.mp4"], |_| Ok(rich(1)));
    assert_eq!(
        app.state.track_picker(),
        Some(TrackPicker {
            tracks: vec![track(0, false), track(1, false)],
            selected: Some(1),
            attached: vec![track(4, true)],
        })
    );
}

#[test]
fn choosing_a_track_re_probes_that_stream_and_keeps_the_destination() {
    let mut app = App::new();
    app.add(&["/m/trip.mp4"], with_audio);
    app.apply(Intent::ChooseDestinationRequested);
    app.apply(Intent::DestinationPicked(Some(abs("/out/t.ivf"))));
    for same in [0, 2, -1] {
        assert_eq!(app.apply(Intent::VideoTrackSelected(same)).effects, []);
    }
    let update = app.apply(Intent::VideoTrackSelected(1));
    let Some(Effect::Probe { request, .. }) = update.effects.first().cloned() else {
        unreachable!("{update:?}")
    };
    assert_eq!(request.video_stream, Some(1));
    // While reading, the last facts stay visible and the picker shows the
    // requested track, but Convert waits.
    assert_eq!(app.state.track_picker().and_then(|p| p.selected), Some(1));
    assert_eq!(
        app.state.blocker().map(|b| b.text).as_deref(),
        Some("Reading the source…")
    );
    app.answer(update, |_| Ok(rich(1)));
    assert_eq!(app.state.items()[0].destination().path, abs("/out/t.ivf"));
    app.apply(Intent::ConvertRequested);
    let (_, job) = only_start(&app.apply(Intent::OmissionsConfirmed).effects);
    assert_eq!(job.video_stream, 1);
}

#[test]
fn a_track_that_cannot_be_read_still_lets_the_user_choose_another() {
    let mut app = App::new();
    app.add(&["/m/trip.mp4"], with_audio);
    let update = app.apply(Intent::VideoTrackSelected(1));
    app.answer(update, |_| {
        Err(ProbeFailure {
            code: Some(FailureCode::UnsupportedSource),
            message: "Interlaced video is not supported".into(),
        })
    });
    assert_eq!(app.states(), [ItemState::Unreadable]);
    assert_eq!(app.state.track_picker().and_then(|p| p.selected), Some(1));
    let back = app.apply(Intent::VideoTrackSelected(0));
    assert!(matches!(
        back.effects.as_slice(),
        [Effect::Probe { request, .. }] if request.video_stream == Some(0)
    ));
}

#[test]
fn a_missing_media_tool_blocks_only_files_that_need_it() {
    let mut app = App::new();
    app.apply(Intent::EngineAnswered(Ok(capabilities(false))));
    app.add(&["/m/a.mp4"], |_| {
        Err(ProbeFailure {
            code: Some(FailureCode::MediaToolMissing),
            message: "this build has no FFmpeg".into(),
        })
    });
    assert_eq!(
        app.state.blocker(),
        Some(Blocker {
            text: "This build converts Y4M files only.".into(),
            severity: Severity::Problem
        })
    );
    app.apply(Intent::RemoveRequested(0));
    app.add(&["/m/a.y4m"], ready);
    assert_eq!(
        starts(&app.apply(Intent::ConvertRequested).effects).len(),
        1
    );
}

#[test]
fn the_selected_row_falls_back_to_what_can_be_edited() {
    let mut app = App::new();
    app.add(&["/m/a.y4m", "/m/b.y4m"], ready);
    let (a, _) = convert(&mut app);
    assert_eq!(
        app.state.selected_index(),
        Some(1),
        "the waiting file first"
    );
    app.complete(a);
    app.apply(Intent::RowSelected(0));
    assert_eq!(app.state.selected_index(), Some(0));
    app.apply(Intent::RowSelected(9));
    assert_eq!(
        app.state.selected_index(),
        Some(0),
        "out of range changes nothing"
    );
    app.apply(Intent::RemoveRequested(0));
    assert_eq!(app.state.selected_index(), Some(0));
}

// The workspace shell.

#[test]
fn a_workspace_switch_never_touches_the_queue_and_compare_is_refused() {
    let mut app = App::new();
    app.add(&["/m/a.y4m", "/m/b.y4m"], ready);
    convert(&mut app);
    assert_eq!(app.state.workspace(), Workspace::Encode);
    let before = app.state.clone();
    let update = app.apply(Intent::WorkspaceRequested(Workspace::Compare));
    assert_eq!(update.effects, []);
    assert_eq!(app.state, before, "Compare has no flow yet");

    let update = app.apply(Intent::WorkspaceRequested(Workspace::Analyze));
    assert_eq!(update.effects, []);
    assert_eq!(app.state.workspace(), Workspace::Analyze);
    // Only the workspace changed: the running file, queue and settings
    // are the same values.
    assert_eq!(app.state.items(), before.items());
    assert_eq!(app.state.settings(), before.settings());
    assert_eq!(
        app.state.active().map(Item::id),
        before.active().map(Item::id)
    );
    app.apply(Intent::WorkspaceRequested(Workspace::Encode));
    assert_eq!(app.state, before);
}

/// Opening files to convert shows Encode, from any workspace.
#[test]
fn opening_to_convert_shows_encode() {
    let mut app = App::new();
    app.apply(Intent::WorkspaceRequested(Workspace::Analyze));
    let update = app.apply(Intent::AddRequested);
    assert_eq!(update.effects, [Effect::ShowOpenDialog]);
    assert_eq!(app.state.workspace(), Workspace::Encode);
}

/// Jobs shows every conversion of the queue with its own state: the
/// converting file, the files queued behind it in the run, and how each
/// earlier attempt ended. A file that only waits has no job.
#[test]
fn jobs_list_each_conversion_with_its_own_state() {
    use crate::analysis::Analysis;
    use crate::jobs::{self, State, Work};
    let list = |app: &App| -> Vec<(PathBuf, State)> {
        jobs::list(&app.state, &Analysis::new())
            .iter()
            .map(|job| match job.work {
                Work::Convert(item) => (item.source().to_path_buf(), job.state),
                Work::Inspect(..) => unreachable!("no documents"),
            })
            .collect()
    };
    let mut app = App::new();
    app.add(&["/m/a.y4m", "/m/b.y4m", "/m/c.y4m"], ready);
    assert_eq!(list(&app), []);
    let (a, _) = convert(&mut app);
    assert_eq!(
        list(&app),
        [
            (abs("/m/a.y4m"), State::Running),
            (abs("/m/b.y4m"), State::Queued),
            (abs("/m/c.y4m"), State::Queued),
        ]
    );
    // The bar shows only once the engine counts frames.
    let none = Analysis::new();
    let job = jobs::list(&app.state, &none)[0];
    assert_eq!(crate::present::jobs::line(&job).fraction, None);
    app.event(
        a,
        Event::Progress {
            progress: studio_domain::job::Progress {
                frames_read: 45,
                frames_encoded: 45,
                bytes_written: 9,
                frames_coded_milli: 45_000,
            },
        },
    );
    let job = jobs::list(&app.state, &none)[0];
    assert_eq!(crate::present::jobs::line(&job).fraction, Some(0.5));
    let (b, _) = only_start(&app.complete(a).effects);
    app.fail(b, FailureCode::Output);
    let c = app.state.items()[2]
        .attempt()
        .map_or(0, |attempt| attempt.request);
    // Cancel: cancelling, and the run stops after it.
    app.apply(Intent::CancelRequested);
    assert_eq!(list(&app)[2], (abs("/m/c.y4m"), State::Cancelling));
    app.event(c, Event::Cancelled);
    assert_eq!(
        list(&app),
        [
            (abs("/m/a.y4m"), State::Completed),
            (abs("/m/b.y4m"), State::Failed),
            (abs("/m/c.y4m"), State::Cancelled),
        ]
    );
    let (label, _) = crate::present::jobs::entry(&jobs::list(&app.state, &Analysis::new()));
    assert_eq!(label, "Jobs");
    // Closing while one converts: interrupted, never resumed.
    let retry = app.apply(Intent::RowAction(2));
    app.answer(retry, ready);
    let (d, _) = convert(&mut app);
    assert!(d > c);
    app.state.interrupt_for_shutdown();
    assert_eq!(list(&app)[2], (abs("/m/c.y4m"), State::Interrupted));
}

#[test]
fn related_replay_checks_the_current_engine_and_keeps_the_original_record() {
    use studio_domain::protocol::OptionsIdentity;

    let mut app = App::new();
    let identity = |revision: &str| OptionsIdentity {
        schema: 1,
        codec_revision: revision.into(),
    };
    app.apply(Intent::EngineAnswered(Ok(Capabilities {
        options: Some(identity("original")),
        ..capabilities(true)
    })));
    app.add(&["/m/replay.y4m"], ready);
    app.apply(Intent::OptionEdited {
        key: "keyint".into(),
        value: Some(OptionValue::Integer(3)),
    });
    let (id, original) = only_start(&app.apply(Intent::ConvertRequested).effects);
    let policy = |value| EffectiveOption {
        key: "keyint".into(),
        requested: Some(OptionValue::Integer(3)),
        value: OptionValue::Integer(value),
        origin: OptionValue::Choice("explicit".into()),
        screen: None,
        screen_origin: None,
    };
    app.apply(Intent::JobPolicy {
        id,
        values: vec![policy(3)],
    });
    app.complete(id);
    let original_item = app.state.items()[0].clone();

    app.apply(Intent::EngineAnswered(Ok(Capabilities {
        options: Some(identity("current")),
        ..capabilities(true)
    })));
    app.apply(Intent::OptionEdited {
        key: "keyint".into(),
        value: Some(OptionValue::Integer(7)),
    });
    let prepare = |app: &mut App, action| {
        let effects = app
            .apply(Intent::PrepareRelated {
                item: original_item.id(),
                action,
            })
            .effects;
        match effects.as_slice() {
            [Effect::PrepareRelated { request, job }] => (*request, (**job).clone()),
            _ => unreachable!("A related preparation should start one check."),
        }
    };
    let (request, job) = prepare(&mut app, RelatedAction::Reencode);
    assert_eq!(job.settings, original.settings);
    assert_eq!(job.identity, Some(identity("current")));
    let mut changed = facts(None);
    changed.fingerprint.len += 1;
    app.apply(Intent::RelatedPrepared {
        request,
        result: Ok((Box::new(changed), vec![policy(4)])),
    });
    assert!(matches!(
        app.state.related(),
        Some(RelatedPreview::Failed(_))
    ));
    assert_eq!(app.state.items(), std::slice::from_ref(&original_item));
    app.apply(Intent::RelatedDeclined);

    let (request, _) = prepare(&mut app, RelatedAction::CloneSettings);
    app.apply(Intent::RelatedPrepared {
        request,
        result: Ok((Box::new(facts(None)), vec![policy(4)])),
    });
    assert!(
        matches!(app.state.related(), Some(RelatedPreview::Ready { detail, .. }) if
        detail.contains("encoder revision") && detail.contains("3 → 4"))
    );
    assert_eq!(
        app.state.option_request().values.get("keyint"),
        Some(&OptionValue::Integer(7))
    );
    app.apply(Intent::RelatedApproved);
    assert_eq!(app.state.option_request(), &original.settings);
    assert_eq!(app.state.items(), std::slice::from_ref(&original_item));

    let (request, _) = prepare(&mut app, RelatedAction::Reencode);
    app.apply(Intent::RelatedPrepared {
        request,
        result: Ok((Box::new(facts(None)), vec![policy(4)])),
    });
    assert_eq!(app.state.items().len(), 1);
    let (_, replay) = only_start(&app.apply(Intent::RelatedApproved).effects);
    assert_eq!(replay.settings, original.settings);
    assert_ne!(replay.destination, original.destination);
    assert_eq!(app.state.items()[0], original_item);
    let attempt = app.state.items()[1].attempt();
    assert_eq!(attempt.map(|a| a.policy.as_slice()), Some(&[policy(4)][..]));
}
