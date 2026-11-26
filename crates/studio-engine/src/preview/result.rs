// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Finished encoder output and independent AV2 documents share one decoder
//! session and a bounded temporal-unit index. Only Studio encoder output
//! can use its one-picture-per-unit contract for random-access seeks.
//! Documents count the decoder's emitted pictures and replay valid state.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::PathBuf;

use splot_core::annexb::AnnexBObuCursor;
use splot_core::headers::ContentInterpretation;
use splot_core::span::ByteOffset;
use splot_core::stream_reader::{ReaderError, TemporalUnitReader, is_random_access_point};
use splot_core::types::ObuType;
use splot_decode::{
    DataProps, DecodeError, DecodeLimitThreshold, DecodeLimits, Decoder, ObuLocation, Picture,
    SendStatus,
};
use studio_domain::analysis::SourceDigest;
use studio_domain::job::{SourceFingerprint, WirePath};
use studio_domain::preview::{
    ColorMatrix, MediaTime, OutputEvent, PictureFailure, PictureStage, SourceRef,
};

use super::{Context, Decoded, Outcome, Stopped};
use crate::inspect::document::HashReader;
use crate::output::FileId;
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
    /// Present for a document, whose identity must match analysis.
    document_id: Option<FileId>,
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

/// A document picture with the decoder evidence necessary to join it to source bytes.
pub(super) struct DocumentPicture {
    pub picture: Picture,
    pub output: OutputEvent,
    pub time: Option<MediaTime>,
}

