// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The picture area at the top of the card and the row thumbnails: which
//! picture is shown, and which requests the one preview engine process gets.
//!
//! [`Preview`] is a state machine without processes or windows. The
//! [`Scene`] says what the selected file can show ([`Subject`]); the user's
//! [`Input`] says which frame, side and view; engine answers arrive as
//! [`Picture`]s. Every change returns the [`Effect`]s (commands) to send.
//!
//! Transport rules:
//! - Generations increase with every `Picture` or `Play` sent. A picture is
//!   shown only when it answers the newest stage request and shows the side
//!   the user is looking at; anything else is dropped.
//! - At most one request is in flight. A new Analyze seek or playback
//!   request replaces older work at once. Encode stepping coalesces to the
//!   latest position while it waits for a still picture.
//! - Thumbnails go out only when no stage request waits and nothing plays.
//! - Answered pictures are kept in a [`CACHE_BYTES`] cache, least recently
//!   used first out, and dropped once their file leaves the list.
//! - Nothing is sent while the window is hidden; playback stops then.
//! - A process that dies is restarted for the next request, at most
//!   [`MAX_DEATHS`] times in a row without an answer in between.
//!
//! A source picture never stands in for a result: the side label is taken
//! from the shown picture's own origin.

use studio_domain::analysis::SourceDigest;
use studio_domain::layers::{LayerDecodeSelection, LayerDecodeStatus};
use studio_domain::preview::{
    GrainPolicy, OutputEvent, PictureBox, PictureFailure, PictureFrame, PictureOptions,
    PictureOrigin, PictureSurface, PictureTarget, PictureView, SourceRef,
};
use studio_domain::protocol::Command;

use crate::engine::Picture;
use crate::present::preview as text;
use crate::queue::ItemId;

mod cache;
mod document;
mod reference;
mod scene;
#[cfg(test)]
mod tests;
mod viewer;

use cache::Cache;
use reference::ReferencePreview;
pub use scene::{Scene, Subject, scene};
pub(crate) use viewer::HistoryView;
use viewer::Viewer;
pub(crate) use viewer::blocks::{BlockPicture, BlockViewport};
pub use viewer::{ViewerView, ZOOMS};

/// The memory answered pictures may hold, in bytes: about a dozen
/// full-size (1280x720) pictures, or many more stage-sized ones.
pub const CACHE_BYTES: usize = 32 * 1024 * 1024;

/// How many times in a row the preview process may die before the preview
/// gives up for this session.
pub const MAX_DEATHS: u32 = 3;

/// A row thumbnail's size in logical pixels; the window draws it this size.
pub const THUMB_LOGICAL: (f32, f32) = (64.0, 36.0);

/// Something the user did to the picture area, or the window around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// The picture area and a row thumbnail in physical pixels.
    Resized {
        /// The picture area.
        stage: PictureBox,
        /// A row thumbnail.
        thumb: PictureBox,
    },
    /// The window can be seen (`false`: minimized).
    Visible(bool),
    /// Press and hold (`true`) or release (`false`): show the original.
    Compare(bool),
    /// Move this many frames.
    Step(i64),
    /// Go to this frame.
    Seek(u64),
    /// Go to this one-based output number in Analyze.
    SeekOutput(u64),
    /// Cancel the current Analyze decode request.
    CancelDecode,
    /// Go to the last frame.
    End,
    /// Play, or pause.
    PlayPause,
    /// Select the RGB presentation or a decoded plane in Analyze.
    Surface(PictureSurface),
    /// Set the Analyze viewer scale. Zero fits the picture.
    Zoom(u8),
    /// Select the Analyze film grain policy.
    Grain(GrainPolicy),
    /// Read the sample at a point relative to the painted image.
    PickSample {
        /// Physical pixels from the left edge of the image.
        x: i32,
        /// Physical pixels from the top edge of the image.
        y: i32,
        /// The painted image width in physical pixels.
        width: i32,
        /// The painted image height in physical pixels.
        height: i32,
    },
    /// Read a sample at typed coordinates in the selected visible plane.
    ReadSample {
        /// The column, when the text is a valid integer.
        x: Option<u32>,
        /// The row, when the text is a valid integer.
        y: Option<u32>,
    },
    /// Switch between fitting the frame and actual pixels.
    ToggleNative,
    /// Move the actual-pixels view by this many physical pixels.
    Pan {
        /// To the right.
        dx: i64,
        /// Down.
        dy: i64,
    },
}

/// A command for the preview process. Its request id is its generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effect {
    /// The envelope's request id.
    pub request_id: u64,
    /// The command.
    pub command: Command,
}

/// What the window must redraw.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Dirty {
    /// The picture area's picture.
    pub picture: bool,
    /// Its labels, state and controls.
    pub view: bool,
    /// The row thumbnails.
    pub thumbs: bool,
}

impl Dirty {
    /// Anything at all.
    #[must_use]
    pub const fn any(self) -> bool {
        self.picture || self.view || self.thumbs
    }
}

/// What the picture area is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageState {
    /// No file: the drop zone.
    None,
    /// Waiting for a picture; a quiet placeholder.
    Loading,
    /// A picture.
    Picture,
    /// No picture, with why.
    Unavailable,
}

