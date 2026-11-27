// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Replays source units through the native decoder for a single coded frame.

use std::io::{BufReader, Read};

use splot_core::stream_reader::{ReaderError, TemporalUnitReader};
use splot_decode::block_evidence::{
    BlockCoefficientDomain as NativeDomain, BlockEvidenceCapture, BlockEvidenceData,
    BlockEvidenceId, BlockEvidenceKind as NativeKind, BlockEvidenceRecord, BlockEvidenceReport,
    BlockEvidenceRequest, BlockEvidenceStatus, BlockEvidenceTree, BlockPartition,
    BlockTransformCoefficients, BlockTransformId as NativeTransformId,
};
use splot_decode::{
    DataProps, DecodeError, DecodeLimits, Decoder, ObuLocation, Picture, PlaneId, PlaneRect,
    SendStatus,
};
use studio_domain::analysis::AnalysisFailure;
use studio_domain::blocks::{
    BlockCoefficientDomain, BlockCoefficientMatrix, BlockCoefficientQuantization,
    BlockCoefficients, BlockCoverage, BlockData, BlockEvidence, BlockEvidenceKind, BlockFrame,
    BlockId, BlockQuery, BlockRecord, BlockRect, BlockSecondaryTransform, BlockTransform,
    BlockTransformDetails, BlockTransformId, BlockTree,
};
use studio_domain::identity::DecodeLayers;
use studio_domain::preview::{GrainPolicy, SamplePlane};
use studio_domain::stages::StageScope;

mod entropy;
mod prediction;
mod stages;

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
    let stage_scope = match query.kind {
        BlockEvidenceKind::Stages(scope) => Some(scope),
        _ => None,
    };
    let reader_cancellation = cancellation.clone();
    let request = capture_request(query, stage_scope, cancellation)?;
    let (mut decoder, capture) = Decoder::new_with_block_evidence(settings, request)
        .map_err(|error| decode_failed(&error))?;
    let mut progress = ReplayProgress {
        target: query.coded_offset,
        limit,
        partial,
        replayed_through: 0,
        presentation: None,
        capture_presentation: stage_scope.is_some_and(|scope| scope.include_grain),
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
    assemble(query, report, progress, stage_scope, &reader_cancellation)
}

fn capture_request(
    query: BlockQuery,
    stage_scope: Option<StageScope>,
    cancellation: Cancellation,
) -> Result<BlockEvidenceRequest, Refusal> {
    let region = query.region.map(native_rect).transpose()?;
    let request = BlockEvidenceRequest::new(
        move |location| absolute(location) == Some(query.coded_offset),
        region,
        query.max_events as usize,
        query.max_bytes as usize,
    )
    .with_kind(match query.kind {
        BlockEvidenceKind::Structure | BlockEvidenceKind::Entropy(_) => {
            NativeKind::PartitionsAndBlocks
        }
        BlockEvidenceKind::Partitions => NativeKind::Partitions,
        BlockEvidenceKind::Coding => NativeKind::Blocks,
        BlockEvidenceKind::Transforms | BlockEvidenceKind::Coefficients => NativeKind::Transforms,
        BlockEvidenceKind::Stages(_) => NativeKind::Stages,
    })
    .with_cancellation(move || cancellation.is_set());
    let request = match query.detail {
        Some(id) if query.kind == BlockEvidenceKind::Coefficients => {
            request.with_transform_coefficients(native_transform(id)?)
        }
        Some(id) => request.with_transform_details(native_transform(id)?),
        None => request,
    };
    Ok(if let BlockEvidenceKind::Entropy(scope) = query.kind {
        request.with_entropy(entropy::native_scope(scope)?)
    } else if let Some(scope) = stage_scope {
        request.with_stage_samples(stages::plane(scope.plane), stages::region(scope.region)?)
    } else {
        request
    })
}

