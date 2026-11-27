// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use std::fs;

use studio_domain::analysis::AnalysisFailure;
use studio_domain::blocks::{BlockCoverage, BlockEvidenceKind, BlockQuery};
use studio_domain::layers::LayerDecodeSelection;
use studio_domain::preview::{SamplePlane, SampleRect};
use studio_domain::protocol::{Command, Event};
use studio_domain::stages::{DecodeStage, MAX_STAGE_BYTES, StageScope, StageStatus};

use super::{Inspect, TestResult, failure};

const GRAIN: &[u8] =
    include_bytes!("../preview/fixtures/syn-3frame-filmgrain-inter-64x64-10bit.ivf");
const MULTI_TILE: &[u8] = include_bytes!("fixtures/syn-2tile-zone1-edge-128x64-q192.ivf");

fn query(coded_offset: u64, region: SampleRect) -> BlockQuery {
    BlockQuery {
        coded_offset,
        selection: LayerDecodeSelection::Default,
        kind: BlockEvidenceKind::Stages(StageScope {
            plane: SamplePlane::Y,
            include_grain: true,
            region,
        }),
        region: None,
        detail: None,
        max_events: 1,
        max_bytes: MAX_STAGE_BYTES,
    }
}

#[test]
fn stage_replay_distinguishes_reference_from_grain_at_exact_coordinates() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("grain.ivf");
    fs::write(&path, GRAIN)?;
    let mut inspect = Inspect::spawn()?;
    inspect.open(1, &path)?;
    let query = query(
        2412,
        SampleRect {
            x: 3,
            y: 5,
            width: 8,
            height: 8,
        },
    );
    let Event::AnalysisBlocksReady { evidence, .. } =
        inspect.ask(Command::AnalysisBlocks { document: 1, query })?
    else {
        return Err("Expected stage evidence.".into());
    };
    assert!(evidence.matches(query));
    assert_eq!(evidence.coverage, BlockCoverage::Complete);
    // The AVM results and fixture revision are in preview/fixtures/README.md.
    for (kind, sample) in [(DecodeStage::Reference, 120), (DecodeStage::Grain, 110)] {
        let stage = evidence
            .stages
            .iter()
            .find(|stage| stage.stage == kind)
            .ok_or("Missing stage")?;
        assert_eq!(stage.status, StageStatus::Applied);
        assert_eq!(stage.bit_depth, 10);
        assert_eq!(stage.sample(3, 5), Some(sample));
    }

    // The engine verifies the file again before it publishes a replay.
    fs::write(&path, &GRAIN[..GRAIN.len() - 1])?;
    let stale = inspect.ask(Command::AnalysisBlocks { document: 1, query })?;
    assert_eq!(failure(&stale), Some(AnalysisFailure::SourceChanged));
    Ok(())
}

#[test]
fn stage_replay_preserves_eight_bit_edge_geometry_and_obeys_byte_limit() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("tiles.ivf");
    fs::write(&path, MULTI_TILE)?;
    let mut inspect = Inspect::spawn()?;
    inspect.open(1, &path)?;
    let query = query(
        60,
        SampleRect {
            x: 120,
            y: 56,
            width: 32,
            height: 32,
        },
    );
    let Event::AnalysisBlocksReady { evidence, .. } =
        inspect.ask(Command::AnalysisBlocks { document: 1, query })?
    else {
        return Err("Expected stage evidence.".into());
    };
    assert!(evidence.matches(query));
    let reference = evidence
        .stages
        .iter()
        .find(|stage| stage.stage == DecodeStage::Reference)
        .ok_or("Missing reference stage")?;
    assert_eq!(reference.bit_depth, 8);
    assert_eq!(
        reference.region,
        Some(SampleRect {
            x: 120,
            y: 56,
            width: 8,
            height: 8
        })
    );
    assert_eq!(reference.samples.len(), 64);
    assert_eq!(reference.sample(120, 56), Some(226));
    assert_eq!(reference.sample(128, 63), None);
    let no_grain = evidence
        .stages
        .iter()
        .find(|stage| stage.stage == DecodeStage::Grain)
        .ok_or("Missing grain status")?;
    assert_eq!(no_grain.status, StageStatus::Skipped);

    let limited = BlockQuery {
        max_bytes: 1,
        ..query
    };
    let Event::AnalysisBlocksReady { evidence, .. } = inspect.ask(Command::AnalysisBlocks {
        document: 1,
        query: limited,
    })?
    else {
        return Err("Expected stage coverage.".into());
    };
    assert!(evidence.matches(limited));
    assert_eq!(evidence.coverage, BlockCoverage::Truncated);
    assert!(evidence.stages.iter().all(|stage| stage.samples.is_empty()));
    Ok(())
}

#[test]
fn captured_prediction_and_rounded_residual_do_not_reverse_clipping() -> TestResult {
    const EIGHT: &[u8] = include_bytes!("fixtures/syn-gdf-xtile-lr-512x256-q190.ivf");
    const TEN: &[u8] =
        include_bytes!("../preview/fixtures/syn-3frame-padded-filters-422-134x69-10bit-q150.ivf");
    let dir = tempfile::tempdir()?;
    for (depth, bytes, offset, x, y, expected) in [
        (8, EIGHT, 74, 145, 42, [225, 33, 255]),
        (10, TEN, 7_835, 31, 20, [882, 156, 1023]),
    ] {
        let path = dir.path().join(format!("{depth}-bit-clipping.ivf"));
        fs::write(&path, bytes)?;
        let mut inspect = Inspect::spawn()?;
        inspect.open(1, &path)?;
        let query = query(
            offset,
            SampleRect {
                x,
                y,
                width: 1,
                height: 1,
            },
        );
        let Event::AnalysisBlocksReady { evidence, .. } =
            inspect.ask(Command::AnalysisBlocks { document: 1, query })?
        else {
            return Err("Expected stage evidence.".into());
        };
        assert!(evidence.matches(query));
        for (kind, value) in [
            DecodeStage::Prediction,
            DecodeStage::Residual,
            DecodeStage::Reconstruction,
        ]
        .into_iter()
        .zip(expected)
        {
            let stage = evidence
                .stages
                .iter()
                .find(|stage| stage.stage == kind)
                .ok_or("Missing clipping stage")?;
            assert_eq!(stage.bit_depth, depth);
            assert_eq!(stage.sample(x, y), Some(value));
        }
    }
    Ok(())
}
