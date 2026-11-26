// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! One analysis document: a local file opened read-only, its framing read
//! once with Splot's streaming demuxer ([`TemporalUnitReader::next_unit`]) and
//! envelope parser ([`AnnexBObuCursor`]), and the facts kept for paging.
//!
//! Reading holds one unit (an Annex B OBU or an IVF frame record) of at most
//! [`MAX_UNIT_BYTES`] in memory; Splot's reader grows its buffer only as bytes
//! arrive, so a declared length is never allocated ahead of its data. At most
//! [`MAX_RETAINED_ELEMENTS`] elements are kept, then reading stops.
//!
//! What cannot be parsed is an explicit unparsed region starting at the end
//! of the last parsed element:
//! - Annex B cannot be resynchronised after a bad or truncated length, so
//!   the rest of the file is one region.
//! - An IVF record whose payload does not parse keeps its packet and the OBUs
//!   before the fault; the rest of the payload is a region, and the next
//!   record is read as usual.
//! - A truncated last IVF record, a partial record header, an over-cap unit
//!   or a read failure ends reading; the rest of the file, from the start of
//!   that record, is one region.
//! - The element limit ends reading too; the rest of the file, from the end
//!   of the last element kept, is one region. When that element is inside
//!   an IVF payload, the region starts inside the payload and ends past it,
//!   so it belongs to no packet.
//!
//! Splot reports where each OBU header starts, its size and its payload; a
//! length prefix span, an IVF record header span and every elementary-stream
//! span are Studio arithmetic on those values ([`Provenance::Derived`]).

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::PathBuf;

use splot_core::annexb::AnnexBObuCursor;
use splot_core::ivf::{IVF_FRAME_HEADER_SIZE, IvfHeader};
use splot_core::obu::ObuHeader;
use splot_core::span::ByteOffset;
use splot_core::stream_reader::{ReaderError, StreamUnit, TemporalUnitReader};
use splot_core::types::ObuType;
use studio_domain::analysis::{
    AnalysisFailure, DocumentSummary, Element, Framing, IvfHeaderFact, LayerCount, LayerId,
    LocateTarget, MAX_BYTE_RANGE, MAX_PAGE_ELEMENTS, ObuFact, ObuTypeCount, PacketFact, Problem,
    ProblemKind, Provenance, Row, RowFilter, ScanOutcome, Span,
};
use studio_domain::job::{SourceFingerprint, WirePath};
use studio_domain::protocol::MAX_MESSAGE_BYTES;

use crate::output::FileId;
use crate::source;

mod evidence;

use evidence::Evidence;

/// The largest unit read: one Annex B OBU, or one IVF frame payload. The
/// same cap as the preview's result reader; a larger declared unit stops
/// reading as [`ProblemKind::LimitReached`].
pub const MAX_UNIT_BYTES: usize = 64 << 20;

/// The most elements kept for one document, besides the final unparsed
/// region.
///
/// Their list never has more than [`ELEMENT_SLOTS`] slots of at most 64
/// bytes, 16 MiB: it doubles up to [`GROWTH_STEP_LIMIT`] slots (1 MiB) and
/// then grows once to the full size, so it holds at most 17 MiB while it
/// moves. Messages come on top: at most [`MAX_KEPT_MESSAGE_BYTES`] for the
/// regions inside IVF payloads, and two of at most [`MAX_PROBLEM_MESSAGE`]
/// bytes: the final region's and the summary's problem (the stop, or the
/// first damage).
pub const MAX_RETAINED_ELEMENTS: usize = 1 << 18;

/// The slots of the element list at its full size: the elements and the
/// final unparsed region.
pub const ELEMENT_SLOTS: usize = MAX_RETAINED_ELEMENTS + 1;

/// Below this many slots the element list doubles; above it, it grows once
/// to [`ELEMENT_SLOTS`].
const GROWTH_STEP_LIMIT: usize = 1 << 14;

/// The longest problem message kept, in bytes.
pub const MAX_PROBLEM_MESSAGE: usize = 512;

/// The most message bytes kept for the unparsed regions inside IVF
/// payloads of one document; later regions say only that the reason was not
/// kept.
pub const MAX_KEPT_MESSAGE_BYTES: usize = 64 * 1024;

