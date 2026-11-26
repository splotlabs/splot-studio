// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Numerical prediction and motion facts for the selected coding block.

use slint::ComponentHandle;
use studio_domain::blocks::{
    BlockBlend, BlockMotion, BlockPrediction, BlockRect, BlockReference, BlockRefinement,
    BlockVector, IntraAngleDelta, IntraPredictor,
};

use super::model;
use crate::analysis::{Analysis, BlockMotionInput, Input, SelectedMotion};
use crate::preview::{BlockViewport, Preview};
use crate::ui::{
    AppWindow, BlockMotionView, MotionGlyph, MotionRegion, PredictionFact, PredictorRow,
};

pub(crate) fn bind(window: &AppWindow, send: impl Fn(Input) + Clone + 'static) {
    let motion = window.global::<BlockMotionView>();
    motion.on_inspect_references({
        let send = send.clone();
        move || send(Input::BlockMotion(BlockMotionInput::InspectReferences))
    });
    motion.on_preview_reference({
        let send = send.clone();
        move |index| send(Input::BlockMotion(BlockMotionInput::Preview(index)))
    });
    motion.on_follow_reference({
        let send = send.clone();
        move |index| send(Input::BlockMotion(BlockMotionInput::Follow(index)))
    });
    motion.on_set_overlay(move |show| send(Input::BlockMotion(BlockMotionInput::Overlay(show))));
}

pub(super) fn render(window: &AppWindow, analysis: &Analysis) {
    let view = window.global::<BlockMotionView>();
    let selected = analysis.selected_motion();
    view.set_active(selected.is_some());
    view.set_show_overlay(analysis.show_motion());
    let Some(selected) = selected else {
        view.set_facts(model(Vec::new()));
        view.set_predictors(model(Vec::new()));
        view.set_can_inspect_references(false);
        view.set_reference_status("".into());
        view.set_can_show_overlay(false);
        view.set_vectors(model(Vec::new()));
        view.set_copy_source(MotionRegion::default());
        return;
    };
    view.set_facts(model(facts(selected.prediction, selected.motion)));
    view.set_predictors(model(predictors(&selected)));
    let (vectors, copy_source) = analysis.overlay().picture.map_or_else(
        || (Vec::new(), MotionRegion::default()),
        |picture| picture_marks(&selected, picture.viewport),
    );
    view.set_can_show_overlay(!vectors.is_empty() || copy_source.width > 0.0);
    view.set_vectors(model(vectors));
    view.set_copy_source(copy_source);
    let has_frame = matches!(selected.prediction, Some(BlockPrediction::Inter { references, .. })
        if references.iter().any(|reference| matches!(reference, Some(BlockReference::Frame { .. }))));
    let references = &selected.document.references;
    let need_references = has_frame && selected.references.is_none();
    view.set_can_inspect_references(need_references && !references.busy());
    let message = if need_references && references.busy() {
        "Reading reference slots…"
    } else if need_references {
        references
            .notice()
            .unwrap_or("Read the coded frame's input slots to locate its reference pictures.")
    } else {
        selected.document.reference_preview_notice().unwrap_or("")
    };
    view.set_reference_status(message.into());
}

/// Updates the illustrative extent on a compact reference picture.
pub fn render_reference(window: &AppWindow, analysis: &Analysis, preview: &Preview) {
    window
        .global::<BlockMotionView>()
        .set_preview_extent(reference_extent(analysis, preview).unwrap_or_default());
}

fn reference_extent(analysis: &Analysis, preview: &Preview) -> Option<MotionRegion> {
    let selected = analysis.selected_motion()?;
    let position = analysis.motion_preview_position()?;
    let BlockPrediction::Inter { references, .. } = selected.prediction? else {
        return None;
    };
    let reference = (*references.get(position)?)?;
    let motion = selected.motion?;
    let block = selected.block.coded;
    let origin = translated_origin(block, reference, motion, position)?;
    let picture = preview.reference_preview()?.ok()?;
    let viewport = BlockViewport::from_header(&picture.header)?;
    Some(project_region(viewport, origin, block))
}

