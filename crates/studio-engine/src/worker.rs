// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The worker session: framed commands in, framed events out.
//!
//! The caller's thread reads commands and never runs codec work, so it can
//! read `Cancel` while the encoder is busy. One thread resolves option
//! queries and one job thread runs the single job. One writer thread
//! owns the output stream so frames never interleave. A peer that stops
//! reading can block command replies. The job can still replace the progress
//! slot or append a stage or terminal event. With `Start.live`, the
//! [`crate::live`] thread decodes
//! the packets the job has written and replaces a one-picture slot, which
//! the writer empties only when no event is waiting; the job hands it
//! packets without ever waiting, and the thread ends with the job.
//!
//! A cancel (the `Cancel` command, the end of input or a failed output) is
//! cooperative: it sets the job's flag, which the job polls between frames
//! and packets, and cancels the encoder's token, which stops a codec call in
//! progress (one `receive_packet` codes several frames) at its next
//! internal check. The job then cleans up and ends as `Cancelled`, and the
//! worker keeps serving until the end of its input.
//!
//! As a last resort, a job that has not stopped [`CANCEL_GRACE`] after a
//! cancel is abandoned: the reader removes its temporary file, emits
//! `Cancelled`, waits for the writer and exits the process while the job
//! thread is still running, after killing every `FFmpeg` child
//! ([`crate::media::kill_all`]). Both happen under the lock that orders
//! the terminal event and the publication claim, so a forced cancel never
//! follows a claimed publication and nothing is published after it.

use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, Scope, ScopedJoinHandle};
use std::time::{Duration, Instant};

use splot_encode::CancelToken;
use studio_domain::job::{Failure, FailureCode, JobPurpose, JobSpec, SourceFacts};
use studio_domain::options::OptionRequest;
use studio_domain::preview::{
    MAX_PICTURE_HEIGHT, MAX_PICTURE_WIDTH, PictureBox, PictureFailure, PictureHeader,
};
use studio_domain::protocol::{
    Command, Envelope, ErrorCode, Event, ProtocolError, decode, read_frame, write_message,
    write_picture,
};
use thiserror::Error;

mod export;

use self::export::ExportRun;
use crate::job::{self, Control, JOB_STACK_BYTES, Stop};
use crate::{build_identity, capabilities, live, options, source};

/// The shortest gap between two progress events.
pub const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// How long a cancelled job may take to stop by itself before the worker
/// abandons it and exits. A backstop only: the encoder's token stops a
/// codec call in milliseconds (release, 720p and 1080p, presets 6 and 10:
/// at most 14 ms; upstream bounds it at about 160 ms), so this is reached
/// only if something ignores the cancel.
pub const CANCEL_GRACE: Duration = Duration::from_secs(3);

/// Command replies queued for a peer that does not read before the reader
/// stops taking commands. Job events do not count against it.
const REPLY_BACKLOG: usize = 64;

