// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The picture area's state machine, with the engine's answers written out.

use std::path::Path;
use std::time::Instant;

use studio_domain::job::{
    ChromaLayout, JobReport, Omissions, Rational, SourceColor, SourceFacts, SourceFingerprint,
    SourceReader, Stage, WirePath,
};
use studio_domain::preview::{
    ColorBasis, ColorMatrix, DocumentPictureInfo, MediaTime, PictureHeader, SamplePlane,
    SamplePoint, SampleRect,
};
use studio_domain::protocol::Event;

use super::*;
use crate::activity::Input as JobInput;
use crate::recovery::{Disk, Kind};
use crate::state::{AppState, Effect as StateEffect, Intent};
use crate::testing::abs;

const STAGE: PictureBox = PictureBox {
    width: 832,
    height: 468,
};
const THUMB: PictureBox = PictureBox {
    width: 128,
    height: 72,
};

fn source(name: &str) -> SourceRef {
    SourceRef {
        path: WirePath::from_path(&abs(&format!("/videos/{name}"))),
        fingerprint: SourceFingerprint {
            len: 1000,
            modified: Some((1_700_000_000, 0)),
        },
        video_stream: 0,
    }
}

fn source_subject(item: ItemId, name: &str) -> Subject {
    Subject::Source {
        item,
        source: source(name),
        frames: Some(300),
        size: (1920, 1080),
    }
}

fn result_subject(item: ItemId, name: &str) -> Subject {
    Subject::Result {
        item,
        path: WirePath::from_path(&abs(&format!("/videos/{name}.ivf"))),
        source: source(name),
        frames: Some(300),
        size: (1920, 1080),
    }
}

fn live_subject(item: ItemId, job: u64, name: &str) -> Subject {
    Subject::Live {
        item,
        job,
        source: source(name),
        frames: Some(300),
        size: (1920, 1080),
    }
}

fn stage(subject: Subject) -> Scene {
    Scene {
        stage: Some(subject),
        thumbs: Vec::new(),
    }
}

/// A sized preview showing `subject`, with the effects of getting there.
fn shown(subject: Subject) -> (Preview, Vec<Effect>) {
    let mut preview = Preview::new();
    none(preview.sync(stage(subject)));
    let effects = preview.input(Input::Resized {
        stage: STAGE,
        thumb: THUMB,
    });
    (preview, effects)
}

fn header(generation: u64, origin: PictureOrigin, frame: u64, w: u16, h: u16) -> PictureHeader {
    PictureHeader {
        generation,
        origin,
        frame,
        time: Some(MediaTime {
            num: frame * 1001,
            den: 30000,
        }),
        frame_count: Some(300),
        output: None,
        document_info: None,
        options: PictureOptions::default(),
        width: w,
        height: h,
        frame_width: 1920,
        frame_height: 1080,
        view: PictureView::Fit,
        basis: ColorBasis {
            matrix: ColorMatrix::Bt709,
            assumed: true,
        },
    }
}

fn picture(header: PictureHeader) -> Picture {
    let bytes = vec![0; header.payload_len().unwrap_or(0) as usize];
    Picture::new(header.generation, header, &bytes).unwrap_or_else(|| unreachable!())
}

/// The answer the engine gives to `effect`: the frame asked for (the poster
/// is frame 42), from the side asked for, as big as the box.
fn answer(effect: &Effect) -> Picture {
    let Command::Picture {
        generation,
        target,
        frame,
        view,
        options,
        size,
    } = &effect.command
    else {
        unreachable!("not a picture request: {effect:?}")
    };
    let origin = match target {
        PictureTarget::Source { .. } => PictureOrigin::Source,
        PictureTarget::Result { .. } => PictureOrigin::Result,
        PictureTarget::Document { .. } => PictureOrigin::Document,
    };
    let index = match frame {
        PictureFrame::Poster => 42,
        PictureFrame::Index { index } => *index,
    };
    picture(PictureHeader {
        view: *view,
        options: *options,
        ..header(*generation, origin, index, size.width, size.height)
    })
}

#[track_caller]
#[allow(clippy::needless_pass_by_value)]
fn none(effects: Vec<Effect>) {
    assert_eq!(effects, []);
}

fn only(effects: &[Effect]) -> &Effect {
    assert_eq!(effects.len(), 1, "{effects:?}");
    &effects[0]
}

fn requested(effect: &Effect) -> (PictureOrigin, PictureFrame, PictureView, PictureBox) {
    let picture = answer(effect);
    let Command::Picture { frame, size, .. } = effect.command else {
        unreachable!()
    };
    (picture.header.origin, frame, picture.header.view, size)
}

#[test]
fn a_selected_file_asks_for_its_poster_at_the_picture_area_size() {
    let (mut preview, effects) = shown(source_subject(1, "a.y4m"));
    let ask = only(&effects);
    assert_eq!(
        requested(ask),
        (
            PictureOrigin::Source,
            PictureFrame::Poster,
            PictureView::Fit,
            STAGE
        )
    );
    assert_eq!(ask.request_id, 1);
    assert_eq!(preview.view().state, StageState::Loading);
    none(preview.picture(answer(ask)));
    let view = preview.view();
    assert_eq!(view.state, StageState::Picture);
    assert_eq!(view.badge, "Original · colours assumed (BT.709)");
    assert_eq!(view.frame_text, "43 / 300");
    assert_eq!(view.time_text, "0:01.40");
    assert!(view.navigable && view.can_play && !view.can_compare);
}

#[test]
fn an_answer_to_an_older_generation_is_dropped() {
    let (mut preview, effects) = shown(source_subject(1, "a.y4m"));
    let ask = only(&effects).clone();
    let mut stale = answer(&ask);
    stale.header.generation = 0;
    none(preview.picture(stale));
    assert_eq!(preview.view().state, StageState::Loading);
    // Another file is selected before the first answers: its answer is
    // not shown, and the new file is asked for.
    let next = preview.sync(stage(source_subject(2, "b.y4m")));
    none(next);
    let next = preview.picture(answer(&ask));
    assert_eq!(preview.view().state, StageState::Loading);
    let ask = only(&next);
    assert!(ask.request_id > 1);
    assert!(matches!(
        &ask.command,
        Command::Picture { target: PictureTarget::Source { source: s }, .. } if *s == source("b.y4m")
    ));
}

