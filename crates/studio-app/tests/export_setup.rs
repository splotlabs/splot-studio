// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The compiled Analyze export form, the real worker, and a writable journal.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Key, Platform, WindowAdapter, WindowEvent};
use slint::{ComponentHandle, LogicalSize, Model, PlatformError};
use studio_app::analysis::{Document, Input as AnalysisInput};
use studio_app::engine::{Delivery, ENGINE_NAME};
use studio_app::export_setup::Draft;
use studio_app::journal::Writer;
use studio_app::preview::{self, Dirty};
use studio_app::session::{Route, Session};
use studio_app::state::{AppState, Intent};
use studio_app::workspace::Workspace;
use studio_app::{AppWindow, DecodeExport, ExportPurpose};
use studio_domain::preview::PictureBox;
use studio_domain::protocol::Event;

const CLIP: &[u8] = include_bytes!(
    "../../studio-engine/tests/preview/fixtures/syn-3frame-padded-filters-422-134x69-10bit-q150.ivf"
);

struct Headless(Rc<MinimalSoftwareWindow>);

impl Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.0.clone())
    }
}

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Rig {
    session: Rc<RefCell<Session>>,
    rx: Receiver<(Route, Delivery)>,
    prepared: bool,
    saved: bool,
}

impl Rig {
    fn wait(&mut self, ready: impl Fn(&Session) -> bool) -> Result<(), Box<dyn std::error::Error>> {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !ready(&self.session.borrow()) {
            let (route, delivery) = self
                .rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))?;
            self.prepared |= matches!(&delivery, Delivery::Event(e) if matches!(e.body, Event::ExportPrepared { .. }));
            self.saved |= matches!(
                (&route, &delivery),
                (Route::ExportSaved(_), Delivery::JournalSaved(Ok(())))
            );
            self.session
                .borrow_mut()
                .deliver(route, delivery, Instant::now());
        }
        Ok(())
    }
}

fn press(window: &AppWindow, key: Key) {
    window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
    window
        .window()
        .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
}

fn wire_window(window: &AppWindow, rig: &Rig, destination: PathBuf) {
    let export = window.global::<DecodeExport>();
    let draft = Rc::new(RefCell::new(None::<Draft>));
    export.on_open({
        let window = window.as_weak();
        let draft = draft.clone();
        let session = rig.session.clone();
        move |purpose| {
            if purpose != ExportPurpose::Frame {
                return;
            }
            let session = session.borrow();
            let pending = Draft::new(
                purpose,
                session.export_sources(),
                session.analysis().shown().map(Document::id),
                session.export_frame_source().as_ref(),
            )
            .unwrap_or_else(|error| unreachable!("{error}"));
            let window = window
                .upgrade()
                .unwrap_or_else(|| unreachable!("The window closed."));
            pending.render(&window);
            *draft.borrow_mut() = Some(pending);
            window.invoke_show_export_setup();
        }
    });
    export.on_cancel({
        let window = window.as_weak();
        move || {
            if let Some(window) = window.upgrade() {
                window.invoke_close_export_setup();
            }
        }
    });
    export.on_choose_destination({
        let window = window.as_weak();
        let session = rig.session.clone();
        move |form| {
            let pending = draft
                .borrow_mut()
                .take()
                .unwrap_or_else(|| unreachable!("The form did not open."));
            // Supply a real destination without opening the OS picker in the headless test.
            let plan = pending
                .plan(&form)
                .unwrap_or_else(|error| unreachable!("{error}"));
            let mut session = session.borrow_mut();
            session.export_start(plan.requests(&destination), Instant::now());
            assert_eq!(session.export_notice(), None);
            assert!(session.export_active());
            let window = window
                .upgrade()
                .unwrap_or_else(|| unreachable!("The window closed."));
            studio_app::render_jobs(&window, &session.jobs());
            window.invoke_close_export_setup();
        }
    });
}

