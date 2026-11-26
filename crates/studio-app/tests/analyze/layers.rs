// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Layers and provenance in the real window, using the real inspection process.

use std::fs;

use slint::platform::Key;
use slint::{ComponentHandle, Model};
use studio_app::analysis::{Input, LayerInput};
use studio_app::{LayerDetail, Layers};
use studio_domain::analysis::Element;
use studio_domain::layers::{LayerDecodeSelection, LayerScope};
use studio_domain::preview::PictureBox;
use studio_domain::timeline::TimelineTarget;

use super::{Rig, Scratch};

const CONTENT: &[u8] =
    include_bytes!("../../../studio-engine/tests/inspect/fixtures/syn-output-ci-2frame-64x64.ivf");
const DEFAULT_PRIORITY: &[u8] =
    include_bytes!("../../../studio-engine/tests/inspect/fixtures/syn-ci-default-priority.obu");
const POINTS: &[u8] =
    include_bytes!("../../../studio-engine/tests/preview/fixtures/syn-msdo-ops-random-access.obu");

pub(super) fn inspect(rig: &mut Rig, scratch: &Scratch) {
    effective_metadata_and_origin(rig, scratch);
    inherited_unspecified_metadata(rig, scratch);
    requested_selection_and_visible_filter(rig, scratch);
}

fn effective_metadata_and_origin(rig: &mut Rig, scratch: &Scratch) {
    open(rig, scratch, "content interpretation.ivf", CONTENT, 83);
    assert!(
        rig.window
            .global::<Layers>()
            .get_status()
            .contains("Temporal unit 1")
    );
    let ci = record(rig, |row| row.label == "Effective content interpretation");
    let layer = rig.window.global::<Layers>();
    let effective = layer
        .get_rows()
        .row_data(usize::try_from(ci).unwrap_or(usize::MAX))
        .unwrap_or_else(|| unreachable!("expected effective metadata"));
    assert_eq!(effective.origin, "Show OBU at byte 71");
    assert_eq!(effective.context, "Extended layer 0, embedded 0");
    assert_eq!(effective.value, "Signaled for this layer");

    assert!(layer.get_rows().iter().any(|row| {
        row.label == "Aspect ratio and timing"
            && row.value == "Sample aspect ratio unspecified · Display tick 1/30 s"
    }));

    // Exercise the actual focus traversal and a source-record button.
    let tabs = rig.tab_to(Key::Return, |input| {
        matches!(input, Input::Layer(LayerInput::Origin(_)))
    });
    assert!(tabs < 30);
    let keyboard_origin = {
        let mut queued = rig.inputs.borrow_mut();
        let action = queued
            .iter()
            .find(|input| matches!(input, Input::Layer(LayerInput::Origin(_))))
            .cloned();
        queued.clear();
        action
    };
    let Some(Input::Layer(LayerInput::Origin(index))) = keyboard_origin else {
        unreachable!("expected a keyboard source link");
    };
    let origin = rig
        .window
        .global::<Layers>()
        .get_rows()
        .row_data(usize::try_from(index).unwrap_or(usize::MAX))
        .and_then(|row| {
            row.origin
                .strip_prefix("Show OBU at byte ")?
                .parse::<u64>()
                .ok()
        });
    rig.session
        .analysis_input(Input::Layer(LayerInput::Origin(index)));
    rig.idle("the keyboard source link");
    assert_eq!(selected_header(rig), origin);
    assert_eq!(rig.global().get_evidence_tab(), 1);

    rig.session.analysis_input(Input::GoTo("83".into()));
    rig.global().invoke_evidence_view(3);
    rig.idle("the selected metadata context again");
    let ci = record(rig, |row| row.label == "Effective content interpretation");
    rig.window.global::<Layers>().invoke_open_origin(ci);
    rig.idle("the exact content-interpretation OBU");
    assert_eq!(selected_header(rig), Some(71));
    assert_eq!(rig.global().get_evidence_tab(), 1);
    rig.global()
        .invoke_evidence_search("ci_scan_type_idc".into());
    rig.idle("the native content-interpretation syntax field");
    assert!(
        rig.global()
            .get_evidence_rows()
            .iter()
            .any(|row| row.label == "ci_scan_type_idc")
    );
}

fn inherited_unspecified_metadata(rig: &mut Rig, scratch: &Scratch) {
    open(
        rig,
        scratch,
        "default CI priority.obu",
        DEFAULT_PRIORITY,
        504,
    );
    let inherited = record(rig, |row| {
        row.label == "Effective content interpretation"
            && row.context == "Extended layer 0, embedded 2"
            && row.value == "Unspecified · Inherited from embedded layer 1"
            && row.origin == "Show OBU at byte 504"
    });
    rig.window.global::<Layers>().invoke_open_origin(inherited);
    rig.idle("the random-access reset for inherited defaults");
    assert_eq!(selected_header(rig), Some(504));
    assert_eq!(rig.global().get_evidence_tab(), 1);
}

