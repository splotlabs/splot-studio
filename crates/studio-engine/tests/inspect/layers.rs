// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use std::error::Error;
use std::fs;

use studio_domain::identity::{IdentityCompletion, IdentityKind, IdentityRow};
use studio_domain::layers::{
    ContentState, LayerDecodeSelection, LayerEvidence, LayerFact, LayerRow, LayerScope,
    OperatingPointLayerSource,
};

use studio_domain::preview::GrainPolicy;

use super::{
    AnalysisFailure, Command, Element, Event, Inspect, LocateTarget, RowFilter, SyntaxEvent,
};

type TestResult = Result<(), Box<dyn Error>>;
type Report = (u64, LayerEvidence, Vec<LayerRow>);

fn report(
    inspect: &mut Inspect,
    element: u64,
    selection: LayerDecodeSelection,
) -> Result<Report, Box<dyn Error>> {
    let inspection = inspect.next_id;
    let answer = inspect.ask(Command::AnalysisLayers {
        document: 1,
        element,
        selection,
    })?;
    let Event::AnalysisLayersReady {
        document: 1,
        evidence,
        total,
    } = answer
    else {
        return Err(format!("Expected layer evidence. Got {answer:?}").into());
    };
    let mut rows = Vec::new();
    while (rows.len() as u64) < total {
        let first = rows.len() as u64;
        let answer = inspect.ask(Command::AnalysisLayerPage {
            document: 1,
            inspection,
            first,
            count: 8,
        })?;
        let Event::AnalysisLayerPage {
            document: 1,
            inspection: returned,
            first: start,
            total: reported,
            rows: entries,
        } = answer
        else {
            return Err(format!("Expected a layer page. Got {answer:?}").into());
        };
        assert_eq!((returned, start, reported), (inspection, first, total));
        if entries.is_empty() {
            return Err("The layer pager returned no rows before the last page".into());
        }
        rows.extend(entries);
    }
    assert_eq!(total, rows.len() as u64);
    Ok((inspection, *evidence, rows))
}

fn assert_member(rows: &[LayerRow], scope: LayerScope, temporal: Option<u8>, member: Option<bool>) {
    assert!(
        rows.iter().any(|row| {
            row.scope == scope
                && matches!(&row.fact, LayerFact::Presence {
            temporal: found_temporal, member: found_member, ..
        } if temporal.is_none_or(|id| id == *found_temporal) && *found_member == member)
        }),
        "Missing membership {member:?} for {scope:?} at temporal layer {temporal:?}"
    );
}

#[test]
fn scoped_operating_point_membership_is_distinct_from_presence() -> TestResult {
    let mut inspect = Inspect::spawn()?;
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("layers.obu");
    fs::write(
        &source,
        include_bytes!("../preview/fixtures/syn-msdo-ops-random-access.obu"),
    )?;
    let opened = inspect.open(1, &source)?;
    let rows = inspect.all(1, opened.elements)?;
    let anchor = rows
        .iter()
        .rposition(|row| matches!(row, Element::Obu { .. }))
        .ok_or("Missing frame OBU")? as u64;
    let selection = LayerDecodeSelection::OperatingPoint {
        scope: LayerScope::Global,
        set: 0,
        point: 1,
    };
    let (first, summary, rows) = report(&mut inspect, anchor, selection)?;
    assert_eq!(summary.selection, selection);
    assert!(summary.membership_reason.is_none());
    assert!(rows.iter().any(|row| row.scope == LayerScope::Global
        && matches!(row.fact, LayerFact::Composition { streams: 2, .. })));
    assert_member(&rows, LayerScope::Global, None, None);
    assert_member(&rows, LayerScope::Extended { id: 3 }, None, Some(true));
    assert_member(&rows, LayerScope::Extended { id: 7 }, None, Some(false));
    assert!(rows.iter().any(|row| {
        matches!(&row.fact, LayerFact::OperatingPoint { set: 1, point: Some(0), layers, .. }
            if layers.iter().any(|layer|
                layer.xlayer == 3 && layer.source == OperatingPointLayerSource::Inherited { set: 0, point: 1 }))
    }));
    let local = LayerDecodeSelection::OperatingPoint {
        scope: LayerScope::Extended { id: 3 },
        set: 2,
        point: 0,
    };
    let (_, local_summary, local_rows) = report(&mut inspect, anchor, local)?;
    assert!(local_summary.membership_reason.is_none());
    assert_member(
        &local_rows,
        LayerScope::Extended { id: 3 },
        Some(1),
        Some(false),
    );
    assert_member(
        &local_rows,
        LayerScope::Extended { id: 7 },
        None,
        Some(true),
    );
    let missing = LayerDecodeSelection::OperatingPoint {
        scope: LayerScope::Global,
        set: 15,
        point: 0,
    };
    let (_, summary, rows) = report(&mut inspect, anchor, missing)?;
    assert!(summary.membership_reason.is_some());
    assert!(rows.iter().any(|row| matches!(
        row.fact,
        LayerFact::OperatingPoint {
            set: 0,
            point: Some(1),
            ..
        }
    )));
    assert!(matches!(
        inspect.ask(Command::AnalysisLayerPage {
            document: 1,
            inspection: first,
            first: 0,
            count: 8
        })?,
        Event::AnalysisFailed {
            code: AnalysisFailure::InvalidRequest,
            ..
        }
    ));
    Ok(())
}

