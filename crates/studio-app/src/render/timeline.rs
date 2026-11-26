// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Presents the bounded timeline page and relays view changes.

use std::fmt::Write;
use std::path::Path;

use slint::{ComponentHandle, SharedString};
use studio_domain::timeline::{
    ByteDistribution, TimelineBucket, TimelineClock, TimelineEntry, TimelinePage,
    TimelineQuantizer, TimelineSelection, TimelineTarget, TimelineTime, TimelineView,
};

use crate::analysis::{Analysis, Input, Phase};
use crate::present::count;
use crate::ui::{AppWindow, TimelineBar, TimelineUi};

const BUCKETS: u32 = 64;

pub(super) fn render(window: &AppWindow, analysis: &Analysis) {
    let ui = window.global::<TimelineUi>();
    let doc = analysis.shown();
    ui.set_available(doc.is_some_and(|doc| matches!(doc.phase(), Phase::Open(_))));
    let Some(doc) = doc else {
        ui.set_built(false);
        ui.set_exact_statistics(SharedString::new());
        return;
    };
    let timeline = doc.timeline();
    let page = timeline.page();
    ui.set_built(timeline.shown());
    ui.set_can_build(!timeline.busy());
    ui.set_can_cancel(timeline.can_cancel());
    ui.set_status(
        timeline
            .notice()
            .unwrap_or_else(|| status(page, timeline.busy()))
            .into(),
    );
    let range = timeline.range();
    let view = range.view;
    let first = range.first;
    let limit = range.limit;
    ui.set_order(i32::from(view == TimelineView::Output));
    ui.set_first(first.to_string().into());
    ui.set_count(limit.to_string().into());
    ui.set_scope(scope(view).into());
    let selection = timeline.selection().copied();
    ui.set_selected(selection.is_some());
    ui.set_selected_index(
        selection
            .and_then(|item| ordinal(item, view))
            .map_or_else(String::new, |index| index.to_string())
            .into(),
    );
    ui.set_selection_detail(selection.map_or_else(String::new, selection_line).into());
    render_page(
        &ui,
        page.filter(|page| page.view == view),
        first,
        limit,
        selection,
        doc.path(),
    );
}

fn render_page(
    ui: &TimelineUi<'_>,
    page: Option<&TimelinePage>,
    first: u64,
    limit: u64,
    selection: Option<TimelineSelection>,
    source: &Path,
) {
    let total = page.map_or(0, |page| page.total);
    let visible = total.saturating_sub(first).min(limit);
    ui.set_total(total.to_string().into());
    ui.set_can_pan_back(first > 0 && total > 0);
    ui.set_can_pan_forward(first.saturating_add(limit) < total);
    ui.set_can_zoom_in(visible > 1);
    ui.set_can_zoom_out(visible < total);
    ui.set_can_go_to_time(
        page.is_some_and(|page| page.complete && page.verified && page.clock == TimelineClock::Ivf),
    );
    let Some(page) = page.filter(|page| {
        page.first == first
            && page.buckets.iter().map(|bucket| bucket.count).sum::<u64>() == visible
    }) else {
        ui.set_range_label(SharedString::new());
        ui.set_summary(SharedString::new());
        ui.set_bars(super::model(Vec::new()));
        ui.set_exact_statistics(SharedString::new());
        return;
    };
    let selected = selection.and_then(|selection| ordinal(selection, page.view));
    let maximum = page.bytes.map_or(1, |bytes| bytes.max.max(1));
    let mut sequence = None;
    let bars = page
        .buckets
        .iter()
        .map(|bucket| {
            let entry_sequence = bucket.entry.map(|entry| entry.sequence_offset);
            let boundary = entry_sequence.is_some()
                && (sequence.is_some() && sequence != entry_sequence || bucket.first == 0);
            sequence = entry_sequence;
            bar(bucket, maximum, selected, boundary)
        })
        .collect();
    if let Some(bars) = super::refill(&ui.get_bars(), bars) {
        ui.set_bars(bars);
    }
    let label = range_label(page);
    ui.set_range_label(label.clone().into());
    let exact_entry = page
        .buckets
        .iter()
        .filter_map(|bucket| bucket.entry)
        .find(|entry| Some(entry.ordinal) == selected);
    if let Some(entry) = exact_entry {
        ui.set_selection_detail(entry_detail(entry).into());
    }
    let summary = statistics(page);
    ui.set_summary(summary.clone().into());
    ui.set_exact_statistics(report(page, &label, &summary, selection, source).into());
}

