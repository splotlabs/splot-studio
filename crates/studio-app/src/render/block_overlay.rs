// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The current crop and its matching rows, as shown by Slint.

use std::collections::HashSet;
use std::fmt::Write as _;

use slint::Color;
use studio_domain::blocks::{
    BlockCoverage, BlockData, BlockPrediction, BlockRecord, BlockTree, IntraPredictor,
};
use studio_domain::preview::SamplePlane;

use super::{
    blocks::{prediction_text, rect},
    refill,
};
use crate::analysis::block_overlay::{Mode, Overlay, geometry, sample_rect};
use crate::ui::{BlockOutline, BlockOverlay, OverlayRow};

const TABLE_ROWS: usize = 6;

pub(super) fn render(view: &BlockOverlay<'_>, overlay: &Overlay) {
    view.set_mode(overlay.mode as i32);
    view.set_plane(match overlay.plane {
        None => 0,
        Some(SamplePlane::Y) => 1,
        Some(SamplePlane::U) => 2,
        Some(SamplePlane::V) => 3,
    });
    view.set_tool(overlay.tool);
    view.set_can_pick(overlay.picture.is_some() && (!overlay.busy() || overlay.viewport.is_some()));
    view.set_busy(overlay.busy());
    view.set_status(status(overlay).into());
    view.set_legend(match overlay.mode {
        Mode::Off => "",
        Mode::Partitions => "SB is a superblock. P is a partition. B is a coding block. SB/B is an unsplit superblock.",
        Mode::Transforms => "Y, U and V identify the transform plane. Select a transform for its details.",
        Mode::Quantizer => "Q shows the decoder's effective qindex before plane adjustments. Select a small region to read its value.",
        Mode::Segment => "S shows the decoded segment ID.",
        Mode::Tools => "+ means used. − means unused. ? means the decoder could not provide this fact.",
    }.into());

    let mut outlines = Vec::new();
    let records = overlay.records();
    let selected = overlay.selected.map(|selection| selection.key);
    let selected_index = records.iter().position(|(_, key)| Some(*key) == selected);
    let first = selected_index.map_or(0, |i| i / TABLE_ROWS * TABLE_ROWS);
    let rows: Vec<_> = records
        .iter()
        .skip(first)
        .take(TABLE_ROWS)
        .map(|(record, key)| OverlayRow {
            label: format!("{} · {}", row_label(overlay, record), identity(record)).into(),
            detail: rect(geometry(record)).into(),
            selected: Some(*key) == selected,
        })
        .collect();
    view.set_first_row(as_int(first));
    view.set_row_count(as_int(records.len()));
    if let Some(rows) = refill(&view.get_rows(), rows) {
        view.set_rows(rows);
    }
    view.set_picked(picked(overlay).into());

    if let Some(picture) = overlay.picture {
        let (width, height) = picture.viewport.size();
        view.set_sample_width(i32::try_from(width).unwrap_or(i32::MAX));
        view.set_sample_height(i32::try_from(height).unwrap_or(i32::MAX));
        for (record, key) in records {
            if let Some(outline) = outline(overlay, picture, record, Some(key) == selected) {
                outlines.push(outline);
            }
        }
        outlines.sort_by_key(|item| {
            (
                item.selected,
                std::cmp::Reverse(i64::from(item.width) * i64::from(item.height)),
            )
        });
        // The top outline also owns the label when nodes share one origin.
        let mut labelled = HashSet::with_capacity(outlines.len());
        for outline in outlines.iter_mut().rev() {
            if !labelled.insert((outline.x, outline.y)) {
                outline.label.clear();
            }
        }
    }
    if let Some(outlines) = refill(&view.get_outlines(), outlines) {
        view.set_outlines(outlines);
    }
}

fn status(overlay: &Overlay) -> String {
    if overlay.picture.is_none() {
        return "Decoder evidence requires a paused picture with a coded frame location."
            .to_owned();
    }
    if let Some(notice) = &overlay.notice {
        return notice.clone();
    }
    if overlay.busy() {
        return "Reading decoder evidence. Studio can decode earlier frames first.".to_owned();
    }
    match overlay.viewport.as_ref().map(|report| report.coverage) {
        Some(BlockCoverage::Complete) => "Regions without a mark have no matching record in this capture.".to_owned(),
        Some(BlockCoverage::Truncated) => "Capture limit reached. Unmarked regions may be uncaptured. Zoom in or select a point to read more.".to_owned(),
        Some(BlockCoverage::Cancelled) => "Capture stopped. Some regions may be uncaptured.".to_owned(),
        Some(BlockCoverage::DecodeFailed) => "The decoder failed during capture. Some regions may be uncaptured.".to_owned(),
        Some(BlockCoverage::AllocationFailed) => "The decoder ran out of space for evidence. Some regions may be uncaptured.".to_owned(),
        Some(BlockCoverage::Unavailable | BlockCoverage::FrameNotFound) => "Block evidence is unavailable for this picture.".to_owned(),
        None => "No decoder evidence for this crop.".to_owned(),
    }
}