/// The message of a region whose own message was not kept.
const MESSAGE_NOT_KEPT: &str =
    "the OBU does not parse; the reason is not kept, too many regions did not parse";

/// The part of a message that a page's elements may take; the rest is for
/// the envelope around them.
const PAGE_BUDGET: usize = MAX_MESSAGE_BYTES as usize - 1024;

/// Why a request has no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The machine-readable reason.
    pub code: AnalysisFailure,
    /// A human-readable explanation.
    pub message: String,
}

impl Refusal {
    pub(crate) fn new(code: AnalysisFailure, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// An open document.
#[derive(Debug)]
pub struct Document {
    path: PathBuf,
    file: File,
    id: FileId,
    summary: DocumentSummary,
    items: Vec<Item>,
    evidence: Evidence,
}

impl Document {
    /// Opens `source` and reads its framing. `cancelled` is polled while
    /// reading.
    ///
    /// # Errors
    /// [`AnalysisFailure::Io`] when the file cannot be opened or its first
    /// bytes read, [`AnalysisFailure::Unsupported`] when it is neither IVF
    /// nor Annex B, [`AnalysisFailure::Cancelled`], and
    /// [`AnalysisFailure::SourceChanged`] when it changed while being read.
    pub fn open(source: &WirePath, cancelled: &dyn Fn() -> bool) -> Result<Self, Refusal> {
        let io =
            |what: &str, e: io::Error| Refusal::new(AnalysisFailure::Io, format!("{what}: {e}"));
        let path = source
            .to_path_buf()
            .map_err(|e| Refusal::new(AnalysisFailure::InvalidRequest, e.to_string()))?;
        let not_a_file = || Refusal::new(AnalysisFailure::Unsupported, "not a regular file");
        // Before opening: opening a FIFO waits for a writer, and Windows
        // refuses to open a directory.
        let named = std::fs::metadata(&path).map_err(|e| io("cannot inspect the file", e))?;
        if !named.is_file() {
            return Err(not_a_file());
        }
        let file = File::open(&path).map_err(|e| io("cannot open the file", e))?;
        // The file opened, in case the path changed after the check.
        let metadata = file
            .metadata()
            .map_err(|e| io("cannot inspect the file", e))?;
        if !metadata.is_file() {
            return Err(not_a_file());
        }
        let fingerprint = SourceFingerprint::of(&metadata);
        let id = FileId::of_file(&file, &path).map_err(|e| io("cannot identify the file", e))?;
        let reader = file
            .try_clone()
            .map_err(|e| io("cannot read the file", e))?;
        let checked = Checked::new(reader, cancelled);
        let scanned = scan(BufReader::new(checked), fingerprint.len, cancelled)?;
        let document = Self {
            summary: scanned.summary(fingerprint),
            path,
            file,
            id,
            items: scanned.items,
            evidence: Evidence::default(),
        };
        document.check()?;
        Ok(document)
    }

    /// What opening found.
    #[must_use]
    pub const fn summary(&self) -> &DocumentSummary {
        &self.summary
    }

    /// The number of rows `filter` admits, and rows from `first` on: at
    /// most `count`, fewer at the end or when more would not fit in one
    /// protocol message.
    ///
    /// # Errors
    /// [`AnalysisFailure::InvalidRequest`] for a `count` outside
    /// `1..=MAX_PAGE_ELEMENTS` or a `first` past the last row, and
    /// [`AnalysisFailure::SourceChanged`].
    pub fn page(
        &self,
        filter: RowFilter,
        first: u64,
        count: u32,
    ) -> Result<(u64, Vec<Row>), Refusal> {
        self.check()?;
        page(&self.items, filter, first, count)
    }

    /// The element `target` names and its row among those `filter` admits.
    ///
    /// # Errors
    /// [`AnalysisFailure::InvalidRequest`] for an offset outside the file or
    /// an index past the last element, and [`AnalysisFailure::SourceChanged`].
    pub fn locate(
        &self,
        filter: RowFilter,
        target: LocateTarget,
    ) -> Result<(Option<u64>, Row), Refusal> {
        self.check()?;
        locate(&self.items, filter, target)
    }

    /// The file's bytes `offset..offset + len`.
    ///
    /// # Errors
    /// [`AnalysisFailure::InvalidRequest`] for a `len` outside
    /// `1..=MAX_BYTE_RANGE` or a range that leaves the file,
    /// [`AnalysisFailure::SourceChanged`] and [`AnalysisFailure::Io`].
    pub fn bytes(&mut self, offset: u64, len: u32) -> Result<Vec<u8>, Refusal> {
        self.check()?;
        let read = read_range(&mut self.file, self.summary.fingerprint.len, offset, len);
        // The bytes count only if the file is still the one opened after
        // they were read.
        self.check()?;
        read
    }

    /// Whether the path still names the opened file, unchanged.
    fn check(&self) -> Result<(), Refusal> {
        source::still_the_same(&self.path, &self.id, self.summary.fingerprint).map_err(|_| {
            Refusal::new(
                AnalysisFailure::SourceChanged,
                "the file changed or was replaced since it was opened",
            )
        })
    }
}

/// Fails reads once `cancelled` says so.
pub(super) struct Checked<'a, R> {
    inner: R,
    cancelled: &'a dyn Fn() -> bool,
}

impl<'a, R> Checked<'a, R> {
    pub(super) fn new(inner: R, cancelled: &'a dyn Fn() -> bool) -> Self {
        Self { inner, cancelled }
    }
}

impl<R: Read> Read for Checked<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if (self.cancelled)() {
            return Err(io::Error::other("cancelled"));
        }
        self.inner.read(buf)
    }
}

