// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use std::cell::RefCell;
use std::fs;
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::platform::Key;
use slint::{ComponentHandle, Model};
use studio_app::BlockOverlay;
use studio_app::analysis::{BlockOverlayInput, Input};
use studio_app::preview::{Dirty, Input as PreviewInput, StageState};
use studio_domain::preview::{PictureBox, PictureView};

use super::{Rig, Scratch};

const RECTANGLE: &[u8] =
    include_bytes!("../../../studio-engine/tests/inspect/fixtures/syn-hrect-intra-64x64-q120.ivf");
const CHROMA_TREE: &[u8] = include_bytes!(
    "../../../studio-engine/tests/inspect/fixtures/syn-lossless-sdp-nondc-chroma-v-intra-64x64-10bit.ivf"
);
const DENSE: &[u8] = include_bytes!(
    "../../../studio-engine/tests/inspect/fixtures/syn-gdf-xtile-lr-512x256-q190.ivf"
);

fn open(rig: &mut Rig, scratch: &Scratch, name: &str, bytes: &[u8]) {
    rig.session.block_overlay_input(BlockOverlayInput::Mode(0));
    while rig.session.analysis().shown().is_some() {
        rig.session.analysis_input(Input::Close);
    }
    let path = scratch.0.join(name);
    fs::write(&path, bytes).unwrap_or_else(|error| unreachable!("{error}"));
    rig.session.analysis_input(Input::Open(vec![path]));
    rig.idle("the overlay source");
    rig.session.preview_input(PreviewInput::Resized {
        stage: PictureBox::clamped(200, 160),
        thumb: PictureBox::clamped(64, 36),
    });
    rig.session.analysis_input(Input::TogglePlayback);
    rig.settle("the paused picture", |session| {
        session.preview().view().state == StageState::Picture
            && session.preview().view().viewer.can_sample
    });
}

pub(super) fn inspect(rig: &mut Rig, scratch: &Scratch) {
    let actions = Rc::new(RefCell::new(Vec::new()));
    studio_app::bind_block_overlay(&rig.window, {
        let actions = actions.clone();
        move |action| actions.borrow_mut().push(action)
    });
    let settle = |rig: &mut Rig, what: &str| {
        for action in actions.take() {
            rig.session.block_overlay_input(action);
        }
        rig.settle(what, |session| {
            session.preview().view().viewer.can_sample && !session.analysis().waiting()
        });
        studio_app::render_preview(
            &rig.window,
            rig.session.state(),
            rig.session.preview(),
            Dirty {
                picture: true,
                view: true,
                thumbs: false,
            },
        );
    };

    open(rig, scratch, "rectangular blocks.ivf", RECTANGLE);
    view(rig).invoke_choose(1);
    settle(rig, "rectangular partition outlines");
    assert!(
        view(rig)
            .get_outlines()
            .iter()
            .any(|item| item.width == 64 && item.height == 32)
    );

    view(rig).invoke_pick(42, 118, 160, 160);
    settle(rig, "the lower rectangular coding block");
    assert!(view(rig).get_picked().contains("Coded luma (0, 32) 64×32"));
    assert!(view(rig).get_picked().contains("qindex 120"));
    assert!(view(rig).get_rows().iter().any(|row| row.selected));

    view(rig).invoke_pick(42, 42, 160, 160);
    settle(rig, "the upper rectangular coding block");
    assert!(view(rig).get_picked().contains("Coded luma (0, 0) 64×32"));
    rig.session.analysis_input(Input::Back);
    settle(rig, "history returns to the lower coding block");
    assert!(view(rig).get_picked().contains("Coded luma (0, 32) 64×32"));

    for _ in 0..35 {
        rig.press(Key::Tab);
        rig.press(Key::DownArrow);
        if actions
            .borrow()
            .iter()
            .any(|action| matches!(action, BlockOverlayInput::Move(1)))
        {
            break;
        }
        actions.take();
        rig.inputs.take();
    }
    assert!(
        actions
            .borrow()
            .iter()
            .any(|action| matches!(action, BlockOverlayInput::Move(1)))
    );
    settle(rig, "keyboard block selection");
    assert!(view(rig).get_rows().iter().any(|row| row.selected));

    view(rig).invoke_choose(2);
    settle(rig, "transform geometry");
    assert!(
        view(rig)
            .get_rows()
            .iter()
            .all(|row| row.label.contains("Y transform"))
    );
    view(rig).invoke_pick(42, 42, 160, 160);
    settle(rig, "exact transform details");
    assert!(view(rig).get_picked().contains("PlaneTxType DCT_DCT"));
    assert_eq!(
        view(rig)
            .get_outlines()
            .iter()
            .filter(|item| item.selected)
            .count(),
        1
    );

    zoom_and_pan(rig, &settle);
    inspect_chroma(rig, scratch, &settle);
    inspect_dense(rig, scratch, &settle);
}

