// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Presents the bounded decoder records for the selected coded frame.

use studio_domain::blocks::{
    BlockCoverage, BlockData, BlockMotion, BlockPrediction, BlockRecord, BlockRect, BlockTree,
    BlockVector, IntraPredictor,
};

use super::{model, references::output_text};
use crate::analysis::{Document, blocks::TAB};
use crate::ui::{BlockTableRow, Blocks};

pub(super) fn render_document(view: &Blocks<'_>, document: Option<&Document>) {
    let document = document.filter(|doc| doc.evidence.tab == TAB && !doc.layers.shown);
    let busy = document.is_some_and(|doc| doc.blocks.busy());
    let stopping = document.is_some_and(|doc| doc.blocks.stopping());
    let ready = document.is_some_and(Document::block_query_ready);
    let evidence = document
        .filter(|_| ready)
        .and_then(Document::block_evidence);
    view.set_busy(busy);
    view.set_stopping(stopping);
    view.set_can_inspect(ready && !busy);
    let status = if let Some(evidence) = evidence {
        let count = evidence.records.len();
        match evidence.coverage {
            BlockCoverage::Complete => format!("{count} decoder records in the requested scope."),
            BlockCoverage::Truncated => format!(
                "Partial result. A capture limit stopped the query after {count} decoder records."
            ),
            BlockCoverage::Cancelled => {
                format!("Inspection stopped. {count} decoder records are available.")
            }
            BlockCoverage::FrameNotFound => {
                "No admitted coded frame matched this offset.".to_owned()
            }
            BlockCoverage::Unavailable => {
                "The decoder could not provide block evidence for this frame.".to_owned()
            }
            BlockCoverage::DecodeFailed => format!(
                "The decoder failed during inspection. {count} partial records are available."
            ),
            BlockCoverage::AllocationFailed => format!(
                "The decoder ran out of space for evidence. {count} partial records are available."
            ),
        }
    } else if let Some(notice) = document.and_then(|doc| doc.blocks.notice()) {
        notice.to_owned()
    } else if stopping {
        "Stopping block inspection. The current decode unit must finish.".to_owned()
    } else if busy {
        "Decoding the selected frame and its prerequisites…".to_owned()
    } else if ready {
        "Inspect decoder block records for this frame. Studio may decode earlier frames.".to_owned()
    } else {
        "Select an indexed coded frame in the timeline or OBU list.".to_owned()
    };
    view.set_status(status.into());
    let context = document.zip(evidence).and_then(|(doc, evidence)| {
        evidence.frame.map(|frame| {
            format!(
                "Coded byte {}. Sequence byte {}. Layers X{}/M{}/T{}. Replay through byte {}. {} Q shows the block qindex before plane adjustments. Transform details, reconstruction stages and entropy evidence are unavailable.",
                frame.coded_offset,
                frame.sequence_offset,
                frame.layers.extended,
                frame.layers.embedded,
                frame.layers.temporal,
                evidence.replayed_through,
                output_text(doc, frame.coded_offset),
            )
        })
    });
    view.set_context(context.unwrap_or_default().into());
    view.set_rows(model(
        evidence
            .map(|evidence| evidence.records.iter().map(row).collect())
            .unwrap_or_default(),
    ));
}

fn row(record: &BlockRecord) -> BlockTableRow {
    let tree = match record.id.tree {
        BlockTree::Shared => "Shared",
        BlockTree::Luma => "Luma",
        BlockTree::Chroma => "Chroma",
    };
    let identity = format!("{} / {tree}", record.id.tile);
    let mut geometry = rect(record.id.coded);
    if record.visible != Some(record.id.coded) {
        match record.visible {
            Some(visible) => {
                geometry.push_str("\nVisible ");
                geometry.push_str(&rect(visible));
            }
            None => geometry.push_str("\nOutside view"),
        }
    }
    if let Some(parent) = record.parent {
        geometry.push_str("\nParent ");
        geometry.push_str(&rect(parent.coded));
    }
    let (prediction, quantizer) = match &record.data {
        BlockData::Partition { split } => (format!("Partition\n{split}"), "—".to_owned()),
        BlockData::Coding {
            has_chroma,
            prediction,
            qindex,
            segment,
        } => {
            let mut prediction = prediction_text(prediction.as_ref());
            if *has_chroma && record.id.tree == BlockTree::Shared {
                prediction.push_str("\nChroma");
            }
            (prediction, format!("Q {qindex}\nSeg {segment}"))
        }
        BlockData::Motion { motion } => (motion_text(*motion), "—".to_owned()),
    };
    BlockTableRow {
        identity: identity.into(),
        geometry: geometry.into(),
        prediction: prediction.into(),
        quantizer: quantizer.into(),
    }
}

fn rect(rect: BlockRect) -> String {
    format!("({}, {}) {}×{}", rect.x, rect.y, rect.width, rect.height)
}

fn prediction_text(prediction: Option<&BlockPrediction>) -> String {
    match prediction {
        Some(BlockPrediction::Intra { luma, chroma }) => {
            let mut text = "Intra".to_owned();
            if let Some(luma) = luma {
                text.push_str("\nY ");
                text.push_str(&intra_text(luma));
            }
            if let Some(chroma) = chroma {
                text.push_str("\nUV ");
                text.push_str(&intra_text(chroma));
            }
            text
        }
        Some(BlockPrediction::Inter {
            mode, inter_intra, ..
        }) => match inter_intra {
            Some(intra) => format!("Inter\n{mode}\nInter-intra {intra}"),
            None => format!("Inter\n{mode}"),
        },
        Some(BlockPrediction::IntraBlockCopy) => "Intra block copy".to_owned(),
        None => "Unavailable".to_owned(),
    }
}

fn intra_text(predictor: &IntraPredictor) -> String {
    match predictor {
        IntraPredictor::Mode {
            name,
            reference_line: Some(line),
            ..
        } if *line > 0 => {
            format!("{name}, line {line}")
        }
        IntraPredictor::Mode { name, .. } => name.clone(),
        IntraPredictor::Palette { colors } => format!("Palette ({colors} colors)"),
        IntraPredictor::Dip { mode, .. } => format!("DIP {mode}"),
        IntraPredictor::ChromaFromLuma { mode, .. } => format!("CfL {mode}"),
    }
}

fn motion_text(motion: BlockMotion) -> String {
    let vector = |vector: BlockVector| format!("({}, {})", vector.col, vector.row);
    match motion {
        BlockMotion::Inter { vectors, .. } => {
            let vectors = vectors.map(|value| value.map_or_else(|| "—".to_owned(), vector));
            format!("Base motion (⅛)\n{} / {}", vectors[0], vectors[1])
        }
        BlockMotion::IntraBlockCopy { vector: copy, .. } => {
            format!("Copy vector (⅛)\n{}", vector(copy))
        }
    }
}
