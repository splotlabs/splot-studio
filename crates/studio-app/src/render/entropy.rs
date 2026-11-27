// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Presents entropy observations without converting them into file-byte claims.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use slint::ComponentHandle;
use studio_domain::blocks::{
    BlockCoverage, BlockData, BlockEvidence, BlockPrediction, EntropyCoding, EntropyEvidence,
    EntropyScope, EntropySymbol, IntraPredictor, MICROBITS_PER_BIT,
};

use super::{blocks::rect, model};
use crate::analysis::entropy::{
    Entropy, EntropyInput, PAGE_ROWS, coding_name as coding, operation,
};
use crate::analysis::{Analysis, Input};
use crate::ui::{AppWindow, EntropyTableRow, EntropyView};

pub(crate) fn bind(window: &AppWindow, send: impl Fn(Input) + Clone + 'static) {
    let view = window.global::<EntropyView>();
    let action = |input: EntropyInput| {
        let send = send.clone();
        move || send(Input::Entropy(input.clone()))
    };
    view.on_read(action(EntropyInput::Read));
    view.on_read_block(action(EntropyInput::ReadBlock));
    view.on_read_transform(action(EntropyInput::ReadTransform));
    view.on_stop(action(EntropyInput::Stop));
    let integer = |input: fn(i32) -> EntropyInput| {
        let send = send.clone();
        move |value| send(Input::Entropy(input(value)))
    };
    view.on_filter(integer(EntropyInput::Kind));
    view.on_pick(integer(EntropyInput::Pick));
    view.on_move(integer(EntropyInput::Move));
    view.on_search(move |value| send(Input::Entropy(EntropyInput::Search(value.to_string()))));
}

pub(super) fn render(window: &AppWindow, analysis: &Analysis) {
    let view = window.global::<EntropyView>();
    let selection = analysis.entropy_selection();
    let state = selection.and_then(|selection| analysis.entropy_for(selection));
    let report = state.and_then(|state| state.evidence.as_deref());
    let entropy = report.and_then(|report| report.entropy.as_ref());
    let selection_scope = selection.map(|selection| selection.scope);
    let scope = state.and_then(|state| state.scope).or(selection_scope);
    let busy = state.is_some_and(Entropy::busy);
    let stopping = state.is_some_and(|state| state.stopping);
    let filtered = state.map_or_else(Vec::new, |state| state.filtered());
    let selected = state.map_or(0, |state| state.selected);
    let first = state.map_or(0, Entropy::first);
    let symbol = filtered.get(selected).copied();
    let tool_stats = selection
        .zip(analysis.overlay().viewport.as_deref())
        .filter(|(selection, viewport)| viewport.frame == Some(selection.frame))
        .and_then(|(_, viewport)| prediction_stats(viewport))
        .unwrap_or_else(|| {
            "Choose a coding block in the Tool Use overlay to see captured prediction counts."
                .to_owned()
        });
    let status = if let Some(report) = report {
        let count = entropy.map_or(0, |entropy| entropy.symbols.len());
        if count > 0 && filtered.is_empty() {
            format!(
                "{} No symbols match the filters.",
                coverage_status(report.coverage, count)
            )
        } else {
            coverage_status(report.coverage, count)
        }
    } else if let Some(notice) = state.and_then(|state| state.notice.as_deref()) {
        notice.to_owned()
    } else if stopping {
        "Stopping entropy capture…".to_owned()
    } else if busy {
        "Decoding the selected frame and its prerequisites…".to_owned()
    } else if selection.is_some() {
        "Read entropy symbols for this selection. Studio may decode earlier frames.".to_owned()
    } else {
        "Select a coding block or transform in the picture overlay.".to_owned()
    };
    view.set_status(status.into());
    view.set_scope(scope.map_or_else(String::new, scope_text).into());
    view.set_accounting(
        report
            .zip(entropy)
            .map_or_else(String::new, |(report, entropy)| {
                totals(entropy, report.coverage == BlockCoverage::Complete)
            })
            .into(),
    );
    view.set_statistics(
        if scope.is_some() {
            tool_stats.as_str()
        } else {
            ""
        }
        .into(),
    );
    view.set_detail(symbol.map_or_else(String::new, symbol_detail).into());
    let digest = analysis
        .shown()
        .and_then(crate::analysis::Document::summary)
        .and_then(|summary| summary.digest.as_deref())
        .map(|digest| digest.sha256.as_str());
    set_clipboard(&view, report, symbol, digest, &tool_stats);
    let showing_transform = matches!(scope, Some(EntropyScope::Transform(_)));
    let selected_transform = matches!(selection_scope, Some(EntropyScope::Transform(_)));
    view.set_can_read(selection.is_some() && !busy && (!showing_transform || selected_transform));
    view.set_can_read_block((showing_transform || selected_transform) && !busy);
    view.set_read_label(
        if selected_transform {
            "Read Selected Transform"
        } else {
            "Read Symbols"
        }
        .into(),
    );
    view.set_can_read_transform(
        !busy
            && matches!(scope, Some(EntropyScope::Block(_)))
            && symbol.is_some_and(|symbol| symbol.transform.is_some()),
    );
    view.set_busy(busy);
    view.set_stopping(stopping);
    view.set_query(state.map_or("", |state| state.search.as_str()).into());
    view.set_kind(state.map_or(0, |state| state.kind));
    view.set_first(i32::try_from(first).unwrap_or_default());
    view.set_count(i32::try_from(filtered.len()).unwrap_or_default());
    view.set_page_size(i32::try_from(PAGE_ROWS).unwrap_or_default());
    view.set_rows(model(
        filtered
            .into_iter()
            .enumerate()
            .skip(first)
            .take(PAGE_ROWS)
            .map(|(index, symbol)| symbol_row(symbol, index == selected))
            .collect(),
    ));
}

