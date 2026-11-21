// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The app without its window: the [`AppState`], the engine workers it
//! asks for, and the journal it is saved to.
//!
//! [`Session::dispatch`] applies an intent and carries out its effects:
//! starting probe and job workers, talking to them, removing a dead
//! worker's temporary file, and submitting the journal whenever the
//! durable state changed (never for progress counters). What only a window
//! can do (dialogs, timers, Show in Folder) comes back as [`UiEffect`]s.
//! Worker events are handed to `post` on their reader threads; whoever owns
//! the session brings them back to its thread and calls [`Session::deliver`].
//! The window uses the Slint event loop for that, the tests a channel.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use studio_domain::job::WirePath;
use studio_domain::protocol::{BuildIdentity, Command, Event};
use studio_domain::settings::OutputKind;

use crate::activity::{self, Changed};
use crate::engine::{self, Delivery, Ending, EngineStatus, Worker};
use crate::journal::{self, Writer};
use crate::recovery::{self, RealDisk};
use crate::source::ProbeFailure;
use crate::state::{AppState, Effect, Intent};

/// The request id of the startup `Hello`; probes and jobs count from 1.
const HELLO_ID: u64 = 0;

/// Which request a worker serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// The startup handshake.
    Hello,
    /// A probe, by request id.
    Probe(u64),
    /// A job, by request id.
    Job(u64),
}

/// How worker deliveries leave their reader threads.
pub type Post = Arc<dyn Fn(Route, Delivery) + Send + Sync>;

/// What only the window can do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiEffect {
    /// Show the native Open dialog.
    ShowOpenDialog,
    /// Show the native Save dialog.
    ShowSaveDialog {
        /// The folder to start in.
        directory: Option<PathBuf>,
        /// The suggested file name.
        file_name: String,
        /// The output kind, for the file type filter.
        output: OutputKind,
    },
    /// Arm the [`activity::CANCEL_TIMEOUT`] timer for job `0`; when it
    /// fires, dispatch [`activity::Input::CancelTimedOut`] for it.
    CancelTimer(u64),
    /// Show this file in the file manager.
    Reveal(PathBuf),
    /// The startup handshake answered.
    Engine(EngineStatus),
}

/// What one dispatch changed, for the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// What only the window can do, in order.
    pub ui: Vec<UiEffect>,
    /// What to redraw.
    pub changed: Changed,
}

/// The state, its workers and its journal.
pub struct Session {
    state: AppState,
    engine: Result<PathBuf, String>,
    post: Post,
    hello: Option<Worker>,
    probe: Option<(u64, Worker)>,
    job: Option<(u64, Worker)>,
    journal: Option<Writer>,
    /// The journal bytes last submitted.
    saved: Option<Vec<u8>>,
    shut_down: bool,
}

impl Session {
    /// A session over `state`, starting workers from the `engine`
    /// executable (or reporting why there is none) and saving to `journal`
    /// when given. Saves once right away, so a recovered queue is recorded.
    #[must_use]
    pub fn new(
        state: AppState,
        engine: Result<PathBuf, String>,
        post: Post,
        journal: Option<Writer>,
    ) -> Self {
        let mut session = Self {
            state,
            engine,
            post,
            hello: None,
            probe: None,
            job: None,
            journal,
            saved: None,
            shut_down: false,
        };
        session.persist();
        session
    }

    /// The state.
    #[must_use]
    pub const fn state(&self) -> &AppState {
        &self.state
    }

    /// Sends the startup `Hello`.
    pub fn start_hello(&mut self, client: BuildIdentity, now: Instant) -> Outcome {
        let started = self.spawn(Route::Hello).and_then(|mut worker| {
            worker
                .send(HELLO_ID, Command::Hello { client })
                .map_err(|e| format!("could not send the handshake: {e}"))?;
            // End of input tells the worker to exit once it has answered.
            worker.close_input();
            Ok(worker)
        });
        match started {
            Ok(worker) => {
                self.hello = Some(worker);
                Outcome {
                    ui: Vec::new(),
                    changed: Changed::Nothing,
                }
            }
            Err(e) => {
                let status = match &self.engine {
                    Ok(path) if !path.exists() => EngineStatus::NotFound(path.clone()),
                    _ => EngineStatus::Failed(e),
                };
                self.engine_answered(status, now)
            }
        }
    }

