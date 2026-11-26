// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! One analysis document: a local file opened read-only, its framing read
//! once with Splot's streaming demuxer ([`TemporalUnitReader::next_unit`]) and
//! envelope parser ([`AnnexBObuCursor`]), and the facts kept for paging.
//!
//! Read one OBU or IVF record at a time. Keep completed index pages on disk.
//! Splot reports where each OBU header starts, its size and its payload; a
//! length prefix span, an IVF record header span and every elementary-stream
//! span are Studio arithmetic on those values ([`Provenance::Derived`]).

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use splot_core::annexb::AnnexBObuCursor;
use splot_core::ivf::{IVF_FRAME_HEADER_SIZE, IVF_SIGNATURE, IvfHeader, parse_ivf_header};
use splot_core::obu::ObuHeader;
use splot_core::span::ByteOffset;
use splot_core::stream_reader::{ReaderError, StreamUnit, TemporalUnitReader};
use splot_core::types::ObuType;
use studio_domain::analysis::{
    AnalysisFailure, DocumentSummary, Element, Framing, IvfHeaderFact, LayerCount, LayerId,
    LocateTarget, MAX_BYTE_RANGE, MAX_PAGE_ELEMENTS, ObuFact, ObuTypeCount, PacketFact, Problem,
    ProblemKind, Provenance, Row, RowFilter, ScanOutcome, SourceDigest, Span,
};
use studio_domain::job::{SourceFingerprint, WirePath};
use studio_domain::protocol::MAX_MESSAGE_BYTES;

use crate::output::FileId;
use crate::source;

mod cache;
mod cache_session;
pub(super) mod evidence;

pub(crate) use cache_session::clear_cache;

use cache_session::{CacheSession, Progress};
mod index;

use index::Index;

use evidence::Evidence;

/// The largest unit read: one Annex B OBU, or one IVF frame payload. The
/// same cap as the preview's result reader; a larger declared unit stops
/// reading as [`ProblemKind::LimitReached`].
pub const MAX_UNIT_BYTES: usize = 64 << 20;