/// The session ended because the framing is unrecoverable or a stream failed.
///
/// A bad *message* is not an error here: it is answered with
/// [`Event::Error`] and the session continues.
#[derive(Debug, Error)]
pub enum SessionError {
    /// The command stream is broken: bad length, truncated frame, or I/O failure.
    #[error("cannot read command: {0}")]
    Read(#[source] ProtocolError),
    /// The event stream could not be written.
    #[error("cannot write event: {0}")]
    Write(#[source] ProtocolError),
}

/// Serves commands from `input` until it ends, then waits for a running job
/// to reach its terminal event (the end of input cancels it). It also waits
/// for the option resolver to finish accepted requests and for the writer to
/// send queued events.
///
/// Every frame gets one reply, echoing its `request_id` (0 when the frame
/// carries none), except `Start`, which is answered by its job's events, all
/// with the `Start`'s `request_id`. One job per session; a second `Start` and
/// a `Cancel` without a running job are refused with
/// [`ErrorCode::WrongState`].
///
/// A cancelled job that does not stop within [`CANCEL_GRACE`] (normally it
/// stops well before) is abandoned:
/// once its `Cancelled` event is written, this exits the process with the
/// status the session would have returned (0 unless a stream failed),
/// without waiting for the end of input.
///
/// # Errors
/// [`SessionError::Read`] on an invalid length prefix, a stream that ends
/// inside a frame, or an input failure (a running job is cancelled first);
/// [`SessionError::Write`] when the output fails (a running job is
/// cancelled, and later events are discarded).
pub fn run(input: &mut impl Read, output: impl Write + Send) -> Result<(), SessionError> {
    let outbox = Outbox::default();
    thread::scope(|scope| {
        let writer = scope.spawn(|| outbox.write_all(output));
        let (requests, pending) = sync_channel(1);
        let replies = &outbox;
        let resolver = scope.spawn(move || resolve_options(pending, replies));
        let (read, forced) = serve(input, &outbox, scope, &requests);
        drop(requests);
        if !forced {
            // Drain accepted queries before the writer closes.
            let _ = resolver.join();
        }
        outbox.close();
        let written = writer
            .join()
            .unwrap_or_else(|_| Err(ProtocolError::Io(std::io::Error::other("writer panicked"))));
        let result = read
            .map_err(SessionError::Read)
            .and(written.map_err(SessionError::Write));
        if forced {
            // The job thread is still inside the codec; leaving the scope
            // would join it. Its file is gone and its events are discarded.
            if let Err(error) = &result {
                let _ = writeln!(std::io::stderr(), "{}: {error}", crate::ENGINE_NAME);
            }
            // Exiting runs no destructors: the job's FFmpeg child would
            // outlive the worker.
            crate::media::kill_all();
            std::process::exit(i32::from(result.is_err()));
        }
        result
    })
}

/// Reads commands until the end of input, then cancels and joins the job.
/// The flag is `true` when the job was abandoned instead and must not be
/// joined.
fn serve<'scope>(
    input: &mut impl Read,
    outbox: &'scope Outbox,
    scope: &'scope Scope<'scope, '_>,
    requests: &SyncSender<Resolution>,
) -> (Result<(), ProtocolError>, bool) {
    let mut job: Option<ScopedJoinHandle<'scope, ()>> = None;
    let result = loop {
        let envelope = match next_command(input, outbox) {
            Ok(Some(command)) => command,
            Ok(None) => break Ok(()),
            Err(error) => break Err(error),
        };
        let id = envelope.request_id;
        match envelope.body {
            Command::Hello { .. } => outbox.reply(id, hello()),
            Command::DescribeOptions {
                schema,
                first,
                count,
            } => {
                outbox.reply_options(id, describe_options(schema, first, count));
            }
            Command::ResolveOptions {
                schema,
                source,
                settings,
                first,
                count,
            } => {
                let request = Resolution {
                    id,
                    schema,
                    source,
                    settings,
                    first,
                    count,
                };
                queue_resolution(requests, outbox, request);
            }
            // A probe runs here, in order with the other replies, bounded by
            // the media tools' timeout; the end of input is seen after it.
            Command::Probe {
                source,
                video_stream,
            } => {
                outbox.reply(id, probe(&source, video_stream));
            }
            Command::Start { job: spec, live } => {
                start(scope, outbox, id, spec, live, false, &mut job);
            }
            Command::StartChecked { job: spec, live } => {
                start(scope, outbox, id, spec, live, true, &mut job);
            }
            Command::ConfirmPolicy => {
                outbox.confirm(id);
            }
            Command::ExportPrepare { export } => {
                export::start(scope, outbox, id, *export, &mut job);
            }
            Command::ExportProceed => export::proceed(outbox, id),
            Command::ExportPublish => export::permit(outbox, id),
            Command::Cancel => {
                if cancel_job(outbox, id) {
                    return (Ok(()), true);
                }
            }
            Command::Picture { .. }
            | Command::Play { .. }
            | Command::PictureSample { .. }
            | Command::StopPlayback
            | Command::CloseDocumentPlayback { .. } => {
                outbox.reply(
                    id,
                    refusal(
                        ErrorCode::WrongState,
                        "Use the preview process for pictures.",
                    ),
                );
            }
            Command::AnalysisOpen { .. }
            | Command::AnalysisOpenProgressive { .. }
            | Command::AnalysisCancel { .. }
            | Command::AnalysisPage { .. }
            | Command::AnalysisLocate { .. }
            | Command::AnalysisBytes { .. }
            | Command::AnalysisSyntax { .. }
            | Command::AnalysisSyntaxPage { .. }
            | Command::AnalysisValidate { .. }
            | Command::AnalysisCancelValidation { .. }
            | Command::AnalysisFindings { .. }
            | Command::BatchCheckSettings { .. }
            | Command::BatchValidate { .. }
            | Command::BatchCancel { .. }
            | Command::BatchFindings { .. }
            | Command::BatchExport { .. }
            | Command::BatchForget { .. }
            | Command::AnalysisClose { .. } => {
                outbox.reply(
                    id,
                    refusal(
                        ErrorCode::WrongState,
                        "Use the inspect process for analysis.",
                    ),
                );
            }
        }
    };
    (result, stop_job(outbox, job))
}

fn cancel_job(outbox: &Outbox, id: u64) -> bool {
    let message = match outbox.cancel(id) {
        Cancel::Accepted => return outbox.settle(CANCEL_GRACE),
        Cancel::Inactive => "no job is running",
        Cancel::WrongExport => "Use the request ID of the running export.",
        Cancel::Publishing => "The export is publishing.",
    };
    outbox.reply(id, refusal(ErrorCode::WrongState, message));
    false
}

fn stop_job(outbox: &Outbox, job: Option<ScopedJoinHandle<'_, ()>>) -> bool {
    // The end of input, or an unreadable one, means nobody can cancel later.
    outbox.stop(&outbox.lock());
    if outbox.settle(CANCEL_GRACE) {
        return true;
    }
    if let Some(handle) = job {
        // The job body cannot panic past `catch_unwind`.
        let _ = handle.join();
    }
    false
}

fn next_command(
    input: &mut impl Read,
    outbox: &Outbox,
) -> Result<Option<Envelope<Command>>, ProtocolError> {
    while let Some(frame) = read_frame(input)? {
        match decode::<Command>(&frame) {
            Ok(command) => return Ok(Some(command)),
            Err(error) => {
                let code = match error {
                    ProtocolError::IncompatibleVersion { .. } => ErrorCode::IncompatibleVersion,
                    _ => ErrorCode::Malformed,
                };
                outbox.reply(request_id_of(&frame), refusal(code, error.to_string()));
            }
        }
    }
    Ok(None)
}

struct Resolution {
    id: u64,
    schema: u32,
    source: Box<SourceFacts>,
    settings: OptionRequest,
    first: u32,
    count: u32,
}

fn probe(source: &studio_domain::job::WirePath, video_stream: Option<u32>) -> Event {
    match source.to_path_buf() {
        Ok(path) => match source::probe(&path, video_stream) {
            Ok(facts) => Event::Probed { facts },
            Err(stop) => failed(stop),
        },
        Err(e) => failed(Stop::failed(FailureCode::InvalidSource, e.to_string())),
    }
}

fn hello() -> Event {
    Event::Hello {
        engine: build_identity(),
        capabilities: studio_domain::protocol::Capabilities {
            options: Some(job::options_identity()),
            ..capabilities()
        },
    }
}

fn describe_options(schema: u32, first: u32, count: u32) -> Event {
    match options::describe(schema, first, count) {
        Ok((total, options)) => Event::OptionsDescribed {
            first,
            total,
            options,
        },
        Err(error) => Event::OptionsFailed { error },
    }
}

fn queue_resolution(requests: &SyncSender<Resolution>, outbox: &Outbox, request: Resolution) {
    let id = request.id;
    if let Err(error) = requests.try_send(request) {
        let event = match error {
            TrySendError::Full(_) => {
                refusal(ErrorCode::Busy, "Wait for a pending encoder option reply.")
            }
            TrySendError::Disconnected(_) => refusal(
                ErrorCode::WrongState,
                "The encoder option resolver stopped.",
            ),
        };
        outbox.reply(id, event);
    }
}

fn resolve_options(pending: Receiver<Resolution>, outbox: &Outbox) {
    for request in pending {
        let Resolution {
            id,
            schema,
            source,
            settings,
            first,
            count,
        } = request;
        let event = catch_unwind(AssertUnwindSafe(|| {
            match options::resolve(schema, &source, &settings, first, count) {
                Ok((total, values)) => Event::OptionsResolved {
                    first,
                    total,
                    values,
                },
                Err(error) => Event::OptionsFailed { error },
            }
        }))
        .unwrap_or_else(|_| {
            refusal(
                ErrorCode::WrongState,
                "The encoder option query stopped unexpectedly.",
            )
        });
        outbox.reply_options(id, event);
    }
}

fn start<'scope>(
    scope: &'scope Scope<'scope, '_>,
    outbox: &'scope Outbox,
    id: u64,
    spec: JobSpec,
    live: Option<PictureBox>,
    checked: bool,
    job: &mut Option<ScopedJoinHandle<'scope, ()>>,
) {
    if job.is_some() {
        outbox.reply(
            id,
            refusal(
                ErrorCode::WrongState,
                "this worker has already started its job",
            ),
        );
        return;
    }
    if spec.identity.as_ref() != Some(&job::options_identity()) {
        outbox.reply(
            id,
            failed(Stop::failed(
                FailureCode::InvalidSettings,
                "identity: Use the option schema and codec revision reported by this engine.",
            )),
        );
        return;
    }
    match spawn_job(scope, outbox, id, spec, live, checked) {
        Ok(handle) => *job = Some(handle),
        Err(error) => outbox.job_end(
            id,
            failed(Stop::failed(
                FailureCode::Encoder,
                format!("cannot start the job thread: {error}"),
            )),
        ),
    }
}

