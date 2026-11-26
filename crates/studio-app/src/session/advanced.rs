// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Advanced queries use one background client per open editor. Its input
//! slot keeps only the newest generation. The background thread owns the
//! worker and handles all process and pipe operations.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use studio_domain::job::SourceFacts;
use studio_domain::options::{
    EffectiveOption, FieldError, MAX_OPTIONS_PAGE_ITEMS, OptionDescriptor, OptionRequest,
};
use studio_domain::protocol::{BuildIdentity, Command, Event, OptionsIdentity};

use crate::engine::{AdvancedReply, Delivery, Role, Worker};

use super::{Outcome, Post, Route, Session, ending_text};

const QUERY_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_OPTIONS: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Input {
    identity: OptionsIdentity,
    source: Option<SourceFacts>,
    settings: OptionRequest,
}

struct Request {
    generation: u64,
    want_catalog: bool,
    preview: bool,
    input: Input,
}

#[derive(Default)]
struct Latest {
    request: Mutex<Option<Request>>,
    generation: AtomicU64,
}

struct Client {
    wake: SyncSender<()>,
    latest: Arc<Latest>,
}

impl Client {
    fn start(engine: PathBuf, post: Post) -> std::io::Result<Self> {
        let (wake, incoming) = sync_channel(1);
        let latest = Arc::<Latest>::default();
        let queries = Arc::clone(&latest);
        thread::Builder::new()
            .name("advanced-options".into())
            .spawn(move || serve(&engine, &post, &queries, &incoming))?;
        Ok(Self { wake, latest })
    }

    fn submit(&self, request: Request) {
        self.latest
            .generation
            .store(request.generation, Ordering::Relaxed);
        *self
            .latest
            .request
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(request);
        // One wake-up is enough because the request is read from the slot.
        let _ = self.wake.try_send(());
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.latest.generation.store(0, Ordering::Relaxed);
    }
}

#[derive(Default)]
pub(super) struct Advanced {
    client: Option<Client>,
    open: bool,
    generation: u64,
    input: Option<Input>,
    catalog: Vec<OptionDescriptor>,
    catalog_identity: Option<OptionsIdentity>,
    policy: Vec<EffectiveOption>,
    error: Option<FieldError>,
    pending: bool,
    preview: Option<PresetPreview>,
}

#[derive(Default)]
struct PresetPreview {
    policy: Vec<EffectiveOption>,
    error: Option<FieldError>,
    pending: bool,
}

