// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::Duration;

use slint::{SharedString, TimerMode};
use studio_domain::job::SourceFacts;
use studio_domain::options::{EffectiveOption, FieldError, OptionDescriptor, OptionRequest};
use studio_domain::protocol::OptionsIdentity;

use crate::advanced::{self, Reset};
use crate::queue::{Item, ItemId};
use crate::render::model;
use crate::session::Session;
use crate::state::{AppState, Intent};
use crate::ui::{AdvancedOptionRow, AppWindow, FieldRow};
use crate::workspace::Workspace;

use super::{Controller, with};

const SETTLE: Duration = Duration::from_millis(180);

#[derive(PartialEq, Eq)]
struct Inputs {
    item: Option<ItemId>,
    request: OptionRequest,
    source: Option<SourceFacts>,
    engine: Option<OptionsIdentity>,
    locked: bool,
}

#[derive(Default)]
pub(super) struct Editor {
    inputs: Option<Inputs>,
    query: SharedString,
    modified: bool,
    expert: bool,
    selected: String,
    invalid: BTreeMap<(String, String), InvalidEdit>,
    pending_focus: Option<(String, String)>,
    engine_error: Option<FieldError>,
    candidate: Option<OptionRequest>,
    imported_changes: Option<String>,
    preview_started: bool,
    rows: Vec<AdvancedOptionRow>,
    fields: Vec<FieldRow>,
    fields_key: String,
}

struct InvalidEdit {
    text: String,
    message: String,
}

impl Controller {
    pub(super) fn wire_advanced(self: &Rc<Self>, window: &AppWindow) {
        window.on_advanced_open_changed({
            let ctl = Rc::downgrade(self);
            move |open| with(&ctl, |ctl| ctl.open_advanced(open))
        });
        window.on_advanced_search({
            let ctl = Rc::downgrade(self);
            move |query| with(&ctl, |ctl| ctl.view_advanced(|editor| editor.query = query))
        });
        window.on_advanced_modified({
            let ctl = Rc::downgrade(self);
            move |modified| {
                with(&ctl, |ctl| {
                    ctl.view_advanced(|editor| editor.modified = modified);
                });
            }
        });
        window.on_advanced_expert({
            let ctl = Rc::downgrade(self);
            move |expert| {
                with(&ctl, |ctl| {
                    ctl.view_advanced(|editor| editor.expert = expert);
                });
            }
        });
        window.on_advanced_select({
            let ctl = Rc::downgrade(self);
            move |key| {
                with(&ctl, |ctl| {
                    ctl.view_advanced(|editor| {
                        if editor.selected != key.as_str() {
                            editor.selected = key.to_string();
                        }
                    });
                });
            }
        });
        window.on_advanced_edit({
            let ctl = Rc::downgrade(self);
            move |id, text| with(&ctl, |ctl| ctl.edit_advanced(id.as_str(), text.as_str()))
        });
        window.on_advanced_reset({
            let ctl = Rc::downgrade(self);
            move |scope| with(&ctl, |ctl| ctl.reset_advanced(scope.as_str()))
        });
        window.on_advanced_undo({
            let ctl = Rc::downgrade(self);
            move || with(&ctl, |ctl| ctl.dispatch(Intent::UndoOptions))
        });
        window.on_advanced_redo({
            let ctl = Rc::downgrade(self);
            move || with(&ctl, |ctl| ctl.dispatch(Intent::RedoOptions))
        });
        window.on_advanced_preview_preset({
            let ctl = Rc::downgrade(self);
            move |preset| with(&ctl, |ctl| ctl.preview_preset(preset))
        });
        window.on_advanced_apply_preset({
            let ctl = Rc::downgrade(self);
            move || with(&ctl, Controller::apply_preset)
        });
    }

    fn open_advanced(self: &Rc<Self>, open: bool) {
        self.preset_files.invalidate();
        self.advanced_settle.stop();
        let mut editor = self.advanced.borrow_mut();
        editor.inputs = None;
        if !open {
            editor.candidate = None;
            editor.imported_changes = None;
            editor.preview_started = false;
        }
        editor.engine_error = None;
        drop(editor);
        let outcome = self.session.borrow_mut().advanced_refresh(open);
        self.handle(outcome);
        self.view_advanced(|_| {});
    }

