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
//! Opening a file is not interruptible: a media source runs `ffprobe`
//! (bounded by its timeout), a result file is read once for its index.

mod cache;
mod result;
mod source;

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use studio_domain::preview::{
    ColorBasis, MAX_PICTURE_HEIGHT, MAX_PICTURE_WIDTH, MediaTime, PictureBox, PictureFailure,
    PictureFrame, PictureHeader, PictureOrigin, PictureTarget, PictureView, SourceRef,
};
use studio_domain::protocol::{
    Command, Envelope, ErrorCode, Event, ProtocolError, decode, read_frame, write_message,
    write_picture,
};

use crate::job::{JOB_STACK_BYTES, Stop};
use crate::media::process::Killer;
use crate::picture::{self, Frame, Rendered};
use crate::worker::SessionError;
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
            Command::Probe { .. }
            | Command::Start { .. }
            | Command::Cancel
            | Command::AnalysisOpen { .. }
            | Command::AnalysisPage { .. }
            | Command::AnalysisBytes { .. }
            | Command::AnalysisClose { .. } => {
                shared.reply(
                    id,
                    refusal(
                        ErrorCode::WrongState,
                        "the preview process runs no jobs or analysis",
                    ),
                );
                continue;
            }
            Command::Picture {
                generation,
                target,
                frame,
                view,
                size,
            } => Request::Picture {
                generation,
                target,
                frame,
                view,
                size,
            },
            Command::Play {
                generation,
                target,
                from,
                view,
                size,
            } => Request::Play {
                generation,
                target,
                from,
                view,
                size,
            },
            Command::StopPlayback => Request::Stop,
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

/// Best-effort `request_id` of a frame that did not decode as a command.
fn request_id_of(frame: &[u8]) -> u64 {
    serde_json::from_slice::<serde_json::Value>(frame)
        .ok()
        .and_then(|value| value.get("request_id")?.as_u64())
        .unwrap_or(0)
}

/// One request for the work thread.
#[derive(Debug)]
enum Request {
    Picture {
        generation: u64,
        target: PictureTarget,
        frame: PictureFrame,
        view: PictureView,
        size: PictureBox,
    },
    Play {
        generation: u64,
        target: PictureTarget,
        from: u64,
        view: PictureView,
        size: PictureBox,
    },
    Stop,
}

