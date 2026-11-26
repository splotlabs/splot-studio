// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Bounded input for a document decoder that advances in display order.

use std::cell::Cell;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use splot_core::ivf::IvfError;
use splot_core::stream_reader::{ReaderError, TemporalUnitReader};
use splot_decode::{DataProps, Decoder, SendStatus};
use studio_domain::preview::PictureFailure;

use super::{
    Context, Damage, DecodePosition, MAX_UNIT_BYTES, Outcome, READ_CHUNK_BYTES, decode_failure,
    empty_stream, framing_damage, ivf_damage, partial_message, unreadable,
};

/// A reader confined to verified bytes, including when retrying one unit.
struct DocumentReader {
    file: File,
    end: u64,
    offset: u64,
    serial: Arc<AtomicU64>,
    request: Cell<u64>,
}

impl Read for DocumentReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if self.serial.load(Ordering::SeqCst) != self.request.get() {
            return Err(io::Error::other("Preview request ended."));
        }
        let remaining = self.end.saturating_sub(self.offset);
        let max = usize::try_from(remaining)
            .unwrap_or(usize::MAX)
            .min(bytes.len())
            .min(READ_CHUNK_BYTES);
        let count = self.file.read(&mut bytes[..max])?;
        if count == 0 && max != 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "The file ended before the verified boundary.",
            ));
        }
        self.offset += count as u64;
        Ok(count)
    }
}

impl Seek for DocumentReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.offset = self.file.seek(position)?;
        Ok(self.offset)
    }
}

/// One forward framing cursor for the active decoder. Picture provenance
/// carries absolute file locations so past units need no retained index.
pub(super) struct DocumentInput {
    reader: TemporalUnitReader<BufReader<DocumentReader>>,
    prefix_end: Option<u64>,
    damage: Option<Damage>,
    accepted: bool,
}

impl DocumentInput {
    pub(super) fn open(file: &File, end: u64, len: u64, context: &Context<'_>) -> Outcome<Self> {
        let mut file = file
            .try_clone()
            .map_err(|e| unreadable(format!("Cannot read the preview file. {e}")))?;
        file.seek(SeekFrom::Start(0))
            .map_err(|e| unreadable(format!("Cannot seek the preview file. {e}")))?;
        let bounded = DocumentReader {
            file,
            end,
            offset: 0,
            serial: Arc::clone(&context.shared.serial),
            request: Cell::new(context.serial),
        };
        Ok(Self {
            reader: TemporalUnitReader::with_max_unit_bytes(
                BufReader::new(bounded),
                MAX_UNIT_BYTES,
            ),
            prefix_end: (end < len).then_some(end),
            damage: None,
            accepted: false,
        })
    }

    pub(super) fn damage(&self) -> Option<&Damage> {
        self.damage.as_ref()
    }

    /// Returns false when the cursor reaches EOF or finds a framing error.
    pub(super) fn feed(&mut self, decoder: &mut Decoder, context: &Context<'_>) -> Outcome<bool> {
        self.reader.get_ref().get_ref().request.set(context.serial);
        let damage = match self.reader.next_temporal_unit() {
            Ok(Some(unit)) => {
                let offset = unit.offset.get();
                let len = unit.bytes.len() as u64;
                if unit.pts.is_none()
                    && self
                        .prefix_end
                        .is_some_and(|end| offset.checked_add(len) == Some(end))
                {
                    // The last raw unit has no verified next delimiter.
                    None
                } else {
                    // DataProps uses signed ticks. Preserve all IVF timestamp bits.
                    let pts = i64::from_ne_bytes(unit.pts.unwrap_or(0).to_ne_bytes());
                    let duration = i64::try_from(len)
                        .map_err(|_| unreadable("The temporal unit is too large."))?;
                    let props = DataProps::default()
                        .with_user_data(offset)
                        .with_pts(pts)
                        .with_duration(duration);
                    context.check()?;
                    let position = unit.position();
                    let status = decoder.send_data(unit.bytes, props).map_err(|e| {
                        decode_failure(true, unit.index as u64, DecodePosition::Unit, &e)
                    })?;
                    if status == SendStatus::Full {
                        self.reader.seek(position).map_err(|e| {
                            unreadable(format!("Cannot seek the preview file. {e}"))
                        })?;
                    } else {
                        self.accepted = true;
                    }
                    return Ok(true);
                }
            }
            Ok(None) if self.prefix_end.is_some() => None,
            Ok(None) => ivf_damage(&self.reader),
            Err(ReaderError::Ivf(IvfError::TruncatedFramePayload { .. }))
                if self.prefix_end.is_some() =>
            {
                None
            }
            Err(e) => {
                context.check()?;
                Some(framing_damage(&e))
            }
        };
        self.damage = damage.or_else(|| match self.prefix_end {
            Some(end) => Some(Damage {
                code: PictureFailure::PartialCoverage,
                message: partial_message(end),
            }),
            None => (!self.accepted).then(empty_stream),
        });
        Ok(false)
    }
}