    fn view_advanced(self: &Rc<Self>, change: impl FnOnce(&mut Editor)) {
        let Ok(mut editor) = self.advanced.try_borrow_mut() else {
            return;
        };
        change(&mut editor);
        drop(editor);
        if let Some(window) = self.window.upgrade() {
            self.render_advanced(&window, &self.session.borrow());
        }
    }

    pub(super) fn render_advanced(self: &Rc<Self>, window: &AppWindow, session: &Session) {
        if !window.get_advanced_open() || session.state().workspace() != Workspace::Encode {
            self.preset_files.invalidate();
            return;
        }
        let state = session.state();
        let locked = state.settings_view().read_only;
        let request = state.visible_option_request();
        let item = state.selected();
        let inputs = Inputs {
            item: item.map(Item::id),
            request: request.clone(),
            source: item.and_then(Item::facts).cloned(),
            engine: state.capabilities().and_then(|cap| cap.options.clone()),
            locked,
        };
        let mut editor = self.advanced.borrow_mut();
        if editor.inputs.as_ref() != Some(&inputs) {
            self.preset_files.invalidate();
            let refresh = editor.inputs.is_some() && !locked;
            if editor.inputs.is_some() {
                editor.candidate = None;
                editor.imported_changes = None;
                editor.preview_started = false;
            }
            editor.inputs = Some(inputs);
            if refresh {
                let weak = Rc::downgrade(self);
                self.advanced_settle
                    .start(TimerMode::SingleShot, SETTLE, move || {
                        with(&weak, |ctl| {
                            let open = ctl.window.upgrade().is_some_and(|window| {
                                window.get_advanced_open()
                                    && ctl.session.borrow().state().workspace() == Workspace::Encode
                            });
                            let outcome = ctl.session.borrow_mut().advanced_refresh(open);
                            ctl.handle(outcome);
                        });
                    });
            }
        }
        let catalog = session.advanced_catalog();
        window.set_advanced_preset_files_ready(
            !catalog.is_empty()
                && state
                    .capabilities()
                    .is_some_and(|cap| cap.options.is_some()),
        );
        let policy = if locked {
            item.and_then(Item::attempt)
                .map_or(&[][..], |attempt| &attempt.policy)
        } else {
            session.advanced_policy()
        };
        let error = (!locked).then(|| session.advanced_error()).flatten();
        editor.select_error(catalog, request, error);
        let Editor {
            selected,
            pending_focus,
            ..
        } = &mut *editor;
        if let Some((key, _)) = pending_focus {
            selected.clone_from(key);
        }
        editor.render_rows(window, catalog, request, error.is_some());
        editor.render_detail(window, catalog, request, policy);
        let error_text = editor
            .invalid
            .iter()
            .find(|((key, _), _)| *key == editor.selected)
            .map(|(_, error)| error.message.clone())
            .or_else(|| error.map(|error| format!("{}. {}", error.field, error.message)));
        window.set_advanced_error(error_text.unwrap_or_default().into());
        window.set_advanced_status(
            if self.advanced_settle.running() || session.advanced_pending() {
                "Resolving settings…"
            } else if item.and_then(Item::facts).is_none() {
                "Add a source to resolve settings."
            } else {
                ""
            }
            .into(),
        );
        window.set_advanced_can_undo(!locked && state.can_undo_options());
        window.set_advanced_can_redo(!locked && state.can_redo_options());
        editor.render_preset(window, session, policy);
        let focus = editor.focus_field();
        drop(editor);
        if let Some(field) = focus {
            window.invoke_focus_advanced_error(field.into());
        }
    }