fn requested_selection_and_visible_filter(rig: &mut Rig, scratch: &Scratch) {
    open(rig, scratch, "operating points.obu", POINTS, 208);
    let index = record(rig, |row| {
        row.label == "Operating-point set 0, point 1"
            && row.context == "Global scope"
            && row.can_request
    });
    let request = Input::Layer(LayerInput::Decode(index));
    let _ = rig.tab_to(Key::Return, |input| input == &request);
    rig.inputs.borrow_mut().clear();
    rig.session.analysis_input(Input::TimelineBuild);
    rig.session.analysis_input(request);
    rig.idle("the keyboard-selected operating point");
    let expected = LayerDecodeSelection::OperatingPoint {
        scope: LayerScope::Global,
        set: 0,
        point: 1,
    };
    let selected = |rig: &Rig| {
        rig.session
            .analysis()
            .shown()
            .map(studio_app::analysis::Document::layer_decode_selection)
    };
    assert_eq!(selected(rig), Some(expected));
    assert_eq!(
        rig.window.global::<Layers>().get_request(),
        "Requested playback point · Global scope · Set 0, point 1"
    );
    assert!(
        rig.window
            .global::<Layers>()
            .get_decode()
            .contains("yet to produce")
    );

    let timeline = |rig: &Rig| {
        rig.session
            .analysis()
            .shown()
            .map(studio_app::analysis::Document::timeline)
            .and_then(studio_app::analysis::Timeline::scan)
    };
    assert_eq!(timeline(rig), None);
    rig.session.analysis_input(Input::TimelineBuild);
    rig.idle("the timeline for the requested operating point");
    let scan = timeline(rig);
    assert!(scan.is_some());
    let page = rig
        .session
        .analysis()
        .shown()
        .and_then(|doc| doc.timeline().page())
        .unwrap_or_else(|| unreachable!("expected a policy-specific timeline"));
    assert_eq!(page.decode_selection, expected);
    assert!(page.complete && page.verified);

    let size = PictureBox::clamped(320, 180);
    rig.session
        .preview_input(studio_app::preview::Input::Resized {
            stage: size,
            thumb: size,
        });
    rig.session
        .analysis_input(Input::TimelineSelect(TimelineTarget::Output { index: 0 }));
    rig.settle("playback of the selected timeline generation", |session| {
        let preview = session.preview().view();
        preview.output.is_some_and(|output| output.ordinal == 0) && !session.analysis().waiting()
    });
    let output = rig
        .session
        .preview()
        .view()
        .output
        .unwrap_or_else(|| unreachable!("expected the selected output picture"));
    assert_eq!(output.extended_layer, 3);
    assert_eq!(
        rig.window.global::<Layers>().get_decode(),
        "Playback produced a picture for this request."
    );
    let anchor = selected_header(rig);
    assert_eq!(anchor, output.showing_offset.or(output.coded_offset));
    assert!(anchor.is_some());

    rig.global().invoke_layer_filter(1);
    rig.idle("the independent visibility filter");
    assert!(rig.global().get_filtered());
    assert_eq!(selected(rig), Some(expected));
    assert_eq!(selected_header(rig), anchor);
    assert_eq!(timeline(rig), scan);

    rig.window.global::<Layers>().invoke_reset_point();
    rig.idle("the default operating point");
    assert_eq!(selected(rig), Some(LayerDecodeSelection::Default));
    assert_eq!(timeline(rig), None);
    assert!(rig.global().get_filtered());
}

fn open(rig: &mut Rig, scratch: &Scratch, name: &str, bytes: &[u8], offset: u64) {
    rig.session.analysis_input(Input::Close);
    let path = scratch.0.join(name);
    fs::write(&path, bytes).unwrap_or_else(|error| unreachable!("{error}"));
    rig.session.analysis_input(Input::Open(vec![path]));
    rig.idle("the AV2 layer fixture");
    rig.session.analysis_input(Input::GoTo(offset.to_string()));
    rig.global().invoke_evidence_view(3);
    rig.idle("the layer inspector");
    assert!(rig.window.global::<Layers>().get_shown());
    assert_eq!(selected_header(rig), Some(offset));
}

fn record(rig: &mut Rig, predicate: impl Fn(&LayerDetail) -> bool) -> i32 {
    for _ in 0..8 {
        let layers = rig.window.global::<Layers>();
        if let Some(index) = layers.get_rows().iter().position(|row| predicate(&row)) {
            return i32::try_from(index).unwrap_or(i32::MAX);
        }
        assert!(layers.get_has_next(), "{}", layers.get_status());
        layers.invoke_move_page(1);
        rig.idle("the next layer page");
    }
    unreachable!("expected layer evidence within eight pages")
}

fn selected_header(rig: &Rig) -> Option<u64> {
    let document = rig.session.analysis().shown()?;
    let Element::Obu { obu } = &document.selected()?.row.element else {
        return None;
    };
    Some(obu.header.offset)
}
