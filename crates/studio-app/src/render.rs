// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Pushes an [`AppState`] into the window. The window only ever receives
//! display strings and plain flags; [`render`] is used for every change of
//! phase, [`render_counters`] for progress ticks.

mod analysis;
pub(crate) mod block_motion;
mod block_overlay;
pub(crate) mod blocks;
mod evidence;
pub(crate) mod layers;
mod references;
pub(crate) mod timeline;

use std::time::Instant;

use slint::{ComponentHandle, Image, Model, ModelRc, SharedString, VecModel};
use studio_domain::job::FailureCode;
use studio_domain::settings::OutputKind;

use crate::activity::{self, Phase};
use crate::engine::Picture;
use crate::jobs::Job;
use crate::present::queue::{self as text, RowKind};
use crate::present::{self, Tone as ToneText};
use crate::preview::{Dirty, Preview, StageState as Stage};
use crate::queue::{Ending, Item, ItemState};
use crate::simple::{self, Priority, Quality};
use crate::state::{AppState, Mode, RelatedPreview, Severity};
use crate::ui::{
    AppWindow, BlockMotionView, CheckRow, Fact, JobRow, JobView, Notice, Playback, QueueRow,
    RowState, SourceView, StageState, Tone, ViewMode, WorkspaceView,
};
use crate::workspace::Workspace;

pub use analysis::render_analysis;

/// How many files the confirmation names before "and N more files".
const SHEET_FILES: usize = 5;

/// Pushes the whole state into the window.
///
/// Public so renderer snapshots can show a state without running workers.
pub fn render(window: &AppWindow, state: &AppState, now: Instant) {
    let workspace = state.workspace();
    window.set_workspace(match workspace {
        Workspace::Analyze => WorkspaceView::Analyze,
        Workspace::Encode | Workspace::Compare => WorkspaceView::Encode,
    });
    window.set_workspace_title(workspace.label().into());
    window.set_workspace_index(
        Workspace::OFFERED
            .iter()
            .position(|w| *w == workspace)
            .and_then(|i| i32::try_from(i).ok())
            .unwrap_or(0),
    );
    window.set_can_analyze_result(state.analyzable().is_some());
    let mode = state.mode();
    window.set_view_mode(match mode {
        Mode::Empty => ViewMode::Empty,
        Mode::Single => ViewMode::Single,
        Mode::List => ViewMode::List,
    });
    render_settings(window, state);
    render_notices(window, state);
    window.set_can_add(state.confirmation().is_none());
    window.set_card_replaces(state.active().is_none());
    window.set_list_title(text::files(state.items().len()).into());
    window.set_rows(model(state.items().iter().map(row).collect()));
    window.set_selected_row(
        state
            .chosen_index()
            .and_then(|i| i32::try_from(i).ok())
            .unwrap_or(-1),
    );
    let selected = state.selected();
    render_source(window, state, selected);
    window.set_empty_caption(present::empty_caption(state.capabilities()).into());
    window.set_media_tool_line(present::media_tool(state.capabilities()).into());
    render_confirmation(window, state);
    render_related(window, state);
    render_destination(window, state, selected);
    render_primary(window, state);
    render_detail(window, mode, selected);

    match state.focus() {
        Some(item) if item.state().is_active() || mode == Mode::Single => {
            render_job(window, state, item, now);
        }
        _ => {
            window.set_job_view(JobView::None);
            window.set_run_position(SharedString::new());
        }
    }
}

fn render_related(window: &AppWindow, state: &AppState) {
    window.set_job_can_generate_pass(state.can_generate_pass());
    window.set_job_actions_enabled(
        state.active().is_none()
            && state.related().is_none()
            && state.confirmation().is_none()
            && state
                .capabilities()
                .is_some_and(|cap| cap.options.is_some()),
    );
    let (title, detail, ready, failed) = match state.related() {
        Some(RelatedPreview::Checking) => ("Checking job compatibility…", "", false, false),
        Some(RelatedPreview::Cancelling) => {
            ("Finishing the compatibility check…", "", false, false)
        }
        Some(RelatedPreview::Ready { title, detail }) => {
            (title.as_str(), detail.as_str(), true, false)
        }
        Some(RelatedPreview::Failed(detail)) => {
            ("Cannot use these settings", detail.as_str(), false, true)
        }
        None => ("", "", false, false),
    };
    window.set_job_related_title(title.into());
    window.set_job_related_detail(detail.into());
    window.set_job_related_ready(ready);
    window.set_job_related_failed(failed);
}