#[test]
fn completed_temporal_unit_links_effective_ci_to_its_syntax() -> TestResult {
    let mut inspect = Inspect::spawn()?;
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("ci.ivf");
    fs::write(
        &source,
        include_bytes!("fixtures/syn-output-ci-2frame-64x64.ivf"),
    )?;
    let opened = inspect.open(1, &source)?;
    let elements = inspect.all(1, opened.elements)?;
    let anchor = elements
        .iter()
        .position(|row| matches!(row, Element::Obu { obu } if obu.header.offset == 83))
        .ok_or("Missing reset OBU")? as u64;
    let (_, summary, rows) = report(&mut inspect, anchor, LayerDecodeSelection::Default)?;
    assert_eq!(summary.temporal_unit, Some(1));
    assert!(summary.metadata_complete);
    let origin = rows.iter().find_map(|row| match &row.fact {
        LayerFact::ContentInterpretation {
            embedded: 0,
            value:
                ContentState::Signaled {
                    reset: 83,
                    inherited_from,
                    sequence_header: Some(_),
                    ..
                },
        } if row.scope == (LayerScope::Extended { id: 0 }) && inherited_from.is_empty() => {
            row.origin
        }
        _ => None,
    });
    assert_eq!(origin, Some(71));
    let located = inspect.ask(Command::AnalysisLocate {
        document: 1,
        filter: RowFilter::default(),
        target: LocateTarget::Offset {
            offset: origin.ok_or("Missing CI origin")?,
        },
    })?;
    let Event::AnalysisLocated {
        document: 1, found, ..
    } = located
    else {
        return Err(format!("Expected indexed CI origin. Got {located:?}").into());
    };
    let ci = found.index;
    assert!(
        matches!(&found.element, Element::Obu { obu } if obu.type_name == "OBU_CONTENT_INTERPRETATION")
    );
    let trace = inspect.next_id;
    assert!(matches!(
        inspect.ask(Command::AnalysisSyntax { document: 1, element: ci })?,
        Event::AnalysisSyntaxReady { document: 1, element, .. } if element == ci
    ));
    let syntax = inspect.ask(Command::AnalysisSyntaxPage {
        document: 1,
        trace,
        first: 0,
        count: 256,
    })?;
    assert!(
        matches!(&syntax, Event::AnalysisSyntaxPage { element, events, .. } if *element == ci
        && events.iter().any(|event| matches!(event, SyntaxEvent::Signaled { name, span: Some(_), .. } if name == "ci_scan_type_idc")))
    );
    Ok(())
}

