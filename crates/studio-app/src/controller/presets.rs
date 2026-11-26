// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::thread;

use slint::ComponentHandle;
use studio_domain::options::{OptionDescriptor, OptionRequest, OptionValue};
use studio_domain::protocol::OptionsIdentity;

use crate::portable_preset::{self, PresetChange};
use crate::ui::AppWindow;

use super::{Controller, current, with};

#[derive(Default)]
pub(super) struct PresetFiles {
    revision: Cell<u64>,
    active: RefCell<Option<Arc<()>>>,
}

#[derive(Clone)]
struct Ticket {
    revision: u64,
    id: Arc<()>,
}

impl PresetFiles {
    pub(super) fn invalidate(&self) {
        self.revision.set(self.revision.get().wrapping_add(1));
    }

    pub(super) fn busy(&self) -> bool {
        self.active.borrow().is_some()
    }

    fn begin(&self) -> Option<Ticket> {
        let mut active = self.active.borrow_mut();
        if active.is_some() {
            return None;
        }
        let id = Arc::new(());
        *active = Some(Arc::clone(&id));
        Some(Ticket {
            revision: self.revision.get(),
            id,
        })
    }

    fn matches(&self, ticket: &Ticket) -> bool {
        self.active
            .borrow()
            .as_ref()
            .is_some_and(|id| Arc::ptr_eq(id, &ticket.id))
    }

    fn valid(&self, ticket: &Ticket) -> bool {
        self.matches(ticket) && ticket.revision == self.revision.get()
    }

    fn finish(&self, ticket: &Ticket) -> Option<bool> {
        if !self.matches(ticket) {
            return None;
        }
        self.active.borrow_mut().take();
        Some(ticket.revision == self.revision.get())
    }
}

struct FileInput {
    identity: OptionsIdentity,
    request: OptionRequest,
    catalog: Vec<OptionDescriptor>,
}

enum FileResult {
    Cancelled,
    Saved,
    Unchanged,
    Import {
        request: OptionRequest,
        changes: String,
    },
}

impl FileInput {
    fn run(self, path: &Path, import: bool) -> Result<FileResult, String> {
        let Self {
            identity,
            request,
            catalog,
        } = self;
        if !import {
            portable_preset::export(path, &identity, &request, &catalog)
                .map_err(|error| error.to_string())?;
            return Ok(FileResult::Saved);
        }
        let preview = portable_preset::preview_import(path, &identity, &request, &catalog)
            .map_err(|error| error.to_string())?;
        if preview.changes.is_empty() {
            Ok(FileResult::Unchanged)
        } else {
            Ok(FileResult::Import {
                request: preview.request,
                changes: change_summary(&preview.changes),
            })
        }
    }
}

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
        if self.preset_files.busy() || self.block_invalid_advanced() {
            return;
        }
        let input = match self.preset_file_input(import) {
            Ok(input) => input,
            Err(error) => {
                preset_status(&window, error, true);
                return;
            }
        };
        let Some(ticket) = self.preset_files.begin() else {
            return;
        };
        window.set_advanced_preset_files_ready(false);
        preset_status(&window, "", false);
        let dialog = rfd::AsyncFileDialog::new()
            .set_parent(&window.window().window_handle())
            .add_filter("Splot presets", &["json"]);
        let weak = Rc::downgrade(self);
        let failed = ticket.clone();
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
            with(&weak, |ctl| {
                if let Some(file) = selected.filter(|_| ctl.preset_files.valid(&ticket)) {
                    ctl.run_preset_file(ticket, input, file.path().to_path_buf(), import);
                } else {
                    ctl.finish_preset_file(&ticket, Ok(FileResult::Cancelled));
                }
            });
        });
        if spawned.is_err() {
            self.finish_preset_file(&failed, Err("Cannot open the preset file dialog.".into()));
        }
    }

    fn preset_file_input(&self, import: bool) -> Result<FileInput, &'static str> {
        let session = self.session.borrow();
        let state = session.state();
        let identity = state
            .capabilities()
            .and_then(|cap| cap.options.clone())
            .ok_or("Wait for the encoder before using a preset file.")?;
        let catalog = session.advanced_catalog();
        if catalog.is_empty() {
            return Err("Wait for the encoder options before using a preset file.");
        }
        if import && state.settings_view().read_only {
            return Err("Select an editable job before importing a preset.");
        }
        Ok(FileInput {
            identity,
            catalog: catalog.to_vec(),
            request: state.visible_option_request().clone(),
        })
    }

    fn run_preset_file(
        self: &Rc<Self>,
        ticket: Ticket,
        input: FileInput,
        path: PathBuf,
        import: bool,
    ) {
        if let Some(window) = self.window.upgrade() {
            preset_status(
                &window,
                if import {
                    "Reading preset…"
                } else {
                    "Saving preset…"
                },
                false,
            );
        }
        let failed = ticket.clone();
        let worker = thread::Builder::new()
            .name("studio-preset-file".into())
            .spawn(move || {
                let result = input.run(&path, import);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ctl) = current() {
                        ctl.finish_preset_file(&ticket, result);
                    }
                });
            });
        if let Err(error) = worker {
            self.finish_preset_file(
                &failed,
                Err(format!("Cannot start the preset file task. {error}")),
            );
        }
    }

    fn finish_preset_file(self: &Rc<Self>, ticket: &Ticket, result: Result<FileResult, String>) {
        let Some(valid) = self.preset_files.finish(ticket) else {
            return;
        };
        let Some(window) = self.window.upgrade() else {
            return;
        };
        preset_status(&window, "", false);
        self.render_advanced(&window, &self.session.borrow());
        if valid && self.preset_files.revision.get() == ticket.revision {
            match result {
                Ok(FileResult::Cancelled) => {}
                Ok(FileResult::Saved) => preset_status(&window, "Preset saved.", false),
                Ok(FileResult::Unchanged) => {
                    preset_status(&window, "The preset matches the current settings.", false);
                }
                Ok(FileResult::Import { request, changes }) => {
                    self.queue_preset_preview(Some(request), Some(changes));
                }
                Err(error) => preset_status(&window, &error, true),
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_files_serialize_requests_and_discard_stale_results() {
        let files = PresetFiles::default();
        let first = files
            .begin()
            .unwrap_or_else(|| unreachable!("The slot is free."));
        assert!(files.valid(&first));
        assert!(files.busy());
        assert!(files.begin().is_none());

        files.invalidate();
        assert!(!files.valid(&first));
        assert!(files.begin().is_none());
        assert_eq!(files.finish(&first), Some(false));

        let second = files
            .begin()
            .unwrap_or_else(|| unreachable!("The slot is free."));
        assert_eq!(files.finish(&first), None);
        assert!(files.valid(&second));
        assert_eq!(files.finish(&second), Some(true));
        assert!(!files.busy());
    }
}