/// The picture area as the window draws it: one flag per control.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct StageView {
    /// What it shows.
    pub state: StageState,
    /// The picture, in [`StageState::Picture`].
    pub picture: Option<Picture>,
    /// The picture is actual pixels, one per physical pixel.
    pub native: bool,
    /// Which side is visible, and what else is true of it.
    pub badge: String,
    /// What pressing does, or that the original is on its way.
    pub hint: String,
    /// The placeholder's or the failure's text.
    pub message: String,
    /// The transport row (stepping, scrubber, play, actual pixels) is shown.
    pub transport: bool,
    /// Its controls can be used now.
    pub navigable: bool,
    /// The frame count, `0` when it is not known.
    pub frames: u64,
    /// The frame shown, zero-based.
    pub position: u64,
    /// `12 / 300`.
    pub frame_text: String,
    /// `0:00.40`, or empty when the time is not known.
    pub time_text: String,
    /// Play is offered.
    pub can_play: bool,
    /// Playing now.
    pub playing: bool,
    /// The timing assumption or a playback problem.
    pub play_note: String,
    /// Why Analyze could not use the entered output number.
    pub seek_error: String,
    /// The current file scan or decode work in Analyze.
    pub decode_work: String,
    /// Output identity for the current Analyze picture.
    pub output: Option<OutputEvent>,
    /// Precise controls for Analyze.
    pub viewer: ViewerView,
    /// Press and hold shows the original.
    pub can_compare: bool,
    /// Showing the original while held.
    pub comparing: bool,
    /// The actual-pixels toggle is offered.
    pub can_native: bool,
    /// The actual-pixels toggle is on.
    pub native_on: bool,
    /// The whole state in one sentence, for screen readers.
    pub accessible: String,
}

/// Which request a picture answers.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Key {
    target: PictureTarget,
    frame: PictureFrame,
    view: PictureView,
    options: PictureOptions,
    size: PictureBox,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Purpose {
    Stage,
    Thumbnail,
    Reference,
}

/// The request in flight.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Sent {
    Picture {
        generation: u64,
        key: Key,
        purpose: Purpose,
    },
    Play {
        generation: u64,
        target: PictureTarget,
        view: PictureView,
        options: PictureOptions,
        size: PictureBox,
        /// `StopPlayback` was sent; `PlaybackEnded` is due.
        stopping: bool,
    },
}

/// What the user wants the picture area to show next.
enum Want {
    Picture(Key),
    Play {
        target: PictureTarget,
        from: u64,
        view: PictureView,
        options: PictureOptions,
    },
}

/// The picture on the stage and what it is.
#[derive(Debug, Clone)]
struct Shown {
    picture: Picture,
    /// `None` for a live picture.
    target: Option<PictureTarget>,
    size: PictureBox,
    /// It answered a poster request.
    poster: bool,
}

impl Shown {
    fn shows(&self, key: &Key) -> bool {
        self.target.as_ref() == Some(&key.target)
            && self.size == key.size
            && self.picture.header.view == key.view
            && self.picture.header.options == key.options
            && match key.frame {
                PictureFrame::Poster => self.poster,
                PictureFrame::Index { index } => self.picture.header.frame == index,
            }
    }
}

/// The picture area's state and the preview process's request queue. The
/// flags are independent (sized, visible, comparing, playing).
#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct Preview {
    scene: Scene,
    stage_box: PictureBox,
    thumb_box: PictureBox,
    /// A size has been reported; nothing is requested before.
    sized: bool,
    visible: bool,
    /// The frame asked for; `None` for the poster.
    position: Option<u64>,
    /// The frame count as last learned.
    frames: Option<u64>,
    /// Actual pixels, with the view's top-left corner.
    native: Option<(u32, u32)>,
    comparing: bool,
    /// The frame compared, fixed when the press started.
    compare_frame: Option<u64>,
    /// What was shown when the press started, to come back to.
    saved: Option<Shown>,
    playing: bool,
    /// The scrubber moved while playing: play on from there.
    replay: bool,
    play_note: Option<String>,
    shown: Option<Shown>,
    stage_failure: Option<String>,
    document_progress: String,
    document_notice: String,
    layer_decode_feedback: Option<(u64, LayerDecodeSelection, LayerDecodeStatus)>,
    /// A newer prefix available for the next seek or playback request.
    document_digest: Option<Box<SourceDigest>>,
    /// The last output while Next checks for the end of a stream with an unknown count.
    before_next: Option<Shown>,
    viewer: Viewer,
    /// The stage request that failed, not asked again.
    failed: Option<Key>,
    /// The newest live picture of the subject's job.
    live: Option<Picture>,
    live_note: Option<String>,
    next_generation: u64,
    /// The newest stage request sent (`Picture` or `Play`).
    stage_generation: u64,
    inflight: Option<Sent>,
    /// Deaths of the process without an answer in between.
    deaths: u32,
    /// The preview gave up for this session, with why.
    broken: Option<String>,
    cache: Cache,
    thumb_failures: Vec<Key>,
    reference: Option<ReferencePreview>,
    dirty: Dirty,
}

impl Default for Preview {
    fn default() -> Self {
        Self::new()
    }
}

impl Preview {
    /// Nothing selected, nothing requested.
    #[must_use]
    pub fn new() -> Self {
        Self {
            scene: Scene::default(),
            stage_box: PictureBox::clamped(1, 1),
            thumb_box: PictureBox::clamped(1, 1),
            sized: false,
            visible: true,
            position: None,
            frames: None,
            native: None,
            comparing: false,
            compare_frame: None,
            saved: None,
            playing: false,
            replay: false,
            play_note: None,
            shown: None,
            stage_failure: None,
            document_progress: String::new(),
            document_notice: String::new(),
            layer_decode_feedback: None,
            document_digest: None,
            before_next: None,
            viewer: Viewer::default(),
            failed: None,
            live: None,
            live_note: None,
            next_generation: 1,
            stage_generation: 0,
            inflight: None,
            deaths: 0,
            broken: None,
            cache: Cache::default(),
            thumb_failures: Vec::new(),
            reference: None,
            dirty: Dirty::default(),
        }
    }