fn spawn_job<'scope>(
    scope: &'scope Scope<'scope, '_>,
    outbox: &'scope Outbox,
    id: u64,
    spec: JobSpec,
    live: Option<PictureBox>,
    checked: bool,
) -> std::io::Result<ScopedJoinHandle<'scope, ()>> {
    outbox.begin_job(id, checked);
    if spec.purpose == JobPurpose::Encode
        && let Some(size) = live
    {
        start_live(scope, outbox, id, size);
    }
    thread::Builder::new()
        .name("studio-job".to_owned())
        .stack_size(JOB_STACK_BYTES)
        .spawn_scoped(scope, move || {
            let mut report = |event: Event| match event {
                Event::Progress { .. } => outbox.progress(id, event),
                event => outbox.job_event(id, event),
            };
            let end = catch_unwind(AssertUnwindSafe(|| job::run(&spec, outbox, &mut report)))
                .unwrap_or_else(|_| {
                    failed(Stop::failed(
                        FailureCode::Encoder,
                        "the job stopped unexpectedly (panic)",
                    ))
                });
            outbox.job_end(id, end);
        })
}

/// Starts the live decoder for the job. Without it the job runs as it
/// would without `live`, and the app is told why there are no pictures.
fn start_live<'scope>(
    scope: &'scope Scope<'scope, '_>,
    outbox: &'scope Outbox,
    id: u64,
    size: PictureBox,
) {
    let unavailable = |code, message: String| {
        outbox.job_event(
            id,
            Event::PictureUnavailable {
                generation: 0,
                code,
                message,
            },
        );
    };
    if !size.is_valid() {
        return unavailable(
            PictureFailure::InvalidRequest,
            format!(
                "the live picture box {}x{} is outside 1x1 to {MAX_PICTURE_WIDTH}x{MAX_PICTURE_HEIGHT}",
                size.width, size.height
            ),
        );
    }
    let (feed, units) = live::Feed::new();
    let spawned = thread::Builder::new()
        .name("studio-live".to_owned())
        .spawn_scoped(scope, move || {
            live::run(&units, size, &|header, pixels| {
                outbox.live_picture(header, pixels);
            });
        });
    match spawned {
        Ok(_) => *outbox.feed() = Some(feed),
        Err(error) => unavailable(
            PictureFailure::Unreadable,
            format!("cannot start the live picture thread: {error}"),
        ),
    }
}

