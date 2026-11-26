// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Native file dialogs for validation jobs and report export.

use std::cell::{Cell, RefCell};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::thread;

use slint::ComponentHandle;
use studio_domain::validation::{MAX_EXTERNAL_HLS_BYTES, ReportFormat, ValidationSettings};

use crate::ui::AppWindow;

use super::{Controller, current, with};

#[derive(Default)]
pub(super) struct BatchFiles {
    draft: RefCell<Option<Arc<()>>>,
    context: RefCell<Option<Vec<u8>>>,
    reading_context: RefCell<Option<Arc<()>>>,
    saving: Cell<bool>,
}

impl BatchFiles {
    fn matches(&self, draft: &Arc<()>) -> bool {
        self.draft
            .borrow()
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, draft))
    }
}

impl Controller {
    pub(super) fn wire_batch(self: &Rc<Self>, window: &AppWindow) {
        window.on_validate_files_requested({
            let ctl = Rc::downgrade(self);
            move || with(&ctl, |ctl| ctl.show_batch_setup())
        });
        window.on_validation_cancel({
            let ctl = Rc::downgrade(self);
            move || with(&ctl, |ctl| ctl.close_batch_setup())
        });
        window.on_validation_choose_files({
            let ctl = Rc::downgrade(self);
            move || with(&ctl, Controller::choose_validation_files)
        });
        window.on_validation_pick_context({
            let ctl = Rc::downgrade(self);
            move || with(&ctl, Controller::choose_validation_context)
        });
        window.on_validation_clear_context({
            let ctl = Rc::downgrade(self);
            move || {
                with(&ctl, |ctl| {
                    if let Some(window) = ctl.window.upgrade() {
                        ctl.batch_files.context.borrow_mut().take();
                        window.set_validation_context("".into());
                        window.set_validation_notice("".into());
                    }
                });
            }
        });
        window.on_job_export({
            let ctl = Rc::downgrade(self);
            move |index, json| {
                let Ok(index) = usize::try_from(index) else {
                    return;
                };
                let format = if json {
                    ReportFormat::Json
                } else {
                    ReportFormat::Text
                };
                with(&ctl, |ctl| ctl.choose_report_destination(index, format));
            }
        });
        window.on_job_remove({
            let ctl = Rc::downgrade(self);
            move |index| {
                let Ok(index) = usize::try_from(index) else {
                    return;
                };
                with(&ctl, |ctl| {
                    let outcome = ctl.session.borrow_mut().batch_remove(index);
                    ctl.handle(outcome);
                });
            }
        });
        window.on_stop_validation_batch({
            let ctl = Rc::downgrade(self);
            move || {
                with(&ctl, |ctl| {
                    let outcome = ctl.session.borrow_mut().batch_cancel_remaining();
                    ctl.handle(outcome);
                });
            }
        });
    }

    fn show_batch_setup(&self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        if window.get_validation_open()
            || window.get_sheet_open()
            || self.session.borrow().batch_active()
        {
            return;
        }
        *self.batch_files.draft.borrow_mut() = Some(Arc::new(()));
        window.set_validation_strict(false);
        window.set_validation_busy(false);
        window.set_validation_notice("".into());
        window.set_validation_context("".into());
        window.invoke_show_validation_setup();
    }

    fn close_batch_setup(&self) {
        self.batch_files.draft.borrow_mut().take();
        self.batch_files.context.borrow_mut().take();
        if let Some(window) = self.window.upgrade() {
            window.invoke_close_validation_setup();
        }
    }

    fn batch_dialog(&self, window: &AppWindow) -> Option<Arc<()>> {
        if window.get_validation_busy() {
            return None;
        }
        let draft = self.batch_files.draft.borrow().clone()?;
        window.set_validation_busy(true);
        window.set_validation_notice("".into());
        Some(draft)
    }