#[test]
fn scrubbing_keeps_one_request_in_flight_and_asks_for_the_latest_frame() {
    let (mut preview, effects) = shown(source_subject(1, "a.y4m"));
    preview.picture(answer(only(&effects)));
    let first = preview.input(Input::Seek(100));
    let ask = only(&first).clone();
    assert_eq!(requested(&ask).1, PictureFrame::Index { index: 100 });
    for frame in [110, 120, 130, 140] {
        none(preview.input(Input::Seek(frame)));
    }
    none(preview.input(Input::Step(1)));
    // The answer for 100 is the newest one sent: it is shown on the way,
    // and the latest position (141) goes out next.
    let next = preview.picture(answer(&ask));
    assert_eq!(preview.view().picture.map(|p| p.header.frame), Some(100));
    assert_eq!(preview.view().position, 141);
    assert_eq!(requested(only(&next)).1, PictureFrame::Index { index: 141 });
}

#[test]
fn stepping_back_to_a_seen_frame_is_served_from_the_cache() {
    let (mut preview, effects) = shown(source_subject(1, "a.y4m"));
    preview.picture(answer(only(&effects)));
    let ask = preview.input(Input::Step(1));
    assert_eq!(requested(only(&ask)).1, PictureFrame::Index { index: 43 });
    preview.picture(answer(only(&ask)));
    // The poster was frame 42; asked for by index now, it is not cached
    // under that key, so it is asked once; 43 then comes from the cache.
    let back = preview.input(Input::Step(-1));
    preview.picture(answer(only(&back)));
    none(preview.input(Input::Step(1)));
    assert_eq!(preview.view().picture.map(|p| p.header.frame), Some(43));
}

#[test]
fn holding_shows_the_original_at_the_same_frame_and_view_and_release_returns() {
    let (mut preview, effects) = shown(result_subject(1, "a.y4m"));
    preview.picture(answer(only(&effects)));
    let view = preview.view();
    assert_eq!(view.badge, "AV2 result · colours assumed (BT.709)");
    assert!(view.can_compare);
    assert_eq!(view.hint, text::HOLD);
    let press = preview.input(Input::Compare(true));
    let ask = only(&press).clone();
    assert_eq!(
        requested(&ask),
        (
            PictureOrigin::Source,
            PictureFrame::Index { index: 42 },
            PictureView::Fit,
            STAGE
        )
    );
    // Until the original arrives the result stays, labelled as such.
    assert!(preview.view().badge.starts_with("AV2 result"));
    assert_eq!(preview.view().hint, text::LOADING_ORIGINAL);
    preview.picture(answer(&ask));
    assert!(preview.view().badge.starts_with("Original"));
    none(preview.input(Input::Compare(false)));
    let view = preview.view();
    assert!(view.badge.starts_with("AV2 result"));
    assert_eq!(view.picture.map(|p| p.header.frame), Some(42));
}

#[test]
fn an_original_that_arrives_after_release_is_not_shown_as_the_result() {
    let (mut preview, effects) = shown(result_subject(1, "a.y4m"));
    preview.picture(answer(only(&effects)));
    let ask = only(&preview.input(Input::Compare(true))).clone();
    none(preview.input(Input::Compare(false)));
    preview.picture(answer(&ask));
    assert!(preview.view().badge.starts_with("AV2 result"));
}

#[test]
fn the_cache_is_bounded_in_bytes_and_drops_the_oldest() {
    let mut cache = Cache::default();
    let big = |generation, frame| {
        picture(header(
            generation,
            PictureOrigin::Source,
            frame,
            studio_domain::preview::MAX_PICTURE_WIDTH,
            studio_domain::preview::MAX_PICTURE_HEIGHT,
        ))
    };
    let key = |index| Key {
        target: PictureTarget::Source {
            source: source("a.y4m"),
        },
        frame: PictureFrame::Index { index },
        view: PictureView::Fit,
        options: PictureOptions::default(),
        size: STAGE,
    };
    for frame in 0..40 {
        cache.put(key(frame), big(frame + 1, frame));
        assert!(cache.bytes <= CACHE_BYTES, "{} bytes", cache.bytes);
    }
    let each = big(0, 0).bytes();
    assert_eq!(cache.entries.len(), CACHE_BYTES / each);
    assert!(!cache.contains(&key(0)), "the oldest went first");
    assert!(cache.contains(&key(39)));
    // Using an entry keeps it.
    let survivor = 40 - CACHE_BYTES / each;
    assert!(cache.get(&key(survivor as u64)).is_some());
    cache.put(key(99), big(100, 99));
    assert!(cache.contains(&key(survivor as u64)));
    assert_eq!(
        cache.bytes,
        cache.entries.iter().map(|(_, p)| p.bytes()).sum::<usize>()
    );
}

#[test]
fn removing_a_file_drops_its_pictures_and_its_late_answer() {
    let (mut preview, effects) = shown(source_subject(1, "a.y4m"));
    preview.picture(answer(only(&effects)));
    assert!(preview.cached_bytes() > 0);
    for _ in 0..20 {
        let ask = preview.input(Input::Step(1));
        // Removed while its request is in flight.
        none(preview.sync(Scene::default()));
        assert_eq!(preview.cached_bytes(), 0);
        assert_eq!(preview.view().state, StageState::None);
        none(preview.picture(answer(only(&ask))));
        assert_eq!(preview.cached_bytes(), 0);
        assert!(preview.view().picture.is_none());
        // Added again: asked for afresh, shown once answered.
        let again = preview.sync(stage(source_subject(1, "a.y4m")));
        preview.picture(answer(only(&again)));
        assert_eq!(preview.view().state, StageState::Picture);
    }
    assert!(
        preview.cached_bytes()
            <= 2 * answer(&Effect {
                request_id: 0,
                command: Command::Picture {
                    generation: 0,
                    target: PictureTarget::Source {
                        source: source("a.y4m")
                    },
                    frame: PictureFrame::Poster,
                    view: PictureView::Fit,
                    options: PictureOptions::default(),
                    size: STAGE,
                },
            })
            .bytes()
    );
}