#[test]
fn global_layer_configuration_keeps_both_extended_layers_distinct() -> TestResult {
    let mut inspect = Inspect::spawn()?;
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("layers.obu");
    fs::write(
        &source,
        include_bytes!("fixtures/syn-xlayer3-7-global-lcr-10bit.obu"),
    )?;
    let opened = inspect.open(1, &source)?;
    let elements = inspect.all(1, opened.elements)?;
    let anchor = elements
        .iter()
        .rposition(|row| matches!(row, Element::Obu { .. }))
        .ok_or("Missing frame OBU")? as u64;
    let (_, _, rows) = report(&mut inspect, anchor, LayerDecodeSelection::Default)?;
    assert!(rows.iter().any(|row| row.scope == LayerScope::Global
        && matches!(
            row.fact,
            LayerFact::Configuration {
                global_id: 1,
                xlayer_map: Some(0x88),
                ..
            }
        )));
    assert!(
        rows.iter()
            .any(|row| row.scope == LayerScope::Extended { id: 3 }
                && matches!(
                    row.fact,
                    LayerFact::SequenceHeader {
                        id: 0,
                        max_width: 64,
                        max_height: 64,
                        ..
                    }
                ))
    );
    assert!(
        rows.iter()
            .any(|row| row.scope == LayerScope::Extended { id: 7 }
                && matches!(
                    row.fact,
                    LayerFact::SequenceHeader {
                        id: 1,
                        max_width: 96,
                        max_height: 64,
                        ..
                    }
                ))
    );
    Ok(())
}

#[test]
fn external_context_keeps_raw_declarations_inspectable() -> TestResult {
    let mut inspect = Inspect::spawn()?;
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("external.ivf");
    fs::write(&source, include_bytes!("fixtures/syn-lcr-64x64.ivf"))?;
    let opened = inspect.open(1, &source)?;
    let elements = inspect.all(1, opened.elements)?;
    let anchor = elements
        .iter()
        .rposition(|row| matches!(row, Element::Obu { .. }))
        .ok_or("Missing frame OBU")? as u64;
    let (_, _, rows) = report(&mut inspect, anchor, LayerDecodeSelection::Default)?;
    assert!(
        rows.iter()
            .any(|row| matches!(row.fact, LayerFact::Configuration { .. }))
    );
    Ok(())
}

#[test]
fn inherited_content_interpretation_expires_at_the_next_sequence() -> TestResult {
    let mut inspect = Inspect::spawn()?;
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("ci-inheritance.obu");
    fs::write(
        &source,
        include_bytes!("fixtures/syn-ci-inheritance-reset.obu"),
    )?;
    let opened = inspect.open(1, &source)?;
    let elements = inspect.all(1, opened.elements)?;
    let first = elements
        .iter()
        .rposition(|row| {
            matches!(row, Element::Obu { obu }
        if obu.header.offset < 537 && obu.embedded_layer_id.value == 1)
        })
        .ok_or("Missing enhancement OBU in the first sequence")? as u64;
    let last = elements
        .iter()
        .rposition(|row| {
            matches!(row, Element::Obu { obu }
        if obu.header.offset > 572 && obu.embedded_layer_id.value == 1)
        })
        .ok_or("Missing enhancement OBU in the next sequence")? as u64;

    let (_, summary, rows) = report(&mut inspect, first, LayerDecodeSelection::Default)?;
    assert!(summary.metadata_complete);
    assert!(rows.iter().any(|row| {
        row.scope == LayerScope::Extended { id: 0 }
            && row.origin == Some(34)
            && matches!(&row.fact, LayerFact::ContentInterpretation {
                embedded: 1,
                value: ContentState::Signaled { inherited_from, .. },
            } if inherited_from == &[0])
    }));

    let (_, summary, rows) = report(&mut inspect, last, LayerDecodeSelection::Default)?;
    assert!(summary.metadata_complete);
    assert!(rows.iter().any(|row| {
        row.scope == LayerScope::Extended { id: 0 }
            && row.origin.is_none()
            && matches!(
                &row.fact,
                LayerFact::ContentInterpretation {
                    embedded: 1,
                    value: ContentState::Unspecified { reset: 572, .. },
                }
            )
    }));
    Ok(())
}

