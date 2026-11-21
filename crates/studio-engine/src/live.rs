// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Live pictures of a running job: the packets it has just written,
//! decoded by the Splot decoder on a thread of their own.
//!
//! The job never waits for this. It hands each written packet to a
//! [`Feed`], which copies it into a queue of [`QUEUE_UNITS`] units with a
//! non-blocking send. When the decoder falls that far behind, the unit is
//! dropped and so is every later one up to the next random access point
//! (see [`crate::stream`]), where the decoder starts afresh: a decoder
//! cannot skip a unit, and the Splot decoder has no way to resynchronise
//! other than a new session at a key frame. The packets are only read, so
//! the output is the same with or without live pictures.
//!
//! The decoder runs on [`DECODE_THREADS`] worker. Of its pictures, the
//! newest is converted and offered at most once per [`INTERVAL`]; the
//! offer replaces any picture not yet written, so a slow reader only ever
//! gets fewer pictures.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use splot_decode::{DataProps, Decoder, Picture, SendStatus};
use studio_domain::preview::{
    ColorBasis, MediaTime, PictureBox, PictureHeader, PictureOrigin, PictureView,
};

use crate::{picture, stream};

/// The shortest gap between two live pictures: about four a second.
pub const INTERVAL: Duration = Duration::from_millis(250);

/// Temporal units waiting for the live decoder before units are dropped.
pub const QUEUE_UNITS: usize = 32;

/// Decoder threads for live pictures, beside the encoder's own.
pub const DECODE_THREADS: usize = 1;

/// What live pictures need to know of the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stream {
    /// How its pictures are converted: the source's basis.
    pub basis: ColorBasis,
    /// Frames per second as `(num, den)`.
    pub frame_rate: (u32, u32),
    /// The frames the job will encode, when known.
    pub frame_count: Option<u64>,
}

/// One message to the live decoder.
#[derive(Debug)]
pub enum Message {
    /// The stream starts; sent before any unit.
    Stream(Stream),
    /// One temporal unit, as written to the output.
    Unit {
        /// The unit's bytes.
        data: Vec<u8>,
        /// Its index, which is its frame's display index.
        index: u64,
        /// Whether a decoder can start here.
        random_access: bool,
        /// Units before this one were dropped: start a new decoder.
        restart: bool,
    },
}

/// The job's end of the queue. Never blocks.
#[derive(Debug)]
pub struct Feed {
    tx: SyncSender<Message>,
    next: u64,
    lost: bool,
}

impl Feed {
    /// A feed and the receiver for [`run`].
    #[must_use]
    pub fn new() -> (Self, Receiver<Message>) {
        let (tx, rx) = mpsc::sync_channel(QUEUE_UNITS);
        (
            Self {
                tx,
                next: 0,
                lost: false,
            },
            rx,
        )
    }

    /// The stream is starting.
    pub fn stream(&mut self, stream: Stream) {
        if self.tx.try_send(Message::Stream(stream)).is_err() {
            self.lost = true;
        }
    }

    /// A packet's temporal unit was written to the output.
    pub fn packet(&mut self, data: &[u8]) {
        let index = self.next;
        self.next += 1;
        let random_access = stream::is_random_access(data);
        if self.lost && !random_access {
            return;
        }
        let unit = Message::Unit {
            data: data.to_vec(),
            index,
            random_access,
            restart: self.lost,
        };
        self.lost = match self.tx.try_send(unit) {
            Ok(()) => false,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => true,
        };
    }
}

/// Decodes the units from `rx` until the feed is dropped, and hands the
/// newest picture, at most once per [`INTERVAL`] and no larger than
/// `size`, to `offer`.
pub fn run(rx: &Receiver<Message>, size: PictureBox, offer: &dyn Fn(PictureHeader, Vec<u8>)) {
    let mut live = Live {
        stream: None,
        decoder: None,
        held: None,
        next_offer: Instant::now(),
        size,
    };
    loop {
        let message = if live.held.is_some() {
            let wait = live.next_offer.saturating_duration_since(Instant::now());
            match rx.recv_timeout(wait) {
                Ok(message) => message,
                Err(RecvTimeoutError::Timeout) => {
                    live.offer(offer);
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match rx.recv() {
                Ok(message) => message,
                Err(_) => return,
            }
        };
        match message {
            Message::Stream(stream) => live.stream = Some(stream),
            Message::Unit {
                data,
                index,
                random_access,
                restart,
            } => {
                live.unit(&data, index, random_access, restart);
                if Instant::now() >= live.next_offer {
                    live.offer(offer);
                }
            }
        }
    }
}

struct Live {
    stream: Option<Stream>,
    decoder: Option<Decoder>,
    /// The newest picture not yet offered.
    held: Option<Picture>,
    next_offer: Instant,
    size: PictureBox,
}

impl Live {
    fn unit(&mut self, data: &[u8], index: u64, random_access: bool, restart: bool) {
        if restart || self.decoder.is_none() {
            // A decoder can only start at a random access point.
            self.decoder = None;
            if !random_access {
                return;
            }
            self.decoder = stream::decoder(DECODE_THREADS).ok();
        }
        if self.feed(data, index).is_err() {
            // A failed session stays failed; wait for the next key frame.
            self.decoder = None;
        }
    }

    fn feed(&mut self, data: &[u8], index: u64) -> splot_decode::Result<()> {
        let Some(decoder) = self.decoder.as_mut() else {
            return Ok(());
        };
        let props = DataProps::default().with_user_data(index);
        while decoder.send_data(data, props)? == SendStatus::Full {
            if let Some(picture) = decoder.get_picture()? {
                self.held = Some(picture);
            }
        }
        while let Some(picture) = decoder.get_picture()? {
            self.held = Some(picture);
        }
        Ok(())
    }

    fn offer(&mut self, offer: &dyn Fn(PictureHeader, Vec<u8>)) {
        let (Some(held), Some(stream)) = (self.held.take(), self.stream) else {
            return;
        };
        let Some(frame) = stream::frame(&held) else {
            return;
        };
        let rendered = picture::render(&frame, stream.basis, PictureView::Fit, self.size);
        let index = held.props().user_data;
        let (num, den) = stream.frame_rate;
        let header = PictureHeader {
            generation: 0,
            origin: PictureOrigin::Live,
            frame: index,
            time: (num > 0).then(|| MediaTime {
                num: index.saturating_mul(u64::from(den)),
                den: u64::from(num),
            }),
            frame_count: stream.frame_count,
            width: rendered.width,
            height: rendered.height,
            frame_width: u32::try_from(held.width()).unwrap_or(u32::MAX),
            frame_height: u32::try_from(held.height()).unwrap_or(u32::MAX),
            view: rendered.view,
            basis: stream.basis,
        };
        drop(held);
        offer(header, rendered.pixels);
        self.next_offer = Instant::now() + INTERVAL;
    }
}
