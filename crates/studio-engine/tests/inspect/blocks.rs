// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use std::fs;

use studio_domain::analysis::AnalysisFailure;
use studio_domain::blocks::{
    BlockCoverage, BlockData, BlockEvidence, BlockEvidenceKind, BlockId, BlockMotion,
    BlockPrediction, BlockQuery, BlockRect, BlockReference, BlockVector, IntraAngleDelta,
    IntraPredictor, MAX_BLOCK_BYTES, MAX_BLOCK_RECORDS,
};
use studio_domain::layers::LayerDecodeSelection;
use studio_domain::preview::{GrainPolicy, SamplePlane};
use studio_domain::protocol::{Command, Event};
use studio_domain::timeline::TimelineTarget;

use super::{Inspect, TestResult, failure};

const MULTI_TILE: &[u8] = include_bytes!("fixtures/syn-2tile-zone1-edge-128x64-q192.ivf");
const PADDED: &[u8] = include_bytes!("../preview/fixtures/syn-mrl-padded-444-67x67-10bit-q120.ivf");
const COMPOUND: &[u8] = include_bytes!("fixtures/syn-2frame-sameref-compound-64x32-10bit-q150.ivf");

fn query(coded_offset: u64, x: u32, y: u32) -> BlockQuery {
    BlockQuery {
        coded_offset,
        selection: LayerDecodeSelection::Default,
        kind: BlockEvidenceKind::Structure,
        region: Some(BlockRect {
            x,
            y,
            width: 1,
            height: 1,
        }),
        detail: None,
        max_events: MAX_BLOCK_RECORDS,
        max_bytes: MAX_BLOCK_BYTES,
    }
}

fn request(query: BlockQuery) -> Command {
    Command::AnalysisBlocks { document: 1, query }
}

fn capture(
    inspect: &mut Inspect,
    query: BlockQuery,
) -> Result<BlockEvidence, Box<dyn std::error::Error>> {
    let reply = inspect.ask(request(query))?;
    let Event::AnalysisBlocksReady {
        document: 1,
        evidence,
    } = reply
    else {
        return Err(format!("Expected block evidence. Engine reply: {reply:?}").into());
    };
    assert!(evidence.matches(query));
    Ok(*evidence)
}

#[test]
fn block_query_preserves_tile_intra_and_budget_coverage() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("tiles.ivf");
    fs::write(&path, MULTI_TILE)?;
    let mut inspect = Inspect::spawn()?;
    inspect.open(1, &path)?;

    let query = query(60, 64, 0);
    let evidence = capture(&mut inspect, query)?;
    assert_eq!(evidence.coverage, BlockCoverage::Complete);
    assert!(evidence.records.iter().any(|record| {
        record.id.tile == 1
            && record.id.coded == (BlockRect { x: 64, y: 0, width: 64, height: 32 })
            && matches!(&record.data, BlockData::Coding {
                prediction: Some(BlockPrediction::Intra { luma: Some(IntraPredictor::Mode { name, .. }), .. }),
                qindex: 192,
                segment: 0,
                ..
            } if name == "Vertical")
    }));

    let budget = capture(
        &mut inspect,
        BlockQuery {
            max_bytes: 1,
            ..query
        },
    )?;
    assert_eq!(budget.coverage, BlockCoverage::Truncated);
    assert_eq!(budget.records, []);

    let empty = capture(
        &mut inspect,
        BlockQuery {
            region: Some(BlockRect {
                x: 128,
                y: 64,
                width: 1,
                height: 1,
            }),
            ..query
        },
    )?;
    assert_eq!(empty.coverage, BlockCoverage::Complete);
    assert_eq!(empty.records, []);

    let invalid = inspect.ask(request(BlockQuery {
        max_events: 0,
        ..query
    }))?;
    assert_eq!(failure(&invalid), Some(AnalysisFailure::InvalidRequest));
    Ok(())
}

#[test]
fn block_query_preserves_padded_edge_and_chroma_in_ten_bit_input() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("padded.ivf");
    fs::write(&path, PADDED)?;
    let mut inspect = Inspect::spawn()?;
    inspect.open(1, &path)?;

    let evidence = capture(&mut inspect, query(62, 32, 64))?;
    assert_eq!(evidence.coverage, BlockCoverage::Complete);
    assert!(evidence.records.iter().any(|record| {
        record.id.coded == (BlockRect { x: 32, y: 64, width: 16, height: 8 })
            && record.visible == Some(BlockRect { x: 32, y: 64, width: 16, height: 3 })
            && matches!(&record.data, BlockData::Coding {
                prediction: Some(BlockPrediction::Intra {
                    luma: Some(IntraPredictor::Mode { name, reference_line: Some(1), angle_delta: Some(IntraAngleDelta::DecodedLuma(2)), .. }),
                    ..
                }),
                qindex: 168,
                segment: 0,
                ..
            } if name == "D203")
    }));
    Ok(())
}