/// One kept element, in the compact form it is stored in.
#[derive(Debug, Clone)]
enum Item {
    IvfHeader {
        len: u64,
    },
    Packet {
        index: u64,
        pts: u64,
        payload_offset: u64,
        len: u64,
        stream_offset: u64,
    },
    Obu {
        index: u64,
        packet: Option<u64>,
        /// Where its length prefix starts.
        prefix: u64,
        /// Where its header starts, as Splot reports it.
        offset: u64,
        /// `num_bytes_in_obu`.
        size: u32,
        header: ObuHeader,
        stream_offset: u64,
    },
    Unparsed {
        offset: u64,
        len: u64,
        stream_offset: Option<u64>,
        kind: ProblemKind,
        message: String,
    },
}

impl Item {
    /// Its first byte in the file. Elements are kept in this order.
    fn start(&self) -> u64 {
        match self {
            Self::IvfHeader { .. } => 0,
            Self::Packet { payload_offset, .. } => {
                payload_offset.saturating_sub(IVF_FRAME_HEADER_SIZE as u64)
            }
            Self::Obu { prefix, .. } => *prefix,
            Self::Unparsed { offset, .. } => *offset,
        }
    }

    /// Whether `filter` admits it.
    const fn admitted(&self, filter: RowFilter) -> bool {
        match self {
            Self::IvfHeader { .. } | Self::Packet { .. } => filter.is_all(),
            Self::Obu { header, .. } => filter.admits_obu(
                header.obu_type.raw(),
                header.temporal_layer_id.get(),
                header.embedded_layer_id.get(),
            ),
            Self::Unparsed { .. } => filter.admits_unparsed(),
        }
    }

    /// The first byte after it in the file.
    fn end(&self) -> u64 {
        match self {
            Self::IvfHeader { len } => *len,
            Self::Packet {
                payload_offset,
                len,
                ..
            } => payload_offset.saturating_add(*len),
            Self::Obu { offset, size, .. } => offset.saturating_add(u64::from(*size)),
            Self::Unparsed { offset, len, .. } => offset.saturating_add(*len),
        }
    }

