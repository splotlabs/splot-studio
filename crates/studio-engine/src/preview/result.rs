// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Studio encoder output uses a bounded random-access index and emits one
//! picture per temporal unit. Documents stream framed units in order and
//! replay from the start when no session can continue to the requested ordinal.

mod replay;

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::rc::Rc;

use splot_core::annexb::AnnexBObuCursor;
use splot_core::ivf::{IvfError, is_ivf, parse_ivf_header};
use splot_core::span::ByteOffset;
use splot_core::stream_reader::{ReaderError, TemporalUnitReader, is_random_access_point};
use splot_core::types::ObuType;
use splot_decode::{
    DataProps, DecodeError, DecodeLimits, Decoder, ObuLocation, Picture, SendStatus,
};
use studio_domain::analysis::SourceDigest;
use studio_domain::job::{SourceFingerprint, WirePath};
use studio_domain::preview::{
    ColorMatrix, GrainPolicy, MediaTime, OutputEvent, PictureFailure, PictureStage, SourceRef,
};

use self::replay::DocumentInput;
use super::document::DocumentPicture;
use super::{Context, Decoded, Outcome, Stopped};
use crate::inspect::document::HashReader;
use crate::output::FileId;
use crate::source::{self, Selection};
use crate::stream::{
    self, MAX_DOCUMENT_UNITS as MAX_UNITS, MAX_UNIT_BYTES, PREVIEW_DECODE_THREADS,
};

/// Maximum read between cancellation checks while feeding a temporal unit.
const READ_CHUNK_BYTES: usize = 1 << 20;

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
    grain: GrainPolicy,
    /// The next unit to send.
    next_send: usize,
    /// The display index of the next picture.
    next_out: u64,
    drained: bool,
    document: Option<DocumentInput>,
}

/// An open output file.
pub struct ResultPictures {
    path: PathBuf,
    fingerprint: SourceFingerprint,
    /// Present for a document, whose identity must match analysis.
    document_id: Option<FileId>,
    verified_bytes: u64,
    file: File,
    units: Vec<Unit>,
    /// Why the index stopped early.
    damaged: Option<Damage>,
    count: Option<u64>,
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

fn changed() -> Stopped {
    Stopped::unavailable(
        PictureFailure::SourceChanged,
        "The document changed. Open it again in Analyze.",
    )
}

impl ResultPictures {
    /// Opens and indexes `path`. The source, when given and still the file
    /// it names, supplies the colour basis and, for `.obu`, the timing;
    /// otherwise neither is known.
    pub fn open(
        path: &WirePath,
        reference: Option<&SourceRef>,
        document: Option<(SourceFingerprint, &SourceDigest)>,
        context: &Context<'_>,
    ) -> Outcome<Self> {
        let path = path.to_path_buf().map_err(|e| unreadable(e.to_string()))?;
        let inspect_error = |error| unreadable(format!("Cannot inspect the preview file. {error}"));
        let open_error = |error| {
            if document.is_some() {
                changed()
            } else {
                unreadable(format!("Cannot open the output. {error}"))
            }
        };
        let named = std::fs::metadata(&path).map_err(open_error)?;
        if !named.is_file() {
            return Err(unreadable("The preview path must name a regular file."));
        }
        let file = File::open(&path).map_err(open_error)?;
        let metadata = file.metadata().map_err(inspect_error)?;
        if !metadata.is_file() {
            return Err(unreadable("The preview file must be a regular file."));
        }
        let fingerprint = SourceFingerprint::of(&metadata);
        let reader = file
            .try_clone()
            .map_err(|e| unreadable(format!("Cannot read the preview file. {e}")))?;
        let (indexed, document_id, verified_bytes) = if let Some((expected, digest)) = document {
            if fingerprint != expected || digest.bytes > fingerprint.len {
                return Err(changed());
            }
            let id = FileId::of_file(&file, &path).map_err(inspect_error)?;
            source::still_the_same(&path, &id, fingerprint).map_err(Stopped::from_stop)?;
            let reader = ScanReader {
                context,
                inner: reader.take(digest.bytes),
                completed: 0,
                total: digest.bytes,
            };
            // Any legal IVF file header fits within this buffer. Hash these
            // bytes with the rest of the verified scope before exposing them.
            let mut reader = HashReader::new(reader);
            let verify_error = |e| {
                context
                    .check()
                    .err()
                    .unwrap_or_else(|| unreadable(format!("Cannot verify the document bytes. {e}")))
            };
            let mut header = Vec::new();
            reader
                .by_ref()
                .take(u64::from(u16::MAX))
                .read_to_end(&mut header)
                .map_err(verify_error)?;
            let header = is_ivf(&header).then(|| parse_ivf_header(&header));
            io::copy(&mut reader, &mut io::sink()).map_err(verify_error)?;
            if &reader.finish() != digest {
                return Err(changed());
            }
            source::still_the_same(&path, &id, fingerprint).map_err(Stopped::from_stop)?;
            let header = match header.transpose() {
                Ok(header) => header,
                Err(IvfError::TruncatedHeader { .. }) if digest.bytes < fingerprint.len => {
                    return Err(partial_coverage(digest.bytes));
                }
                Err(e) => return Err(unreadable(format!("Cannot parse the preview file. {e}"))),
            };
            if header.is_some_and(|header| header.fourcc != *b"AV02") {
                return Err(Stopped::unavailable(
                    PictureFailure::Unsupported,
                    "Preview supports only AV02 IVF files.",
                ));
            }
            let time_base = header
                .map(|header| (header.timebase_numerator, header.timebase_denominator))
                .filter(|(num, den)| *num != 0 && *den != 0);
            ((Vec::new(), None, time_base), Some(id), digest.bytes)
        } else {
            (index(reader)?, None, fingerprint.len)
        };
        let (units, damaged, ivf_time_base) = indexed;
        let (matrix, source_rate) = reference.map_or((None, None), facts_of);
        Ok(Self {
            path,
            fingerprint,
            document_id,
            verified_bytes,
            file,
            units,
            damaged,
            count: None,
            ivf_time_base,
            source_rate,
            matrix,
            session: None,
            buffer: Vec::new(),
        })
    }