#[test]
fn block_query_preserves_compound_motion_and_stops_after_a_missing_target() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("compound.ivf");
    fs::write(&path, COMPOUND)?;
    let mut inspect = Inspect::spawn()?;
    inspect.open(1, &path)?;

    let evidence = capture(&mut inspect, query(207, 0, 0))?;
    assert_eq!(evidence.coverage, BlockCoverage::Complete);
    let id = evidence
        .records
        .iter()
        .find_map(|record| match &record.data {
            BlockData::Coding {
                prediction:
                    Some(BlockPrediction::Inter {
                        mode,
                        references,
                        precision_eighths: Some(2),
                        ..
                    }),
                ..
            } if mode == "NEW_NEWMV"
                && references.iter().all(|reference| {
                    matches!(reference, Some(BlockReference::Frame { slot: 0, .. }))
                }) =>
            {
                Some(record.id)
            }
            _ => None,
        });
    assert_eq!(
        id.map(|BlockId { coded, .. }| coded),
        Some(BlockRect {
            x: 0,
            y: 0,
            width: 64,
            height: 16
        })
    );
    assert!(evidence.records.iter().any(|record| {
        Some(record.id) == id
            && matches!(
                record.data,
                BlockData::Motion {
                    motion: BlockMotion::Inter {
                        vectors: [
                            Some(BlockVector { col: 32, row: 0 }),
                            Some(BlockVector { col: -128, row: 0 })
                        ],
                        ..
                    }
                }
            )
    }));

    let absent = capture(&mut inspect, query(44, 0, 0))?;
    assert_eq!(absent.coverage, BlockCoverage::FrameNotFound);
    assert!(absent.replayed_through < 207);
    assert_eq!(absent.records, []);
    Ok(())
}

#[test]
fn coefficient_replay_preserves_avm_levels_scan_and_depth() -> TestResult {
    const CHROMA: &[u8] =
        include_bytes!("../preview/fixtures/syn-3frame-padded-filters-422-134x69-10bit-q150.ivf");
    let dir = tempfile::tempdir()?;

    // AVM a1ec4594d97028ebc34083ec8346616d697c2fc9, before inverse transforms.
    for (fixture, source, ordinal, plane, x, depth, extent, qindex, quant, dc, raster, ac) in [
        (
            1,
            MULTI_TILE,
            0,
            SamplePlane::Y,
            64,
            8,
            10,
            192,
            9984,
            (-1, -312),
            1,
            (-11, -3432),
        ),
        (
            2,
            CHROMA,
            1,
            SamplePlane::U,
            128,
            10,
            17,
            198,
            12032,
            (-18, -27072),
            4,
            (1, 1504),
        ),
    ] {
        let path = dir.path().join(format!("source-{fixture}.ivf"));
        fs::write(&path, source)?;
        let mut inspect = Inspect::spawn()?;
        inspect.open(1, &path)?;
        let coefficients = coefficient_query(&mut inspect, ordinal, plane, x, 64)?;
        let result = capture(&mut inspect, coefficients)?;
        let values = assert_coefficient_values(result, extent, dc, raster, ac)?;
        let metadata = values
            .quantization
            .ok_or("The dequantizer inputs are missing.")?;
        assert_eq!(
            (
                metadata.bit_depth,
                metadata.qindex,
                metadata.dc_quant,
                metadata.ac_quant
            ),
            (depth, qindex, quant, quant)
        );
        if depth == 8 {
            assert_eq!((values.width, values.height), (32, 32));
            assert_eq!(
                values.scan.as_deref().and_then(|scan| scan.get(..6)),
                Some([0, 32, 1, 64, 33, 2].as_slice())
            );
            let limited = capture(
                &mut inspect,
                BlockQuery {
                    max_bytes: 1,
                    ..coefficients
                },
            )?;
            assert_eq!(limited.coverage, BlockCoverage::Truncated);
            assert!(limited.coefficients.is_none());
        } else {
            assert_eq!((values.width, values.height), (4, 16));
        }
    }
    Ok(())
}

fn coefficient_query(
    inspect: &mut Inspect,
    ordinal: u64,
    plane: SamplePlane,
    x: u32,
    plane_x: u32,
) -> Result<BlockQuery, Box<dyn std::error::Error>> {
    let scan = inspect.next_id;
    let scanned = inspect.ask(Command::AnalysisIdentityScan {
        document: 1,
        grain: GrainPolicy::Skip,
        selection: LayerDecodeSelection::Default,
    })?;
    assert!(matches!(scanned, Event::AnalysisIdentityScanned { .. }));
    let output =
        super::timeline_location(inspect, scan, TimelineTarget::Output { index: ordinal })?;
    let request = BlockQuery {
        kind: BlockEvidenceKind::Transforms,
        ..query(output.coded_offset, x, 0)
    };
    let geometry = capture(inspect, request)?;
    let id = geometry
        .records
        .iter()
        .find_map(|record| match &record.data {
            BlockData::Transform { transform }
                if transform.id.plane == plane
                    && transform.id.coded.x == plane_x
                    && transform.id.coded.y == 0 =>
            {
                Some(transform.id)
            }
            _ => None,
        })
        .ok_or("The decoder did not return the expected transform.")?;
    assert!(geometry.coefficients.is_none());
    Ok(BlockQuery {
        kind: BlockEvidenceKind::Coefficients,
        region: None,
        detail: Some(id),
        max_events: 1,
        ..request
    })
}

