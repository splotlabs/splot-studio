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
use studio_app::{
    Analysis, AnalysisState, AppWindow, NavStep, StageState, ViewMode, WorkspaceView,
};

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
    let playback = window.global::<studio_app::Playback>();
    playback.on_compare({
        let log = log.clone();
        move |held| log.borrow_mut().push(format!("compare {held}"))
    });
    playback.on_step({
        let log = log.clone();
        move |delta| log.borrow_mut().push(format!("step {delta}"))
    });
    playback.on_seek({
        let log = log.clone();
        move |frame| log.borrow_mut().push(format!("seek {frame}"))
    });
    playback.on_end(record("end"));
    playback.on_play_pause(record("play"));
    playback.on_toggle_native(record("native"));
    let take = || std::mem::take(&mut *log.borrow_mut());

    // A converted file's result, with stepping and compare offered.
    window.set_view_mode(ViewMode::Single);
    playback.set_state(StageState::Picture);
    playback.set_badge("AV2 result".into());
    playback.set_transport(true);
    playback.set_navigable(true);
    playback.set_can_play(true);
    playback.set_can_compare(true);
    playback.set_can_native(true);
    playback.set_frames(300);
    playback.set_frame_text("43 / 300".into());

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
    playback.set_comparing(true);
    press(&window, Key::Tab);
    assert_eq!(take(), ["compare false"]);
    playback.set_comparing(false);
    // Tab skips the step buttons (the picture's arrows do the same) and
    // lands on Play; Enter plays.
    press(&window, Key::Return);
    assert_eq!(take(), ["play"], "Tab from the picture reaches Play");
    // Then the scrubber: Right steps one frame from the position.
    playback.set_position(42.0);
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
    playback.set_can_compare(false);
    playback.set_navigable(false);
    pointer(&window, x, y, true);
    pointer(&window, x, y, false);
    key_down(&window, "o");
    key_up(&window, "o");
    press(&window, " ");
    press(&window, Key::LeftArrow);
    assert_eq!(take(), [] as [String; 0]);

    an_output_byte_link_moves_keyboard_focus(&window);
}

fn an_output_byte_link_moves_keyboard_focus(window: &AppWindow) {
    window.set_workspace(WorkspaceView::Analyze);
    window.window().set_size(LogicalSize::new(800.0, 680.0));
    let analysis = window.global::<Analysis>();
    analysis.set_state(AnalysisState::Open);
    analysis.set_can_playback(true);
    analysis.set_bytes_shown(false);
    let playback = window.global::<studio_app::Playback>();
    playback.set_state(StageState::Picture);
    playback.set_navigable(true);
    playback.set_output_unit("byte 0x20".into());
    let link = Rc::new(RefCell::new(None));
    playback.on_show_bytes({
        let weak = window.as_weak();
        let link = link.clone();
        move |index| {
            *link.borrow_mut() = Some(index);
            if let Some(window) = weak.upgrade() {
                window.global::<Analysis>().set_bytes_shown(true);
            }
        }
    });
    let cursor_move = Rc::new(RefCell::new(None));
    analysis.on_cursor({
        let cursor_move = cursor_move.clone();
        move |step| *cursor_move.borrow_mut() = Some(step)
    });

    // Start on the picture. Tab to the temporal unit link and open its bytes.
    pointer(window, 400.0, 160.0, true);
    pointer(window, 400.0, 160.0, false);
    for _ in 0..16 {
        press(window, Key::Tab);
        press(window, Key::Return);
        if link.borrow().is_some() {
            break;
        }
    }
    assert_eq!(*link.borrow(), Some(0));
    assert!(analysis.get_bytes_shown());
    press(window, Key::RightArrow);
    assert_eq!(*cursor_move.borrow(), Some(NavStep::Right));
}
