// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Result pictures: a finished IVF or Annex B `.obu` file, decoded by the
//! Splot streaming decoder.
//!
//! Opening a file reads it once through Splot's demuxer, without decoding,
//! into an index of its temporal units: where each one is, its IVF
//! timestamp, and whether a decoder can start there (see
//! [`crate::stream`]). A frame is then decoded from the nearest random
//! access point at or before it. One decoder session is kept, so frame
//! `n + 1` after frame `n` costs one frame; a jump back, or past the next
//! random access point, starts a new session there. With the encoder's
//! default key-frame spacing of 120 frames, any frame costs at most 120
//! frames of decoding; nothing replays the file from its start.
//!
//! The index ends at the first framing error: frames before it can be
//! shown, frames after it are reported as unreadable, and the frame count
//! is then unknown.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::PathBuf;

use splot_core::annexb::AnnexBObuCursor;
use splot_core::span::ByteOffset;
use splot_core::stream_reader::{TemporalUnitReader, is_random_access_point};
use splot_core::types::ObuType;
use splot_decode::{DataProps, Decoder, SendStatus};
use studio_domain::job::{SourceFingerprint, WirePath};
use studio_domain::preview::{ColorMatrix, MediaTime, PictureFailure, SourceRef};

use super::{Context, Decoded, Outcome, Stopped};
use crate::source::{self, Selection};
use crate::stream::{self, PREVIEW_DECODE_THREADS};

/// The largest temporal unit read.
pub const MAX_UNIT_BYTES: usize = 64 << 20;

/// The most temporal units indexed: over nine hours at 30 frames a second,
/// about 24 MiB of index.
pub const MAX_UNITS: usize = 1 << 20;

/// One temporal unit in the file.
#[derive(Debug, Clone, Copy)]
struct Unit {
    offset: u64,
    len: usize,
    /// The IVF timestamp; unused for Annex B.
    pts: u64,
    random_access: bool,
}

/// A decoder positioned in the file.
struct Session {
    decoder: Decoder,
    /// The next unit to send.
    next_send: usize,
    /// The display index of the next picture.
    next_out: u64,
    drained: bool,
}

/// An open output file.
pub struct ResultPictures {
    path: PathBuf,
    fingerprint: SourceFingerprint,
    file: File,
    units: Vec<Unit>,
    /// Why the index stopped early.
    damaged: Option<String>,
    /// IVF seconds per timestamp tick as `(num, den)`.
    ivf_time_base: Option<(u32, u32)>,
    /// Frames per second from the source, for `.obu`.
    source_rate: Option<(u32, u32)>,
    matrix: Option<ColorMatrix>,
    session: Option<Session>,
    buffer: Vec<u8>,
}

fn unreadable(message: impl Into<String>) -> Stopped {
    Stopped::unavailable(PictureFailure::Unreadable, message)
}

impl ResultPictures {
    /// Opens and indexes `path`. The source, when given and still the file
    /// it names, supplies the colour basis and, for `.obu`, the timing;
    /// otherwise neither is known.
    pub fn open(path: &WirePath, reference: Option<&SourceRef>) -> Outcome<Self> {
        let path = path.to_path_buf().map_err(|e| unreadable(e.to_string()))?;
        let file =
            File::open(&path).map_err(|e| unreadable(format!("cannot open the output: {e}")))?;
        let metadata = file
            .metadata()
            .map_err(|e| unreadable(format!("cannot inspect the output: {e}")))?;
        if !metadata.is_file() {
            return Err(unreadable("the output is not a regular file"));
        }
        let fingerprint = SourceFingerprint::of(&metadata);
        let reader = file
            .try_clone()
            .map_err(|e| unreadable(format!("cannot read the output: {e}")))?;
        let (units, damaged, ivf_time_base) = index(reader);
        let (matrix, source_rate) = reference.map_or((None, None), facts_of);
        Ok(Self {
            path,
            fingerprint,
            file,
            units,
            damaged,
            ivf_time_base,
            source_rate,
            matrix,
            session: None,
            buffer: Vec::new(),
        })
    }

    /// Releases the decoder; the index stays.
    pub fn rest(&mut self) {
        self.session = None;
    }

    pub fn check(&self) -> Outcome<()> {
        let now = std::fs::metadata(&self.path)
            .map(|m| SourceFingerprint::of(&m))
            .map_err(|e| unreadable(format!("the output is gone: {e}")))?;
        if now == self.fingerprint {
            Ok(())
        } else {
            Err(unreadable("the output file changed; open it again"))
        }
    }

    pub fn frame_count(&self) -> Option<u64> {
        self.damaged.is_none().then_some(self.units.len() as u64)
    }

    pub const fn matrix(&self) -> Option<ColorMatrix> {
        self.matrix
    }