    fn element(&self) -> Element {
        let span = |offset, len, provenance| Span {
            offset,
            len,
            provenance,
        };
        match self {
            Self::IvfHeader { len } => Element::IvfHeader {
                span: span(0, *len, Provenance::Signaled),
            },
            Self::Packet {
                index,
                pts,
                payload_offset,
                len,
                stream_offset,
            } => Element::Packet {
                packet: PacketFact {
                    index: *index,
                    pts: *pts,
                    header: span(
                        payload_offset.saturating_sub(IVF_FRAME_HEADER_SIZE as u64),
                        IVF_FRAME_HEADER_SIZE as u64,
                        Provenance::Derived,
                    ),
                    payload: span(*payload_offset, *len, Provenance::Located),
                    stream: span(*stream_offset, *len, Provenance::Derived),
                },
            },
            Self::Obu {
                index,
                packet,
                prefix,
                offset,
                size,
                header,
                stream_offset,
            } => {
                let size = u64::from(*size);
                let header_len = u64::from(header.header_size_bytes);
                let prefix_len = offset.saturating_sub(*prefix);
                let extension = if header.has_header_extension {
                    Provenance::Signaled
                } else {
                    Provenance::Inferred
                };
                let layer = |value, provenance| LayerId { value, provenance };
                Element::Obu {
                    obu: ObuFact {
                        index: *index,
                        packet: *packet,
                        obu_type: header.obu_type.raw(),
                        type_name: header.obu_type.spec_name().to_owned(),
                        reserved: header.obu_type.is_reserved(),
                        has_extension: header.has_header_extension,
                        temporal_layer_id: layer(
                            header.temporal_layer_id.get(),
                            Provenance::Signaled,
                        ),
                        embedded_layer_id: layer(header.embedded_layer_id.get(), extension),
                        extended_layer_id: layer(header.extended_layer_id.get(), extension),
                        length_prefix: span(*prefix, prefix_len, Provenance::Derived),
                        obu: span(*offset, size, Provenance::Located),
                        header: span(*offset, header_len, Provenance::Located),
                        payload: span(
                            offset.saturating_add(header_len),
                            size.saturating_sub(header_len),
                            Provenance::Located,
                        ),
                        stream: span(
                            *stream_offset,
                            prefix_len.saturating_add(size),
                            Provenance::Derived,
                        ),
                    },
                }
            }
            Self::Unparsed {
                offset,
                len,
                kind,
                message,
                ..
            } => Element::Unparsed {
                span: span(*offset, *len, Provenance::Derived),
                kind: *kind,
                message: if message.is_empty() {
                    MESSAGE_NOT_KEPT.to_owned()
                } else {
                    message.clone()
                },
            },
        }
    }
}

/// What reading a file found.
#[derive(Debug)]
struct Scanned {
    framing: Framing,
    header: Option<IvfHeader>,
    items: Vec<Item>,
    outcome: ScanOutcome,
}

impl Scanned {
    fn summary(&self, fingerprint: SourceFingerprint) -> DocumentSummary {
        let count =
            |wanted: fn(&Item) -> bool| self.items.iter().filter(|i| wanted(i)).count() as u64;
        DocumentSummary {
            fingerprint,
            framing: self.framing,
            ivf_header: self.header.map(|h| IvfHeaderFact {
                version: h.version,
                header_len: h.header_len,
                fourcc: h.fourcc,
                width: h.width,
                height: h.height,
                timebase_numerator: h.timebase_numerator,
                timebase_denominator: h.timebase_denominator,
                frame_count: h.frame_count,
            }),
            elements: self.items.len() as u64,
            packets: count(|i| matches!(i, Item::Packet { .. })),
            obus: count(|i| matches!(i, Item::Obu { .. })),
            unparsed: count(|i| matches!(i, Item::Unparsed { .. })),
            obu_types: self.obu_types(),
            layers: self.layers(),
            outcome: self.outcome.clone(),
        }
    }

    /// The OBU types present, by raw type. `obu_type` has five bits.
    fn obu_types(&self) -> Vec<ObuTypeCount> {
        let mut counts = [(0_u64, ""); 32];
        for item in &self.items {
            if let Item::Obu { header, .. } = item
                && let Some(slot) = counts.get_mut(usize::from(header.obu_type.raw()))
            {
                *slot = (slot.0 + 1, header.obu_type.spec_name());
            }
        }
        (0_u8..)
            .zip(counts)
            .filter(|(_, (count, _))| *count > 0)
            .map(|(obu_type, (count, name))| ObuTypeCount {
                obu_type,
                type_name: name.to_owned(),
                count,
            })
            .collect()
    }