    /// Releases the decoder and its input buffer. The index stays.
    pub fn rest(&mut self) {
        self.session = None;
        self.buffer = Vec::new();
    }

    pub fn choose_grain(&mut self, grain: GrainPolicy) {
        if self
            .session
            .as_ref()
            .is_some_and(|session| session.grain != grain)
        {
            self.rest();
        }
    }

    pub fn check(&self) -> Outcome<()> {
        if let Some(id) = &self.document_id {
            return source::still_the_same(&self.path, id, self.fingerprint)
                .map_err(Stopped::from_stop);
        }
        let now = std::fs::metadata(&self.path)
            .map(|m| SourceFingerprint::of(&m))
            .map_err(|e| unreadable(format!("the output is gone: {e}")))?;
        if now == self.fingerprint {
            Ok(())
        } else {
            Err(unreadable("the output file changed; open it again"))
        }
    }

    pub const fn is_document(&self) -> bool {
        self.document_id.is_some()
    }

    pub fn frame_count(&self) -> Option<u64> {
        if self.is_document() {
            self.count
        } else {
            self.damaged.is_none().then_some(self.units.len() as u64)
        }
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
                    last.saturating_add(
                        index
                            .saturating_add(1)
                            .saturating_sub(self.units.len() as u64),
                    )
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
        (u64::from(rate.0) + u64::from(rate.1) / 2) / u64::from(rate.1.max(1))
    }

    pub fn seek(&mut self, context: &Context<'_>, index: u64, grain: GrainPolicy) -> Outcome<()> {
        let document = self.is_document();
        let start = if document {
            if self.count.is_some_and(|count| index >= count) {
                return Err(super::out_of_range(index, self.count));
            }
            // An arbitrary access point need not restore the sequence, layer,
            // or external-header state. Start at byte zero for a fresh session.
            0
        } else {
            self.encoded_seek_start(index)?
        };
        let reusable = self.session.as_ref().is_some_and(|session| {
            session.grain == grain
                && (document || !session.drained)
                && session.next_out <= index
                && session.next_out >= start as u64
        });
        if !reusable {
            let input = if document {
                Some(DocumentInput::open(
                    &self.file,
                    self.verified_bytes,
                    self.fingerprint.len,
                    context,
                )?)
            } else {
                None
            };
            let reuse = self.session.take().filter(|session| session.grain == grain);
            let decoder = if let Some(mut session) = reuse {
                session.decoder.flush();
                session.decoder
            } else {
                let decoder = if document {
                    stream::preview_decoder(PREVIEW_DECODE_THREADS, DecodeLimits::DEFAULT, grain)
                } else {
                    stream::decoder(PREVIEW_DECODE_THREADS)
                };
                decoder.map_err(|e| decode_failure(document, 0, DecodePosition::Output, &e))?
            };
            self.session = Some(Session {
                decoder,
                grain,
                next_send: start,
                next_out: start as u64,
                drained: false,
                document: input,
            });
        }
        while let Some(at) = self
            .session
            .as_ref()
            .map(|s| s.next_out)
            .filter(|at| *at < index)
        {
            if document {
                context.progress(PictureStage::Replay, at, Some(index));
            }
            if self.picture(context)?.is_none() {
                return Err(super::out_of_range(index, self.frame_count()));
            }
        }
        Ok(())
    }

