// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The Slint-thread side of the app: window callbacks become intents for
//! the [`Session`], its outcomes become redraws, dialogs and timers.
//! Worker events and journal results reach this thread through
//! [`slint::invoke_from_event_loop`].

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, SharedString, Timer, TimerMode};
use studio_domain::protocol::BuildIdentity;
use studio_domain::settings::OutputKind;

use crate::activity::{self, Changed};
use crate::engine::{Delivery, EngineStatus};
use crate::render::{render, render_counters};
use crate::session::{Outcome, Post, Route, Session, UiEffect};
use crate::state::Intent;
use crate::ui::AppWindow;
use crate::{APP_NAME, present, reveal};

/// The window's controller.
pub struct Controller {
    window: slint::Weak<AppWindow>,
    session: RefCell<Session>,
    /// Ticks the elapsed time while a file converts; stopped otherwise.
    clock: Timer,
    /// Fires once [`activity::CANCEL_TIMEOUT`] after Cancel.
    cancel_deadline: Timer,
}

thread_local! {
    /// Worker events arrive through `invoke_from_event_loop`, whose closures
    /// must be `Send`; they find the (non-`Send`) controller here, on the
    /// Slint thread.
    static CONTROLLER: RefCell<Weak<Controller>> = const { RefCell::new(Weak::new()) };
}

/// How worker deliveries reach the Slint thread.
#[must_use]
pub fn post() -> Post {
    Arc::new(|route, delivery| {
        // Fails only once the event loop is gone and nobody listens.
        let _ = slint::invoke_from_event_loop(move || deliver(route, delivery));
    })
}

/// How journal results reach the Slint thread.
pub fn journal_saved(result: Result<(), String>) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ctl) = current() {
            ctl.dispatch(Intent::JournalSaved(result));
        }
    });
}

fn current() -> Option<Rc<Controller>> {
    CONTROLLER.with(|c| c.borrow().upgrade())
}

fn deliver(route: Route, delivery: Delivery) {
    if let Some(ctl) = current() {
        let outcome = ctl
            .session
            .borrow_mut()
            .deliver(route, delivery, Instant::now());
        ctl.handle(outcome);
    }
}

fn with(ctl: &Weak<Controller>, f: impl FnOnce(&Rc<Controller>)) {
    if let Some(ctl) = ctl.upgrade() {
        f(&ctl);
    }
}

/// How long after a size change the fitted size is asserted once more.
const FIT_SETTLE: Duration = Duration::from_millis(300);

/// Sizes the window to its content (the window is the card).
fn fit(window: &slint::Weak<AppWindow>) {
    if let Some(window) = window.upgrade() {
        let size = slint::LogicalSize::new(window.get_fitted_width(), window.get_fitted_height());
        if size.width > 0.0 && size.height > 0.0 {
            window.window().set_size(size);
        }
    }
}