fn picture_marks(
    selected: &SelectedMotion<'_>,
    viewport: BlockViewport,
) -> (Vec<MotionGlyph>, MotionRegion) {
    let block = selected.block.coded;
    let center = (
        i64::from(block.x) * 8 + i64::from(block.width) * 4,
        i64::from(block.y) * 8 + i64::from(block.height) * 4,
    );
    let start = viewport.project_eighths(center.0, center.1);
    let glyph = |vector: BlockVector, label: &str| {
        let endpoint = viewport.project_eighths(
            center.0 + i64::from(vector.col),
            center.1 + i64::from(vector.row),
        );
        let (end, mark) = clip_motion(start, endpoint)?;
        Some(MotionGlyph {
            line: format!(
                "M {} {} L {} {}",
                start.0 * 1000.0,
                start.1 * 1000.0,
                end.0 * 1000.0,
                end.1 * 1000.0
            )
            .into(),
            endpoint_x: end.0,
            endpoint_y: end.1,
            mark,
            label: label.into(),
        })
    };
    let mut copy = MotionRegion::default();
    let vectors = match (selected.prediction, selected.motion) {
        (
            Some(BlockPrediction::Inter { references, .. }),
            Some(BlockMotion::Inter { vectors, warp, .. }),
        ) => (0..2)
            .filter_map(|position| {
                let BlockReference::Frame {
                    scaled: Some(false),
                    ..
                } = references[position]?
                else {
                    return None;
                };
                if warp[position].is_some() {
                    return None;
                }
                glyph(vectors[position]?, ["1", "2"][position])
            })
            .collect(),
        (_, Some(BlockMotion::IntraBlockCopy { vector, source, .. })) => {
            if let Some(source) = source {
                copy = project_region(
                    viewport,
                    (i64::from(source.x) * 8, i64::from(source.y) * 8),
                    source,
                );
            }
            glyph(vector, "").into_iter().collect()
        }
        _ => Vec::new(),
    };
    (vectors, copy)
}

fn clip_motion(start: (f32, f32), end: (f32, f32)) -> Option<((f32, f32), bool)> {
    if !(0.0..=1.0).contains(&start.0) || !(0.0..=1.0).contains(&start.1) {
        return None;
    }
    let reach = |start: f32, end: f32| {
        if end < 0.0 {
            start / (start - end)
        } else if end > 1.0 {
            (1.0 - start) / (end - start)
        } else {
            1.0
        }
    };
    let fraction = reach(start.0, end.0).min(reach(start.1, end.1));
    Some((
        (
            start.0 + (end.0 - start.0) * fraction,
            start.1 + (end.1 - start.1) * fraction,
        ),
        fraction >= 1.0,
    ))
}

fn project_region(viewport: BlockViewport, origin: (i64, i64), shape: BlockRect) -> MotionRegion {
    let (x, y) = viewport.project_eighths(origin.0, origin.1);
    let (right, bottom) = viewport.project_eighths(
        origin.0 + i64::from(shape.width) * 8,
        origin.1 + i64::from(shape.height) * 8,
    );
    if x >= 1.0 || y >= 1.0 || right <= 0.0 || bottom <= 0.0 {
        return MotionRegion::default();
    }
    MotionRegion {
        x,
        y,
        width: right - x,
        height: bottom - y,
    }
}

fn predictors(selected: &SelectedMotion<'_>) -> Vec<PredictorRow> {
    let Some(BlockPrediction::Inter {
        references,
        differences,
        ..
    }) = selected.prediction
    else {
        return Vec::new();
    };
    let (vectors, warp) = match selected.motion {
        Some(BlockMotion::Inter { vectors, warp, .. }) => (vectors, warp),
        _ => ([None; 2], [None; 2]),
    };
    references
        .iter()
        .enumerate()
        .filter_map(|(position, reference)| {
            let reference = (*reference)?;
            let mut detail = Vec::new();
            let label = match reference {
                BlockReference::Frame {
                    list_index,
                    slot,
                    scaled,
                } => {
                    let scale = match scaled {
                        Some(true) => "Scaled reference",
                        Some(false) => "Unit scale",
                        None => "Scale unavailable",
                    };
                    detail.push(scale.to_owned());
                    format!(
                        "Predictor {} · reference {list_index} · slot {slot}",
                        position + 1
                    )
                }
                BlockReference::Tip => {
                    detail
                        .push("Synthetic reference. No single coded picture to follow.".to_owned());
                    "TIP".to_owned()
                }
            };
            let target = selected
                .retained_reference(position)
                .map(|(_, frame)| frame);
            if let Some(frame) = target {
                let layers = frame.layers;
                detail.push(format!(
                    "Coded byte {}. Layer X{}/M{}/T{}. {} × {} luma samples",
                    frame.coded_offset,
                    layers.extended,
                    layers.embedded,
                    layers.temporal,
                    frame.width,
                    frame.height
                ));
            } else if selected.references.is_some()
                && matches!(reference, BlockReference::Frame { .. })
            {
                detail.push("Retained picture identity unavailable".to_owned());
            }
            detail.push(match differences[position] {
                Some(vector) => format!("Signaled difference. {}", vector_text(vector)),
                None => "Signaled difference unused".to_owned(),
            });
            detail.push(match vectors[position] {
                Some(vector) => format!("Resolved base vector. {}", vector_text(vector)),
                None => "Resolved base vector unavailable".to_owned(),
            });
            if let Some(model) = warp[position] {
                detail.push(format!(
                    "Resolved affine model {model:?} in AV2 WarpParams units"
                ));
            }
            Some(PredictorRow {
                position: i32::try_from(position).ok()?,
                label: label.into(),
                detail: detail.join(". ").into(),
                available: target.is_some(),
            })
        })
        .collect()
}