    fn encoded_seek_start(&self, index: u64) -> Outcome<usize> {
        let at = usize::try_from(index).unwrap_or(usize::MAX);
        if at >= self.units.len() {
            return Err(match &self.damaged {
                Some(damage) => unreadable(format!(
                    "frame {index} is past the readable part of the file: {}",
                    damage.message
                )),
                None => super::out_of_range(index, self.frame_count()),
            });
        }
        self.units[..=at]
            .iter()
            .rposition(|unit| unit.random_access)
            .ok_or_else(|| unreadable(format!("there is no key frame at or before frame {index}")))
    }

    pub fn next(&mut self, context: &Context<'_>) -> Outcome<Option<(u64, Decoded)>> {
        let Some((index, picture)) = self.picture(context)? else {
            return Ok(None);
        };
        let decoded = if self.is_document() {
            Decoded::Document(Rc::new(self.document_picture(index, picture)?))
        } else {
            Decoded::Picture(Box::new(picture))
        };
        Ok(Some((index, decoded)))
    }

    fn file_offset(&self, location: ObuLocation) -> Outcome<u64> {
        location
            .user_data
            .checked_add(location.offset.get())
            .filter(|absolute| *absolute < self.verified_bytes)
            .ok_or_else(|| unreadable("The decoder returned an OBU outside the verified bytes."))
    }

    fn document_picture(&self, index: u64, picture: Picture) -> Outcome<DocumentPicture> {
        let provenance = picture.provenance();
        let props = picture.props();
        let unit_len = props.duration.unsigned_abs();
        if props.user_data != provenance.display.user_data
            || provenance.display.offset.get() >= unit_len
            || props
                .user_data
                .checked_add(unit_len)
                .is_none_or(|end| end > self.verified_bytes)
        {
            return Err(unreadable(
                "The decoder returned an invalid display location.",
            ));
        }
        let time = self.ivf_time_base.and_then(|(num, den)| {
            let pts = u64::from_ne_bytes(props.pts.to_ne_bytes());
            Some(MediaTime {
                num: pts.checked_mul(u64::from(num))?,
                den: u64::from(den),
            })
        });
        let interval = picture
            .content_interpretation()
            .and_then(|content| content.timing_info)
            .filter(|timing| timing.equal_picture_interval && timing.time_scale != 0)
            .and_then(|timing| {
                let ticks = u64::from(timing.num_ticks_per_picture_minus_1?) + 1;
                let num = u64::from(timing.num_units_in_display_tick).checked_mul(ticks)?;
                (num != 0).then_some(MediaTime {
                    num,
                    den: u64::from(timing.time_scale),
                })
            });
        let output = OutputEvent {
            ordinal: index,
            unit_offset: provenance.display.user_data,
            unit_len,
            coded_offset: Some(self.file_offset(provenance.coded)?),
            showing_offset: Some(self.file_offset(provenance.display)?),
            sequence_offset: Some(self.file_offset(provenance.sequence_header)?),
            extended_layer: picture.extended_layer_id().get(),
            embedded_layer: picture.embedded_layer_id().get(),
            temporal_layer: picture.temporal_layer_id().get(),
            interval,
        };
        Ok(DocumentPicture {
            picture,
            output,
            time,
        })
    }

    /// The session's next picture in display order; `None` at the end.
    fn picture(&mut self, context: &Context<'_>) -> Outcome<Option<(u64, Picture)>> {
        let decoded = self.decode_next(context);
        if matches!(decoded, Err(Stopped::Unavailable(..))) {
            self.session = None;
        }
        decoded
    }