#[test]
fn nothing_is_asked_while_the_window_is_hidden_and_playback_stops() {
    let (mut preview, effects) = shown(source_subject(1, "a.y4m"));
    preview.picture(answer(only(&effects)));
    let play = preview.input(Input::PlayPause);
    let Command::Play {
        generation, from, ..
    } = only(&play).command
    else {
        unreachable!("{play:?}")
    };
    assert_eq!(from, 42);
    let hidden = preview.input(Input::Visible(false));
    assert_eq!(only(&hidden).command, Command::StopPlayback);
    assert!(!preview.view().playing);
    none(preview.input(Input::Step(5)));
    none(preview.playback_ended(generation, Some(50)));
    let shown = preview.input(Input::Visible(true));
    assert_eq!(requested(only(&shown)).1, PictureFrame::Index { index: 47 });
}

#[test]
fn a_dead_engine_is_restarted_a_bounded_number_of_times() {
    let (mut preview, effects) = shown(source_subject(1, "a.y4m"));
    let mut ask = only(&effects).clone();
    for _ in 0..MAX_DEATHS {
        let again = preview.process_ended("signal: 9 (SIGKILL)", false);
        let resent = only(&again);
        assert_eq!(requested(resent), requested(&ask), "the same picture again");
        assert!(resent.request_id > ask.request_id);
        ask = resent.clone();
    }
    none(preview.process_ended("signal: 9 (SIGKILL)", false));
    let view = preview.view();
    assert_eq!(view.state, StageState::Unavailable);
    assert!(view.message.contains("stopped working"), "{}", view.message);
    none(preview.input(Input::Step(1)));
    // An answer resets the count: deaths must be in a row.
    let (mut preview, effects) = shown(source_subject(1, "a.y4m"));
    let mut ask = only(&effects).clone();
    for _ in 0..3 * MAX_DEATHS {
        let again = preview.process_ended("exit status: 101", false);
        ask = only(&again).clone();
        preview.picture(answer(&ask));
        ask = only(&preview.input(Input::Step(1))).clone();
    }
    assert_eq!(preview.process_ended("exit status: 101", false).len(), 1);
    let _ = ask;
}

#[test]
fn an_incompatible_or_missing_engine_gives_up_at_once() {
    let (mut preview, _) = shown(source_subject(1, "a.y4m"));
    none(preview.process_ended("protocol 4", true));
    assert!(preview.view().message.contains("does not match"));
    let (mut preview, _) = shown(source_subject(1, "a.y4m"));
    none(preview.spawn_failed("engine not found"));
    assert!(preview.view().message.contains("did not start"));
    none(preview.input(Input::Step(1)));
}

#[test]
fn playback_without_timing_says_why() {
    let (mut preview, effects) = shown(source_subject(1, "a.y4m"));
    let mut untimed = answer(only(&effects));
    untimed.header.time = None;
    preview.picture(untimed);
    let view = preview.view();
    assert!(view.navigable && !view.can_play);
    assert_eq!(view.play_note, text::NO_TIMING);
    none(preview.input(Input::PlayPause));
    // An engine that refuses playback for want of timing says the same.
    let (mut preview, effects) = shown(source_subject(1, "a.y4m"));
    preview.picture(answer(only(&effects)));
    let play = preview.input(Input::PlayPause);
    let Command::Play { generation, .. } = only(&play).command else {
        unreachable!()
    };
    preview.unavailable(generation, PictureFailure::NoTiming, "no timing");
    let view = preview.view();
    assert!(!view.playing && !view.can_play);
    assert_eq!(view.play_note, text::NO_TIMING);
}

#[test]
fn a_live_picture_for_a_job_that_ended_is_dropped() {
    let (mut preview, _) = shown(live_subject(1, 7, "a.y4m"));
    let live = |frame| picture(header(0, PictureOrigin::Live, frame, 832, 468));
    preview.live(7, live(10));
    let view = preview.view();
    assert_eq!(
        view.badge,
        "Live · frame 11 of 300 · colours assumed (BT.709)"
    );
    assert!(!view.navigable && view.can_compare);
    // Another job's picture is not this file's.
    preview.live(8, live(20));
    assert_eq!(preview.view().picture.map(|p| p.header.frame), Some(10));
    // The job completed: its late picture never stands in for the result.
    let effects = preview.sync(stage(result_subject(1, "a.y4m")));
    preview.live(7, live(30));
    let view = preview.view();
    assert_eq!(view.state, StageState::Loading);
    preview.picture(answer(only(&effects)));
    assert!(preview.view().badge.starts_with("AV2 result"));
}

#[test]
fn comparing_a_live_frame_reads_the_source_frame_of_the_same_index() {
    let (mut preview, _) = shown(live_subject(1, 7, "a.y4m"));
    let live = |frame| picture(header(0, PictureOrigin::Live, frame, 832, 468));
    preview.live(7, live(10));
    let ask = only(&preview.input(Input::Compare(true))).clone();
    assert_eq!(
        requested(&ask),
        (
            PictureOrigin::Source,
            PictureFrame::Index { index: 10 },
            PictureView::Fit,
            STAGE
        )
    );
    preview.picture(answer(&ask));
    // Newer live pictures wait while the original is held.
    preview.live(7, live(14));
    assert!(preview.view().badge.starts_with("Original"));
    preview.input(Input::Compare(false));
    assert_eq!(preview.view().picture.map(|p| p.header.frame), Some(14));
}

