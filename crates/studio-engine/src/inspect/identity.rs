// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Bounded, process-owned tables for native decoder identities.

mod timeline;

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use splot_core::annexb::AnnexBObuCursor;
use splot_core::stream_reader::{TemporalUnit, TemporalUnitReader, is_random_access_point};
use splot_core::types::ObuType;
use splot_decode::{
    BaseQuantizer, BaseQuantizerOrigin, CodedFrameEvent, DataProps, DecodeLimits, Decoder,
    ObuLocation, Picture, SendStatus,
};
use studio_domain::analysis::{AnalysisFailure, SourceDigest};
use studio_domain::identity::{
    DecodeLayers, IDENTITY_SCHEMA, IdentityCompletion, IdentityKind, IdentityPage, IdentityRow,
    IdentitySummary, MAX_IDENTITY_PAGE,
};
use studio_domain::job::SourceFingerprint;
use studio_domain::preview::GrainPolicy;
use studio_domain::timeline::TimelineQuantizer;

use super::document::{Checked, HashReader, Refusal};
use crate::stream::{MAX_UNIT_BYTES, PREVIEW_DECODE_THREADS, preview_decoder_settings};

const RECORD_BYTES: usize = 32;
const MAX_INDEX_BYTES: u64 = 128 << 20;
type Record = [u64; 4];

use timeline::TimelineIndex;

#[derive(Debug)]
struct Table<const WORDS: usize = 4> {
    file: File,
    count: u64,
}

impl<const WORDS: usize> Table<WORDS> {
    fn new() -> io::Result<Self> {
        Ok(Self {
            file: tempfile::tempfile()?,
            count: 0,
        })
    }

    const BYTES: usize = WORDS * 8;

    fn append(&mut self, record: [u64; WORDS]) -> io::Result<()> {
        self.write(self.count, record)?;
        self.count += 1;
        Ok(())
    }

    fn write(&mut self, row: u64, record: [u64; WORDS]) -> io::Result<()> {
        let mut bytes = [0; 64];
        for (chunk, value) in bytes.as_chunks_mut::<8>().0.iter_mut().zip(record) {
            *chunk = value.to_le_bytes();
        }
        self.seek(row)?;
        self.file.write_all(&bytes[..Self::BYTES])
    }

    fn seek(&mut self, row: u64) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(row * Self::BYTES as u64))?;
        Ok(())
    }

    fn read(&mut self) -> io::Result<[u64; WORDS]> {
        let mut bytes = [0; 64];
        self.file.read_exact(&mut bytes[..Self::BYTES])?;
        Ok(Self::decode(&bytes))
    }

    fn decode(bytes: &[u8]) -> [u64; WORDS] {
        let mut words = [0; WORDS];
        for (word, chunk) in words.iter_mut().zip(bytes.as_chunks::<8>().0) {
            *word = u64::from_le_bytes(*chunk);
        }
        words
    }

    fn read_window(
        &mut self,
        first: u64,
        end: u64,
        mut consume: impl FnMut(u64, [u64; WORDS]),
    ) -> io::Result<()> {
        let mut block = [0; 2048];
        let rows = u64::try_from(block.len() / Self::BYTES).map_err(io::Error::other)?;
        self.seek(first)?;
        let mut at = first;
        while at < end {
            let count = rows.min(end - at);
            let bytes = usize::try_from(count).map_err(io::Error::other)? * Self::BYTES;
            self.file.read_exact(&mut block[..bytes])?;
            for offset in (0..bytes).step_by(Self::BYTES) {
                consume(at, Self::decode(&block[offset..offset + Self::BYTES]));
                at += 1;
            }
        }
        Ok(())
    }

    fn at(&mut self, row: u64) -> io::Result<[u64; WORDS]> {
        self.seek(row)?;
        self.read()
    }

    fn lower_bound(&mut self, mut before: impl FnMut([u64; WORDS]) -> bool) -> io::Result<u64> {
        let (mut lo, mut hi) = (0, self.count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if before(self.at(mid)?) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(lo)
    }
}

#[derive(Debug)]
struct State {
    temporal: Table,
    coded: Table,
    continuation: Table,
    output: Table,
    timeline: TimelineIndex,
    fingerprint: SourceFingerprint,
    grain: GrainPolicy,
    digest: Option<SourceDigest>,
    matches_structure: bool,
    changed: bool,
    completion: IdentityCompletion,
    write_error: Option<String>,
}

