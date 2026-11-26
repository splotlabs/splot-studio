// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use studio_domain::job::JobSpec;
use studio_domain::options::FieldError;
use studio_domain::protocol::{Command, Event};

use crate::engine::Delivery;
use crate::jobs::Work;
use crate::queue::Item;
use crate::state::{Intent, RelatedAction, RelatedResolution};

use super::{Outcome, Route, Session, advanced::Connection};

impl Session {
    /// Request an action for the source or conversion in the Jobs list.
    pub fn prepare_job_action(
        &mut self,
        index: usize,
        action: RelatedAction,
        now: Instant,
    ) -> Outcome {
        let Some(Work::Convert(item)) = self.jobs().get(index).map(|job| job.work) else {
            return Outcome::nothing();
        };
        self.dispatch(
            Intent::PrepareRelated {
                item: item.id(),
                action,
            },
            now,
        )
    }

    /// Check the selected source and VBR draft before generating a first pass.
    pub fn generate_pass(&mut self, now: Instant) -> Outcome {
        let Some(item) = self.state.selected().map(Item::id) else {
            return Outcome::nothing();
        };
        self.dispatch(
            Intent::PrepareRelated {
                item,
                action: RelatedAction::FirstPass,
            },
            now,
        )
    }

    pub(super) fn prepare_related(
        &self,
        request: u64,
        job: JobSpec,
        pending: &mut VecDeque<Intent>,
    ) {
        let post = Arc::clone(&self.post);
        let started = self.engine.clone().and_then(|engine| {
            thread::Builder::new()
                .name("related-job-preflight".into())
                .spawn(move || {
                    let result = resolve(&engine, &job);
                    post(Route::Related(request), Delivery::Related(result));
                })
                .map_err(|error| error.to_string())
        });
        if let Err(message) = started {
            pending.push_back(Intent::RelatedPrepared {
                request,
                result: Err(FieldError::new("engine", message)),
            });
        }
    }
}

fn resolve(engine: &Path, job: &JobSpec) -> RelatedResolution {
    let identity = job.identity.as_ref().ok_or_else(|| {
        FieldError::new(
            "identity",
            "The current engine does not provide an option identity.",
        )
    })?;
    let mut connection = Connection::open(engine, identity)?;
    let facts = match connection.ask(
        1,
        Command::Probe {
            source: job.source.clone(),
            video_stream: Some(job.video_stream),
        },
    )? {
        Event::Probed { facts } => facts,
        Event::Failed { failure } => return Err(FieldError::new("source", failure.message)),
        _ => {
            return Err(FieldError::new(
                "source",
                "The engine did not return source facts.",
            ));
        }
    };
    if facts.fingerprint != job.fingerprint || facts.video_stream != job.video_stream {
        return Err(FieldError::new(
            "source",
            "The source or its selected track changed. Add the source again to use it.",
        ));
    }
    let policy = connection.resolve(2, identity.schema, &facts, &job.settings, &|| true)?;
    Ok((Box::new(facts), policy))
}