impl Session {
    /// Refresh the open Advanced editor after the controller debounce.
    /// An unchanged input reuses its last result. Closing the editor
    /// invalidates pending results and drops the background client.
    pub fn advanced_refresh(&mut self, open: bool) -> Outcome {
        if !open || self.shut_down {
            return self.close_advanced();
        }
        if self.advanced.open && self.advanced_matches() {
            return Outcome::nothing();
        }
        self.advanced.open = true;
        self.advanced.preview = None;
        self.advanced.policy.clear();
        self.advanced.error = None;
        self.advanced.pending = false;
        self.advanced.generation += 1;
        let identity = self.current_identity().cloned();
        if self.advanced.catalog_identity != identity {
            self.advanced.catalog.clear();
            self.advanced.catalog_identity = None;
        }
        let Some(identity) = identity else {
            self.advanced.client = None;
            self.advanced.input = None;
            self.advanced.error = Some(engine_error(
                "The engine does not report an encoder option schema.",
            ));
            return redraw();
        };
        let (settings, source) = self.current_advanced_settings();
        let input = Input {
            identity,
            source: source.cloned(),
            settings: settings.clone(),
        };
        self.advanced.input = Some(input.clone());
        if self.advanced.client.is_none() {
            let client = self
                .engine
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|engine| {
                    Client::start(engine.clone(), Arc::clone(&self.post))
                        .map_err(|error| error.to_string())
                });
            match client {
                Ok(client) => self.advanced.client = Some(client),
                Err(message) => {
                    self.advanced.error = Some(engine_error(message));
                    return redraw();
                }
            }
        }
        if let Some(client) = &self.advanced.client {
            client.submit(Request {
                generation: self.advanced.generation,
                want_catalog: self.advanced.catalog_identity.is_none(),
                preview: false,
                input,
            });
            self.advanced.pending = true;
        }
        redraw()
    }

    /// The descriptors for the engine currently selected by the app.
    #[must_use]
    pub fn advanced_catalog(&self) -> &[OptionDescriptor] {
        if self.advanced.catalog_identity.as_ref() == self.current_identity() {
            &self.advanced.catalog
        } else {
            &[]
        }
    }

    /// The latest complete effective policy for the current editor input.
    #[must_use]
    pub fn advanced_policy(&self) -> &[EffectiveOption] {
        if self.state.settings_view().read_only {
            return self
                .state
                .selected()
                .and_then(crate::queue::Item::attempt)
                .map_or(&[], |attempt| attempt.policy.as_slice());
        }
        if self.advanced_matches() {
            &self.advanced.policy
        } else {
            &[]
        }
    }

    /// The validation or engine error for the current editor input.
    #[must_use]
    pub fn advanced_error(&self) -> Option<&FieldError> {
        if self.advanced_matches() {
            self.advanced.error.as_ref()
        } else {
            None
        }
    }

    /// Whether the open editor awaits an answer for the current input.
    #[must_use]
    pub fn advanced_pending(&self) -> bool {
        self.advanced.open && (!self.advanced_matches() || self.advanced.pending)
    }

    /// Resolve proposed settings without changing the editor or the queue.
    /// The editor retains its existing policy while the proposal resolves.
    pub fn advanced_preview(&mut self, candidate: &OptionRequest) -> Outcome {
        if !self.advanced.open || self.shut_down {
            return Outcome::nothing();
        }
        let mut preview = PresetPreview::default();
        let source = self.current_advanced_settings().1;
        if source.is_none() {
            preview.error = Some(FieldError::new(
                "source",
                "Open a source before previewing a preset.",
            ));
        } else if self.advanced_pending() {
            preview.error = Some(engine_error("Wait for the current option check."));
        } else if let (Some(client), Some(input)) = (&self.advanced.client, &self.advanced.input) {
            self.advanced.generation += 1;
            preview.pending = true;
            client.submit(Request {
                generation: self.advanced.generation,
                want_catalog: false,
                preview: true,
                input: Input {
                    settings: candidate.clone(),
                    ..input.clone()
                },
            });
        } else {
            preview.error = Some(engine_error(
                "The engine could not check the proposed settings.",
            ));
        }
        self.advanced.preview = Some(preview);
        redraw()
    }

    /// The proposed settings' complete effective policy.
    #[must_use]
    pub fn advanced_preview_policy(&self) -> &[EffectiveOption] {
        self.current_preview()
            .map_or(&[], |preview| preview.policy.as_slice())
    }

    /// The proposed settings' validation or engine error.
    #[must_use]
    pub fn advanced_preview_error(&self) -> Option<&FieldError> {
        self.current_preview()?.error.as_ref()
    }

    /// Whether the proposed settings await an answer.
    #[must_use]
    pub fn advanced_preview_pending(&self) -> bool {
        self.current_preview()
            .is_some_and(|preview| preview.pending)
    }

    /// Discard a preset preview without changing the editor or its policy.
    pub fn advanced_cancel_preview(&mut self) -> Outcome {
        let Some(preview) = self.advanced.preview.take() else {
            return Outcome::nothing();
        };
        if preview.pending {
            self.advanced.generation += 1;
            if let Some(client) = &self.advanced.client {
                client.latest.generation.store(0, Ordering::Relaxed);
            }
        }
        redraw()
    }

    fn current_preview(&self) -> Option<&PresetPreview> {
        if self.advanced_matches() {
            self.advanced.preview.as_ref()
        } else {
            None
        }
    }

    pub(super) fn close_advanced(&mut self) -> Outcome {
        if !self.advanced.open {
            return Outcome::nothing();
        }
        self.advanced.open = false;
        self.advanced.preview = None;
        self.advanced.client = None;
        self.advanced.input = None;
        self.advanced.pending = false;
        self.advanced.policy.clear();
        self.advanced.error = None;
        redraw()
    }

    pub(super) fn deliver_advanced(&mut self, generation: u64, delivery: Delivery) -> Outcome {
        if !self.advanced.open || self.advanced.generation != generation || !self.advanced_matches()
        {
            return Outcome::nothing();
        }
        let Delivery::Advanced(reply) = delivery else {
            return Outcome::nothing();
        };
        if reply.preview {
            let Some(preview) = self.advanced.preview.as_mut() else {
                return Outcome::nothing();
            };
            preview.pending = false;
            match reply.policy {
                Ok(policy) => preview.policy = policy,
                Err(error) => preview.error = Some(error),
            }
            return redraw();
        }
        if let Some(catalog) = reply.catalog {
            self.advanced.catalog = catalog;
            self.advanced.catalog_identity = self.current_identity().cloned();
        }
        self.advanced.pending = false;
        match reply.policy {
            Ok(policy) => self.advanced.policy = policy,
            Err(error) => self.advanced.error = Some(error),
        }
        redraw()
    }

    fn current_identity(&self) -> Option<&OptionsIdentity> {
        self.state.capabilities()?.options.as_ref()
    }

    fn advanced_matches(&self) -> bool {
        let Some(input) = &self.advanced.input else {
            return self.current_identity().is_none();
        };
        let (settings, source) = self.current_advanced_settings();
        self.current_identity() == Some(&input.identity)
            && settings == &input.settings
            && source == input.source.as_ref()
    }

    fn current_advanced_settings(&self) -> (&OptionRequest, Option<&SourceFacts>) {
        let selected = self.state.selected();
        let locked = self.state.settings_view().read_only;
        let request = if locked {
            selected
                .and_then(crate::queue::Item::attempt)
                .map(|attempt| &attempt.job.settings)
        } else {
            None
        };
        let source = selected
            .filter(|_| !locked)
            .filter(|item| {
                !matches!(
                    item.state(),
                    crate::queue::ItemState::Probing | crate::queue::ItemState::Unreadable
                )
            })
            .and_then(crate::queue::Item::facts);
        (
            request.unwrap_or_else(|| self.state.option_request()),
            source,
        )
    }
}

