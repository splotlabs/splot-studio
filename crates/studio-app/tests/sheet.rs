// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The video-only confirmation with real keys, in the real compiled window.
//!
//! Runs headless on Slint's software renderer, so it needs no display: one
//! test function, because a process has one Slint platform.

use std::cell::Cell;
use std::rc::Rc;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Key, Platform, PointerEventButton, WindowAdapter, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, LogicalSize, PlatformError, SharedString};
use studio_app::AppWindow;

struct Headless(Rc<MinimalSoftwareWindow>);

impl Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.0.clone())
    }
}

fn press(window: &AppWindow, key: impl Into<SharedString>) {
    let text = key.into();
    window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    window
        .window()
        .dispatch_event(WindowEvent::KeyReleased { text });
}

fn shift_tab(window: &AppWindow) {
    window.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Shift.into(),
    });
    press(window, Key::Tab);
    window.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Shift.into(),
    });
}

fn click(window: &AppWindow, x: f32, y: f32) {
    let position = LogicalPosition::new(x, y);
    window
        .window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    for event in [
        WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        },
        WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        },
    ] {
        window.window().dispatch_event(event);
    }
}

#[test]
fn the_confirmation_defaults_to_cancel_and_keeps_the_keyboard() {
    let platform = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Headless(platform.clone())))
        .unwrap_or_else(|e| unreachable!("{e}"));
    let window = studio_app::new_window().unwrap_or_else(|e| unreachable!("{e}"));
    window.window().set_size(LogicalSize::new(464.0, 640.0));
    window.show().unwrap_or_else(|e| unreachable!("{e}"));

    let confirmed = Rc::new(Cell::new(0));
    let declined = Rc::new(Cell::new(0));
    let opened = Rc::new(Cell::new(0));
    window.on_omissions_confirmed({
        let n = confirmed.clone();
        move || n.set(n.get() + 1)
    });
    window.on_omissions_declined({
        let n = declined.clone();
        move || n.set(n.get() + 1)
    });
    window.on_open_requested({
        let n = opened.clone();
        move || n.set(n.get() + 1)
    });
    let counts = || (confirmed.get(), declined.get());

    window.set_sheet_body("This file also has 1 audio track.".into());
    window.invoke_show_sheet();
    assert!(window.get_sheet_open());

    // Cancel has the focus: Enter and Space cancel, never convert.
    press(&window, Key::Return);
    assert_eq!(counts(), (0, 1), "Enter must activate Cancel");
    press(&window, " ");
    assert_eq!(counts(), (0, 2), "Space must activate Cancel");
    // Escape is Cancel.
    press(&window, Key::Escape);
    assert_eq!(counts(), (0, 3), "Escape must cancel");
    // Converting takes a deliberate Tab to it.
    press(&window, Key::Tab);
    press(&window, Key::Return);
    assert_eq!(counts(), (1, 3), "Tab then Enter converts");
    // Tab stays inside: the next stop is Cancel again, not the window.
    press(&window, Key::Tab);
    press(&window, Key::Return);
    assert_eq!(counts(), (1, 4), "Tab must wrap to Cancel inside the sheet");
    shift_tab(&window);
    press(&window, " ");
    assert_eq!(counts(), (2, 4), "Shift+Tab reaches Convert Video Only");
    // Clicks outside the card reach nothing behind it (the empty state's
    // Open zone is here).
    click(&window, 232.0, 160.0);
    click(&window, 20.0, 620.0);
    assert_eq!(opened.get(), 0, "the window behind the sheet got a click");
    assert_eq!(counts(), (2, 4));

    window.invoke_close_sheet();
    assert!(!window.get_sheet_open());
    press(&window, Key::Return);
    assert_eq!(counts(), (2, 4), "a closed sheet answers nothing");

    // Opened again, Cancel has the focus again.
    window.invoke_show_sheet();
    press(&window, Key::Return);
    assert_eq!(counts(), (2, 5));
    window.invoke_close_sheet();
    // Behind it, the empty state's Open zone takes clicks again.
    click(&window, 232.0, 160.0);
    assert_eq!(
        opened.get(),
        1,
        "the Open zone must work once the sheet is closed"
    );
}