impl State {
    fn table(&mut self, kind: IdentityKind) -> &mut Table {
        match kind {
            IdentityKind::Temporal => &mut self.temporal,
            IdentityKind::Coded => &mut self.coded,
            IdentityKind::Continuation => &mut self.continuation,
            IdentityKind::Output => &mut self.output,
        }
    }

    fn summary(&self) -> IdentitySummary {
        IdentitySummary {
            schema: IDENTITY_SCHEMA,
            codec_revision: env!("SPLOT_CODEC_REVISION").into(),
            spec_revision: "AV2 v1.0.0".into(),
            fingerprint: self.fingerprint,
            grain: self.grain,
            digest: self.digest.clone(),
            matches_structure: self.matches_structure,
            completion: self.completion.clone(),
            temporal: self.temporal.count,
            coded: self.coded.count,
            continuations: self.continuation.count,
            outputs: self.output.count,
        }
    }

    fn timeline_result(&mut self, result: io::Result<()>) {
        if let Err(error) = result {
            self.write_error = Some(format!("Cannot index the frame timeline. {error}"));
        }
    }

    fn append(&mut self, kind: IdentityKind, record: Record) {
        if self.write_error.is_some() {
            return;
        }
        let records =
            self.temporal.count + self.coded.count + self.continuation.count + self.output.count;
        if records >= MAX_INDEX_BYTES / RECORD_BYTES as u64 {
            self.write_error = Some("The identity index reached its disk limit.".into());
            return;
        }
        if let Err(error) = self.table(kind).append(record) {
            self.write_error = Some(format!("Cannot write an identity record. {error}"));
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct IdentityIndex(Arc<Mutex<State>>);

impl IdentityIndex {
    pub(super) fn new(grain: GrainPolicy, fingerprint: SourceFingerprint) -> Result<Self, Refusal> {
        Ok(Self(Arc::new(Mutex::new(State {
            temporal: Table::new().map_err(|error| storage(&error))?,
            coded: Table::new().map_err(|error| storage(&error))?,
            continuation: Table::new().map_err(|error| storage(&error))?,
            output: Table::new().map_err(|error| storage(&error))?,
            timeline: TimelineIndex::new().map_err(|error| storage(&error))?,
            fingerprint,
            grain,
            digest: None,
            matches_structure: false,
            changed: false,
            completion: IdentityCompletion::Running,
            write_error: None,
        }))))
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn summary(&self) -> IdentitySummary {
        self.state().summary()
    }

    pub(super) fn page(
        &self,
        kind: IdentityKind,
        first: u64,
        count: u32,
    ) -> Result<IdentityPage, Refusal> {
        if !(1..=MAX_IDENTITY_PAGE).contains(&count) {
            return Err(invalid("The identity page size is out of range."));
        }
        let mut state = self.state();
        if state.changed {
            return Err(source_changed());
        }
        let summary = state.summary();
        let table = state.table(kind);
        if first > table.count {
            return Err(invalid(
                "The first row is past the end of the identity table.",
            ));
        }
        let count = usize::try_from((table.count - first).min(u64::from(count)))
            .map_err(|_| invalid("The identity page size is out of range."))?;
        let mut rows = Vec::with_capacity(count);
        table.seek(first).map_err(|error| storage(&error))?;
        for _ in 0..count {
            let [a, b, c, d] = table.read().map_err(|error| storage(&error))?;
            rows.push(match kind {
                IdentityKind::Temporal => IdentityRow::Temporal {
                    delimiter_offset: a,
                },
                IdentityKind::Coded => IdentityRow::Coded {
                    coded_offset: a,
                    sequence_offset: b,
                    layers: unpack_layers(c),
                },
                IdentityKind::Continuation => IdentityRow::Continuation {
                    coded_offset: a,
                    obu_offset: b,
                },
                IdentityKind::Output => IdentityRow::Output {
                    coded_offset: a,
                    showing_offset: b,
                    sequence_offset: c,
                    layers: unpack_layers(d),
                },
            });
        }
        Ok(IdentityPage {
            kind,
            first,
            summary,
            rows,
        })
    }

    pub(super) fn scan(
        &self,
        file: &mut File,
        expected: Option<&SourceDigest>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), Refusal> {
        let (grain, len) = {
            let state = self.state();
            (state.grain, state.fingerprint.len)
        };
        let mut hashed = HashReader::new(Checked::new(file.take(len), cancelled));
        let result = self.decode(BufReader::new(&mut hashed), grain, cancelled);
        let digest = hashed.finish();
        let mut state = self.state();
        if let Some(expected) = expected.filter(|expected| expected.bytes == digest.bytes) {
            if *expected != digest {
                state.changed = true;
                return Err(source_changed());
            }
            state.matches_structure = digest.bytes == len;
        }
        state.digest = Some(digest);
        state.completion = if let Some(message) = state.write_error.take() {
            IdentityCompletion::Stopped { message }
        } else if cancelled() {
            IdentityCompletion::Cancelled
        } else {
            match result {
                Ok(())
                    if state
                        .digest
                        .as_ref()
                        .is_some_and(|digest| digest.bytes == len) =>
                {
                    IdentityCompletion::Complete
                }
                Ok(()) => {
                    state.changed = true;
                    return Err(source_changed());
                }
                Err(message) => IdentityCompletion::Stopped { message },
            }
        };
        Ok(())
    }

    fn decode(
        &self,
        reader: impl Read,
        grain: GrainPolicy,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        let observed = self.clone();
        let settings =
            preview_decoder_settings(PREVIEW_DECODE_THREADS, DecodeLimits::DEFAULT, grain);
        let mut decoder = Decoder::new_with_coded_frame_observer(settings, move |event| {
            observed.observe(event);
        })
        .map_err(|error| format!("Cannot start the identity decoder. {error}"))?;
        let mut reader = TemporalUnitReader::with_max_unit_bytes(reader, MAX_UNIT_BYTES);
        while !self.stopped(cancelled) {
            let Some(unit) = reader
                .next_temporal_unit()
                .map_err(|error| error.to_string())?
            else {
                if let Some(warning) = reader.ivf_warning() {
                    return Err(format!(
                        "The IVF file ends inside a record. ({})",
                        warning.rule_id()
                    ));
                }
                if let Some(header) = reader.ivf_header() {
                    self.state()
                        .timeline
                        .start_unit(u64::from(header.header_len));
                }
                decoder.drain();
                return self.receive(&mut decoder, cancelled);
            };
            self.index_unit(&unit, cancelled)?;
            if self.stopped(cancelled) {
                return Ok(());
            }
            // The submission token is the file offset of its first byte.
            let props = DataProps::default().with_user_data(unit.offset.get());
            if decoder
                .send_data(unit.bytes, props)
                .map_err(|error| decode_failed(&error))?
                != SendStatus::Accepted
            {
                return Err("The decoder refused an identity scan submission.".into());
            }
            self.receive(&mut decoder, cancelled)?;
        }
        Ok(())
    }

    fn index_unit(
        &self,
        unit: &TemporalUnit<'_>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        let mut cursor = AnnexBObuCursor::new(unit.bytes, unit.offset);
        let ambiguous = unit.pts.is_some() && multiple_temporal_units(unit, cancelled);
        let pts = unit.pts.filter(|_| !ambiguous);
        let mut prefix = 0;
        let mut temporal_prefix = None;
        self.state().timeline.start_unit(unit.offset.get());
        while !self.stopped(cancelled) {
            let Some(obu) = cursor.next_obu().map_err(|error| error.to_string())? else {
                break;
            };
            let offset = obu.offset.get();
            let kind = obu.header.obu_type;
            let bytes = u64::from(obu.size);
            let end = usize::try_from(offset - unit.offset.get() + bytes)
                .map_err(|_| "The OBU exceeds the supported input size.")?;
            let delimiter = kind == ObuType::TemporalDelimiter;
            if delimiter {
                temporal_prefix = Some(prefix);
            }
            // Probe once per temporal unit. The prefix ends at this frame OBU,
            // so a following delimiter cannot turn this unit into a candidate.
            let marked = matches!(
                kind,
                ObuType::ClosedLoopKey | ObuType::OpenLoopKey | ObuType::RasFrame
            ) && temporal_prefix
                .take()
                .is_some_and(|start| is_random_access_point(&unit.bytes[start..end]));
            let mut state = self.state();
            if delimiter {
                state.append(IdentityKind::Temporal, [offset, 0, 0, 0]);
            }
            let result = state.timeline.obu(offset, bytes, pts, ambiguous, marked);
            state.timeline_result(result);
            prefix = end;
        }
        Ok(())
    }

    fn receive(&self, decoder: &mut Decoder, cancelled: &dyn Fn() -> bool) -> Result<(), String> {
        while !self.stopped(cancelled) {
            let Some(picture) = decoder
                .wait_picture()
                .map_err(|error| decode_failed(&error))?
            else {
                break;
            };
            let location = picture.provenance();
            let coded = file_offset(location.coded);
            let showing = file_offset(location.display);
            let mut state = self.state();
            state.append(
                IdentityKind::Output,
                [
                    coded,
                    showing,
                    file_offset(location.sequence_header),
                    pack_layers(
                        picture.extended_layer_id().get(),
                        picture.embedded_layer_id().get(),
                        picture.temporal_layer_id().get(),
                    ),
                ],
            );
            if state.write_error.is_none() {
                let result = state
                    .timeline
                    .output(coded, showing, picture_interval(&picture));
                state.timeline_result(result);
            }
        }
        Ok(())
    }

    fn stopped(&self, cancelled: &dyn Fn() -> bool) -> bool {
        cancelled() || self.state().write_error.is_some()
    }

    fn observe(&self, event: CodedFrameEvent) {
        let (kind, record, quantizer) = match event {
            CodedFrameEvent::Frame {
                coded,
                sequence_header,
                extended_layer_id,
                embedded_layer_id,
                temporal_layer_id,
                base_quantizer,
                ..
            } => (
                IdentityKind::Coded,
                [
                    file_offset(coded),
                    file_offset(sequence_header),
                    pack_layers(
                        extended_layer_id.get(),
                        embedded_layer_id.get(),
                        temporal_layer_id.get(),
                    ),
                    0,
                ],
                timeline_quantizer(base_quantizer),
            ),
            CodedFrameEvent::Continuation { coded, obu, .. } => (
                IdentityKind::Continuation,
                [file_offset(coded), file_offset(obu), 0, 0],
                TimelineQuantizer::Unavailable,
            ),
            _ => return,
        };
        let mut state = self.state();
        state.append(kind, record);
        if state.write_error.is_none() {
            let result = match kind {
                IdentityKind::Coded => state.timeline.frame(record[0], quantizer),
                IdentityKind::Continuation => state.timeline.continuation(record[0], record[1]),
                _ => return,
            };
            state.timeline_result(result);
        }
    }
}

fn timeline_quantizer(native: Option<BaseQuantizer>) -> TimelineQuantizer {
    let Some(BaseQuantizer { index, origin, .. }) = native else {
        return TimelineQuantizer::Unavailable;
    };
    match origin {
        BaseQuantizerOrigin::Signaled => TimelineQuantizer::HeaderBase { value: index },
        BaseQuantizerOrigin::Derived => TimelineQuantizer::DerivedBase { value: index },
        BaseQuantizerOrigin::Inherited => TimelineQuantizer::InheritedBase { value: index },
        _ => TimelineQuantizer::Unavailable,
    }
}

fn multiple_temporal_units(unit: &TemporalUnit<'_>, cancelled: &dyn Fn() -> bool) -> bool {
    let mut cursor = AnnexBObuCursor::new(unit.bytes, unit.offset);
    let mut delimiter = false;
    while !cancelled() {
        let Ok(Some(obu)) = cursor.next_obu() else {
            break;
        };
        if obu.header.obu_type == ObuType::TemporalDelimiter
            && std::mem::replace(&mut delimiter, true)
        {
            return true;
        }
    }
    false
}

fn picture_interval(picture: &Picture) -> Option<(u64, u64)> {
    let info = picture.content_interpretation()?.timing_info?;
    if !info.equal_picture_interval || info.time_scale == 0 {
        return None;
    }
    let ticks = u64::from(info.num_ticks_per_picture_minus_1?) + 1;
    let num = u64::from(info.num_units_in_display_tick).checked_mul(ticks)?;
    (num != 0).then_some((num, u64::from(info.time_scale)))
}

fn file_offset(location: ObuLocation) -> u64 {
    // The reader bounds the end of each submission to the source length.
    location.user_data + location.offset.get()
}

fn pack_layers(extended: u8, embedded: u8, temporal: u8) -> u64 {
    u64::from_le_bytes([extended, embedded, temporal, 0, 0, 0, 0, 0])
}

fn unpack_layers(value: u64) -> DecodeLayers {
    let [extended, embedded, temporal, ..] = value.to_le_bytes();
    DecodeLayers {
        extended,
        embedded,
        temporal,
    }
}

fn decode_failed(error: &splot_decode::DecodeError) -> String {
    format!("Cannot decode the source for the identity index. {error}")
}

fn invalid(message: &str) -> Refusal {
    Refusal::new(AnalysisFailure::InvalidRequest, message)
}

fn storage(error: &io::Error) -> Refusal {
    Refusal::new(
        AnalysisFailure::Io,
        format!("Cannot access the identity index. {error}"),
    )
}

fn source_changed() -> Refusal {
    Refusal::new(
        AnalysisFailure::SourceChanged,
        "The source bytes changed during the identity scan.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use studio_domain::analysis::{Element, LocateTarget, RowFilter};
    use studio_domain::job::WirePath;

    use crate::inspect::document::Document;

    const BRIDGE_IVF: &[u8] =
        include_bytes!("../../tests/inspect/fixtures/syn-bridge-celu-64x64.ivf");
    const LAYERED_10_BIT: &[u8] =
        include_bytes!("../../tests/preview/fixtures/syn-tlayer2-dependent-10bit.ivf");

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn refusal(error: Refusal) -> io::Error {
        io::Error::other(error.message)
    }

    #[test]
    fn distinct_temporal_units_and_non_output_frame_within_one_ivf_packet() -> TestResult {
        let mut source = tempfile::NamedTempFile::new()?;
        source.write_all(BRIDGE_IVF)?;
        let source = source.into_temp_path();
        let document = Document::open(&WirePath::from_path(&source), &|| false).map_err(refusal)?;
        let summary = document.summary();
        assert_eq!(summary.packets, 1);
        let index = IdentityIndex::new(GrainPolicy::Skip, summary.fingerprint).map_err(refusal)?;
        assert_eq!(
            index
                .page(IdentityKind::Temporal, 0, 1)
                .map_err(refusal)?
                .summary
                .completion,
            IdentityCompletion::Running
        );
        index
            .scan(
                &mut File::open(&source)?,
                summary.digest.as_deref(),
                &|| false,
            )
            .map_err(refusal)?;
        let summary = index.summary();
        assert!(summary.matches_structure);
        assert_eq!(summary.completion, IdentityCompletion::Complete);
        assert_eq!(
            (summary.temporal, summary.coded, summary.outputs),
            (2, 3, 2)
        );

        let temporal = index.page(IdentityKind::Temporal, 0, 2).map_err(refusal)?;
        for (row, expected) in temporal.rows.iter().zip([45, 83]) {
            assert_eq!(
                *row,
                IdentityRow::Temporal {
                    delimiter_offset: expected
                }
            );
            let (_, structural) = document
                .locate(
                    RowFilter::default(),
                    LocateTarget::Offset { offset: expected },
                )
                .map_err(refusal)?;
            assert_eq!(structural.parent, Some(1));
            assert!(
                matches!(structural.element, Element::Obu { obu } if obu.obu.offset == expected)
            );
        }

        let coded = index.page(IdentityKind::Coded, 0, 3).map_err(refusal)?;
        let outputs = index.page(IdentityKind::Output, 0, 2).map_err(refusal)?;
        for row in &outputs.rows {
            let IdentityRow::Output { coded_offset, .. } = row else {
                return Err("The output page contained a different kind of row.".into());
            };
            assert!(coded.rows.iter().any(|row| matches!(row, IdentityRow::Coded { coded_offset: offset, .. } if offset == coded_offset)));
        }
        assert!(coded.rows.iter().any(|row| {
            matches!(row, IdentityRow::Coded { coded_offset, .. }
                if !outputs.rows.iter().any(|output| matches!(output, IdentityRow::Output { coded_offset: offset, .. } if offset == coded_offset)))
        }));
        Ok(())
    }

    #[test]
    fn timeline_reconciles_bridge_bytes_and_rejects_ambiguous_clock() -> TestResult {
        use studio_domain::timeline::{
            ByteDistribution, TimelineClock, TimelineFileBytes, TimelineTarget, TimelineTime,
            TimelineView,
        };

        let mut source = tempfile::NamedTempFile::new()?;
        source.write_all(BRIDGE_IVF)?;
        let document =
            Document::open(&WirePath::from_path(source.path()), &|| false).map_err(refusal)?;
        let summary = document.summary();
        let index = IdentityIndex::new(GrainPolicy::Skip, summary.fingerprint).map_err(refusal)?;
        index
            .scan(&mut source.reopen()?, summary.digest.as_deref(), &|| false)
            .map_err(refusal)?;
        let header = summary.ivf_header;
        let coded = index
            .timeline(header, TimelineView::Coded, 0, u64::MAX, 3)
            .map_err(refusal)?;
        assert_eq!(
            coded.file_bytes,
            TimelineFileBytes {
                file: 101,
                obu: 51,
                assigned_obu: 26,
                unassigned_obu: 25,
                framing: 50,
                unclassified: 0
            }
        );
        assert_eq!(
            coded.bytes,
            Some(ByteDistribution {
                min: 4,
                max: 11,
                sum: 26
            })
        );
        assert_eq!(coded.clock, TimelineClock::Conflicting);
        assert_eq!(
            coded
                .buckets
                .iter()
                .map(|bucket| bucket.entry.map(|entry| entry.quantizer))
                .collect::<Vec<_>>(),
            [
                Some(TimelineQuantizer::HeaderBase { value: 21 }),
                Some(TimelineQuantizer::InheritedBase { value: 21 }),
                Some(TimelineQuantizer::HeaderBase { value: 57 }),
            ]
        );
        assert!(
            coded.buckets[0]
                .entry
                .ok_or("Missing first row.")?
                .random_access
        );
        assert_eq!(
            coded.buckets[1]
                .entry
                .ok_or("Missing bridge row.")?
                .output_ordinal,
            None
        );
        let output = index
            .timeline(header, TimelineView::Output, 0, u64::MAX, 1)
            .map_err(refusal)?;
        assert_eq!(
            output.bytes,
            Some(ByteDistribution {
                min: 11,
                max: 11,
                sum: 22
            })
        );
        assert_eq!(output.buckets[0].count, 2);
        assert_eq!(output.rate, None);
        let selected = index
            .timeline_locate(header, TimelineTarget::Output { index: 1 })
            .map_err(refusal)?;
        assert_eq!(
            (selected.coded_ordinal, selected.coded_offset, selected.time),
            (2, 90, None)
        );
        assert!(
            index
                .timeline_locate(
                    header,
                    TimelineTarget::Time {
                        time: TimelineTime { num: 0, den: 1 }
                    }
                )
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn annex_b_timeline_links_show_existing_without_inventing_time() -> TestResult {
        use studio_domain::timeline::{TimelineClock, TimelineTarget, TimelineView};

        let mut source = tempfile::NamedTempFile::new()?;
        source.write_all(include_bytes!(
            "../../tests/preview/fixtures/syn-mlayer2-derived-sef-8bit.obu"
        ))?;
        let document =
            Document::open(&WirePath::from_path(source.path()), &|| false).map_err(refusal)?;
        let summary = document.summary();
        let index = IdentityIndex::new(GrainPolicy::Skip, summary.fingerprint).map_err(refusal)?;
        index
            .scan(&mut source.reopen()?, summary.digest.as_deref(), &|| false)
            .map_err(refusal)?;
        let page = index
            .timeline(None, TimelineView::Output, 0, u64::MAX, 2)
            .map_err(refusal)?;
        assert!(page.complete);
        assert_eq!(page.total, 4);
        assert_eq!(page.clock, TimelineClock::Unavailable);
        assert_eq!(page.rate, None);
        assert_eq!(
            page.file_bytes.obu + page.file_bytes.framing,
            page.file_bytes.file
        );
        let selection = index
            .timeline_locate(None, TimelineTarget::Output { index: 3 })
            .map_err(refusal)?;
        assert_eq!(
            (
                selection.coded_offset,
                selection.showing_offset,
                selection.time
            ),
            (787, Some(1328), None)
        );
        Ok(())
    }

    #[test]
    fn ten_bit_output_paging_and_source_verification() -> TestResult {
        use studio_domain::timeline::{TimelineClock, TimelineTarget, TimelineTime, TimelineView};

        let mut source = tempfile::NamedTempFile::new()?;
        source.write_all(LAYERED_10_BIT)?;
        let source = source.into_temp_path();
        let document = Document::open(&WirePath::from_path(&source), &|| false).map_err(refusal)?;
        let summary = document.summary();
        let index = IdentityIndex::new(GrainPolicy::Skip, summary.fingerprint).map_err(refusal)?;
        index
            .scan(
                &mut File::open(&source)?,
                summary.digest.as_deref(),
                &|| false,
            )
            .map_err(refusal)?;
        let page = index.page(IdentityKind::Output, 0, 4).map_err(refusal)?;
        assert!(page.summary.matches_structure);
        assert_eq!(page.summary.completion, IdentityCompletion::Complete);
        assert_eq!(page.rows.len(), 4);
        assert_eq!(
            page.rows[2..],
            index
                .page(IdentityKind::Output, 2, 2)
                .map_err(refusal)?
                .rows
        );

        let header = summary.ivf_header;
        let rate = index
            .timeline(header, TimelineView::Output, 0, 2, 1)
            .map_err(refusal)?;
        assert_eq!(rate.clock, TimelineClock::Ivf);
        assert_eq!(
            rate.rate,
            Some(TimelineTime {
                num: 175_440,
                den: 2
            })
        );
        let located = index
            .timeline_locate(
                header,
                TimelineTarget::Time {
                    time: TimelineTime { num: 3, den: 100 },
                },
            )
            .map_err(refusal)?;
        assert_eq!(located.output_ordinal, Some(1));
        assert_eq!(located.time, Some(TimelineTime { num: 1, den: 30 }));

        let mut wrong = summary.digest.clone().ok_or("Missing source digest.")?;
        wrong.sha256 = "wrong".into();
        let index = IdentityIndex::new(GrainPolicy::Skip, summary.fingerprint).map_err(refusal)?;
        assert!(matches!(
            index.scan(&mut File::open(&source)?, Some(&wrong), &|| false),
            Err(Refusal {
                code: AnalysisFailure::SourceChanged,
                ..
            })
        ));
        assert!(matches!(
            index.page(IdentityKind::Output, 0, 1),
            Err(Refusal {
                code: AnalysisFailure::SourceChanged,
                ..
            })
        ));
        Ok(())
    }

    #[test]
    fn an_index_at_its_disk_limit_does_not_append() -> TestResult {
        let source = tempfile::tempfile()?;
        let fingerprint = SourceFingerprint::of(&source.metadata()?);
        let index = IdentityIndex::new(GrainPolicy::Skip, fingerprint).map_err(refusal)?;
        let mut state = index.state();
        state.coded.count = MAX_INDEX_BYTES / RECORD_BYTES as u64 - 1;
        state.append(IdentityKind::Temporal, [1, 0, 0, 0]);
        state.append(IdentityKind::Output, [2, 2, 1, 0]);
        assert_eq!(state.temporal.file.metadata()?.len(), RECORD_BYTES as u64);
        assert_eq!(state.output.file.metadata()?.len(), 0);
        assert!(state.write_error.is_some());
        Ok(())
    }

    #[test]
    fn cancellation_returns_an_unverified_partial_index() -> TestResult {
        let mut source = tempfile::NamedTempFile::new()?;
        source.write_all(BRIDGE_IVF)?;
        let fingerprint = SourceFingerprint::of(&source.as_file().metadata()?);
        let index = IdentityIndex::new(GrainPolicy::Skip, fingerprint).map_err(refusal)?;
        index
            .scan(&mut source.reopen()?, None, &|| true)
            .map_err(refusal)?;
        let page = index.page(IdentityKind::Output, 0, 1).map_err(refusal)?;
        assert_eq!(page.summary.completion, IdentityCompletion::Cancelled);
        assert!(!page.summary.matches_structure);
        assert_eq!(page.rows, [] as [IdentityRow; 0]);
        Ok(())
    }
}
