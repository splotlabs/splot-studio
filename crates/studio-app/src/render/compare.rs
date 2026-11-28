// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Pushes the Compare workspace into the window's `Comparison` global, as
//! display strings.

use slint::{ComponentHandle, SharedString};

use super::model;
use crate::compare::Compare;
use crate::present::compare::{self as text, SideText, State};
use crate::ui::{AppWindow, CompareSetting, CompareSide, CompareState, Comparison};

fn lines<T: AsRef<str>>(lines: &[T]) -> slint::ModelRc<SharedString> {
    model(lines.iter().map(|line| line.as_ref().into()).collect())
}

fn side(side: &SideText) -> CompareSide {
    CompareSide {
        name: side.name.as_str().into(),
        lines: lines(&side.lines),
        exact: lines(&side.exact),
    }
}

fn int(value: u64) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// Pushes the whole Compare workspace.
///
/// Public so renderer snapshots and UI tests can show a state.
pub fn render_compare(window: &AppWindow, compare: &Compare) {
    let view = text::view(compare);
    let global = window.global::<Comparison>();
    global.set_state(match view.state {
        State::Empty => CompareState::Empty,
        State::Reading => CompareState::Reading,
        State::Ready => CompareState::Ready,
        State::Failed => CompareState::Failed,
    });
    global.set_message(view.message.into());
    global.set_notice(view.notice.into());
    global.set_a(side(&view.a));
    global.set_b(side(&view.b));
    global.set_verdict(view.verdict.into());
    global.set_verdict_problem(view.verdict_problem);
    global.set_verdict_details(lines(&view.verdict_details));
    global.set_basis_labels(lines(&view.basis_labels));
    global.set_basis_index(int(view.basis_index as u64));
    global.set_offset_shown(view.offset.is_some());
    global.set_offset_limit(int(view.offset_limit));
    let offset = view.offset.unwrap_or(0);
    global.set_offset(i32::try_from(offset).unwrap_or(if offset < 0 {
        i32::MIN
    } else {
        i32::MAX
    }));
    global.set_alignment(view.alignment.into());
    global.set_alignment_problem(view.alignment_problem);
    global.set_segments(lines(&view.segments));
    global.set_grain(view.grain);
    global.set_settings_status(view.settings_status.into());
    global.set_settings(model(
        view.settings
            .into_iter()
            .map(|row| CompareSetting {
                key: row.key.into(),
                a: row.a.into(),
                b: row.b.into(),
                difference: row.difference.into(),
            })
            .collect(),
    ));
    global.set_summary(view.summary.into());
}