fn render_settings(window: &AppWindow, state: &AppState) {
    let view = state.settings_view();
    let settings = view.settings;
    window.set_output_index(AppState::output_index(settings.output));
    window.set_preset(i32::from(settings.preset.get()));
    window.set_crf(i32::from(settings.crf.get()));
    window.set_rate_overridden(view.quality_is_overridden());
    window.set_quality_index(simple::index(&Quality::ALL, view.quality()));
    let priority = view.priority();
    window.set_priority_index(simple::index(&Priority::ALL, priority));
    let priority_caption = if view.priority_is_overridden() {
        "Custom settings from Advanced."
    } else {
        priority.map_or("A speed preset set in Advanced.", Priority::description)
    };
    window.set_priority_caption(priority_caption.into());
    window.set_advanced_custom(!state.visible_option_request().values.is_empty());
    window.set_locked(view.read_only);
    let note = match (state.mode(), view.read_only, state.selected()) {
        (Mode::List, true, Some(item)) => text::settings_note(item),
        _ => String::new(),
    };
    window.set_settings_note(note.into());
}

fn render_notices(window: &AppWindow, state: &AppState) {
    let mut notices = Vec::new();
    if let Some(reason) = state.save_error() {
        notices.push(notice(text::save_failed(reason), ToneText::Warning));
    }
    if let Some(problem) = state.journal_problem() {
        let (line, t) = text::journal_problem(problem);
        notices.push(notice(line, t));
    }
    if let Some(restored) = state.restored() {
        notices.push(notice(text::restored(restored), ToneText::Neutral));
    }
    window.set_notices(model(notices));
}

fn notice(line: String, t: ToneText) -> Notice {
    Notice {
        text: line.into(),
        tone: tone(t),
    }
}

fn row(item: &Item) -> QueueRow {
    let (kind, state_text) = text::row_state(item);
    let (action, action_description) = text::row_action(item);
    let fraction = item.activity().and_then(activity::Activity::fraction);
    QueueRow {
        name: present::file_name(item.source()).into(),
        line: text::row_line(item).into(),
        state: match kind {
            RowKind::Probing => RowState::Probing,
            RowKind::Waiting => RowState::Waiting,
            RowKind::Unreadable => RowState::Unreadable,
            RowKind::Converting => RowState::Converting,
            RowKind::Done => RowState::Done,
            RowKind::Failed => RowState::Failed,
            RowKind::Cancelled => RowState::Cancelled,
            RowKind::Interrupted => RowState::Interrupted,
        },
        state_text: state_text.into(),
        action: action.into(),
        action_description: action_description.into(),
        progress_known: fraction.is_some(),
        progress: fraction.unwrap_or(0.0),
        accessible: text::row_accessible(item).into(),
    }
}