fn status(page: Option<&TimelinePage>, busy: bool) -> &'static str {
    match page {
        Some(page) if page.complete && page.verified => "Timeline complete. Source verified.",
        Some(page) if page.complete => {
            "Timeline complete. Full source verification is unavailable."
        }
        Some(_) if busy => "Reading picture identities. The timeline shows partial coverage.",
        Some(_) => "The timeline shows partial coverage.",
        None if busy => "Reading picture identities…",
        None => "Build a timeline to inspect coded frames and output pictures.",
    }
}

const fn scope(view: TimelineView) -> &'static str {
    match view {
        TimelineView::Coded => {
            "Coded frame OBU bytes include continuation OBUs. Bars show maximum, with mean and minimum marks."
        }
        TimelineView::Output => {
            "Output order counts each display OBU once. Bars show maximum, with mean and minimum marks."
        }
    }
}

fn range_label(page: &TimelinePage) -> String {
    let order = match page.view {
        TimelineView::Coded => "Coded",
        TimelineView::Output => "Output",
    };
    page.buckets.last().map_or_else(
        || format!("No {order} rows in this range."),
        |bucket| {
            format!(
                "{order} {}–{} · {} indexed",
                count(page.first),
                count(bucket.first.saturating_add(bucket.count.saturating_sub(1))),
                count(page.total),
            )
        },
    )
}

fn statistics(page: &TimelinePage) -> String {
    let rows: u64 = page.buckets.iter().map(|bucket| bucket.count).sum();
    let distribution = distribution_line(page.bytes, rows);
    let file = page.file_bytes;
    format!(
        "{distribution}. File {} bytes · framing {} · unclassified {}. OBU {} bytes · assigned {} · other OBU {}. {}",
        count(file.file),
        count(file.framing),
        count(file.unclassified),
        count(file.obu),
        count(file.assigned_obu),
        count(file.unassigned_obu),
        rate_line(page)
    )
}

fn rate_line(page: &TimelinePage) -> String {
    if let Some(rate) = page.rate {
        return format!(
            "Rate {} bit/s over the output timestamp window.",
            fraction(rate)
        );
    }
    let reason = match (page.view, page.clock) {
        (TimelineView::Coded, _) => "Switch to output order to inspect the display OBU rate.",
        (_, TimelineClock::Unavailable) => {
            "Bitrate unavailable. There is no absolute output clock."
        }
        (_, TimelineClock::Discontinuous) => {
            "Bitrate unavailable. The output clock is discontinuous."
        }
        (_, TimelineClock::Ivf) => "Bitrate unavailable. This range needs a valid end timestamp.",
        (_, TimelineClock::Conflicting) => {
            "Bitrate unavailable. The IVF timestamps do not identify each temporal unit."
        }
    };
    reason.into()
}

fn distribution_line(distribution: Option<ByteDistribution>, rows: u64) -> String {
    match distribution {
        Some(bytes) if rows == 1 => format!("{} bytes", count(bytes.sum)),
        Some(bytes) if rows > 1 => format!(
            "{} bytes total · min {} · max {} · mean {} bytes",
            count(bytes.sum),
            count(bytes.min),
            count(bytes.max),
            fraction(TimelineTime {
                num: u128::from(bytes.sum),
                den: rows
            })
        ),
        _ => "Byte statistics unavailable".into(),
    }
}

fn fraction(time: TimelineTime) -> String {
    if time.den == 1 {
        time.num.to_string()
    } else {
        format!("{}/{}", time.num, time.den)
    }
}

fn entry_detail(entry: TimelineEntry) -> String {
    let samples = entry.output_ordinal.map_or_else(
        || "No output indexed".into(),
        |output| format!("Output {output}"),
    );
    let quantizer = match entry.quantizer {
        TimelineQuantizer::HeaderBase { value } => {
            format!("Header base quantizer {value} (signaled)")
        }
        TimelineQuantizer::DerivedBase { value } => format!("Base quantizer {value} (derived)"),
        TimelineQuantizer::InheritedBase { value } => format!("Base quantizer {value} (inherited)"),
        TimelineQuantizer::Unavailable => "Header base quantizer unavailable".into(),
    };
    let access = if entry.random_access {
        " · Random access"
    } else {
        ""
    };
    let time = entry.time.map_or_else(
        || "Time unavailable".into(),
        |time| format!("{} s", fraction(time)),
    );
    let interval = entry.interval.map_or_else(String::new, |interval| {
        format!(" · Signaled interval {} s", fraction(interval))
    });
    let showing = entry.showing_offset.map_or_else(String::new, |offset| {
        format!(" · Display OBU byte {offset}")
    });
    format!(
        "{} bytes · Coded {} · {samples} · Coded OBU byte {}{showing} · Sequence byte {} · {time}{interval} · {quantizer}{access}",
        entry.bytes, entry.coded_ordinal, entry.coded_offset, entry.sequence_offset
    )
}

