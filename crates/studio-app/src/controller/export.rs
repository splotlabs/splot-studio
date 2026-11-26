// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The decoded export form and its native destination picker.

use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

use slint::ComponentHandle;

use crate::analysis::Document;
use crate::export_setup::{Draft, Plan};
use crate::session::Session;
use crate::ui::{AppWindow, DecodeExport, ExportForm, ExportPurpose};

use super::{Controller, with};

impl Controller {
    pub(super) fn wire_export(self: &Rc<Self>, window: &AppWindow) {
        let export = window.global::<DecodeExport>();
        export.on_open({
            let ctl = Rc::downgrade(self);
            move |purpose| with(&ctl, |ctl| ctl.open_export_setup(purpose))
        });
        export.on_cancel({
            let ctl = Rc::downgrade(self);
            move || with(&ctl, |ctl| ctl.close_export_setup())
        });
        export.on_choose_destination({
            let ctl = Rc::downgrade(self);
            move |form| with(&ctl, |ctl| ctl.choose_export_destination(&form))
        });
        export.on_source_changed({
            let ctl = Rc::downgrade(self);
            move |index, selected| {
                with(&ctl, |ctl| {
                    ctl.edit_export(|draft| draft.select_source(index, selected))
                });
            }
        });
        export.on_depth_changed({
            let ctl = Rc::downgrade(self);
            move |index, depth| {
                with(&ctl, |ctl| {
                    ctl.edit_export(|draft| draft.choose_depth(index, depth))
                });
            }
        });
        export.on_chroma_changed({
            let ctl = Rc::downgrade(self);
            move |index, chroma| {
                with(&ctl, |ctl| {
                    ctl.edit_export(|draft| draft.choose_chroma(index, chroma))
                });
            }
        });
        export.on_cancel_batch({
            let ctl = Rc::downgrade(self);
            move || {
                with(&ctl, |ctl| {
                    let outcome = ctl
                        .session
                        .borrow_mut()
                        .export_cancel_remaining(Instant::now());
                    ctl.handle(outcome);
                });
            }
        });
    }

    fn open_export_setup(&self, purpose: ExportPurpose) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let session = self.session.borrow();
        if window.get_export_open()
            || window.get_sheet_open()
            || window.get_validation_open()
            || session.export_active()
        {
            return;
        }
        let draft = Draft::new(
            purpose,
            session.export_sources(),
            session.analysis().shown().map(Document::id),
            session.export_frame_source(),
        );
        drop(session);
        let export = window.global::<DecodeExport>();
        match draft {
            Ok(draft) => {
                draft.render(&window);
                *self.export_draft.borrow_mut() = Some(draft);
                export.set_notice("".into());
                export.set_busy(false);
                window.invoke_show_export_setup();
            }
            Err(error) => {
                export.set_job_notice(error.into());
                window.invoke_show_jobs();
            }
        }
    }

    fn close_export_setup(&self) {
        self.export_draft.borrow_mut().take();
        if let Some(window) = self.window.upgrade() {
            window.invoke_close_export_setup();
        }
    }

    fn edit_export(&self, edit: impl FnOnce(&mut Draft)) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let export = window.global::<DecodeExport>();
        if export.get_busy() {
            return;
        }
        if let Some(draft) = self.export_draft.borrow_mut().as_mut() {
            edit(draft);
            draft.render_sources(&export);
        }
    }

    fn choose_export_destination(self: &Rc<Self>, form: &ExportForm) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let export = window.global::<DecodeExport>();
        if export.get_busy() {
            return;
        }
        let (ticket, planned) = match self.export_draft.borrow().as_ref() {
            Some(draft) => (draft.ticket.clone(), draft.plan(form)),
            None => return,
        };
        let plan = match planned {
            Ok(plan) => plan,
            Err(error) => {
                export.set_notice(error.into());
                return;
            }
        };
        export.set_notice("".into());
        export.set_busy(true);

        let batch = plan.is_batch();
        let mut dialog = rfd::AsyncFileDialog::new()
            .set_parent(&window.window().window_handle())
            .set_title(if batch {
                "Choose Export Folder"
            } else {
                "Save Decoded Export"
            });
        if let Some((parent, name)) = plan.suggested_file() {
            dialog = dialog.set_directory(parent);
            if !batch {
                dialog = dialog.set_file_name(name.to_string_lossy());
            }
        }
        if !batch {
            dialog = dialog.add_filter(plan.format_label(), &[plan.extension()]);
        }
        let weak = Rc::downgrade(self);
        if slint::spawn_local(async move {
            let file = if batch {
                dialog.pick_folder().await
            } else {
                dialog.save_file().await
            };
            let chosen = file.map(|file| file.path().to_path_buf());
            with(&weak, |ctl| {
                ctl.export_destination_picked(&ticket, plan, chosen)
            });
        })
        .is_err()
        {
            export.set_busy(false);
            export.set_notice("Cannot open the export destination dialog.".into());
        }
    }

    fn export_destination_picked(
        self: &Rc<Self>,
        ticket: &Rc<()>,
        plan: Plan,
        chosen: Option<PathBuf>,
    ) {
        let current = self
            .export_draft
            .borrow()
            .as_ref()
            .is_some_and(|draft| Rc::ptr_eq(&draft.ticket, ticket));
        let Some(window) = self.window.upgrade().filter(|_| current) else {
            return;
        };
        let export = window.global::<DecodeExport>();
        export.set_busy(false);
        let Some(path) = chosen else {
            return;
        };

        let outcome = self
            .session
            .borrow_mut()
            .export_start(plan.requests(&path), Instant::now());
        let notice = self.session.borrow().export_notice().map(str::to_owned);
        self.handle(outcome);
        if let Some(notice) = notice {
            export.set_notice(notice.into());
        } else {
            self.close_export_setup();
            window.invoke_show_jobs();
        }
    }
}

pub(super) fn render_export_status(window: &AppWindow, session: &Session) {
    let export = window.global::<DecodeExport>();
    let active = session.export_active();
    let ready = session
        .analysis()
        .shown()
        .and_then(Document::summary)
        .is_some_and(|summary| {
            summary
                .digest
                .as_ref()
                .is_some_and(|digest| digest.bytes > 0 && digest.bytes == summary.fingerprint.len)
        });
    export.set_active(active);
    export.set_can_start(!active && ready);
    export.set_can_frame(!active && ready && session.export_frame_source().is_some());
    export.set_job_notice(session.export_notice().unwrap_or_default().into());
}