    fn edit_advanced(self: &Rc<Self>, id: &str, text: &str) {
        let control = id.rsplit('/').next().unwrap_or(id);
        if (matches!(control, "@mode" | "@state" | "@frames" | "@add")
            || control.starts_with("@remove"))
            && let Some(window) = self.window.upgrade()
        {
            window.invoke_flush_advanced_draft();
        }
        let proposed = {
            let session = self.session.borrow();
            let state = session.state();
            if state.settings_view().read_only {
                return;
            }
            let Ok(editor) = self.advanced.try_borrow() else {
                return;
            };
            let Some(descriptor) = selected(session.advanced_catalog(), &editor.selected) else {
                return;
            };
            let policy = session.advanced_policy();
            let edit = advanced::edit(descriptor, state.option_request(), policy, id, text).map(
                |request| {
                    let detail = advanced::inspect(descriptor, &request, policy);
                    (request, detail.fields)
                },
            );
            (descriptor.key.clone(), edit)
        };
        let (key, proposed) = proposed;
        match proposed {
            Ok((request, fields)) => {
                let mut editor = self.advanced.borrow_mut();
                editor.invalid.remove(&(key.clone(), id.into()));
                editor.reconcile_invalid(&key, id, &fields);
                drop(editor);
                self.dispatch(Intent::ReplaceOptions(request));
                self.view_advanced(|_| {});
            }
            Err(message) => self.view_advanced(|editor| {
                editor.invalid.insert(
                    (key, id.into()),
                    InvalidEdit {
                        text: text.into(),
                        message,
                    },
                );
            }),
        }
    }

    fn reset_advanced(self: &Rc<Self>, scope: &str) {
        let proposed = {
            let session = self.session.borrow();
            let state = session.state();
            if state.settings_view().read_only {
                return;
            }
            let selected_key = self.advanced.borrow().selected.clone();
            let catalog = session.advanced_catalog();
            let scope = match scope {
                "all" => Reset::All,
                "selected" => Reset::Field(&selected_key),
                "group" => {
                    if let Some(descriptor) = selected(catalog, &selected_key) {
                        Reset::Group(advanced::catalog::group(descriptor))
                    } else if state.option_request().values.contains_key(&selected_key) {
                        Reset::Group("Unavailable")
                    } else {
                        return;
                    }
                }
                _ => return,
            };
            let proposed = advanced::reset(state.option_request(), catalog, scope);
            self.advanced
                .borrow_mut()
                .invalid
                .retain(|(key, _), _| !match scope {
                    Reset::All => true,
                    Reset::Field(field) => key == field,
                    Reset::Group(group) => selected(catalog, key)
                        .is_some_and(|descriptor| advanced::catalog::group(descriptor) == group),
                });
            proposed
        };
        self.dispatch(Intent::ReplaceOptions(proposed));
        self.view_advanced(|_| {});
        if let Some(window) = self.window.upgrade() {
            window.invoke_reset_advanced_draft();
        }
    }

    pub(super) fn block_invalid_advanced(self: &Rc<Self>) -> bool {
        let Some(window) = self.window.upgrade() else {
            return false;
        };
        window.invoke_flush_advanced_draft();
        let target = self.advanced.borrow().invalid.keys().next().cloned();
        let Some(target) = target else {
            return false;
        };
        self.advanced.borrow_mut().pending_focus = Some(target);
        window.set_advanced_open(true);
        self.view_advanced(|_| {});
        true
    }

    pub(super) fn preview_simple_preset(self: &Rc<Self>, intent: &Intent) -> bool {
        let Some(candidate) = simple_preset_candidate(self.session.borrow().state(), intent) else {
            return false;
        };
        let Some(window) = self.window.upgrade() else {
            return true;
        };
        window.set_advanced_open(true);
        window.set_advanced_preset_candidate(i32::from(candidate.base.preset.get()));
        self.queue_preset(Some(candidate));
        true
    }

    fn preview_preset(self: &Rc<Self>, preset: i32) {
        let proposed = self
            .session
            .borrow()
            .state()
            .proposed_options(&Intent::PresetEdited(preset));
        self.queue_preset(proposed);
    }

    fn queue_preset(self: &Rc<Self>, candidate: Option<OptionRequest>) {
        self.queue_preset_preview(candidate, None);
    }

