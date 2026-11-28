// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Wires the Compare workspace and its file dialog.

use std::rc::Rc;
use std::time::Instant;

use slint::ComponentHandle;

use super::{Controller, with};
use crate::session::{Outcome, Session};
use crate::ui::AppWindow;

impl Controller {
    fn compare(self: &Rc<Self>, act: impl FnOnce(&mut Session) -> Outcome) {
        self.check_visible();
        let outcome = act(&mut self.session.borrow_mut());
        self.handle(outcome);
    }

    pub(super) fn wire_compare(self: &Rc<Self>, window: &AppWindow) {
        let weak = Rc::downgrade(self);
        crate::bind_compare(window, move |input| {
            with(&weak, |ctl| {
                ctl.compare(|session| session.compare_input(input, Instant::now()));
            });
        });
    }

    /// Asks for one AV2 file of the comparison.
    pub(super) fn show_compare_dialog(self: &Rc<Self>, window: &AppWindow) {
        let dialog = rfd::AsyncFileDialog::new()
            .set_title("Choose an AV2 Stream to Compare")
            .set_parent(&window.window().window_handle());
        // A convenience, not a capability claim. See `show_open_dialog` for macOS.
        #[cfg(not(target_os = "macos"))]
        let dialog = dialog
            .add_filter("AV2 streams", &["ivf", "obu"])
            .add_filter("All files", &["*"]);
        let picked = dialog.pick_file();
        let weak = Rc::downgrade(self);
        let spawned = slint::spawn_local(async move {
            let path = picked.await.map(|file| file.path().to_path_buf());
            with(&weak, |ctl| {
                ctl.compare(|session| session.compare_picked(path));
            });
        });
        if spawned.is_err() {
            self.compare(|session| session.compare_picked(None));
        }
    }
}
