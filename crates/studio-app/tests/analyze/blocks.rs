// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use std::fs;

use slint::{ComponentHandle, Model};
use studio_app::Blocks;
use studio_app::analysis::Input;

use super::{Rig, Scratch, activate_analyze_key};

const MULTI_TILE: &[u8] = include_bytes!(
    "../../../studio-engine/tests/inspect/fixtures/syn-2tile-zone1-edge-128x64-q192.ivf"
);
const PADDED: &[u8] = include_bytes!(
    "../../../studio-engine/tests/inspect/fixtures/syn-mrl-padded-444-67x67-10bit-q120.ivf"
);

pub(super) fn inspect(rig: &mut Rig, scratch: &Scratch) {
    for (name, bytes, offset, quantizer) in [
        ("8-bit tiles.ivf", MULTI_TILE, "60", "Q 192"),
        ("10-bit padded.ivf", PADDED, "62", "Q 168"),
    ] {
        while rig.session.analysis().shown().is_some() {
            rig.session.analysis_input(Input::Close);
        }
        let path = scratch.0.join(name);
        fs::write(&path, bytes).unwrap_or_else(|error| unreachable!("{error}"));
        rig.session.analysis_input(Input::Open(vec![path]));
        rig.idle("the block evidence source");
        rig.session.analysis_input(Input::GoTo(offset.into()));
        rig.global().invoke_evidence_view(5);
        rig.idle("the selected coded frame");

        assert!(rig.window.global::<Blocks>().get_can_inspect());
        assert_eq!(rig.window.global::<Blocks>().get_rows().row_count(), 0);
        activate_analyze_key(rig, &Input::InspectBlocks);
        rig.inputs.borrow_mut().clear();
        rig.session.analysis_input(Input::InspectBlocks);
        rig.idle("decoder block records");
        let view = rig.window.global::<Blocks>();
        assert!(
            view.get_context()
                .contains(&format!("Coded byte {offset}."))
        );
        assert!(
            view.get_rows()
                .iter()
                .any(|row| row.quantizer.starts_with(quantizer))
        );
    }

    rig.session.analysis_input(Input::InspectBlocks);
    studio_app::render_analysis(&rig.window, rig.session.analysis());
    assert!(rig.window.global::<Blocks>().get_busy());
    rig.window.global::<Blocks>().invoke_cancel();
    assert!(
        rig.inputs
            .borrow_mut()
            .drain(..)
            .any(|input| input == Input::StopBlocks)
    );
    rig.session.analysis_input(Input::StopBlocks);
    studio_app::render_analysis(&rig.window, rig.session.analysis());
    assert!(rig.window.global::<Blocks>().get_stopping());
    rig.idle("the stopped decode");
    assert!(!rig.window.global::<Blocks>().get_busy());
}
