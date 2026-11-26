// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use std::rc::Rc;
use std::time::Duration;

use slint::{SharedString, TimerMode};
use studio_domain::job::SourceFacts;
use studio_domain::options::{EffectiveOption, FieldError, OptionDescriptor, OptionRequest};
use studio_domain::protocol::OptionsIdentity;

use crate::advanced::{self, Reset};
use crate::queue::Item;
use crate::render::model;
use crate::session::Session;
use crate::state::Intent;
use crate::ui::{AdvancedOptionRow, AppWindow, FieldRow};
use crate::workspace::Workspace;

use super::{Controller, with};

const SETTLE: Duration = Duration::from_millis(180);

#[derive(PartialEq, Eq)]
struct Inputs {
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
    edit_error: Option<String>,
    engine_error: Option<FieldError>,
    candidate: Option<OptionRequest>,
    rows: Vec<AdvancedOptionRow>,
    fields: Vec<FieldRow>,
    fields_key: String,
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
                    ctl.view_advanced(|editor| editor.modified = modified)
                })
            }
        });
        window.on_advanced_expert({
            let ctl = Rc::downgrade(self);
            move |expert| {
                with(&ctl, |ctl| {
                    ctl.view_advanced(|editor| editor.expert = expert)
                })
            }
        });
        window.on_advanced_select({
            let ctl = Rc::downgrade(self);
            move |key| {
                with(&ctl, |ctl| {
                    ctl.view_advanced(|editor| {
                        if editor.selected != key.as_str() {
                            editor.selected = key.to_string();
                            editor.edit_error = None;
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
        self.advanced_settle.stop();
        let mut editor = self.advanced.borrow_mut();
        editor.inputs = None;
        editor.candidate = None;
        editor.edit_error = None;
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
            return;
        }
        let state = session.state();
        let locked = state.settings_view().read_only;
        let request = state.visible_option_request();
        let item = state.selected();
        let inputs = Inputs {
            request: request.clone(),
            source: item.and_then(Item::facts).cloned(),
            engine: state.capabilities().and_then(|cap| cap.options.clone()),
            locked,
        };
        let mut editor = self.advanced.borrow_mut();
        if editor.inputs.as_ref() != Some(&inputs) {
            let refresh = editor.inputs.is_some() && !locked;
            editor.inputs = Some(inputs);
            editor.candidate = None;
            editor.edit_error = None;
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
        let policy = if locked {
            item.and_then(Item::attempt)
                .map_or(&[][..], |attempt| &attempt.policy)
        } else {
            session.advanced_policy()
        };
        let error = (!locked).then(|| session.advanced_error()).flatten();
        let focus = editor.select_error(catalog, error);
        let rows = advanced::options(
            catalog,
            request,
            editor.query.as_str(),
            editor.modified,
            editor.expert,
        )
        .into_iter()
        .map(|row| AdvancedOptionRow {
            key: row.key.into(),
            label: row.title.into(),
            group: row.group.into(),
            modified: row.modified,
        })
        .collect::<Vec<_>>();
        if error.is_none() && !rows.iter().any(|row| row.key == editor.selected.as_str()) {
            editor.selected = rows
                .first()
                .map_or_else(String::new, |row| row.key.to_string());
        }
        if rows != editor.rows {
            window.set_advanced_options(model(rows.clone()));
            editor.rows = rows;
        }
        editor.render_detail(window, catalog, request, policy);
        let error_text = editor
            .edit_error
            .clone()
            .or_else(|| error.map(|error| error.message.clone()));
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
        drop(editor);
        if let Some(field) = focus {
            window.invoke_focus_advanced_error(field.into());
        }
    }

    fn edit_advanced(self: &Rc<Self>, id: &str, text: &str) {
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
            advanced::edit(
                descriptor,
                state.option_request(),
                session.advanced_policy(),
                id,
                text,
            )
        };
        match proposed {
            Ok(proposed) => {
                self.advanced.borrow_mut().edit_error = None;
                self.dispatch(Intent::ReplaceOptions(proposed));
            }
            Err(error) => self.view_advanced(|editor| editor.edit_error = Some(error)),
        }
    }

    fn reset_advanced(self: &Rc<Self>, scope: &str) {
        let proposed = {
            let session = self.session.borrow();
            let state = session.state();
            if state.settings_view().read_only {
                return;
            }
            let editor = self.advanced.borrow();
            let catalog = session.advanced_catalog();
            let scope = match scope {
                "all" => Reset::All,
                "selected" => Reset::Field(&editor.selected),
                "group" => {
                    let Some(row) = editor
                        .rows
                        .iter()
                        .find(|row| row.key == editor.selected.as_str())
                    else {
                        return;
                    };
                    Reset::Group(row.group.as_str())
                }
                _ => return,
            };
            advanced::reset(state.option_request(), catalog, scope)
        };
        self.dispatch(Intent::ReplaceOptions(proposed));
    }

    fn preview_preset(self: &Rc<Self>, preset: i32) {
        let proposed = self
            .session
            .borrow()
            .state()
            .proposed_options(&Intent::PresetEdited(preset));
        self.advanced.borrow_mut().candidate = proposed.clone();
        let outcome = if let Some(proposed) = proposed {
            self.session.borrow_mut().advanced_preview(&proposed)
        } else {
            self.session.borrow_mut().advanced_cancel_preview()
        };
        self.handle(outcome);
        self.view_advanced(|_| {});
    }

    fn apply_preset(self: &Rc<Self>) {
        let ready = {
            let session = self.session.borrow();
            !session.advanced_policy().is_empty() && !session.advanced_preview_policy().is_empty()
        };
        let candidate = self.advanced.borrow_mut().candidate.take_if(|_| ready);
        if let Some(candidate) = candidate {
            self.dispatch(Intent::ReplaceOptions(candidate));
        }
    }
}

impl Editor {
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
        let (label, help, effective, origin, screen, fields) =
            detail.map_or_else(Default::default, |detail| {
                let fields = detail
                    .fields
                    .into_iter()
                    .map(|row| FieldRow {
                        id: row.id.into(),
                        label: row.label.into(),
                        kind: row.kind.into(),
                        text: row.text.into(),
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
            });
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
            self.candidate.is_some() && !baseline.is_empty() && !candidate.is_empty(),
        );
        let text = self.candidate.as_ref().map_or_else(String::new, |request| {
            if let Some(error) = session.advanced_preview_error() {
                error.message.clone()
            } else if session.advanced_preview_pending() {
                format!("Resolving preset {}…", request.base.preset.get())
            } else if candidate.is_empty() || baseline.is_empty() {
                "Add a source and resolve its current settings to compare presets.".into()
            } else {
                advanced::differences(baseline, candidate)
            }
        });
        window.set_advanced_preset_preview(text.into());
    }

    fn select_error(
        &mut self,
        catalog: &[OptionDescriptor],
        error: Option<&FieldError>,
    ) -> Option<String> {
        if self.engine_error.as_ref() == error {
            return None;
        }
        self.engine_error = error.cloned();
        let error = error?;
        let descriptor = catalog
            .iter()
            .filter(|d| {
                error.field == d.key
                    || error
                        .field
                        .strip_prefix(&d.key)
                        .is_some_and(|suffix| suffix.starts_with('.'))
            })
            .max_by_key(|d| d.key.len())?;
        self.selected.clone_from(&descriptor.key);
        Some(
            error
                .field
                .strip_prefix(&descriptor.key)
                .and_then(|suffix| suffix.strip_prefix('.'))
                .unwrap_or_default()
                .into(),
        )
    }
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
