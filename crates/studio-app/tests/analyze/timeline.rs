// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use studio_domain::preview::PictureBox;

use super::*;

pub(super) fn controls(rig: &mut Rig, scratch: &Scratch) {
    use slint::language::ColorScheme;
    use studio_app::TimelineUi;
    use studio_domain::timeline::TimelineView;

    while rig.session.analysis().shown().is_some() {
        rig.session.analysis_input(Input::Close);
    }
    check_lossless_quantizer(rig, scratch);
    let ivf = scratch.0.join("three timed pictures.ivf");
    fs::write(
        &ivf,
        include_bytes!(
            "../../../studio-engine/tests/preview/fixtures/syn-3frame-filmgrain-inter-64x64-10bit.ivf"
        ),
    )
    .unwrap_or_else(|e| unreachable!("{e}"));
    rig.session.analysis_input(Input::Open(vec![ivf]));
    rig.idle("timeline fixture structure");
    rig.window.global::<TimelineUi>().set_expanded(true);
    rig.window.global::<TimelineUi>().invoke_build();
    rig.settle("the complete coded timeline", |session| {
        session.analysis().shown().is_some_and(|doc| {
            doc.timeline().page().is_some_and(|page| {
                page.complete && page.verified && page.view == TimelineView::Coded
            })
        })
    });

    assert_eq!(
        rig.window.global::<TimelineUi>().get_status(),
        "Timeline complete. Source verified."
    );
    assert_eq!(rig.window.global::<TimelineUi>().get_bars().row_count(), 3);
    assert!(
        rig.window
            .global::<TimelineUi>()
            .get_summary()
            .contains("framing")
    );
    assert!(
        rig.window
            .global::<TimelineUi>()
            .get_exact_statistics()
            .contains("Header base quantizer")
    );
    assert!(
        rig.window
            .global::<TimelineUi>()
            .get_scope()
            .contains("continuation OBUs")
    );

    rig.tab_to(Key::RightArrow, |input| {
        matches!(input, Input::TimelineStep(1))
    });
    rig.inputs
        .borrow_mut()
        .retain(|input| matches!(input, Input::TimelineStep(_)));
    rig.idle("the exact row chosen with the keyboard");
    assert_eq!(rig.window.global::<TimelineUi>().get_selected_index(), "0");
    assert!(
        rig.window
            .global::<TimelineUi>()
            .get_selection_detail()
            .contains("Coded 0")
    );
    assert!(rig.selected().is_some());
    assert!(rig.global().get_selected_offset().parse::<u64>().is_ok());
    for (scheme, scale, file_name) in [
        (
            ColorScheme::Light,
            1.0,
            "timeline-coded-keyboard-light-1x.ppm",
        ),
        (
            ColorScheme::Dark,
            2.0,
            "timeline-coded-keyboard-dark-2x.ppm",
        ),
    ] {
        rig.snapshot(scheme, scale, file_name);
    }

    check_table(rig);

    rig.window.global::<TimelineUi>().invoke_zoom(1);
    rig.idle("the visible zoom range");
    assert_eq!(rig.window.global::<TimelineUi>().get_bars().row_count(), 2);
    check_output_order(rig);
    check_stopped_timeline(rig, scratch);
}

fn check_stopped_timeline(rig: &mut Rig, scratch: &Scratch) {
    use studio_app::TimelineUi;

    rig.session.analysis_input(Input::Close);
    let source = scratch.0.join("partial IVF record.ivf");
    let mut bytes = include_bytes!(
        "../../../studio-engine/tests/preview/fixtures/syn-luma-palette2-row-mono-16x16-q0.ivf"
    )
    .to_vec();
    bytes.push(1);
    fs::write(&source, bytes).unwrap_or_else(|error| unreachable!("{error}"));
    rig.session.analysis_input(Input::Open(vec![source]));
    rig.idle("the source with an incomplete last record");
    rig.window.global::<TimelineUi>().invoke_build();
    rig.idle("the retained timeline after a scan error");

    let timeline = rig.window.global::<TimelineUi>();
    let reason = "The IVF file ends inside a record.";
    assert_eq!(timeline.get_bars().row_count(), 1);
    assert!(timeline.get_status().contains(reason));
    assert!(timeline.get_exact_statistics().contains(reason));
}