#[test]
fn playback_is_replaced_at_once_and_its_late_pictures_are_dropped() {
    let (mut preview, effects) = shown(source_subject(1, "a.y4m"));
    preview.picture(answer(only(&effects)));
    let play = preview.input(Input::PlayPause);
    let Command::Play { generation, .. } = only(&play).command else {
        unreachable!()
    };
    let frame = |f| picture(header(generation, PictureOrigin::Source, f, 832, 468));
    preview.picture(frame(43));
    preview.picture(frame(44));
    assert_eq!(preview.view().position, 44);
    // The scrubber moved while playing: playback goes on from there at
    // once, and the old run's late pictures and end are dropped.
    let seek = preview.input(Input::Seek(200));
    let Command::Play {
        generation: next,
        from: 200,
        ..
    } = only(&seek).command
    else {
        unreachable!("{seek:?}")
    };
    assert!(next > generation);
    preview.picture(frame(45));
    none(preview.playback_ended(generation, Some(45)));
    assert_eq!(preview.view().position, 200);
    assert!(preview.view().playing);
    // A step pauses and asks for the one frame.
    let step = preview.input(Input::Step(1));
    assert_eq!(requested(only(&step)).1, PictureFrame::Index { index: 201 });
    assert!(!preview.view().playing);
}

#[test]
fn compare_during_playback_plays_the_other_side_from_the_current_frame() {
    let (mut preview, effects) = shown(result_subject(1, "a.y4m"));
    preview.picture(answer(only(&effects)));
    let play = preview.input(Input::PlayPause);
    let Command::Play { generation, .. } = only(&play).command else {
        unreachable!()
    };
    preview.picture(picture(header(
        generation,
        PictureOrigin::Result,
        60,
        832,
        468,
    )));
    let switch = preview.input(Input::Compare(true));
    let Command::Play {
        target: PictureTarget::Source { .. },
        from: 60,
        ..
    } = only(&switch).command
    else {
        unreachable!("{switch:?}")
    };
}

#[test]
fn pausing_stops_playback_without_asking_for_the_shown_frame_again() {
    let (mut preview, effects) = shown(source_subject(1, "a.y4m"));
    preview.picture(answer(only(&effects)));
    let play = preview.input(Input::PlayPause);
    let Command::Play { generation, .. } = only(&play).command else {
        unreachable!()
    };
    preview.picture(picture(header(
        generation,
        PictureOrigin::Source,
        50,
        832,
        468,
    )));
    let pause = preview.input(Input::PlayPause);
    assert_eq!(only(&pause).command, Command::StopPlayback);
    none(preview.playback_ended(generation, Some(50)));
    assert_eq!(preview.view().position, 50);
}

#[test]
fn thumbnails_go_one_at_a_time_after_the_stage_and_failures_are_not_retried() {
    let mut preview = Preview::new();
    preview.input(Input::Resized {
        stage: STAGE,
        thumb: THUMB,
    });
    let scene = Scene {
        stage: Some(source_subject(1, "a.y4m")),
        thumbs: vec![
            (1, source("a.y4m")),
            (2, source("b.y4m")),
            (3, source("c.y4m")),
        ],
    };
    let stage_ask = only(&preview.sync(scene.clone())).clone();
    assert_eq!(requested(&stage_ask).3, STAGE);
    let thumb = preview.picture(answer(&stage_ask));
    assert_eq!(
        requested(only(&thumb)),
        (
            PictureOrigin::Source,
            PictureFrame::Poster,
            PictureView::Fit,
            THUMB
        )
    );
    let next = preview.picture(answer(only(&thumb)));
    assert!(preview.thumbnail(1).is_some() && preview.thumbnail(2).is_none());
    let Command::Picture { generation, .. } = only(&next).command else {
        unreachable!()
    };
    let last = preview.unavailable(generation, PictureFailure::Unreadable, "gone");
    preview.picture(answer(only(&last)));
    assert!(preview.thumbnail(3).is_some());
    none(preview.sync(scene));
    assert!(preview.take_dirty().thumbs);
}

#[test]
fn a_frame_past_the_end_teaches_the_frame_count() {
    let subject = Subject::Source {
        item: 1,
        source: source("a.mp4"),
        frames: None,
        size: (1920, 1080),
    };
    let (mut preview, effects) = shown(subject);
    let mut poster = answer(only(&effects));
    poster.header.frame_count = None;
    preview.picture(poster);
    let ask = preview.input(Input::Seek(500));
    let Command::Picture { generation, .. } = only(&ask).command else {
        unreachable!()
    };
    let next = preview.unavailable(generation, PictureFailure::OutOfRange, "no frame 500");
    assert_eq!(requested(only(&next)).1, PictureFrame::Index { index: 499 });
    assert_eq!(preview.view().frames, 500);
}

#[test]
fn actual_pixels_start_centred_and_follow_the_engines_origin() {
    let (mut preview, effects) = shown(source_subject(1, "a.y4m"));
    preview.picture(answer(only(&effects)));
    let ask = only(&preview.input(Input::ToggleNative)).clone();
    let centre = PictureView::Native {
        x: (1920 - 832) / 2,
        y: (1080 - 468) / 2,
    };
    assert_eq!(requested(&ask).2, centre);
    let mut got = answer(&ask);
    got.header.view = PictureView::Native { x: 500, y: 300 };
    none(preview.picture(got));
    assert!(preview.view().native);
    let pan = preview.input(Input::Pan {
        dx: 10_000,
        dy: -10,
    });
    assert_eq!(
        requested(only(&pan)).2,
        PictureView::Native {
            x: 1920 - 832,
            y: 290
        }
    );
}

#[test]
fn an_unavailable_picture_says_why_inside_the_area() {
    let (mut preview, effects) = shown(result_subject(1, "a.y4m"));
    let Command::Picture { generation, .. } = only(&effects).command else {
        unreachable!()
    };
    none(preview.unavailable(generation, PictureFailure::Unreadable, "No such file"));
    let view = preview.view();
    assert_eq!(view.state, StageState::Unavailable);
    assert!(view.message.contains("converted file cannot be read"));
    assert!(!view.navigable && !view.can_compare);
}

/// A disk on which nothing exists.
struct NoFiles;

impl Disk for NoFiles {
    fn kind(&self, _: &Path) -> Kind {
        Kind::Missing
    }
}