fn refusal(code: ErrorCode, message: impl Into<String>) -> Event {
    Event::Error {
        code,
        message: message.into(),
    }
}

fn failed(stop: Stop) -> Event {
    match stop {
        Stop::Failed(failure) => Event::Failed { failure },
        // Probes and thread starts are not cancellable.
        Stop::Cancelled => Event::Failed {
            failure: Failure {
                code: FailureCode::Encoder,
                message: "cancelled".to_owned(),
            },
        },
    }
}

/// Best-effort `request_id` of a frame that did not decode as a command.
pub(crate) fn request_id_of(frame: &[u8]) -> u64 {
    serde_json::from_slice::<serde_json::Value>(frame)
        .ok()
        .and_then(|value| value.get("request_id")?.as_u64())
        .unwrap_or(0)
}

/// Events waiting for the writer thread, and the job's cancel flag.
#[derive(Debug, Default)]
struct Outbox {
    state: Mutex<State>,
    changed: Condvar,
    /// Set by `Cancel`, the end of input, or a failed output.
    cancel: AtomicBool,
    /// The job's packets go here while live pictures are wanted. Dropped
    /// when the job ends, which ends the live thread.
    live: Mutex<Option<live::Feed>>,
}

#[derive(Debug, Default)]
struct State {
    /// Events that are never dropped, in order.
    queue: VecDeque<Envelope<Event>>,
    /// The newest progress, not yet written. Replaced, never queued.
    progress: Option<Envelope<Event>>,
    /// The newest live picture, not yet written. Replaced, never queued.
    picture: Option<(PictureHeader, Vec<u8>)>,
    job: JobState,
    policy: PolicyCheck,
    /// The `request_id` of the job's `Start`.
    job_id: u64,
    /// The job's temporary file, while it exists.
    temp: Option<PathBuf>,
    export: Option<ExportRun>,
    /// Stops the job's encoder, once it exists.
    encoder: Option<CancelToken>,
    /// The job has claimed publication; it can no longer be cancelled.
    publishing: bool,
    /// No more events will be added.
    closed: bool,
    /// The output failed; events are discarded.
    broken: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum JobState {
    #[default]
    None,
    Running,
    Ended,
}

enum Cancel {
    Inactive,
    WrongExport,
    Publishing,
    Accepted,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum PolicyCheck {
    #[default]
    Disabled,
    Reporting,
    Waiting,
    Confirmed,
}

impl Outbox {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues a command reply, waiting while the peer is far behind.
    fn reply(&self, request_id: u64, event: Event) {
        let mut state = self.lock();
        while state.queue.len() >= REPLY_BACKLOG && !state.broken {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        state.push(Envelope::new(request_id, event));
        drop(state);
        self.changed.notify_all();
    }

    /// Checks the full frame size before it reaches the writer.
    fn reply_options(&self, request_id: u64, event: Event) {
        let envelope = Envelope::new(request_id, event);
        let event = match write_message(&mut std::io::sink(), &envelope) {
            Ok(()) => envelope.body,
            Err(ProtocolError::InvalidLength { .. }) => refusal(
                ErrorCode::PayloadLimit,
                "The option reply exceeds 64 KiB. Request fewer options.",
            ),
            Err(_) => refusal(ErrorCode::Malformed, "Cannot encode the option reply."),
        };
        self.reply(request_id, event);
    }

    fn feed(&self) -> MutexGuard<'_, Option<live::Feed>> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Offers a live picture. Never waits; replaces one not yet written.
    fn live_picture(&self, header: PictureHeader, pixels: Vec<u8>) {
        let mut state = self.lock();
        if state.job == JobState::Running && !state.broken {
            state.picture = Some((header, pixels));
        }
        drop(state);
        self.changed.notify_all();
    }

    fn begin_job(&self, request_id: u64, checked: bool) {
        let mut state = self.lock();
        state.job = JobState::Running;
        state.job_id = request_id;
        if checked {
            state.policy = PolicyCheck::Reporting;
        }
    }

    fn confirm(&self, request_id: u64) {
        let mut state = self.lock();
        if state.job != JobState::Running
            || state.job_id != request_id
            || state.policy != PolicyCheck::Waiting
            || self.cancel.load(Ordering::Relaxed)
        {
            drop(state);
            self.reply(
                request_id,
                refusal(ErrorCode::WrongState, "Wait for the complete job policy."),
            );
            return;
        }
        state.policy = PolicyCheck::Confirmed;
        drop(state);
        self.changed.notify_all();
    }

    /// Queues a stage event after any pending progress, so order holds.
    fn job_event(&self, request_id: u64, event: Event) {
        let mut state = self.lock();
        if state.job == JobState::Running {
            if state.policy == PolicyCheck::Reporting
                && let Event::JobPolicy {
                    first,
                    total,
                    values,
                } = &event
                && *first as usize + values.len() == *total as usize
            {
                state.policy = PolicyCheck::Waiting;
            }
            state.flush_progress();
            state.push(Envelope::new(request_id, event));
        }
        drop(state);
        self.changed.notify_all();
    }

    /// Replaces the pending progress. Never waits.
    fn progress(&self, request_id: u64, event: Event) {
        let mut state = self.lock();
        if state.job == JobState::Running && !state.broken {
            state.progress = Some(Envelope::new(request_id, event));
        }
        drop(state);
        self.changed.notify_all();
    }

    /// Queues the job's terminal event; nothing of the job follows it. A job
    /// already abandoned by [`Self::settle`] has had its terminal event.
    fn job_end(&self, request_id: u64, event: Event) {
        let mut state = self.lock();
        state.end_job(request_id, event);
        drop(state);
        // Ends the live thread.
        self.feed().take();
        self.changed.notify_all();
    }

    /// After a cancel, waits up to `grace` for the job to end. A job that has
    /// claimed publication is waited for until it completes. Otherwise the
    /// job is abandoned: its temporary file is removed and `Cancelled` is
    /// queued as its terminal event. Returns whether it was abandoned.
    fn settle(&self, grace: Duration) -> bool {
        let deadline = Instant::now() + grace;
        let mut state = self.lock();
        loop {
            if state.job != JobState::Running {
                return false;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if !state.publishing && left.is_zero() {
                break;
            }
            let wait = if state.publishing { grace } else { left };
            state = self
                .changed
                .wait_timeout(state, wait)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        if let Some(temp) = state.temp.take() {
            // Already gone is fine; anything else cannot be helped here.
            let _ = fs::remove_file(temp);
        }
        state.progress = None;
        state.picture = None;
        let id = state.job_id;
        let event = state
            .export
            .take()
            .map_or(Event::Cancelled, ExportRun::cancel);
        state.end_job(id, event);
        drop(state);
        self.changed.notify_all();
        true
    }

    /// Sets the cancel flag and stops a codec call in progress. Called under
    /// the lock (`state`), so [`Control::hold_encoder`] cannot miss it.
    fn stop(&self, state: &State) {
        if state.publishing && state.export.is_some() {
            return;
        }
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(token) = &state.encoder {
            token.cancel();
        }
        self.changed.notify_all();
    }

    /// Requests cancellation and acknowledges it, unless no job is running.
    /// Decided under the lock that orders the terminal event, so an
    /// acknowledgement never follows it.
    fn cancel(&self, request_id: u64) -> Cancel {
        let mut state = self.lock();
        if state.job != JobState::Running {
            return Cancel::Inactive;
        }
        if state.export.is_some() {
            if state.job_id != request_id {
                return Cancel::WrongExport;
            }
            if state.publishing {
                return Cancel::Publishing;
            }
        }
        self.stop(&state);
        state.flush_progress();
        state.push(Envelope::new(request_id, Event::CancelRequested));
        drop(state);
        self.changed.notify_all();
        Cancel::Accepted
    }

    fn close(&self) {
        self.lock().closed = true;
        self.changed.notify_all();
    }

    /// The writer thread: writes until closed and empty. Progress is held
    /// back to one per [`PROGRESS_INTERVAL`]; the rest is written at once,
    /// a live picture only when no event is waiting.
    fn write_all(&self, mut output: impl Write) -> Result<(), ProtocolError> {
        let mut next_progress = Instant::now();
        let mut state = self.lock();
        loop {
            let envelope = if let Some(envelope) = state.queue.pop_front() {
                envelope
            } else if let Some(envelope) =
                state.progress.take_if(|_| Instant::now() >= next_progress)
            {
                next_progress = Instant::now() + PROGRESS_INTERVAL;
                envelope
            } else if let Some((header, pixels)) = state.picture.take() {
                let id = state.job_id;
                drop(state);
                self.changed.notify_all();
                if let Err(error) = write_picture(&mut output, id, header, &pixels) {
                    return Err(self.broke(error));
                }
                state = self.lock();
                continue;
            } else if state.progress.is_some() {
                let wait = next_progress.saturating_duration_since(Instant::now());
                state = self
                    .changed
                    .wait_timeout(state, wait)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
                continue;
            } else if state.closed {
                return Ok(());
            } else {
                state = self
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
                continue;
            };
            drop(state);
            self.changed.notify_all();
            if let Err(error) = write_message(&mut output, &envelope) {
                return Err(self.broke(error));
            }
            state = self.lock();
        }
    }

    /// The output failed: discards every event and cancels the job.
    fn broke(&self, error: ProtocolError) -> ProtocolError {
        let mut state = self.lock();
        state.broken = true;
        state.queue.clear();
        state.progress = None;
        state.picture = None;
        // Nobody can hear the job any more.
        self.stop(&state);
        drop(state);
        self.changed.notify_all();
        error
    }
}

impl Control for Outbox {
    fn cancel_flag(&self) -> &AtomicBool {
        &self.cancel
    }

    fn await_policy_confirmation(&self) -> bool {
        let mut state = self.lock();
        while state.job == JobState::Running
            && !self.cancel.load(Ordering::Relaxed)
            && matches!(state.policy, PolicyCheck::Reporting | PolicyCheck::Waiting)
        {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        state.job == JobState::Running && !self.cancel.load(Ordering::Relaxed)
    }

    /// Creates the file under the lock, so an abandoning reader always
    /// knows of it, and never after the job was abandoned.
    fn hold_temp(&self, create: &mut dyn FnMut() -> Option<PathBuf>) {
        let mut state = self.lock();
        if state.job == JobState::Running && !self.cancel.load(Ordering::Relaxed) {
            state.temp = create();
        }
    }

    fn release_temp(&self) {
        self.lock().temp = None;
    }

    fn hold_encoder(&self, token: CancelToken) {
        let mut state = self.lock();
        if self.cancel.load(Ordering::Relaxed) {
            token.cancel();
        }
        state.encoder = Some(token);
    }

    fn claim_publish(&self) -> bool {
        let mut state = self.lock();
        let free = state.job == JobState::Running && !self.cancel.load(Ordering::Relaxed);
        state.publishing = free;
        free
    }

    fn stream_started(&self, stream: live::Stream) {
        if let Some(feed) = self.feed().as_mut() {
            feed.stream(stream);
        }
    }

    fn packet_written(&self, unit: &[u8]) {
        if let Some(feed) = self.feed().as_mut() {
            feed.packet(unit);
        }
    }
}

impl State {
    fn end_job(&mut self, request_id: u64, event: Event) {
        if self.job != JobState::Ended {
            self.flush_progress();
            self.push(Envelope::new(request_id, event));
            self.job = JobState::Ended;
            self.picture = None;
        }
    }

    fn push(&mut self, envelope: Envelope<Event>) {
        if !self.broken {
            self.queue.push_back(envelope);
        }
    }

    fn flush_progress(&mut self) {
        if let Some(progress) = self.progress.take() {
            self.push(progress);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::mpsc;

    use studio_domain::protocol::{BuildIdentity, PROTOCOL_VERSION, read_message};

    use super::*;

    fn hello(id: u64) -> Envelope<Command> {
        let client = BuildIdentity {
            name: "t".into(),
            version: "0".into(),
        };
        Envelope::new(id, Command::Hello { client })
    }

    fn serve_bytes(input: &[u8]) -> (Result<(), SessionError>, Vec<u8>) {
        let mut out = Vec::new();
        let result = run(&mut { input }, &mut out);
        (result, out)
    }

    #[test]
    fn hello_reports_both_outputs() {
        let mut input = Vec::new();
        assert!(write_message(&mut input, &hello(3)).is_ok());
        let (result, out) = serve_bytes(&input);
        assert!(result.is_ok());
        let reply = read_message::<Event>(&mut out.as_slice());
        assert!(matches!(
            reply,
            Ok(Some(Envelope { request_id: 3, body: Event::Hello { ref capabilities, .. }, .. }))
                if capabilities.encode_outputs.len() == 2
        ));
    }

    #[test]
    fn bad_message_is_answered_and_session_continues() {
        let bad = format!(
            r#"{{"protocol_version":{PROTOCOL_VERSION},"request_id":8,"body":{{"type":"encode"}}}}"#
        );
        let bad = bad.as_bytes();
        let mut input = Vec::new();
        input.extend_from_slice(&u32::try_from(bad.len()).unwrap_or(0).to_le_bytes());
        input.extend_from_slice(bad);
        assert!(write_message(&mut input, &hello(9)).is_ok());
        let (result, out) = serve_bytes(&input);
        assert!(result.is_ok());
        let mut reader = out.as_slice();
        assert!(matches!(
            read_message::<Event>(&mut reader),
            Ok(Some(Envelope {
                request_id: 8,
                body: Event::Error {
                    code: ErrorCode::Malformed,
                    ..
                },
                ..
            }))
        ));
        assert!(matches!(
            read_message::<Event>(&mut reader),
            Ok(Some(Envelope {
                request_id: 9,
                body: Event::Hello { .. },
                ..
            }))
        ));
    }

    #[test]
    fn cancel_without_a_job_is_refused() {
        let mut input = Vec::new();
        assert!(write_message(&mut input, &Envelope::new(4, Command::Cancel)).is_ok());
        let (result, out) = serve_bytes(&input);
        assert!(result.is_ok());
        assert!(matches!(
            read_message::<Event>(&mut out.as_slice()),
            Ok(Some(Envelope {
                request_id: 4,
                body: Event::Error {
                    code: ErrorCode::WrongState,
                    ..
                },
                ..
            }))
        ));
    }

    #[test]
    fn truncated_frame_ends_the_session_with_nothing_written() {
        let (result, out) = serve_bytes(&[10, 0, 0, 0, b'{']);
        assert!(matches!(
            result,
            Err(SessionError::Read(ProtocolError::Truncated))
        ));
        assert_eq!(out, Vec::<u8>::new());
    }

    /// A writer that blocks until the test lets it go.
    struct Gate(mpsc::Receiver<()>, Vec<u8>);

    impl Write for &mut Gate {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.1.is_empty() {
                let _ = self.0.recv();
            }
            self.1.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The job side never waits on a stuck writer, progress collapses to the
    /// newest value, and the terminal event is still delivered in order.
    #[test]
    fn a_stuck_writer_blocks_neither_job_events_nor_the_terminal() {
        let outbox = Outbox::default();
        let (open, gate) = mpsc::channel();
        let mut sink = Gate(gate, Vec::new());
        thread::scope(|scope| {
            let writer = scope.spawn(|| outbox.write_all(&mut sink));
            outbox.begin_job(1, false);
            outbox.job_event(
                1,
                Event::Stage {
                    stage: studio_domain::job::Stage::Encoding,
                },
            );
            for frames_read in 0..10_000 {
                let progress = studio_domain::job::Progress {
                    frames_read,
                    ..Default::default()
                };
                outbox.progress(1, Event::Progress { progress });
            }
            outbox.job_end(1, Event::Cancelled);
            outbox.close();
            assert!(open.send(()).is_ok());
            assert!(matches!(writer.join(), Ok(Ok(()))));
        });
        let mut reader = sink.1.as_slice();
        let mut bodies = Vec::new();
        while let Ok(Some(envelope)) = read_message::<Event>(&mut reader) {
            bodies.push(envelope.body);
        }
        assert!(matches!(bodies.first(), Some(Event::Stage { .. })));
        assert!(matches!(bodies.last(), Some(Event::Cancelled)));
        // The stage, one progress (the newest), the terminal.
        assert!(
            matches!(bodies.as_slice(), [_, Event::Progress { progress }, _] if progress.frames_read == 9_999),
            "{bodies:?}"
        );
    }

    /// `Working` keeps its place among the job's events: after what came
    /// before it, never replaced by progress, and never after the terminal.
    #[test]
    fn working_is_ordered_kept_and_never_follows_the_terminal() {
        let outbox = Outbox::default();
        let partial = Event::Working {
            partial: studio_domain::job::WirePath::from_path(std::path::Path::new(
                ".out.ivf.x.partial",
            )),
        };
        let progress = |frames_read| Event::Progress {
            progress: studio_domain::job::Progress {
                frames_read,
                ..Default::default()
            },
        };
        outbox.begin_job(1, false);
        outbox.progress(1, progress(1));
        outbox.job_event(1, partial.clone());
        outbox.progress(1, progress(2));
        outbox.progress(1, progress(3));
        outbox.job_end(1, Event::Cancelled);
        outbox.job_event(1, partial.clone());
        let bodies: Vec<Event> = outbox.lock().queue.drain(..).map(|e| e.body).collect();
        assert_eq!(
            bodies,
            [progress(1), partial, progress(3), Event::Cancelled],
            "{bodies:?}"
        );
    }

    /// The encoder's token is cancelled with the flag, also when it is
    /// handed over after the cancel. A job that still does not end is
    /// abandoned after the grace (the backstop): its temporary file is
    /// removed, `Cancelled` is its terminal event, and its own late end adds
    /// nothing. A job that ends in time is not abandoned.
    #[test]
    fn cancel_reaches_the_encoder_and_the_backstop_abandons_a_stuck_job() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let temp = dir.path().join("out.ivf.partial");
        let outbox = Outbox::default();
        outbox.begin_job(1, false);
        outbox.hold_temp(&mut || {
            fs::write(&temp, b"x").ok()?;
            Some(temp.clone())
        });
        let early = CancelToken::default();
        outbox.hold_encoder(early.clone());
        assert!(!early.is_cancelled());
        assert!(matches!(outbox.cancel(2), Cancel::Accepted));
        assert!(early.is_cancelled());
        let late = CancelToken::default();
        outbox.hold_encoder(late.clone());
        assert!(late.is_cancelled());
        let grace = Duration::from_millis(50);
        let asked = Instant::now();
        assert!(outbox.settle(grace));
        assert!(asked.elapsed() >= grace);
        assert!(!temp.exists());
        outbox.job_end(
            1,
            Event::Failed {
                failure: Failure {
                    code: FailureCode::Encoder,
                    message: "late".into(),
                },
            },
        );
        let bodies: Vec<Event> = outbox.lock().queue.drain(..).map(|e| e.body).collect();
        assert_eq!(bodies, [Event::CancelRequested, Event::Cancelled]);

        let outbox = Outbox::default();
        outbox.begin_job(1, false);
        assert!(matches!(outbox.cancel(2), Cancel::Accepted));
        thread::scope(|scope| {
            scope.spawn(|| {
                thread::sleep(Duration::from_millis(20));
                outbox.job_end(1, Event::Cancelled);
            });
            assert!(!outbox.settle(Duration::from_secs(10)));
        });
        Ok(())
    }

    /// Output failure cancels the job and the session reports it.
    #[test]
    fn a_failed_output_cancels() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut input = Vec::new();
        assert!(write_message(&mut input, &hello(1)).is_ok());
        let result = run(&mut input.as_slice(), Broken);
        assert!(matches!(result, Err(SessionError::Write(_))));
    }
}