// Checks the exact signed pairs after the response crosses the worker protocol.
fn assert_coefficient_values(
    evidence: BlockEvidence,
    extent: u16,
    dc: (i32, i32),
    raster: usize,
    ac: (i32, i32),
) -> Result<Box<studio_domain::blocks::BlockCoefficients>, Box<dyn std::error::Error>> {
    assert_eq!(evidence.coverage, BlockCoverage::Complete);
    let values = evidence
        .coefficients
        .ok_or("The decoder did not return coefficients.")?;
    assert_eq!(
        values.domain,
        studio_domain::blocks::BlockCoefficientDomain::Plane
    );
    assert_eq!(values.eob, extent);
    assert_eq!((values.quantized[0], values.dequantized[0]), dc);
    assert_eq!((values.quantized[raster], values.dequantized[raster]), ac);
    Ok(values)
}

#[test]
fn entropy_replay_keeps_the_coded_frame_and_exact_transform_scope() -> TestResult {
    use studio_domain::blocks::{EntropyScope, MAX_ENTROPY_BYTES, MAX_ENTROPY_EVENTS};

    const FILTERS: &[u8] =
        include_bytes!("../preview/fixtures/syn-3frame-padded-filters-422-134x69-10bit-q150.ivf");
    // AVM a1ec4594d97028ebc34083ec8346616d697c2fc9, before the first coefficient read.
    for (name, bytes, offset, x, skip_context, skip_cutoff) in [
        ("eight.ivf", MULTI_TILE, 60, 64, 0, 32683),
        // The first packet starts after the 32-byte IVF and 12-byte packet headers.
        ("eight.obu", &MULTI_TILE[44..], 16, 64, 0, 32683),
        ("ten.ivf", FILTERS, 73, 0, 1, 604),
    ] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join(name);
        fs::write(&path, bytes)?;
        let mut inspect = Inspect::spawn()?;
        inspect.open(1, &path)?;

        let point = query(offset, x, 0);
        let geometry = capture(
            &mut inspect,
            BlockQuery {
                kind: BlockEvidenceKind::Transforms,
                ..point
            },
        )?;
        let transform = geometry
            .records
            .iter()
            .find_map(|record| match &record.data {
                BlockData::Transform { transform }
                    if transform.id.plane == SamplePlane::Y
                        && transform.id.coded.x == x
                        && transform.id.coded.y == 0 =>
                {
                    Some(transform.id)
                }
                _ => None,
            })
            .ok_or("Expected a transform at the test point.")?;
        let query = BlockQuery {
            kind: BlockEvidenceKind::Entropy(EntropyScope::Transform(transform)),
            region: None,
            max_events: MAX_ENTROPY_EVENTS,
            max_bytes: MAX_ENTROPY_BYTES,
            ..point
        };
        let details = capture(&mut inspect, query)?;
        assert_eq!(details.frame, geometry.frame);
        assert_eq!(details.records, []);
        let entropy = details
            .entropy
            .ok_or("Expected entropy for the selected transform.")?;
        assert_eq!(entropy.scope, EntropyScope::Transform(transform));
        assert!(entropy.tile_payload_bytes < bytes.len() as u64);
        let first = entropy
            .symbols
            .first()
            .ok_or("Expected a transform symbol.")?;
        assert_eq!(first.ordinal, 0);
        assert_eq!(first.syntax.as_deref(), Some("all_zero"));
        assert_eq!(first.value, 0);
        assert_eq!(
            first.context.as_ref().map(|context| context.values[2]),
            Some(skip_context)
        );
        assert_eq!(
            first.cdf.as_ref().map(|cdf| cdf.cutoffs_q15.as_slice()),
            Some([skip_cutoff].as_slice())
        );
        assert!(first.estimated_microbits.is_some());

        let partial = capture(
            &mut inspect,
            BlockQuery {
                max_bytes: 1,
                ..query
            },
        )?;
        assert_eq!(partial.coverage, BlockCoverage::Truncated);
        let partial = partial
            .entropy
            .ok_or("Expected scope metadata for the partial capture.")?;
        assert_eq!(partial.end_ordinal, entropy.end_ordinal);
        assert_eq!(partial.symbols, []);

        let block = capture(
            &mut inspect,
            BlockQuery {
                kind: BlockEvidenceKind::Entropy(EntropyScope::Block(transform.block)),
                ..query
            },
        )?;
        let block = block
            .entropy
            .ok_or("Expected entropy for the parent block.")?;
        assert_eq!(block.scope, EntropyScope::Block(transform.block));
        assert_eq!(block.tile_payload_bytes, entropy.tile_payload_bytes);
    }
    Ok(())
}