fn facts() -> SourceFacts {
    SourceFacts {
        reader: SourceReader::Y4m,
        fingerprint: source("a.y4m").fingerprint,
        video_stream: 0,
        video_tracks: Vec::new(),
        omissions: Omissions::default(),
        width: 640,
        height: 360,
        frame_rate: Rational { num: 25, den: 1 },
        pixel_aspect: None,
        color: SourceColor::default(),
        bit_depth: 8,
        chroma: ChromaLayout::Yuv420,
        frame_count: Some(30),
        disclosures: Vec::new(),
    }
}

#[test]
fn the_scene_follows_the_file_from_source_to_live_to_result() {
    let now = Instant::now();
    let mut state = AppState::default();
    let apply = |state: &mut AppState, intent| state.apply(intent, now, &NoFiles).effects;
    apply(
        &mut state,
        Intent::EngineAnswered(Ok(studio_domain::protocol::Capabilities {
            encode_outputs: studio_domain::settings::OutputKind::ALL.to_vec(),
            media_tool: None,
            analysis_framings: Vec::new(),
            analysis_evidence: None,
            options: None,
        })),
    );
    let effects = apply(&mut state, Intent::FilesOpened(vec![abs("/videos/a.y4m")]));
    assert!(matches!(scene(&state).stage, Some(Subject::Loading { .. })));
    let Some(StateEffect::Probe { request, .. }) = effects.first() else {
        unreachable!("{effects:?}")
    };
    apply(
        &mut state,
        Intent::ProbeAnswered {
            id: request.id,
            result: Ok(facts()),
        },
    );
    assert_eq!(
        scene(&state).stage,
        Some(Subject::Source {
            item: 1,
            source: source("a.y4m"),
            frames: Some(30),
            size: (640, 360),
        })
    );
    assert_eq!(scene(&state).thumbs, [], "one file has no rows");
    let effects = apply(&mut state, Intent::ConvertRequested);
    let Some(StateEffect::Start { id, job }) = effects.first() else {
        unreachable!("{effects:?}")
    };
    let (id, destination) = (*id, job.destination.clone());
    assert!(matches!(scene(&state).stage, Some(Subject::Live { job, .. }) if job == id));
    for stage in [Stage::Verifying, Stage::Publishing] {
        apply(
            &mut state,
            Intent::Job {
                id,
                input: JobInput::Event(Event::Stage { stage }),
            },
        );
    }
    apply(
        &mut state,
        Intent::Job {
            id,
            input: JobInput::Event(Event::Completed {
                report: JobReport {
                    pass_digest: None,
                    destination: destination.clone(),
                    frames: 30,
                    bytes: 1,
                    checks: Vec::new(),
                    disclosures: Vec::new(),
                },
            }),
        },
    );
    assert!(matches!(
        scene(&state).stage,
        Some(Subject::Result { path, .. }) if path == destination
    ));
}

#[test]
fn partial_updates_keep_the_decode_request_and_completion_verifies_the_selected_output() {
    let full = document_subject(1);
    let prefix = |bytes| {
        let mut subject = full.clone();
        let Subject::Document { digest, .. } = &mut subject else {
            unreachable!("a document subject")
        };
        digest.bytes = bytes;
        subject
    };
    let (mut preview, initial) = shown(prefix(100));
    let next_prefix = prefix(500);
    none(preview.sync(stage(next_prefix.clone())));
    assert_eq!(
        preview.document_request_generation(),
        Some(only(&initial).request_id)
    );
    let seek = preview.input(Input::SeekOutput(2));
    let Command::Picture { target, frame, .. } = &only(&seek).command else {
        unreachable!("a picture request")
    };
    assert_eq!(Some(target.clone()), next_prefix.target());
    assert_eq!(*frame, PictureFrame::Index { index: 1 });

    let verified = preview.sync(stage(full.clone()));
    let Command::Picture { target, frame, .. } = &only(&verified).command else {
        unreachable!("a verified picture request")
    };
    assert_eq!(Some(target.clone()), full.target());
    assert_eq!(*frame, PictureFrame::Index { index: 1 });
}

#[test]
fn operating_point_feedback_matches_the_active_document_generation() {
    use studio_domain::layers::LayerScope;

    let initial = document_subject(7);
    let (mut preview, opening) = shown(initial.clone());
    let old_generation = only(&opening).request_id;
    let mut selected = initial;
    let Subject::Document { selection, .. } = &mut selected else {
        unreachable!("a document subject");
    };
    let policy = LayerDecodeSelection::OperatingPoint {
        scope: LayerScope::Extended { id: 0 },
        set: 2,
        point: 0,
    };
    *selection = policy;
    let switch = preview.sync(stage(selected));
    let [close, open] = switch.as_slice() else {
        unreachable!("{switch:?}");
    };
    assert_eq!(
        close.command,
        Command::CloseDocumentPlayback { document: 7 }
    );
    none(preview.picture(output_picture(old_generation, 0, None)));
    assert!(preview.take_layer_decode_feedback().is_none());

    preview.picture(output_picture(open.request_id, 0, None));
    assert_eq!(
        preview.take_layer_decode_feedback(),
        Some((7, policy, LayerDecodeStatus::Active))
    );

    let grain = preview.input(Input::Grain(GrainPolicy::Skip));
    assert_eq!(
        preview.take_layer_decode_feedback(),
        Some((7, policy, LayerDecodeStatus::Pending))
    );
    none(preview.unavailable(open.request_id, PictureFailure::Unreadable, "old error"));
    assert!(preview.take_layer_decode_feedback().is_none());
    none(preview.unavailable(
        only(&grain).request_id,
        PictureFailure::Unsupported,
        "Cannot decode this operating point.",
    ));
    assert_eq!(
        preview.take_layer_decode_feedback(),
        Some((
            7,
            policy,
            LayerDecodeStatus::Rejected("Cannot decode this operating point.".into()),
        ))
    );
}

fn document_subject(document: u64) -> Subject {
    let source = source("clip.obu");
    Subject::Document {
        document,
        fingerprint: source.fingerprint,
        path: source.path,
        selection: LayerDecodeSelection::Default,
        digest: Box::new(studio_domain::analysis::SourceDigest {
            sha256: "0".repeat(64),
            bytes: source.fingerprint.len,
        }),
    }
}