impl Controller {
    /// Wires every callback of `window` to `session`, draws it and starts
    /// the engine handshake. `files` are added as if picked.
    pub fn start(window: &AppWindow, session: Session, files: Vec<PathBuf>) -> Rc<Self> {
        let ctl = Rc::new(Self {
            window: window.as_weak(),
            session: RefCell::new(session),
            clock: Timer::default(),
            cancel_deadline: Timer::default(),
        });
        CONTROLLER.with(|c| *c.borrow_mut() = Rc::downgrade(&ctl));
        render(window, ctl.session.borrow().state(), Instant::now());

        let on = |intent: fn(i32) -> Intent| {
            let ctl = Rc::downgrade(&ctl);
            move |value| with(&ctl, |ctl| ctl.dispatch(intent(value)))
        };
        window.on_quality_selected(on(Intent::QualitySelected));
        window.on_priority_selected(on(Intent::PrioritySelected));
        window.on_output_selected(on(Intent::OutputSelected));
        window.on_track_selected(on(Intent::VideoTrackSelected));
        window.on_preset_edited(on(Intent::PresetEdited));
        window.on_crf_edited(on(Intent::CrfEdited));
        window.on_row_selected(on(Intent::RowSelected));
        window.on_row_action(on(Intent::RowAction));
        window.on_remove_requested(on(Intent::RemoveRequested));
        let on = |intent: fn() -> Intent| {
            let ctl = Rc::downgrade(&ctl);
            move || with(&ctl, |ctl| ctl.dispatch(intent()))
        };
        window.on_open_requested(on(|| Intent::AddRequested));
        window.on_change_requested(on(|| Intent::ReplaceRequested));
        window.on_choose_destination_requested(on(|| Intent::ChooseDestinationRequested));
        window.on_encode_requested(on(|| Intent::ConvertRequested));
        window.on_omissions_confirmed(on(|| Intent::OmissionsConfirmed));
        window.on_omissions_declined(on(|| Intent::OmissionsDeclined));
        window.on_reset_requested(on(|| Intent::ResetRequested));
        window.on_cancel_requested(on(|| Intent::CancelRequested));
        window.on_clear_finished(on(|| Intent::ClearFinished));
        window.on_reveal_requested({
            let ctl = Rc::downgrade(&ctl);
            move || {
                with(&ctl, |ctl| {
                    let path = ctl
                        .session
                        .borrow()
                        .state()
                        .focus()
                        .filter(|i| i.state() == crate::queue::ItemState::Completed)
                        .map(|i| i.destination().path.clone());
                    if let Some(path) = path {
                        ctl.reveal(path);
                    }
                });
            }
        });
        window.on_fit_changed({
            let weak = window.as_weak();
            move || {
                fit(&weak);
                // Again once the compositor has answered: on GNOME Wayland a late
                // configure for the previous size can otherwise win.
                let weak = weak.clone();
                Timer::single_shot(FIT_SETTLE, move || fit(&weak));
            }
        });
        window.on_quit_requested(|| {
            // Only fails when no event loop is running, so there is nothing to quit.
            let _ = slint::quit_event_loop();
        });

        let client = BuildIdentity {
            name: APP_NAME.to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        };
        let outcome = ctl.session.borrow_mut().start_hello(client, Instant::now());
        ctl.handle(outcome);
        if !files.is_empty() {
            ctl.dispatch(Intent::FilesOpened(files));
        }
        ctl
    }

    /// Applies an intent and handles what it leads to.
    pub fn dispatch(self: &Rc<Self>, intent: Intent) {
        let outcome = self.session.borrow_mut().dispatch(intent, Instant::now());
        self.handle(outcome);
    }

