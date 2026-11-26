// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use std::{collections::VecDeque, sync::Arc, time::Instant};

use studio_domain::job::{Failure, FailureCode, JobSpec, SourceFacts};
use studio_domain::options::{EffectiveOption, MAX_OPTIONS_PAGE_ITEMS};
use studio_domain::protocol::{BuildIdentity, Command, Event};

use crate::activity;
use crate::engine::Delivery;
use crate::journal;
use crate::state::Intent;

use super::{HELLO_ID, Outcome, Route, Session};
use crate::queue::ItemId;

// A reply can carry four rows. Limit the total across all pages too.
const MAX_POLICY_OPTIONS: u32 = 512;

pub(super) struct Request {
    pub(super) item: Option<ItemId>,
    pub(super) job: JobSpec,
    pub(super) source: SourceFacts,
}

pub(super) struct Preflight {
    pub(super) id: u64,
    pub(super) request: Request,
    policy: Vec<EffectiveOption>,
    total: Option<u32>,
}

pub(super) enum Next {
    Send(Box<Command>),
    Ready(Vec<EffectiveOption>),
    Failed(Failure),
    Ignore,
}

impl Preflight {
    pub(super) const fn new(id: u64, request: Request) -> Self {
        Self {
            id,
            request,
            policy: Vec::new(),
            total: None,
        }
    }

    pub(super) fn handle(&mut self, delivery: Delivery) -> Next {
        match delivery {
            Delivery::Event(reply) if reply.request_id == super::HELLO_ID => match reply.body {
                Event::Hello { capabilities, .. }
                    if capabilities.options.is_some()
                        && capabilities.options == self.request.job.identity =>
                {
                    self.query()
                }
                _ => {
                    failure("The engine or encoder option schema changed. Refresh the submission.")
                }
            },
            Delivery::Event(reply) if reply.request_id == self.id => match reply.body {
                Event::OptionsResolved {
                    first,
                    total,
                    values,
                } => {
                    let end = usize::try_from(first)
                        .ok()
                        .and_then(|first| first.checked_add(values.len()));
                    if total > MAX_POLICY_OPTIONS
                        || first as usize != self.policy.len()
                        || self.total.is_some_and(|previous| previous != total)
                        || values.is_empty()
                        || values.len() > MAX_OPTIONS_PAGE_ITEMS as usize
                        || end.is_none_or(|end| end > total as usize)
                    {
                        return failure("The engine sent an invalid encoder policy page.");
                    }
                    self.total = Some(total);
                    self.policy.extend(values);
                    if self.policy.len() == total as usize {
                        Next::Ready(std::mem::take(&mut self.policy))
                    } else {
                        self.query()
                    }
                }
                Event::OptionsFailed { error } => {
                    failure(format!("{}: {}", error.field, error.message))
                }
                Event::Error { message, .. } => failure(message),
                _ => failure("The engine sent an unexpected preflight reply."),
            },
            Delivery::Ended(ending) => failure(format!(
                "The engine stopped during preflight ({}).",
                super::ending_text(&ending)
            )),
            _ => Next::Ignore,
        }
    }

    pub(super) fn query(&self) -> Next {
        let Some(identity) = &self.request.job.identity else {
            return failure("The engine does not report an encoder option schema.");
        };
        if self.request.source.fingerprint != self.request.job.fingerprint
            || self.request.source.video_stream != self.request.job.video_stream
        {
            return failure("The source changed after submission. Add the source again.");
        }
        let Ok(first) = u32::try_from(self.policy.len()) else {
            return failure("The engine option policy exceeds the supported size.");
        };
        Next::Send(Box::new(Command::ResolveOptions {
            schema: identity.schema,
            source: Box::new(self.request.source.clone()),
            settings: self.request.job.settings.clone(),
            first,
            count: MAX_OPTIONS_PAGE_ITEMS,
        }))
    }
}

fn failure(message: impl Into<String>) -> Next {
    Next::Failed(Failure {
        code: FailureCode::InvalidSettings,
        message: message.into(),
    })
}

