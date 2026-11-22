// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The workspace shell in the real compiled window: it shows Encode as the
//! app state says, and its reserved Jobs entry is no Tab stop, so the
//! keyboard reaches the Encode view first.
//!
//! Runs headless on Slint's software renderer, so it needs no display: one
//! test function, because a process has one Slint platform.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Instant;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Key, Platform, WindowAdapter, WindowEvent};
use slint::{ComponentHandle, LogicalSize, PlatformError, SharedString};
use studio_app::state::AppState;

struct Headless(Rc<MinimalSoftwareWindow>);

impl Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.0.clone())
    }
}

fn press(window: &studio_app::AppWindow, key: impl Into<SharedString>) {
    let text = key.into();
    window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    window
        .window()
        .dispatch_event(WindowEvent::KeyReleased { text });
}

#[test]
fn the_shell_shows_encode_and_jobs_is_no_tab_stop() {
    let platform = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Headless(platform.clone())))
        .unwrap_or_else(|e| unreachable!("{e}"));
    let window = studio_app::new_window().unwrap_or_else(|e| unreachable!("{e}"));
    window.window().set_size(LogicalSize::new(464.0, 640.0));
    window.show().unwrap_or_else(|e| unreachable!("{e}"));
    // Rust owns both: the title has no default, and hiding Encode first
    // shows that render sets it back.
    assert_eq!(window.get_workspace_title(), "");
    window.set_encode_shown(false);
    studio_app::render(&window, &AppState::default(), Instant::now());
    assert!(window.get_encode_shown());
    assert_eq!(window.get_workspace_title(), "Encode");

    let opened = Rc::new(Cell::new(0));
    window.on_open_requested({
        let n = opened.clone();
        move || n.set(n.get() + 1)
    });

    // The first Tab stop is the empty state's Open…, not the toolbar.
    press(&window, Key::Tab);
    press(&window, " ");
    assert_eq!(opened.get(), 1, "Tab did not reach Open… first");
}
