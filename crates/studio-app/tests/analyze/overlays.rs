// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use std::cell::RefCell;
use std::fs;
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::language::ColorScheme::{Dark, Light};
use slint::platform::Key;
use slint::{ComponentHandle, Model};
use studio_app::analysis::{BlockOverlayInput, Input};
use studio_app::preview::{Dirty, Input as PreviewInput, StageState};
use studio_app::{BlockOverlay, CoefficientView};
use studio_domain::preview::{PictureBox, PictureView};

use super::{Rig, Scratch};

const RECTANGLE: &[u8] =
    include_bytes!("../../../studio-engine/tests/inspect/fixtures/syn-hrect-intra-64x64-q120.ivf");
const EXACT: &[u8] = include_bytes!(
    "../../../studio-engine/tests/inspect/fixtures/syn-2tile-zone1-edge-128x64-q192.ivf"
);
const CHROMA_TREE: &[u8] = include_bytes!(
    "../../../studio-engine/tests/inspect/fixtures/syn-lossless-sdp-nondc-chroma-v-intra-64x64-10bit.ivf"
);
const PADDED: &[u8] = include_bytes!(
    "../../../studio-engine/tests/preview/fixtures/syn-mrl-padded-444-67x67-10bit-q120.ivf"
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

pub(super) fn inspect(rig: &mut Rig, scratch: &Scratch, clipboard: &RefCell<String>) {
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
            .get_rows()
            .iter()
            .any(|row| row.label.contains("PARTITION_HORZ"))
    );
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
    inspect_coefficients(rig, scratch, &settle, &actions, clipboard);
    inspect_edge(rig, scratch, &settle);
    inspect_chroma(rig, scratch, &settle);
    inspect_dense(rig, scratch, &settle);
}

fn inspect_coefficients(
    rig: &mut Rig,
    scratch: &Scratch,
    settle: &impl Fn(&mut Rig, &str),
    actions: &RefCell<Vec<BlockOverlayInput>>,
    clipboard: &RefCell<String>,
) {
    open(rig, scratch, "tile coefficients.ivf", EXACT);
    view(rig).invoke_choose(2);
    settle(rig, "the transform overlay");
    view(rig).invoke_pick(110, 20, 200, 100);
    settle(rig, "the second tile transform");
    assert!(!rig.window.global::<CoefficientView>().get_active());
    rig.window.global::<CoefficientView>().invoke_read();
    settle(rig, "exact coefficients");
    let coefficients = rig.window.global::<CoefficientView>();
    assert!(coefficients.get_active());
    assert!(coefficients.get_geometry().contains("Y plane"));
    assert_eq!(
        coefficients.get_rows().row_data(0).map(|row| row.index),
        Some(0)
    );
    let report: serde_json::Value = serde_json::from_str(&coefficients.get_report())
        .unwrap_or_else(|error| unreachable!("{error}"));
    assert_eq!(report["version"], 1);
    assert_eq!(report["frame"]["coded_offset"], 60);
    assert_eq!(report["scale"], 1);
    assert_eq!(report["source_name"], "tile coefficients.ivf");
    assert_eq!(report["source_digest"]["bytes"], EXACT.len() as u64);
    assert_eq!(
        report["source_digest"]["sha256"].as_str().map(str::len),
        Some(64)
    );
    assert_eq!(
        report["values"]["codec_revision"].as_str().map(str::len),
        Some(40)
    );
    assert_eq!(report["transform"]["plane_tx_type"], "DCT_DCT");
    assert_eq!(report["values"]["id"]["plane"], "y");
    assert_eq!(report["values"]["id"]["block"]["tile"], 1);
    assert_eq!(report["values"]["quantized"][0], -1);
    assert_eq!(report["values"]["dequantized"][1], -3432);
    assert_eq!(report["values"]["scan"][1], 32);
    let first = coefficients
        .get_rows()
        .row_data(0)
        .unwrap_or_else(|| unreachable!("no first coefficient"));
    assert_eq!(first.quantized, "-1");
    assert_eq!(first.dequantized, "-312");
    assert_eq!(first.scan, "0");
    navigate_coefficient_table(rig, settle, actions, clipboard);

    rig.window.global::<CoefficientView>().invoke_read();
    view(rig).invoke_choose_plane(2);
    settle(rig, "change plane during coefficient capture");
    assert!(!rig.window.global::<CoefficientView>().get_active());
    assert!(
        rig.window
            .global::<CoefficientView>()
            .get_report()
            .is_empty()
    );
    view(rig).invoke_choose_plane(1);
    settle(rig, "return to luma");
    view(rig).invoke_pick(110, 20, 200, 100);
    settle(rig, "return to transform selection");
    assert!(!rig.window.global::<CoefficientView>().get_active());
}