fn symbol_row(symbol: &EntropySymbol, selected: bool) -> EntropyTableRow {
    let operation = operation(symbol);
    let cost = cost_text(symbol.estimated_microbits);
    EntropyTableRow {
        ordinal: symbol.ordinal.to_string().into(),
        operation: operation.into(),
        value: symbol.value.to_string().into(),
        accessible: format!(
            "Ordinal {}. {}. Value {}. Estimated bits {cost}.",
            symbol.ordinal, operation, symbol.value,
        )
        .into(),
        cost: cost.into(),
        selected,
    }
}

fn scope_text(scope: EntropyScope) -> String {
    let block = scope.block();
    let tree = match block.tree {
        studio_domain::blocks::BlockTree::Shared => "Shared",
        studio_domain::blocks::BlockTree::Luma => "Luma",
        studio_domain::blocks::BlockTree::Chroma => "Chroma",
    };
    let block = format!(
        "Tile {}. {tree} tree. Coding block {}.",
        block.tile,
        rect(block.coded)
    );
    match scope {
        EntropyScope::Block(_) => block,
        EntropyScope::Transform(id) => {
            format!("{block} {:?} transform {}.", id.plane, rect(id.coded))
        }
    }
}

fn coverage_status(coverage: BlockCoverage, count: usize) -> String {
    match coverage {
        BlockCoverage::Complete => format!("{count} captured symbols in this scope."),
        BlockCoverage::Truncated => {
            format!("Partial capture. A limit stopped capture after {count} symbols.")
        }
        BlockCoverage::Cancelled => {
            format!("Capture stopped. {count} partial symbols are available.")
        }
        BlockCoverage::FrameNotFound => "The decoder did not find the selected frame.".to_owned(),
        BlockCoverage::Unavailable => {
            "The decoder could not provide entropy evidence for this scope.".to_owned()
        }
        BlockCoverage::DecodeFailed => {
            format!("The decoder failed during capture. {count} partial symbols are available.")
        }
        BlockCoverage::AllocationFailed => format!(
            "There was insufficient space for capture. {count} partial symbols are available."
        ),
    }
}

fn cost_text(cost: Option<u32>) -> String {
    cost.map_or_else(
        || "Unavailable".to_owned(),
        |cost| format_bits(u64::from(cost)),
    )
}

fn format_bits(microbits: u64) -> String {
    let unit = u64::from(MICROBITS_PER_BIT);
    format!("{}.{:06}", microbits / unit, microbits % unit)
}