fn redraw() -> Outcome {
    Outcome {
        advanced: true,
        ..Outcome::nothing()
    }
}

fn serve(engine: &Path, post: &Post, latest: &Latest, incoming: &Receiver<()>) {
    let mut connection = None;
    let mut catalog = None;
    while incoming.recv().is_ok() {
        let request = latest
            .request
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let Some(request) = request else { continue };
        let current = || latest.generation.load(Ordering::Relaxed) == request.generation;
        if !current() {
            continue;
        }
        let identity = &request.input.identity;
        if connection
            .as_ref()
            .is_none_or(|connection: &Connection| &connection.identity != identity)
        {
            connection = match Connection::open(engine, identity) {
                Ok(open) => Some(open),
                Err(error) => {
                    if current() {
                        post(
                            Route::Advanced(request.generation),
                            Delivery::Advanced(AdvancedReply {
                                preview: request.preview,
                                catalog: None,
                                policy: Err(error),
                            }),
                        );
                    }
                    continue;
                }
            };
            catalog = None;
        }
        let Some(worker) = &mut connection else {
            continue;
        };
        let reply = worker.query(&mut catalog, &request, &current);
        if reply
            .policy
            .as_ref()
            .is_err_and(|error| error.field == "engine")
        {
            connection = None;
        }
        if current() {
            post(
                Route::Advanced(request.generation),
                Delivery::Advanced(reply),
            );
        }
    }
}

