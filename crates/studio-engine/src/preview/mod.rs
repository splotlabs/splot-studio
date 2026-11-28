// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The preview process: pictures of sources and finished outputs, and
//! video-only playback, over the same framed protocol as the worker.
//!
//! Three threads. The caller's thread reads commands and never decodes: a
//! `Picture`, `Play` or `StopPlayback` replaces the one request slot (an
//! older request not yet started is dropped unanswered, as the protocol
//! allows) and bumps a serial number that every running piece of work
//! checks between frames; a media decode blocked inside `ffmpeg` is killed.
//! The work thread serves one request at a time. The writer thread owns
//! the output: control events are queued in order, and pictures go
//! through a one-picture slot that a newer picture replaces, so a slow
//! reader gets fewer pictures and never a backlog. Before an event is
//! queued, a picture waiting in the slot is queued ahead of it, so a
//! `PlaybackEnded` never overtakes the pictures it ends.
//!
//! The work thread keeps up to [`OPEN_TARGETS`] files open, the
//! [`ACTIVE_SESSIONS`] most recently used with their decoding session (an
//! `ffmpeg` child, or a Splot decoder of [`crate::stream::PREVIEW_DECODE_THREADS`]
//! threads holding at most [`crate::stream::DECODE_STORE_BYTES`]), and a
//! [`cache::Cache`] of presentation pictures bounded by
//! [`cache::CACHE_BYTES`]. A request's own memory is one decoded frame and
//! one picture of at most [`studio_domain::preview::MAX_PICTURE_BYTES`].
//! Idle, nothing runs: every thread waits on a condition variable or a
//! pipe. The process ends at the end of its input.
//!
//! Document verification and replay check cancellation between bounded reads.
//! A media source runs `ffprobe` with a timeout.

mod cache;
mod document;
mod result;
mod source;

use std::cell::Cell;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use studio_domain::preview::{
    ColorBasis, ColorMatrix, GrainPolicy, MAX_PICTURE_HEIGHT, MAX_PICTURE_WIDTH, MAX_PICTURE_ZOOM,
    MediaTime, PictureBox, PictureFailure, PictureFrame, PictureHeader, PictureOptions,
    PictureOrigin, PictureStage, PictureSurface, PictureTarget, PictureView, SamplePlane,
    SamplePoint,
};
use studio_domain::protocol::{
    Command, Envelope, ErrorCode, Event, ProtocolError, decode, read_frame, write_message,
    write_picture,
};

pub(crate) use self::document::picture_info;
use self::document::{DocumentPicture, Recent};
use crate::job::{JOB_STACK_BYTES, Stop};
use crate::media::process::Killer;
use crate::picture::{self, Frame};
use crate::worker::{SessionError, request_id_of};
use crate::{build_identity, capabilities};

/// Files kept open with their decoding sessions; the least recently used
/// one beyond this is closed.
pub const OPEN_TARGETS: usize = 4;

/// Open files that keep a decoding session (an `ffmpeg` child or a Splot
/// decoder) between requests; the others keep only their file and index.
pub const ACTIVE_SESSIONS: usize = 2;

/// During playback, a frame is shown at least this often even when every
/// frame is late, so slow decoding still shows pictures.
pub const MIN_PLAYBACK_INTERVAL: Duration = Duration::from_millis(250);

/// Command replies queued for a peer that does not read before the reader
/// stops taking commands.
const REPLY_BACKLOG: usize = 64;

/// The document playback interval when the stream provides no usable timing.
const ASSUMED_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 30);

/// Serves the preview protocol on `input` and `output` until the input
/// ends. Every frame gets one reply except `Picture` and `Play` (answered
/// by pictures) and `StopPlayback` (see [`Command::StopPlayback`]); job
/// commands are refused with [`ErrorCode::WrongState`].
///
/// # Errors
/// [`SessionError::Read`] on an unrecoverable input frame or failure,
/// [`SessionError::Write`] when the output fails.
pub fn run(input: &mut impl Read, output: impl Write + Send) -> Result<(), SessionError> {
    let shared = Shared::default();
    thread::scope(|scope| {
        let writer = scope.spawn(|| shared.write_all(output));
        let work = thread::Builder::new()
            .name("studio-preview".to_owned())
            .stack_size(JOB_STACK_BYTES)
            .spawn_scoped(scope, || work(&shared));
        let read = match work {
            Ok(work) => {
                let read = serve(input, &shared);
                shared.close();
                // The work body returns on close; it has no panics to report.
                let _ = work.join();
                read
            }
            Err(error) => Err(ProtocolError::Io(error)),
        };
        shared.finish();
        let written = writer
            .join()
            .unwrap_or_else(|_| Err(ProtocolError::Io(std::io::Error::other("writer panicked"))));
        read.map_err(SessionError::Read)
            .and(written.map_err(SessionError::Write))
    })
}