/// The number of OBUs validation reads by default.
pub const MAX_RETAINED_ELEMENTS: usize = 1 << 18;

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
    index: Index,
    scan: Option<Scan<BufReader<HashReader<File>>>>,
    evidence: Evidence,
    cache: Option<CacheSession>,
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
        let mut document = Self::open_impl(source, cancelled, false)?;
        while !document.advance(cancelled)? {}
        Ok(document)
    }

    /// Opens the source and reads its first framing unit.
    ///
    /// # Errors
    /// The source could not be identified, opened or read.
    pub fn open_progressive(
        source: &WirePath,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self, Refusal> {
        Self::open_impl(source, cancelled, true)
    }

    fn open_impl(
        source: &WirePath,
        cancelled: &dyn Fn() -> bool,
        cached: bool,
    ) -> Result<Self, Refusal> {
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
        // A cloned handle shares the seek position on supported platforms.
        let mut reader = File::open(&path).map_err(|e| io("Cannot read the file", e))?;
        let scan_id =
            FileId::of_file(&reader, &path).map_err(|e| io("Cannot identify the file", e))?;
        if id != scan_id {
            return Err(Refusal::new(
                AnalysisFailure::SourceChanged,
                "The source changed during open.",
            ));
        }
        if cancelled() {
            return Err(Refusal::new(
                AnalysisFailure::Cancelled,
                "The open was cancelled.",
            ));
        }
        let header =
            probe_ivf_header(&mut reader).map_err(|e| io("Cannot inspect the file header", e))?;
        let mut index = Index::new()?;
        if let Some(header) = &header {
            index.push(Item::IvfHeader {
                len: u64::from(header.header_len),
            })?;
        }
        let buffer_bytes = header.map_or(8192, |header| usize::from(header.header_len).max(8192));
        let mut input = BufReader::with_capacity(buffer_bytes, HashReader::new(reader));
        // Seed the digest for the IVF header even before the first record.
        if header.is_some() {
            use std::io::BufRead;
            input
                .fill_buf()
                .map_err(|e| io("Cannot read the file", e))?;
        }
        let mut scan = Scan::new(input);
        scan.builder.framing = header.map(|_| Framing::Ivf);
        scan.header = header;
        let outcome = match header {
            Some(header) if header.fourcc != *b"AV02" => scan
                .builder
                .finish(
                    Some((
                        ProblemKind::Unsupported,
                        format!(
                            "The IVF codec is {:?}. Open an AV02 stream.",
                            String::from_utf8_lossy(&header.fourcc)
                        ),
                    )),
                    fingerprint.len,
                    &mut index,
                    None,
                )
                .map(Some)?,
            Some(_) => None,
            None => scan.step(fingerprint.len, &mut index, cancelled)?,
        };
        let summary = scan.summary(fingerprint, &index, outcome.as_ref());
        let cache = cached
            .then(|| CacheSession::new(&path, &id, &summary))
            .flatten();
        let document = Self {
            summary,
            path,
            file,
            id,
            index,
            scan: outcome.is_none().then_some(scan),
            evidence: Evidence::default(),
            cache,
        };
        document.check()?;
        if document.scan.is_none() {
            document.publish_cache();
        }
        Ok(document)
    }

    /// Reads another batch of framing units. Returns true when the scan ends.
    ///
    /// # Errors
    /// The source changed or the scan could not continue.
    pub fn advance(&mut self, cancelled: &dyn Fn() -> bool) -> Result<bool, Refusal> {
        self.check()?;
        if self.scan.is_none() {
            return Ok(true);
        }
        // Cache verification gets one bounded step per scan batch. Cold
        // discovery continues while a candidate is verified.
        if let Some(cache) = &mut self.cache
            && let Progress::Warm(index, summary) = cache.advance(&self.summary, cancelled)?
        {
            self.check()?;
            self.index = index;
            self.summary = summary;
            self.scan = None;
            return Ok(true);
        }
        let Some(scan) = &mut self.scan else {
            return Ok(true);
        };
        let fingerprint = self.summary.fingerprint;
        let start = scan.units.get_ref().get_ref().bytes;
        let rows = self.index.len();
        let mut outcome = None;
        for _ in 0..256 {
            outcome = scan.step(fingerprint.len, &mut self.index, cancelled)?;
            if outcome.is_some()
                || self.index.len() - rows >= 256
                || scan.units.get_ref().get_ref().bytes - start >= 4 << 20
            {
                break;
            }
        }
        self.summary = scan.summary(fingerprint, &self.index, outcome.as_ref());
        if outcome.is_some() {
            self.scan = None;
        }
        self.check()?;
        if self.scan.is_none() {
            self.publish_cache();
        }
        Ok(self.scan.is_none())
    }

    fn publish_cache(&self) {
        if let Some(cache) = &self.cache {
            cache.publish(&self.summary, &self.index);
        }
    }

    pub(super) const fn identity(&self) -> &FileId {
        &self.id
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
        self.index.page(filter, first, count)
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
        self.index.locate(filter, target)
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

fn probe_ivf_header(reader: &mut File) -> io::Result<Option<IvfHeader>> {
    let mut prefix = [0; 4];
    let ivf = match reader.read_exact(&mut prefix) {
        Ok(()) => prefix == IVF_SIGNATURE,
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => false,
        Err(error) => return Err(error),
    };
    reader.seek(SeekFrom::Start(0))?;
    if !ivf {
        return Ok(None);
    }
    // Insert the file header before the first record fills an index page.
    let mut wire = Vec::new();
    (&mut *reader)
        .take(u64::from(u16::MAX))
        .read_to_end(&mut wire)?;
    reader.seek(SeekFrom::Start(0))?;
    Ok(parse_ivf_header(&wire).ok())
}

/// Fails reads once `cancelled` says so.
pub(crate) struct Checked<'a, R> {
    inner: R,
    cancelled: &'a dyn Fn() -> bool,
}

impl<'a, R> Checked<'a, R> {
    pub(crate) fn new(inner: R, cancelled: &'a dyn Fn() -> bool) -> Self {
        Self { inner, cancelled }
    }
}

impl<R: Read> Read for Checked<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if (self.cancelled)() {
            return Err(io::Error::other("cancelled"));
        }
        let max = buf.len().min(1 << 20);
        self.inner.read(&mut buf[..max])
    }
}

/// Hashes bytes from the file handle, including any bytes read ahead by the buffer.
#[derive(Debug)]
pub(crate) struct HashReader<R> {
    inner: R,
    hash: Sha256,
    bytes: u64,
}

impl<R> HashReader<R> {
    pub(crate) fn new(inner: R) -> Self {
        Self {
            inner,
            hash: Sha256::new(),
            bytes: 0,
        }
    }

    fn snapshot(&self) -> SourceDigest {
        SourceDigest {
            sha256: crate::artifact::hex(&self.hash.clone().finalize()),
            bytes: self.bytes,
        }
    }

    pub(crate) fn finish(self) -> SourceDigest {
        SourceDigest {
            sha256: crate::artifact::hex(&self.hash.finalize()),
            bytes: self.bytes,
        }
    }
}