    /// The box live pictures are asked for at a job's start.
    #[must_use]
    pub const fn live_box(&self) -> PictureBox {
        self.stage_box
    }

    /// The bytes the cache holds.
    #[must_use]
    pub const fn cached_bytes(&self) -> usize {
        self.cache.bytes
    }

    /// What changed since the last call.
    pub fn take_dirty(&mut self) -> Dirty {
        std::mem::take(&mut self.dirty)
    }

    /// The app state changed: follow the selected file and the list.
    pub fn sync(&mut self, mut scene: Scene) -> Vec<Effect> {
        let mut out = Vec::new();
        if scene.thumbs != self.scene.thumbs {
            self.dirty.thumbs = true;
        }
        if let (
            Some(Subject::Document {
                document,
                path,
                fingerprint,
                digest,
                selection,
            }),
            Some(Subject::Document {
                document: new_document,
                path: new_path,
                fingerprint: new_fingerprint,
                digest: new_digest,
                selection: new_selection,
            }),
        ) = (&mut self.scene.stage, &mut scene.stage)
            && document == new_document
            && path == new_path
            && fingerprint == new_fingerprint
            && selection == new_selection
            && new_digest.bytes > digest.bytes
        {
            if new_digest.bytes == new_fingerprint.len {
                // A frame export needs a picture verified against the complete source.
                // Keep the selected output and view while the worker verifies it.
                digest.clone_from(new_digest);
                self.document_digest = None;
            } else {
                self.document_digest = Some(std::mem::replace(new_digest, digest.clone()));
            }
        }
        if scene.stage != self.scene.stage {
            let same_document = matches!(
                (&self.scene.stage, &scene.stage),
                (Some(Subject::Document {
                    document: old, path: old_path, fingerprint: old_stamp, ..
                }), Some(Subject::Document {
                    document: new, path: new_path, fingerprint: new_stamp, ..
                })) if old == new && old_path == new_path && old_stamp == new_stamp
            );
            let previous_grain = self.viewer.options.grain;
            self.release_document(&mut out);
            self.layer_decode_feedback = None;
            let keep = matches!(
                (&self.scene.stage, &scene.stage),
                (Some(Subject::Source { item: a, .. }), Some(Subject::Live { item: b, .. })) if a == b
            );
            self.position = matches!(scene.stage, Some(Subject::Document { .. })).then_some(0);
            self.frames = scene.stage.as_ref().and_then(Subject::frames);
            self.comparing = false;
            self.compare_frame = None;
            self.saved = None;
            self.playing = false;
            self.replay = false;
            self.play_note = None;
            self.document_progress.clear();
            self.document_notice.clear();
            self.document_digest = None;
            self.before_next = None;
            self.viewer = Viewer::default();
            if same_document {
                self.viewer.options.grain = previous_grain;
            }
            self.stage_failure = None;
            self.failed = None;
            self.live = None;
            self.live_note = None;
            if !keep {
                self.shown = None;
            }
            self.scene.stage = scene.stage;
            if self.native.is_some() {
                self.native = Some(self.centre());
            }
            self.dirty.picture = true;
            self.dirty.view = true;
        }
        self.scene.thumbs = scene.thumbs;
        let scene = &self.scene;
        self.cache.retain(|k| referenced(scene, &k.target));
        self.thumb_failures.retain(|k| referenced(scene, &k.target));
        out.extend(self.pump());
        out
    }

    /// The user did something to the picture area or the window.
    pub fn input(&mut self, input: Input) -> Vec<Effect> {
        match input {
            Input::Resized { stage, thumb } => {
                if (stage, thumb) != (self.stage_box, self.thumb_box) || !self.sized {
                    self.dirty.thumbs |= thumb != self.thumb_box;
                    self.stage_box = stage;
                    self.thumb_box = thumb;
                    self.sized = true;
                    if let Some(origin) = self.native {
                        self.native = Some(self.clamp_origin(origin));
                    }
                }
            }
            Input::Visible(visible) => {
                self.visible = visible;
                if !visible && self.playing {
                    self.playing = false;
                    self.dirty.view = true;
                }
                if !visible && self.document_id().is_some() {
                    return self.cancel_document_work();
                }
            }
            Input::Compare(on) => self.compare(on),
            Input::Step(delta) => {
                // Stepping is frame by frame: it pauses.
                self.playing = false;
                if let Some(frame) = self.current_frame() {
                    let to = if delta < 0 {
                        frame.saturating_sub(delta.unsigned_abs())
                    } else {
                        frame.saturating_add(delta.unsigned_abs())
                    };
                    let before_next =
                        if delta == 1 && self.document_id().is_some() && self.frames.is_none() {
                            self.shown.clone()
                        } else {
                            None
                        };
                    self.seek(to);
                    self.before_next = before_next;
                }
            }
            Input::Seek(frame) => self.seek(frame),
            Input::SeekOutput(output) => self.seek_output(output),
            Input::CancelDecode => return self.cancel_document_work(),
            Input::Surface(surface) => self.choose_surface(surface),
            Input::Zoom(factor) => self.choose_zoom(factor),
            Input::Grain(grain) => self.choose_grain(grain),
            Input::PickSample {
                x,
                y,
                width,
                height,
            } => return self.pick_sample(x, y, width, height),
            Input::ReadSample { x, y } => return self.read_sample(x, y),
            Input::End => {
                if let Some(count) = self.frames {
                    self.seek(count.saturating_sub(1));
                }
            }
            Input::PlayPause => {
                if self.playing {
                    if self.document_id().is_some() && self.document_decoding() {
                        return self.cancel_document_decode();
                    }
                    self.playing = false;
                } else if self.can_play() {
                    self.promote_document_digest();
                    self.before_next = None;
                    if let (Some(frame), Some(count)) = (self.current_frame(), self.frames)
                        && frame.saturating_add(1) >= count
                    {
                        self.position = Some(0);
                    }
                    self.playing = true;
                }
                self.dirty.view = true;
            }
            Input::ToggleNative => {
                if self.can_native() {
                    self.native = match self.native {
                        Some(_) => None,
                        None => Some(self.centre()),
                    };
                    self.dirty.view = true;
                }
            }
            Input::Pan { dx, dy } => {
                if self.document_id().is_some() {
                    self.pan_document(dx, dy);
                } else if let Some((x, y)) = self.native {
                    let moved = |at: u32, by: i64| {
                        let at = i64::from(at).saturating_add(by).max(0);
                        u32::try_from(at).unwrap_or(u32::MAX)
                    };
                    self.native = Some(self.clamp_origin((moved(x, dx), moved(y, dy))));
                }
            }
        }
        self.pump()
    }