impl Session {
    pub(super) fn begin_preflight(
        &mut self,
        id: u64,
        job: JobSpec,
        pending: &mut VecDeque<Intent>,
    ) {
        let source = self
            .state
            .items()
            .iter()
            .find(|item| item.attempt().is_some_and(|attempt| attempt.request == id))
            .and_then(crate::queue::Item::facts)
            .cloned();
        let started = source
            .ok_or_else(|| "The submitted source has no probe result.".to_owned())
            .and_then(|source| {
                if source.fingerprint != job.fingerprint || source.video_stream != job.video_stream
                {
                    return Err("The source changed after submission. Add the source again.".into());
                }
                let mut worker = self.spawn(Route::Job(id))?;
                worker
                    .send(
                        HELLO_ID,
                        Command::Hello {
                            client: BuildIdentity {
                                name: crate::APP_NAME.to_owned(),
                                version: env!("CARGO_PKG_VERSION").to_owned(),
                            },
                        },
                    )
                    .map_err(|error| error.to_string())?;
                Ok((worker, source))
            });
        match started {
            Ok((worker, source)) => {
                self.preflight_queue = self
                    .state
                    .items()
                    .iter()
                    .filter(|item| self.state.queued(item))
                    .filter_map(|item| {
                        let submission = item.submitted.as_ref()?;
                        let source = item.facts()?;
                        (submission.policy.is_empty()).then(|| Request {
                            item: Some(item.id()),
                            job: submission.job.clone(),
                            source: source.clone(),
                        })
                    })
                    .collect();
                self.preflight = Some(Preflight::new(
                    id,
                    Request {
                        item: None,
                        job,
                        source,
                    },
                ));
                self.job = Some((id, worker));
            }
            Err(detail) => pending.push_back(Intent::Job {
                id,
                input: activity::Input::WorkerEnded { detail },
            }),
        }
    }

    pub(super) fn deliver_preflight(
        &mut self,
        id: u64,
        delivery: Delivery,
        now: Instant,
    ) -> Outcome {
        let disconnected = matches!(delivery, Delivery::Ended(_));
        let Some(preflight) = self.preflight.as_mut() else {
            return Outcome::nothing();
        };
        let next = preflight.handle(delivery);
        self.preflight_next(id, next, disconnected, now)
    }

    fn preflight_next(&mut self, id: u64, next: Next, disconnected: bool, now: Instant) -> Outcome {
        let result = match next {
            Next::Ignore => return Outcome::nothing(),
            Next::Send(command) => {
                return match self.send_job(id, *command) {
                    Ok(()) => Outcome::nothing(),
                    Err(message) => self.preflight_failed(
                        id,
                        Failure {
                            code: FailureCode::InvalidSettings,
                            message,
                        },
                        now,
                    ),
                };
            }
            Next::Ready(values) => Ok(values),
            Next::Failed(failure) if disconnected => {
                return self.preflight_failed(id, failure, now);
            }
            Next::Failed(failure) => Err(failure),
        };
        let Some(preflight) = self.preflight.take() else {
            return Outcome::nothing();
        };
        let mut outcome = if let Some(item) = preflight.request.item {
            self.dispatch(Intent::QueuedPolicy { item, result }, now)
        } else {
            let values = match result {
                Ok(values) => values,
                Err(failure) => return self.preflight_failed(id, failure, now),
            };
            let changed = self
                .state
                .items()
                .iter()
                .filter_map(crate::queue::Item::attempt)
                .find(|attempt| attempt.request == id)
                .is_some_and(|attempt| !attempt.policy.is_empty() && attempt.policy != values);
            if changed {
                return self.preflight_failed(id, Failure {
                    code: FailureCode::InvalidSettings,
                    message: "The effective encoder policy changed. Add the source again to make a new submission.".into(),
                }, now);
            }
            self.prepared = Some((id, preflight.request.job));
            self.dispatch(Intent::JobPolicy { id, values }, now)
        };
        outcome.merge(self.continue_preflight(id, now));
        outcome
    }

    fn continue_preflight(&mut self, id: u64, now: Instant) -> Outcome {
        let mut outcome = Outcome::nothing();
        while let Some(request) = self.preflight_queue.pop_front() {
            let preflight = Preflight::new(id, request);
            let next = if self
                .prepared
                .as_ref()
                .is_some_and(|(_, active)| active.identity == preflight.request.job.identity)
            {
                preflight.query()
            } else {
                Next::Failed(Failure {
                    code: FailureCode::InvalidSettings,
                    message: "The engine or encoder option schema changed. Refresh the submission."
                        .into(),
                })
            };
            if let Next::Failed(failure) = next {
                if let Some(item) = preflight.request.item {
                    outcome.merge(self.dispatch(
                        Intent::QueuedPolicy {
                            item,
                            result: Err(failure),
                        },
                        now,
                    ));
                }
                continue;
            }
            self.preflight = Some(preflight);
            outcome.merge(self.preflight_next(id, next, false, now));
            return outcome;
        }
        outcome.merge(self.save_prepared(id, now));
        outcome
    }

    fn save_prepared(&mut self, id: u64, now: Instant) -> Outcome {
        let Some(writer) = &self.journal else {
            return self.start_prepared(id, now);
        };
        let journal = journal::snapshot(
            self.state.next_id(),
            self.state.option_request().clone(),
            self.state.items(),
            self.view(),
        );
        let bytes = match journal::encode(&journal) {
            Ok(bytes) => bytes,
            Err(error) => return self.journal_failed(id, &error.to_string(), now),
        };
        let post = Arc::clone(&self.post);
        writer.submit_confirmed(bytes, move |result| {
            post(Route::Journal(id), Delivery::JournalSaved(result));
        });
        Outcome::nothing()
    }

