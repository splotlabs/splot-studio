// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Frame-level reference evidence as a bounded text table.

use std::fmt::Write as _;

use studio_domain::layers::LayerDecodeSelection;
use studio_domain::references::{
    ReferenceEvidence, ReferenceFrame, ReferenceOrigin, ReferenceSlotState,
};

use super::model;
use crate::analysis::{Document, references::follow_offset};
use crate::ui::{ReferenceLinkRow, ReferenceSlotRow, References};

pub(super) fn render_document(view: &References<'_>, document: Option<&Document>) {
    let Some(document) =
        document.filter(|doc| doc.evidence.tab == crate::analysis::references::TAB)
    else {
        view.set_can_inspect(false);
        render(view, None, "", false, "");
        return;
    };
    let ready = document.reference_offset().is_some() && !document.selection_pending();
    let references = &document.references;
    let busy = references.busy();
    let evidence = references.evidence().filter(|_| ready);
    view.set_can_inspect(ready && !busy && evidence.is_none());
    let status = references.notice().unwrap_or_else(|| {
        if busy {
            "Reading reference state…"
        } else if !ready {
            "Select a coded-frame OBU to inspect its references."
        } else if evidence.is_none() {
            "Inspect the selected frame’s reference state."
        } else {
            ""
        }
    });
    let output = evidence.map_or_else(String::new, |evidence| output_text(document, evidence));
    render(view, evidence, status, busy, &output);
}

fn output_text(document: &Document, evidence: &ReferenceEvidence) -> String {
    if evidence.query.selection != LayerDecodeSelection::Default {
        return "Output identity is unavailable for this operating point.".to_owned();
    }
    let Some(selection) = document
        .reference_timeline()
        .filter(|frame| frame.coded_offset == evidence.frame.coded_offset)
    else {
        return "Indexed output identity is unavailable.".to_owned();
    };
    match (selection.output_ordinal, selection.showing_offset) {
        (Some(output), Some(offset)) => format!(
            "Coded index {}. Indexed output {output} was requested by the OBU at byte {offset}.",
            selection.coded_ordinal
        ),
        (Some(output), None) => format!(
            "Coded index {}. Indexed output {output}.",
            selection.coded_ordinal
        ),
        _ if document
            .timeline()
            .page()
            .is_some_and(|page| page.complete && page.verified) =>
        {
            format!(
                "Coded index {}. The complete timeline records no output.",
                selection.coded_ordinal
            )
        }
        _ => format!(
            "Coded index {}. Output information is incomplete.",
            selection.coded_ordinal
        ),
    }
}

fn render(
    view: &References<'_>,
    evidence: Option<&ReferenceEvidence>,
    status: &str,
    busy: bool,
    output: &str,
) {
    view.set_status(status.into());
    view.set_busy(busy);
    view.set_output(output.into());
    let Some(evidence) = evidence else {
        view.set_frame(Default::default());
        view.set_context(Default::default());
        view.set_links(model(Vec::new()));
        view.set_slots(model(Vec::new()));
        return;
    };
    view.set_frame(format!("Coded frame at byte {}", evidence.frame.coded_offset).into());
    view.set_context(context(evidence).into());
    view.set_links(model(
        evidence
            .dependencies
            .iter()
            .enumerate()
            .map(|(index, link)| {
                let kind = match link.origin {
                    ReferenceOrigin::Signaled => "Signaled",
                    ReferenceOrigin::Derived => "Derived",
                    ReferenceOrigin::Bridge => "Bridge source",
                };
                let label = link.index.map_or_else(
                    || format!("{kind} · slot {}", link.slot),
                    |index| format!("{kind} reference {index} · slot {}", link.slot),
                );
                let scale = if link.scaled { "Scaled" } else { "Unit scale" };
                let detail = evidence
                    .slots
                    .iter()
                    .find(|slot| slot.slot == link.slot)
                    .map_or_else(
                        || "Source slot unavailable".to_owned(),
                        |slot| slot_text(slot.before),
                    );
                ReferenceLinkRow {
                    label: label.into(),
                    detail: format!("{detail}. {scale}.").into(),
                    available: follow_offset(evidence, index).is_some(),
                }
            })
            .collect(),
    ));
    view.set_slots(model(
        evidence
            .slots
            .iter()
            .map(|slot| ReferenceSlotRow {
                slot: slot.slot.to_string().into(),
                before: slot_text(slot.before).into(),
                after: slot_text(slot.after).into(),
                changed: slot.before != slot.after,
            })
            .collect(),
    ));
}

fn context(evidence: &ReferenceEvidence) -> String {
    let mut text = format!(
        "{}. OBU type {}. Refreshed slots {}.",
        frame_context(evidence.frame),
        evidence.frame_obu_type,
        slot_list(evidence.refresh_mask),
    );
    if evidence.resets_sequence {
        text.push_str(" The frame starts a coded video sequence.");
    }
    if let Some(mask) = evidence.allowed_input_slots {
        let _ = write!(text, " Allowed input slots {}.", slot_list(mask));
    }
    if !evidence.required_long_term_ids.is_empty() {
        let ids = labels(evidence.required_long_term_ids.iter());
        let _ = write!(text, " Required long-term IDs {ids}.");
    }
    text
}

fn slot_list(mask: u32) -> String {
    if mask == 0 {
        return "none".to_owned();
    }
    labels((0..u32::BITS).filter(|index| mask & (1 << index) != 0))
}

fn labels(values: impl IntoIterator<Item = impl ToString>) -> String {
    values
        .into_iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn frame_context(frame: ReferenceFrame) -> String {
    format!(
        "Sequence header at byte {}. Layers x{}, m{}, t{}. {} × {} luma samples",
        frame.sequence_offset,
        frame.layers.extended,
        frame.layers.embedded,
        frame.layers.temporal,
        frame.width,
        frame.height,
    )
}

fn slot_text(state: ReferenceSlotState) -> String {
    match state {
        ReferenceSlotState::Absent => "Slot absent".to_owned(),
        ReferenceSlotState::Empty => "Empty".to_owned(),
        ReferenceSlotState::Invalid => "Invalid".to_owned(),
        ReferenceSlotState::Unavailable => "Retained frame identity unavailable".to_owned(),
        ReferenceSlotState::Retained {
            frame,
            long_term_id,
            restricted_order_hint,
        } => {
            let mut text = format!(
                "Coded frame at byte {}. {}",
                frame.coded_offset,
                frame_context(frame)
            );
            if let Some(id) = long_term_id {
                let _ = write!(text, ". Long-term ID {id}");
            }
            if restricted_order_hint {
                text.push_str(". Restricted order hint");
            }
            text
        }
    }
}