fn serve(input: &mut impl Read, shared: &Shared) -> Result<(), ProtocolError> {
    loop {
        let Some(frame) = read_frame(input)? else {
            return Ok(());
        };
        let envelope = match decode::<Command>(&frame) {
            Ok(envelope) => envelope,
            Err(error) => {
                let code = match error {
                    ProtocolError::IncompatibleVersion { .. } => ErrorCode::IncompatibleVersion,
                    _ => ErrorCode::Malformed,
                };
                shared.reply(request_id_of(&frame), refusal(code, error.to_string()));
                continue;
            }
        };
        let id = envelope.request_id;
        let request = match envelope.body {
            Command::Hello { .. } => {
                shared.reply(
                    id,
                    Event::Hello {
                        engine: build_identity(),
                        capabilities: capabilities(),
                    },
                );
                continue;
            }
            Command::Picture {
                generation,
                target,
                frame,
                view,
                options,
                size,
            } => Request::Picture {
                generation,
                target,
                frame,
                display: Display {
                    view,
                    size,
                    options,
                },
            },
            Command::Play {
                generation,
                target,
                from,
                view,
                options,
                size,
            } => Request::Play {
                generation,
                target,
                from,
                display: Display {
                    view,
                    size,
                    options,
                },
            },
            Command::PictureSample {
                generation,
                target,
                ordinal,
                grain,
                point,
            } => Request::Sample {
                generation,
                target,
                ordinal,
                grain,
                point,
            },
            Command::StopPlayback => Request::Stop,
            Command::CloseDocumentPlayback { document } => {
                shared.close_document(document);
                continue;
            }
            _ => {
                shared.reply(
                    id,
                    refusal(
                        ErrorCode::WrongState,
                        "the preview process runs no jobs or analysis",
                    ),
                );
                continue;
            }
        };
        shared.submit(id, request);
    }
}

fn refusal(code: ErrorCode, message: impl Into<String>) -> Event {
    Event::Error {
        code,
        message: message.into(),
    }
}

/// One request for the work thread.
#[derive(Debug)]
enum Request {
    Picture {
        generation: u64,
        target: PictureTarget,
        frame: PictureFrame,
        display: Display,
    },
    Play {
        generation: u64,
        target: PictureTarget,
        from: u64,
        display: Display,
    },
    Sample {
        generation: u64,
        target: PictureTarget,
        ordinal: u64,
        grain: GrainPolicy,
        point: SamplePoint,
    },
    Stop,
}