/// The selected file's name, facts and track choice. In one-file mode
/// that is the one file, exactly as before the queue existed.
fn render_source(window: &AppWindow, state: &AppState, selected: Option<&Item>) {
    let (name, path) = selected.map_or_else(Default::default, |i| {
        (
            present::file_name(i.source()),
            i.source().display().to_string(),
        )
    });
    window.set_source_name(name.into());
    window.set_source_path(path.into());
    let facts = selected.and_then(Item::facts);
    window.set_source_line(present::source_line(facts, selected.and_then(Item::size)).into());
    let view = match selected.map(Item::state) {
        None => SourceView::None,
        Some(ItemState::Probing) => SourceView::Probing,
        Some(ItemState::Unreadable) => SourceView::Unreadable,
        Some(_) => SourceView::Ready,
    };
    window.set_source_view(view);
    let failure = selected
        .filter(|i| i.state() == ItemState::Unreadable)
        .and_then(Item::probe_failure);
    let (problem, problem_tone) = failure.map_or((String::new(), Tone::Neutral), |f| {
        let t = if f.code == Some(FailureCode::MediaToolMissing) {
            Tone::Warning
        } else {
            Tone::Error
        };
        (present::probe_problem(f), t)
    });
    window.set_source_problem(problem.into());
    window.set_source_problem_tone(problem_tone);
    window.set_has_source_facts(facts.is_some());
    let rows: Vec<Fact> = facts
        .map_or_else(Vec::new, present::facts)
        .into_iter()
        .map(|(label, value)| Fact {
            label: label.into(),
            value: value.into(),
        })
        .collect();
    window.set_source_facts(model(rows));
    let summary = facts.map_or_else(String::new, |f| {
        present::omission_summary(&f.omissions, crate::source::attached_pictures(f))
    });
    // One file shows what is left out under its card, always; a row says
    // "video only" and Advanced names what for the selected file.
    let (summary, listed) = match state.mode() {
        Mode::List => (String::new(), summary),
        Mode::Empty | Mode::Single => (summary, String::new()),
    };
    let disclosures: Vec<SharedString> = std::iter::once(listed)
        .filter(|s| !s.is_empty())
        .chain(
            facts
                .map_or(&[][..], |f| f.disclosures.as_slice())
                .iter()
                .map(|&d| present::disclosure(d).to_owned()),
        )
        .map(SharedString::from)
        .collect();
    window.set_source_disclosures(model(disclosures));
    window.set_omission_summary(summary.into());

    let picker = state.track_picker();
    window.set_has_track_picker(picker.is_some());
    let (labels, index, note) = picker.map_or_else(
        || (Vec::new(), -1, String::new()),
        |p| {
            (
                p.tracks
                    .iter()
                    .map(|t| SharedString::from(present::track(t)))
                    .collect(),
                p.selected.and_then(|i| i32::try_from(i).ok()).unwrap_or(-1),
                present::attached_note(&p.attached),
            )
        },
    );
    window.set_track_labels(model(labels));
    window.set_track_index(index);
    window.set_track_note(note.into());
}

/// Shows or closes the video-only confirmation to match the state.
fn render_confirmation(window: &AppWindow, state: &AppState) {
    match state.confirmation() {
        Some(c) => {
            let (body, items) = match c.asked.as_slice() {
                [one] => (
                    present::omission_question(&one.omissions, one.attached_pictures, c.output),
                    Vec::new(),
                ),
                many => (
                    text::omission_question_many(many.len(), c.output),
                    text::omission_lines(many, SHEET_FILES),
                ),
            };
            window.set_sheet_body(body.into());
            window.set_sheet_items(model(items.into_iter().map(SharedString::from).collect()));
            if !window.get_sheet_open() {
                window.invoke_show_sheet();
            }
        }
        None if window.get_sheet_open() => window.invoke_close_sheet(),
        None => {}
    }
}

fn render_destination(window: &AppWindow, state: &AppState, selected: Option<&Item>) {
    let destination = selected.map(Item::destination);
    window.set_has_destination(destination.is_some());
    window.set_destination_editable(state.destination_editable());
    window.set_destination_path(
        destination
            .map_or_else(String::new, |d| d.path.display().to_string())
            .into(),
    );
    window.set_destination_help(
        match destination {
            None => "",
            Some(d) if d.chosen => "Chosen by you. An existing file is never replaced.",
            Some(_) => "Next to the source. An existing file is never replaced.",
        }
        .into(),
    );
    // The line above Convert names the one file's output; a list names
    // each in Advanced.
    let line = match (state.mode(), destination) {
        (Mode::Single, Some(d)) if d.chosen => format!("Saves video only to {}", d.path.display()),
        (Mode::Single, Some(d)) => format!(
            "Saves video only as {}, next to the source",
            present::file_name(&d.path)
        ),
        _ => String::new(),
    };
    window.set_destination_line(line.into());
    let editable = selected
        .is_some_and(|i| i.state() == ItemState::Waiting || i.state() == ItemState::Probing);
    window.set_destination_warning(
        if editable && destination.is_some_and(|d| d.exists) {
            "A file already exists here. Choose another name, or move that file away."
        } else {
            ""
        }
        .into(),
    );
}