fn label(record: &BlockRecord) -> String {
    match &record.data {
        BlockData::Partition { split } if record.parent.is_none() => format!("Superblock {split}"),
        BlockData::Partition { split } => format!("Partition {split}"),
        BlockData::Coding { .. } if record.parent.is_none() => {
            "Superblock · Coding block".to_owned()
        }
        BlockData::Coding { .. } => "Coding block".to_owned(),
        BlockData::Transform { transform } => {
            format!("{} transform", plane_label(transform.id.plane))
        }
        BlockData::Motion { .. } => "Motion".to_owned(),
    }
}

fn row_label(overlay: &Overlay, record: &BlockRecord) -> String {
    match (&record.data, overlay.mode) {
        (BlockData::Coding { qindex, .. }, Mode::Quantizer) => format!("Q {qindex}"),
        (BlockData::Coding { segment, .. }, Mode::Segment) => format!("Segment {segment}"),
        (BlockData::Coding { prediction, .. }, Mode::Tools) => {
            tool_use(overlay, prediction.as_ref())
                .map_or("Unavailable", |used| if used { "Used" } else { "Unused" })
                .to_owned()
        }
        _ => label(record),
    }
}

fn identity(record: &BlockRecord) -> String {
    let tree = match record.id.tree {
        BlockTree::Shared => "shared tree",
        BlockTree::Luma => "luma tree",
        BlockTree::Chroma => "chroma tree",
    };
    format!("Tile {} · {tree}", record.id.tile)
}

fn picked(overlay: &Overlay) -> String {
    let (Some(record), Some(selected)) = (overlay.selected_record(), overlay.selected) else {
        return "Select a region in the picture or in the list.".to_owned();
    };
    let frame = selected.frame;
    let mut text = format!(
        "{} · {}\nCoded frame at byte {} · X{}/M{}/T{}\nCoded luma {}",
        label(record),
        identity(record),
        frame.coded_offset,
        frame.layers.extended,
        frame.layers.embedded,
        frame.layers.temporal,
        rect(geometry(record))
    );
    if let Some(visible) = record.visible.filter(|v| *v != geometry(record)) {
        let _ = write!(text, "\nVisible luma {}", rect(visible));
    }
    if let Some(parent) = record.parent {
        let _ = write!(text, "\nPartition parent {}", rect(parent.coded));
    }
    match &record.data {
        BlockData::Coding {
            prediction,
            segment,
            qindex,
            ..
        } => {
            let prediction = prediction.as_ref();
            let _ = write!(
                text,
                "\nDecoder qindex {qindex} before plane adjustments · Segment {segment}\n{}",
                prediction_text(prediction).replace('\n', " · ")
            );
            if overlay.mode == Mode::Tools {
                let state = tool_use(overlay, prediction)
                    .map_or("Unavailable", |used| if used { "Used" } else { "Unused" });
                let _ = write!(text, "\nSelected tool · {state}");
            }
        }
        BlockData::Transform { transform } => {
            let _ = write!(
                text,
                "\nNative {} {}",
                plane_label(transform.id.plane),
                rect(transform.id.coded)
            );
            append_transform_details(&mut text, overlay);
        }
        _ => {}
    }
    text
}

fn append_transform_details(text: &mut String, overlay: &Overlay) {
    let detail = overlay
        .details
        .as_ref()
        .and_then(|reply| reply.records.first())
        .and_then(|record| match &record.data {
            BlockData::Transform { transform } => transform.details.as_ref(),
            _ => None,
        });
    if let Some(detail) = detail {
        let coefficients = if detail.has_coefficients {
            "Present"
        } else {
            "Zero"
        };
        let transform_type = if detail.has_coefficients {
            detail.plane_tx_type.as_deref().unwrap_or("Unavailable")
        } else {
            "Unused"
        };
        let segment = if detail.lossless {
            "Lossless segment"
        } else {
            "Lossy segment"
        };
        let _ = write!(
            text,
            "\nPlaneTxType {transform_type} · Coefficients {coefficients} · {segment}"
        );
    } else if overlay.busy() {
        text.push_str("\nReading transform details…");
    } else {
        text.push_str("\nTransform details are unavailable.");
    }
}