fn navigate_coefficient_table(
    rig: &mut Rig,
    settle: &impl Fn(&mut Rig, &str),
    actions: &RefCell<Vec<BlockOverlayInput>>,
    clipboard: &RefCell<String>,
) {
    for _ in 0..50 {
        rig.press(Key::Tab);
        rig.press(Key::PageDown);
        if actions
            .borrow()
            .iter()
            .any(|action| matches!(action, BlockOverlayInput::MoveCoefficient(6)))
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
            .any(|action| matches!(action, BlockOverlayInput::MoveCoefficient(6)))
    );
    settle(rig, "keyboard coefficient selection");
    assert_eq!(
        rig.window
            .global::<CoefficientView>()
            .get_rows()
            .row_data(0)
            .map(|row| (row.index, row.selected)),
        Some((6, true))
    );
    rig.press(Key::Home);
    settle(rig, "keyboard returns to DC");
    assert_eq!(
        rig.window.global::<CoefficientView>().get_position(),
        "Coefficient 1 of 1024"
    );
    for (scheme, scale, name) in [
        (Light, 1.0, "coefficients-keyboard-light-1x.ppm"),
        (Dark, 2.0, "coefficients-keyboard-dark-2x.ppm"),
        (Light, 2.0, "coefficients-keyboard-light-2x.ppm"),
        (Dark, 1.0, "coefficients-keyboard-dark-1x.ppm"),
    ] {
        rig.snapshot(scheme, scale, name);
    }
    rig.press(Key::Backtab);
    rig.press(Key::Backtab);
    rig.press(Key::Return);
    assert_eq!(
        clipboard.borrow().as_str(),
        rig.window.global::<CoefficientView>().get_report().as_str()
    );
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

fn inspect_edge(rig: &mut Rig, scratch: &Scratch, settle: &impl Fn(&mut Rig, &str)) {
    open(rig, scratch, "coded edge blocks.ivf", PADDED);
    view(rig).invoke_choose(1);
    settle(rig, "the cropped coded picture");
    view(rig).invoke_pick(85, 156, 160, 160);
    settle(rig, "the coded block at the bottom edge");
    let picked = view(rig).get_picked();
    assert!(picked.contains("Coded luma (32, 64) 16×8"), "{picked}");
    assert!(picked.contains("Visible luma (32, 64) 16×3"), "{picked}");
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
    view(rig).invoke_choose(5);
    settle(rig, "observed prediction tool use");
    assert!(
        view(rig)
            .get_rows()
            .iter()
            .any(|row| row.label.starts_with("Unused"))
    );
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
    view(rig).invoke_choose_plane(2);
    settle(rig, "the dense chroma quantizer map");
    let chroma_outlines = view(rig).get_outlines().row_count();
    assert!((1..=max_records).contains(&chroma_outlines));
    eprintln!(
        "Dense chroma overlay. {chroma_outlines} outlines. Median software redraw {:?}.",
        redraw_time(rig)
    );

    view(rig).invoke_choose_plane(1);
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