pub(super) struct Connection {
    worker: Worker,
    replies: Receiver<Delivery>,
    identity: OptionsIdentity,
}

impl Connection {
    pub(super) fn open(engine: &Path, identity: &OptionsIdentity) -> Result<Self, FieldError> {
        let (reply, replies) = sync_channel(2);
        let worker = Worker::spawn(engine, Role::Worker, move |message| {
            let _ = reply.send(message);
        })
        .map_err(|error| engine_error(error.to_string()))?;
        let mut connection = Self {
            worker,
            replies,
            identity: identity.clone(),
        };
        let event = connection.ask(
            0,
            Command::Hello {
                client: BuildIdentity {
                    name: crate::APP_NAME.into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                },
            },
        )?;
        match event {
            Event::Hello { capabilities, .. }
                if capabilities.options.as_ref() == Some(identity) =>
            {
                Ok(connection)
            }
            _ => Err(engine_error(
                "The engine or encoder option schema changed. Restart Splot Studio.",
            )),
        }
    }

    fn query(
        &mut self,
        catalog: &mut Option<Vec<OptionDescriptor>>,
        request: &Request,
        current: &impl Fn() -> bool,
    ) -> AdvancedReply {
        let schema = request.input.identity.schema;
        let generation = request.generation;
        if request.want_catalog && catalog.is_none() {
            let descriptions = collect_pages(current, |first| {
                match self.ask(
                    generation,
                    Command::DescribeOptions {
                        schema,
                        first,
                        count: MAX_OPTIONS_PAGE_ITEMS,
                    },
                )? {
                    Event::OptionsDescribed {
                        first,
                        total,
                        options,
                    } => Ok((first, total, options)),
                    event => Err(reply_error(event)),
                }
            });
            match descriptions {
                Ok(descriptions) => *catalog = Some(descriptions),
                Err(error) => {
                    return AdvancedReply {
                        preview: request.preview,
                        catalog: None,
                        policy: Err(error),
                    };
                }
            }
        }
        let policy = match &request.input.source {
            None => Ok(Vec::new()),
            Some(source) => {
                self.resolve(generation, schema, source, &request.input.settings, current)
            }
        };
        AdvancedReply {
            preview: request.preview,
            catalog: if request.want_catalog {
                catalog.clone()
            } else {
                None
            },
            policy,
        }
    }

    pub(super) fn resolve(
        &mut self,
        generation: u64,
        schema: u32,
        source: &SourceFacts,
        settings: &OptionRequest,
        current: &impl Fn() -> bool,
    ) -> Result<Vec<EffectiveOption>, FieldError> {
        collect_pages(current, |first| {
            let event = self.ask(
                generation,
                Command::ResolveOptions {
                    schema,
                    source: Box::new(source.clone()),
                    settings: settings.clone(),
                    first,
                    count: MAX_OPTIONS_PAGE_ITEMS,
                },
            )?;
            match event {
                Event::OptionsResolved {
                    first,
                    total,
                    values,
                } => Ok((first, total, values)),
                event => Err(reply_error(event)),
            }
        })
    }

    pub(super) fn ask(&mut self, id: u64, command: Command) -> Result<Event, FieldError> {
        self.worker
            .send(id, command)
            .map_err(|error| engine_error(error.to_string()))?;
        match self.replies.recv_timeout(QUERY_TIMEOUT) {
            Ok(Delivery::Event(reply)) if reply.request_id == id => Ok(reply.body),
            Ok(Delivery::Ended(ending)) => Err(engine_error(ending_text(&ending))),
            Ok(_) => Err(engine_error("The engine sent an unexpected option reply.")),
            Err(error) => Err(engine_error(format!(
                "The engine did not answer the option request. {error}"
            ))),
        }
    }
}