fn totals(evidence: &EntropyEvidence, complete: bool) -> String {
    let symbols = &evidence.symbols;
    let mut kinds = BTreeMap::<&str, (usize, usize, u64)>::new();
    let mut syntax = BTreeMap::<&str, usize>::new();
    let mut total = 0;
    let mut known = 0;
    let mut shared = 0;
    let mut shared_cost = 0;
    let mut shared_known = 0;
    for row in symbols {
        *syntax
            .entry(row.syntax.as_deref().unwrap_or("Unavailable"))
            .or_default() += 1;
        if matches!(evidence.scope, EntropyScope::Transform(_))
            && row.is_shared_between_transforms()
        {
            shared += 1;
            if let Some(cost) = row.estimated_microbits {
                shared_cost += u64::from(cost);
                shared_known += 1;
            }
            continue;
        }
        let kind = kinds.entry(coding(row.coding)).or_default();
        kind.0 += 1;
        if let Some(cost) = row.estimated_microbits {
            kind.1 += 1;
            kind.2 += u64::from(cost);
            total += u64::from(cost);
            known += 1;
        }
    }
    let captured_end = symbols
        .last()
        .and_then(EntropySymbol::end_ordinal)
        .unwrap_or(0);
    let span = format!(
        "Captured scope ordinals [0, {captured_end}). Observed end ordinal {}.",
        evidence.end_ordinal,
    );
    // A stopped capture holds only the first symbols of the owner. Its sum is a lower bound.
    let owner = if complete {
        "symbols for this owner."
    } else {
        "captured symbols. The capture is partial, so this sum is a lower bound for this owner."
    };
    let mut text = if known == 0 {
        format!("Estimated cost for this owner is unavailable. {span}\n")
    } else {
        format!(
            "{} estimated bits from {known} of {} {owner} {span}\n",
            format_bits(total),
            symbols.len() - shared
        )
    };
    if shared > 0 {
        let cost = if shared_known == 0 {
            "Estimated cost is unavailable.".to_owned()
        } else {
            format!(
                "{} estimated bits from {shared_known} of these symbols.",
                format_bits(shared_cost)
            )
        };
        let _ = writeln!(text, "{shared} shared U/V symbols. {cost}");
    }
    let _ = write!(
        text,
        "Tile payload {} bytes. This exact count covers the whole tile. Other shared syntax, headers, termination and wrapper overhead have no cost allocation in this view.",
        evidence.tile_payload_bytes,
    );
    for (kind, (count, known, cost)) in kinds {
        let cost = if known == 0 {
            "cost unavailable".to_owned()
        } else {
            format!(
                "{} estimated bits from the {known} with a known cost",
                format_bits(cost)
            )
        };
        let _ = write!(text, "\n{kind}. {count} captured, {cost}.");
    }
    if !syntax.is_empty() {
        text.push_str(if complete {
            "\nCaptured syntax counts"
        } else {
            "\nCaptured syntax counts. The capture is partial."
        });
        for (name, count) in syntax {
            let _ = write!(text, "\n{name}  {count}");
        }
    }
    text
}

fn symbol_detail(symbol: &EntropySymbol) -> String {
    let kind = coding(symbol.coding);
    let syntax = symbol.syntax.as_deref().unwrap_or("Unavailable");
    let mut detail = format!("{kind} symbol {}. Syntax {syntax}.", symbol.ordinal);
    if let Some(id) = symbol.transform {
        let _ = write!(detail, " {:?} transform {}.", id.plane, rect(id.coded));
    } else if symbol.is_shared_between_transforms() {
        detail.push_str(" Joint U/V syntax. The parent block owns this cost.");
    }

    if let Some(context) = &symbol.context {
        for (name, value) in context.names.split(',').zip(context.values) {
            let _ = write!(detail, " {name} = {value}.");
        }
    } else if symbol.coding == EntropyCoding::Cdf {
        detail.push_str(" The decoder supplied no context selectors for this read.");
    }
    if let Some(bits) = symbol.bypass_bits {
        let _ = write!(detail, " {bits} equiprobable bits.");
    } else {
        let _ = write!(
            detail,
            " Range {}. Chosen interval {} before renormalization. Cost is log₂(range / interval).",
            symbol.range, symbol.interval
        );
    }
    if let Some(cdf) = &symbol.cdf {
        let update = if cdf.updates { "updates" } else { "keeps" };
        let _ = write!(
            detail,
            " CDF cutoffs before the read, in Q15 units. {:?}. Adaptation count {}. Rate index {}. This read {update} the CDF.",
            cdf.cutoffs_q15, cdf.adaptation_count, cdf.rate_index,
        );
    } else if symbol.coding == EntropyCoding::Cdf {
        detail.push_str(" CDF before the read is unavailable.");
    }
    detail
}