fn outline(
    overlay: &Overlay,
    picture: crate::preview::BlockPicture,
    record: &BlockRecord,
    selected: bool,
) -> Option<BlockOutline> {
    let region = picture.viewport.project(sample_rect(geometry(record)))?;
    let transparent = Color::from_argb_u8(0, 0, 0, 0);
    let cyan = Color::from_argb_u8(230, 129, 243, 241);
    let lavender = Color::from_argb_u8(240, 184, 158, 255);
    let amber = Color::from_argb_u8(240, 255, 213, 119);
    let (line, tint, strong, label) = match (&record.data, overlay.mode) {
        (BlockData::Partition { .. }, Mode::Partitions) if record.parent.is_none() => {
            (amber, transparent, true, "SB".into())
        }
        (BlockData::Partition { .. }, Mode::Partitions) => {
            (lavender, transparent, false, "P".into())
        }
        (BlockData::Coding { .. }, Mode::Partitions) if record.parent.is_none() => {
            (amber, transparent, true, "SB/B".into())
        }
        (BlockData::Coding { .. }, Mode::Partitions) => (cyan, transparent, false, "B".into()),
        (BlockData::Transform { transform }, Mode::Transforms) => (
            cyan,
            transparent,
            false,
            plane_label(transform.id.plane).into(),
        ),
        (BlockData::Coding { qindex, .. }, Mode::Quantizer) => (
            cyan,
            Color::from_argb_u8(38, 18, 190, 221),
            false,
            format!("Q {qindex}"),
        ),
        (BlockData::Coding { segment, .. }, Mode::Segment) => {
            let colors = [
                0x001c_bcdf,
                0x009b_74f3,
                0x00f2_c467,
                0x00ea_7676,
                0x0078_df98,
                0x007d_90f3,
                0x00f5_87cd,
                0x00ad_ce6c,
            ];
            let color = *colors.get(usize::from(*segment))?;
            (
                Color::from_argb_encoded(0xe000_0000 | color),
                Color::from_argb_encoded(0x3800_0000 | color),
                false,
                format!("S {segment}"),
            )
        }
        (BlockData::Coding { prediction, .. }, Mode::Tools) => {
            match tool_use(overlay, prediction.as_ref()) {
                Some(true) => (
                    amber,
                    Color::from_argb_u8(66, 246, 185, 54),
                    true,
                    "+".into(),
                ),
                Some(false) => (
                    cyan,
                    Color::from_argb_u8(29, 18, 190, 221),
                    false,
                    "−".into(),
                ),
                None => (
                    lavender,
                    Color::from_argb_u8(46, 154, 112, 223),
                    false,
                    "?".into(),
                ),
            }
        }
        _ => return None,
    };
    Some(BlockOutline {
        x: i32::try_from(region.x).ok()?,
        y: i32::try_from(region.y).ok()?,
        width: i32::try_from(region.width).ok()?,
        height: i32::try_from(region.height).ok()?,
        line,
        tint,
        strong,
        label: label.into(),
        selected,
    })
}

fn tool_use(overlay: &Overlay, prediction: Option<&BlockPrediction>) -> Option<bool> {
    use IntraPredictor::{ChromaFromLuma, Dip, Palette};
    let prediction = prediction?;
    if overlay.tool == 0 {
        return Some(matches!(prediction, BlockPrediction::IntraBlockCopy));
    }
    let plane = overlay.active_plane()?;
    if matches!(
        (plane, overlay.tool),
        (SamplePlane::Y, 3) | (SamplePlane::U | SamplePlane::V, 2)
    ) {
        return None;
    }
    let BlockPrediction::Intra { luma, chroma } = prediction else {
        return Some(false);
    };
    let predictor = match plane {
        SamplePlane::Y => luma.as_ref()?,
        SamplePlane::U | SamplePlane::V => chroma.as_ref()?,
    };
    Some(matches!(
        (overlay.tool, predictor),
        (1, Palette { .. }) | (2, Dip { .. }) | (3, ChromaFromLuma { .. })
    ))
}

const fn plane_label(plane: SamplePlane) -> &'static str {
    match plane {
        SamplePlane::Y => "Y",
        SamplePlane::U => "U",
        SamplePlane::V => "V",
    }
}

fn as_int(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}
