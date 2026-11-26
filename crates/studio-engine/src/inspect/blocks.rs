// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Replays source units through the native decoder for a single coded frame.

use std::io::{BufReader, Read};

use splot_core::stream_reader::{ReaderError, TemporalUnitReader};
use splot_decode::block_evidence::{
    BlockEvidenceCapture, BlockEvidenceData, BlockEvidenceId, BlockEvidenceKind as NativeKind,
    BlockEvidenceRecord, BlockEvidenceRequest, BlockEvidenceStatus, BlockEvidenceTree,
};
use splot_decode::{
    DataProps, DecodeError, DecodeLimits, Decoder, ObuLocation, PlaneRect, SendStatus,
};
use studio_domain::analysis::AnalysisFailure;
use studio_domain::blocks::{
    BlockCoverage, BlockData, BlockEvidence, BlockEvidenceKind, BlockFrame, BlockId, BlockQuery,
    BlockRecord, BlockRect, BlockTree,
};
use studio_domain::identity::DecodeLayers;
use studio_domain::preview::GrainPolicy;

mod prediction;

use super::Cancellation;
use super::document::{Checked, MAX_UNIT_BYTES, Refusal};
use crate::stream::{operating_point_selection, preview_decoder_settings};

pub(super) fn replay(
    input: impl Read,
    limit: u64,
    partial: bool,
    query: BlockQuery,
    cancellation: Cancellation,
) -> Result<BlockEvidence, Refusal> {
    let selection =
        operating_point_selection(query.selection).map_err(|error| decode_failed(&error))?;
    let settings = preview_decoder_settings(1, DecodeLimits::DEFAULT, GrainPolicy::Skip)
        .with_operating_point_selection(selection);
    let region = query.region.map(native_rect).transpose()?;
    let reader_cancellation = cancellation.clone();
    let request = BlockEvidenceRequest::new(
        move |location| absolute(location) == Some(query.coded_offset),
        region,
        query.max_events as usize,
        query.max_bytes as usize,
    )
    .with_kind(match query.kind {
        BlockEvidenceKind::Structure => NativeKind::PartitionsAndBlocks,
        BlockEvidenceKind::Partitions => NativeKind::Partitions,
        BlockEvidenceKind::Coding => NativeKind::Blocks,
    })
    .with_cancellation(move || cancellation.is_set());
    let (mut decoder, capture) = Decoder::new_with_block_evidence(settings, request)
        .map_err(|error| decode_failed(&error))?;
    let mut progress = ReplayProgress {
        target: query.coded_offset,
        limit,
        partial,
        replayed_through: 0,
    };
    let outcome = feed(
        &mut decoder,
        &capture,
        input,
        &reader_cancellation,
        &mut progress,
    );
    drop(decoder);
    let report = capture.take().ok_or_else(|| {
        Refusal::new(
            AnalysisFailure::Io,
            "The decoder did not finish the capture.",
        )
    })?;
    if report.frame.is_none()
        && matches!(
            report.status,
            BlockEvidenceStatus::FrameNotFound | BlockEvidenceStatus::DecodeFailed
        )
    {
        outcome?;
    }
    let coverage = match report.status {
        BlockEvidenceStatus::Complete => BlockCoverage::Complete,
        BlockEvidenceStatus::Truncated => BlockCoverage::Truncated,
        BlockEvidenceStatus::Cancelled => BlockCoverage::Cancelled,
        BlockEvidenceStatus::FrameNotFound => BlockCoverage::FrameNotFound,
        BlockEvidenceStatus::Unavailable => BlockCoverage::Unavailable,
        BlockEvidenceStatus::DecodeFailed => BlockCoverage::DecodeFailed,
        BlockEvidenceStatus::AllocationFailed => BlockCoverage::AllocationFailed,
        _ => {
            return Err(unsupported(
                "The decoder returned an unknown capture status.",
            ));
        }
    };
    let frame = report
        .frame
        .map(|frame| {
            Ok(BlockFrame {
                coded_offset: offset(frame.provenance.coded)?,
                sequence_offset: offset(frame.provenance.sequence_header)?,
                layers: DecodeLayers {
                    extended: frame.extended_layer_id.get(),
                    embedded: frame.embedded_layer_id.get(),
                    temporal: frame.temporal_layer_id.get(),
                },
            })
        })
        .transpose()?;
    let records = report
        .records
        .iter()
        .map(record)
        .collect::<Result<Vec<_>, _>>()?;
    let evidence = BlockEvidence {
        query,
        frame,
        coverage,
        replayed_through: progress.replayed_through,
        records,
    };
    if !evidence.matches(query) {
        return Err(unsupported("The decoder returned invalid block evidence."));
    }
    Ok(evidence)
}