fn zoom_and_pan(rig: &mut Rig, settle: &impl Fn(&mut Rig, &str)) {
    rig.session.preview_input(PreviewInput::Zoom(4));
    settle(rig, "zoomed transform geometry");
    rig.session
        .preview_input(PreviewInput::Pan { dx: -24, dy: 0 });
    settle(rig, "panned transform geometry");
    let picture = rig
        .session
        .preview()
        .view()
        .picture
        .unwrap_or_else(|| unreachable!("preview"));
    assert!(matches!(
        picture.header.view,
        PictureView::Zoom { factor: 4, .. }
    ));
    assert!(view(rig).get_outlines().iter().any(|item| item.x < 0));
}

fn inspect_chroma(rig: &mut Rig, scratch: &Scratch, settle: &impl Fn(&mut Rig, &str)) {
    open(rig, scratch, "overlapping trees.ivf", CHROMA_TREE);
    view(rig).invoke_choose(1);
    settle(rig, "the luma tree");
    view(rig).invoke_pick(40, 40, 160, 160);
    settle(rig, "the luma block");
    assert!(view(rig).get_picked().contains("luma tree"));
    assert!(view(rig).get_picked().contains("Superblock"));
    view(rig).invoke_choose_plane(2);
    settle(rig, "the chroma tree");
    view(rig).invoke_pick(40, 40, 160, 160);
    settle(rig, "the overlapping chroma block");
    assert!(view(rig).get_picked().contains("chroma tree"));
    assert!(view(rig).get_picked().contains("Coded luma (0, 0) 64×64"));
}

fn inspect_dense(rig: &mut Rig, scratch: &Scratch, settle: &impl Fn(&mut Rig, &str)) {
    use studio_domain::blocks::MAX_BLOCK_RECORDS;

    open(rig, scratch, "dense multi-tile blocks.ivf", DENSE);
    settle(rig, "the dense picture without an overlay");
    let redraw_without_evidence = redraw_time(rig);
    let capture = Instant::now();
    view(rig).invoke_choose(3);
    settle(rig, "bounded quantizer map");
    let capture_elapsed = capture.elapsed();
    assert!(view(rig).get_status().contains("Capture limit reached"));
    assert!(
        view(rig)
            .get_rows()
            .iter()
            .any(|row| row.label.starts_with("Q "))
    );
    let outlined = view(rig).get_outlines().row_count();
    let max_records = MAX_BLOCK_RECORDS as usize;
    assert!((1..=max_records).contains(&outlined));
    let redraw_with_evidence = redraw_time(rig);
    eprintln!(
        "Dense overlay. {outlined} outlines. Capture {capture_elapsed:?}. Median software redraw off {redraw_without_evidence:?}, on {redraw_with_evidence:?}."
    );

    view(rig).invoke_pick(172, 24, 200, 100);
    view(rig).invoke_choose_plane(2);
    settle(rig, "the plane change cancelled the old point");
    assert!(view(rig).get_rows().iter().all(|row| !row.selected));

    view(rig).invoke_choose_plane(1);
    view(rig).invoke_pick(172, 24, 200, 100);
    settle(rig, "a point in the uncovered part of the dense map");
    assert!(view(rig).get_picked().contains("Tile 1"));
    assert!(view(rig).get_outlines().row_count() <= 2 * max_records);

    rig.session.block_overlay_input(BlockOverlayInput::Mode(2));
    rig.session.block_overlay_input(BlockOverlayInput::Stop);
    settle(rig, "the stopped transform query");
    assert!(view(rig).get_status().contains("capture stopped"));
    assert_eq!(view(rig).get_outlines().row_count(), 0);
}

fn redraw_time(rig: &Rig) -> Duration {
    let _ = rig.pixels();
    let mut elapsed: [Duration; 5] = std::array::from_fn(|_| {
        let draw = Instant::now();
        let _ = rig.pixels();
        draw.elapsed()
    });
    elapsed.sort_unstable();
    elapsed[2]
}

fn view(rig: &Rig) -> BlockOverlay<'_> {
    rig.window.global::<BlockOverlay>()
}
