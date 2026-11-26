// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Bounded, process-owned tables for native decoder identities.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use splot_core::annexb::AnnexBObuCursor;
use splot_core::stream_reader::TemporalUnitReader;
use splot_core::types::ObuType;
use splot_decode::{CodedFrameEvent, DataProps, DecodeLimits, Decoder, ObuLocation, SendStatus};
use studio_domain::analysis::{AnalysisFailure, SourceDigest};
use studio_domain::identity::{
    DecodeLayers, IDENTITY_SCHEMA, IdentityCompletion, IdentityKind, IdentityPage, IdentityRow,
    IdentitySummary, MAX_IDENTITY_PAGE,
};
use studio_domain::job::SourceFingerprint;
use studio_domain::preview::GrainPolicy;

use super::document::{Checked, HashReader, Refusal};
use crate::stream::{MAX_UNIT_BYTES, PREVIEW_DECODE_THREADS, preview_decoder_settings};

const RECORD_BYTES: usize = 32;
const MAX_INDEX_BYTES: u64 = 128 << 20;
type Record = [u64; 4];

#[derive(Debug)]
struct Table {
    file: File,
    count: u64,
}

impl Table {
    fn new() -> io::Result<Self> {
        Ok(Self {
            file: tempfile::tempfile()?,
            count: 0,
        })
    }

    fn append(&mut self, record: Record) -> io::Result<()> {
        let mut bytes = [0; RECORD_BYTES];
        for (chunk, value) in bytes.chunks_exact_mut(8).zip(record) {
            chunk.copy_from_slice(&value.to_le_bytes());
        }
        self.seek(self.count)?;
        self.file.write_all(&bytes)?;
        self.count += 1;
        Ok(())
    }

    fn seek(&mut self, row: u64) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(row * RECORD_BYTES as u64))?;
        Ok(())
    }

    fn read(&mut self) -> io::Result<Record> {
        let mut bytes = [0; RECORD_BYTES];
        self.file.read_exact(&mut bytes)?;
        let mut words = [0; 4];
        for (word, chunk) in words.iter_mut().zip(bytes.chunks_exact(8)) {
            let mut value = [0; 8];
            value.copy_from_slice(chunk);
            *word = u64::from_le_bytes(value);
        }
        Ok(words)
    }
}

#[derive(Debug)]
struct State {
    temporal: Table,
    coded: Table,
    continuation: Table,
    output: Table,
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
            temporal: Table::new().map_err(storage)?,
            coded: Table::new().map_err(storage)?,
            continuation: Table::new().map_err(storage)?,
            output: Table::new().map_err(storage)?,
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
        let count = (table.count - first).min(u64::from(count)) as usize;
        let mut rows = Vec::with_capacity(count);
        table.seek(first).map_err(storage)?;
        for _ in 0..count {
            let [a, b, c, d] = table.read().map_err(storage)?;
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
                decoder.drain();
                return self.receive(&mut decoder, cancelled);
            };
            let mut cursor = AnnexBObuCursor::new(unit.bytes, unit.offset);
            while !self.stopped(cancelled) {
                let Some(obu) = cursor.next_obu().map_err(|error| error.to_string())? else {
                    break;
                };
                if obu.header.obu_type == ObuType::TemporalDelimiter {
                    self.state()
                        .append(IdentityKind::Temporal, [obu.offset.get(), 0, 0, 0]);
                }
            }
            if self.stopped(cancelled) {
                return Ok(());
            }
            // The submission token is the file offset of its first byte.
            let props = DataProps::default().with_user_data(unit.offset.get());
            if decoder
                .send_data(unit.bytes, props)
                .map_err(decode_failed)?
                != SendStatus::Accepted
            {
                return Err("The decoder refused an identity scan submission.".into());
            }
            self.receive(&mut decoder, cancelled)?;
        }
        Ok(())
    }

    fn receive(&self, decoder: &mut Decoder, cancelled: &dyn Fn() -> bool) -> Result<(), String> {
        while !self.stopped(cancelled) {
            let Some(picture) = decoder.wait_picture().map_err(decode_failed)? else {
                break;
            };
            let location = picture.provenance();
            self.state().append(
                IdentityKind::Output,
                [
                    file_offset(location.coded),
                    file_offset(location.display),
                    file_offset(location.sequence_header),
                    pack_layers(
                        picture.extended_layer_id().get(),
                        picture.embedded_layer_id().get(),
                        picture.temporal_layer_id().get(),
                    ),
                ],
            );
        }
        Ok(())
    }

    fn stopped(&self, cancelled: &dyn Fn() -> bool) -> bool {
        cancelled() || self.state().write_error.is_some()
    }

    fn observe(&self, event: CodedFrameEvent) {
        let (kind, record) = match event {
            CodedFrameEvent::Frame {
                coded,
                sequence_header,
                extended_layer_id,
                embedded_layer_id,
                temporal_layer_id,
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
            ),
            CodedFrameEvent::Continuation { coded, obu, .. } => (
                IdentityKind::Continuation,
                [file_offset(coded), file_offset(obu), 0, 0],
            ),
            _ => return,
        };
        self.state().append(kind, record);
    }
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

fn decode_failed(error: splot_decode::DecodeError) -> String {
    format!("Cannot decode the source for the identity index. {error}")
}

fn invalid(message: &str) -> Refusal {
    Refusal::new(AnalysisFailure::InvalidRequest, message)
}

fn storage(error: io::Error) -> Refusal {
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
        let document =
            Document::open(&WirePath::from_path(source.path()), &|| false).map_err(refusal)?;
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
            .scan(&mut source.reopen()?, summary.digest.as_deref(), &|| false)
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
    fn ten_bit_output_paging_and_source_verification() -> TestResult {
        let mut source = tempfile::NamedTempFile::new()?;
        source.write_all(LAYERED_10_BIT)?;
        let document =
            Document::open(&WirePath::from_path(source.path()), &|| false).map_err(refusal)?;
        let summary = document.summary();
        let index = IdentityIndex::new(GrainPolicy::Skip, summary.fingerprint).map_err(refusal)?;
        index
            .scan(&mut source.reopen()?, summary.digest.as_deref(), &|| false)
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

        let mut wrong = summary.digest.clone().ok_or("Missing source digest.")?;
        wrong.sha256 = "wrong".into();
        let index = IdentityIndex::new(GrainPolicy::Skip, summary.fingerprint).map_err(refusal)?;
        assert!(matches!(
            index.scan(&mut source.reopen()?, Some(&wrong), &|| false),
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
        assert!(page.rows.is_empty());
        Ok(())
    }
}