struct ReplayProgress {
    target: u64,
    limit: u64,
    partial: bool,
    replayed_through: u64,
}

fn feed(
    decoder: &mut Decoder,
    capture: &BlockEvidenceCapture,
    input: impl Read,
    cancellation: &Cancellation,
    progress: &mut ReplayProgress,
) -> Result<(), Refusal> {
    let cancelled = || cancellation.is_set();
    let mut reader = TemporalUnitReader::with_max_unit_bytes(
        BufReader::new(Checked::new(input.take(progress.limit), &cancelled)),
        MAX_UNIT_BYTES,
    );
    while !capture.is_finished() && !cancelled() {
        let unit = match reader.next_temporal_unit() {
            Ok(unit) => unit,
            Err(_) if cancelled() => break,
            Err(error) => return Err(read_failed(&error)),
        };
        if cancelled() {
            break;
        }
        let Some(unit) = unit else {
            break;
        };
        let base = unit.offset.get();
        if base > progress.target {
            break;
        }
        let end = base
            .checked_add(unit.bytes.len() as u64)
            .ok_or_else(|| unsupported("The decoder input offset exceeds the source range."))?;
        if progress.partial && unit.pts.is_none() && end == progress.limit {
            return Err(unsupported(
                "The index does not verify the last raw temporal unit in this prefix.",
            ));
        }
        if decoder
            .send_data(unit.bytes, DataProps::default().with_user_data(base))
            .map_err(|error| decode_failed(&error))?
            != SendStatus::Accepted
        {
            return Err(unsupported("The decoder did not accept the replay unit."));
        }
        progress.replayed_through = end;
        while !capture.is_finished()
            && !cancelled()
            && decoder
                .wait_picture()
                .map_err(|error| decode_failed(&error))?
                .is_some()
        {}
        if progress.target < end {
            break;
        }
    }
    if !capture.is_finished() && !cancelled() {
        decoder.drain();
        while !capture.is_finished()
            && !cancelled()
            && decoder
                .get_picture()
                .map_err(|error| decode_failed(&error))?
                .is_some()
        {}
    }
    Ok(())
}

fn native_rect(rect: BlockRect) -> Result<PlaneRect, Refusal> {
    PlaneRect::new(
        rect.x as usize,
        rect.y as usize,
        rect.width as usize,
        rect.height as usize,
    )
    .map_err(|error| Refusal::new(AnalysisFailure::InvalidRequest, error.to_string()))
}

fn wire_rect(rect: PlaneRect) -> Result<BlockRect, Refusal> {
    let component = |value| {
        u32::try_from(value)
            .map_err(|_| unsupported("A block coordinate exceeds the protocol range."))
    };
    Ok(BlockRect {
        x: component(rect.x())?,
        y: component(rect.y())?,
        width: component(rect.width())?,
        height: component(rect.height())?,
    })
}

fn block_id(id: BlockEvidenceId) -> Result<BlockId, Refusal> {
    let tree = match id.tree {
        BlockEvidenceTree::Shared => BlockTree::Shared,
        BlockEvidenceTree::Luma => BlockTree::Luma,
        BlockEvidenceTree::Chroma => BlockTree::Chroma,
        _ => {
            return Err(unsupported(
                "The decoder returned an unknown partition tree.",
            ));
        }
    };
    Ok(BlockId {
        tree,
        tile: u32::try_from(id.tile)
            .map_err(|_| unsupported("A tile index exceeds the protocol range."))?,
        coded: wire_rect(id.coded)?,
    })
}