fn collect_pages<T>(
    current: &impl Fn() -> bool,
    mut ask: impl FnMut(u32) -> Result<(u32, u32, Vec<T>), FieldError>,
) -> Result<Vec<T>, FieldError> {
    let mut rows = Vec::new();
    let mut expected_total = None;
    while current() {
        let (first, total, page) =
            ask(u32::try_from(rows.len()).map_err(|error| engine_error(error.to_string()))?)?;
        if first as usize != rows.len()
            || total as usize > MAX_OPTIONS
            || expected_total.is_some_and(|expected| total != expected)
            || page.is_empty()
            || page.len() > MAX_OPTIONS_PAGE_ITEMS as usize
            || rows.len() + page.len() > total as usize
        {
            return Err(engine_error("The engine sent an invalid option page."));
        }
        rows.extend(page);
        if rows.len() == total as usize {
            return Ok(rows);
        }
        expected_total = Some(total);
    }
    Err(engine_error(
        "A newer option request replaced this request.",
    ))
}

fn reply_error(event: Event) -> FieldError {
    match event {
        Event::OptionsFailed { error } => error,
        Event::Error { message, .. } => engine_error(message),
        _ => engine_error("The engine sent an unexpected option reply."),
    }
}

fn engine_error(message: impl Into<String>) -> FieldError {
    FieldError::new("engine", message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppState, Intent};
    use std::time::Instant;
    use studio_domain::options::OPTIONS_SCHEMA_VERSION;
    use studio_domain::protocol::Capabilities;

    #[test]
    fn stale_validation_cannot_reappear_after_an_edit_or_close() {
        let mut session = Session::new(
            AppState::default(),
            Err("No engine process for this test.".into()),
            Arc::new(|_, _| {}),
            None,
        );
        let now = Instant::now();
        session.dispatch(
            Intent::EngineAnswered(Ok(Capabilities {
                options: Some(OptionsIdentity {
                    schema: OPTIONS_SCHEMA_VERSION,
                    codec_revision: "test".into(),
                }),
                ..Capabilities::default()
            })),
            now,
        );
        session.advanced_refresh(true);
        let generation = session.advanced.generation;
        let reply = || {
            Delivery::Advanced(AdvancedReply {
                preview: false,
                catalog: None,
                policy: Err(FieldError::new("rate_control", "Invalid draft.")),
            })
        };
        assert!(
            session
                .deliver(Route::Advanced(generation), reply(), now)
                .advanced
        );
        assert_eq!(
            session.advanced_error().map(|error| error.field.as_str()),
            Some("rate_control")
        );

        session.dispatch(
            Intent::OptionEdited {
                key: "fast_decode".into(),
                value: Some(studio_domain::options::OptionValue::Bool(false)),
            },
            now,
        );
        assert!(session.advanced_pending());
        assert!(session.advanced_error().is_none());
        assert!(
            !session
                .deliver(Route::Advanced(generation), reply(), now)
                .advanced
        );

        session.advanced_refresh(false);
        assert!(!session.advanced_pending());
        assert!(
            !session
                .deliver(Route::Advanced(generation), reply(), now)
                .advanced
        );
    }

    #[test]
    fn pages_keep_order_and_stop_at_total() {
        let pages = collect_pages(&|| true, |first| match first {
            0 => Ok((first, 3, vec!["width", "height"])),
            2 => Ok((first, 3, vec!["bit_depth"])),
            _ => Err(engine_error("Unexpected page request.")),
        });
        assert_eq!(pages, Ok(vec!["width", "height", "bit_depth"]));
    }

    #[test]
    fn invalid_or_superseded_pages_cannot_complete() {
        let invalid = collect_pages(&|| true, |_| Ok((1, 1, vec!["width"])));
        assert!(invalid.is_err());
        let superseded = collect_pages::<()>(&|| false, |_| unreachable!("request is stale"));
        assert!(superseded.is_err());
    }
}