    /// A picture from the preview process.
    pub fn picture(&mut self, picture: Picture) -> Vec<Effect> {
        let generation = picture.header.generation;
        if !self.valid_document_answer(&picture.header) {
            return self.refused("The engine sent invalid picture geometry or output identity.");
        }
        match self.inflight.clone() {
            Some(Sent::Picture {
                generation: sent,
                key,
                purpose,
            }) if sent == generation => {
                self.deaths = 0;
                self.inflight = None;
                // Cached under the view actually produced: the engine may
                // have moved an origin back inside the frame.
                let key = Key {
                    view: picture.header.view,
                    ..key
                };
                if purpose == Purpose::Reference {
                    self.show_reference(&key, picture);
                    return self.pump();
                }
                if referenced(&self.scene, &key.target) {
                    self.cache.put(key.clone(), picture.clone());
                }
                if purpose == Purpose::Thumbnail {
                    self.dirty.thumbs = true;
                } else if sent == self.stage_generation && self.on_side(&key.target) {
                    if let Some(count) = picture.header.frame_count {
                        self.frames = Some(count);
                    }
                    if let (PictureView::Native { x, y }, Some(_)) = (key.view, self.native) {
                        self.native = Some((x, y));
                    }
                    self.show(Shown {
                        poster: key.frame == PictureFrame::Poster,
                        target: Some(key.target),
                        size: key.size,
                        picture,
                    });
                }
            }
            Some(Sent::Play {
                generation: sent,
                target,
                size,
                ..
            }) if sent == generation => {
                self.deaths = 0;
                if self.on_side(&target)
                    && (!matches!(target, PictureTarget::Document { .. }) || self.playing)
                {
                    if let Some(count) = picture.header.frame_count {
                        self.frames = Some(count);
                    }
                    self.position = Some(picture.header.frame);
                    if matches!(target, PictureTarget::Document { .. })
                        && let Some(Sent::Play { view, .. }) = &mut self.inflight
                    {
                        // Frame geometry can change while the same playback request runs.
                        *view = picture.header.view;
                    }
                    self.show(Shown {
                        picture,
                        target: Some(target),
                        size,
                        poster: false,
                    });
                }
            }
            // Stale: an older request, or one that was replaced.
            _ => {}
        }
        self.pump()
    }

    /// The preview process could not produce a requested picture.
    pub fn unavailable(
        &mut self,
        generation: u64,
        code: PictureFailure,
        message: &str,
    ) -> Vec<Effect> {
        if self.sample_unavailable(generation, code, message) {
            return Vec::new();
        }
        match self.inflight.clone() {
            Some(Sent::Picture {
                generation: sent,
                key,
                purpose,
            }) if sent == generation => {
                self.deaths = 0;
                self.inflight = None;
                if purpose == Purpose::Reference {
                    self.reference_failed(code, message);
                } else if purpose == Purpose::Thumbnail {
                    self.thumb_failures.push(key);
                } else if sent == self.stage_generation && self.on_side(&key.target) {
                    self.reject_layer_decode(&key.target, code, message);
                    self.stage_failed(key, code, message);
                }
            }
            Some(Sent::Play {
                generation: sent,
                target,
                ..
            }) if sent == generation => {
                self.deaths = 0;
                self.inflight = None;
                self.playing = false;
                if self.on_side(&target) {
                    self.reject_layer_decode(&target, code, message);
                }
                if matches!(target, PictureTarget::Document { .. }) {
                    let detail = if code == PictureFailure::SourceChanged {
                        text::DOCUMENT_CHANGED.to_owned()
                    } else {
                        text::playback_failure(code, message)
                    };
                    if code == PictureFailure::PartialCoverage {
                        self.partial_coverage(detail);
                    } else {
                        self.fail_document(detail);
                    }
                } else {
                    self.play_note = Some(text::playback_failure(code, message));
                    self.dirty.view = true;
                }
            }
            _ => {}
        }
        self.pump()
    }

