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
use studio_domain::protocol::{Command, Event};

use super::{Inspect, TestResult, failure};

const MULTI_TILE: &[u8] = include_bytes!("fixtures/syn-2tile-zone1-edge-128x64-q192.ivf");
const PADDED: &[u8] = include_bytes!("fixtures/syn-mrl-padded-444-67x67-10bit-q120.ivf");
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