pub(super) fn facts(
    prediction: Option<&BlockPrediction>,
    motion: Option<BlockMotion>,
) -> Vec<PredictionFact> {
    let mut rows = Vec::new();
    match prediction {
        Some(BlockPrediction::Intra { luma, chroma }) => {
            for (label, predictor) in [("Luma", luma), ("Chroma", chroma)] {
                if let Some(predictor) = predictor {
                    rows.push(fact(label, intra(predictor)));
                }
            }
        }
        Some(BlockPrediction::Inter {
            mode,
            precision_eighths,
            interpolation,
            inter_intra,
            bawp,
            skip_mode,
            adaptive_mvd,
            ..
        }) => {
            rows.push(fact("Inter mode", mode));
            rows.push(fact("Motion precision", precision(*precision_eighths)));
            rows.push(fact("Interpolation", interpolation));
            if let Some(mode) = inter_intra {
                rows.push(fact("Inter-intra", mode));
            }
            let tools = [
                (*bawp, "block-adaptive weights"),
                (*skip_mode, "inter skip"),
                (*adaptive_mvd, "adaptive vector difference"),
            ]
            .into_iter()
            .filter_map(|(used, name)| used.then_some(name))
            .collect::<Vec<_>>();
            if !tools.is_empty() {
                rows.push(fact("Other tools in use", tools.join(", ")));
            }
            match motion {
                Some(BlockMotion::Inter {
                    refinement, blend, ..
                }) => {
                    rows.push(fact("Subblock motion", refinement_text(refinement)));
                    if let Some(blend) = blend {
                        rows.push(fact("Compound blend", blend_text(blend)));
                    }
                }
                _ => rows.push(fact("Resolved motion", "Unavailable")),
            }
        }
        Some(BlockPrediction::IntraBlockCopy) => {
            rows.push(fact("Intra block copy", "Uses the current coded picture."));
            if let Some(BlockMotion::IntraBlockCopy {
                vector,
                precision_eighths,
                difference,
                morph,
                source,
            }) = motion
            {
                rows.push(fact("Resolved block vector", vector_text(vector)));
                if let Some(difference) = difference {
                    rows.push(fact("Signaled vector difference", vector_text(difference)));
                    rows.push(fact("Copy precision", precision(precision_eighths)));
                } else {
                    rows.push(fact("Signaled vector difference", "Unused"));
                }
                rows.push(fact("Morph prediction", on_off(morph)));
                let source = source.map_or_else(
                    || "Direct integer luma source unavailable.".to_owned(),
                    |rect| {
                        format!(
                            "({}, {}), {} × {} luma samples",
                            rect.x, rect.y, rect.width, rect.height
                        )
                    },
                );
                rows.push(fact("Copy source", source));
            } else {
                rows.push(fact("Copy motion", "Unavailable"));
            }
        }
        None => rows.push(fact("Prediction", "Unavailable")),
    }
    rows
}

fn fact(label: &str, value: impl Into<slint::SharedString>) -> PredictionFact {
    PredictionFact {
        label: label.into(),
        value: value.into(),
    }
}

fn intra(predictor: &IntraPredictor) -> String {
    match predictor {
        IntraPredictor::Mode {
            name,
            angle_delta,
            angle_degrees,
            reference_line,
            above_reference_line,
            secondary_reference_line,
            dpcm,
        } => {
            let mut text = vec![name.clone()];
            if let Some(delta) = angle_delta {
                let (source, value) = match delta {
                    IntraAngleDelta::DecodedLuma(value) => ("Decoded", *value),
                    IntraAngleDelta::ChromaFromLuma(value) => ("From luma", *value),
                    IntraAngleDelta::ChromaDerived(value) => ("Derived", *value),
                    IntraAngleDelta::DpcmZero => ("DPCM", 0),
                };
                text.push(format!("{source} angle delta {value:+} in 3° steps"));
            }
            if let Some(angle) = angle_degrees {
                text.push(format!("Resolved angle {angle}°"));
            }
            if let Some(line) = reference_line {
                text.push(format!("Reference line {line}"));
            }
            if let Some(line) = above_reference_line {
                text.push(format!("Above reference line {line}"));
            }
            if let Some(blending) = secondary_reference_line {
                text.push(format!(
                    "Secondary reference-line blending {}",
                    on_off(*blending)
                ));
            }
            if let Some(dpcm) = dpcm {
                text.push(format!("DPCM {dpcm}"));
            }
            text.join(". ")
        }
        IntraPredictor::Palette { colors } => format!("Palette with {colors} colors"),
        IntraPredictor::Dip { mode, transpose } => {
            format!("DIP mode {mode}. Transpose {}", on_off(*transpose))
        }
        IntraPredictor::ChromaFromLuma { mode, alpha } => alpha.map_or_else(
            || format!("Chroma from luma. {mode}"),
            |[u, v]| format!("Chroma from luma. {mode}. U alpha {u:+}. V alpha {v:+}"),
        ),
    }
}