    /// The layer pairs present. `obu_tlayer_id` has two bits and
    /// `obu_mlayer_id` three.
    fn layers(&self) -> Vec<LayerCount> {
        // (all, inferred) per pair.
        let mut counts = [[(0_u64, 0_u64); 8]; 4];
        for item in &self.items {
            if let Item::Obu { header, .. } = item
                && let Some(slot) = counts
                    .get_mut(usize::from(header.temporal_layer_id.get()))
                    .and_then(|row| row.get_mut(usize::from(header.embedded_layer_id.get())))
            {
                slot.0 += 1;
                slot.1 += u64::from(!header.has_header_extension);
            }
        }
        let mut layers = Vec::new();
        for (temporal, row) in (0_u8..).zip(counts) {
            for (embedded, (count, inferred)) in (0_u8..).zip(row) {
                if count > 0 {
                    layers.push(LayerCount {
                        temporal,
                        embedded,
                        count,
                        inferred,
                    });
                }
            }
        }
        layers
    }
}

/// The facts gathered so far.
#[derive(Debug, Default)]
struct Builder {
    framing: Option<Framing>,
    items: Vec<Item>,
    obus: u64,
    /// The elementary-stream offset of the next IVF payload.
    stream: u64,
    /// The first region that did not parse while reading went on.
    damaged: Option<Problem>,
    /// The heap bytes of the messages kept for those regions.
    message_bytes: usize,
}

/// What one unit did to the reading.
enum Step {
    Continue,
    End,
    Stop(ProblemKind, String),
}

fn problem(kind: ProblemKind, message: impl Into<String>) -> Step {
    Step::Stop(kind, message.into())
}

impl Builder {
    /// The first byte after the last element.
    fn end(&self) -> u64 {
        self.items.last().map_or(0, Item::end)
    }

    /// Keeps `item`, unless the limit is reached.
    fn push(&mut self, item: Item) -> Result<(), Step> {
        // Until an IVF document has its framing, its header still needs a slot.
        let room = MAX_RETAINED_ELEMENTS - usize::from(self.framing.is_none());
        if self.items.len() >= room {
            return Err(problem(
                ProblemKind::LimitReached,
                format!("only the first {MAX_RETAINED_ELEMENTS} elements are kept"),
            ));
        }
        if !make_room(&mut self.items) {
            return Err(problem(
                ProblemKind::LimitReached,
                "no memory for more elements",
            ));
        }
        self.items.push(item);
        Ok(())
    }

    /// One Annex B unit: exactly one OBU, or the bytes that did not parse.
    fn annex_b(&mut self, offset: ByteOffset, bytes: &[u8]) -> Step {
        let first = self.framing.is_none();
        let parsed = AnnexBObuCursor::new(bytes, offset).next_obu();
        if first {
            // Without a signature, a temporal delimiter first is the only
            // evidence that this is an AV2 stream.
            if !matches!(parsed, Ok(Some(ref obu)) if obu.header.obu_type == ObuType::TemporalDelimiter)
            {
                return Step::Continue;
            }
            self.framing = Some(Framing::AnnexB);
        }
        match parsed {
            Ok(Some(obu)) => {
                let at = offset.get();
                self.obu(at, &obu, None, at).err().unwrap_or(Step::Continue)
            }
            // The reader yields no empty unit.
            Ok(None) => Step::Continue,
            Err(e) => problem(
                ProblemKind::Malformed,
                format!("the OBU does not parse: {e}"),
            ),
        }
    }

    /// One IVF frame record and the OBUs of its payload.
    fn ivf_frame(
        &mut self,
        index: usize,
        pts: u64,
        payload_offset: ByteOffset,
        payload: &[u8],
    ) -> Step {
        let start = payload_offset.get();
        let len = payload.len() as u64;
        let stream = self.stream;
        self.stream = stream.saturating_add(len);
        let packet = Item::Packet {
            index: index as u64,
            pts,
            payload_offset: start,
            len,
            stream_offset: stream,
        };
        if let Err(stop) = self.push(packet) {
            return stop;
        }
        let mut obus = AnnexBObuCursor::new(payload, payload_offset);
        // The next OBU's length prefix: the payload start, then the end of
        // each OBU.
        let mut at = start;
        loop {
            match obus.next_obu() {
                Ok(Some(obu)) => {
                    let stream_offset = stream.saturating_add(at - start);
                    if let Err(stop) = self.obu(at, &obu, Some(index as u64), stream_offset) {
                        return stop;
                    }
                    at = obu.offset.get().saturating_add(u64::from(obu.size));
                }
                Ok(None) => return Step::Continue,
                Err(e) => {
                    let mut message = clip(format!(
                        "IVF record {index}: the OBU does not parse: {e}; the rest of its payload is not read"
                    ));
                    self.damaged.get_or_insert_with(|| Problem {
                        kind: ProblemKind::Malformed,
                        offset: at,
                        message: message.clone(),
                    });
                    message.shrink_to_fit();
                    let kept = self.message_bytes.saturating_add(message.capacity());
                    if kept > MAX_KEPT_MESSAGE_BYTES {
                        message = String::new();
                    } else {
                        self.message_bytes = kept;
                    }
                    let region = Item::Unparsed {
                        offset: at,
                        len: start.saturating_add(len).saturating_sub(at),
                        stream_offset: Some(stream.saturating_add(at - start)),
                        kind: ProblemKind::Malformed,
                        message,
                    };
                    return self.push(region).err().unwrap_or(Step::Continue);
                }
            }
        }
    }