fn copy_header(report: &BlockEvidence, evidence: &EntropyEvidence, digest: Option<&str>) -> String {
    let mut text = format!(
        "Entropy capture TSV 1\nSchema\t{}\nAV2 specification\t{}\nCodec revision\t{}\nScope\t{}\nCoverage\t{}\n",
        evidence.schema,
        evidence.spec,
        evidence.codec_revision,
        scope_text(evidence.scope),
        coverage_status(report.coverage, evidence.symbols.len()),
    );
    if let Some(digest) = digest {
        let _ = writeln!(text, "Source SHA-256\t{digest}");
    }
    let _ = writeln!(text, "Decode selection\t{:?}", report.query.selection);
    if let Some(frame) = report.frame {
        let _ = writeln!(
            text,
            "Coded offset\t{}\nSequence offset\t{}\nLayers (X/M/T)\t{}/{}/{}",
            frame.coded_offset,
            frame.sequence_offset,
            frame.layers.extended,
            frame.layers.embedded,
            frame.layers.temporal
        );
    }
    let _ = writeln!(
        text,
        "Tile payload bytes\t{}\nObserved end ordinal\t{}",
        evidence.tile_payload_bytes, evidence.end_ordinal
    );
    text.push_str("Ordinal\tSyntax\tContext\tTransform\tCoding\tDecoded value\tEstimated bits\tBypass bits\tRange\tInterval\tCDF Q15 cutoffs\tAdaptation count\tRate index\tUpdates\n");
    text
}

fn symbol_tsv(symbol: &EntropySymbol) -> String {
    let context = symbol.context.as_ref().map_or_else(String::new, |context| {
        context
            .names
            .split(',')
            .zip(context.values)
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join(",")
    });
    let transform = if symbol.is_shared_between_transforms() {
        "Shared U/V".to_owned()
    } else {
        symbol.transform.map_or_else(String::new, |id| {
            format!("{:?} {}", id.plane, rect(id.coded))
        })
    };
    let cdf = symbol.cdf.as_ref().map_or_else(
        || "\t\t\t".to_owned(),
        |cdf| {
            format!(
                "{:?}\t{}\t{}\t{}",
                cdf.cutoffs_q15, cdf.adaptation_count, cdf.rate_index, cdf.updates
            )
        },
    );
    let (bypass, range, interval) = symbol.bypass_bits.map_or_else(
        || {
            (
                String::new(),
                symbol.range.to_string(),
                symbol.interval.to_string(),
            )
        },
        |bits| (bits.to_string(), String::new(), String::new()),
    );
    format!(
        "{}\t{}\t{context}\t{transform}\t{}\t{}\t{}\t{bypass}\t{range}\t{interval}\t{cdf}",
        symbol.ordinal,
        symbol.syntax.as_deref().unwrap_or_default(),
        coding(symbol.coding),
        symbol.value,
        cost_text(symbol.estimated_microbits),
    )
}

fn set_clipboard(
    view: &EntropyView<'_>,
    report: Option<&BlockEvidence>,
    symbol: Option<&EntropySymbol>,
    digest: Option<&str>,
    statistics: &str,
) {
    let Some((report, evidence)) =
        report.and_then(|report| Some((report, report.entropy.as_ref()?)))
    else {
        view.set_copied_capture("".into());
        view.set_copied_symbol("".into());
        return;
    };
    let mut text = copy_header(report, evidence, digest);
    view.set_copied_symbol(
        symbol
            .map_or_else(String::new, |symbol| {
                format!("{text}{}\n", symbol_tsv(symbol))
            })
            .into(),
    );
    for symbol in &evidence.symbols {
        let _ = writeln!(text, "{}", symbol_tsv(symbol));
    }
    let _ = writeln!(text, "\nCaptured tool statistics\n{statistics}");
    view.set_copied_capture(text.into());
}