    pub fn time(&self, index: u64) -> Option<MediaTime> {
        if let Some((num, den)) = self.ivf_time_base {
            let unit = usize::try_from(index).ok().and_then(|i| self.units.get(i));
            // Past the last unit, timing continues one tick per frame.
            let pts = unit.map_or_else(
                || {
                    let last = self.units.last().map_or(0, |u| u.pts);
                    last + (index + 1).saturating_sub(self.units.len() as u64)
                },
                |unit| unit.pts,
            );
            return (den > 0).then(|| MediaTime {
                num: pts.saturating_mul(u64::from(num)),
                den: u64::from(den),
            });
        }
        let (num, den) = self.source_rate?;
        (num > 0).then(|| MediaTime {
            num: index.saturating_mul(u64::from(den)),
            den: u64::from(num),
        })
    }

    pub fn second(&self) -> u64 {
        let rate = self
            .ivf_time_base
            .map(|(num, den)| (den, num))
            .or(self.source_rate)
            .unwrap_or((30, 1));
        u64::from(rate.0 + rate.1 / 2) / u64::from(rate.1.max(1))
    }

    pub fn seek(&mut self, context: &Context<'_>, index: u64) -> Outcome<()> {
        let at = usize::try_from(index).unwrap_or(usize::MAX);
        if at >= self.units.len() {
            return Err(match &self.damaged {
                Some(damage) => unreadable(format!(
                    "frame {index} is past the readable part of the file: {damage}"
                )),
                None => super::out_of_range(index, self.frame_count()),
            });
        }
        let start = self.units[..=at]
            .iter()
            .rposition(|unit| unit.random_access)
            .ok_or_else(|| {
                unreadable(format!("there is no key frame at or before frame {index}"))
            })?;
        let reusable = self.session.as_ref().is_some_and(|session| {
            !session.drained && session.next_out <= index && session.next_out >= start as u64
        });
        if !reusable {
            let decoder = match self.session.take() {
                Some(mut session) => {
                    session.decoder.flush();
                    session.decoder
                }
                None => stream::decoder(PREVIEW_DECODE_THREADS)
                    .map_err(|e| unreadable(format!("cannot start the decoder: {e}")))?,
            };
            self.session = Some(Session {
                decoder,
                next_send: start,
                next_out: start as u64,
                drained: false,
            });
        }
        while self.session.as_ref().is_some_and(|s| s.next_out < index) {
            context.check()?;
            if self.picture()?.is_none() {
                break;
            }
        }
        Ok(())
    }

    pub fn next(&mut self, context: &Context<'_>) -> Outcome<Option<(u64, Decoded)>> {
        context.check()?;
        Ok(self
            .picture()?
            .map(|(index, picture)| (index, Decoded::Picture(Box::new(picture)))))
    }

    /// The session's next picture in display order; `None` at the end.
    fn picture(&mut self) -> Outcome<Option<(u64, splot_decode::Picture)>> {
        let decoded = self.decode_next();
        if decoded.is_err() {
            self.session = None;
        }
        decoded
    }

    fn decode_next(&mut self) -> Outcome<Option<(u64, splot_decode::Picture)>> {
        let Some(session) = self.session.as_mut() else {
            return Ok(None);
        };
        let failed = |at: u64, e: splot_decode::DecodeError| {
            unreadable(format!("the output does not decode at frame {at}: {e}"))
        };
        loop {
            if let Some(picture) = session
                .decoder
                .get_picture()
                .map_err(|e| failed(session.next_out, e))?
            {
                let index = picture.props().user_data;
                if index != session.next_out {
                    return Err(unreadable(format!(
                        "the decoder returned frame {index} where frame {} was due; the \
                         stream does not show one frame per temporal unit",
                        session.next_out
                    )));
                }
                session.next_out += 1;
                return Ok(Some((index, picture)));
            }
            let Some(unit) = self.units.get(session.next_send).copied() else {
                if session.drained {
                    return Ok(None);
                }
                session.decoder.drain();
                session.drained = true;
                continue;
            };
            self.buffer.resize(unit.len, 0);
            self.file
                .seek(SeekFrom::Start(unit.offset))
                .and_then(|_| self.file.read_exact(&mut self.buffer))
                .map_err(|e| unreadable(format!("cannot read the output: {e}")))?;
            let props = DataProps::default().with_user_data(session.next_send as u64);
            match session
                .decoder
                .send_data(&self.buffer, props)
                .map_err(|e| failed(session.next_send as u64, e))?
            {
                SendStatus::Accepted => session.next_send += 1,
                // `get_picture` now waits for a picture.
                SendStatus::Full => {}
            }
        }
    }
}

/// The matrix and frame rate of the result's source, when it is still the
/// file it names.
fn facts_of(reference: &SourceRef) -> (Option<ColorMatrix>, Option<(u32, u32)>) {
    let Ok(path) = reference.path.to_path_buf() else {
        return (None, None);
    };
    let opened = source::open(
        &path,
        Selection {
            fingerprint: Some(reference.fingerprint),
            video_stream: Some(reference.video_stream),
        },
    );
    opened.map_or((None, None), |source| {
        (source.matrix, Some(source.header.frame_rate))
    })
}