    fn engine_answered(&mut self, status: EngineStatus, now: Instant) -> Outcome {
        let answer = match &status {
            EngineStatus::Ready { capabilities, .. } => Ok(capabilities.clone()),
            EngineStatus::NotFound(path) => Err(format!(
                "The encoding engine is missing ({}). Reinstall Splot Studio.",
                path.display()
            )),
            EngineStatus::Incompatible(why) => Err(format!(
                "The encoding engine does not match this app: {why}. Reinstall Splot Studio."
            )),
            EngineStatus::Failed(why) => Err(format!("The encoding engine did not start: {why}.")),
        };
        let mut outcome = self.dispatch(Intent::EngineAnswered(answer), now);
        outcome.ui.insert(0, UiEffect::Engine(status));
        outcome
    }

    /// Applies `intent` and everything it leads to.
    pub fn dispatch(&mut self, intent: Intent, now: Instant) -> Outcome {
        let mut outcome = Outcome {
            ui: Vec::new(),
            changed: Changed::Nothing,
        };
        if self.shut_down {
            return outcome;
        }
        let mut pending = VecDeque::from([intent]);
        while let Some(intent) = pending.pop_front() {
            // Destinations are looked at before anything can start, but not
            // for every progress counter.
            let counters = matches!(
                intent,
                Intent::Job {
                    input: activity::Input::Event(Event::Progress { .. }),
                    ..
                }
            );
            if !counters {
                self.state.refresh_destinations(&RealDisk);
            }
            let update = self.state.apply(intent, now, &RealDisk);
            outcome.changed = more(outcome.changed, update.changed);
            for effect in update.effects {
                self.perform(effect, &mut pending, &mut outcome.ui);
            }
        }
        if outcome.changed == Changed::Phase {
            self.persist();
        }
        outcome
    }

    /// Routes one worker delivery.
    pub fn deliver(&mut self, route: Route, delivery: Delivery, now: Instant) -> Outcome {
        let nothing = Outcome {
            ui: Vec::new(),
            changed: Changed::Nothing,
        };
        if self.shut_down {
            return nothing;
        }
        match route {
            Route::Hello => {
                // Only the first delivery answers; the stream ending after
                // it is the worker exiting as asked. Dropping it reaps it.
                if self.hello.take().is_some() {
                    self.engine_answered(EngineStatus::from_hello(delivery), now)
                } else {
                    nothing
                }
            }
            Route::Probe(id) => {
                let result = match delivery {
                    Delivery::Event(e) if e.request_id != id => return nothing,
                    Delivery::Event(e) => match e.body {
                        Event::Probed { facts } => Ok(facts),
                        Event::Failed { failure } => Err(ProbeFailure::from(failure)),
                        Event::Error { message, .. } => Err(ProbeFailure::other(format!(
                            "The engine refused: {message}"
                        ))),
                        _ => return nothing,
                    },
                    Delivery::Ended(ending) => {
                        if self
                            .probe
                            .as_ref()
                            .is_some_and(|(current, _)| *current == id)
                        {
                            self.probe = None;
                        }
                        Err(ProbeFailure::other(format!(
                            "The engine stopped without answering ({}).",
                            ending_text(&ending)
                        )))
                    }
                };
                // A probe that already answered ignores this one.
                self.dispatch(Intent::ProbeAnswered { id, result }, now)
            }
            Route::Job(id) => {
                let input = match delivery {
                    Delivery::Event(e) if e.request_id != id => {
                        eprintln!(
                            "{}: ignoring an event for request {} during job {id}",
                            crate::APP_NAME,
                            e.request_id
                        );
                        return nothing;
                    }
                    Delivery::Event(e) => activity::Input::Event(e.body),
                    Delivery::Ended(ending) => {
                        if self.job.as_ref().is_some_and(|(current, _)| *current == id) {
                            self.job = None;
                        }
                        activity::Input::WorkerEnded {
                            detail: ending_text(&ending),
                        }
                    }
                };
                self.dispatch(Intent::Job { id, input }, now)
            }
        }
    }