    pub(super) fn queue_preset_preview(
        self: &Rc<Self>,
        candidate: Option<OptionRequest>,
        imported_changes: Option<String>,
    ) {
        let mut editor = self.advanced.borrow_mut();
        editor.candidate = candidate;
        editor.imported_changes = imported_changes;
        editor.preview_started = false;
        drop(editor);
        let outcome = self.session.borrow_mut().advanced_cancel_preview();
        self.handle(outcome);
        self.view_advanced(|_| {});
        self.check_preset_preview();
    }

    pub(super) fn check_preset_preview(self: &Rc<Self>) {
        let mut session = self.session.borrow_mut();
        if !has_source(session.state())
            || session.advanced_pending()
            || (session.advanced_policy().is_empty() && session.advanced_error().is_none())
        {
            return;
        }
        let mut editor = self.advanced.borrow_mut();
        if editor.preview_started {
            return;
        }
        let Some(candidate) = editor.candidate.clone() else {
            return;
        };
        editor.preview_started = true;
        drop(editor);
        let outcome = session.advanced_preview(&candidate);
        drop(session);
        self.handle(outcome);
    }

    fn apply_preset(self: &Rc<Self>) {
        if self.block_invalid_advanced() {
            return;
        }
        let ready = can_apply_preset(&self.session.borrow());
        let candidate = self.advanced.borrow_mut().candidate.take_if(|_| ready);
        if let Some(candidate) = candidate {
            self.dispatch(Intent::ReplaceOptions(candidate));
        }
    }
}

impl Editor {
    fn reconcile_invalid(
        &mut self,
        key: &str,
        action: &str,
        fields: &[advanced::fields::FieldRow],
    ) {
        self.invalid = std::mem::take(&mut self.invalid)
            .into_iter()
            .filter_map(|((option, field), draft)| {
                if option != key {
                    return Some(((option, field), draft));
                }
                let field = reindex_draft(field, action)?;
                fields
                    .iter()
                    .any(|row| row.id == field && row.kind != "readonly")
                    .then_some(((option, field), draft))
            })
            .collect();
    }

    fn render_rows(
        &mut self,
        window: &AppWindow,
        catalog: &[OptionDescriptor],
        request: &OptionRequest,
        engine_error: bool,
    ) {
        let mut rows = advanced::options(
            catalog,
            request,
            self.query.as_str(),
            self.modified,
            self.expert,
        )
        .into_iter()
        .map(|row| AdvancedOptionRow {
            key: row.key.into(),
            label: row.title.into(),
            group: row.group.into(),
            modified: row.modified,
        })
        .collect::<Vec<_>>();
        if !rows.iter().any(|row| row.key == self.selected.as_str())
            && (self.pending_focus.is_some()
                || self.invalid.keys().any(|(key, _)| *key == self.selected)
                || engine_error)
        {
            if let Some(descriptor) = selected(catalog, &self.selected) {
                rows.insert(
                    0,
                    AdvancedOptionRow {
                        key: descriptor.key.as_str().into(),
                        label: advanced::catalog::title(descriptor).into(),
                        group: advanced::catalog::group(descriptor).into(),
                        modified: request.values.contains_key(&descriptor.key),
                    },
                );
            } else if request.values.contains_key(&self.selected) {
                rows.insert(
                    0,
                    AdvancedOptionRow {
                        key: self.selected.as_str().into(),
                        label: self.selected.as_str().into(),
                        group: "Unavailable".into(),
                        modified: true,
                    },
                );
            }
        }
        if !engine_error
            && self.pending_focus.is_none()
            && !rows.iter().any(|row| row.key == self.selected.as_str())
        {
            self.selected = rows
                .first()
                .map_or_else(String::new, |row| row.key.to_string());
        }
        if rows != self.rows {
            window.set_advanced_options(model(rows.clone()));
            self.rows = rows;
        }
    }