fn render_primary(window: &AppWindow, state: &AppState) {
    let mode = state.mode();
    let blocker = state.blocker();
    window.set_can_encode(blocker.is_none());
    window.set_encode_status_tone(match blocker.as_ref().map(|b| b.severity) {
        Some(Severity::Problem) => Tone::Error,
        Some(Severity::Warning) => Tone::Warning,
        Some(Severity::Info) | None => Tone::Neutral,
    });
    let startable = state.startable_count();
    window.set_primary_text(text::primary(mode == Mode::Single, startable, state.paused()).into());
    // The restored-list notice already says what came back.
    let summary = if mode == Mode::List && !state.running() && state.restored().is_none() {
        text::summary(state.items())
    } else {
        String::new()
    };
    // After a run, the summary replaces "nothing waiting".
    let quiet = !summary.is_empty() && startable == 0;
    window.set_show_primary(mode != Mode::List || !quiet);
    window.set_can_clear_finished(
        mode == Mode::List
            && state
                .items()
                .iter()
                .any(|i| i.state() == ItemState::Completed),
    );
    window.set_summary_text(summary.into());
    let status = match blocker {
        Some(_) if quiet => String::new(),
        Some(b) => b.text,
        None => match (mode, state.selected()) {
            (Mode::Single, Some(item)) => format!(
                "Ready to convert to {}",
                present::file_name(&item.destination().path)
            ),
            _ => String::new(),
        },
    };
    window.set_encode_status(status.into());
}

/// The selected row's result, under Advanced, in the list only (one file
/// shows it in the activity area instead).
fn render_detail(window: &AppWindow, mode: Mode, selected: Option<&Item>) {
    let item = selected.filter(|i| mode == Mode::List && i.ending().is_some());
    let (headline, t) = item.map_or((String::new(), ToneText::Neutral), text::headline);
    window.set_detail_headline(headline.into());
    window.set_detail_tone(tone(t));
    window.set_detail_text(
        item.map_or_else(String::new, |i| text::detail(i, "Retry"))
            .into(),
    );
    window.set_detail_checks(model(item.map_or_else(Vec::new, checks)));
}