impl DocumentPicture {
    pub fn matrix(&self) -> Outcome<Option<ColorMatrix>> {
        let Some(color) = self
            .picture
            .content_interpretation()
            .map(ContentInterpretation::derived_color)
        else {
            return Ok(None);
        };
        let unsupported = |message| Stopped::unavailable(PictureFailure::Unsupported, message);
        if color.full_range || matches!(color.code_points.transfer_characteristics, 16 | 18) {
            return Err(unsupported(
                "Preview supports only limited-range SDR pictures.",
            ));
        }
        if self.picture.plane(splot_decode::PlaneId::U).is_none() {
            return Ok(None);
        }
        match color.code_points.matrix_coefficients {
            1 => Ok(Some(ColorMatrix::Bt709)),
            2 => Ok(None),
            5 | 6 => Ok(Some(ColorMatrix::Bt601)),
            9 => Ok(Some(ColorMatrix::Bt2020)),
            _ => Err(unsupported("Preview does not support this color matrix.")),
        }
    }
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
        let (indexed, document_id) = if let Some((expected, digest)) = document {
            if fingerprint != expected || digest.bytes > fingerprint.len {
                return Err(changed());
            }
            let id = FileId::of_file(&file, &path).map_err(inspect_error)?;
            source::still_the_same(&path, &id, fingerprint).map_err(Stopped::from_stop)?;
            let reader = ScanReader {
                context,
                inner: reader,
                completed: 0,
                total: fingerprint.len,
            };
            let mut reader = HashReader::with_limit(reader, digest.bytes);
            let indexed = index(&mut reader, Some(context))?;
            let remaining = reader.remaining();
            io::copy(&mut reader.by_ref().take(remaining), &mut io::sink()).map_err(|e| {
                context
                    .check()
                    .err()
                    .unwrap_or_else(|| unreadable(format!("Cannot verify the document bytes. {e}")))
            })?;
            if &reader.finish() != digest {
                return Err(changed());
            }
            source::still_the_same(&path, &id, fingerprint).map_err(Stopped::from_stop)?;
            (indexed, Some(id))
        } else {
            (index(reader, None)?, None)
        };
        let (units, damaged, ivf_time_base) = indexed;
        let (matrix, source_rate) = reference.map_or((None, None), facts_of);
        Ok(Self {
            path,
            fingerprint,
            document_id,
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

    pub fn seek(&mut self, context: &Context<'_>, index: u64) -> Outcome<()> {
        let document = self.is_document();
        let start = if document {
            if self.count.is_some_and(|count| index >= count) {
                return Err(super::out_of_range(index, self.count));
            }
            if index >= MAX_UNITS as u64 {
                return Err(Stopped::unavailable(
                    PictureFailure::Unsupported,
                    format!("Preview can decode at most {MAX_UNITS} output pictures."),
                ));
            }
            // An arbitrary access point need not restore the sequence, layer,
            // or external-header state. Start at byte zero for a fresh session.
            0
        } else {
            self.encoded_seek_start(index)?
        };
        let reusable = self.session.as_ref().is_some_and(|session| {
            (document || !session.drained)
                && session.next_out <= index
                && session.next_out >= start as u64
        });
        if !reusable {
            let decoder = if let Some(mut session) = self.session.take() {
                session.decoder.flush();
                session.decoder
            } else {
                let decoder = if document {
                    let limit = DecodeLimitThreshold::max(MAX_UNITS as u64);
                    let limits = DecodeLimits::DEFAULT
                        .with_max_frames_to_decode(limit)
                        .with_max_output_frames(limit);
                    stream::preview_decoder(PREVIEW_DECODE_THREADS, limits)
                } else {
                    stream::decoder(PREVIEW_DECODE_THREADS)
                };
                decoder.map_err(|e| decode_failure(document, 0, DecodePosition::Output, &e))?
            };
            self.session = Some(Session {
                decoder,
                next_send: start,
                next_out: start as u64,
                drained: false,
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
            Decoded::Document(Box::new(self.document_picture(index, picture)?))
        } else {
            Decoded::Picture(Box::new(picture))
        };
        Ok(Some((index, decoded)))
    }

    fn unit(&self, token: u64) -> Outcome<&Unit> {
        usize::try_from(token)
            .ok()
            .and_then(|token| self.units.get(token))
            .ok_or_else(|| unreadable("The decoder returned an unknown temporal unit."))
    }

    fn file_offset(&self, location: ObuLocation) -> Outcome<u64> {
        let unit = self.unit(location.user_data)?;
        let offset = location.offset.get();
        if offset < unit.len as u64
            && let Some(absolute) = unit.offset.checked_add(offset)
        {
            return Ok(absolute);
        }
        Err(unreadable(
            "The decoder returned an OBU location outside its temporal unit.",
        ))
    }

    fn document_picture(&self, index: u64, picture: Picture) -> Outcome<DocumentPicture> {
        let provenance = picture.provenance();
        let unit = self.unit(provenance.display.user_data)?;
        let time = self.ivf_time_base.and_then(|(num, den)| {
            Some(MediaTime {
                num: unit.pts.checked_mul(u64::from(num))?,
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
            unit_offset: unit.offset,
            unit_len: unit.len as u64,
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
            if let Some(picture) = session.decoder.get_picture().map_err(|e| {
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
            let Some(unit) = self.units.get(session.next_send).copied() else {
                if session.drained {
                    if document {
                        if let Some(damage) = &self.damaged {
                            return Err(Stopped::unavailable(damage.code, damage.message.clone()));
                        }
                        self.count = Some(session.next_out);
                    }
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
                .map_err(|e| unreadable(format!("Cannot read the preview file. {e}")))?;
            let token = session.next_send as u64;
            let props = DataProps::default().with_user_data(token);
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
fn index(file: impl Read, context: Option<&Context<'_>>) -> Outcome<Index> {
    let mut reader = TemporalUnitReader::with_max_unit_bytes(BufReader::new(file), MAX_UNIT_BYTES);
    let mut units: Vec<Unit> = Vec::new();
    let document = context.is_some();
    let damaged = loop {
        if let Some(context) = context {
            context.check()?;
        }
        if units.len() >= MAX_UNITS {
            break Some(Damage {
                code: PictureFailure::Unsupported,
                message: format!("Preview can read at most {MAX_UNITS} temporal units."),
            });
        }
        let unit = match reader.next_temporal_unit() {
            Ok(Some(unit)) => unit,
            Ok(None) => {
                break reader.ivf_warning().map(|warning| Damage {
                    code: PictureFailure::Unreadable,
                    message: format!("The IVF file is damaged ({}).", warning.rule_id()),
                });
            }
            Err(e) => {
                if let Some(context) = context {
                    context.check()?;
                }
                let code = match e {
                    ReaderError::UnitTooLarge { .. } => PictureFailure::Unsupported,
                    _ => PictureFailure::Unreadable,
                };
                break Some(Damage {
                    code,
                    message: format!("Cannot parse the preview file. {e}"),
                });
            }
        };
        if !document
            && unit.pts.is_none()
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
            random_access: !document && is_random_access_point(unit.bytes),
        });
        if document
            && reader
                .ivf_header()
                .is_some_and(|header| header.fourcc != *b"AV02")
        {
            break None;
        }
    };
    let ivf_header = reader.ivf_header();
    let damaged = if document && ivf_header.is_some_and(|header| header.fourcc != *b"AV02") {
        units.clear();
        Some(Damage {
            code: PictureFailure::Unsupported,
            message: "Preview supports only AV02 IVF files.".into(),
        })
    } else {
        damaged
    };
    let time_base = ivf_header
        .map(|header| (header.timebase_numerator, header.timebase_denominator))
        .filter(|(num, den)| !document || (*num != 0 && *den != 0));
    let damaged = damaged.or_else(|| {
        units.is_empty().then(|| Damage {
            code: PictureFailure::Unreadable,
            message: "The preview file contains no temporal units.".into(),
        })
    });
    Ok((units, damaged, time_base))
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
    fn the_index_finds_every_key_frame() -> Result<(), Box<dyn std::error::Error>> {
        for kind in [OutputKind::Ivf, OutputKind::Av2AnnexB] {
            let (bytes, keys) = encoded(kind);
            assert!(keys.len() >= 3, "{keys:?}");
            let mut file = tempfile::tempfile()?;
            file.write_all(&bytes)?;
            file.seek(SeekFrom::Start(0))?;
            let (units, damaged, time_base) = index(file, None).map_err(|e| format!("{e:?}"))?;
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