    fn obu(
        &mut self,
        prefix: u64,
        obu: &splot_core::annexb::ObuEnvelope<'_>,
        packet: Option<u64>,
        stream_offset: u64,
    ) -> Result<(), Step> {
        self.push(Item::Obu {
            index: self.obus,
            packet,
            prefix,
            offset: obu.offset.get(),
            size: obu.size,
            header: obu.header,
            stream_offset,
        })?;
        self.obus += 1;
        Ok(())
    }

    /// Ends reading: `stop` makes the rest of the file one region.
    fn finish(
        mut self,
        stop: Option<(ProblemKind, String)>,
        len: u64,
        header: Option<IvfHeader>,
    ) -> Result<Scanned, Refusal> {
        let Some(framing) = self.framing else {
            return Err(match stop {
                Some((ProblemKind::Io, message)) => Refusal::new(AnalysisFailure::Io, message),
                Some((ProblemKind::Cancelled, _)) => {
                    Refusal::new(AnalysisFailure::Cancelled, "the open was cancelled")
                }
                _ if len == 0 => Refusal::new(AnalysisFailure::Unsupported, "the file is empty"),
                _ => Refusal::new(
                    AnalysisFailure::Unsupported,
                    "neither an IVF file nor an AV2 Annex B stream that starts with a temporal \
                     delimiter OBU",
                ),
            });
        };
        let end = self.end();
        // Splot read every byte up to `len` unless it stopped; anything left
        // is reported, never dropped.
        let stop = stop.or_else(|| {
            (end < len).then(|| {
                (
                    ProblemKind::Malformed,
                    "bytes after the last element were not read".to_owned(),
                )
            })
        });
        let outcome = match stop {
            Some((kind, message)) => {
                let message = clip(message);
                // `push` left its slot free; only an allocation failure in
                // a small list can make this grow beyond it.
                if len > end && make_room(&mut self.items) {
                    self.items.push(Item::Unparsed {
                        offset: end,
                        len: len - end,
                        stream_offset: None,
                        kind,
                        message: message.clone(),
                    });
                }
                ScanOutcome::Stopped {
                    problem: Problem {
                        kind,
                        offset: end,
                        message,
                    },
                }
            }
            None => match self.damaged {
                Some(first) => ScanOutcome::Damaged { first },
                None => ScanOutcome::Complete,
            },
        };
        Ok(Scanned {
            framing,
            header,
            items: self.items,
            outcome,
        })
    }
}