fn test_picture_info() -> DocumentPictureInfo {
    let chroma = SampleRect {
        x: 1,
        y: 2,
        width: 960,
        height: 540,
    };
    DocumentPictureInfo {
        bit_depth: 10,
        chroma: ChromaLayout::Yuv420,
        coded_width: 1924,
        coded_height: 1084,
        y: SampleRect {
            x: 2,
            y: 4,
            width: 1920,
            height: 1080,
        },
        u: Some(chroma),
        v: Some(chroma),
        color: None,
        chroma_position: None,
        film_grain: true,
    }
}

fn output_picture(generation: u64, frame: u64, interval: Option<MediaTime>) -> Picture {
    picture(PictureHeader {
        time: None,
        frame_count: None,
        document_info: Some(Box::new(test_picture_info())),
        output: Some(OutputEvent {
            ordinal: frame,
            unit_offset: 20,
            unit_len: 30,
            extended_layer: 0,
            embedded_layer: 0,
            temporal_layer: 1,
            coded_offset: Some(15),
            showing_offset: Some(22),
            sequence_offset: Some(5),
            interval,
        }),
        ..header(generation, PictureOrigin::Document, frame, 2, 2)
    })
}

#[test]
fn reference_preview_verifies_the_coded_source_without_replacing_the_main_picture() {
    use studio_domain::identity::DecodeLayers;
    use studio_domain::references::ReferenceFrame;

    let subject = document_subject(7);
    let Some(target) = subject.target() else {
        unreachable!("expected a document picture target")
    };
    let frame = ReferenceFrame {
        coded_offset: 15,
        sequence_offset: 5,
        layers: DecodeLayers {
            extended: 0,
            embedded: 0,
            temporal: 0,
        },
        width: 1924,
        height: 1084,
    };
    let (mut preview, opening) = shown(subject);
    preview.picture(output_picture(only(&opening).request_id, 0, None));
    let request = preview.request_reference_preview(target.clone(), frame, 1);
    let request = only(&request);
    assert!(matches!(
        request.command,
        Command::Picture {
            frame: PictureFrame::Index { index: 1 },
            view: PictureView::Fit,
            size: PictureBox {
                width: 240,
                height: 136
            },
            ..
        }
    ));
    assert_eq!(preview.document_request_generation(), None);
    // The output uses a later temporal layer to display the coded reference.
    preview.picture(output_picture(request.request_id, 1, None));
    assert_eq!(
        preview
            .reference_preview()
            .and_then(Result::ok)
            .map(|picture| picture.header.frame),
        Some(1)
    );
    assert_eq!(preview.view().frame_text, "Output 1");

    let request = preview.request_reference_preview(
        target,
        ReferenceFrame {
            coded_offset: 16,
            ..frame
        },
        1,
    );
    preview.picture(output_picture(only(&request).request_id, 1, None));
    assert!(matches!(preview.reference_preview(), Some(Err(_))));
    assert_eq!(preview.view().frame_text, "Output 1");
}

#[test]
fn reference_preview_cancellation_rejects_late_pictures() {
    use studio_domain::identity::DecodeLayers;
    use studio_domain::references::ReferenceFrame;

    let subject = document_subject(7);
    let Some(target) = subject.target() else {
        unreachable!("expected a document picture target")
    };
    let (mut preview, opening) = shown(subject);
    preview.picture(output_picture(only(&opening).request_id, 0, None));
    let request = preview.request_reference_preview(
        target,
        ReferenceFrame {
            coded_offset: 15,
            sequence_offset: 5,
            layers: DecodeLayers {
                extended: 0,
                embedded: 0,
                temporal: 1,
            },
            width: 1924,
            height: 1084,
        },
        1,
    );
    let generation = only(&request).request_id;
    let cancel = preview.clear_reference_preview();
    assert_eq!(
        only(&cancel),
        &Effect {
            request_id: generation,
            command: Command::StopPlayback
        }
    );
    none(preview.picture(output_picture(generation, 1, None)));
    assert!(preview.reference_preview().is_none());
    assert_eq!(preview.view().frame_text, "Output 1");
}

#[test]
fn next_at_a_partial_boundary_preserves_the_picture_with_no_frame_count() {
    let (mut preview, opening) = shown(document_subject(7));
    preview.picture(output_picture(only(&opening).request_id, 0, None));
    let next = only(&preview.input(Input::Step(1))).request_id;
    none(preview.unavailable(next, PictureFailure::PartialCoverage, "Partial index."));
    let view = preview.view();
    assert_eq!(view.frame_text, "Output 1");
    assert!(view.picture.is_some());
    assert_eq!(view.frames, 0);
    assert_eq!(view.seek_error, "Partial index.");
}

#[test]
fn rapid_document_seeks_and_cancel_discard_old_work() {
    use studio_domain::preview::PictureStage;

    let (mut preview, opening) = shown(document_subject(7));
    let first = only(&opening).request_id;
    preview.picture(output_picture(first, 0, None));
    assert_eq!(preview.output_byte(1), Some((7, 15)));
    assert_eq!(preview.output_byte(2), Some((7, 22)));
    assert_eq!(preview.view().frame_text, "Output 1");
    assert_eq!(preview.cached_bytes(), 0);

    let old = only(&preview.input(Input::SeekOutput(27))).request_id;
    preview.progress(old, PictureStage::Replay, 9, Some(26));
    assert!(preview.view().decode_work.contains("9 / 26"));
    assert!(preview.view().picture.is_none());
    assert_eq!(preview.output_byte(1), None);
    let latest = only(&preview.input(Input::SeekOutput(3))).request_id;
    preview.progress(old, PictureStage::Replay, 25, Some(26));
    none(preview.picture(output_picture(old, 26, None)));
    assert_eq!(preview.view().decode_work, "Decoding to output 3…");
    assert!(preview.view().picture.is_none());

    let cancel = preview.input(Input::CancelDecode);
    assert_eq!(only(&cancel).command, Command::StopPlayback);
    assert_eq!(only(&cancel).request_id, latest);
    none(preview.picture(output_picture(latest, 2, None)));
    assert_eq!(preview.view().message, "Decode cancelled.");
    assert_eq!(preview.view().decode_work, "");
    assert!(preview.view().picture.is_none());

    let retry = only(&preview.input(Input::SeekOutput(3))).request_id;
    preview.picture(output_picture(retry, 2, None));
    assert_eq!(preview.view().frame_text, "Output 3");
    assert_eq!(preview.output_byte(2), Some((7, 22)));
}

