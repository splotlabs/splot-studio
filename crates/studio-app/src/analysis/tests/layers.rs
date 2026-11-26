// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use studio_domain::layers::{LayerDecodeSelection, LayerEvidence};

use super::{Command, Effect, Event, Input, doc, opened};

#[test]
fn a_cancelled_layer_read_cannot_replace_the_new_anchor() {
    let (mut analysis, engine) = opened(4);
    let selected = analysis.input(Input::RowClicked(0));
    engine.settle(&mut analysis, selected);
    let first = analysis.input(Input::EvidenceView(3));
    let inspection = layer_request(&first, 0);

    let moved = analysis.input(Input::RowClicked(2));
    assert!(moved.iter().any(|effect| matches!(
        effect.command,
        Command::AnalysisCancelLayers {
            document: 1,
            inspection: cancelled
        } if cancelled == inspection
    )));
    assert!(
        moved
            .iter()
            .all(|effect| !matches!(effect.command, Command::AnalysisLayers { .. })),
        "the first read has yet to finish"
    );
    engine.settle(&mut analysis, moved);

    // A successful result can race the cancel acknowledgement.
    let current = analysis.event(inspection, ready(1, 0, 1));
    assert!(doc(&analysis).layers.current.is_none());
    let current_id = layer_request(&current, 2);
    assert_eq!(analysis.event(current_id, ready(2, 2, 19)), Vec::new());
    assert!(doc(&analysis).layers.current.is_none());
    assert_eq!(analysis.event(current_id, ready(1, 2, 19)), Vec::new());
    assert_eq!(
        doc(&analysis)
            .layers
            .current
            .as_ref()
            .map(|page| page.evidence.offset),
        Some(19)
    );
    assert_eq!(analysis.event(inspection, ready(1, 0, 1)), Vec::new());
    assert_eq!(
        doc(&analysis)
            .layers
            .current
            .as_ref()
            .map(|page| page.evidence.offset),
        Some(19)
    );
}

fn layer_request(effects: &[Effect], element: u64) -> u64 {
    effects
        .iter()
        .find_map(|effect| {
            matches!(effect.command, Command::AnalysisLayers {
            document: 1,
            element: selected,
            selection: LayerDecodeSelection::Default,
        } if selected == element)
            .then_some(effect.request_id)
        })
        .unwrap_or_else(|| unreachable!("expected layer inspection for element {element}"))
}

fn ready(document: u64, element: u64, offset: u64) -> Event {
    Event::AnalysisLayersReady {
        document,
        evidence: Box::new(LayerEvidence {
            element,
            offset,
            temporal_unit: Some(0),
            selection: LayerDecodeSelection::Default,
            membership_reason: None,
            metadata_complete: true,
            catalog_limited: false,
        }),
        total: 0,
    }
}