fn selection_line(selection: TimelineSelection) -> String {
    let output = selection.output_ordinal.map_or_else(
        || "No output indexed".into(),
        |output| format!("Output {output}"),
    );
    format!(
        "Coded {} · {output} · Coded OBU byte {}",
        selection.coded_ordinal, selection.coded_offset
    )
}

const fn ordinal(selection: TimelineSelection, view: TimelineView) -> Option<u64> {
    match view {
        TimelineView::Coded => Some(selection.coded_ordinal),
        TimelineView::Output => selection.output_ordinal,
    }
}

fn bucket_label(bucket: &TimelineBucket) -> String {
    if bucket.count == 1 {
        count(bucket.first)
    } else {
        format!(
            "{}–{}",
            count(bucket.first),
            count(bucket.first.saturating_add(bucket.count.saturating_sub(1)))
        )
    }
}

#[allow(clippy::cast_precision_loss)]
fn bar(bucket: &TimelineBucket, scale: u64, selected: Option<u64>, boundary: bool) -> TimelineBar {
    let (minimum, maximum, mean) = bucket.bytes.map_or((0.0, 0.0, 0.0), |bytes| {
        let scale = scale as f32;
        (
            bytes.min as f32 / scale,
            bytes.max as f32 / scale,
            bytes.sum as f32 / bucket.count.max(1) as f32 / scale,
        )
    });
    let mut marker = String::new();
    if boundary {
        marker.push('|');
    }
    match bucket.entry {
        Some(entry) if entry.random_access => marker.push('◆'),
        Some(entry) if entry.output_ordinal.is_none() => marker.push('–'),
        _ if bucket.bytes.is_none() => marker.push('?'),
        _ => {}
    }
    TimelineBar {
        first: bucket.first.to_string().into(),
        count: bucket.count.to_string().into(),
        label: bucket_label(bucket).into(),
        detail: bucket
            .entry
            .map_or_else(
                || distribution_line(bucket.bytes, bucket.count),
                entry_detail,
            )
            .into(),
        minimum,
        maximum,
        mean,
        known: bucket.bytes.is_some(),
        selected: selected.is_some_and(|i| i >= bucket.first && i - bucket.first < bucket.count),
        marker: marker.into(),
    }
}

fn report(
    page: &TimelinePage,
    label: &str,
    summary: &str,
    selection: Option<TimelineSelection>,
    source: &Path,
) -> String {
    let mut report = format!(
        "Source {}\n{label}\n{}\n{}\n{summary}\n",
        source.display(),
        status(Some(page), false),
        scope(page.view)
    );
    if let Some(selection) = selection {
        let _ = writeln!(report, "Selection: {}", selection_line(selection));
    }
    for bucket in &page.buckets {
        let detail = bucket.entry.map_or_else(
            || distribution_line(bucket.bytes, bucket.count),
            entry_detail,
        );
        let _ = writeln!(report, "{}\t{detail}", bucket_label(bucket));
    }
    report
}

pub(crate) fn bind(window: &AppWindow, send: impl Fn(Input) + Clone + 'static) {
    let ui = window.global::<TimelineUi>();
    let on = |input: Input| {
        let send = send.clone();
        move || send(input.clone())
    };
    ui.on_build(on(Input::TimelineBuild));
    ui.on_cancel(on(Input::TimelineCancel));
    ui.on_findings(on(Input::TimelineLinkFindings));
    ui.on_step({
        let send = send.clone();
        move |step| send(Input::TimelineStep(step))
    });
    ui.on_reorder({
        let send = send.clone();
        move |order| send(range(view(order), 0, u64::MAX))
    });
    ui.on_go_to({
        let send = send.clone();
        let window = window.as_weak();
        move |kind, text| {
            if let Some(input) = target(kind, &text) {
                send(Input::TimelineSelect(input));
            } else if let Some(window) = window.upgrade() {
                window.global::<TimelineUi>().set_status(
                    if kind == 2 {
                        "Enter non-negative seconds or a fraction with a positive denominator."
                    } else {
                        "Enter a non-negative frame index."
                    }
                    .into(),
                );
            }
        }
    });
    ui.on_pick({
        let send = send.clone();
        let window = window.as_weak();
        move |slot| {
            if let Some(window) = window.upgrade() {
                let ui = window.global::<TimelineUi>();
                if let Some(input) = pick(&ui, slot) {
                    send(input);
                }
            }
        }
    });
    ui.on_pan({
        let send = send.clone();
        let window = window.as_weak();
        move |direction| {
            if let Some(window) = window.upgrade() {
                let ui = window.global::<TimelineUi>();
                if let Some(input) = change_range(&ui, direction, false) {
                    send(input);
                }
            }
        }
    });
    ui.on_zoom({
        let window = window.as_weak();
        move |direction| {
            if let Some(window) = window.upgrade() {
                let ui = window.global::<TimelineUi>();
                if let Some(input) = change_range(&ui, direction, true) {
                    send(input);
                }
            }
        }
    });
}