#[test]
fn nearest_unspecified_layer_takes_priority_over_signaled_ci() -> TestResult {
    let mut inspect = Inspect::spawn()?;
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("ci-default-priority.obu");
    fs::write(
        &source,
        include_bytes!("fixtures/syn-ci-default-priority.obu"),
    )?;
    let opened = inspect.open(1, &source)?;
    let elements = inspect.all(1, opened.elements)?;
    let anchor = elements
        .iter()
        .rposition(|row| matches!(row, Element::Obu { obu } if obu.embedded_layer_id.value == 2))
        .ok_or("Missing embedded layer 2 OBU")? as u64;

    let (_, summary, rows) = report(&mut inspect, anchor, LayerDecodeSelection::Default)?;
    assert!(summary.metadata_complete);
    assert!(rows.iter().any(|row| {
        row.scope == (LayerScope::Extended { id: 0 })
            && row.origin == Some(35)
            && matches!(
                row.fact,
                LayerFact::ContentInterpretation {
                    embedded: 0,
                    value: ContentState::Signaled {
                        color_codes: Some([1, 1, 1]),
                        ..
                    },
                }
            )
    }));
    for (layer, inheritance) in [(1, [].as_slice()), (2, [1].as_slice())] {
        assert!(
            rows.iter().any(|row| {
                row.scope == (LayerScope::Extended { id: 0 })
                    && row.origin.is_none()
                    && matches!(
                        &row.fact,
                        LayerFact::ContentInterpretation {
                            embedded,
                            value: ContentState::Unspecified { reset: 504, inherited_from },
                        } if *embedded == layer && inherited_from == inheritance
                    )
            }),
            "Missing unspecified state and reset for embedded layer {layer}"
        );
    }
    let reset = inspect.ask(Command::AnalysisLocate {
        document: 1,
        filter: RowFilter::default(),
        target: LocateTarget::Offset { offset: 504 },
    })?;
    assert!(matches!(
        reset,
        Event::AnalysisLocated { found, .. } if matches!(
            &found.element,
            Element::Obu { obu } if obu.header.offset == 504
                && obu.type_name == "OBU_CLOSED_LOOP_KEY" && obu.embedded_layer_id.value == 1
        )
    ));
    Ok(())
}

#[test]
fn timeline_output_ordinals_follow_the_requested_operating_point() -> TestResult {
    let mut inspect = Inspect::spawn()?;
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("layers.obu");
    fs::write(
        &source,
        include_bytes!("../preview/fixtures/syn-msdo-ops-random-access.obu"),
    )?;
    let _ = inspect.open(1, &source)?;
    let selection = LayerDecodeSelection::OperatingPoint {
        scope: LayerScope::Global,
        set: 0,
        point: 1,
    };
    let mut outputs = Vec::new();
    for requested in [LayerDecodeSelection::Default, selection] {
        let scan = inspect.next_id;
        let result = inspect.ask(Command::AnalysisIdentityScan {
            document: 1,
            grain: GrainPolicy::Skip,
            selection: requested,
        })?;
        let Event::AnalysisIdentityScanned {
            document: 1,
            summary,
        } = result
        else {
            return Err(format!("Expected an identity scan result. Got {result:?}").into());
        };
        assert_eq!(summary.completion, IdentityCompletion::Complete);
        assert!(summary.matches_structure);
        assert_eq!(summary.selection, requested);

        let result = inspect.ask(Command::AnalysisIdentityPage {
            document: 1,
            scan,
            kind: IdentityKind::Output,
            first: 0,
            count: 128,
        })?;
        let Event::AnalysisIdentityPage {
            document: 1, page, ..
        } = result
        else {
            return Err(format!("Expected an output page. Got {result:?}").into());
        };
        assert_eq!(page.summary.selection, requested);
        assert_eq!(page.rows.len() as u64, summary.outputs);
        assert_ne!(summary.outputs, 0);
        if requested == selection {
            assert!(page.rows.iter().all(
                |row| matches!(row, IdentityRow::Output { layers, .. } if layers.extended == 3)
            ));
        }
        outputs.push(summary.outputs);
    }
    assert!(outputs[0] > outputs[1]);
    Ok(())
}