impl<R: Read> Read for HashReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(buffer)?;
        self.hash.update(&buffer[..count]);
        self.bytes += count as u64;
        Ok(count)
    }
}

/// One kept element, in the compact form it is stored in.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// The facts gathered so far.
#[derive(Debug, Default)]
struct Builder {
    framing: Option<Framing>,
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
    RecordDone,
    End,
    Stop(ProblemKind, String),
}

fn problem(kind: ProblemKind, message: impl Into<String>) -> Step {
    Step::Stop(kind, message.into())
}

impl Builder {
    /// One Annex B unit: exactly one OBU, or the bytes that did not parse.
    fn annex_b(&mut self, index: &mut Index, offset: ByteOffset, bytes: &[u8]) -> Step {
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
                self.obu(index, at, &obu, None, at)
                    .err()
                    .unwrap_or(Step::Continue)
            }
            // The reader yields no empty unit.
            Ok(None) => Step::Continue,
            Err(e) => problem(
                ProblemKind::Malformed,
                format!("the OBU does not parse: {e}"),
            ),
        }
    }

    /// Insert one record. Parse the payload in later steps.
    fn ivf_frame(
        &mut self,
        facts: &mut Index,
        index: usize,
        pts: u64,
        payload_offset: ByteOffset,
        len: usize,
    ) -> Result<PendingRecord, Step> {
        let start = payload_offset.get();
        let len = len as u64;
        let stream = self.stream;
        self.stream = stream.saturating_add(len);
        if self.framing.is_none() {
            self.framing = Some(Framing::Ivf);
            push(
                facts,
                Item::IvfHeader {
                    len: start - IVF_FRAME_HEADER_SIZE as u64,
                },
            )?;
        }
        push(
            facts,
            Item::Packet {
                index: index as u64,
                pts,
                payload_offset: start,
                len,
                stream_offset: stream,
            },
        )?;
        Ok(PendingRecord {
            index: index as u64,
            start,
            stream,
            consumed: 0,
        })
    }

    fn ivf_payload(
        &mut self,
        facts: &mut Index,
        record: &mut PendingRecord,
        payload: &[u8],
        cancelled: &dyn Fn() -> bool,
    ) -> Step {
        let mut at = record.start + record.consumed as u64;
        let mut obus = AnnexBObuCursor::new(&payload[record.consumed..], ByteOffset::new(at));
        for _ in 0..256 {
            if cancelled() {
                return cancel();
            }
            let stream_offset = record.stream + (at - record.start);
            match obus.next_obu() {
                Ok(Some(obu)) => {
                    if let Err(stop) = self.obu(facts, at, &obu, Some(record.index), stream_offset)
                    {
                        return stop;
                    }
                    at = obu.offset.get().saturating_add(u64::from(obu.size));
                    let Ok(consumed) = usize::try_from(at - record.start) else {
                        return problem(
                            ProblemKind::Io,
                            "The OBU position is outside the IVF payload.",
                        );
                    };
                    record.consumed = consumed;
                }
                Ok(None) => return Step::RecordDone,
                Err(e) => {
                    let mut message = clip(format!(
                        "IVF record {}: the OBU does not parse: {e}; the rest of its payload is not read",
                        record.index
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
                    return push(
                        facts,
                        Item::Unparsed {
                            offset: at,
                            len: payload.len() as u64 - (at - record.start),
                            stream_offset: Some(stream_offset),
                            kind: ProblemKind::Malformed,
                            message,
                        },
                    )
                    .err()
                    .unwrap_or(Step::RecordDone);
                }
            }
        }
        Step::Continue
    }

    fn obu(
        &mut self,
        facts: &mut Index,
        prefix: u64,
        obu: &splot_core::annexb::ObuEnvelope<'_>,
        packet: Option<u64>,
        stream_offset: u64,
    ) -> Result<(), Step> {
        push(
            facts,
            Item::Obu {
                index: self.obus,
                packet,
                prefix,
                offset: obu.offset.get(),
                size: obu.size,
                header: obu.header,
                stream_offset,
            },
        )?;
        self.obus += 1;
        Ok(())
    }

    /// Ends reading: `stop` makes the rest of the file one region.
    fn finish(
        &mut self,
        stop: Option<(ProblemKind, String)>,
        len: u64,
        index: &mut Index,
        partial_record_end: Option<u64>,
    ) -> Result<ScanOutcome, Refusal> {
        if self.framing.is_none() {
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
        }
        let end = partial_record_end.unwrap_or_else(|| index.end());
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
        Ok(match stop {
            Some((kind, message)) => {
                let message = clip(message);
                // The pager keeps space for the terminal region.
                if len > end {
                    let item = Item::Unparsed {
                        offset: end,
                        len: len - end,
                        stream_offset: None,
                        kind,
                        message: message.clone(),
                    };
                    index.push_tail(item);
                }
                ScanOutcome::Stopped {
                    problem: Problem {
                        kind,
                        offset: end,
                        message,
                    },
                }
            }
            None => match self.damaged.take() {
                Some(first) => ScanOutcome::Damaged { first },
                None => ScanOutcome::Complete,
            },
        })
    }
}

fn cancel() -> Step {
    problem(
        ProblemKind::Cancelled,
        "The scan stopped at the last indexed element.",
    )
}

#[derive(Debug)]
struct PendingRecord {
    index: u64,
    start: u64,
    stream: u64,
    consumed: usize,
}

fn push(index: &mut Index, item: Item) -> Result<(), Step> {
    index.push(item).map_err(|error| {
        let kind = match error.code {
            AnalysisFailure::LimitReached => ProblemKind::LimitReached,
            _ => ProblemKind::Io,
        };
        problem(kind, error.message)
    })
}

#[derive(Debug)]
struct Scan<R: Read> {
    units: TemporalUnitReader<R>,
    builder: Builder,
    header: Option<IvfHeader>,
    pending: Option<PendingRecord>,
}

impl<R: Read> Scan<R> {
    fn new(input: R) -> Self {
        Self {
            units: TemporalUnitReader::with_max_unit_bytes(input, MAX_UNIT_BYTES),
            builder: Builder::default(),
            header: None,
            pending: None,
        }
    }

    fn step(
        &mut self,
        len: u64,
        facts: &mut Index,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Option<ScanOutcome>, Refusal> {
        let units = &mut self.units;
        let doc = &mut self.builder;
        let step = if cancelled() {
            cancel()
        } else if let Some(record) = &mut self.pending {
            let payload = units.last_ivf_payload().ok_or_else(|| {
                Refusal::new(
                    AnalysisFailure::Io,
                    "The IVF record payload is unavailable.",
                )
            })?;
            doc.ivf_payload(facts, record, payload, cancelled)
        } else {
            match units.next_unit() {
                Ok(None) => Step::End,
                Ok(Some(StreamUnit::AnnexBObu { offset, bytes })) => {
                    doc.annex_b(facts, offset, bytes)
                }
                Ok(Some(StreamUnit::IvfFrame {
                    index,
                    pts,
                    payload_offset,
                    payload,
                })) => match doc.ivf_frame(facts, index, pts, payload_offset, payload.len()) {
                    Ok(record) => {
                        self.pending = Some(record);
                        Step::Continue
                    }
                    Err(stop) => stop,
                },
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
        if self.header.is_none()
            && let Some(found) = units.ivf_header()
        {
            self.header = Some(found);
            doc.framing = Some(Framing::Ivf);
            let header = Item::IvfHeader {
                len: u64::from(found.header_len),
            };
            if found.fourcc != *b"AV02" {
                *facts = Index::new()?;
                facts.push(header)?;
                return doc
                    .finish(
                        Some((
                            ProblemKind::Unsupported,
                            format!(
                                "The IVF codec is {:?}. Open an AV02 stream.",
                                String::from_utf8_lossy(&found.fourcc)
                            ),
                        )),
                        len,
                        facts,
                        None,
                    )
                    .map(Some);
            }
            if facts.len() == 0 {
                facts.push(header)?;
            }
        }
        match step {
            Step::Stop(kind, message) => {
                let partial_end = self
                    .pending
                    .as_ref()
                    .map(|record| record.start + record.consumed as u64);
                doc.finish(Some((kind, message)), len, facts, partial_end)
                    .map(Some)
            }
            Step::End => doc.finish(None, len, facts, None).map(Some),
            Step::Continue if doc.framing.is_none() => doc.finish(None, len, facts, None).map(Some),
            Step::RecordDone => {
                self.pending = None;
                Ok(None)
            }
            Step::Continue => Ok(None),
        }
    }
}

impl Scan<BufReader<HashReader<File>>> {
    fn summary(
        &self,
        fingerprint: SourceFingerprint,
        index: &Index,
        outcome: Option<&ScanOutcome>,
    ) -> DocumentSummary {
        index.facts().summary(
            fingerprint,
            self.builder.framing.unwrap_or(Framing::AnnexB),
            self.header,
            outcome.cloned().unwrap_or(ScanOutcome::Scanning),
            (self.units.get_ref().get_ref().bytes >= index.end() || outcome.is_some())
                .then(|| self.units.get_ref().get_ref().snapshot()),
        )
    }
}

#[cfg(test)]
use tests::scan;

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
