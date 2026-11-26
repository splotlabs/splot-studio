// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Editing and history of the option draft for future submissions.

use std::collections::VecDeque;
use std::mem;

use studio_domain::options::OptionRequest;
use studio_domain::settings::{Crf, OutputKind, SpeedPreset};

use crate::queue::{Attempt, ItemState};
use crate::recovery::Disk;
use crate::simple::{Priority, Quality};

use super::{AppState, Intent, Mode, SettingsView};

const HISTORY_LIMIT: usize = 32;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct OptionHistory {
    undo: VecDeque<OptionRequest>,
    redo: Vec<OptionRequest>,
    generation: u64,
}

impl OptionHistory {
    fn push_undo(&mut self, request: OptionRequest) {
        if self.undo.len() == HISTORY_LIMIT {
            self.undo.pop_front();
        }
        self.undo.push_back(request);
    }
}

impl SettingsView {
    /// The Simple quality step, when it describes the current request.
    #[must_use]
    pub fn quality(self) -> Option<Quality> {
        if self.quality_override {
            None
        } else {
            Quality::of(self.settings.crf)
        }
    }

    /// The Simple priority step, when it describes the current request.
    #[must_use]
    pub fn priority(self) -> Option<Priority> {
        if self.priority_override {
            None
        } else {
            Priority::of(self.settings.preset)
        }
    }

    /// Whether a typed override changes the preset policy shown by Simple.
    #[must_use]
    pub const fn priority_is_overridden(self) -> bool {
        self.priority_override
    }

    /// Whether a typed rate-control override takes precedence over Simple quality.
    #[must_use]
    pub const fn quality_is_overridden(self) -> bool {
        self.quality_override
    }
}

impl AppState {
    /// The visible option request from the draft or the selected job.
    #[must_use]
    pub fn visible_option_request(&self) -> &OptionRequest {
        self.settings_attempt()
            .map_or(&self.settings, |attempt| &attempt.job.settings)
    }

    /// Settings and Simple step labels for the selected view.
    #[must_use]
    pub fn settings_view(&self) -> SettingsView {
        let attempt = self.settings_attempt();
        let request = attempt.map_or(&self.settings, |attempt| &attempt.job.settings);
        SettingsView {
            settings: request.base,
            read_only: attempt.is_some(),
            quality_override: request.values.contains_key("rate_control"),
            priority_override: request.values.keys().any(|key| {
                matches!(key.as_str(), "tune" | "fast_decode") || key.starts_with("preset.")
            }),
        }
    }

    fn settings_attempt(&self) -> Option<&Attempt> {
        let item = match self.mode() {
            Mode::Single => self.items.first().filter(|item| item.state.is_active()),
            Mode::List => self
                .selected
                .and_then(|id| self.item(id))
                .filter(|item| item.state.is_active() || item.state == ItemState::Completed),
            Mode::Empty => None,
        };
        item.and_then(|item| item.attempt.as_ref())
    }

    fn options_editable(&self) -> bool {
        self.settings_attempt().is_none()
    }

    /// Whether an earlier draft can be restored from the current view.
    #[must_use]
    pub fn can_undo_options(&self) -> bool {
        self.options_editable() && !self.option_history.undo.is_empty()
    }

    /// Whether a later draft can be restored from the current view.
    #[must_use]
    pub fn can_redo_options(&self) -> bool {
        self.options_editable() && !self.option_history.redo.is_empty()
    }

    /// Changes whenever an edit, undo or redo changes the option draft.
    #[must_use]
    pub const fn options_generation(&self) -> u64 {
        self.option_history.generation
    }

    /// Returns the exact draft an edit proposes, without changing the state.
    /// Returns no proposal for a no-op, an invalid edit, or a locked view.
    #[must_use]
    pub fn proposed_options(&self, intent: &Intent) -> Option<OptionRequest> {
        if !self.options_editable() {
            return None;
        }
        let mut request = self.settings.clone();
        match intent {
            Intent::ReplaceOptions(next) => request.clone_from(next),
            Intent::OutputSelected(index) => request.base.output = step(&OutputKind::ALL, *index)?,
            Intent::PresetEdited(value) => {
                request.base.preset = SpeedPreset::new(u8::try_from(*value).ok()?).ok()?;
            }
            Intent::CrfEdited(value) => {
                request.base.crf = Crf::new(u8::try_from(*value).ok()?).ok()?;
            }
            Intent::QualitySelected(index) => {
                request.base.crf = step(&Quality::ALL, *index)?.crf();
            }
            Intent::PrioritySelected(index) => {
                request.base.preset = step(&Priority::ALL, *index)?.preset();
            }
            Intent::OptionEdited { key, value } if self.confirmation.is_none() => match value {
                Some(value) => {
                    request.values.insert(key.clone(), value.clone());
                }
                None => {
                    request.values.remove(key);
                }
            },
            _ => return None,
        }
        (request != self.settings).then_some(request)
    }

    pub(super) fn edit_options(&mut self, intent: Intent, disk: &dyn Disk) {
        if !self.options_editable() {
            return;
        }
        match intent {
            Intent::UndoOptions => {
                if let Some(request) = self.option_history.undo.pop_back() {
                    let previous = self.switch_options(request, disk);
                    self.option_history.redo.push(previous);
                }
            }
            Intent::RedoOptions => {
                if let Some(request) = self.option_history.redo.pop() {
                    let previous = self.switch_options(request, disk);
                    self.option_history.push_undo(previous);
                }
            }
            intent => {
                if let Some(request) = self.proposed_options(&intent) {
                    self.option_history.redo.clear();
                    let previous = self.switch_options(request, disk);
                    self.option_history.push_undo(previous);
                }
            }
        }
    }

    fn switch_options(&mut self, request: OptionRequest, disk: &dyn Disk) -> OptionRequest {
        let previous = mem::replace(&mut self.settings, request);
        self.option_history.generation = self.option_history.generation.wrapping_add(1);
        if previous.base.output != self.settings.base.output {
            self.follow_output_kind(disk);
        }
        if previous.values != self.settings.values {
            if self.run.is_none() {
                self.confirmation = None;
            }
            for item in &mut self.items {
                if item.submitted.is_none() {
                    item.ack = None;
                }
            }
        }
        previous
    }
}

fn step<T: Copy>(all: &[T], index: i32) -> Option<T> {
    usize::try_from(index)
        .ok()
        .and_then(|i| all.get(i).copied())
}