#[test]
fn document_switch_close_and_worker_death_clear_pictures_and_links() {
    let (mut preview, opening) = shown(document_subject(7));
    let old = only(&opening).request_id;
    preview.picture(output_picture(old, 0, None));
    let seek = only(&preview.input(Input::Step(1))).request_id;

    // The same path under a new document read must have its own generation.
    let switch = preview.sync(stage(document_subject(8)));
    let [close, open] = switch.as_slice() else {
        unreachable!("{switch:?}");
    };
    assert_eq!(
        close.command,
        Command::CloseDocumentPlayback { document: 7 }
    );
    none(preview.picture(output_picture(seek, 1, None)));
    assert!(preview.view().picture.is_none());
    preview.picture(output_picture(open.request_id, 0, None));
    assert_eq!(preview.output_byte(0), Some((8, 20)));
    let play = only(&preview.input(Input::PlayPause)).request_id;
    preview.picture(output_picture(play, 1, None));

    none(preview.process_ended("exit", false));
    assert_eq!(preview.view().state, StageState::Unavailable);
    assert!(!preview.view().playing);
    none(preview.picture(output_picture(play, 2, None)));
    assert!(preview.view().picture.is_none());
    assert_eq!(preview.output_byte(0), None);

    assert_eq!(
        only(&preview.sync(Scene::default())).command,
        Command::CloseDocumentPlayback { document: 8 }
    );
    assert_eq!(preview.view().state, StageState::None);
    none(preview.picture(output_picture(open.request_id, 0, None)));
    assert!(preview.view().picture.is_none());
}

#[test]
fn document_timing_and_output_count_use_only_decoder_evidence() {
    let (mut preview, opening) = shown(document_subject(7));
    let first = only(&opening).request_id;
    preview.picture(output_picture(first, 0, None));
    let view = preview.view();
    assert!(view.can_play);
    assert_eq!(view.time_text, "");
    assert!(view.play_note.contains("assumes 30 fps"));
    let seek = only(&preview.input(Input::SeekOutput(99))).request_id;
    none(preview.unavailable(seek, PictureFailure::OutOfRange, "output absent"));
    assert_eq!(preview.view().frames, 0);
    assert!(preview.view().picture.is_none());

    let request = only(&preview.input(Input::SeekOutput(2))).request_id;
    let interval = Some(MediaTime { num: 1, den: 24 });
    preview.picture(output_picture(request, 1, interval));
    let view = preview.view();
    assert_eq!(view.play_note, "");
    assert_eq!(view.time_text, "");
    let play = only(&preview.input(Input::PlayPause)).request_id;
    preview.picture(output_picture(play, 4, interval));
    none(preview.playback_ended(play, Some(4)));
    assert_eq!(preview.view().frame_text, "Output 5 / 5");
}

#[test]
fn invalid_output_number_preserves_the_assumed_playback_rate() {
    let (mut preview, opening) = shown(document_subject(7));
    preview.picture(output_picture(only(&opening).request_id, 0, None));
    let play = only(&preview.input(Input::PlayPause)).request_id;

    none(preview.input(Input::SeekOutput(0)));
    preview.picture(output_picture(play, 1, None));
    let view = preview.view();
    assert!(view.playing);
    assert_eq!(view.seek_error, "Enter a positive output number.");
    assert!(view.play_note.contains("assumes 30 fps"));

    let request = only(&preview.input(Input::SeekOutput(3))).request_id;
    preview.picture(output_picture(request, 2, None));
    let view = preview.view();
    assert_eq!(view.seek_error, "");
    assert!(view.play_note.contains("assumes 30 fps"));
}

#[test]
fn next_at_the_end_restores_the_last_output_and_confirms_the_count() {
    let (mut preview, opening) = shown(document_subject(7));
    let first = only(&opening).request_id;
    preview.picture(output_picture(first, 0, None));

    let next = only(&preview.input(Input::Step(1))).request_id;
    let view = preview.view();
    assert_eq!(view.frame_text, "Output 2");
    assert!(view.picture.is_none());

    none(preview.unavailable(next, PictureFailure::OutOfRange, "end of file"));
    let view = preview.view();
    assert_eq!(view.frame_text, "Output 1 / 1");
    assert_eq!(view.picture.map(|picture| picture.header.frame), Some(0));
    assert_eq!(preview.output_byte(1), Some((7, 15)));
    none(preview.input(Input::Step(1)));
}