    fn perform(&mut self, effect: Effect, pending: &mut VecDeque<Intent>, ui: &mut Vec<UiEffect>) {
        match effect {
            Effect::ShowOpenDialog => ui.push(UiEffect::ShowOpenDialog),
            Effect::ShowSaveDialog {
                directory,
                file_name,
                output,
            } => ui.push(UiEffect::ShowSaveDialog {
                directory,
                file_name,
                output,
            }),
            Effect::Reveal(path) => ui.push(UiEffect::Reveal(path)),
            Effect::Probe { item, request } => {
                // One metadata read for the row; the engine reads the rest.
                let size = std::fs::metadata(&request.path).ok().map(|m| m.len());
                self.state.note_size(item, size);
                let id = request.id;
                // Replacing an older probe drops its worker, which then exits.
                let started = self.spawn(Route::Probe(id)).and_then(|mut worker| {
                    let command = Command::Probe {
                        source: WirePath::from_path(&request.path),
                        video_stream: request.video_stream,
                    };
                    worker
                        .send(id, command)
                        .map_err(|e| format!("could not ask the engine: {e}"))?;
                    worker.close_input();
                    Ok(worker)
                });
                match started {
                    Ok(worker) => self.probe = Some((id, worker)),
                    Err(message) => {
                        self.probe = None;
                        pending.push_back(Intent::ProbeAnswered {
                            id,
                            result: Err(ProbeFailure::other(message)),
                        });
                    }
                }
            }
            Effect::Start { id, job } => {
                let started = self.spawn(Route::Job(id)).and_then(|mut worker| {
                    worker
                        .send(id, Command::Start { job })
                        .map_err(|e| format!("could not send the job: {e}"))?;
                    Ok(worker)
                });
                match started {
                    // The previous job's worker, already told to exit, is
                    // dropped here and reaped by its watchdog.
                    Ok(worker) => self.job = Some((id, worker)),
                    Err(detail) => pending.push_back(Intent::Job {
                        id,
                        input: activity::Input::WorkerEnded { detail },
                    }),
                }
            }
            Effect::Worker { id, effect } => {
                let Some((_, worker)) = self.job.as_mut().filter(|(current, _)| *current == id)
                else {
                    return;
                };
                match effect {
                    activity::Effect::None => {}
                    activity::Effect::SendCancel => {
                        // Cancel carries the job's request id: it is about that job.
                        if worker.send(id, Command::Cancel).is_err() {
                            // The worker is already gone; its end of stream decides.
                            worker.close_input();
                        }
                        ui.push(UiEffect::CancelTimer(id));
                    }
                    activity::Effect::CloseInput => worker.close_input(),
                    activity::Effect::Kill => worker.kill(),
                }
            }
            Effect::CleanPartial {
                item,
                partial,
                destination,
            } => {
                let fate = recovery::remove_partial(&partial, &destination);
                pending.push_back(Intent::PartialCleaned { item, fate });
            }
        }
    }

    fn spawn(&self, route: Route) -> Result<Worker, String> {
        let engine = self.engine.as_ref().map_err(Clone::clone)?;
        let post = Arc::clone(&self.post);
        Worker::spawn(engine, move |delivery| post(route, delivery)).map_err(|e| e.to_string())
    }

    /// Submits the journal if it differs from the last one submitted.
    fn persist(&mut self) {
        let Some(writer) = &self.journal else {
            return;
        };
        let journal = journal::snapshot(
            self.state.next_id(),
            self.state.settings(),
            self.state.items(),
        );
        let Ok(bytes) = journal::encode(&journal) else {
            return;
        };
        if self.saved.as_ref() != Some(&bytes) {
            writer.submit(bytes.clone());
            self.saved = Some(bytes);
        }
    }

    /// Stops everything: the converting file is journalled as interrupted
    /// and the journal written, then every worker is told to stop (a
    /// running job cancels and removes its temporary file) and killed if it
    /// has not exited within [`engine::EXIT_GRACE`]. Blocks until done.
    pub fn shutdown(&mut self) {
        if self.shut_down {
            return;
        }
        self.state.interrupt_for_shutdown();
        self.persist();
        if let Some(writer) = &self.journal {
            writer.flush();
        }
        self.shut_down = true;
        let workers = [
            self.job.take().map(|(_, w)| w),
            self.probe.take().map(|(_, w)| w),
            self.hello.take(),
        ];
        for worker in workers.into_iter().flatten() {
            worker.shutdown(engine::EXIT_GRACE);
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shutdown();
    }
}

const fn more(a: Changed, b: Changed) -> Changed {
    match (a, b) {
        (Changed::Phase, _) | (_, Changed::Phase) => Changed::Phase,
        (Changed::Counters, _) | (_, Changed::Counters) => Changed::Counters,
        (Changed::Nothing, Changed::Nothing) => Changed::Nothing,
    }
}

fn ending_text(ending: &Ending) -> String {
    match &ending.error {
        None => ending.exit.clone(),
        Some(e) => format!("{e}; {}", ending.exit),
    }
}