fn record(record: &BlockEvidenceRecord) -> Result<BlockRecord, Refusal> {
    let data = match record.data {
        BlockEvidenceData::Partition(split) => BlockData::Partition {
            split: format!("{split:?}"),
        },
        BlockEvidenceData::Block(facts) => BlockData::Coding {
            has_chroma: facts.has_chroma,
            prediction: prediction::prediction(facts.prediction),
            qindex: facts.qindex,
            segment: facts.segment_id,
        },
        BlockEvidenceData::Motion(motion) => BlockData::Motion {
            motion: prediction::motion(motion)?,
        },
        _ => return Err(unsupported("The decoder returned an unknown record kind.")),
    };
    Ok(BlockRecord {
        id: block_id(record.id)?,
        parent: record.parent.map(block_id).transpose()?,
        visible: record.visible.map(wire_rect).transpose()?,
        data,
    })
}

fn absolute(location: ObuLocation) -> Option<u64> {
    location.user_data.checked_add(location.offset.get())
}

fn offset(location: ObuLocation) -> Result<u64, Refusal> {
    absolute(location).ok_or_else(|| unsupported("The decoder returned an invalid source offset."))
}

fn unsupported(message: impl Into<String>) -> Refusal {
    Refusal::new(AnalysisFailure::Unsupported, message)
}

fn read_failed(error: &ReaderError) -> Refusal {
    let code = match error {
        ReaderError::Io(_) => AnalysisFailure::Io,
        ReaderError::UnitTooLarge { .. } => AnalysisFailure::LimitReached,
        ReaderError::Ivf(_) => AnalysisFailure::Unsupported,
    };
    Refusal::new(code, format!("Cannot read the replay unit. {error}"))
}

fn decode_failed(error: &DecodeError) -> Refusal {
    let code = match error {
        DecodeError::Limit { .. } => AnalysisFailure::LimitReached,
        DecodeError::Pool { .. } | DecodeError::Input { .. } | DecodeError::State { .. } => {
            AnalysisFailure::Io
        }
        _ => AnalysisFailure::Unsupported,
    };
    Refusal::new(code, format!("Cannot decode the replay unit. {error}"))
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use studio_domain::blocks::{MAX_BLOCK_BYTES, MAX_BLOCK_RECORDS};
    use studio_domain::layers::LayerDecodeSelection;

    use super::*;

    struct Interrupt<'a> {
        source: &'a [u8],
        flag: &'a AtomicBool,
    }

    impl Read for Interrupt<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let count = self.source.read(buffer)?;
            self.flag.store(true, Ordering::SeqCst);
            Ok(count)
        }
    }

    #[test]
    fn cancellation_during_read_stops_before_decoder_submission() -> Result<(), String> {
        let source =
            include_bytes!("../../tests/inspect/fixtures/syn-2tile-zone1-edge-128x64-q192.ivf");
        let requested = Arc::new(AtomicBool::new(false));
        let input = Interrupt {
            source,
            flag: &requested,
        };
        let query = BlockQuery {
            coded_offset: 60,
            selection: LayerDecodeSelection::Default,
            kind: BlockEvidenceKind::Structure,
            region: None,
            max_events: MAX_BLOCK_RECORDS,
            max_bytes: MAX_BLOCK_BYTES,
        };
        let evidence = replay(
            input,
            source.len() as u64,
            false,
            query,
            Cancellation {
                stopped: Arc::default(),
                requested: Arc::clone(&requested),
            },
        )
        .map_err(|error| error.message)?;
        assert_eq!(evidence.coverage, BlockCoverage::Cancelled);
        assert_eq!(evidence.replayed_through, 0);
        assert_eq!(evidence.records, []);
        Ok(())
    }
}
