// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The Slint-thread side of the app: window callbacks become intents for
//! the [`Session`], its outcomes become redraws, dialogs and timers.
//! Worker events and journal results reach this thread through
//! [`slint::invoke_from_event_loop`].

mod advanced;

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, SharedString, Timer, TimerMode};
use studio_domain::preview::PictureBox;
use studio_domain::protocol::BuildIdentity;
use studio_domain::settings::OutputKind;

use crate::activity::{self, Changed};
use crate::engine::{Delivery, EngineStatus};
use crate::preview::{Dirty, Input};
use crate::render::{render, render_analysis, render_counters, render_jobs, render_preview};
use crate::session::{Outcome, Post, Route, Session, UiEffect};
use crate::state::Intent;
use crate::ui::AppWindow;
use crate::workspace::Workspace;
use crate::{APP_NAME, present, reveal};

/// The window's controller.
pub struct Controller {
    window: slint::Weak<AppWindow>,
    session: RefCell<Session>,
    /// Ticks the elapsed time while a file converts; stopped otherwise.
    clock: Timer,
    /// Fires once [`activity::CANCEL_TIMEOUT`] after Cancel.
    cancel_deadline: Timer,
    /// Fires [`RESIZE_SETTLE`] after the picture area's size last changed.
    resize_settle: Timer,
    advanced: RefCell<advanced::Editor>,
    advanced_settle: Timer,
    /// The window was not minimized when last looked at.
    visible: Cell<bool>,
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
        ctl.check_visible();
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

/// How long the picture area's size must hold before pictures are asked
/// for at the new size (moving between screens can change it repeatedly).
const RESIZE_SETTLE: Duration = Duration::from_millis(150);

/// The picture area and a row thumbnail, in physical pixels.
fn geometry(window: &AppWindow) -> Input {
    let size = |w: i32, h: i32| {
        PictureBox::clamped(u32::try_from(w).unwrap_or(1), u32::try_from(h).unwrap_or(1))
    };
    Input::Resized {
        stage: size(window.get_stage_px_width(), window.get_stage_px_height()),
        thumb: size(window.get_thumb_px_width(), window.get_thumb_px_height()),
    }
}

/// A logical length in whole physical pixels.
// Clamped to a range `i64` holds exactly, so the cast cannot truncate.
#[allow(clippy::cast_possible_truncation)]
fn physical(logical: f32, scale: f32) -> i64 {
    let value = (f64::from(logical) * f64::from(scale)).round();
    if value.is_finite() {
        value.clamp(-1e9, 1e9) as i64
    } else {
        0
    }
}

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
            resize_settle: Timer::default(),
            advanced: RefCell::default(),
            advanced_settle: Timer::default(),
            visible: Cell::new(true),
        });
        CONTROLLER.with(|c| *c.borrow_mut() = Rc::downgrade(&ctl));
        render(window, ctl.session.borrow().state(), Instant::now());
        render_analysis(window, ctl.session.borrow().analysis());
        render_jobs(window, &ctl.session.borrow().jobs());
        let all = Dirty {
            picture: true,
            view: true,
            thumbs: true,
        };
        render_preview(
            window,
            ctl.session.borrow().state(),
            ctl.session.borrow().preview(),
            all,
        );

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
        ctl.wire_advanced(window);
        ctl.wire_preview(window);
        ctl.wire_analysis(window);

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
        self.check_visible();
        if self.preview_simple_preset(&intent) {
            return;
        }
        let outcome = self.session.borrow_mut().dispatch(intent, Instant::now());
        self.handle(outcome);
    }

    fn wire_analysis(self: &Rc<Self>, window: &AppWindow) {
        let weak = Rc::downgrade(self);
        crate::bind_analysis(window, move |input| {
            with(&weak, |ctl| {
                let outcome = ctl.session.borrow_mut().analysis_input(input);
                ctl.handle(outcome);
            });
        });
        let session = |act: fn(&mut Session, Instant) -> Outcome| {
            let ctl = Rc::downgrade(self);
            move || {
                with(&ctl, |ctl| {
                    ctl.check_visible();
                    let outcome = act(&mut ctl.session.borrow_mut(), Instant::now());
                    ctl.handle(outcome);
                });
            }
        };
        window.on_analyze_open_requested(session(Session::analyze_open_requested));
        window
            .global::<crate::ui::Analysis>()
            .on_open(session(Session::analyze_open_requested));
        window.on_analyze_result_requested(session(Session::analyze_result));
        let job = |act: fn(&mut Session, usize, Instant) -> Outcome| {
            let ctl = Rc::downgrade(self);
            move |index: i32| {
                let Ok(index) = usize::try_from(index) else {
                    return;
                };
                with(&ctl, |ctl| {
                    ctl.check_visible();
                    let outcome = act(&mut ctl.session.borrow_mut(), index, Instant::now());
                    ctl.handle(outcome);
                });
            }
        };
        window.on_job_show(job(Session::show_job));
        window.on_job_cancel(job(Session::cancel_job));
        window.on_workspace_chosen({
            let ctl = Rc::downgrade(self);
            move |index| {
                if let Some(workspace) = Workspace::offered(index) {
                    with(&ctl, |ctl| {
                        ctl.dispatch(Intent::WorkspaceRequested(workspace));
                    });
                }
            }
        });
    }

    /// Hands the picture area's input to the session.
    fn preview_input(self: &Rc<Self>, input: Input) {
        self.check_visible();
        let outcome = self.session.borrow_mut().preview_input(input);
        self.handle(outcome);
    }

    /// Tells the preview when the window was minimized or restored. Slint
    /// reports no event for it, so this is looked at whenever anything
    /// happens; Wayland does not tell at all, and there the window always
    /// counts as visible.
    fn check_visible(self: &Rc<Self>) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let visible = !window.window().is_minimized();
        if visible != self.visible.replace(visible) {
            let outcome = self
                .session
                .borrow_mut()
                .preview_input(Input::Visible(visible));
            self.handle(outcome);
        }
    }

    fn wire_preview(self: &Rc<Self>, window: &AppWindow) {
        let on = |input: fn(i32) -> Input| {
            let ctl = Rc::downgrade(self);
            move |value| with(&ctl, |ctl| ctl.preview_input(input(value)))
        };
        window.on_stage_step(on(|delta| Input::Step(i64::from(delta))));
        window.on_stage_seek(on(|frame| Input::Seek(u64::try_from(frame).unwrap_or(0))));
        let on = |input: Input| {
            let ctl = Rc::downgrade(self);
            move || with(&ctl, |ctl| ctl.preview_input(input))
        };
        window.on_stage_end(on(Input::End));
        window.on_stage_play_pause(on(Input::PlayPause));
        window.on_stage_toggle_native(on(Input::ToggleNative));
        window.on_stage_compare({
            let ctl = Rc::downgrade(self);
            move |held| with(&ctl, |ctl| ctl.preview_input(Input::Compare(held)))
        });
        window.on_stage_pan({
            let ctl = Rc::downgrade(self);
            let weak = window.as_weak();
            move |dx, dy| {
                let scale = weak.upgrade().map_or(1.0, |w| w.window().scale_factor());
                let (dx, dy) = (physical(dx, scale), physical(dy, scale));
                with(&ctl, |ctl| ctl.preview_input(Input::Pan { dx, dy }));
            }
        });
        window.on_stage_geometry_changed({
            let ctl = Rc::downgrade(self);
            move || {
                with(&ctl, |ctl| {
                    let weak = Rc::downgrade(ctl);
                    ctl.resize_settle
                        .start(TimerMode::SingleShot, RESIZE_SETTLE, move || {
                            with(&weak, |ctl| {
                                if let Some(window) = ctl.window.upgrade() {
                                    ctl.preview_input(geometry(&window));
                                }
                            });
                        });
                });
            }
        });
        // The first size needs no settling.
        self.preview_input(geometry(window));
    }

    fn handle(self: &Rc<Self>, outcome: Outcome) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        for effect in outcome.ui {
            match effect {
                UiEffect::ShowOpenDialog => self.show_open_dialog(&window),
                UiEffect::ShowAnalyzeDialog => self.show_analyze_dialog(&window),
                UiEffect::ShowLocateDialog => self.show_locate_dialog(&window),
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
        if outcome.advanced || outcome.changed == Changed::Phase {
            self.render_advanced(&window, &session);
        }
        if outcome.preview.any() {
            render_preview(&window, state, session.preview(), outcome.preview);
        }
        if outcome.analysis {
            render_analysis(&window, session.analysis());
        }
        if outcome.analysis || outcome.changed != Changed::Nothing {
            render_jobs(&window, &session.jobs());
        }
        drop(session);
        if outcome.advanced || outcome.changed == Changed::Phase {
            self.check_preset_preview();
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

    /// Shows the native Open dialog for AV2 streams to analyze, as
    /// [`Self::show_open_dialog`] does for files to convert.
    fn show_analyze_dialog(self: &Rc<Self>, window: &AppWindow) {
        let dialog = rfd::AsyncFileDialog::new()
            .set_title("Open AV2 Streams to Analyze")
            .set_parent(&window.window().window_handle());
        // A convenience, not a capability claim: the engine identifies the
        // framing from the contents. See `show_open_dialog` for macOS.
        #[cfg(not(target_os = "macos"))]
        let dialog = dialog
            .add_filter("AV2 streams", &["ivf", "obu"])
            .add_filter("All files", &["*"]);
        let picked = dialog.pick_files();
        let weak = Rc::downgrade(self);
        let spawned = slint::spawn_local(async move {
            let paths = picked.await.map_or_else(Vec::new, |files| {
                files.iter().map(|f| f.path().to_path_buf()).collect()
            });
            with(&weak, |ctl| {
                let outcome = ctl.session.borrow_mut().analyze_picked(paths);
                ctl.handle(outcome);
            });
        });
        if spawned.is_err() {
            let outcome = self.session.borrow_mut().analyze_picked(Vec::new());
            self.handle(outcome);
        }
    }

    /// Asks where the shown missing document is now: one file, any name.
    fn show_locate_dialog(self: &Rc<Self>, window: &AppWindow) {
        let picked = rfd::AsyncFileDialog::new()
            .set_title("Locate the Stream")
            .set_parent(&window.window().window_handle())
            .pick_file();
        let weak = Rc::downgrade(self);
        let spawned = slint::spawn_local(async move {
            let path = picked.await.map(|file| file.path().to_path_buf());
            with(&weak, |ctl| {
                let outcome = ctl.session.borrow_mut().locate_picked(path);
                ctl.handle(outcome);
            });
        });
        if spawned.is_err() {
            let outcome = self.session.borrow_mut().locate_picked(None);
            self.handle(outcome);
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
        self.resize_settle.stop();
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
