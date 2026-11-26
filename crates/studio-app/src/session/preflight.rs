// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use studio_domain::job::{Failure, FailureCode, JobSpec, SourceFacts};
use studio_domain::options::{EffectiveOption, MAX_OPTIONS_PAGE_ITEMS};
use studio_domain::protocol::{Command, Event};

use crate::engine::Delivery;

// A reply can carry four rows. Limit the total across all pages too.
const MAX_POLICY_OPTIONS: u32 = 512;

pub(super) struct Preflight {
    pub(super) id: u64,
    pub(super) job: JobSpec,
    source: SourceFacts,
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
    pub(super) fn new(id: u64, job: JobSpec, source: SourceFacts) -> Self {
        Self {
            id,
            job,
            source,
            policy: Vec::new(),
            total: None,
        }
    }

    pub(super) fn handle(&mut self, delivery: Delivery) -> Next {
        match delivery {
            Delivery::Event(reply) if reply.request_id == super::HELLO_ID => match reply.body {
                Event::Hello { capabilities, .. }
                    if capabilities.options.is_some()
                        && capabilities.options == self.job.identity =>
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

    fn query(&self) -> Next {
        let Some(identity) = &self.job.identity else {
            return failure("The engine does not report an encoder option schema.");
        };
        let Ok(first) = u32::try_from(self.policy.len()) else {
            return failure("The engine option policy exceeds the supported size.");
        };
        Next::Send(Box::new(Command::ResolveOptions {
            schema: identity.schema,
            source: Box::new(self.source.clone()),
            settings: self.job.settings.clone(),
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