    fn stage_failed(&mut self, key: Key, code: PictureFailure, message: &str) {
        if code == PictureFailure::PartialCoverage {
            if let Some(previous) = self.before_next.take() {
                self.position = Some(previous.picture.header.frame);
                self.show(previous);
            }
            self.partial_coverage(message.to_owned());
            return;
        }
        if let (PictureFailure::OutOfRange, PictureFrame::Index { index }) = (code, key.frame)
            && let Some(previous) = self.before_next.take()
            && previous.picture.header.frame.checked_add(1) == Some(index)
        {
            self.position = Some(previous.picture.header.frame);
            self.frames = Some(index);
            self.show(previous);
            return;
        }
        self.before_next = None;
        // Encode keeps its existing fallback to the preceding index.
        if let (PictureFailure::OutOfRange, PictureFrame::Index { index }) = (code, key.frame)
            && index > 0
            && !matches!(key.target, PictureTarget::Document { .. })
        {
            self.frames = Some(self.frames.map_or(index, |n| n.min(index)));
            self.position = Some(index - 1);
            self.dirty.view = true;
            return;
        }
        let result = matches!(key.target, PictureTarget::Result { .. });
        self.stage_failure = Some(
            if matches!(key.target, PictureTarget::Document { .. })
                && code == PictureFailure::SourceChanged
            {
                text::DOCUMENT_CHANGED.into()
            } else {
                text::picture_failure(code, message, result)
            },
        );
        self.document_progress.clear();
        self.failed = Some(key);
        self.shown = None;
        self.playing = false;
        self.dirty.picture = true;
        self.dirty.view = true;
    }

    /// Playback stopped.
    pub fn playback_ended(&mut self, generation: u64, frame: Option<u64>) -> Vec<Effect> {
        if let Some(Sent::Play {
            generation: sent,
            stopping,
            target,
            ..
        }) = &self.inflight
            && *sent == generation
        {
            // After a stop the app already knows where the user is; only
            // the natural end says where playback got to.
            if let (Some(frame), false) = (frame, *stopping) {
                self.position = Some(frame);
                if matches!(target, PictureTarget::Document { .. }) {
                    self.frames = frame.checked_add(1);
                }
            }
            self.document_progress.clear();
            self.inflight = None;
            self.playing = false;
            self.dirty.view = true;
        }
        self.pump()
    }

    /// The preview process refused a command: the two ends disagree, so
    /// nothing more is asked of it.
    pub fn refused(&mut self, message: &str) -> Vec<Effect> {
        self.give_up(text::refused(message))
    }

    /// The preview process could not be started.
    pub fn spawn_failed(&mut self, why: &str) -> Vec<Effect> {
        self.give_up(text::not_started(why))
    }

    /// The preview process is gone. Unless it speaks another protocol or
    /// died too often, the next request starts a new one.
    pub fn process_ended(&mut self, why: &str, incompatible: bool) -> Vec<Effect> {
        let reference = matches!(
            self.inflight.take(),
            Some(Sent::Picture {
                purpose: Purpose::Reference,
                ..
            })
        );
        if self.playing {
            self.playing = false;
            self.dirty.view = true;
        }
        if incompatible {
            return self.give_up(text::incompatible(why));
        }
        self.deaths += 1;
        if self.deaths > MAX_DEATHS {
            return self.give_up(text::stopped(why));
        }
        if reference {
            self.set_reference_error(text::stopped(why));
            return self.pump();
        }
        if self.document_id().is_some() {
            self.fail_document(text::stopped(why));
            return Vec::new();
        }
        self.pump()
    }

    fn give_up(&mut self, why: String) -> Vec<Effect> {
        self.inflight = None;
        self.playing = false;
        if self.document_id().is_some() {
            self.fail_document(why.clone());
        }
        self.set_reference_error(why.clone());
        self.broken = Some(why);
        self.dirty.view = true;
        Vec::new()
    }

    /// A live picture from job `job`. Kept only while the selected file is
    /// that job's.
    pub fn live(&mut self, job: u64, picture: Picture) {
        if !matches!(self.scene.stage, Some(Subject::Live { job: j, .. }) if j == job) {
            return;
        }
        if let Some(count) = picture.header.frame_count {
            self.frames = Some(count);
        }
        self.live_note = None;
        if !self.comparing {
            self.shown = Some(Shown {
                picture: picture.clone(),
                target: None,
                size: self.stage_box,
                poster: false,
            });
            self.dirty.picture = true;
        }
        self.live = Some(picture);
        self.dirty.view = true;
    }

    /// Job `job` cannot send live pictures.
    pub fn live_unavailable(&mut self, job: u64, message: &str) {
        if matches!(self.scene.stage, Some(Subject::Live { job: j, .. }) if j == job) {
            self.live_note = Some(text::live_failure(message));
            self.dirty.view = true;
        }
    }

    /// The row thumbnail of `item`, when one has arrived.
    #[must_use]
    pub fn thumbnail(&self, item: ItemId) -> Option<&Picture> {
        let (_, source) = self.scene.thumbs.iter().find(|(i, _)| *i == item)?;
        self.cache.peek(&self.thumb_key(source))
    }

    fn thumb_key(&self, source: &SourceRef) -> Key {
        Key {
            target: PictureTarget::Source {
                source: source.clone(),
            },
            frame: PictureFrame::Poster,
            view: PictureView::Fit,
            options: PictureOptions::default(),
            size: self.thumb_box,
        }
    }