    fn choose_validation_files(self: &Rc<Self>) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let Some(draft) = self.batch_dialog(&window) else {
            return;
        };
        let dialog = rfd::AsyncFileDialog::new()
            .set_parent(&window.window().window_handle())
            .set_title("Choose AV2 Files to Validate");
        // The worker identifies the input by its contents.
        #[cfg(not(target_os = "macos"))]
        let dialog = dialog
            .add_filter("AV2 streams", &["ivf", "obu"])
            .add_filter("All files", &["*"]);
        let picked = dialog.pick_files();
        let weak = Rc::downgrade(self);
        if slint::spawn_local(async move {
            let selected = picked.await;
            with(&weak, |ctl| {
                let Some(window) = ctl.window.upgrade() else {
                    return;
                };
                if !ctl.batch_files.matches(&draft) {
                    return;
                }
                window.set_validation_busy(false);
                if let Some(files) = selected.filter(|files| !files.is_empty()) {
                    let paths = files.iter().map(|file| file.path().to_path_buf()).collect();
                    let settings = ValidationSettings {
                        strict: window.get_validation_strict(),
                        external_hls: ctl.batch_files.context.borrow_mut().take(),
                    };
                    ctl.close_batch_setup();
                    let outcome = ctl.session.borrow_mut().batch_start(paths, settings);
                    ctl.handle(outcome);
                    window.set_report_dialog_error("".into());
                    window.invoke_show_jobs();
                }
            });
        })
        .is_err()
        {
            window.set_validation_busy(false);
            window.set_validation_notice("Cannot open the file selection dialog.".into());
        }
    }

    fn choose_validation_context(self: &Rc<Self>) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        if self.batch_files.reading_context.borrow().is_some() {
            window.set_validation_notice("Wait for the context file to finish reading.".into());
            return;
        }
        let Some(draft) = self.batch_dialog(&window) else {
            return;
        };
        let picked = rfd::AsyncFileDialog::new()
            .set_parent(&window.window().window_handle())
            .set_title("Choose External AV2 Context")
            .add_filter("Annex B streams", &["obu"])
            .pick_file();
        let weak = Rc::downgrade(self);
        if slint::spawn_local(async move {
            let selected = picked.await;
            with(&weak, |ctl| {
                if !ctl.batch_files.matches(&draft) {
                    return;
                }
                if let Some(file) = selected {
                    ctl.read_validation_context(draft, file.path().to_path_buf());
                } else if let Some(window) = ctl.window.upgrade() {
                    window.set_validation_busy(false);
                }
            });
        })
        .is_err()
        {
            window.set_validation_busy(false);
            window.set_validation_notice("Cannot open the context selection dialog.".into());
        }
    }

    fn read_validation_context(&self, draft: Arc<()>, path: PathBuf) {
        *self.batch_files.reading_context.borrow_mut() = Some(Arc::clone(&draft));
        let window = self.window.clone();
        let spawned = thread::Builder::new()
            .name("validation-context".into())
            .spawn(move || {
                let name = crate::present::file_name(&path);
                let result = read_context(&path);
                let _ = window.upgrade_in_event_loop(move |window| {
                    let Some(ctl) = current() else { return };
                    let mut reading = ctl.batch_files.reading_context.borrow_mut();
                    if !reading
                        .as_ref()
                        .is_some_and(|ticket| Arc::ptr_eq(ticket, &draft))
                    {
                        return;
                    }
                    reading.take();
                    drop(reading);
                    if !ctl.batch_files.matches(&draft) {
                        return;
                    }
                    window.set_validation_busy(false);
                    match result {
                        Ok(bytes) => {
                            *ctl.batch_files.context.borrow_mut() = Some(bytes);
                            window.set_validation_context(name.into());
                        }
                        Err(error) => window.set_validation_notice(error.into()),
                    }
                });
            });
        if spawned.is_err() {
            self.batch_files.reading_context.borrow_mut().take();
            if let Some(window) = self.window.upgrade() {
                window.set_validation_busy(false);
                window.set_validation_notice("Cannot start the context file read.".into());
            }
        }
    }

    fn choose_report_destination(self: &Rc<Self>, index: usize, format: ReportFormat) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        if self.batch_files.saving.get() {
            return;
        }
        let Some((id, name)) = self.session.borrow().batch_export_target(index) else {
            return;
        };
        let (label, extension) = match format {
            ReportFormat::Json => ("JSON report", "json"),
            ReportFormat::Text => ("Text report", "txt"),
        };
        self.batch_files.saving.set(true);
        window.set_report_dialog_error("".into());
        let selected = rfd::AsyncFileDialog::new()
            .set_parent(&window.window().window_handle())
            .set_title("Save Full Validation Report")
            .add_filter(label, &[extension])
            .set_file_name(format!("{name}.{extension}"))
            .save_file();
        let weak = Rc::downgrade(self);
        if slint::spawn_local(async move {
            let path = selected.await;
            with(&weak, |ctl| {
                ctl.batch_files.saving.set(false);
                if let Some(file) = path {
                    let outcome = ctl
                        .session
                        .borrow_mut()
                        .batch_export(id, file.path(), format);
                    ctl.handle(outcome);
                }
            });
        })
        .is_err()
        {
            self.batch_files.saving.set(false);
            window.set_report_dialog_error("Cannot open the report save dialog.".into());
        }
    }
}

fn read_context(path: &Path) -> Result<Vec<u8>, String> {
    let io = |error| format!("Cannot read the context file. {error}");
    let metadata = fs::metadata(path).map_err(io)?;
    if !metadata.is_file() {
        return Err("Choose a regular file for the context.".into());
    }
    let cap = MAX_EXTERNAL_HLS_BYTES as u64;
    let too_large = || format!("Choose a context file with at most {cap} bytes.");
    if metadata.len() > cap {
        return Err(too_large());
    }
    let file = File::open(path).map_err(io)?;
    if !file.metadata().map_err(io)?.is_file() {
        return Err("Choose a regular file for the context.".into());
    }
    let mut bytes = Vec::new();
    file.take(cap + 1).read_to_end(&mut bytes).map_err(io)?;
    if bytes.len() > MAX_EXTERNAL_HLS_BYTES {
        return Err(too_large());
    }
    if bytes.is_empty() {
        return Err("The context file is empty.".into());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Scratch;

    #[test]
    fn context_keeps_exact_bytes_and_rejects_empty_or_large_files()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = Scratch::new("validation-context");
        let path = dir.path("context.obu");
        let bytes = vec![0x9b; MAX_EXTERNAL_HLS_BYTES];
        fs::write(&path, &bytes)?;
        assert_eq!(read_context(&path)?, bytes);
        fs::write(&path, vec![0x9a; MAX_EXTERNAL_HLS_BYTES + 1])?;
        assert!(read_context(&path).is_err());
        fs::write(&path, [])?;
        assert!(read_context(&path).is_err());
        assert!(read_context(&dir.0).is_err());
        Ok(())
    }
}