/// Reads the framing of a file of `len` bytes from `input`.
fn scan(input: impl Read, len: u64, cancelled: &dyn Fn() -> bool) -> Result<Scanned, Refusal> {
    let cancel = || {
        Step::Stop(
            ProblemKind::Cancelled,
            "reading was cancelled; the elements read before are kept".to_owned(),
        )
    };
    let mut units = TemporalUnitReader::with_max_unit_bytes(input, MAX_UNIT_BYTES);
    let mut doc = Builder::default();
    let mut header: Option<IvfHeader> = None;
    let stop = loop {
        let step = if cancelled() {
            cancel()
        } else {
            match units.next_unit() {
                Ok(None) => Step::End,
                Ok(Some(StreamUnit::AnnexBObu { offset, bytes })) => doc.annex_b(offset, bytes),
                Ok(Some(StreamUnit::IvfFrame {
                    index,
                    pts,
                    payload_offset,
                    payload,
                })) => doc.ivf_frame(index, pts, payload_offset, payload),
                Ok(Some(StreamUnit::IvfWarning(warning))) => problem(
                    ProblemKind::Malformed,
                    format!(
                        "the file ends inside an IVF record header ({})",
                        warning.rule_id()
                    ),
                ),
                Err(ReaderError::UnitTooLarge { declared, cap, .. }) => problem(
                    ProblemKind::LimitReached,
                    format!("a unit declares {declared} bytes; at most {cap} are read"),
                ),
                // Splot reads a file as IVF exactly when it starts with `DKIF`.
                Err(ReaderError::Ivf(e)) => {
                    doc.framing = Some(Framing::Ivf);
                    problem(ProblemKind::Malformed, format!("{e} ({})", e.rule_id()))
                }
                Err(ReaderError::Io(_)) if cancelled() => cancel(),
                Err(ReaderError::Io(e)) => {
                    problem(ProblemKind::Io, format!("reading stopped: {e}"))
                }
            }
        };
        // Splot reads the IVF file header with the first record; it goes in
        // front of what was read with it.
        if doc.framing != Some(Framing::AnnexB)
            && header.is_none()
            && let Some(found) = units.ivf_header()
        {
            header = Some(found);
            doc.framing = Some(Framing::Ivf);
            let item = Item::IvfHeader {
                len: u64::from(found.header_len),
            };
            if found.fourcc != *b"AV02" {
                doc.items = vec![item];
                break Some((
                    ProblemKind::Unsupported,
                    format!(
                        "the IVF codec is {:?}, not AV02; its records are not read",
                        String::from_utf8_lossy(&found.fourcc)
                    ),
                ));
            }
            if make_room(&mut doc.items) {
                doc.items.insert(0, item);
            } else {
                break Some((
                    ProblemKind::LimitReached,
                    "no memory for more elements".to_owned(),
                ));
            }
        }
        match step {
            Step::Stop(kind, message) => break Some((kind, message)),
            Step::End => break None,
            Step::Continue if doc.framing.is_none() => break None,
            Step::Continue => {}
        }
    };
    doc.finish(stop, len, header)
}

/// Makes room for one more element without the list ever passing
/// [`ELEMENT_SLOTS`] slots; `false` when it is full or the memory is not
/// there.
fn make_room(items: &mut Vec<Item>) -> bool {
    let capacity = items.capacity();
    if items.len() < capacity {
        return true;
    }
    let wanted = if capacity < GROWTH_STEP_LIMIT {
        (capacity * 2).max(64)
    } else {
        ELEMENT_SLOTS
    };
    let wanted = wanted.min(ELEMENT_SLOTS);
    wanted > items.len() && items.try_reserve_exact(wanted - items.len()).is_ok()
}

/// Keeps a message to [`MAX_PROBLEM_MESSAGE`] bytes, on a character boundary.
fn clip(mut message: String) -> String {
    if message.len() > MAX_PROBLEM_MESSAGE {
        let mut at = MAX_PROBLEM_MESSAGE;
        while !message.is_char_boundary(at) {
            at -= 1;
        }
        message.truncate(at);
    }
    message
}

/// The packet whose payload holds the element at `at`, if any. Only the
/// OBUs and unparsed regions after a packet and wholly inside its payload
/// have one.
fn parent(items: &[Item], at: usize) -> Option<usize> {
    let item = items.get(at)?;
    if !matches!(
        item,
        Item::Obu {
            packet: Some(_),
            ..
        } | Item::Unparsed { .. }
    ) {
        return None;
    }
    let packet = items[..at]
        .iter()
        .rposition(|i| matches!(i, Item::Packet { .. }))?;
    let inside = item.start() < items[packet].end() && item.end() <= items[packet].end();
    inside.then_some(packet)
}

fn row(items: &[Item], at: usize) -> Row {
    Row {
        index: at as u64,
        parent: parent(items, at).map(|p| p as u64),
        element: items[at].element(),
    }
}