    fn render_detail(
        &mut self,
        window: &AppWindow,
        catalog: &[OptionDescriptor],
        request: &OptionRequest,
        policy: &[EffectiveOption],
    ) {
        let detail =
            selected(catalog, &self.selected).map(|d| advanced::inspect(d, request, policy));
        window.set_advanced_selected(self.selected.as_str().into());
        let (label, help, effective, origin, screen, fields) = detail.map_or_else(
            || {
                if request.values.contains_key(&self.selected) {
                    return (
                        self.selected.clone(),
                        "This encoder cannot use this saved option. Reset the option to remove it."
                            .into(),
                        String::new(),
                        String::new(),
                        String::new(),
                        Vec::new(),
                    );
                }
                Default::default()
            },
            |detail| {
                let fields = detail
                    .fields
                    .into_iter()
                    .map(|row| FieldRow {
                        label: row.label.into(),
                        kind: row.kind.into(),
                        text: self
                            .invalid
                            .get(&(self.selected.clone(), row.id.clone()))
                            .map_or(row.text.as_str(), |invalid| invalid.text.as_str())
                            .into(),
                        id: row.id.into(),
                        choices: model(row.choices.into_iter().map(SharedString::from).collect()),
                        help: row.help.into(),
                    })
                    .collect::<Vec<_>>();
                (
                    detail.title,
                    detail.help,
                    detail.effective,
                    detail.origin,
                    detail.screen,
                    fields,
                )
            },
        );
        window.set_advanced_label(label.into());
        window.set_advanced_help(help.into());
        window.set_advanced_effective(effective.into());
        window.set_advanced_origin(origin.into());
        window.set_advanced_screen(screen.into());
        // Reuse the native inputs while their displayed values stay the same.
        if self.fields_key != self.selected || !same_fields(&fields, &self.fields) {
            self.fields_key.clone_from(&self.selected);
            window.set_advanced_fields(model(fields.clone()));
            self.fields = fields;
        }
    }

    fn render_preset(&self, window: &AppWindow, session: &Session, baseline: &[EffectiveOption]) {
        let candidate = session.advanced_preview_policy();
        window.set_advanced_can_apply_preset(
            self.candidate.is_some() && self.invalid.is_empty() && can_apply_preset(session),
        );
        let text = self.candidate.as_ref().map_or_else(String::new, |request| {
            if !has_source(session.state()) {
                "Open a source to compare encoder policy. You can apply this preset now.".into()
            } else if let Some(error) = session.advanced_preview_error() {
                error.message.clone()
            } else if session.advanced_pending() || session.advanced_preview_pending() {
                format!("Resolving preset {}…", request.base.preset.get())
            } else if candidate.is_empty() {
                "Check the current settings to compare presets.".into()
            } else if baseline.is_empty() {
                "The proposed preset resolves. Correct the current settings to compare policy."
                    .into()
            } else {
                advanced::differences(baseline, candidate)
            }
        });
        let imported = self
            .candidate
            .as_ref()
            .and(self.imported_changes.as_deref());
        window.set_advanced_imported_preview(imported.is_some());
        let text = if let Some(changes) = imported {
            format!("{text}\n\n{changes}")
        } else {
            text
        };
        window.set_advanced_preset_preview(text.into());
    }

    fn select_error(
        &mut self,
        catalog: &[OptionDescriptor],
        request: &OptionRequest,
        error: Option<&FieldError>,
    ) {
        if self.engine_error.as_ref() == error {
            return;
        }
        self.engine_error = error.cloned();
        let Some(error) = error else {
            return;
        };
        if request.values.contains_key(&error.field) && selected(catalog, &error.field).is_none() {
            self.pending_focus
                .get_or_insert_with(|| (error.field.clone(), "@reset".into()));
            return;
        }
        let descriptor = catalog
            .iter()
            .filter(|d| {
                error.field == d.key
                    || error
                        .field
                        .strip_prefix(&d.key)
                        .is_some_and(|suffix| suffix.starts_with('.'))
            })
            .max_by_key(|d| d.key.len());
        if let Some(descriptor) = descriptor {
            let suffix = error
                .field
                .strip_prefix(&descriptor.key)
                .unwrap_or_default();
            self.pending_focus.get_or_insert_with(|| {
                (
                    descriptor.key.clone(),
                    format!("value{}", suffix.replace('.', "/")),
                )
            });
        }
    }

