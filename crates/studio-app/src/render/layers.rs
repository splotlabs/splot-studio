// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use slint::ComponentHandle;
use studio_domain::layers::{LayerDecodeSelection, LayerDecodeStatus};

use super::{model, refill};
use crate::analysis::layers::{LayerInspector, selection_name};
use crate::analysis::{Document, Input, LayerInput};
use crate::ui::{AppWindow, LayerDetail, Layers};

pub(crate) fn bind(window: &AppWindow, send: impl Fn(Input) + Clone + 'static) {
    let global = window.global::<Layers>();
    let on = |input: fn(i32) -> LayerInput| {
        let send = send.clone();
        move |index| send(Input::Layer(input(index)))
    };
    global.on_move_page(on(LayerInput::Page));
    global.on_open_origin(on(LayerInput::Origin));
    global.on_request_point(on(LayerInput::Decode));
    global.on_reset_point(move || send(Input::Layer(LayerInput::DecodeDefault)));
}

pub(super) fn render(window: &AppWindow, document: Option<&Document>) {
    let target = window.global::<Layers>();
    let Some(state) = document.and_then(|doc| doc.summary().map(|_| &doc.layers)) else {
        target.set_shown(false);
        target.set_rows(model(Vec::new()));
        return;
    };
    target.set_shown(state.shown);
    if !state.shown {
        return;
    }
    target.set_status(status(state).into());
    target.set_request(
        format!(
            "Requested playback point · {}",
            selection_name(state.selection)
        )
        .into(),
    );
    target.set_can_reset(state.selection != LayerDecodeSelection::Default);
    target.set_decode(
        match state.decode.as_ref() {
            Some(LayerDecodeStatus::Active) => {
                "Playback produced a picture for this request.".into()
            }
            Some(LayerDecodeStatus::Rejected(reason)) => {
                format!("Playback rejected this request. {reason}")
            }
            Some(LayerDecodeStatus::Pending) | None => {
                "Playback has yet to produce a picture for this request.".into()
            }
        }
        .into(),
    );
    let page = state.current.as_ref();
    let rows = page.map_or_else(Vec::new, |page| {
        page.rows
            .iter()
            .map(|row| LayerDetail {
                label: row.label.as_str().into(),
                value: row.value.as_str().into(),
                context: row.context.as_str().into(),
                origin: row
                    .origin
                    .map_or_else(String::new, |origin| format!("Show OBU at byte {origin}"))
                    .into(),
                can_request: row
                    .request
                    .is_some_and(|selection| selection != state.selection),
            })
            .collect()
    });
    if let Some(rows) = refill(&target.get_rows(), rows) {
        target.set_rows(rows);
    }
    target.set_page(
        page.filter(|page| page.count > 0)
            .map_or_else(String::new, |page| {
                format!(
                    "Records {}–{} of {}",
                    page.first + 1,
                    page.first + page.count,
                    page.total
                )
            })
            .into(),
    );
    target.set_has_previous(!state.loading() && page.is_some_and(|page| !page.previous.is_empty()));
    target.set_has_next(
        !state.loading()
            && page.is_some_and(|page| page.count > 0 && page.first + page.count < page.total),
    );
}

fn status(state: &LayerInspector) -> String {
    if let Some(reason) = &state.error {
        return reason.clone();
    }
    if let Some(page) = &state.current {
        let evidence = &page.evidence;
        let unit = evidence.temporal_unit.map_or_else(
            || "Temporal unit unavailable".into(),
            |unit| format!("Temporal unit {unit}"),
        );
        let mut message = format!("OBU at byte {} · {unit}", evidence.offset);
        if !evidence.metadata_complete {
            message.push_str(". Effective metadata coverage is incomplete.");
        }
        if let Some(reason) = &evidence.membership_reason {
            message.push_str(". ");
            message.push_str(reason);
        }
        if evidence.catalog_limited {
            message.push_str(". The declaration catalog reached its limit.");
        }
        if page.total == 0 {
            message.push_str(". No layer facts are available here.");
        }
        return message;
    }
    if state.loading() {
        "Reading layer evidence at the selected OBU.".into()
    } else {
        "Select an OBU to inspect its layer and metadata context.".into()
    }
}