    pub(super) fn journal_failed(&mut self, id: u64, message: &str, now: Instant) -> Outcome {
        self.state.pause_after_current();
        self.preflight_failed(
            id,
            Failure {
                code: FailureCode::Output,
                message: format!("Cannot save the submitted encoder policy. {message}"),
            },
            now,
        )
    }

    pub(super) fn start_prepared(&mut self, id: u64, now: Instant) -> Outcome {
        let Some((_, job)) = self.prepared.take_if(|(current, _)| *current == id) else {
            return Outcome::nothing();
        };
        let live = Some(self.preview.live_box());
        if let Err(detail) = self.send_job(id, Command::StartChecked { job, live }) {
            return self.dispatch(
                Intent::Job {
                    id,
                    input: activity::Input::WorkerEnded { detail },
                },
                now,
            );
        }
        self.confirming = Some((id, 0));
        Outcome::nothing()
    }

    pub(super) fn confirm_policy(
        &mut self,
        id: u64,
        first: u32,
        total: u32,
        values: &[EffectiveOption],
        now: Instant,
    ) -> Outcome {
        let Some((_, offset)) = self.confirming.filter(|(current, _)| *current == id) else {
            return Outcome::nothing();
        };
        let next = self
            .state
            .items()
            .iter()
            .filter_map(crate::queue::Item::attempt)
            .find(|attempt| attempt.request == id)
            .and_then(|attempt| matching_page(&attempt.policy, offset, first, total, values));
        let Some(end) = next else {
            return self.preflight_failed(
                id,
                Failure {
                    code: FailureCode::InvalidSettings,
                    message:
                        "The engine policy differs from the saved policy. Submit the job again."
                            .into(),
                },
                now,
            );
        };

        if end == total as usize {
            self.confirming = None;
            if let Err(message) = self.send_job(id, Command::ConfirmPolicy) {
                return self.preflight_failed(
                    id,
                    Failure {
                        code: FailureCode::InvalidSettings,
                        message,
                    },
                    now,
                );
            }
        } else {
            self.confirming = Some((id, end));
        }
        Outcome::nothing()
    }

    pub(super) fn clear_preflight(&mut self) {
        self.preflight = None;
        self.preflight_queue.clear();
        self.prepared = None;
        self.confirming = None;
    }

    fn send_job(&mut self, id: u64, command: Command) -> Result<(), String> {
        let Some((_, worker)) = self.job.as_mut().filter(|(current, _)| *current == id) else {
            return Err("The job worker stopped.".into());
        };
        worker.send(id, command).map_err(|error| error.to_string())
    }

    fn preflight_failed(&mut self, id: u64, failure: Failure, now: Instant) -> Outcome {
        self.clear_preflight();
        self.job.take();
        self.dispatch(
            Intent::Job {
                id,
                input: activity::Input::Event(Event::Failed { failure }),
            },
            now,
        )
    }
}

fn matching_page(
    expected: &[EffectiveOption],
    offset: usize,
    first: u32,
    total: u32,
    values: &[EffectiveOption],
) -> Option<usize> {
    if total > MAX_POLICY_OPTIONS
        || total as usize != expected.len()
        || first as usize != offset
        || values.is_empty()
        || values.len() > MAX_OPTIONS_PAGE_ITEMS as usize
    {
        return None;
    }
    let end = offset.checked_add(values.len())?;
    (expected.get(offset..end)? == values).then_some(end)
}

#[cfg(test)]
mod tests {
    use studio_domain::options::OptionValue;

    use super::*;

    fn option(key: &str) -> EffectiveOption {
        EffectiveOption {
            key: key.into(),
            requested: None,
            value: OptionValue::Bool(false),
            origin: OptionValue::Choice("codec default".into()),
            screen: None,
            screen_origin: None,
        }
    }

    #[test]
    fn policy_confirmation_requires_matching_order_total_and_values() {
        let expected = [option("rdoq"), option("wavefront")];
        assert_eq!(matching_page(&expected, 0, 0, 2, &expected[..1]), Some(1));
        assert_eq!(matching_page(&expected, 1, 1, 2, &expected[1..]), Some(2));
        assert_eq!(matching_page(&expected, 1, 0, 2, &expected[..1]), None);
        assert_eq!(matching_page(&expected, 0, 0, 1, &expected[..1]), None);
        assert_eq!(matching_page(&expected, 0, 0, 2, &expected[1..]), None);
        assert_eq!(matching_page(&expected, 0, 0, 2, &[]), None);
    }
}