fn prediction_stats(report: &BlockEvidence) -> Option<String> {
    let mut count = 0;
    let mut unknown = 0;
    let mut tools = [0; 7];
    for record in &report.records {
        let BlockData::Coding {
            prediction,
            has_chroma,
            ..
        } = &record.data
        else {
            continue;
        };
        count += 1;
        let Some(prediction) = prediction else {
            unknown += 1;
            continue;
        };
        match prediction {
            BlockPrediction::Intra { luma, chroma } => {
                let predictors = [luma, chroma].into_iter().flatten();
                for (index, used) in [
                    predictors
                        .clone()
                        .any(|p| matches!(p, IntraPredictor::Palette { .. })),
                    predictors
                        .clone()
                        .any(|p| matches!(p, IntraPredictor::Dip { .. })),
                    predictors
                        .clone()
                        .any(|p| matches!(p, IntraPredictor::ChromaFromLuma { .. })),
                ]
                .into_iter()
                .enumerate()
                {
                    tools[index] += usize::from(used);
                }
                if (record.id.tree != studio_domain::blocks::BlockTree::Chroma && luma.is_none())
                    || (*has_chroma && chroma.is_none())
                {
                    unknown += 1;
                }
            }
            BlockPrediction::IntraBlockCopy => tools[3] += 1,
            BlockPrediction::Inter {
                inter_intra,
                bawp,
                adaptive_mvd,
                ..
            } => {
                tools[4] += usize::from(inter_intra.is_some());
                tools[5] += usize::from(*bawp);
                tools[6] += usize::from(*adaptive_mvd);
            }
        }
    }
    if count == 0 {
        return None;
    }
    let coverage = if report.coverage == BlockCoverage::Complete {
        "Complete"
    } else {
        "Partial"
    };
    let region = report.query.region.map_or_else(|| "Frame".to_owned(), rect);
    Some(format!(
        "{coverage} overlay capture for {region}. Observed prediction tools in {count} captured blocks. Some prediction details are unavailable for {unknown} of those blocks.\nPalette {}. Data-driven intra {}. Chroma from luma {}. Intra block copy {}. Inter-intra {}. Block-adaptive weighted prediction {}. Adaptive motion-vector differences {}.",
        tools[0], tools[1], tools[2], tools[3], tools[4], tools[5], tools[6],
    ))
}

#[cfg(test)]
mod tests {
    use studio_domain::blocks::{BlockId, BlockRect, BlockTree};

    use super::*;

    fn symbol(ordinal: u64, estimated_microbits: Option<u32>) -> EntropySymbol {
        EntropySymbol {
            ordinal,
            transform: None,
            syntax: None,
            context: None,
            coding: EntropyCoding::Cdf,
            value: 0,
            bypass_bits: None,
            cdf: None,
            estimated_microbits,
            range: 32768,
            interval: 16384,
        }
    }

    fn evidence(symbols: Vec<EntropySymbol>) -> EntropyEvidence {
        EntropyEvidence {
            schema: 1,
            spec: "1.0.0".to_owned(),
            codec_revision: "test".to_owned(),
            scope: EntropyScope::Block(BlockId {
                tile: 0,
                tree: BlockTree::Shared,
                coded: BlockRect {
                    x: 0,
                    y: 0,
                    width: 8,
                    height: 8,
                },
            }),
            tile_payload_bytes: 77,
            end_ordinal: 9,
            symbols,
        }
    }

    #[test]
    fn cost_sums_are_exact_in_microbits_and_name_their_coverage() {
        assert_eq!(format_bits(1_500_000), "1.500000");
        assert_eq!(format_bits(999_999), "0.999999");

        // 1 + 0.25 bits. The third symbol has no estimate and adds nothing.
        let capture = evidence(vec![
            symbol(0, Some(1_000_000)),
            symbol(1, Some(250_000)),
            symbol(2, None),
        ]);
        let complete = totals(&capture, true);
        assert!(
            complete.starts_with("1.250000 estimated bits from 2 of 3 symbols for this owner."),
            "{complete}"
        );
        assert!(complete.contains("Captured scope ordinals [0, 3). Observed end ordinal 9."));
        assert!(
            complete.contains("Tile payload 77 bytes. This exact count covers the whole tile.")
        );
        assert!(!complete.contains("partial"));

        let partial = totals(&capture, false);
        assert!(
            partial.starts_with("1.250000 estimated bits from 2 of 3 captured symbols."),
            "{partial}"
        );
        assert!(partial.contains("this sum is a lower bound for this owner"));
        assert!(partial.contains("Captured syntax counts. The capture is partial."));
    }
}