fn open_frame(source: PathBuf, journal_path: PathBuf) -> Result<Rig, Box<dyn std::error::Error>> {
    let engine = PathBuf::from(env!("CARGO_BIN_EXE_splot-studio"))
        .with_file_name(format!("{ENGINE_NAME}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        engine.is_file(),
        "Build studio-engine in the same profile first."
    );
    let journal = Writer::spawn(journal_path, |_| {})?;
    let (tx, rx) = mpsc::channel();
    let session = Session::new(
        AppState::default(),
        Ok(engine),
        Arc::new(move |route, delivery| {
            let _ = tx.send((route, delivery));
        }),
        Some(journal),
    );
    let mut rig = Rig {
        session: Rc::new(RefCell::new(session)),
        rx,
        prepared: false,
        saved: false,
    };
    {
        let mut session = rig.session.borrow_mut();
        session.dispatch(
            Intent::WorkspaceRequested(Workspace::Analyze),
            Instant::now(),
        );
        session.analysis_input(AnalysisInput::Open(vec![source]));
        session.preview_input(preview::Input::Resized {
            stage: PictureBox::clamped(716, 240),
            thumb: PictureBox::clamped(64, 36),
        });
    }
    rig.wait(|session| {
        session
            .analysis()
            .shown()
            .and_then(Document::summary)
            .is_some()
    })?;
    rig.session
        .borrow_mut()
        .analysis_input(AnalysisInput::TogglePlayback);
    rig.wait(|session| session.export_frame_source().is_some())?;

    Ok(rig)
}

#[test]
fn a_keyboard_selected_frame_exports_through_the_journal_and_worker()
-> Result<(), Box<dyn std::error::Error>> {
    let scratch =
        Scratch(std::env::temp_dir().join(format!("studio-export-ui-{}", std::process::id())));
    std::fs::create_dir(&scratch.0)?;
    let source = scratch.0.join("decoded samples.ivf");
    std::fs::write(&source, CLIP)?;
    let journal_path = scratch.0.join("queue.json");
    let mut rig = open_frame(source.clone(), journal_path.clone())?;

    let platform = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Headless(platform)))?;
    let window = studio_app::new_window()?;
    window.window().set_size(LogicalSize::new(800.0, 680.0));
    window.show()?;
    {
        let session = rig.session.borrow();
        studio_app::render(&window, session.state(), Instant::now());
        studio_app::render_analysis(&window, session.analysis());
        studio_app::render_preview(
            &window,
            session.state(),
            session.preview(),
            Dirty {
                picture: true,
                view: true,
                thumbs: false,
            },
        );
    }
    let export = window.global::<DecodeExport>();
    export.set_can_frame(true);
    export.set_can_start(true);
    let destination = scratch.0.join("output.yuv");
    wire_window(&window, &rig, destination.clone());
    for _ in 0..24 {
        press(&window, Key::Tab);
        press(&window, Key::Return);
        if window.get_export_open() {
            break;
        }
    }
    assert!(window.get_export_open());
    assert_eq!(export.get_purpose(), ExportPurpose::Frame);
    assert!(export.get_frame_details().contains("10-bit"));
    press(&window, Key::Tab);
    press(&window, Key::Return);
    assert!(!window.get_export_open());
    assert!(window.get_jobs().iter().any(|job| job.kind == "Export raw"));
    assert!(!destination.exists());

    rig.wait(|session| !session.export_active())?;
    assert!(rig.prepared && rig.saved);
    studio_app::render_jobs(&window, &rig.session.borrow().jobs());
    let row = window
        .get_jobs()
        .iter()
        .find(|job| job.kind == "Export raw")
        .unwrap_or_else(|| unreachable!("No export row."));
    assert_eq!(row.state, "Completed");
    assert_eq!(row.detail.lines().next(), Some("Exported 1 frame."));
    assert!(row.detail.contains("output.yuv.layout.json"));
    assert_eq!(std::fs::metadata(destination)?.len(), 36_984);
    let layout = std::fs::read(scratch.0.join("output.yuv.layout.json"))?;
    let layout = serde_json::from_slice::<serde_json::Value>(&layout)?;
    assert_eq!(layout["frames"], 1);
    assert_eq!(layout["outputs"][0]["ordinal"], 0);
    assert_eq!(layout["frame_bytes"], 36_984);
    assert_eq!(layout["sample_format"]["bit_depth"], 10);
    assert_eq!(layout["endianness"], "little");
    assert_eq!(std::fs::read(source)?, CLIP);
    assert!(std::fs::read_to_string(journal_path)?.contains("output.yuv"));
    rig.session.borrow_mut().shutdown();
    Ok(())
}
