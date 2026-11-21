// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The picture area with real keys and pointer events, in the real compiled
//! window: press and hold, the O key, Space, the arrows, Home and End, and
//! where Tab goes from it.
//!
//! Runs headless on Slint's software renderer, so it needs no display: one
//! test function, because a process has one Slint platform.

use std::cell::RefCell;
use std::rc::Rc;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Key, Platform, PointerEventButton, WindowAdapter, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, LogicalSize, PlatformError, SharedString};
use studio_app::{AppWindow, StageState, ViewMode};

struct Headless(Rc<MinimalSoftwareWindow>);

impl Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.0.clone())
    }
}

fn key_down(window: &AppWindow, key: impl Into<SharedString>) {
    window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
}

fn key_up(window: &AppWindow, key: impl Into<SharedString>) {
    window
        .window()
        .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
}

fn press(window: &AppWindow, key: impl Into<SharedString>) {
    let text = key.into();
    key_down(window, text.clone());
    key_up(window, text);
}

fn pointer(window: &AppWindow, x: f32, y: f32, down: bool) {
    let position = LogicalPosition::new(x, y);
    window
        .window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    window.window().dispatch_event(if down {
        WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        }
    } else {
        WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        }
    });
}

#[test]
fn the_picture_area_answers_hold_keys_and_tab() {
    let platform = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Headless(platform.clone())))
        .unwrap_or_else(|e| unreachable!("{e}"));
    let window = studio_app::new_window().unwrap_or_else(|e| unreachable!("{e}"));
    window.window().set_size(LogicalSize::new(464.0, 680.0));
    window.show().unwrap_or_else(|e| unreachable!("{e}"));

    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let record = |what: &'static str| {
        let log = log.clone();
        move || log.borrow_mut().push(what.to_owned())
    };
    window.on_stage_compare({
        let log = log.clone();
        move |held| log.borrow_mut().push(format!("compare {held}"))
    });
    window.on_stage_step({
        let log = log.clone();
        move |delta| log.borrow_mut().push(format!("step {delta}"))
    });
    window.on_stage_seek({
        let log = log.clone();
        move |frame| log.borrow_mut().push(format!("seek {frame}"))
    });
    window.on_stage_end(record("end"));
    window.on_stage_play_pause(record("play"));
    window.on_stage_toggle_native(record("native"));
    let take = || std::mem::take(&mut *log.borrow_mut());

    // A converted file's result, with stepping and compare offered.
    window.set_view_mode(ViewMode::Single);
    window.set_stage_state(StageState::Picture);
    window.set_stage_badge("AV2 result".into());
    window.set_stage_transport(true);
    window.set_stage_navigable(true);
    window.set_stage_can_play(true);
    window.set_stage_can_compare(true);
    window.set_stage_can_native(true);
    window.set_stage_frames(300);
    window.set_stage_frame_text("43 / 300".into());

    // The picture is the first thing in the card (the in-window menu bar,
    // when there is one, sits above it); 150 px down is inside it either way.
    let (x, y) = (232.0, 150.0);
    pointer(&window, x, y, true);
    assert_eq!(
        take(),
        ["compare true"],
        "press and hold shows the original"
    );
    pointer(&window, x, y, false);
    assert_eq!(take(), ["compare false"], "release returns");

    // The click focused the picture: its keys work.
    key_down(&window, "o");
    assert_eq!(take(), ["compare true"], "holding O shows the original");
    key_up(&window, "o");
    assert_eq!(take(), ["compare false"]);
    press(&window, " ");
    press(&window, Key::LeftArrow);
    press(&window, Key::RightArrow);
    press(&window, Key::Home);
    press(&window, Key::End);
    assert_eq!(take(), ["play", "step -1", "step 1", "seek 0", "end"]);

    // Losing focus while O is held lets go of the original.
    window.set_stage_comparing(true);
    press(&window, Key::Tab);
    assert_eq!(take(), ["compare false"]);
    window.set_stage_comparing(false);
    // Tab skips the step buttons (the picture's arrows do the same) and
    // lands on Play; Enter plays.
    press(&window, Key::Return);
    assert_eq!(take(), ["play"], "Tab from the picture reaches Play");
    // Then the scrubber: Right steps one frame from the position.
    window.set_stage_position(42.0);
    press(&window, Key::Tab);
    press(&window, Key::RightArrow);
    press(&window, Key::PageDown);
    assert_eq!(take(), ["seek 43", "seek 72"]);
    // Then actual pixels.
    press(&window, Key::Tab);
    press(&window, " ");
    assert_eq!(take(), ["native"]);

    // Nothing to compare or step (a source still loading): keys and holds
    // do nothing.
    window.set_stage_can_compare(false);
    window.set_stage_navigable(false);
    pointer(&window, x, y, true);
    pointer(&window, x, y, false);
    key_down(&window, "o");
    key_up(&window, "o");
    press(&window, " ");
    press(&window, Key::LeftArrow);
    assert_eq!(take(), [] as [String; 0]);
}
