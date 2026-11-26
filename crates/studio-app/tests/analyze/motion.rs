// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use std::fs;

use slint::platform::Key;
use slint::{ComponentHandle, Model};
use studio_app::BlockMotionView;
use studio_app::analysis::{BlockMotionInput, BlockOverlayInput, Input};
use studio_app::preview::{Dirty, Input as PreviewInput};
use studio_app::session::Session;
use studio_domain::preview::{PictureBox, PictureView};
use studio_domain::timeline::TimelineTarget;

use super::{Rig, Scratch, activate_analyze_key};

const TRANSLATION: &[u8] = include_bytes!(
    "../../../studio-engine/tests/preview/fixtures/syn-4frame-dct-adst-tools-off-200x104.ivf"
);
const CROSS_LAYER: &[u8] =
    include_bytes!("../../../studio-engine/tests/preview/fixtures/syn-mlayer2-crossref-10bit.obu");

pub(super) fn inspect(rig: &mut Rig, scratch: &Scratch) {
    translation(rig, scratch);
    cross_layer(rig, scratch);
}

fn translation(rig: &mut Rig, scratch: &Scratch) {
    open(rig, scratch, "subpixel motion.ivf", TRANSLATION, 11269);
    pick(rig, 84, 24, 200, 104);
    let motion = view(rig);
    assert!(motion.get_facts().iter().any(|fact| fact.value == "NEWMV"));
    assert!(
        motion
            .get_facts()
            .iter()
            .any(|fact| fact.value == "1/8 luma sample")
    );
    assert!(motion.get_predictors().iter().any(|row| {
        row.detail
            .contains("Column +12, row +4 in eighths of a luma sample")
            && row.detail.contains("Signaled difference")
    }));
    assert_eq!(motion.get_vectors().row_count(), 1);
    motion.invoke_set_overlay(true);
    motion.invoke_inspect_references();
    rig.idle("input slot for the selected motion predictor");
    assert!(view(rig).get_show_overlay());
    assert!(
        view(rig)
            .get_predictors()
            .iter()
            .any(|row| row.available && row.detail.contains("Coded byte 72"))
    );

    view(rig).invoke_preview_reference(0);
    let window = rig.window.clone_strong();
    rig.settle("the compact reference preview", |session| {
        studio_app::render_preview(
            &window,
            session.state(),
            session.preview(),
            Dirty {
                picture: true,
                view: true,
                thumbs: false,
            },
        );
        studio_app::render_block_reference(&window, session.analysis(), session.preview());
        window
            .global::<BlockMotionView>()
            .get_preview_picture()
            .size()
            .width
            > 0
    });
    assert!(picture(rig.session.preview().view().output, 11269));
    let extent = view(rig).get_preview_extent();
    assert!((extent.x - 0.3275).abs() < 0.00001);
    assert!((extent.width - 0.32).abs() < 0.00001);

    rig.session.preview_input(PreviewInput::Zoom(4));
    rig.settle("zoom over the selected prediction block", |session| {
        session
            .preview()
            .view()
            .picture
            .is_some_and(|frame| matches!(frame.header.view, PictureView::Zoom { factor: 4, .. }))
    });
    let source_view = rig
        .session
        .preview()
        .view()
        .picture
        .map(|frame| frame.header.view);
    activate_analyze_key(rig, &Input::BlockMotion(BlockMotionInput::Follow(0)));
    rig.settle("key follows the exact reference picture", |session| {
        picture(session.preview().view().output, 72) && !session.analysis().waiting()
    });
    rig.press(Key::Return);
    assert_eq!(rig.inputs.borrow().as_slice(), &[Input::Back]);
    rig.settle("Back restores the block and zoom", |session| {
        picture(session.preview().view().output, 11269) && !session.analysis().waiting()
    });
    assert_eq!(
        rig.session
            .preview()
            .view()
            .picture
            .map(|frame| frame.header.view),
        source_view
    );
    assert!(
        view(rig)
            .get_facts()
            .iter()
            .any(|fact| fact.value == "NEWMV")
    );
}