    fn compare(&mut self, on: bool) {
        if on == self.comparing {
            return;
        }
        if on {
            let Some(frame) = self.shown.as_ref().map(|s| s.picture.header.frame) else {
                return;
            };
            if !self.can_compare() {
                return;
            }
            self.comparing = true;
            self.compare_frame = Some(frame);
            self.saved = self.shown.clone();
        } else {
            self.comparing = false;
            self.compare_frame = None;
            let saved = self.saved.take();
            if matches!(self.scene.stage, Some(Subject::Live { .. })) {
                if let Some(live) = self.live.clone() {
                    self.shown = Some(Shown {
                        picture: live,
                        target: None,
                        size: self.stage_box,
                        poster: false,
                    });
                } else {
                    self.shown = saved;
                }
                self.dirty.picture = true;
            } else if let (Some(saved), Some(Want::Picture(key))) = (saved, self.want())
                && saved.target.as_ref() == Some(&key.target)
                && saved.picture.header.frame == self.current_frame().unwrap_or(u64::MAX)
                && saved.picture.header.view == key.view
            {
                self.shown = Some(saved);
                self.dirty.picture = true;
            }
        }
        self.dirty.view = true;
    }

    fn promote_document_digest(&mut self) {
        if let Some(Subject::Document { digest, .. }) = &mut self.scene.stage
            && let Some(next) = self.document_digest.take()
        {
            *digest = next;
        }
    }

    fn seek(&mut self, frame: u64) {
        if !self.navigable() {
            return;
        }
        let frame = self
            .frames
            .map_or(frame, |count| frame.min(count.saturating_sub(1)));
        if self.document_id().is_some() {
            self.promote_document_digest();
            self.before_next = None;
            if self.current_frame() != Some(frame) {
                self.viewer.clear_sample();
            }
            self.stage_failure = None;
            self.failed = None;
            self.document_notice.clear();
            // Keep only a picture that still represents the requested output.
            if self
                .shown
                .as_ref()
                .is_some_and(|s| s.picture.header.frame != frame)
            {
                self.shown = None;
                self.dirty.picture = true;
            }
        }
        self.position = Some(frame);
        if self.comparing {
            self.compare_frame = Some(frame);
        }
        self.replay = self.playing;
        self.dirty.view = true;
    }

    fn show(&mut self, shown: Shown) {
        self.before_next = None;
        if shown.picture.header.origin == PictureOrigin::Document {
            self.viewer.accept_picture(&shown.picture.header);
            self.accept_layer_decode(&shown);
        }
        self.shown = Some(shown);
        self.stage_failure = None;
        self.failed = None;
        self.document_progress.clear();
        self.dirty.picture = true;
        self.dirty.view = true;
    }

    /// The shown picture shows the side the user is looking at.
    fn on_side(&self, target: &PictureTarget) -> bool {
        match self.want() {
            Some(Want::Picture(key)) => key.target == *target,
            Some(Want::Play { target: t, .. }) => t == *target,
            None => false,
        }
    }

    /// The frame the user is at: the one asked for, or the one shown.
    fn current_frame(&self) -> Option<u64> {
        self.position
            .or_else(|| self.shown.as_ref().map(|s| s.picture.header.frame))
    }

    fn navigable(&self) -> bool {
        self.broken.is_none()
            && matches!(
                self.scene.stage,
                Some(Subject::Source { .. } | Subject::Result { .. } | Subject::Document { .. })
            )
            && self.current_frame().is_some()
    }

    fn can_play(&self) -> bool {
        self.navigable()
            && self.play_note.is_none()
            && (self.document_id().is_some() || self.timing_known())
    }

    /// The shown picture has a time, so the stream has frame timing.
    fn timing_known(&self) -> bool {
        self.shown
            .as_ref()
            .is_some_and(|s| s.picture.header.time.is_some())
    }

    fn can_compare(&self) -> bool {
        self.broken.is_none()
            && match self.scene.stage {
                Some(Subject::Live { .. } | Subject::Result { .. }) => self
                    .shown
                    .as_ref()
                    .is_some_and(|s| s.picture.header.origin != PictureOrigin::Source),
                _ => false,
            }
    }

    fn can_native(&self) -> bool {
        self.broken.is_none()
            && matches!(
                self.scene.stage,
                Some(Subject::Source { .. } | Subject::Result { .. })
            )
    }

    /// The actual-pixels view centred on the frame.
    fn centre(&self) -> (u32, u32) {
        let (width, height) = self
            .scene
            .stage
            .as_ref()
            .map_or((0, 0), Subject::frame_size);
        (
            width.saturating_sub(u32::from(self.stage_box.width)) / 2,
            height.saturating_sub(u32::from(self.stage_box.height)) / 2,
        )
    }

    fn clamp_origin(&self, (x, y): (u32, u32)) -> (u32, u32) {
        let (width, height) = self
            .scene
            .stage
            .as_ref()
            .map_or((0, 0), Subject::frame_size);
        (
            x.min(width.saturating_sub(u32::from(self.stage_box.width))),
            y.min(height.saturating_sub(u32::from(self.stage_box.height))),
        )
    }

    fn view_for(&self, subject: &Subject) -> PictureView {
        match (subject, self.native) {
            (Subject::Document { .. }, _) => self.viewer.view(self.stage_box),
            (Subject::Source { .. } | Subject::Result { .. }, Some((x, y))) => {
                PictureView::Native { x, y }
            }
            _ => PictureView::Fit,
        }
    }