    fn decode_next(&mut self, context: &Context<'_>) -> Outcome<Option<(u64, Picture)>> {
        let document = self.is_document();
        let Some(session) = self.session.as_mut() else {
            return Ok(None);
        };
        loop {
            context.check()?;
            let next = if session
                .document
                .as_ref()
                .is_some_and(|input| input.damage().is_some())
            {
                // An incomplete framing scan does not establish end of stream.
                session.decoder.wait_picture()
            } else {
                session.decoder.get_picture()
            };
            if let Some(picture) = next.map_err(|e| {
                decode_failure(document, session.next_out, DecodePosition::Output, &e)
            })? {
                let index = session.next_out;
                if !document && picture.props().user_data != index {
                    return Err(unreadable(format!(
                        "the decoder returned frame {} where frame {index} was due; the \
                         stream does not show one frame per temporal unit",
                        picture.props().user_data,
                    )));
                }
                session.next_out += 1;
                return Ok(Some((index, picture)));
            }
            if let Some(input) = session.document.as_mut() {
                let decoder = &mut session.decoder;
                if let Some(damage) = input.damage() {
                    return Err(Stopped::unavailable(damage.code, damage.message.clone()));
                }
                if session.drained {
                    self.count = Some(session.next_out);
                    return Ok(None);
                }
                match input.feed(decoder, context) {
                    Ok(true) => {}
                    Ok(false) => {
                        if input.damage().is_none() {
                            decoder.drain();
                            session.drained = true;
                        }
                    }
                    Err(stopped) => {
                        // Cancellation during a read can leave framing midway
                        // through a unit. A new session must replay it.
                        self.session = None;
                        return Err(stopped);
                    }
                }
                continue;
            }
            let Some(unit) = self.units.get(session.next_send).copied() else {
                if session.drained {
                    return Ok(None);
                }
                session.decoder.drain();
                session.drained = true;
                continue;
            };
            context.check()?;
            self.buffer.resize(unit.len, 0);
            self.file
                .seek(SeekFrom::Start(unit.offset))
                .map_err(|e| unreadable(format!("Cannot seek the preview file. {e}")))?;
            read_unit(&mut self.file, &mut self.buffer, context)?;
            let token = session.next_send as u64;
            let props = DataProps::default().with_user_data(token);
            context.check()?;
            match session
                .decoder
                .send_data(&self.buffer, props)
                .map_err(|e| decode_failure(document, token, DecodePosition::Unit, &e))?
            {
                SendStatus::Accepted => session.next_send += 1,
                // `get_picture` now waits for a picture.
                SendStatus::Full => {}
            }
        }
    }
}

fn read_unit(reader: &mut impl Read, buffer: &mut [u8], context: &Context<'_>) -> Outcome<()> {
    for chunk in buffer.chunks_mut(READ_CHUNK_BYTES) {
        context.check()?;
        reader
            .read_exact(chunk)
            .map_err(|e| unreadable(format!("Cannot read the preview file. {e}")))?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum DecodePosition {
    Output,
    Unit,
}

fn decode_failure(
    document: bool,
    at: u64,
    position: DecodePosition,
    error: &DecodeError,
) -> Stopped {
    if !document {
        return unreadable(format!("the output does not decode at frame {at}: {error}"));
    }
    let code = match error {
        DecodeError::Limit { .. }
        | DecodeError::UnsupportedStructure { .. }
        | DecodeError::UnsupportedFeature { .. } => PictureFailure::Unsupported,
        _ => PictureFailure::Unreadable,
    };
    let position = match position {
        DecodePosition::Output => "output",
        DecodePosition::Unit => "temporal unit",
    };
    Stopped::unavailable(
        code,
        format!("Cannot decode the document at {position} {at}. {error}"),
    )
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

struct Damage {
    code: PictureFailure,
    message: String,
}

type Index = (Vec<Unit>, Option<Damage>, Option<(u32, u32)>);

/// Check cancellation between local reads, including reads inside one unit.
struct ScanReader<'a, R> {
    context: &'a Context<'a>,
    inner: R,
    completed: u64,
    total: u64,
}

impl<R: Read> Read for ScanReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.context
            .check()
            .map_err(|_| io::Error::other("Preview request ended."))?;
        let chunk = buffer.len().min(64 << 10);
        let count = self.inner.read(&mut buffer[..chunk])?;
        self.completed += count as u64;
        self.context
            .progress(PictureStage::Scan, self.completed, Some(self.total));
        Ok(count)
    }
}

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
fn index(file: impl Read) -> Outcome<Index> {
    let mut reader = TemporalUnitReader::with_max_unit_bytes(BufReader::new(file), MAX_UNIT_BYTES);
    let mut units: Vec<Unit> = Vec::new();
    let damaged = loop {
        if units.len() >= MAX_UNITS {
            break Some(Damage {
                code: PictureFailure::Unsupported,
                message: format!("Preview can read at most {MAX_UNITS} temporal units."),
            });
        }
        let unit = match reader.next_temporal_unit() {
            Ok(Some(unit)) => unit,
            Ok(None) => break ivf_damage(&reader),
            Err(e) => break Some(framing_damage(e)),
        };
        if unit.pts.is_none()
            && let Err(fault) = annex_b_fault(unit.bytes)
        {
            break Some(Damage {
                code: PictureFailure::Unreadable,
                message: format!("the unit at byte {} {fault}", unit.offset.get()),
            });
        }
        units.push(Unit {
            offset: unit.offset.get(),
            len: unit.bytes.len(),
            pts: unit.pts.unwrap_or(0),
            random_access: is_random_access_point(unit.bytes),
        });
    };
    let time_base = reader
        .ivf_header()
        .map(|header| (header.timebase_numerator, header.timebase_denominator));
    let damaged = damaged.or_else(|| units.is_empty().then(empty_stream));
    Ok((units, damaged, time_base))
}

fn empty_stream() -> Damage {
    Damage {
        code: PictureFailure::Unreadable,
        message: "The preview file contains no temporal units.".into(),
    }
}

fn ivf_damage(reader: &TemporalUnitReader<impl Read>) -> Option<Damage> {
    reader.ivf_warning().map(|warning| Damage {
        code: PictureFailure::Unreadable,
        message: format!("The IVF file is damaged ({}).", warning.rule_id()),
    })
}

fn framing_damage(error: ReaderError) -> Damage {
    Damage {
        code: match error {
            ReaderError::UnitTooLarge { .. } => PictureFailure::Unsupported,
            _ => PictureFailure::Unreadable,
        },
        message: format!("Cannot parse the preview file. {error}"),
    }
}

fn partial_message(end: u64) -> String {
    format!("Analyze verified only the first {end} bytes. Preview stops at that boundary.")
}

fn partial_coverage(end: u64) -> Stopped {
    Stopped::unavailable(PictureFailure::PartialCoverage, partial_message(end))
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
    fn a_superseded_request_stops_reading_a_temporal_unit() {
        use super::super::{Request, Shared};

        struct CancellingRead<'a> {
            shared: &'a Shared,
            reads: usize,
        }

        impl Read for CancellingRead<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                self.reads += 1;
                self.shared.submit(2, Request::Stop);
                buffer.fill(0);
                Ok(buffer.len())
            }
        }

        let shared = Shared::default();
        let mut reader = CancellingRead {
            shared: &shared,
            reads: 0,
        };
        let context = Context {
            shared: &shared,
            serial: 0,
            request_id: 1,
            generation: 1,
            last_progress: std::cell::Cell::default(),
        };
        let mut buffer = vec![0; READ_CHUNK_BYTES * 2];
        assert!(matches!(
            read_unit(&mut reader, &mut buffer, &context),
            Err(Stopped::Superseded)
        ));
        assert_eq!(reader.reads, 1);
    }

    #[test]
    fn preview_stops_at_the_verified_prefix_of_a_sparse_three_gib_file()
    -> Result<(), Box<dyn std::error::Error>> {
        for kind in [OutputKind::Ivf, OutputKind::Av2AnnexB] {
            let (bytes, _) = encoded(kind);
            let (units, _, _) = index(bytes.as_slice()).map_err(|e| format!("{e:?}"))?;
            let fourth = units.get(3).ok_or("Missing fourth temporal unit.")?;
            let end = usize::try_from(fourth.offset)? + fourth.len;
            let mut source = tempfile::NamedTempFile::new()?;
            source.write_all(&bytes)?;
            source.as_file().set_len(3 << 30)?;
            let path = WirePath::from_path(source.path());
            let fingerprint = SourceFingerprint::of(&source.as_file().metadata()?);
            let mut hash = HashReader::new(&bytes[..end]);
            io::copy(&mut hash, &mut io::sink())?;
            let digest = hash.finish();
            let shared = super::super::Shared::default();
            let context = Context {
                shared: &shared,
                request_id: 1,
                generation: 1,
                serial: 0,
                last_progress: std::cell::Cell::default(),
            };
            let mut wrong_digest = digest.clone();
            wrong_digest.sha256 = "0".repeat(64);
            assert!(matches!(
                ResultPictures::open(&path, None, Some((fingerprint, &wrong_digest)), &context),
                Err(Stopped::Unavailable(PictureFailure::SourceChanged, _))
            ));
            let mut pictures =
                ResultPictures::open(&path, None, Some((fingerprint, &digest)), &context)
                    .map_err(|e| format!("{e:?}"))?;
            pictures
                .seek(&context, 0, GrainPolicy::Apply)
                .map_err(|e| format!("{e:?}"))?;

            // The fourth IVF unit has a complete record. For Annex B the
            // prefix does not establish the boundary of its last unit.
            let covered = if kind == OutputKind::Ivf { 4 } else { 3 };
            for ordinal in 0..covered as u64 {
                let output = pictures
                    .decode_next(&context)
                    .map_err(|e| format!("{e:?}"))?;
                assert_eq!(output.map(|(index, _)| index), Some(ordinal));
            }
            assert!(matches!(
                pictures.decode_next(&context),
                Err(Stopped::Unavailable(PictureFailure::PartialCoverage, message))
                    if message.contains("Analyze verified only")
            ));
            assert!(
                pictures
                    .session
                    .as_ref()
                    .is_some_and(|session| !session.drained)
            );
            assert_eq!(pictures.frame_count(), None);
        }
        Ok(())
    }

    #[test]
    fn document_replay_crosses_the_encoded_result_index_limit()
    -> Result<(), Box<dyn std::error::Error>> {
        use splot_core::ivf::{IVF_FRAME_HEADER_SIZE, parse_ivf_header, write_ivf_header};

        let (bytes, _) = encoded(OutputKind::Ivf);
        let mut header = parse_ivf_header(&bytes)?;
        let start = usize::from(header.header_len);
        header.frame_count += u32::try_from(MAX_UNITS)?;
        let mut source = tempfile::NamedTempFile::new()?;
        write_ivf_header(&mut source, &header)?;
        for _ in 0..(MAX_UNITS / 1024) {
            source.write_all(&[0; IVF_FRAME_HEADER_SIZE * 1024])?;
        }
        source.write_all(&bytes[start..])?;
        let mut hash = HashReader::new(File::open(source.path())?);
        io::copy(&mut hash, &mut io::sink())?;
        let digest = hash.finish();
        let path = WirePath::from_path(source.path());
        let fingerprint = SourceFingerprint::of(&source.as_file().metadata()?);
        let shared = super::super::Shared::default();
        let context = Context {
            shared: &shared,
            request_id: 1,
            generation: 1,
            serial: 0,
            last_progress: std::cell::Cell::default(),
        };
        let mut pictures =
            ResultPictures::open(&path, None, Some((fingerprint, &digest)), &context)
                .map_err(|e| format!("{e:?}"))?;
        assert!(pictures.units.is_empty());
        for ordinal in [0, 8, 0] {
            pictures
                .seek(&context, ordinal, GrainPolicy::Apply)
                .map_err(|e| format!("{e:?}"))?;
            let Some((index, Decoded::Document(decoded))) =
                pictures.next(&context).map_err(|e| format!("{e:?}"))?
            else {
                return Err("Missing document picture.".into());
            };
            assert_eq!(index, ordinal);
            assert_eq!(decoded.output.ordinal, ordinal);
            if ordinal == 0 {
                assert_eq!(
                    decoded.output.unit_offset,
                    (start + (MAX_UNITS + 1) * IVF_FRAME_HEADER_SIZE) as u64
                );
            }
        }
        Ok(())
    }

    #[test]
    fn the_index_finds_every_key_frame() -> Result<(), Box<dyn std::error::Error>> {
        for kind in [OutputKind::Ivf, OutputKind::Av2AnnexB] {
            let (bytes, keys) = encoded(kind);
            assert!(keys.len() >= 3, "{keys:?}");
            let mut file = tempfile::tempfile()?;
            file.write_all(&bytes)?;
            file.seek(SeekFrom::Start(0))?;
            let (units, damaged, time_base) = index(file).map_err(|e| format!("{e:?}"))?;
            assert!(damaged.is_none());
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