fn assemble(
    query: BlockQuery,
    report: BlockEvidenceReport,
    progress: ReplayProgress,
    stage_scope: Option<StageScope>,
    cancellation: &Cancellation,
) -> Result<BlockEvidence, Refusal> {
    let mut coverage = match report.status {
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
    let mut stages = if let Some(scope) = stage_scope {
        report
            .stages
            .into_iter()
            .map(|stage| stages::evidence(scope, stage))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };
    if let (Some(scope), Some(picture), Some(_)) = (stage_scope, progress.presentation, frame)
        && !cancellation.is_set()
        && coverage == BlockCoverage::Complete
    {
        let captured_bytes = stages
            .iter()
            .map(|stage| stage.samples.len() * size_of::<i32>())
            .sum::<usize>();
        let remaining = (query.max_bytes as usize).saturating_sub(captured_bytes);
        stages.push(stages::grain(scope, &picture, remaining)?);
    }
    if stage_scope.is_some() && cancellation.is_set() {
        stages.clear();
        coverage = BlockCoverage::Cancelled;
    }
    let evidence = BlockEvidence {
        query,
        frame,
        coverage,
        replayed_through: progress.replayed_through,
        records,
        coefficients: if coverage == BlockCoverage::Complete {
            report
                .coefficients
                .map(coefficients)
                .transpose()?
                .map(Box::new)
        } else {
            None
        },
        stages,
        entropy: report
            .entropy
            .as_ref()
            .map(entropy::wire_evidence)
            .transpose()?,
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
    presentation: Option<Picture>,
    capture_presentation: bool,
}

impl ReplayProgress {
    fn observe(&mut self, picture: Picture) {
        if self.capture_presentation {
            let provenance = picture.provenance();
            if provenance.display == provenance.coded
                && absolute(provenance.coded) == Some(self.target)
            {
                self.presentation = Some(picture);
            }
        }
    }
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
        while (!capture.is_finished() || progress.capture_presentation) && !cancelled() {
            let Some(picture) = decoder
                .wait_picture()
                .map_err(|error| decode_failed(&error))?
            else {
                break;
            };
            progress.observe(picture);
        }
        if progress.target < end {
            break;
        }
    }
    if !capture.is_finished() && !cancelled() {
        decoder.drain();
        while (!capture.is_finished() || progress.capture_presentation) && !cancelled() {
            let Some(picture) = decoder
                .get_picture()
                .map_err(|error| decode_failed(&error))?
            else {
                break;
            };
            progress.observe(picture);
        }
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

fn native_block(block: BlockId) -> Result<BlockEvidenceId, Refusal> {
    let tree = match block.tree {
        BlockTree::Shared => BlockEvidenceTree::Shared,
        BlockTree::Luma => BlockEvidenceTree::Luma,
        BlockTree::Chroma => BlockEvidenceTree::Chroma,
    };
    Ok(BlockEvidenceId::new(
        block.tile as usize,
        tree,
        native_rect(block.coded)?,
    ))
}

fn native_transform(id: BlockTransformId) -> Result<NativeTransformId, Refusal> {
    let block = native_block(id.block)?;
    let plane = match id.plane {
        SamplePlane::Y => PlaneId::Y,
        SamplePlane::U => PlaneId::U,
        SamplePlane::V => PlaneId::V,
    };
    Ok(NativeTransformId::new(block, plane, native_rect(id.coded)?))
}

fn transform_id(id: NativeTransformId) -> Result<BlockTransformId, Refusal> {
    Ok(BlockTransformId {
        block: block_id(id.block)?,
        plane: match id.plane {
            PlaneId::Y => SamplePlane::Y,
            PlaneId::U => SamplePlane::U,
            PlaneId::V => SamplePlane::V,
        },
        coded: wire_rect(id.coded)?,
    })
}

fn coefficients(values: BlockTransformCoefficients) -> Result<BlockCoefficients, Refusal> {
    let domain = match values.domain {
        NativeDomain::Plane => BlockCoefficientDomain::Plane,
        NativeDomain::CrossComponent { mode, .. } => {
            BlockCoefficientDomain::CrossComponent { mode }
        }
        _ => {
            return Err(unsupported(
                "The decoder returned an unknown coefficient domain.",
            ));
        }
    };
    let quantization = values
        .quantization
        .map(|q| {
            Ok::<_, Refusal>(BlockCoefficientQuantization {
                qindex: q.qindex,
                dc_quant: q.dc_quant,
                ac_quant: q.ac_quant,
                denominator: q.denominator,
                bit_depth: q.bit_depth.bits(),
                matrix: q
                    .matrix
                    .map(|m| {
                        Ok::<_, Refusal>(BlockCoefficientMatrix {
                            level: u8::try_from(m.level).map_err(|_| {
                                unsupported("The matrix level exceeds the protocol range.")
                            })?,
                            user_defined: m.user_defined,
                        })
                    })
                    .transpose()?,
            })
        })
        .transpose()?;
    let size = |value| {
        u8::try_from(value)
            .map_err(|_| unsupported("The coefficient grid exceeds the protocol range."))
    };
    Ok(BlockCoefficients {
        id: transform_id(values.id)?,
        codec_revision: env!("SPLOT_CODEC_REVISION").into(),
        width: size(values.width)?,
        height: size(values.height)?,
        domain,
        quantized: values.quantized,
        dequantized: values.dequantized,
        scan: values.scan.map(<[u16]>::to_vec),
        eob: u16::try_from(values.eob)
            .map_err(|_| unsupported("The coefficient scan exceeds the protocol range."))?,
        use_tcq: values.use_tcq,
        forward_scan: values.forward_scan,
        dc_parity_hidden: values.dc_parity_hidden,
        quantization,
        secondary: values.secondary.map(|secondary| BlockSecondaryTransform {
            sec_tx_type: secondary.sec_tx_type,
            most_probable_stx_set: secondary.most_probable_stx_set,
        }),
    })
}

fn record(record: &BlockEvidenceRecord) -> Result<BlockRecord, Refusal> {
    let data = match record.data {
        BlockEvidenceData::Partition(split) => BlockData::Partition {
            split: partition_name(split).to_owned(),
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
        BlockEvidenceData::Transform(transform) => BlockData::Transform {
            transform: BlockTransform {
                id: transform_id(transform.id)?,
                luma_coded: wire_rect(transform.luma_coded)?,
                details: transform.details.map(|details| BlockTransformDetails {
                    has_coefficients: details.has_coefficients,
                    plane_tx_type: details.plane_tx_type.map(|kind| kind.av2_name().to_owned()),
                    lossless: details.lossless,
                }),
            },
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

const fn partition_name(split: BlockPartition) -> &'static str {
    match split {
        BlockPartition::None => "PARTITION_NONE",
        BlockPartition::Horz => "PARTITION_HORZ",
        BlockPartition::Vert => "PARTITION_VERT",
        BlockPartition::Horz3 => "PARTITION_HORZ_3",
        BlockPartition::Vert3 => "PARTITION_VERT_3",
        BlockPartition::Horz4A => "PARTITION_HORZ_4A",
        BlockPartition::Horz4B => "PARTITION_HORZ_4B",
        BlockPartition::Vert4A => "PARTITION_VERT_4A",
        BlockPartition::Vert4B => "PARTITION_VERT_4B",
        BlockPartition::Split => "PARTITION_SPLIT",
    }
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
            detail: None,
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
