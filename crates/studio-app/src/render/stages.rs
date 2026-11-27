// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Presents exact stage values and an independently scaled display image.

use std::fmt::Write;

use slint::{ComponentHandle, Image, Rgb8Pixel, SharedPixelBuffer, SharedString};
use studio_domain::blocks::{BlockCoverage, BlockEvidence, BlockEvidenceKind};
use studio_domain::preview::SampleRect;
use studio_domain::stages::{DecodeStage, StageSamples, StageStatus};

use super::model;
use crate::analysis::stages::{Stages, TAB};
use crate::analysis::{Analysis, Document, Input, StageInput};
use crate::ui::{AppWindow, StageInspector, StageValue};

pub(crate) fn bind(window: &AppWindow, send: impl Fn(Input) + Clone + 'static) {
    let view = window.global::<StageInspector>();
    let submit = move |input| send(Input::Stage(input));
    view.on_capture({
        let submit = submit.clone();
        move |plane, x, y, grain| submit(StageInput::Capture(plane, x.into(), y.into(), grain))
    });
    view.on_choose_grain({
        let submit = submit.clone();
        move |include| submit(StageInput::Grain(include))
    });
    view.on_choose_plane({
        let submit = submit.clone();
        move |index| submit(StageInput::Plane(index))
    });
    view.on_choose_gain({
        let submit = submit.clone();
        move |index| submit(StageInput::Gain(index))
    });
    view.on_use_block({
        let submit = submit.clone();
        move || submit(StageInput::UseBlock)
    });
    view.on_stop({
        let submit = submit.clone();
        move || submit(StageInput::Cancel)
    });
    view.on_choose({
        let submit = submit.clone();
        move |right, index| submit(StageInput::Choose(right, index))
    });
    view.on_choose_mode({
        let submit = submit.clone();
        move |index| submit(StageInput::Mode(index))
    });
    view.on_choose_zoom({
        let submit = submit.clone();
        move |index| submit(StageInput::Zoom(index))
    });
    view.on_pan({
        let submit = submit.clone();
        move |x, y| submit(StageInput::Pan(x, y))
    });
    view.on_sample({
        let submit = submit.clone();
        move |x, y| submit(StageInput::Sample(x.into(), y.into()))
    });
    view.on_pick(move |right, x, y, width, height| {
        submit(StageInput::Pick(right, x, y, width, height));
    });
}

pub(super) fn render(window: &AppWindow, analysis: &Analysis) {
    let view = window.global::<StageInspector>();
    let Some(doc) = analysis
        .shown()
        .filter(|doc| doc.evidence.tab == TAB && !doc.layers.shown)
    else {
        view.set_has_evidence(false);
        view.set_image_a(Image::default());
        view.set_image_b(Image::default());
        return;
    };
    let stages = &doc.stages;
    let busy = stages.busy();
    let ready = doc.block_query_ready();
    view.set_can_capture(ready && !busy);
    view.set_can_use_block(analysis.stage_block_origin(doc).is_some() && ready);
    view.set_busy(busy);
    view.set_stopping(stages.stopping);
    view.set_status(status(doc).into());
    view.set_origin_x(stages.origin.0.to_string().into());
    view.set_origin_y(stages.origin.1.to_string().into());
    view.set_plane(stages.plane);
    view.set_include_grain(stages.include_grain);
    view.set_has_evidence(stages.evidence.as_ref().is_some_and(|e| e.frame.is_some()));
    view.set_stage_a(i32::try_from(stages.selections[0]).unwrap_or_default());
    view.set_stage_b(i32::try_from(stages.selections[1]).unwrap_or_default());
    view.set_mode(stages.mode);
    view.set_gain(stages.gain);
    view.set_zoom(stages.zoom);
    let Some(evidence) = stages.evidence.as_ref() else {
        view.set_image_a(Image::default());
        view.set_image_b(Image::default());
        return;
    };
    render_evidence(&view, stages, evidence);
}