fn pick(ui: &TimelineUi<'_>, slot: i32) -> Option<Input> {
    use slint::Model;
    let bucket = ui.get_bars().row_data(usize::try_from(slot).ok()?)?;
    let first = bucket.first.parse().ok()?;
    let count = bucket.count.parse().ok()?;
    let view = view(ui.get_order());
    if count == 1 {
        let target = match view {
            TimelineView::Coded => TimelineTarget::Coded { index: first },
            TimelineView::Output => TimelineTarget::Output { index: first },
        };
        Some(Input::TimelineSelect(target))
    } else {
        Some(range(view, first, count))
    }
}

fn change_range(ui: &TimelineUi<'_>, direction: i32, zoom: bool) -> Option<Input> {
    let first: u64 = ui.get_first().parse().ok()?;
    let limit: u64 = ui.get_count().parse().ok()?;
    let total: u64 = ui.get_total().parse().ok()?;
    let visible = total.saturating_sub(first).min(limit);
    if visible == 0 || direction == 0 {
        return None;
    }
    let (first, limit) = if zoom {
        let size = if direction > 0 {
            visible.div_ceil(2)
        } else {
            visible.saturating_mul(2).min(total)
        };
        let anchor = ui
            .get_selected_index()
            .parse::<u64>()
            .ok()
            .filter(|index| *index >= first && *index - first < visible)
            .unwrap_or_else(|| first + visible / 2);
        (anchor.saturating_sub(size / 2).min(total - size), size)
    } else if direction < 0 {
        (first.saturating_sub(limit), limit)
    } else {
        (first.saturating_add(limit).min(total - 1), limit)
    };
    Some(range(view(ui.get_order()), first, limit))
}

const fn range(view: TimelineView, first: u64, limit: u64) -> Input {
    Input::TimelineRange {
        view,
        first,
        limit,
        buckets: BUCKETS,
    }
}

const fn view(order: i32) -> TimelineView {
    if order == 1 {
        TimelineView::Output
    } else {
        TimelineView::Coded
    }
}

fn target(kind: i32, text: &str) -> Option<TimelineTarget> {
    let text = text.trim();
    match kind {
        0 => Some(TimelineTarget::Coded {
            index: text.parse().ok()?,
        }),
        1 => Some(TimelineTarget::Output {
            index: text.parse().ok()?,
        }),
        2 => Some(TimelineTarget::Time {
            time: parse_seconds(text)?,
        }),
        _ => None,
    }
}

fn parse_seconds(text: &str) -> Option<TimelineTime> {
    if let Some((num, den)) = text.split_once('/') {
        return Some(TimelineTime {
            num: num.trim().parse().ok()?,
            den: den.trim().parse::<u64>().ok().filter(|den| *den != 0)?,
        });
    }
    if let Some((whole, tail)) = text.split_once('.') {
        let den = 10_u64.checked_pow(u32::try_from(tail.len()).ok()?)?;
        let num = whole
            .parse::<u128>()
            .ok()?
            .checked_mul(u128::from(den))?
            .checked_add(tail.parse::<u128>().ok()?)?;
        return Some(TimelineTime { num, den });
    }
    Some(TimelineTime {
        num: text.parse().ok()?,
        den: 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_entry_preserves_fraction_and_rejects_invalid_time() {
        assert_eq!(
            parse_seconds("12.34"),
            Some(TimelineTime {
                num: 1234,
                den: 100
            })
        );
        assert_eq!(
            parse_seconds("24000/1001"),
            Some(TimelineTime {
                num: 24000,
                den: 1001
            })
        );
        assert_eq!(parse_seconds("1/0"), None);
        assert_eq!(parse_seconds("-1"), None);
        assert_eq!(parse_seconds("1.100000000000000000000"), None);
    }
}