    fn want(&self) -> Option<Want> {
        let subject = self.scene.stage.as_ref()?;
        let view = self.view_for(subject);
        let (target, frame) = if self.comparing {
            let source = PictureTarget::Source {
                source: subject.source()?.clone(),
            };
            (
                source,
                PictureFrame::Index {
                    index: self.compare_frame?,
                },
            )
        } else {
            let frame = self
                .position
                .map_or(PictureFrame::Poster, |index| PictureFrame::Index { index });
            (subject.target()?, frame)
        };
        let options = if matches!(target, PictureTarget::Document { .. }) {
            self.viewer.options
        } else {
            PictureOptions::default()
        };
        if self.playing {
            let from = self.current_frame().unwrap_or(0);
            return Some(Want::Play {
                target,
                from,
                view,
                options,
            });
        }
        Some(Want::Picture(Key {
            target,
            frame,
            view,
            options,
            size: self.stage_box,
        }))
    }

    /// Sends what is wanted next, if the process is free for it.
    fn pump(&mut self) -> Vec<Effect> {
        let mut out = Vec::new();
        if self.broken.is_some() || !self.sized {
            return out;
        }
        if !self.visible {
            self.stop_playback(&mut out);
            return out;
        }
        match self.want() {
            Some(Want::Play {
                target,
                from,
                view,
                options,
            }) => match &self.inflight {
                Some(Sent::Play {
                    target: t,
                    view: v,
                    options: current_options,
                    size,
                    stopping: false,
                    ..
                }) if *t == target
                    && *v == view
                    && *current_options == options
                    && *size == self.stage_box
                    && !self.replay => {}
                None | Some(Sent::Play { .. }) => {
                    self.replay = false;
                    self.send_play(target, from, view, options, &mut out);
                }
                Some(Sent::Picture { .. }) if matches!(target, PictureTarget::Document { .. }) => {
                    self.replay = false;
                    self.send_play(target, from, view, options, &mut out);
                }
                Some(Sent::Picture { .. }) => {}
            },
            Some(Want::Picture(key)) => {
                let shown = self.shown.as_ref().is_some_and(|s| s.shows(&key));
                if shown
                    && matches!(key.target, PictureTarget::Document { .. })
                    && let Some(Sent::Picture {
                        generation,
                        key: pending,
                        purpose: Purpose::Stage,
                    }) = &self.inflight
                    && *pending != key
                {
                    // Panning can return to the picture on screen while the engine renders a new crop.
                    out.push(Effect {
                        request_id: *generation,
                        command: Command::StopPlayback,
                    });
                    self.inflight = None;
                    self.document_progress.clear();
                    self.dirty.view = true;
                }
                if !shown && self.failed.as_ref() != Some(&key) {
                    if let Some(picture) = self.cache.get(&key) {
                        self.show(Shown {
                            poster: key.frame == PictureFrame::Poster,
                            size: key.size,
                            target: Some(key.target),
                            picture,
                        });
                    } else {
                        match &self.inflight {
                            Some(Sent::Picture {
                                key: k,
                                purpose: Purpose::Stage,
                                ..
                            }) if *k == key => {}
                            None | Some(Sent::Play { .. }) => {
                                self.send_picture(key, Purpose::Stage, &mut out);
                            }
                            Some(Sent::Picture { .. })
                                if matches!(key.target, PictureTarget::Document { .. }) =>
                            {
                                self.send_picture(key, Purpose::Stage, &mut out);
                            }
                            Some(Sent::Picture { .. }) => {}
                        }
                    }
                }
            }
            None => {}
        }
        if !self.playing {
            self.stop_playback(&mut out);
        }
        if self.inflight.is_none() {
            if let Some(key) = self.next_reference() {
                self.send_picture(key, Purpose::Reference, &mut out);
            } else if let Some(key) = self.next_thumb() {
                self.send_picture(key, Purpose::Thumbnail, &mut out);
            }
        }
        out
    }

    fn stop_playback(&mut self, out: &mut Vec<Effect>) {
        if let Some(Sent::Play {
            generation,
            stopping,
            ..
        }) = &mut self.inflight
            && !*stopping
        {
            *stopping = true;
            out.push(Effect {
                request_id: *generation,
                command: Command::StopPlayback,
            });
        }
    }

    fn next_thumb(&self) -> Option<Key> {
        self.scene
            .thumbs
            .iter()
            .map(|(_, source)| self.thumb_key(source))
            .find(|k| !self.cache.contains(k) && !self.thumb_failures.contains(k))
    }

    const fn take_generation(&mut self) -> u64 {
        let generation = self.next_generation;
        self.next_generation += 1;
        generation
    }

    fn send_picture(&mut self, key: Key, purpose: Purpose, out: &mut Vec<Effect>) {
        let generation = self.take_generation();
        if purpose == Purpose::Stage {
            self.stage_generation = generation;
            if matches!(key.target, PictureTarget::Document { .. }) {
                self.start_document_request();
            }
        }
        out.push(Effect {
            request_id: generation,
            command: Command::Picture {
                generation,
                target: key.target.clone(),
                frame: key.frame,
                view: key.view,
                options: key.options,
                size: key.size,
            },
        });
        self.inflight = Some(Sent::Picture {
            generation,
            key,
            purpose,
        });
    }

    fn send_play(
        &mut self,
        target: PictureTarget,
        from: u64,
        view: PictureView,
        options: PictureOptions,
        out: &mut Vec<Effect>,
    ) {
        let generation = self.take_generation();
        self.stage_generation = generation;
        if matches!(target, PictureTarget::Document { .. }) {
            self.start_document_request();
        }
        out.push(Effect {
            request_id: generation,
            command: Command::Play {
                generation,
                target: target.clone(),
                from,
                view,
                options,
                size: self.stage_box,
            },
        });
        self.inflight = Some(Sent::Play {
            generation,
            target,
            view,
            options,
            size: self.stage_box,
            stopping: false,
        });
    }