fn render_evidence(view: &StageInspector<'_>, stages: &Stages, evidence: &BlockEvidence) {
    let BlockEvidenceKind::Stages(scope) = evidence.query.kind else {
        return;
    };
    view.set_choices(model(
        DecodeStage::ALL
            .into_iter()
            .map(|kind| {
                let status = evidence
                    .stages
                    .iter()
                    .find(|stage| stage.stage == kind)
                    .map_or(StageStatus::Uncaptured, |stage| stage.status);
                SharedString::from(format!("{} · {}", label(kind), availability(status)))
            })
            .collect(),
    ));
    let [a, b] = [stages.stage(false), stages.stage(true)];
    let plane = match scope.plane {
        studio_domain::preview::SamplePlane::Y => "Y",
        studio_domain::preview::SamplePlane::U => "U",
        studio_domain::preview::SamplePlane::V => "V",
    };
    let label_a = caption(stages, false, a, plane);
    let a_image = a.and_then(|a| image(stages, a, None));
    let (b_image, label_b) = if stages.mode != 0 {
        if let Some((a, b)) = a.zip(b).filter(|(a, b)| a.can_compare(b)) {
            let operator = if stages.mode == 1 {
                "Signed B − A · Zero is mid-gray."
            } else {
                "Absolute |B − A| · Zero is black."
            };
            (
                image(stages, b, Some(a)),
                format!("{} · {}", operator, caption(stages, true, Some(b), plane)),
            )
        } else {
            (
                None,
                "Difference unavailable. The stages must share sample units and geometry."
                    .to_owned(),
            )
        }
    } else {
        (
            b.and_then(|b| image(stages, b, None)),
            caption(stages, true, b, plane),
        )
    };
    view.set_visual_a(a_image.is_some());
    view.set_visual_b(b_image.is_some());
    view.set_image_a(a_image.unwrap_or_default());
    view.set_image_b(b_image.unwrap_or_default());
    view.set_label_a(label_a.into());
    view.set_label_b(label_b.into());

    let (x, y) = stages.sample.map_or_else(
        || (SharedString::default(), SharedString::default()),
        |(x, y)| (x.to_string().into(), y.to_string().into()),
    );
    view.set_sample_x(x);
    view.set_sample_y(y);
    render_readout(view, stages.sample, a, b, plane);
}

fn status(doc: &Document) -> String {
    let stages = &doc.stages;
    if let Some(notice) = &stages.notice {
        return notice.clone();
    }
    if stages.stopping {
        return "Stopping stage capture. The current decode unit must finish.".to_owned();
    }
    if stages.busy() {
        return "Decoding the selected frame and its prerequisites…".to_owned();
    }
    if let Some(evidence) = &stages.evidence {
        let coverage = match evidence.coverage {
            BlockCoverage::Complete => "Replay complete.",
            BlockCoverage::Truncated => "A capture limit stopped the replay.",
            BlockCoverage::Cancelled => "Stage capture stopped.",
            BlockCoverage::FrameNotFound => "No admitted coded frame matched this offset.",
            BlockCoverage::Unavailable => "The decoder could not capture stages for this frame.",
            BlockCoverage::DecodeFailed => "Decode failed during stage capture.",
            BlockCoverage::AllocationFailed => {
                "The decoder could not allocate space for stage capture."
            }
        };
        return format!(
            "{coverage} Coded byte {}. Replay read {} bytes.",
            evidence.query.coded_offset, evidence.replayed_through
        );
    }
    if doc.block_query_ready() {
        "Capture up to 32 × 32 samples on one plane. Studio may decode earlier frames.".to_owned()
    } else {
        "Select an indexed coded frame in the timeline or OBU list.".to_owned()
    }
}

const fn label(stage: DecodeStage) -> &'static str {
    match stage {
        DecodeStage::Prediction => "Prediction",
        DecodeStage::Residual => "Decoded residual",
        DecodeStage::Reconstruction => "Pre-filter reconstruction",
        DecodeStage::Deblock => "Deblock",
        DecodeStage::Cdef => "CDEF",
        DecodeStage::Ccso => "CCSO",
        DecodeStage::Restoration => "Loop restoration",
        DecodeStage::Gdf => "Guided detail filter",
        DecodeStage::Reference => "Reference reconstruction",
        DecodeStage::Grain => "Grain presentation",
    }
}