type Index = (Vec<Unit>, Option<String>, Option<(u32, u32)>);

/// Why an Annex B temporal unit cannot be shown: it must open with a temporal
/// delimiter and every OBU in it must parse.
fn annex_b_fault(unit: &[u8]) -> Result<(), String> {
    let mut cursor = AnnexBObuCursor::new(unit, ByteOffset::new(0));
    let mut first = true;
    loop {
        match cursor.next_obu() {
            Ok(Some(obu)) if first && obu.header.obu_type != ObuType::TemporalDelimiter => {
                return Err("does not start with a temporal delimiter".into());
            }
            Ok(Some(_)) => first = false,
            Ok(None) => return Ok(()),
            Err(e) => return Err(format!("does not parse: {e}")),
        }
    }
}

/// Reads the file's framing: the temporal units, why reading stopped early
/// (if it did), and the IVF time base.
fn index(file: File) -> Index {
    let mut reader = TemporalUnitReader::with_max_unit_bytes(BufReader::new(file), MAX_UNIT_BYTES);
    let mut units: Vec<Unit> = Vec::new();
    let mut damaged = None;
    loop {
        if units.len() >= MAX_UNITS {
            damaged = Some(format!("only the first {MAX_UNITS} frames are read"));
            break;
        }
        let unit = match reader.next_temporal_unit() {
            Ok(Some(unit)) => unit,
            Ok(None) => break,
            Err(e) => {
                damaged = Some(format!("the file does not parse: {e}"));
                break;
            }
        };
        if unit.pts.is_none()
            && let Err(fault) = annex_b_fault(unit.bytes)
        {
            damaged = Some(format!("the unit at byte {} {fault}", unit.offset.get()));
            break;
        }
        units.push(Unit {
            offset: unit.offset.get(),
            len: unit.bytes.len(),
            pts: unit.pts.unwrap_or(0),
            random_access: is_random_access_point(unit.bytes),
        });
    }
    if let Some(warning) = reader.ivf_warning() {
        damaged = Some(format!("the IVF file is damaged ({})", warning.rule_id()));
    }
    let time_base = reader
        .ivf_header()
        .map(|header| (header.timebase_numerator, header.timebase_denominator));
    if units.is_empty() && damaged.is_none() {
        damaged = Some("the file holds no frames".into());
    }
    (units, damaged, time_base)
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::num::NonZeroU32;

    use splot_encode::{Config, Encoder, FrameType, Preset, SourceFrame};
    use studio_domain::settings::OutputKind;

    use super::*;
    use crate::output::PacketSink;

    /// Nine 16x16 frames with key frames every four, in both formats; the
    /// key frames the encoder reports.
    fn encoded(kind: OutputKind) -> (Vec<u8>, Vec<u64>) {
        let mut config = Config::new(16, 16);
        config.keyint = NonZeroU32::new(4).unwrap_or(NonZeroU32::MIN);
        config.preset = Preset::new(10).unwrap_or_default();
        let mut sink =
            PacketSink::new(kind, Vec::new(), &config).unwrap_or_else(|_| unreachable!());
        let mut keys = Vec::new();
        let mut encoder = Encoder::new(config.clone()).unwrap_or_else(|_| unreachable!());
        for index in 0..10_u16 {
            if index < 9 {
                let planes = [vec![index * 20; 256], vec![128; 64], vec![128; 64]];
                let frame = SourceFrame::new(&config, u64::from(index), planes);
                assert!(frame.and_then(|f| encoder.send_frame(f)).is_ok());
            } else {
                assert!(encoder.flush().is_ok());
            }
            while let Ok(Some(packet)) = encoder.receive_packet() {
                if packet.frame_type == FrameType::Key {
                    keys.push(packet.pts);
                }
                assert!(sink.write(&packet).is_ok());
            }
        }
        (sink.finish().unwrap_or_default(), keys)
    }

    #[test]
    fn the_index_finds_every_key_frame() -> std::io::Result<()> {
        for kind in [OutputKind::Ivf, OutputKind::Av2AnnexB] {
            let (bytes, keys) = encoded(kind);
            assert!(keys.len() >= 3, "{keys:?}");
            let mut file = tempfile::tempfile()?;
            file.write_all(&bytes)?;
            file.seek(SeekFrom::Start(0))?;
            let (units, damaged, time_base) = index(file);
            assert_eq!(damaged, None);
            assert_eq!(units.len(), 9);
            let found: Vec<u64> = (0_u64..)
                .zip(&units)
                .filter_map(|(i, unit)| unit.random_access.then_some(i))
                .collect();
            assert_eq!(found, keys, "{kind:?}");
            assert_eq!(time_base.is_some(), kind == OutputKind::Ivf);
            // The units tile the file after the IVF headers.
            let total: usize = units.iter().map(|u| u.len).sum();
            let headers = if kind == OutputKind::Ivf {
                32 + 12 * 9
            } else {
                0
            };
            assert_eq!(total + headers, bytes.len());
        }
        Ok(())
    }
}