    pub(crate) fn export_frame_source(
        &self,
        source: crate::export_jobs::ExportSource,
    ) -> Option<crate::export_jobs::ExportFrameSource> {
        use studio_domain::export::{ExportLayer, ExportSampleFormat};

        let shown = self.shown.as_ref()?;
        let header = &shown.picture.header;
        if header.generation != self.stage_generation {
            return None;
        }
        let PictureTarget::Document {
            document,
            path,
            fingerprint,
            digest,
            selection,
        } = shown.target.as_ref()?
        else {
            return None;
        };
        if *document != source.document
            || *path != source.source.path
            || *fingerprint != source.source.fingerprint
            || **digest != source.source.digest
        {
            return None;
        }
        let output = header.output?;
        let info = header.document_info.as_deref()?;
        Some(crate::export_jobs::ExportFrameSource {
            source,
            generation: header.generation,
            ordinal: output.ordinal,
            layer: ExportLayer {
                extended: output.extended_layer,
                embedded: output.embedded_layer,
                temporal: output.temporal_layer,
            },
            expected: ExportSampleFormat {
                bit_depth: info.bit_depth,
                chroma: info.chroma,
            },
            grain: header.options.grain,
            selection: *selection,
        })
    }

    /// The picture area as it should be drawn now.
    #[must_use]
    pub fn view(&self) -> StageView {
        let mut view = StageView {
            state: StageState::None,
            picture: None,
            native: false,
            badge: String::new(),
            hint: String::new(),
            message: String::new(),
            transport: false,
            navigable: false,
            frames: 0,
            position: 0,
            frame_text: String::new(),
            time_text: String::new(),
            can_play: false,
            playing: self.playing,
            play_note: String::new(),
            seek_error: String::new(),
            decode_work: String::new(),
            output: None,
            viewer: ViewerView::default(),
            can_compare: false,
            comparing: self.comparing,
            can_native: false,
            native_on: self.native.is_some(),
            accessible: String::new(),
        };
        let Some(subject) = &self.scene.stage else {
            return view;
        };
        let live = matches!(subject, Subject::Live { .. });
        let document = matches!(subject, Subject::Document { .. });
        match (&self.shown, subject) {
            // The card under the area says the file is being read.
            (_, Subject::Loading { .. }) => view.state = StageState::Loading,
            // The card says why the file cannot be converted; there is no
            // picture to have, so there is no area.
            (_, Subject::Unavailable { .. }) => return view,
            (Some(shown), _) => {
                let header = &shown.picture.header;
                view.state = StageState::Picture;
                view.picture = Some(shown.picture.clone());
                view.native = matches!(header.view, PictureView::Native { .. });
                let waiting = live && header.origin == PictureOrigin::Source && !self.comparing;
                view.badge = text::badge(header, self.frames, waiting);
                view.time_text = header.time.map_or_else(String::new, text::time);
                view.position = self.current_frame().unwrap_or(header.frame);
                view.output = header.output;
                view.frames = self.frames.unwrap_or(0);
                view.frame_text = text::frame_of(view.position, self.frames);
            }
            (None, _) => match (&self.broken, &self.stage_failure) {
                (Some(why), _) if !live => {
                    view.state = StageState::Unavailable;
                    view.message.clone_from(why);
                }
                (_, Some(why)) => {
                    view.state = StageState::Unavailable;
                    view.message.clone_from(why);
                }
                _ => {
                    view.state = StageState::Loading;
                    let message = if live {
                        text::WAITING_LIVE
                    } else {
                        text::LOADING
                    };
                    message.clone_into(&mut view.message);
                }
            },
        }
        if let Some(note) = self.live_note.as_ref().filter(|_| live) {
            view.message.clone_from(note);
        }
        view.navigable = self.navigable() && (document || view.state == StageState::Picture);
        view.transport = document || self.can_native() && view.state != StageState::Unavailable;
        if document {
            self.document_view(&mut view);
            self.viewer_view(&mut view);
        }
        view.can_play = view.navigable && self.can_play();
        view.play_note = if document {
            self.document_play_note()
        } else if !view.navigable || view.can_play || self.playing {
            String::new()
        } else {
            self.play_note
                .clone()
                .unwrap_or_else(|| text::NO_TIMING.to_owned())
        };
        view.can_compare = self.can_compare() || self.comparing;
        let original = self
            .shown
            .as_ref()
            .is_some_and(|s| s.picture.header.origin == PictureOrigin::Source);
        view.hint = match (self.comparing, original) {
            (false, _) if view.can_compare => text::HOLD.to_owned(),
            (true, false) => text::LOADING_ORIGINAL.to_owned(),
            _ => String::new(),
        };
        view.can_native = self.can_native() && view.state == StageState::Picture;
        view.accessible = text::accessible(&view);
        view
    }
}

/// Whether `target` reads a file the scene still shows.
fn referenced(scene: &Scene, target: &PictureTarget) -> bool {
    match target {
        PictureTarget::Source { source } => {
            scene.thumbs.iter().any(|(_, s)| s == source)
                || scene
                    .stage
                    .as_ref()
                    .and_then(Subject::source)
                    .is_some_and(|s| s == source)
        }
        PictureTarget::Document { .. } => false,
        PictureTarget::Result { path, .. } => {
            matches!(&scene.stage, Some(Subject::Result { path: p, .. }) if p == path)
        }
    }
}
