// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Owns the inspect process that reads the two sides of a comparison. It
//! runs only while a pair is read. Letting it go closes its input, which
//! stops its read.

use std::path::PathBuf;
use std::time::Instant;

use studio_domain::compare::{AlignmentBasis, StreamInput};
use studio_domain::job::WirePath;
use studio_domain::layers::LayerDecodeSelection;
use studio_domain::preview::GrainPolicy;
use studio_domain::protocol::Command;

use super::{Delivery, Intent, Outcome, Role, Route, Session, UiEffect, Workspace, ending_text};
use crate::compare::{Compare, Effect, Input};
use crate::present::compare::basis_at;

impl Session {
    /// The Compare workspace.
    #[must_use]
    pub const fn compare(&self) -> &Compare {
        &self.compare
    }

    /// Whether an inspect process reads a comparison, for tests of its lifecycle.
    #[must_use]
    pub const fn comparing(&self) -> bool {
        self.compare_worker.is_some()
    }

    /// The user did something in the Compare workspace. Nothing in the
    /// queue changes.
    pub fn compare_input(&mut self, input: Input, now: Instant) -> Outcome {
        let mut outcome = Outcome::nothing();
        if self.shut_down {
            return outcome;
        }
        let effects = match input {
            Input::PairResult => {
                let Some(result) = self.state.comparable() else {
                    return outcome;
                };
                // An open dialog was asked for the pair that this one replaces.
                self.compare_dialog = None;
                // The new pair first: showing Compare reads a restored pair.
                let effects = self.compare.pair_encode(result);
                outcome = self.perform_compare(effects);
                outcome.merge(self.dispatch(Intent::WorkspaceRequested(Workspace::Compare), now));
                return outcome;
            }
            Input::Pick(side) => {
                // One dialog at a time: its answer belongs to the side it was asked for.
                if self.compare_dialog.is_none() {
                    self.compare_dialog = Some(side);
                    outcome.ui.push(UiEffect::ShowCompareDialog);
                }
                return outcome;
            }
            // A policy edit waits for the next save (any change of the pair
            // or the queue, or quit), so stepping the offset costs no write each.
            Input::Basis(index) => {
                if let Some(basis) = basis_at(&self.compare, index) {
                    self.compare.set_basis(basis);
                }
                outcome.compare = self.compare.take_dirty();
                return outcome;
            }
            Input::Offset(offset) => {
                self.compare
                    .set_basis(AlignmentBasis::AssumedOffset { offset });
                outcome.compare = self.compare.take_dirty();
                return outcome;
            }
            Input::Grain(apply) => {
                self.compare.set_grain(if apply {
                    GrainPolicy::Apply
                } else {
                    GrainPolicy::Skip
                });
                outcome.compare = self.compare.take_dirty();
                return outcome;
            }
            Input::Close => {
                self.compare_dialog = None;
                self.compare.close()
            }
        };
        outcome.merge(self.perform_compare(effects));
        outcome
    }

    /// The Compare dialog closed; `None` means it was cancelled. A file that
    /// a completed encode of the list published keeps the settings of that job.
    pub fn compare_picked(&mut self, path: Option<PathBuf>) -> Outcome {
        let (Some(side), Some(path), false) = (self.compare_dialog.take(), path, self.shut_down)
        else {
            return Outcome::nothing();
        };
        let input = self.state.published(&path).unwrap_or_else(|| StreamInput {
            path: WirePath::from_path(&path),
            selection: LayerDecodeSelection::Default,
            encode: None,
        });
        let effects = self.compare.pick(side, input);
        self.perform_compare(effects)
    }

    /// Reads a restored comparison once Compare is shown.
    pub(super) fn read_compare_if_shown(&mut self) {
        if self.state.workspace() == Workspace::Compare {
            let effects = self.compare.read_saved();
            self.run_compare(effects);
        }
    }

    pub(super) fn deliver_compare(&mut self, spawn: u64, delivery: Delivery) -> Outcome {
        if self
            .compare_worker
            .as_ref()
            .is_none_or(|(current, _)| *current != spawn)
        {
            return Outcome::nothing();
        }
        let effects = match delivery {
            Delivery::Event(envelope) => self.compare.event(envelope.request_id, envelope.body),
            Delivery::Ended(ending) => {
                self.compare_worker = None;
                self.compare.interrupted(&ending_text(&ending))
            }
            _ => return Outcome::nothing(),
        };
        self.perform_compare(effects)
    }

    fn perform_compare(&mut self, effects: Vec<Effect>) -> Outcome {
        self.run_compare(effects);
        self.persist_view();
        Outcome {
            compare: self.compare.take_dirty(),
            ..Outcome::nothing()
        }
    }

    fn run_compare(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            // Dropping the worker ends its input. It stops its read and exits.
            self.compare_worker = None;
            let Effect::Read(requests) = effect else {
                continue;
            };
            let spawn = self.compare_workers_started;
            self.compare_workers_started += 1;
            let started = self
                .spawn_as(Role::Inspect, Route::Compare(spawn))
                .and_then(|mut worker| {
                    for (id, input) in requests {
                        worker
                            .send(id, Command::CompareDescribe { input })
                            .map_err(|error| format!("Cannot send the request. {error}"))?;
                    }
                    Ok(worker)
                });
            match started {
                Ok(worker) => self.compare_worker = Some((spawn, worker)),
                Err(error) => {
                    // The failure asks for no process.
                    let _ = self.compare.interrupted(&error);
                }
            }
        }
    }
}
