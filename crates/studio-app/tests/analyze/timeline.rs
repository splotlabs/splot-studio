// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use super::*;

pub(super) fn controls(rig: &mut Rig, scratch: &Scratch) {
    use slint::language::ColorScheme;
    use studio_app::TimelineUi;
    use studio_domain::timeline::TimelineView;

    while rig.session.analysis().shown().is_some() {
        rig.session.analysis_input(Input::Close);
    }
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
        timeline_snapshot(rig, scheme, scale, file_name);
    }

    check_table(rig);

    rig.window.global::<TimelineUi>().invoke_zoom(1);
    rig.idle("the visible zoom range");
    assert_eq!(rig.window.global::<TimelineUi>().get_bars().row_count(), 2);
    check_output_order(rig);
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
    timeline_snapshot(
        rig,
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
    rig.window
        .global::<TimelineUi>()
        .invoke_go_to(2, "1/100".into());
    rig.idle("a picture selected from exact time");
    assert_ne!(rig.window.global::<TimelineUi>().get_selected_index(), "0");
    assert!(
        rig.window
            .global::<TimelineUi>()
            .get_selection_detail()
            .contains("Display OBU byte")
    );
    rig.window.global::<TimelineUi>().invoke_findings();
    rig.idle("the linked findings tab");
    assert_eq!(rig.global().get_evidence_tab(), 2);
}

fn timeline_snapshot(rig: &Rig, scheme: slint::language::ColorScheme, scale: f32, file_name: &str) {
    use std::io::Write;
    let Some(folder) = std::env::var_os("SPLOT_STUDIO_TEST_SNAPSHOTS").map(PathBuf::from) else {
        return;
    };
    fs::create_dir_all(&folder).unwrap_or_else(|e| unreachable!("{e}"));
    let native = rig.window.window();
    let size = native.size();
    let palette = rig.window.global::<studio_app::Palette>();
    palette.set_color_scheme(scheme);
    native.dispatch_event(WindowEvent::ScaleFactorChanged {
        scale_factor: scale,
    });
    native.set_size(slint::LogicalSize::new(920.0, 1080.0));
    native.request_redraw();
    let width = native.size().width;
    let height = native.size().height;
    let mut pixels = vec![Rgb8Pixel::default(); (width * height) as usize];
    assert!(rig.platform.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, width as usize);
    }));
    let mut out = fs::File::create(folder.join(file_name)).unwrap_or_else(|e| unreachable!("{e}"));
    write!(
        out,
        "P6
{width} {height}
255
"
    )
    .unwrap_or_else(|e| unreachable!("{e}"));
    let rgb: Vec<_> = pixels.iter().flat_map(|p| [p.r, p.g, p.b]).collect();
    out.write_all(&rgb).unwrap_or_else(|e| unreachable!("{e}"));
    native.dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: 1.0 });
    native.set_size(size);
    palette.set_color_scheme(slint::language::ColorScheme::Light);
}