impl Request {
    fn document(&self) -> Option<u64> {
        let target = match self {
            Self::Picture { target, .. }
            | Self::Play { target, .. }
            | Self::Sample { target, .. } => target,
            Self::Stop => return None,
        };
        match target {
            PictureTarget::Document { document, .. } => Some(*document),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Display {
    view: PictureView,
    size: PictureBox,
    options: PictureOptions,
}

impl Display {
    fn check(self, target: &PictureTarget) -> Outcome<()> {
        valid(self.size)?;
        if self.options != PictureOptions::default()
            && !matches!(target, PictureTarget::Document { .. })
        {
            return Err(Stopped::unavailable(
                PictureFailure::InvalidRequest,
                "Plane and grain controls require a document.",
            ));
        }
        if let PictureView::Zoom { factor, .. } = self.view
            && (!(2..=MAX_PICTURE_ZOOM).contains(&factor)
                || u16::from(factor) > self.size.width.min(self.size.height))
        {
            return Err(Stopped::unavailable(
                PictureFailure::InvalidRequest,
                "The zoom factor is outside the allowed range for this picture box.",
            ));
        }
        Ok(())
    }

    const fn plane(self) -> Option<SamplePlane> {
        match self.options.surface {
            PictureSurface::Rgb => None,
            PictureSurface::Y => Some(SamplePlane::Y),
            PictureSurface::U => Some(SamplePlane::U),
            PictureSurface::V => Some(SamplePlane::V),
        }
    }
}

enum Work {
    Request(u64, u64, Request),
    CloseDocument(u64),
}

#[derive(Debug, Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    /// Bumped by every request and by the end of input; work started under
    /// an older value is superseded.
    serial: Arc<AtomicU64>,
}

/// One frame for the writer.
#[derive(Debug)]
enum Item {
    Event(Envelope<Event>),
    /// A picture with its `request_id`.
    Picture(u64, PictureHeader, Vec<u8>),
}

#[derive(Debug, Default)]
struct State {
    /// Frames that are never dropped, in order.
    queue: VecDeque<Item>,
    /// The newest picture, not yet written. Replaced, never queued, unless
    /// an event must follow it.
    picture: Option<(u64, PictureHeader, Vec<u8>)>,
    /// The newest progress event. It never enters the control backlog.
    progress: Option<Envelope<Event>>,
    /// Release requests must run even when a newer picture replaces the request.
    close_documents: VecDeque<u64>,
    active_document: Option<u64>,
    open_documents: Vec<u64>,
    /// The newest request not yet taken by the work thread.
    request: Option<(u64, u64, Request)>,
    /// Kills the media decode the work thread is waiting on.
    abort: Option<Killer>,
    /// The input ended.
    closed: bool,
    /// The work thread is done; the writer ends once the queue is empty.
    finished: bool,
    /// The output failed; everything is discarded.
    broken: bool,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn notify(&self, state: MutexGuard<'_, State>) {
        drop(state);
        self.changed.notify_all();
    }

    /// Queues a reply, waiting while the peer is far behind.
    fn reply(&self, request_id: u64, event: Event) {
        let mut state = self.lock();
        while state.queue.len() >= REPLY_BACKLOG && !state.broken {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        state.push(Envelope::new(request_id, event));
        self.notify(state);
    }

    /// Replaces the request slot and supersedes running work.
    fn submit(&self, request_id: u64, request: Request) {
        let mut state = self.lock();
        let serial = self.serial.fetch_add(1, Ordering::SeqCst) + 1;
        state.request = Some((serial, request_id, request));
        state.discard_document_update();
        if let Some(killer) = state.abort.take() {
            killer.kill();
        }
        self.notify(state);
    }

    fn close_document(&self, document: u64) {
        let mut state = self.lock();
        if state
            .request
            .as_ref()
            .is_some_and(|(_, _, request)| request.document() == Some(document))
        {
            state.request = None;
        }
        if state.active_document == Some(document) && state.request.is_none() {
            self.serial.fetch_add(1, Ordering::SeqCst);
            state.discard_document_update();
        }
        // At most the open targets and the active request can need closure.
        if (state.active_document == Some(document) || state.open_documents.contains(&document))
            && !state.close_documents.contains(&document)
        {
            state.close_documents.push_back(document);
        }
        self.notify(state);
    }

    fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        state.progress = None;
        self.serial.fetch_add(1, Ordering::SeqCst);
        if let Some(killer) = state.abort.take() {
            killer.kill();
        }
        self.notify(state);
    }

    fn finish(&self) {
        let mut state = self.lock();
        state.finished = true;
        self.notify(state);
    }

    /// The next request, or `None` once the input ended.
    fn take(&self, targets: &[Entry]) -> Option<Work> {
        let mut state = self.lock();
        state.active_document = None;
        state.open_documents.clear();
        state
            .open_documents
            .extend(targets.iter().filter_map(|entry| match entry.key {
                PictureTarget::Document { document, .. } => Some(document),
                _ => None,
            }));
        loop {
            if state.closed {
                return None;
            }
            if let Some(document) = state.close_documents.pop_front() {
                self.notify(state);
                return Some(Work::CloseDocument(document));
            }
            if let Some((serial, id, request)) = state.request.take() {
                state.active_document = request.document();
                return Some(Work::Request(serial, id, request));
            }
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// The writer thread: writes until finished and empty.
    fn write_all(&self, mut output: impl Write) -> Result<(), ProtocolError> {
        let mut state = self.lock();
        loop {
            let written = if let Some(item) = state.queue.pop_front() {
                self.notify(state);
                match item {
                    Item::Event(envelope) => write_message(&mut output, &envelope),
                    Item::Picture(id, header, pixels) => {
                        write_picture(&mut output, id, header, &pixels)
                    }
                }
            } else if let Some((id, header, pixels)) = state.picture.take() {
                self.notify(state);
                write_picture(&mut output, id, header, &pixels)
            } else if let Some(envelope) = state.progress.take() {
                self.notify(state);
                write_message(&mut output, &envelope)
            } else if state.finished {
                return Ok(());
            } else {
                state = self
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
                continue;
            };
            if let Err(error) = written {
                let mut state = self.lock();
                state.broken = true;
                state.queue.clear();
                state.picture = None;
                state.progress = None;
                self.notify(state);
                // Nobody can see pictures any more; stop working on them.
                self.close();
                return Err(error);
            }
            state = self.lock();
        }
    }
}

impl State {
    fn discard_document_update(&mut self) {
        self.progress = None;
        if self
            .picture
            .as_ref()
            .is_some_and(|(_, header, _)| header.origin == PictureOrigin::Document)
        {
            self.picture = None;
        }
    }

    fn push(&mut self, envelope: Envelope<Event>) {
        if !self.broken {
            self.queue.push_back(Item::Event(envelope));
        }
    }

    /// Queues a waiting picture, so that what follows stays behind it.
    fn flush_picture(&mut self) {
        if let Some((id, header, pixels)) = self.picture.take() {
            self.queue.push_back(Item::Picture(id, header, pixels));
        }
    }
}

/// What one request may do: its identity and the means to notice that it
/// was superseded.
struct Context<'a> {
    shared: &'a Shared,
    serial: u64,
    request_id: u64,
    generation: u64,
    last_progress: Cell<Option<(PictureStage, Instant)>>,
}

/// Why a request produced no picture.
#[derive(Debug)]
enum Stopped {
    /// A newer request or the end of input; nothing is answered.
    Superseded,
    /// The answer is [`Event::PictureUnavailable`].
    Unavailable(PictureFailure, String),
}

impl Stopped {
    fn unavailable(code: PictureFailure, message: impl Into<String>) -> Self {
        Self::Unavailable(code, message.into())
    }

    /// A source failure, as the picture failure it means.
    fn from_stop(stop: Stop) -> Self {
        use studio_domain::job::FailureCode;
        match stop {
            Stop::Cancelled => Self::Superseded,
            Stop::Failed(failure) => {
                let code = match failure.code {
                    FailureCode::SourceChanged => PictureFailure::SourceChanged,
                    FailureCode::UnsupportedSource | FailureCode::TooLarge => {
                        PictureFailure::Unsupported
                    }
                    FailureCode::MediaToolMissing => PictureFailure::MediaToolMissing,
                    _ => PictureFailure::Unreadable,
                };
                Self::Unavailable(code, failure.message)
            }
        }
    }
}

type Outcome<T> = Result<T, Stopped>;

impl Context<'_> {
    fn superseded(&self) -> bool {
        self.shared.serial.load(Ordering::SeqCst) != self.serial
    }

    /// `Err(Superseded)` once a newer request arrived.
    fn check(&self) -> Outcome<()> {
        if self.superseded() {
            Err(Stopped::Superseded)
        } else {
            Ok(())
        }
    }

    /// Runs `body` with `killer` registered, so a newer request kills the
    /// decode `body` waits on.
    fn guarded<T>(&self, killer: Option<Killer>, body: impl FnOnce() -> T) -> T {
        if let Some(killer) = killer {
            let mut state = self.shared.lock();
            if self.superseded() {
                killer.kill();
            } else {
                state.abort = Some(killer);
            }
        }
        let result = body();
        self.shared.lock().abort = None;
        result
    }

    /// Progress is advisory and at most one event is waiting to be sent.
    fn progress(&self, stage: PictureStage, completed: u64, total: Option<u64>) {
        let now = Instant::now();
        if self.last_progress.get().is_some_and(|(previous, when)| {
            previous == stage && now.duration_since(when) < MIN_PLAYBACK_INTERVAL
        }) {
            return;
        }
        let mut mailbox = self.shared.lock();
        if !mailbox.broken && !self.superseded() {
            mailbox.progress = Some(Envelope::new(
                self.request_id,
                Event::PictureProgress {
                    generation: self.generation,
                    stage,
                    completed,
                    total,
                },
            ));
            self.last_progress.set(Some((stage, now)));
        }
        self.shared.notify(mailbox);
    }

    /// Offers a picture; it replaces any picture not yet written.
    fn picture(&self, header: PictureHeader, pixels: Vec<u8>) {
        let mut state = self.shared.lock();
        if !state.broken && !self.superseded() {
            state.progress = None;
            state.picture = Some((self.request_id, header, pixels));
        }
        self.shared.notify(state);
    }

    /// Queues an event behind any picture already offered.
    fn event(&self, event: Event) {
        let playback_end = matches!(event, Event::PlaybackEnded { .. });
        let current = || playback_end || !self.superseded();
        let mut state = self.shared.lock();
        while state.queue.len() >= REPLY_BACKLOG - 1 && !state.closed && !state.broken && current()
        {
            state = self
                .shared
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        if !state.closed && !state.broken && current() {
            state.progress = None;
            state.flush_picture();
            state.push(Envelope::new(self.request_id, event));
        }
        self.shared.notify(state);
    }

    /// Waits until `due`. `false` when superseded first.
    fn wait_until(&self, due: Instant) -> bool {
        let mut state = self.shared.lock();
        loop {
            if self.superseded() {
                return false;
            }
            let left = due.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return true;
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

fn work(shared: &Shared) {
    let mut previews = Previews::default();
    while let Some(work) = shared.take(&previews.targets) {
        let (serial, request_id, request) = match work {
            Work::CloseDocument(document) => {
                previews.close_document(document);
                continue;
            }
            Work::Request(serial, request_id, request) => (serial, request_id, request),
        };
        let generation = match &request {
            Request::Picture { generation, .. }
            | Request::Play { generation, .. }
            | Request::Sample { generation, .. } => *generation,
            Request::Stop => continue,
        };
        let context = Context {
            shared,
            serial,
            request_id,
            generation,
            last_progress: Cell::new(None),
        };
        let (target, answer) = match request {
            Request::Picture {
                target,
                frame,
                display,
                ..
            } => {
                let answer = previews.picture(&context, &target, frame, display);
                (target, answer)
            }
            Request::Play {
                target,
                from,
                display,
                ..
            } => {
                let answer = previews.play(&context, &target, from, display);
                (target, answer)
            }
            Request::Sample {
                target,
                ordinal,
                grain,
                point,
                ..
            } => {
                let answer = previews.sample(&context, &target, ordinal, grain, point);
                (target, answer)
            }
            Request::Stop => continue,
        };
        if let Err(Stopped::Unavailable(code, message)) = answer {
            if code == PictureFailure::SourceChanged {
                previews.recent.forget_matching(|entry| *entry == target);
            }
            if !context.superseded() {
                context.event(Event::PictureUnavailable {
                    generation,
                    code,
                    message,
                });
            }
        }
    }
}

/// A decoded frame, ready to convert.
enum Decoded {
    /// Source planes, as the encoder receives them.
    Planes {
        planes: [Vec<u16>; 3],
        sizes: [(usize, usize); 3],
        bit_depth: u8,
    },
    /// A Splot decoder picture.
    Picture(Box<splot_decode::Picture>),
    Document(Rc<DocumentPicture>),
}

impl Decoded {
    fn frame(&self) -> Option<Frame<'_>> {
        match self {
            Self::Planes {
                planes,
                sizes,
                bit_depth,
            } => Frame::packed(planes, *sizes, *bit_depth),
            Self::Picture(picture) => crate::stream::frame(picture),
            Self::Document(picture) => crate::stream::frame(&picture.picture),
        }
    }
}

/// An open file pictures are taken from.
enum Target {
    Source(Box<source::SourcePictures>),
    Result(Box<result::ResultPictures>),
}

impl Target {
    fn open(target: &PictureTarget, context: &Context<'_>) -> Outcome<Self> {
        match target {
            PictureTarget::Source { source } => {
                source::SourcePictures::open(source).map(|s| Self::Source(Box::new(s)))
            }
            PictureTarget::Result { path, source } => {
                result::ResultPictures::open(path, source.as_ref(), None, context)
                    .map(|r| Self::Result(Box::new(r)))
            }
            PictureTarget::Document {
                path,
                fingerprint,
                digest,
                selection,
                ..
            } => result::ResultPictures::open(path, None, Some((*fingerprint, digest)), context)
                .map(|r| Self::Result(Box::new(r.with_layer_selection(*selection)))),
        }
    }

    /// Releases the decoding session, keeping the file open.
    fn rest(&mut self) {
        match self {
            Self::Source(s) => s.rest(),
            Self::Result(r) => r.rest(),
        }
    }

    /// Whether the file is still the one opened; `Err` closes it.
    fn check(&self) -> Outcome<()> {
        match self {
            Self::Source(s) => s.check(),
            Self::Result(r) => r.check(),
        }
    }

    const fn origin(&self) -> PictureOrigin {
        match self {
            Self::Source(_) => PictureOrigin::Source,
            Self::Result(r) if r.is_document() => PictureOrigin::Document,
            Self::Result(_) => PictureOrigin::Result,
        }
    }

    fn check_document(&self) -> Outcome<()> {
        match self {
            Self::Result(r) if r.is_document() => r.check(),
            _ => Ok(()),
        }
    }

    fn frame_count(&self) -> Option<u64> {
        match self {
            Self::Source(s) => s.frame_count(),
            Self::Result(r) => r.frame_count(),
        }
    }

    fn time(&self, index: u64) -> Option<MediaTime> {
        match self {
            Self::Source(s) => s.time(index),
            Self::Result(r) => r.time(index),
        }
    }

    fn basis(&self, frame: &Frame<'_>, decoded: &Decoded) -> Outcome<ColorBasis> {
        let (w, h) = frame.size();
        let matrix = match (self, decoded) {
            (_, Decoded::Document(picture)) => picture.matrix()?,
            (Self::Source(s), _) => s.matrix(),
            (Self::Result(r), _) => r.matrix(),
        };
        Ok(picture::basis(
            matrix,
            frame.chroma.is_none(),
            u32::try_from(w).unwrap_or(u32::MAX),
            u32::try_from(h).unwrap_or(u32::MAX),
        ))
    }

    /// The frames in one second, for the poster rule.
    fn second(&self) -> u64 {
        match self {
            Self::Source(s) => s.second(),
            Self::Result(r) => r.second(),
        }
    }

    /// Makes `index` the next frame [`Self::next`] returns.
    fn prepare(&mut self, grain: GrainPolicy) {
        if let Self::Result(result) = self {
            result.choose_grain(grain);
        }
    }

    fn seek(&mut self, context: &Context<'_>, index: u64, grain: GrainPolicy) -> Outcome<()> {
        match self {
            Self::Source(s) => s.seek(context, index),
            Self::Result(r) => r.seek(context, index, grain),
        }
    }

    fn decode_at(
        &mut self,
        context: &Context<'_>,
        index: u64,
        grain: GrainPolicy,
    ) -> Outcome<Decoded> {
        self.seek(context, index, grain)?;
        match self.next(context)? {
            Some((at, decoded)) if at == index => Ok(decoded),
            Some((at, _)) => Err(Stopped::unavailable(
                PictureFailure::Unreadable,
                format!("The decoder returned output {at}. Expected output {index}."),
            )),
            None => Err(out_of_range(index, self.frame_count())),
        }
    }

    /// The next frame and its index; `None` after the last.
    fn next(&mut self, context: &Context<'_>) -> Outcome<Option<(u64, Decoded)>> {
        match self {
            Self::Source(s) => s.next(context),
            Self::Result(r) => r.next(context),
        }
    }

    /// The representative frame: one second in, or the last of a shorter
    /// stream. Without a known count the frames up to it are read.
    fn poster(
        &mut self,
        context: &Context<'_>,
        grain: GrainPolicy,
    ) -> Outcome<(u64, Option<Decoded>)> {
        let wanted = self.second();
        if let Some(count) = self.frame_count() {
            return match count.checked_sub(1) {
                Some(last) => Ok((wanted.min(last), None)),
                None => Err(Stopped::unavailable(
                    PictureFailure::OutOfRange,
                    "the stream has no frames",
                )),
            };
        }
        self.seek(context, 0, grain)?;
        let mut last = None;
        while last.as_ref().is_none_or(|(index, _)| *index < wanted) {
            match self.next(context)? {
                Some(frame) => last = Some(frame),
                None => break,
            }
        }
        last.map(|(index, decoded)| (index, Some(decoded)))
            .ok_or_else(|| {
                Stopped::unavailable(PictureFailure::OutOfRange, "the stream has no frames")
            })
    }
}

/// An open target.
struct Entry {
    key: PictureTarget,
    target: Target,
    /// The poster's index, once chosen.
    poster: Option<u64>,
}

#[derive(Default)]
struct Previews {
    /// Open files, the most recently used last.
    targets: Vec<Entry>,
    cache: cache::Cache<PictureTarget>,
    recent: Recent,
}

/// The open entry for `target`, opened or checked now.
fn open<'a>(
    targets: &'a mut Vec<Entry>,
    cache: &mut cache::Cache<PictureTarget>,
    target: &PictureTarget,
    context: &Context<'_>,
) -> Outcome<&'a mut Entry> {
    if let Some(at) = targets.iter().position(|entry| entry.key == *target) {
        let entry = targets.remove(at);
        if let Err(stopped) = entry.target.check() {
            cache.forget(target);
            return Err(stopped);
        }
        targets.push(entry);
    } else {
        let opened = Target::open(target, context)?;
        cache.forget(target);
        targets.push(Entry {
            key: target.clone(),
            target: opened,
            poster: None,
        });
        if targets.len() > OPEN_TARGETS {
            // Closing kills its `ffmpeg` and stops its decoder.
            cache.forget(&targets.remove(0).key);
        }
    }
    // Two files keep their sessions (a source and its result, compared);
    // the others keep only their index and file.
    let resting = targets.len().saturating_sub(ACTIVE_SESSIONS);
    for entry in &mut targets[..resting] {
        entry.target.rest();
    }
    targets
        .last_mut()
        .ok_or_else(|| Stopped::unavailable(PictureFailure::Unreadable, "no open file"))
}

impl Previews {
    fn close_document(&mut self, document: u64) {
        let matches = |target: &PictureTarget| matches!(target, PictureTarget::Document { document: id, .. } if *id == document);
        self.targets.retain(|entry| !matches(&entry.key));
        self.cache.forget_matching(matches);
        self.recent.forget_matching(matches);
    }

    fn picture(
        &mut self,
        context: &Context<'_>,
        target: &PictureTarget,
        frame: PictureFrame,
        display: Display,
    ) -> Outcome<()> {
        let grain = display.options.grain;
        self.recent.prepare(target, grain);
        display.check(target)?;
        let entry = open(&mut self.targets, &mut self.cache, target, context)?;
        let open = &mut entry.target;
        open.prepare(grain);
        let (index, decoded) = match (frame, entry.poster) {
            (PictureFrame::Index { index }, _) | (PictureFrame::Poster, Some(index)) => {
                (index, None)
            }
            (PictureFrame::Poster, None) => {
                let (index, decoded) = open.poster(context, grain)?;
                entry.poster = Some(index);
                (index, decoded)
            }
        };
        if let Some((mut header, pixels)) = self.cache.get(target, index, display) {
            header.generation = context.generation;
            header.frame_count = open.frame_count();
            open.check_document()?;
            context.picture(header, pixels);
            return Ok(());
        }
        let decoded = match decoded.or_else(|| self.recent.get(target, index, grain)) {
            Some(decoded) => decoded,
            None => open.decode_at(context, index, grain)?,
        };
        context.check()?;
        open.check_document()?;
        self.recent.keep(target, grain, &decoded);
        let (header, pixels) = render(open, index, &decoded, display, context.generation)?;
        open.check_document()?;
        self.cache.put(target.clone(), display, &header, &pixels);
        context.picture(header, pixels);
        Ok(())
    }

    fn sample(
        &mut self,
        context: &Context<'_>,
        target: &PictureTarget,
        ordinal: u64,
        grain: GrainPolicy,
        point: SamplePoint,
    ) -> Outcome<()> {
        self.recent.prepare(target, grain);
        if !matches!(target, PictureTarget::Document { .. }) {
            return Err(Stopped::unavailable(
                PictureFailure::InvalidRequest,
                "Select a document to inspect samples.",
            ));
        }
        let open = &mut open(&mut self.targets, &mut self.cache, target, context)?.target;
        open.prepare(grain);
        let decoded = match self.recent.get(target, ordinal, grain) {
            Some(decoded) => decoded,
            None => open.decode_at(context, ordinal, grain)?,
        };
        let Decoded::Document(picture) = &decoded else {
            return Err(Stopped::unavailable(
                PictureFailure::Unreadable,
                "The decoder did not return a document picture.",
            ));
        };
        context.check()?;
        open.check_document()?;
        self.recent.keep(target, grain, &decoded);
        let value = picture.sample(point)?;
        context.event(Event::PictureSample {
            generation: context.generation,
            ordinal,
            grain,
            point,
            value,
        });
        Ok(())
    }

    fn play(
        &mut self,
        context: &Context<'_>,
        target: &PictureTarget,
        from: u64,
        display: Display,
    ) -> Outcome<()> {
        let mut last = None;
        let played = self.playback(context, target, from, display, &mut last);
        match played {
            Err(error @ Stopped::Unavailable(..))
                if last.is_none() || matches!(target, PictureTarget::Document { .. }) =>
            {
                Err(error)
            }
            _ => {
                if !context.shared.lock().closed {
                    context.event(Event::PlaybackEnded {
                        generation: context.generation,
                        frame: last,
                    });
                }
                Ok(())
            }
        }
    }

    fn playback(
        &mut self,
        context: &Context<'_>,
        target: &PictureTarget,
        from: u64,
        display: Display,
        last: &mut Option<u64>,
    ) -> Outcome<()> {
        let grain = display.options.grain;
        self.recent.prepare(target, grain);
        display.check(target)?;
        let open = &mut open(&mut self.targets, &mut self.cache, target, context)?.target;
        open.prepare(grain);
        let mut clock = if open.origin() == PictureOrigin::Document {
            PlayClock::Document(DocumentClock::default())
        } else if let Some(start) = open.time(from) {
            PlayClock::Indexed(start)
        } else {
            return Err(Stopped::unavailable(
                PictureFailure::NoTiming,
                "The stream has no playback timing. Provide the source for an Annex B result.",
            ));
        };
        open.seek(context, from, grain)?;
        let started = Instant::now();
        let mut shown = started;
        // The newest frame that was skipped as late. Playback must still end
        // on the stream's last frame, so it is sent if nothing follows it.
        let mut skipped = None;
        loop {
            context.check()?;
            let Some((index, decoded)) = open.next(context)? else {
                if let Some((index, decoded)) = skipped.take() {
                    let (header, pixels) =
                        render(open, index, &decoded, display, context.generation)?;
                    open.check_document()?;
                    self.recent.keep(target, grain, &decoded);
                    context.picture(header, pixels);
                    *last = Some(index);
                }
                open.check_document()?;
                if last.is_none() {
                    return Err(out_of_range(from, open.frame_count()));
                }
                return Ok(());
            };
            let (offset, next_offset) = clock.offsets(open, index, &decoded);
            let now = Instant::now();
            let due = started.checked_add(offset).unwrap_or(now);
            let next_due = started.checked_add(next_offset).unwrap_or(due);
            // Late: the next frame is already due. Skipped, unless nothing
            // was shown for a while.
            if last.is_some() && now >= next_due && now < shown + MIN_PLAYBACK_INTERVAL {
                skipped = Some((index, decoded));
                continue;
            }
            skipped = None;
            let (header, pixels) = render(open, index, &decoded, display, context.generation)?;
            self.recent.keep(target, grain, &decoded);
            drop(decoded);
            if !context.wait_until(due) {
                return Err(Stopped::Superseded);
            }
            open.check_document()?;
            context.picture(header, pixels);
            *last = Some(index);
            shown = Instant::now();
        }
    }
}

enum PlayClock {
    Indexed(MediaTime),
    Document(DocumentClock),
}

impl PlayClock {
    fn offsets(&mut self, target: &Target, index: u64, decoded: &Decoded) -> (Duration, Duration) {
        match (self, decoded) {
            (Self::Indexed(start), _) => (
                elapsed(*start, target.time(index)),
                elapsed(*start, target.time(index + 1)),
            ),
            (Self::Document(clock), Decoded::Document(picture)) => {
                let due = clock.advance(
                    picture.output.unit_offset,
                    picture.time,
                    picture.output.interval,
                );
                // The next PTS is unknown until the decoder emits the next picture.
                (due, due.saturating_add(MIN_PLAYBACK_INTERVAL))
            }
            _ => (Duration::ZERO, Duration::ZERO),
        }
    }
}

#[derive(Default)]
struct DocumentClock {
    previous: Option<(u64, Option<MediaTime>, Option<MediaTime>)>,
    offset: Duration,
}

impl DocumentClock {
    fn advance(
        &mut self,
        unit: u64,
        time: Option<MediaTime>,
        interval: Option<MediaTime>,
    ) -> Duration {
        let mut latest_time = time;
        if let Some((previous_unit, previous_time, previous_interval)) = self.previous {
            let gap = if let Some((start, end)) = previous_time.zip(time) {
                // A reversed PTS is already due. Keep the last valid clock position.
                exact_elapsed(start, end).unwrap_or_else(|| {
                    latest_time = previous_time;
                    Duration::ZERO
                })
            } else {
                previous_interval
                    .and_then(|step| {
                        if previous_unit == unit {
                            Some(Duration::ZERO)
                        } else {
                            exact_elapsed(MediaTime { num: 0, den: 1 }, step)
                        }
                    })
                    .unwrap_or(ASSUMED_INTERVAL)
            };
            self.offset = self.offset.saturating_add(gap);
        }
        self.previous = Some((unit, latest_time, interval));
        self.offset
    }
}

fn exact_elapsed(start: MediaTime, end: MediaTime) -> Option<Duration> {
    let a = u128::from(start.num) * u128::from(end.den);
    let b = u128::from(end.num) * u128::from(start.den);
    let den = u128::from(start.den) * u128::from(end.den);
    let delta = b.checked_sub(a)?;
    let secs = u64::try_from(delta.checked_div(den)?).ok()?;
    // Both inputs use u64, but their denominator product can exhaust u128.
    let nanos = (delta % den).checked_mul(1_000_000_000)?.checked_div(den)?;
    Some(Duration::new(secs, u32::try_from(nanos).ok()?))
}

/// The time from `start` to `time`; zero when unknown or earlier.
fn elapsed(start: MediaTime, time: Option<MediaTime>) -> Duration {
    let seconds = |t: MediaTime| {
        #[allow(clippy::cast_precision_loss)]
        let s = t.num as f64 / t.den.max(1) as f64;
        s
    };
    time.map_or(Duration::ZERO, |time| {
        Duration::try_from_secs_f64(seconds(time) - seconds(start)).unwrap_or(Duration::ZERO)
    })
}

fn render(
    open: &Target,
    index: u64,
    decoded: &Decoded,
    display: Display,
    generation: u64,
) -> Outcome<(PictureHeader, Vec<u8>)> {
    let frame = decoded.frame().ok_or_else(|| {
        Stopped::unavailable(
            PictureFailure::Unreadable,
            "the frame's planes are inconsistent",
        )
    })?;
    let Display {
        view,
        size,
        options,
    } = display;
    let (rendered, (w, h), basis) = if let Some(channel) = display.plane() {
        let plane = frame.plane(channel).ok_or_else(|| {
            Stopped::unavailable(
                PictureFailure::Unsupported,
                "The picture has no selected plane.",
            )
        })?;
        let rendered = picture::render_plane(plane, frame.bit_depth, view, size);
        let basis = ColorBasis {
            matrix: ColorMatrix::Monochrome,
            assumed: false,
        };
        (rendered, plane.size(), basis)
    } else {
        let basis = open.basis(&frame, decoded)?;
        (
            picture::render(&frame, basis, view, size),
            frame.size(),
            basis,
        )
    };
    let (time, output, document_info) = match decoded {
        Decoded::Document(picture) => {
            let info = picture.info().ok_or_else(|| {
                Stopped::unavailable(
                    PictureFailure::Unreadable,
                    "The decoder returned invalid geometry.",
                )
            })?;
            (picture.time, Some(picture.output), Some(Box::new(info)))
        }
        _ => (open.time(index), None, None),
    };
    let header = PictureHeader {
        generation,
        origin: open.origin(),
        frame: index,
        output,
        options,
        document_info,
        time,
        frame_count: open.frame_count(),
        width: rendered.width,
        height: rendered.height,
        frame_width: u32::try_from(w).unwrap_or(u32::MAX),
        frame_height: u32::try_from(h).unwrap_or(u32::MAX),
        view: rendered.view,
        basis,
    };
    Ok((header, rendered.pixels))
}

fn valid(size: PictureBox) -> Outcome<()> {
    if size.is_valid() {
        Ok(())
    } else {
        Err(Stopped::unavailable(
            PictureFailure::InvalidRequest,
            format!(
                "the picture box {}x{} is outside 1x1 to {MAX_PICTURE_WIDTH}x{MAX_PICTURE_HEIGHT}",
                size.width, size.height
            ),
        ))
    }
}

fn out_of_range(index: u64, count: Option<u64>) -> Stopped {
    Stopped::unavailable(
        PictureFailure::OutOfRange,
        match count {
            Some(count) => format!("there is no frame {index}; the stream has {count}"),
            None => format!("there is no frame {index}; the stream ends before it"),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn superseded_samples_are_not_queued() {
        let shared = Shared::default();
        let context = Context {
            shared: &shared,
            serial: 0,
            request_id: 1,
            generation: 1,
            last_progress: Cell::default(),
        };
        shared.submit(2, Request::Stop);
        context.event(Event::PictureSample {
            generation: 1,
            ordinal: 0,
            grain: GrainPolicy::Skip,
            point: SamplePoint {
                plane: SamplePlane::Y,
                x: 0,
                y: 0,
            },
            value: 17,
        });
        assert!(shared.lock().queue.is_empty());

        context.event(Event::PlaybackEnded {
            generation: 1,
            frame: None,
        });
        assert!(matches!(
            shared.lock().queue.front(),
            Some(Item::Event(Envelope {
                body: Event::PlaybackEnded { .. },
                ..
            }))
        ));
    }

    #[test]
    fn a_reversed_pts_does_not_delay_later_pictures() {
        let mut clock = DocumentClock::default();
        for (unit, pts, due) in [(0, 0, 0), (1, 2, 2), (2, 1, 2), (3, 3, 3)] {
            assert_eq!(
                clock.advance(unit, Some(MediaTime { num: pts, den: 1 }), None),
                Duration::from_secs(due)
            );
        }
    }

    #[test]
    fn document_clock_uses_exact_pts_or_signaled_intervals() {
        let ms = |num| Some(MediaTime { num, den: 1000 });
        let mut ivf = DocumentClock::default();
        assert_eq!(ivf.advance(44, ms(120), None), Duration::ZERO);
        assert_eq!(ivf.advance(44, ms(120), None), Duration::ZERO);
        assert_eq!(ivf.advance(92, ms(147), None), Duration::from_millis(27));
        assert_eq!(ivf.advance(61, ms(180), None), Duration::from_millis(60));

        let mut annex_b = DocumentClock::default();
        assert_eq!(annex_b.advance(0, None, ms(40)), Duration::ZERO);
        assert_eq!(annex_b.advance(0, None, ms(40)), Duration::ZERO);
        assert_eq!(
            annex_b.advance(100, None, ms(40)),
            Duration::from_millis(40)
        );

        let mut untimed = DocumentClock::default();
        assert_eq!(untimed.advance(0, None, None), Duration::ZERO);
        assert_eq!(untimed.advance(0, None, None), ASSUMED_INTERVAL);
        assert_eq!(untimed.advance(100, None, None), ASSUMED_INTERVAL * 2);
    }
}