fn on_off(used: bool) -> &'static str {
    if used { "used" } else { "unused" }
}

fn precision(eighths: Option<u8>) -> String {
    match eighths {
        Some(value @ (1 | 2 | 4)) => format!("1/{} luma sample", 8 / value),
        Some(8) => "1 luma sample".to_owned(),
        Some(value @ (16 | 32 | 64)) => format!("{} luma samples", value / 8),
        _ => "Unavailable".to_owned(),
    }
}

fn refinement_text(refinement: Option<BlockRefinement>) -> String {
    match refinement {
        Some(BlockRefinement::None) => "Unused".to_owned(),
        Some(BlockRefinement::Tip) => {
            "TIP derives subblock motion. Effective vectors unavailable.".to_owned()
        }
        Some(BlockRefinement::Compound {
            refine_mv,
            optical_flow,
        }) => format!(
            "Compound plan. Refine-MV {}. Optical flow {}. Effective vectors unavailable.",
            if refine_mv { "enabled" } else { "disabled" },
            if optical_flow {
                "permitted"
            } else {
                "disabled"
            },
        ),
        None => "Unavailable".to_owned(),
    }
}

fn blend_text(blend: BlockBlend) -> String {
    match blend {
        BlockBlend::Average {
            implicit_mask,
            weight,
        } => {
            format!(
                "Average. First weight {weight}. Implicit mask {}.",
                on_off(implicit_mask)
            )
        }
        BlockBlend::DifferenceWeighted { inverse } => {
            format!("Difference-weighted. Inverted mask {}.", on_off(inverse))
        }
        BlockBlend::Wedge { index, sign } => {
            format!("Wedge {index}. Sign flag {}.", u8::from(sign))
        }
    }
}

pub(super) fn vector_text(vector: BlockVector) -> String {
    format!(
        "Column {:+}, row {:+} in eighths of a luma sample",
        vector.col, vector.row
    )
}

pub(super) fn translated_origin(
    block: BlockRect,
    reference: BlockReference,
    motion: BlockMotion,
    position: usize,
) -> Option<(i64, i64)> {
    let BlockReference::Frame {
        scaled: Some(false),
        ..
    } = reference
    else {
        return None;
    };
    let BlockMotion::Inter {
        vectors,
        warp,
        refinement: Some(BlockRefinement::None),
        ..
    } = motion
    else {
        return None;
    };
    if warp.get(position)?.is_some() {
        return None;
    }
    let vector = (*vectors.get(position)?)?;
    Some((
        i64::from(block.x) * 8 + i64::from(vector.col),
        i64::from(block.y) * 8 + i64::from(vector.row),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_projection_retains_signed_subpixel_values() {
        let coded = BlockRect {
            x: 0,
            y: 0,
            width: 64,
            height: 64,
        };
        let motion = BlockMotion::Inter {
            vectors: [Some(BlockVector { col: -4, row: 3 }), None],
            warp: [None, None],
            refinement: Some(BlockRefinement::None),
            blend: None,
        };
        let reference = |scaled| BlockReference::Frame {
            list_index: 0,
            slot: 0,
            scaled,
        };
        assert_eq!(
            translated_origin(coded, reference(Some(false)), motion, 0),
            Some((-4, 3))
        );
        assert_eq!(
            translated_origin(coded, reference(Some(true)), motion, 0),
            None
        );
        assert_eq!(translated_origin(coded, reference(None), motion, 0), None);
    }

    #[test]
    fn visual_line_stops_at_the_crop_without_marking_a_false_endpoint() {
        assert_eq!(
            clip_motion((0.5, 0.5), (-0.5, 1.25)),
            Some(((0.0, 0.875), false))
        );
        assert_eq!(
            clip_motion((0.25, 0.5), (0.125, 0.25)),
            Some(((0.125, 0.25), true))
        );
        assert_eq!(clip_motion((-0.25, 0.5), (0.125, 0.25)), None);
    }

    #[test]
    fn intra_palette_and_refinement_keep_their_provenance() {
        let prediction = BlockPrediction::Intra {
            luma: Some(IntraPredictor::Palette { colors: 2 }),
            chroma: None,
        };
        assert_eq!(
            facts(Some(&prediction), None)[0].value,
            "Palette with 2 colors"
        );
        assert_eq!(refinement_text(Some(BlockRefinement::None)), "Unused");
        assert_eq!(refinement_text(None), "Unavailable");
    }
}
