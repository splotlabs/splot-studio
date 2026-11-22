// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The workspace shell in the real compiled window: the switch is the first
//! Tab stop and chooses with the arrow keys, the hidden workspace takes no
//! keyboard focus, and a round trip through Analyze leaves Encode exactly
//! as it was, scrolled file list included.
//!
//! Runs headless on Slint's software renderer, so it needs no display: one
//! test function, because a process has one Slint platform.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Key, Platform, WindowAdapter, WindowEvent};
use slint::{
    ComponentHandle, LogicalPosition, PhysicalSize, PlatformError, Rgb8Pixel, SharedString,
};
use studio_app::analysis::Analysis;
use studio_app::recovery::{Disk, Kind};
use studio_app::state::{AppState, Effect, Intent};
use studio_app::workspace::Workspace;
use studio_app::{AppWindow, WorkspaceView};
use studio_domain::job::{
    ChromaLayout, Omissions, Rational, SourceFacts, SourceFingerprint, SourceReader,
};

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

struct NoFiles;

impl Disk for NoFiles {
    fn kind(&self, _: &Path) -> Kind {
        Kind::Missing
    }
}

fn facts() -> SourceFacts {
    SourceFacts {
        reader: SourceReader::Y4m,
        fingerprint: SourceFingerprint {
            len: 1000,
            modified: None,
        },
        video_stream: 0,
        video_tracks: Vec::new(),
        omissions: Omissions::default(),
        width: 64,
        height: 48,
        frame_rate: Rational { num: 25, den: 1 },
        pixel_aspect: None,
        bit_depth: 8,
        chroma: ChromaLayout::Yuv420,
        frame_count: Some(10),
        disclosures: Vec::new(),
    }
}

/// Eight probed files: the list shows three and scrolls the rest.
fn eight_files() -> AppState {
    let mut state = AppState::default();
    let now = Instant::now();
    let paths: Vec<PathBuf> = (0..8)
        .map(|i| PathBuf::from(format!("/media/clip {i}.y4m")))
        .collect();
    let mut effects = state
        .apply(Intent::FilesOpened(paths), now, &NoFiles)
        .effects;
    while let Some(Effect::Probe { request, .. }) = effects.pop() {
        effects.extend(
            state
                .apply(
                    Intent::ProbeAnswered {
                        id: request.id,
                        result: Ok(facts()),
                    },
                    now,
                    &NoFiles,
                )
                .effects,
        );
    }
    state
}

const SIZE: (u32, u32) = (900, 680);

/// Renders the window into a fresh buffer.
fn pixels(platform: &MinimalSoftwareWindow, window: &AppWindow) -> Vec<Rgb8Pixel> {
    window.window().request_redraw();
    let mut buffer = vec![Rgb8Pixel::default(); (SIZE.0 * SIZE.1) as usize];
    platform.draw_if_needed(|renderer| {
        renderer.render(&mut buffer, SIZE.0 as usize);
    });
    buffer
}

fn workspace(state: &mut AppState, workspace: Workspace) {
    state.apply(
        Intent::WorkspaceRequested(workspace),
        Instant::now(),
        &NoFiles,
    );
}

#[test]
fn the_switch_leads_the_keys_and_a_round_trip_keeps_encode() {
    let platform = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Headless(platform.clone())))
        .unwrap_or_else(|e| unreachable!("{e}"));
    let window = studio_app::new_window().unwrap_or_else(|e| unreachable!("{e}"));
    window.window().set_size(PhysicalSize::new(SIZE.0, SIZE.1));
    window.show().unwrap_or_else(|e| unreachable!("{e}"));
    // Rust owns the title (no default); render sets it.
    assert_eq!(window.get_workspace_title(), "");
    let mut state = AppState::default();
    studio_app::render(&window, &state, Instant::now());
    assert_eq!(window.get_workspace(), WorkspaceView::Encode);
    assert_eq!(window.get_workspace_title(), "Encode");

    let chosen = Rc::new(Cell::new(-1));
    window.on_workspace_chosen({
        let chosen = chosen.clone();
        move |i| chosen.set(i)
    });
    let opened = Rc::new(Cell::new(0));
    window.on_open_requested({
        let n = opened.clone();
        move || n.set(n.get() + 1)
    });
    let analyze_opened = Rc::new(Cell::new(0));
    window.global::<studio_app::Analysis>().on_open({
        let n = analyze_opened.clone();
        move || n.set(n.get() + 1)
    });

    // The first Tab stop is the switch; Right asks for Analyze.
    press(&window, Key::Tab);
    press(&window, Key::RightArrow);
    assert_eq!(
        chosen.get(),
        1,
        "Right on the switch did not choose Analyze"
    );
    // Encode is still shown until Rust says otherwise: the next stop is
    // Encode's Open….
    press(&window, Key::Tab);
    press(&window, " ");
    assert_eq!(opened.get(), 1, "Tab did not reach Encode's Open… next");

    // With Analyze shown, Encode's controls take no focus: the stop after
    // the switch is Analyze's Open….
    workspace(&mut state, Workspace::Analyze);
    studio_app::render(&window, &state, Instant::now());
    studio_app::render_analysis(&window, &Analysis::new());
    assert_eq!(window.get_workspace(), WorkspaceView::Analyze);
    assert_eq!(window.get_workspace_index(), 1);
    press(&window, Key::Tab);
    press(&window, Key::Tab);
    press(&window, " ");
    assert_eq!(
        (opened.get(), analyze_opened.get()),
        (1, 1),
        "the hidden Encode took a key"
    );

    // Encode with a scrolled file list, then Analyze and back: the same
    // pixels, so its scroll position and view state survived.
    let mut state = eight_files();
    studio_app::render(&window, &state, Instant::now());
    let top = pixels(&platform, &window);
    let scroll = WindowEvent::PointerScrolled {
        position: LogicalPosition::new(150.0, 90.0),
        delta_x: 0.0,
        delta_y: -400.0,
    };
    window.window().dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(150.0, 90.0),
    });
    window.window().dispatch_event(scroll);
    // The pointer leaves, so no row shows a hover it would lose anyway.
    window.window().dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(890.0, 670.0),
    });
    let scrolled = pixels(&platform, &window);
    assert!(top != scrolled, "the file list did not scroll");
    workspace(&mut state, Workspace::Analyze);
    studio_app::render(&window, &state, Instant::now());
    let analyze = pixels(&platform, &window);
    assert!(analyze != scrolled, "Analyze is not shown");
    workspace(&mut state, Workspace::Encode);
    studio_app::render(&window, &state, Instant::now());
    let back = pixels(&platform, &window);
    assert!(back == scrolled, "Encode changed across a workspace switch");
}
