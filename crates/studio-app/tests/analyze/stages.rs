// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use std::cell::RefCell;
use std::fs;

use slint::language::ColorScheme;
use slint::platform::Key;
use slint::{ComponentHandle, Image, Model};
use studio_app::StageInspector;
use studio_app::analysis::{Input, StageInput};
use studio_app::preview::Dirty;

use super::{Rig, Scratch, activate_analyze_key};

const GRAIN: &[u8] = include_bytes!(
    "../../../studio-engine/tests/preview/fixtures/syn-3frame-filmgrain-inter-64x64-10bit.ivf"
);

pub(super) fn inspect(rig: &mut Rig, scratch: &Scratch, clipboard: &RefCell<String>) {
    while rig.session.analysis().shown().is_some() {
        rig.session.analysis_input(Input::Close);
    }
    let path = scratch.0.join("film grain stages.ivf");
    fs::write(&path, GRAIN).unwrap_or_else(|error| unreachable!("{error}"));
    rig.session.analysis_input(Input::Open(vec![path]));
    rig.idle("the film grain source");
    rig.session.analysis_input(Input::GoTo("2412".into()));
    rig.global().invoke_evidence_view(6);
    rig.idle("the third coded frame");
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
    assert!(view(rig).get_can_capture());
    assert!(!view(rig).get_has_evidence());

    view(rig).invoke_choose_grain(true);
    rig.idle("the optional grain policy");
    activate_analyze_key(
        rig,
        &Input::Stage(StageInput::Capture(0, "0".into(), "0".into(), true)),
    );
    rig.idle("captured native decoder stages");
    assert!(view(rig).get_has_evidence(), "{}", view(rig).get_status());
    let choices = view(rig).get_choices();
    assert!(
        choices.iter().any(|row| row.contains("Skipped")),
        "{:?}",
        choices.iter().collect::<Vec<_>>()
    );
    view(rig).invoke_choose(false, 8);
    view(rig).invoke_choose(true, 9);
    view(rig).invoke_choose_mode(1);
    view(rig).invoke_sample("3".into(), "5".into());
    rig.idle("the exact signed difference");
    assert!(view(rig).get_label_b().contains("Signed B − A"));
    assert!(view(rig).get_label_a().contains("Reference reconstruction"));
    assert!(view(rig).get_label_b().contains("Grain presentation"));
    let rows = view(rig).get_sample_values();
    assert_eq!(rows.row_count(), 4);
    assert!(rows.row_data(2).is_some_and(|row| row.value == "-10"));
    let exact = view(rig).get_sample_text();
    assert_eq!(
        exact.as_str(),
        "Y (3, 5)\nA 120\nB 110\nB − A -10\n|B − A| 10"
    );
    let before: Image = view(rig).get_image_b();
    view(rig).invoke_choose_gain(2);
    view(rig).invoke_choose_zoom(2);
    view(rig).invoke_pan(2, 3);
    rig.idle("linked magnification, pan and display gain");
    assert!(view(rig).get_label_a().contains("(2, 3) · 8 × 8"));
    assert!(view(rig).get_label_b().contains("(2, 3) · 8 × 8"));
    assert_ne!(before.size(), view(rig).get_image_b().size());
    assert_eq!(view(rig).get_sample_text(), exact);
    assert!(view(rig).get_can_copy());
    for _ in 0..64 {
        rig.press(Key::Tab);
        rig.press(Key::Return);
        rig.inputs.borrow_mut().clear();
        if clipboard.borrow().as_str() == exact.as_str() {
            break;
        }
        rig.press(Key::Escape);
    }
    assert_eq!(clipboard.borrow().as_str(), exact.as_str());
    rig.snapshot(ColorScheme::Light, 1.0, "stages-light-1x.ppm");
    rig.snapshot(ColorScheme::Dark, 2.0, "stages-dark-2x.ppm");

    view(rig).invoke_choose(false, 1);
    rig.idle("different stage domains");
    assert!(view(rig).get_label_b().contains("Difference unavailable"));
    assert!(!view(rig).get_visual_b());
    assert!(!view(rig).get_sample_text().contains("B − A"));

    rig.session.analysis_input(Input::GoTo("2217".into()));
    rig.idle("the second coded frame");
    assert!(!view(rig).get_has_evidence());
    rig.session.analysis_input(Input::Stage(StageInput::Capture(
        0,
        "0".into(),
        "0".into(),
        false,
    )));
    rig.session.analysis_input(Input::Stage(StageInput::Cancel));
    rig.idle("the cancelled stage request");
    assert!(!view(rig).get_busy());
    assert!(
        view(rig).get_status().contains("stopped"),
        "{}",
        view(rig).get_status()
    );
}

fn view(rig: &Rig) -> StageInspector<'_> {
    rig.window.global()
}