    fn handle(self: &Rc<Self>, outcome: Outcome) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        for effect in outcome.ui {
            match effect {
                UiEffect::ShowOpenDialog => self.show_open_dialog(&window),
                UiEffect::ShowSaveDialog {
                    directory,
                    file_name,
                    output,
                } => self.show_save_dialog(&window, directory, &file_name, output),
                UiEffect::CancelTimer(id) => {
                    let weak = Rc::downgrade(self);
                    self.cancel_deadline.start(
                        TimerMode::SingleShot,
                        activity::CANCEL_TIMEOUT,
                        move || {
                            with(&weak, |ctl| {
                                ctl.dispatch(Intent::Job {
                                    id,
                                    input: activity::Input::CancelTimedOut,
                                });
                            });
                        },
                    );
                }
                UiEffect::Reveal(path) => self.reveal(path),
                UiEffect::Engine(status) => show_engine(&window, &status),
            }
        }
        let session = self.session.borrow();
        let state = session.state();
        if state.active().is_some() {
            if !self.clock.running() {
                let weak = Rc::downgrade(self);
                self.clock
                    .start(TimerMode::Repeated, Duration::from_secs(1), move || {
                        with(&weak, Controller::tick);
                    });
            }
        } else {
            self.clock.stop();
            self.cancel_deadline.stop();
        }
        let now = Instant::now();
        match outcome.changed {
            Changed::Nothing => {}
            Changed::Counters => render_counters(&window, state, now),
            Changed::Phase => render(&window, state, now),
        }
    }

    fn tick(self: &Rc<Self>) {
        if let Some(window) = self.window.upgrade() {
            let session = self.session.borrow();
            if let Some(activity) = session
                .state()
                .active()
                .and_then(crate::queue::Item::activity)
            {
                window.set_elapsed_text(present::elapsed(activity.elapsed(Instant::now())).into());
            }
        }
    }

    /// Opens a converted file's folder off the Slint thread; a failure is
    /// shown under the result.
    fn reveal(self: &Rc<Self>, path: PathBuf) {
        let weak = self.window.clone();
        if let Some(window) = weak.upgrade() {
            window.set_reveal_error(SharedString::new());
        }
        let spawned = std::thread::Builder::new()
            .name("show-in-folder".to_owned())
            .spawn(move || {
                if let Err(e) = reveal::show_in_folder(&path) {
                    let text = format!("Could not show the file in its folder: {e}");
                    let _ = weak.upgrade_in_event_loop(move |w| w.set_reveal_error(text.into()));
                }
            });
        if let (Err(e), Some(window)) = (spawned, self.window.upgrade()) {
            window.set_reveal_error(format!("Could not show the file in its folder: {e}").into());
        }
    }

    /// Shows the native Open dialog without blocking the event loop: rfd's
    /// future completes on its own thread (portal) or the native panel, and
    /// is awaited on the Slint event loop. Several files may be picked.
    fn show_open_dialog(self: &Rc<Self>, window: &AppWindow) {
        let dialog = rfd::AsyncFileDialog::new()
            .set_title("Open Videos")
            .set_parent(&window.window().window_handle());
        // A convenience, not a capability claim: the engine decides what it
        // can read. macOS merges all filters into one allow-list, where
        // "All files" would not widen it; leave that panel unfiltered.
        #[cfg(not(target_os = "macos"))]
        let dialog = dialog
            .add_filter(
                "Video files",
                &["mp4", "m4v", "mov", "mkv", "webm", "ts", "avi", "y4m"],
            )
            .add_filter("All files", &["*"]);
        let picked = dialog.pick_files();
        let weak = Rc::downgrade(self);
        let spawned = slint::spawn_local(async move {
            let paths = picked.await.map_or_else(Vec::new, |files| {
                files.iter().map(|f| f.path().to_path_buf()).collect()
            });
            with(&weak, |ctl| ctl.dispatch(Intent::PickFinished(paths)));
        });
        if spawned.is_err() {
            // No event loop to drive the dialog: treat it as cancelled.
            self.dispatch(Intent::PickFinished(Vec::new()));
        }
    }

    fn show_save_dialog(
        self: &Rc<Self>,
        window: &AppWindow,
        directory: Option<PathBuf>,
        file_name: &str,
        output: OutputKind,
    ) {
        let mut dialog = rfd::AsyncFileDialog::new()
            .set_title("Choose Output")
            .set_parent(&window.window().window_handle())
            .set_file_name(file_name)
            .add_filter(output.label(), &[output.extension()]);
        if let Some(directory) = directory {
            dialog = dialog.set_directory(directory);
        }
        let picked = dialog.save_file();
        let weak = Rc::downgrade(self);
        let spawned = slint::spawn_local(async move {
            let path = picked.await.map(|file| file.path().to_path_buf());
            with(&weak, |ctl| ctl.dispatch(Intent::DestinationPicked(path)));
        });
        if spawned.is_err() {
            self.dispatch(Intent::DestinationPicked(None));
        }
    }

    /// Stops the timers and the session (see [`Session::shutdown`]).
    pub fn shutdown(&self) {
        self.clock.stop();
        self.cancel_deadline.stop();
        self.session.borrow_mut().shutdown();
    }
}

fn show_engine(window: &AppWindow, status: &EngineStatus) {
    window.set_engine_identity(
        match status {
            EngineStatus::Ready { engine, .. } => format!(
                "{} {} · protocol {}",
                engine.name,
                engine.version,
                studio_domain::protocol::PROTOCOL_VERSION
            ),
            other => other.summary(),
        }
        .into(),
    );
}
