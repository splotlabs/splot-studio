// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use std::cell::RefCell;

use slint::platform::Key;
use slint::{ComponentHandle, Model};
use studio_app::EntropyView;
use studio_app::analysis::{BlockOverlayInput, Input};

use super::{Rig, Scratch, overlays};

const RECTANGLE: &[u8] =
    include_bytes!("../../../studio-engine/tests/inspect/fixtures/syn-hrect-intra-64x64-q120.ivf");

pub(super) fn inspect(rig: &mut Rig, scratch: &Scratch, clipboard: &RefCell<String>) {
    overlays::open(rig, scratch, "entropy symbols.ivf", RECTANGLE);
    rig.session.block_overlay_input(BlockOverlayInput::Mode(1));
    rig.idle("the coding block map");
    let pick = |rig: &mut Rig, y| {
        rig.session.block_overlay_input(BlockOverlayInput::Pick {
            x: 42,
            y,
            width: 160,
            height: 160,
        });
    };
    pick(rig, 42);
    rig.session.analysis_input(Input::EvidenceView(7));
    rig.idle("the entropy scope");
    assert!(view(rig).get_can_read());
    assert_eq!(view(rig).get_rows().row_count(), 0, "capture is opt-in");

    view(rig).invoke_read();
    rig.idle("the entropy symbols of the upper block");
    assert!(view(rig).get_scope().contains("Coding block (0, 0)"));
    assert!(view(rig).get_accounting().contains("estimated bits"));
    assert!(view(rig).get_accounting().contains("Tile payload"));
    assert!(view(rig).get_statistics().contains("captured blocks"));
    assert!(
        view(rig)
            .get_copied_capture()
            .starts_with("Entropy capture TSV 1\n")
    );
    assert!(view(rig).get_copied_capture().contains("Coverage\t"));

    view(rig).invoke_search("all_zero".into());
    rig.idle("the filtered transform start symbols");
    assert!(view(rig).get_rows().row_count() > 0);
    assert!(view(rig).get_can_read_transform());
    assert!(view(rig).get_detail().contains("Q15"));
    view(rig).invoke_read_transform();
    rig.idle("the owning transform");
    assert!(view(rig).get_scope().contains("transform"));
    let row = view(rig)
        .get_rows()
        .row_data(0)
        .unwrap_or_else(|| unreachable!("symbol"));
    assert_eq!(row.ordinal, "0");
    assert_eq!(row.operation, "all_zero");
    copy_with_keyboard(rig, clipboard);
    assert!(!view(rig).get_can_read_transform());
    assert!(view(rig).get_can_read_block());
    view(rig).invoke_read_block();
    rig.idle("the parent coding block");
    assert!(!view(rig).get_scope().contains(" transform"));
    assert!(view(rig).get_rows().row_count() > 0);

    pick(rig, 118);
    rig.idle("the new coding block");
    assert!(view(rig).get_scope().contains("Coding block (0, 32)"));
    assert_eq!(view(rig).get_rows().row_count(), 0);
    assert!(view(rig).get_copied_capture().is_empty());
}

fn copy_with_keyboard(rig: &mut Rig, clipboard: &RefCell<String>) {
    let expected = view(rig).get_copied_symbol();
    assert!(expected.contains("CDF Q15 cutoffs"));
    assert!(expected.contains("Source SHA-256"));
    for _ in 0..60 {
        rig.press(Key::Tab);
        rig.chord(Key::Control, "c");
        if clipboard.borrow().as_str() == expected.as_str() {
            return;
        }
    }
    assert_eq!(clipboard.borrow().as_str(), expected.as_str());
}

fn view(rig: &Rig) -> EntropyView<'_> {
    rig.window.global::<EntropyView>()
}