fn checks(item: &Item) -> Vec<CheckRow> {
    match item.ending() {
        Some(Ending::Completed(report)) => report
            .checks
            .iter()
            .map(|c| {
                let (name, outcome, detail, t) = present::check(c);
                CheckRow {
                    name: name.into(),
                    outcome: outcome.into(),
                    detail: detail.into(),
                    tone: tone(t),
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The activity area for `item`: the converting file, or the one file.
fn render_job(window: &AppWindow, state: &AppState, item: &Item, now: Instant) {
    let view = match (item.state(), item.activity().map(activity::Activity::phase)) {
        (ItemState::Running | ItemState::Publishing, Some(Phase::Cancelling { .. })) => {
            JobView::Cancelling
        }
        (ItemState::Running | ItemState::Publishing, _) => JobView::Running,
        // While the one file is read again after it ended, what happened
        // last stays in view, as before there was a queue.
        _ => match item.ending() {
            Some(Ending::Completed(_)) if item.state() == ItemState::Completed => {
                JobView::Completed
            }
            Some(Ending::Completed(_)) | None => JobView::None,
            Some(Ending::Cancelled) => JobView::Cancelled,
            Some(Ending::Failed(_)) => JobView::Failed,
            Some(Ending::Lost { .. }) => JobView::Lost,
            Some(Ending::Interrupted(_)) => JobView::Interrupted,
        },
    };
    let (headline, headline_tone) = text::headline(item);
    window.set_job_view(view);
    window.set_job_headline(headline.into());
    window.set_job_tone(tone(headline_tone));
    let detail = if item.state().is_active() {
        item.activity().map_or_default(present::detail)
    } else {
        text::detail(item, "Convert")
    };
    window.set_job_detail(detail.into());
    window.set_job_checks(model(checks(item)));
    if item.state() != ItemState::Completed {
        window.set_reveal_error(SharedString::new());
    }
    let position = match state.run_position() {
        Some(p) if state.mode() == Mode::List => text::position(p),
        _ => String::new(),
    };
    window.set_run_position(position.into());
    render_counters(window, state, now);
}

/// Updates only the counters, the progress bar, the elapsed time and the
/// converting row's progress.
pub fn render_counters(window: &AppWindow, state: &AppState, now: Instant) {
    let Some(item) = state.focus() else {
        return;
    };
    if let Some(index) = state.items().iter().position(|i| i.id() == item.id()) {
        let rows = window.get_rows();
        if index < rows.row_count() {
            rows.set_row_data(index, row(item));
        }
    }
    let Some(activity) = item.activity() else {
        window.set_progress_known(false);
        window.set_elapsed_text(SharedString::new());
        return;
    };
    let fraction = activity.fraction();
    window.set_progress_known(fraction.is_some());
    window.set_progress_fraction(fraction.unwrap_or(0.0));
    window.set_progress_text(present::progress_text(activity).into());
    window.set_progress_line(present::counters(activity).into());
    window.set_elapsed_text(present::elapsed(activity.elapsed(now)).into());
}

/// Pushes what `dirty` names of the picture area and the row thumbnails.
/// The pixels are not copied: the image shares the picture's buffer.
///
/// Public so renderer snapshots can show pictures without an engine.
pub fn render_preview(window: &AppWindow, state: &AppState, preview: &Preview, dirty: Dirty) {
    if dirty.picture || dirty.view {
        let view = preview.view();
        let playback = window.global::<Playback>();
        if dirty.picture {
            playback.set_image(image(view.picture.as_ref()));
        }
        playback.set_state(match view.state {
            Stage::None => StageState::None,
            Stage::Loading => StageState::Loading,
            Stage::Picture => StageState::Picture,
            Stage::Unavailable => StageState::Unavailable,
        });
        playback.set_native(view.native);
        playback.set_badge(view.badge.into());
        playback.set_hint(view.hint.into());
        playback.set_message(view.message.into());
        playback.set_accessible(view.accessible.into());
        playback.set_transport(view.transport);
        playback.set_navigable(view.navigable);
        playback.set_frames(i32::try_from(view.frames).unwrap_or(i32::MAX));
        // Exact below 2^24 frames, which is over six days at 30 fps.
        #[allow(clippy::cast_precision_loss)]
        playback.set_position(view.position as f32);
        playback.set_frame_text(view.frame_text.into());
        playback.set_time_text(view.time_text.into());
        playback.set_can_play(view.can_play);
        playback.set_playing(view.playing);
        playback.set_play_note(view.play_note.into());
        playback.set_seek_error(view.seek_error.into());
        playback.set_can_compare(view.can_compare);
        playback.set_comparing(view.comparing);
        playback.set_can_native(view.can_native);
        playback.set_native_on(view.native_on);
        playback.set_decode_work(view.decode_work.into());
        render_viewer(&playback, view.viewer);
        let offset = |offset: Option<u64>| {
            offset
                .map_or_else(String::new, |offset| format!("byte 0x{offset:X}"))
                .into()
        };
        playback.set_output_unit(
            view.output
                .map_or_else(String::new, |event| {
                    format!("byte 0x{:X} ({} bytes)", event.unit_offset, event.unit_len)
                })
                .into(),
        );
        playback.set_output_coded(offset(view.output.and_then(|event| event.coded_offset)));
        playback.set_output_showing(offset(view.output.and_then(|event| event.showing_offset)));
        playback.set_output_sequence(offset(view.output.and_then(|event| event.sequence_offset)));
        playback.set_output_layers(
            view.output
                .map_or_else(String::new, |event| {
                    format!(
                        "Layers · extended {} · embedded {} · temporal {}",
                        event.extended_layer, event.embedded_layer, event.temporal_layer
                    )
                })
                .into(),
        );
    }
    if dirty.thumbs {
        window.set_row_thumbs(model(
            state
                .items()
                .iter()
                .map(|i| image(preview.thumbnail(i.id())))
                .collect(),
        ));
    }
    if dirty.view {
        let motion = window.global::<BlockMotionView>();
        let (picture, message) = match preview.reference_preview() {
            Some(Ok(picture)) => (Some(picture), ""),
            Some(Err(message)) => (None, message),
            None if preview.reference_preview_pending() => {
                (None, "Decoding the reference preview…")
            }
            None => (None, ""),
        };
        motion.set_preview_picture(image(picture));
        motion.set_preview_status(message.into());
    }
}

fn render_viewer(playback: &Playback<'_>, viewer: crate::preview::ViewerView) {
    playback.set_surface(viewer.surface);
    playback.set_chroma(viewer.chroma);
    playback.set_zoom(viewer.zoom);
    playback.set_magnification(viewer.magnification);
    playback.set_grain(viewer.grain);
    playback.set_viewer_geometry(viewer.geometry.into());
    playback.set_viewer_policy(viewer.policy.into());
    playback.set_sample_plane(viewer.sample_plane.into());
    playback.set_can_sample(viewer.can_sample);
    playback.set_sample_text(viewer.sample_text.into());
    playback.set_sample_x(
        viewer
            .sample_point
            .map_or_else(String::new, |p| p.x.to_string())
            .into(),
    );
    playback.set_sample_y(
        viewer
            .sample_point
            .map_or_else(String::new, |p| p.y.to_string())
            .into(),
    );
    playback.set_sample_copy(viewer.sample_copy.into());
}

fn image(picture: Option<&Picture>) -> Image {
    picture.map_or_else(Image::default, |p| Image::from_rgb8(p.pixels.clone()))
}

/// The labels of the output kinds, in [`OutputKind::ALL`] order.
#[must_use]
pub fn output_labels() -> ModelRc<SharedString> {
    model(OutputKind::ALL.iter().map(|k| k.label().into()).collect())
}

pub(crate) const fn tone(tone: ToneText) -> Tone {
    match tone {
        ToneText::Neutral => Tone::Neutral,
        ToneText::Success => Tone::Success,
        ToneText::Warning => Tone::Warning,
        ToneText::Error => Tone::Error,
    }
}

/// Pushes the Jobs entry and list. Rows that did not change are not
/// touched, so a focused Cancel keeps its focus while counts move.
///
/// Public so renderer snapshots and UI tests can show a state.
pub fn render_jobs(window: &AppWindow, jobs: &[Job<'_>]) {
    let (label, description) = present::jobs::entry(jobs);
    window.set_jobs_label(label.into());
    window.set_jobs_description(description.into());
    let rows: Vec<JobRow> = jobs
        .iter()
        .map(|job| {
            let line = present::jobs::line(job);
            let kind = line.kind;
            JobRow {
                accessible: format!("{kind} {}. {}. {}", line.name, line.state, line.detail).into(),
                kind: kind.into(),
                name: line.name.into(),
                state: line.state.into(),
                tone: tone(line.tone),
                detail: line.detail.into(),
                progress_known: line.fraction.is_some(),
                progress: line.fraction.unwrap_or(0.0),
                can_cancel: job.can_cancel(),
                can_show: job.can_show(),
                can_export: job.can_export(),
                can_remove: job.can_remove(),
                can_clone_settings: job.can_clone_settings(),
                can_reencode: job.can_reencode(),
                can_encode_with_pass: job.can_encode_with_pass(),
            }
        })
        .collect();
    let shown = window.get_jobs();
    if shown.row_count() == rows.len() {
        for (index, row) in rows.into_iter().enumerate() {
            if shown.row_data(index).as_ref() != Some(&row) {
                shown.set_row_data(index, row);
            }
        }
    } else {
        window.set_jobs(model(rows));
    }
}

pub(crate) fn model<T: Clone + 'static>(rows: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(rows))
}

/// Puts `rows` into `current`, a model made by [`model`], when it has as
/// many: only the rows that differ are set, so the window keeps its row
/// items and assistive technology is told only of the rows that changed. A
/// new model replaces it otherwise, and is returned to be set.
pub(crate) fn refill<T: Clone + PartialEq + 'static>(
    current: &ModelRc<T>,
    rows: Vec<T>,
) -> Option<ModelRc<T>> {
    if current.row_count() != rows.len() {
        return Some(model(rows));
    }
    for (at, row) in rows.into_iter().enumerate() {
        if current.row_data(at).as_ref() != Some(&row) {
            current.set_row_data(at, row);
        }
    }
    None
}
