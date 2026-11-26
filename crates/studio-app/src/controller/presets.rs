// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use std::path::Path;
use std::rc::Rc;

use slint::ComponentHandle;
use studio_domain::options::OptionValue;

use crate::portable_preset::{self, PresetChange};
use crate::ui::AppWindow;

use super::{Controller, with};

impl Controller {
    pub(super) fn wire_presets(self: &Rc<Self>, window: &AppWindow) {
        window.on_advanced_save_preset({
            let ctl = Rc::downgrade(self);
            move || with(&ctl, |ctl| ctl.choose_preset_file(false))
        });
        window.on_advanced_import_preset({
            let ctl = Rc::downgrade(self);
            move || with(&ctl, |ctl| ctl.choose_preset_file(true))
        });
    }

    fn choose_preset_file(self: &Rc<Self>, import: bool) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        if self.block_invalid_advanced() {
            return;
        }
        preset_status(&window, "", false);
        let dialog = rfd::AsyncFileDialog::new()
            .set_parent(&window.window().window_handle())
            .add_filter("Splot presets", &["json"]);
        let weak = Rc::downgrade(self);
        let spawned = slint::spawn_local(async move {
            let selected = if import {
                dialog.set_title("Import Splot Preset").pick_file().await
            } else {
                dialog
                    .set_title("Save Splot Preset")
                    .set_file_name("Custom.splot-preset.json")
                    .save_file()
                    .await
            };
            if let Some(file) = selected {
                with(&weak, |ctl| ctl.use_preset_file(file.path(), import));
            }
        });
        if spawned.is_err() {
            preset_status(&window, "Cannot open the preset file dialog.", true);
        }
    }

    fn use_preset_file(self: &Rc<Self>, path: &Path, import: bool) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let session = self.session.borrow();
        let state = session.state();
        let Some(identity) = state.capabilities().and_then(|cap| cap.options.as_ref()) else {
            preset_status(
                &window,
                "Wait for the encoder before using a preset file.",
                true,
            );
            return;
        };
        let catalog = session.advanced_catalog();
        if catalog.is_empty() {
            preset_status(
                &window,
                "Wait for the encoder options before using a preset file.",
                true,
            );
            return;
        }
        let request = state.visible_option_request();
        if !import {
            match portable_preset::export(path, identity, request, catalog) {
                Ok(()) => preset_status(&window, "Preset saved.", false),
                Err(error) => preset_status(&window, &error.to_string(), true),
            }
            return;
        }
        if state.settings_view().read_only {
            preset_status(
                &window,
                "Select an editable job before importing a preset.",
                true,
            );
            return;
        }
        let imported = portable_preset::preview_import(path, identity, request, catalog);
        drop(session);
        match imported {
            Ok(preview) if !preview.changes.is_empty() => {
                self.queue_preset_preview(
                    Some(preview.request),
                    Some(change_summary(&preview.changes)),
                );
            }
            Ok(_) => preset_status(&window, "The preset matches the current settings.", false),
            Err(error) => preset_status(&window, &error.to_string(), true),
        }
    }
}

fn preset_status(window: &AppWindow, message: &str, error: bool) {
    window.set_advanced_preset_file_status(message.into());
    window.set_advanced_preset_file_error(error);
}

fn change_summary(changes: &[PresetChange]) -> String {
    changes
        .iter()
        .map(|change| {
            format!(
                "{}\n{} → {}",
                change.key,
                change
                    .before
                    .as_ref()
                    .map_or_else(|| "Inherit".into(), value_text),
                change
                    .after
                    .as_ref()
                    .map_or_else(|| "Inherit".into(), value_text)
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn value_text(value: &OptionValue) -> String {
    match value {
        OptionValue::Bool(value) => if *value { "On" } else { "Off" }.into(),
        OptionValue::Integer(value) => value.to_string(),
        OptionValue::Choice(value) => value.clone(),
        OptionValue::None => "None".into(),
        OptionValue::List(values) => values.iter().map(value_text).collect::<Vec<_>>().join(", "),
        OptionValue::Record(values) => values
            .iter()
            .map(|(key, value)| format!("{key} = {}", value_text(value)))
            .collect::<Vec<_>>()
            .join(", "),
    }
}