#[derive(Debug, Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    /// Bumped by every request and by the end of input; work started under
    /// an older value is superseded.
    serial: AtomicU64,
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
        if let Some(killer) = state.abort.take() {
            killer.kill();
        }
        self.notify(state);
    }

    fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
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
    fn take(&self) -> Option<(u64, u64, Request)> {
        let mut state = self.lock();
        loop {
            if state.closed {
                return None;
            }
            if let Some(request) = state.request.take() {
                return Some(request);
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

    /// Offers a picture; it replaces any picture not yet written.
    fn picture(&self, header: PictureHeader, pixels: Vec<u8>) {
        let mut state = self.shared.lock();
        if !state.broken && !self.superseded() {
            state.picture = Some((self.request_id, header, pixels));
        }
        self.shared.notify(state);
    }

    /// Queues an event behind any picture already offered.
    fn event(&self, event: Event) {
        let mut state = self.shared.lock();
        state.flush_picture();
        state.push(Envelope::new(self.request_id, event));
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
    while let Some((serial, request_id, request)) = shared.take() {
        let context = Context {
            shared,
            serial,
            request_id,
        };
        match request {
            Request::Picture {
                generation,
                target,
                frame,
                view,
                size,
            } => {
                let answer = previews.picture(&context, generation, &target, frame, view, size);
                if let Err(Stopped::Unavailable(code, message)) = answer {
                    unavailable(&context, generation, code, message);
                }
            }
            Request::Play {
                generation,
                target,
                from,
                view,
                size,
            } => previews.play(&context, generation, &target, from, view, size),
            Request::Stop => {}
        }
    }
}

fn unavailable(context: &Context<'_>, generation: u64, code: PictureFailure, message: String) {
    if !context.superseded() {
        context.event(Event::PictureUnavailable {
            generation,
            code,
            message,
        });
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
        }
    }
}

/// An open file pictures are taken from.
enum Target {
    Source(Box<source::SourcePictures>),
    Result(Box<result::ResultPictures>),
}

impl Target {
    fn open(target: &PictureTarget) -> Outcome<Self> {
        match target {
            PictureTarget::Source { source } => {
                source::SourcePictures::open(source).map(|s| Self::Source(Box::new(s)))
            }
            PictureTarget::Result { path, source } => {
                result::ResultPictures::open(path, source.as_ref())
                    .map(|r| Self::Result(Box::new(r)))
            }
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
    fn check(&mut self) -> Outcome<()> {
        match self {
            Self::Source(s) => s.check(),
            Self::Result(r) => r.check(),
        }
    }

    const fn origin(&self) -> PictureOrigin {
        match self {
            Self::Source(_) => PictureOrigin::Source,
            Self::Result(_) => PictureOrigin::Result,
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

    fn basis(&self, frame: &Frame<'_>) -> ColorBasis {
        let (w, h) = frame.size();
        let matrix = match self {
            Self::Source(s) => s.matrix(),
            Self::Result(r) => r.matrix(),
        };
        picture::basis(
            matrix,
            frame.chroma.is_none(),
            u32::try_from(w).unwrap_or(u32::MAX),
            u32::try_from(h).unwrap_or(u32::MAX),
        )
    }

    /// The frames in one second, for the poster rule.
    fn second(&self) -> u64 {
        match self {
            Self::Source(s) => s.second(),
            Self::Result(r) => r.second(),
        }
    }

    /// Makes `index` the next frame [`Self::next`] returns.
    fn seek(&mut self, context: &Context<'_>, index: u64) -> Outcome<()> {
        match self {
            Self::Source(s) => s.seek(context, index),
            Self::Result(r) => r.seek(context, index),
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
    fn poster(&mut self, context: &Context<'_>) -> Outcome<(u64, Option<Decoded>)> {
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
        self.seek(context, 0)?;
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

/// Which file a target is; requests for the same one share its sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TargetKey {
    Source(SourceRef),
    Result(studio_domain::job::WirePath, Option<SourceRef>),
}

impl TargetKey {
    fn of(target: &PictureTarget) -> Self {
        match target {
            PictureTarget::Source { source } => Self::Source(source.clone()),
            PictureTarget::Result { path, source } => Self::Result(path.clone(), source.clone()),
        }
    }
}

/// An open target.
struct Entry {
    key: TargetKey,
    target: Target,
    /// The poster's index, once chosen.
    poster: Option<u64>,
}

#[derive(Default)]
struct Previews {
    /// Open files, the most recently used last.
    targets: Vec<Entry>,
    cache: cache::Cache<TargetKey>,
}

/// The open entry for `target`, opened or checked now.
fn open<'a>(
    targets: &'a mut Vec<Entry>,
    cache: &mut cache::Cache<TargetKey>,
    target: &PictureTarget,
) -> Outcome<&'a mut Entry> {
    let key = TargetKey::of(target);
    if let Some(at) = targets.iter().position(|entry| entry.key == key) {
        let mut entry = targets.remove(at);
        if let Err(stopped) = entry.target.check() {
            cache.forget(&key);
            return Err(stopped);
        }
        targets.push(entry);
    } else {
        let opened = Target::open(target)?;
        cache.forget(&key);
        targets.push(Entry {
            key,
            target: opened,
            poster: None,
        });
        if targets.len() > OPEN_TARGETS {
            // Closing kills its `ffmpeg` and stops its decoder.
            targets.remove(0);
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
    fn picture(
        &mut self,
        context: &Context<'_>,
        generation: u64,
        target: &PictureTarget,
        frame: PictureFrame,
        view: PictureView,
        size: PictureBox,
    ) -> Outcome<()> {
        valid(size)?;
        let entry = open(&mut self.targets, &mut self.cache, target)?;
        let key = entry.key.clone();
        let (index, decoded) = match (frame, entry.poster) {
            (PictureFrame::Index { index }, _) | (PictureFrame::Poster, Some(index)) => {
                (index, None)
            }
            (PictureFrame::Poster, None) => {
                let (index, decoded) = entry.target.poster(context)?;
                entry.poster = Some(index);
                (index, decoded)
            }
        };
        let open = &mut entry.target;
        let header =
            |open: &Target, rendered: &Rendered, frame_size: (u32, u32), basis| PictureHeader {
                generation,
                origin: open.origin(),
                frame: index,
                time: open.time(index),
                frame_count: open.frame_count(),
                width: rendered.width,
                height: rendered.height,
                frame_width: frame_size.0,
                frame_height: frame_size.1,
                view: rendered.view,
                basis,
            };
        if let Some((rendered, frame_size, basis)) = self.cache.get(&key, index, view, size) {
            let header = header(open, &rendered, frame_size, basis);
            context.picture(header, rendered.pixels);
            return Ok(());
        }
        let decoded = if let Some(decoded) = decoded {
            decoded
        } else {
            open.seek(context, index)?;
            match open.next(context)? {
                Some((at, decoded)) if at == index => decoded,
                Some((at, _)) => {
                    return Err(Stopped::unavailable(
                        PictureFailure::Unreadable,
                        format!("frame {at} was decoded where frame {index} was expected"),
                    ));
                }
                None => return Err(out_of_range(index, open.frame_count())),
            }
        };
        context.check()?;
        let (rendered, frame_size, basis) = render(open, &decoded, view, size)?;
        let header = header(open, &rendered, frame_size, basis);
        self.cache
            .put(key, index, view, size, &rendered, frame_size, basis);
        context.picture(header, rendered.pixels);
        Ok(())
    }

    fn play(
        &mut self,
        context: &Context<'_>,
        generation: u64,
        target: &PictureTarget,
        from: u64,
        view: PictureView,
        size: PictureBox,
    ) {
        let mut last = None;
        let played = self.playback(context, generation, target, from, view, size, &mut last);
        match played {
            Err(Stopped::Unavailable(code, message)) if last.is_none() => {
                unavailable(context, generation, code, message);
            }
            // Ended, stopped, superseded, or failed after some frames.
            _ => {
                if !context.shared.lock().closed {
                    context.event(Event::PlaybackEnded {
                        generation,
                        frame: last,
                    });
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn playback(
        &mut self,
        context: &Context<'_>,
        generation: u64,
        target: &PictureTarget,
        from: u64,
        view: PictureView,
        size: PictureBox,
        last: &mut Option<u64>,
    ) -> Outcome<()> {
        valid(size)?;
        let open = &mut open(&mut self.targets, &mut self.cache, target)?.target;
        let Some(start) = open.time(from) else {
            return Err(Stopped::unavailable(
                PictureFailure::NoTiming,
                "the stream's frame timing is not known, so it cannot be played; a raw OBU \
                 stream has none unless its source is given",
            ));
        };
        open.seek(context, from)?;
        let started = Instant::now();
        let mut shown = started;
        // The newest frame that was skipped as late. Playback must still end
        // on the stream's last frame, so it is sent if nothing follows it.
        let mut skipped = None;
        loop {
            context.check()?;
            let Some((index, decoded)) = open.next(context)? else {
                if let Some((index, decoded)) = skipped.take() {
                    let (rendered, frame_size, basis) = render(open, &decoded, view, size)?;
                    let header = header_of(generation, open, index, &rendered, frame_size, basis);
                    context.picture(header, rendered.pixels);
                    *last = Some(index);
                }
                if last.is_none() {
                    return Err(out_of_range(from, open.frame_count()));
                }
                return Ok(());
            };
            let due = started + elapsed(start, open.time(index));
            let next_due = started + elapsed(start, open.time(index + 1));
            let now = Instant::now();
            // Late: the next frame is already due. Skipped, unless nothing
            // was shown for a while.
            if last.is_some() && now >= next_due && now < shown + MIN_PLAYBACK_INTERVAL {
                skipped = Some((index, decoded));
                continue;
            }
            skipped = None;
            let (rendered, frame_size, basis) = render(open, &decoded, view, size)?;
            drop(decoded);
            if !context.wait_until(due) {
                return Err(Stopped::Superseded);
            }
            let header = header_of(generation, open, index, &rendered, frame_size, basis);
            context.picture(header, rendered.pixels);
            *last = Some(index);
            shown = Instant::now();
        }
    }
}

/// The header of a played picture.
fn header_of(
    generation: u64,
    open: &Target,
    index: u64,
    rendered: &Rendered,
    frame_size: (u32, u32),
    basis: ColorBasis,
) -> PictureHeader {
    PictureHeader {
        generation,
        origin: open.origin(),
        frame: index,
        time: open.time(index),
        frame_count: open.frame_count(),
        width: rendered.width,
        height: rendered.height,
        frame_width: frame_size.0,
        frame_height: frame_size.1,
        view: rendered.view,
        basis,
    }
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
    decoded: &Decoded,
    view: PictureView,
    size: PictureBox,
) -> Outcome<(Rendered, (u32, u32), ColorBasis)> {
    let frame = decoded.frame().ok_or_else(|| {
        Stopped::unavailable(
            PictureFailure::Unreadable,
            "the frame's planes are inconsistent",
        )
    })?;
    let basis = open.basis(&frame);
    let (w, h) = frame.size();
    let frame_size = (
        u32::try_from(w).unwrap_or(u32::MAX),
        u32::try_from(h).unwrap_or(u32::MAX),
    );
    Ok((
        picture::render(&frame, basis, view, size),
        frame_size,
        basis,
    ))
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