#[test]
fn exact_samples_follow_the_displayed_output_plane_and_grain() {
    let (mut preview, opening) = shown(document_subject(7));
    preview.picture(output_picture(only(&opening).request_id, 0, None));
    let plane_request = preview.input(Input::Surface(PictureSurface::U));
    assert!(preview.view().picture.is_none());
    let plane_generation = only(&plane_request).request_id;
    let chroma_picture = |generation, grain| {
        let mut picture = output_picture(generation, 0, None);
        picture.header.options = PictureOptions {
            surface: PictureSurface::U,
            grain,
        };
        picture.header.frame_width = 960;
        picture.header.frame_height = 540;
        picture
    };
    preview.picture(chroma_picture(plane_generation, GrainPolicy::Apply));
    let point = SamplePoint {
        plane: SamplePlane::U,
        x: 959,
        y: 539,
    };
    let ask = || Input::ReadSample {
        x: Some(point.x),
        y: Some(point.y),
    };
    let sample = preview.input(ask());
    assert!(matches!(only(&sample).command,
        Command::PictureSample { ordinal: 0, grain: GrainPolicy::Apply, point: sent, .. } if sent == point));
    let old_sample = only(&sample).request_id;
    preview.sample_answer(old_sample, 1, GrainPolicy::Apply, point, 900);
    assert_eq!(preview.view().viewer.sample_copy, "");

    let grain_request = preview.input(Input::Grain(GrainPolicy::Skip));
    assert!(preview.view().picture.is_none());
    preview.sample_answer(old_sample, 0, GrainPolicy::Apply, point, 900);
    assert_eq!(preview.view().viewer.sample_copy, "");
    none(preview.picture(chroma_picture(plane_generation, GrainPolicy::Apply)));
    assert!(preview.view().picture.is_none());
    preview.picture(chroma_picture(
        only(&grain_request).request_id,
        GrainPolicy::Skip,
    ));

    let sample = preview.input(ask());
    let exact = only(&sample).request_id;
    preview.sample_answer(exact, 0, GrainPolicy::Skip, point, 1023);
    assert_eq!(preview.view().viewer.sample_copy, "1023");
    assert_eq!(
        preview.view().viewer.sample_text,
        "U chroma (959, 539) = 1023"
    );

    let seek = preview.input(Input::Step(1));
    assert_eq!(preview.view().viewer.sample_copy, "");
    assert!(matches!(
        only(&seek).command,
        Command::Picture {
            options: PictureOptions {
                surface: PictureSurface::U,
                grain: GrainPolicy::Skip
            },
            ..
        }
    ));
    preview.sample_answer(exact, 0, GrainPolicy::Skip, point, 1023);
    assert_eq!(preview.view().viewer.sample_copy, "");
}

#[test]
fn zoom_and_pan_map_the_painted_crop_in_visible_sample_coordinates() {
    let (mut preview, opening) = shown(document_subject(7));
    preview.picture(output_picture(only(&opening).request_id, 0, None));
    let zoom = preview.input(Input::Zoom(4));
    let Command::Picture {
        view: PictureView::Zoom { x, y, factor: 4 },
        ..
    } = only(&zoom).command
    else {
        unreachable!("{zoom:?}");
    };
    let mut picture = output_picture(only(&zoom).request_id, 0, None);
    picture.header.view = PictureView::Zoom { x, y, factor: 4 };
    preview.picture(picture);
    none(preview.input(Input::PickSample {
        x: -1,
        y: 0,
        width: 8,
        height: 8,
    }));
    let pick = preview.input(Input::PickSample {
        x: 7,
        y: 5,
        width: 8,
        height: 8,
    });
    assert!(matches!(only(&pick).command,
        Command::PictureSample { point: SamplePoint { plane: SamplePlane::Y, x: sx, y: sy }, .. }
        if sx == x + 1 && sy == y + 1));
    for _ in 0..3 {
        none(preview.input(Input::Pan { dx: 1, dy: 0 }));
    }
    let pan = preview.input(Input::Pan { dx: 1, dy: -8 });
    assert!(matches!(only(&pan).command,
        Command::Picture { view: PictureView::Zoom { x: left, y: top, factor: 4 }, .. }
        if left == x + 1 && top == y - 2));
}

#[test]
fn a_picture_with_invalid_plane_metadata_cannot_be_displayed_or_sampled() {
    let cases: [fn(&mut DocumentPictureInfo); 3] = [
        |info| info.bit_depth = 12,
        |info| info.coded_width = 1920,
        |info| info.u = None,
    ];
    for damage in cases {
        let (mut preview, opening) = shown(document_subject(7));
        let mut reply = output_picture(only(&opening).request_id, 0, None);
        if let Some(info) = &mut reply.header.document_info {
            damage(info);
        }
        none(preview.picture(reply));
        let view = preview.view();
        assert_eq!(view.state, StageState::Unavailable);
        assert_eq!(view.viewer.geometry, "");
        assert!(!view.viewer.can_sample);
        assert!(view.picture.is_none());
    }
}

#[test]
fn returning_to_the_shown_crop_cancels_an_obsolete_pan() {
    let (mut preview, opening) = shown(document_subject(7));
    preview.picture(output_picture(only(&opening).request_id, 0, None));
    let zoom = preview.input(Input::Zoom(4));
    let Command::Picture { view, .. } = only(&zoom).command else {
        unreachable!("{zoom:?}");
    };
    let mut reply = output_picture(only(&zoom).request_id, 0, None);
    reply.header.view = view;
    preview.picture(reply);

    let pan = preview.input(Input::Pan { dx: 4, dy: 0 });
    let Command::Picture { view: moved, .. } = only(&pan).command else {
        unreachable!("{pan:?}");
    };
    let cancel = preview.input(Input::Pan { dx: -4, dy: 0 });
    assert_eq!(only(&cancel).command, Command::StopPlayback);
    let mut late = output_picture(only(&pan).request_id, 0, None);
    late.header.view = moved;
    none(preview.picture(late));
    assert_eq!(preview.view().picture.map(|p| p.header.view), Some(view));
}

#[test]
fn playback_continues_when_a_new_frame_requires_a_smaller_crop_origin() {
    let (mut preview, opening) = shown(document_subject(7));
    preview.picture(output_picture(only(&opening).request_id, 0, None));
    let zoom = preview.input(Input::Zoom(4));
    let Command::Picture { view, .. } = only(&zoom).command else {
        unreachable!("{zoom:?}");
    };
    let mut first = output_picture(only(&zoom).request_id, 0, None);
    first.header.view = view;
    preview.picture(first);
    let play = preview.input(Input::PlayPause);
    let mut cropped = output_picture(only(&play).request_id, 1, None);
    cropped.header.view = PictureView::Zoom {
        x: 0,
        y: 0,
        factor: 4,
    };
    cropped.header.frame_width = 160;
    cropped.header.frame_height = 80;
    if let Some(info) = &mut cropped.header.document_info {
        info.y.width = 160;
        info.y.height = 80;
        let chroma = Some(SampleRect {
            x: 1,
            y: 2,
            width: 80,
            height: 40,
        });
        info.u = chroma;
        info.v = chroma;
    }
    none(preview.picture(cropped));
    let view = preview.view();
    assert_eq!(view.position, 1);
    assert!(view.playing);
}