    fn focus_field(&mut self) -> Option<String> {
        let (key, field) = self.pending_focus.as_ref()?;
        if *key != self.fields_key {
            return None;
        }
        if field == "@reset" {
            return self.pending_focus.take().map(|(_, field)| field);
        }
        if self.fields.is_empty() {
            return None;
        }
        let field = self
            .fields
            .iter()
            .find(|row| row.id == field.as_str() && row.kind != "readonly")
            .or_else(|| self.fields.iter().find(|row| row.kind != "readonly"))
            .map(|row| row.id.to_string());
        self.pending_focus = None;
        field
    }
}

fn reindex_draft(field: String, action: &str) -> Option<String> {
    let Some((list, removed)) = action.rsplit_once("/@remove") else {
        return Some(field);
    };
    let Some(row) = field
        .strip_prefix(list)
        .and_then(|suffix| suffix.strip_prefix('/'))
    else {
        return Some(field);
    };
    let (index, suffix) = row
        .split_once('/')
        .map_or((row, ""), |(index, suffix)| (index, suffix));
    let (Ok(index), Ok(removed)) = (index.parse::<usize>(), removed.parse::<usize>()) else {
        return Some(field);
    };
    match index.cmp(&removed) {
        std::cmp::Ordering::Less => Some(field),
        std::cmp::Ordering::Equal => None,
        std::cmp::Ordering::Greater => {
            let index = index - 1;
            Some(if suffix.is_empty() {
                format!("{list}/{index}")
            } else {
                format!("{list}/{index}/{suffix}")
            })
        }
    }
}

fn simple_preset_candidate(state: &AppState, intent: &Intent) -> Option<OptionRequest> {
    if matches!(
        intent,
        Intent::PrioritySelected(_) | Intent::PresetEdited(_)
    ) && !state.option_request().values.is_empty()
    {
        state.proposed_options(intent)
    } else {
        None
    }
}

fn has_source(state: &AppState) -> bool {
    state.selected().and_then(Item::facts).is_some()
}

fn can_apply_preset(session: &Session) -> bool {
    !session.state().settings_view().read_only
        && (!has_source(session.state()) || !session.advanced_preview_policy().is_empty())
}

fn selected<'a>(catalog: &'a [OptionDescriptor], key: &str) -> Option<&'a OptionDescriptor> {
    catalog.iter().find(|d| d.key == key)
}

fn same_fields(new: &[FieldRow], previous: &[FieldRow]) -> bool {
    use slint::Model;
    new.len() == previous.len()
        && new.iter().zip(previous).all(|(a, b)| {
            a.id == b.id
                && a.label == b.label
                && a.kind == b.kind
                && a.text == b.text
                && a.help == b.help
                && a.choices.iter().eq(b.choices.iter())
        })
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use studio_domain::options::OptionValue;

    use crate::recovery::RealDisk;
    use crate::session::Session;
    use crate::simple::Priority;
    use crate::state::Startup;

    use super::*;

    #[test]
    fn simple_preview_keeps_any_override_and_can_apply_without_a_source() {
        let now = Instant::now();
        let mut state = AppState::new(Startup::default());
        let intent = Intent::PrioritySelected(0);
        assert!(simple_preset_candidate(&state, &intent).is_none());
        let override_value = OptionValue::Integer(96);
        state.apply(
            Intent::OptionEdited {
                key: "keyint".into(),
                value: Some(override_value.clone()),
            },
            now,
            &RealDisk,
        );
        let proposal = simple_preset_candidate(&state, &intent);
        assert!(proposal.is_some());
        assert_eq!(
            proposal.as_ref().map(|request| request.base.preset),
            Some(Priority::Fast.preset())
        );
        assert_eq!(
            proposal
                .as_ref()
                .and_then(|request| request.values.get("keyint")),
            Some(&override_value)
        );
        assert_ne!(state.option_request().base.preset, Priority::Fast.preset());
        let session = Session::new(
            state,
            Err(String::new()),
            std::sync::Arc::new(|_, _| {}),
            None,
        );
        assert!(can_apply_preset(&session));
    }
}