fn check_lossless_quantizer(rig: &mut Rig, scratch: &Scratch) {
    use studio_app::TimelineUi;

    let source = scratch.0.join("lossless monochrome.ivf");
    fs::write(
        &source,
        include_bytes!(
            "../../../studio-engine/tests/preview/fixtures/syn-luma-palette2-row-mono-16x16-q0.ivf"
        ),
    )
    .unwrap_or_else(|error| unreachable!("{error}"));
    rig.session.analysis_input(Input::Open(vec![source]));
    rig.idle("the lossless source structure");
    rig.window.global::<TimelineUi>().invoke_build();
    rig.idle("the lossless source timeline");
    assert_eq!(rig.window.global::<TimelineUi>().get_bars().row_count(), 1);
    assert!(
        rig.window
            .global::<TimelineUi>()
            .get_exact_statistics()
            .contains("Header base quantizer 0 (signaled)")
    );
    rig.session.analysis_input(Input::Close);
}

fn check_table(rig: &mut Rig) {
    use slint::language::ColorScheme;
    use studio_app::TimelineUi;

    rig.window.global::<TimelineUi>().set_show_table(true);
    rig.tab_to(Key::Return, |input| {
        matches!(
            input,
            Input::TimelineSelect(studio_domain::timeline::TimelineTarget::Coded { .. })
        )
    });
    rig.inputs
        .borrow_mut()
        .retain(|input| matches!(input, Input::TimelineSelect(_)));
    rig.idle("an exact row selected from the table");
    assert!(rig.window.global::<TimelineUi>().get_selected());
    rig.snapshot(
        ColorScheme::Light,
        1.0,
        "timeline-table-keyboard-light-1x.ppm",
    );
    rig.window.global::<TimelineUi>().set_show_table(false);
}

fn check_output_order(rig: &mut Rig) {
    use studio_app::TimelineUi;

    rig.window.global::<TimelineUi>().invoke_reorder(1);
    rig.idle("the complete output timeline");
    assert_eq!(rig.window.global::<TimelineUi>().get_bars().row_count(), 3);
    assert_eq!(rig.window.global::<TimelineUi>().get_selected_index(), "0");
    assert!(
        rig.window
            .global::<TimelineUi>()
            .get_scope()
            .contains("display OBU once")
    );
    rig.window.global::<TimelineUi>().invoke_zoom(1);
    rig.idle("an output range with a following timestamp");
    assert!(
        rig.window
            .global::<TimelineUi>()
            .get_summary()
            .contains("bit/s")
    );
    assert!(rig.window.global::<TimelineUi>().get_can_go_to_time());
    let size = PictureBox::clamped(320, 180);
    rig.session
        .preview_input(studio_app::preview::Input::Resized {
            stage: size,
            thumb: size,
        });
    rig.window
        .global::<TimelineUi>()
        .invoke_go_to(2, "1/100".into());
    rig.idle("a picture selected from exact time");
    assert_eq!(rig.window.global::<TimelineUi>().get_selected_index(), "1");
    check_picture_and_byte(rig, 1);
    assert!(
        rig.window
            .global::<TimelineUi>()
            .get_selection_detail()
            .contains("Display OBU byte")
    );
    rig.window.global::<TimelineUi>().invoke_findings();
    rig.idle("the linked findings tab");
    assert_eq!(rig.global().get_evidence_tab(), 2);

    rig.global().invoke_back();
    rig.idle("the previous linked selection");
    check_picture_and_byte(rig, 0);
    rig.global().invoke_forward();
    rig.idle("the restored linked selection");
    check_picture_and_byte(rig, 1);
}

fn check_picture_and_byte(rig: &mut Rig, expected: u64) {
    rig.settle("the linked picture", |session| {
        let view = session.preview().view();
        view.picture.is_some() && view.output.is_some_and(|output| output.ordinal == expected)
    });
    let doc = rig
        .session
        .analysis()
        .shown()
        .unwrap_or_else(|| unreachable!("the open source"));
    let selection = doc
        .timeline()
        .selection()
        .unwrap_or_else(|| unreachable!("the linked row"));
    let offset = selection.showing_offset.unwrap_or(selection.coded_offset);
    assert_eq!(selection.output_ordinal, Some(expected));
    assert_eq!(doc.bytes().cursor, offset);
    assert!(rig.byte(usize::try_from(offset).unwrap_or(0)).2);
}