const fn availability(status: StageStatus) -> &'static str {
    match status {
        StageStatus::Applied => "Applied",
        StageStatus::Skipped => "Skipped",
        StageStatus::Unsupported => "Unsupported",
        StageStatus::Uncaptured => "Uncaptured",
    }
}

fn caption(stages: &Stages, right: bool, stage: Option<&StageSamples>, plane: &str) -> String {
    let name = label(DecodeStage::ALL[stages.selections[usize::from(right)]]);
    let side = if right { "B" } else { "A" };
    let Some(stage) = stage else {
        return format!("{side} · {name} · Uncaptured");
    };
    let Some(region) = stage.region else {
        return format!("{side} · {name} · {}", availability(stage.status));
    };
    let SampleRect {
        x,
        y,
        width,
        height,
    } = stages.crop(region);
    format!(
        "{side} · {name} · {}. {}. {plane} · Plane depth {} bits · ({x}, {y}) · {width} × {height}",
        availability(stage.status),
        units(stage.stage),
        stage.bit_depth
    )
}

const fn units(stage: DecodeStage) -> &'static str {
    match stage {
        DecodeStage::Residual => "Signed decoded residual after transform rounding",
        DecodeStage::Prediction => "Prediction samples before clipped addition",
        DecodeStage::Grain => "Grain on direct display output",
        _ => "Unsigned reconstruction samples",
    }
}

fn image(stages: &Stages, stage: &StageSamples, subtract: Option<&StageSamples>) -> Option<Image> {
    let r = stages.crop(stage.region?);
    let mut buffer = SharedPixelBuffer::<Rgb8Pixel>::new(r.width, r.height);
    let mut pixels = buffer.make_mut_slice().iter_mut();
    let max = (1_i64 << stage.bit_depth) - 1;
    let gain = if stages.mode == 0 || subtract.is_some() {
        1_i64 << (2 * stages.gain)
    } else {
        1
    };
    for y in r.y..r.y + r.height {
        for x in r.x..r.x + r.width {
            let mut value = i64::from(stage.sample(x, y)?);
            if let Some(a) = subtract {
                value -= i64::from(a.sample(x, y)?);
            }
            value *= gain;
            let display = match (subtract, stages.mode, stage.stage.is_residual()) {
                (Some(_), 2, _) => value.abs() * 255 / max,
                (Some(_), _, _) | (None, _, true) => 128 + value * 127 / max,
                _ => value * 255 / max,
            };
            let gray = u8::try_from(display.clamp(0, 255)).ok()?;
            *pixels.next()? = Rgb8Pixel {
                r: gray,
                g: gray,
                b: gray,
            };
        }
    }
    Some(Image::from_rgb8(buffer))
}

fn render_readout(
    view: &StageInspector<'_>,
    point: Option<(u32, u32)>,
    a: Option<&StageSamples>,
    b: Option<&StageSamples>,
    plane: &str,
) {
    let Some((x, y)) = point else {
        view.set_sample_heading("Enter a nonnegative column and row.".into());
        view.set_sample_values(model(Vec::new()));
        view.set_sample_text("".into());
        view.set_can_copy(false);
        return;
    };
    let left = a.and_then(|a| a.sample(x, y));
    let right = b.and_then(|b| b.sample(x, y));
    let row = |label: &str, value: Option<i64>| StageValue {
        label: label.into(),
        value: value.map_or_else(|| "Unavailable".into(), |number| number.to_string().into()),
    };
    let mut values = vec![
        row("A", left.map(i64::from)),
        row("B", right.map(i64::from)),
    ];
    if let Some(((left, right), _)) = left
        .zip(right)
        .zip(a.zip(b).filter(|(a, b)| a.can_compare(b)))
    {
        let delta = i64::from(right) - i64::from(left);
        values.push(row("B − A", Some(delta)));
        values.push(row("|B − A|", Some(delta.abs())));
    }
    let heading = format!("{plane} ({x}, {y})");
    let mut text = heading.clone();
    for value in &values {
        let _ = write!(text, "\n{} {}", value.label, value.value);
    }
    view.set_sample_heading(heading.into());
    view.set_sample_values(model(values));
    view.set_sample_text(text.into());
    view.set_can_copy(left.is_some() || right.is_some());
}
