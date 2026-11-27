// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Displays the exact coefficient table for the selected transform.

use slint::ComponentHandle;
use studio_domain::blocks::{BlockCoefficientDomain, BlockCoefficients};
use studio_domain::preview::SamplePlane;

use super::refill;
use crate::analysis::block_overlay::Overlay;
use crate::ui::{AppWindow, CoefficientRow, CoefficientView};

const ROWS: usize = 6;

pub(super) fn render(window: &AppWindow, overlay: &Overlay) {
    let view = window.global::<CoefficientView>();
    let selected = overlay.transform_selection().is_some();
    view.set_has_transform(selected);
    view.set_can_read(selected && !overlay.busy());
    let Some((capture, values)) = overlay.coefficients() else {
        view.set_active(false);
        view.set_report("".into());
        view.set_rows(slint::ModelRc::default());
        return;
    };
    view.set_active(true);
    let width = usize::from(values.width);
    let first = overlay.coefficient_index / ROWS * ROWS;
    let rows: Vec<_> = (first..values.quantized.len())
        .take(ROWS)
        .map(|i| CoefficientRow {
            index: i32::try_from(i).unwrap_or_default(),
            x: (i % width).to_string().into(),
            y: (i / width).to_string().into(),
            scan: values
                .scan
                .as_ref()
                .map_or_else(|| "—".into(), |_| capture.scan_at[i].to_string())
                .into(),
            quantized: values.quantized[i].to_string().into(),
            dequantized: values.dequantized[i].to_string().into(),
            selected: i == overlay.coefficient_index,
        })
        .collect();
    if let Some(rows) = refill(&view.get_rows(), rows) {
        view.set_rows(rows);
    }
    view.set_count(i32::try_from(values.quantized.len()).unwrap_or_default());
    view.set_position(
        format!(
            "Coefficient {} of {}",
            overlay.coefficient_index + 1,
            values.quantized.len()
        )
        .into(),
    );

    render_metadata(&view, values);
    view.set_report(capture.report.clone());
}

fn render_metadata(view: &CoefficientView<'_>, values: &BlockCoefficients) {
    let plane = match values.id.plane {
        SamplePlane::Y => "Y",
        SamplePlane::U => "U",
        SamplePlane::V => "V",
    };
    let domain = match values.domain {
        BlockCoefficientDomain::Plane => format!("{plane} plane"),
        BlockCoefficientDomain::CrossComponent { mode } => {
            let component = match values.id.plane {
                SamplePlane::U => "C1",
                SamplePlane::V => "C2",
                SamplePlane::Y => "Cross-component",
            };
            format!("{component} ({plane} slot) before inverse CCTX, mode {mode}")
        }
    };
    let scan = match (&values.scan, values.forward_scan) {
        (None, _) => "No coefficient scan",
        (Some(_), true) => "Forward skip coding uses a scan suffix",
        (Some(_), false) => "Ordinary coding uses a scan prefix",
    };
    view.set_geometry(
        format!(
            "{} × {} coefficients · {domain} · {scan} · Scan extent {}",
            values.width, values.height, values.eob
        )
        .into(),
    );
    let quantizer = values.quantization.map_or_else(
        || "All-zero transform. The decoder skipped dequantization.".to_owned(),
        |q| {
            let matrix = q.matrix.map_or_else(
                || "No matrix".to_owned(),
                |m| {
                    let source = if m.user_defined {
                        "stream-defined"
                    } else {
                        "built-in"
                    };
                    format!("Matrix {}, {source}", m.level)
                },
            );
            format!(
                "{}-bit · Block qindex {} · DC and AC (eighths) {} / {} · Divisor {} · {matrix}",
                q.bit_depth, q.qindex, q.dc_quant, q.ac_quant, q.denominator
            )
        },
    );
    let tcq = if values.use_tcq {
        "TCQ used"
    } else {
        "TCQ unused"
    };
    let secondary = match values.secondary {
        None => "No secondary transform syntax".to_owned(),
        Some(s) if s.sec_tx_type == 0 => "SecTxType 0 · Secondary transform inactive".to_owned(),
        Some(s) => s.most_probable_stx_set.map_or_else(
            || format!("SecTxType {}", s.sec_tx_type),
            |set| {
                format!(
                    "SecTxType {} · Secondary transform set {set}",
                    s.sec_tx_type
                )
            },
        ),
    };
    let parity = if values.dc_parity_hidden {
        "DC magnitude parity derived from other levels"
    } else {
        "DC parity hiding unused"
    };
    view.set_quantizer(format!("{quantizer}\n{tcq} · {secondary} · {parity}").into());
}