fn cross_layer(rig: &mut Rig, scratch: &Scratch) {
    open(rig, scratch, "cross-layer motion.obu", CROSS_LAYER, 1370);
    pick(rig, 24, 24, 160, 160);
    assert!(
        view(rig)
            .get_facts()
            .iter()
            .any(|fact| fact.value == "NEARMV")
    );
    assert!(
        view(rig)
            .get_facts()
            .iter()
            .any(|fact| fact.value == "1/4 luma sample")
    );
    view(rig).invoke_inspect_references();
    rig.idle("the block's cross-layer input slot");
    assert!(view(rig).get_predictors().iter().any(|row| {
        row.label.contains("slot 1")
            && row.detail.contains("Coded byte 776. Layer X0/M0/T0")
            && row.detail.contains("Signaled difference unused")
    }));
    view(rig).invoke_follow_reference(0);
    rig.settle("the retained picture in embedded layer 0", |session| {
        picture(session.preview().view().output, 776) && !session.analysis().waiting()
    });
    rig.press(Key::Escape);
    rig.session.analysis_input(Input::Back);
    rig.settle("the original block in embedded layer 1", |session| {
        picture(session.preview().view().output, 1370) && !session.analysis().waiting()
    });
    assert!(
        view(rig)
            .get_facts()
            .iter()
            .any(|fact| fact.value == "NEARMV")
    );

    select(rig, 1388);
    // AVM places the TIP block at coded luma (0, 0), size 16 × 8.
    pick(rig, 10, 10, 160, 160);
    assert!(
        view(rig)
            .get_predictors()
            .iter()
            .any(|row| row.label == "TIP" && !row.available)
    );
}

fn open(rig: &mut Rig, scratch: &Scratch, name: &str, bytes: &[u8], offset: u64) {
    rig.session.block_overlay_input(BlockOverlayInput::Mode(0));
    while rig.session.analysis().shown().is_some() {
        rig.session.analysis_input(Input::Close);
    }
    let path = scratch.0.join(name);
    fs::write(&path, bytes).unwrap_or_else(|error| unreachable!("{error}"));
    rig.session.analysis_input(Input::Open(vec![path]));
    rig.idle("the motion fixture");
    rig.session.preview_input(PreviewInput::Resized {
        stage: PictureBox::clamped(200, 160),
        thumb: PictureBox::clamped(64, 36),
    });
    rig.session.analysis_input(Input::TimelineBuild);
    rig.settle("the verified motion timeline", |session| {
        session
            .analysis()
            .shown()
            .and_then(|document| document.timeline().page())
            .is_some_and(|page| page.complete && page.verified)
    });
    select(rig, offset);
    rig.session.block_overlay_input(BlockOverlayInput::Mode(3));
    rig.settle("the block quantizer view", ready);
}

fn select(rig: &mut Rig, offset: u64) {
    rig.session
        .analysis_input(Input::TimelineSelect(TimelineTarget::CodedOffset {
            offset,
        }));
    rig.settle(
        "the picture that owns the selected coding block",
        |session| picture(session.preview().view().output, offset) && ready(session),
    );
}

fn picture(output: Option<studio_domain::preview::OutputEvent>, offset: u64) -> bool {
    output.is_some_and(|event| event.coded_offset == Some(offset))
}

fn ready(session: &Session) -> bool {
    session.preview().view().viewer.can_sample && !session.analysis().waiting()
}

fn pick(rig: &mut Rig, x: i32, y: i32, width: i32, height: i32) {
    rig.session.block_overlay_input(BlockOverlayInput::Pick {
        x,
        y,
        width,
        height,
    });
    rig.settle("the selected coding block and motion facts", ready);
}

fn view(rig: &Rig) -> BlockMotionView<'_> {
    rig.window.global::<BlockMotionView>()
}