fn page(
    items: &[Item],
    filter: RowFilter,
    first: u64,
    count: u32,
) -> Result<(u64, Vec<Row>), Refusal> {
    if count == 0 || count > MAX_PAGE_ELEMENTS {
        return Err(Refusal::new(
            AnalysisFailure::InvalidRequest,
            format!("a page holds 1 to {MAX_PAGE_ELEMENTS} rows, not {count}"),
        ));
    }
    // The indices of up to `count` admitted elements from row `first` on,
    // and how many rows there are. Without a filter, rows are elements.
    let skip = usize::try_from(first).unwrap_or(usize::MAX);
    let take = count as usize;
    let (total, wanted): (u64, Vec<usize>) = if filter.is_all() {
        let start = skip.min(items.len());
        (
            items.len() as u64,
            (start..items.len()).take(take).collect(),
        )
    } else {
        let admitted = || (0..items.len()).filter(|at| items[*at].admitted(filter));
        (
            admitted().count() as u64,
            admitted().skip(skip).take(take).collect(),
        )
    };
    if first >= total && !(first == 0 && total == 0) {
        return Err(Refusal::new(
            AnalysisFailure::InvalidRequest,
            format!("there is no row {first}; the filter admits {total}"),
        ));
    }
    let mut rows = Vec::new();
    let mut used: usize = 0;
    for at in wanted {
        let row = row(items, at);
        // A comma between rows; an unencodable row cannot occur.
        let size = serde_json::to_vec(&row).map_or(usize::MAX, |bytes| bytes.len() + 1);
        if !rows.is_empty() && used.saturating_add(size) > PAGE_BUDGET {
            break;
        }
        used = used.saturating_add(size);
        rows.push(row);
    }
    Ok((total, rows))
}

fn locate(
    items: &[Item],
    filter: RowFilter,
    target: LocateTarget,
) -> Result<(Option<u64>, Row), Refusal> {
    let invalid = |message: String| Refusal::new(AnalysisFailure::InvalidRequest, message);
    let at = match target {
        LocateTarget::Element { index } => usize::try_from(index)
            .ok()
            .filter(|at| *at < items.len())
            .ok_or_else(|| {
                invalid(format!(
                    "there is no element {index}; the document has {}",
                    items.len()
                ))
            })?,
        LocateTarget::Offset { offset } => {
            let holds = |at: usize| {
                let item = &items[at];
                (item.start()..item.end()).contains(&offset)
            };
            // The last element that starts at or before the byte holds it,
            // unless the byte is past its end inside its packet's payload.
            items
                .partition_point(|i| i.start() <= offset)
                .checked_sub(1)
                .and_then(|at| {
                    if holds(at) {
                        Some(at)
                    } else {
                        parent(items, at).filter(|p| holds(*p))
                    }
                })
                .ok_or_else(|| invalid(format!("no element holds byte {offset}")))?
        }
    };
    let row_index = items[at]
        .admitted(filter)
        .then(|| items[..at].iter().filter(|i| i.admitted(filter)).count() as u64);
    Ok((row_index, row(items, at)))
}

/// Reads `offset..offset + len` of a file of `file_len` bytes.
fn read_range(
    file: &mut (impl Read + Seek),
    file_len: u64,
    offset: u64,
    len: u32,
) -> Result<Vec<u8>, Refusal> {
    if len == 0 || len > MAX_BYTE_RANGE {
        return Err(Refusal::new(
            AnalysisFailure::InvalidRequest,
            format!("a byte range is 1 to {MAX_BYTE_RANGE} bytes, not {len}"),
        ));
    }
    read_file_range(file, file_len, offset, len)
}

/// Reads a range after the caller applies the applicable allocation limit.
fn read_file_range(
    file: &mut (impl Read + Seek),
    file_len: u64,
    offset: u64,
    len: u32,
) -> Result<Vec<u8>, Refusal> {
    if offset
        .checked_add(u64::from(len))
        .is_none_or(|end| end > file_len)
    {
        return Err(Refusal::new(
            AnalysisFailure::InvalidRequest,
            format!("bytes {offset} + {len} are not inside the file of {file_len} bytes"),
        ));
    }
    // The caller bounds the allocation.
    let mut bytes = vec![0; len as usize];
    file.seek(SeekFrom::Start(offset))
        .and_then(|_| file.read_exact(&mut bytes))
        .map_err(|e| Refusal::new(AnalysisFailure::Io, format!("cannot read the file: {e}")))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests;
