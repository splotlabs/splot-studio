// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use std::cell::RefCell;

use slint::platform::Key;
use slint::{ComponentHandle, Model};
use studio_app::EntropyView;
use studio_app::analysis::{BlockOverlayInput, Input};

use super::{Rig, Scratch, overlays};

const RECTANGLE: &[u8] =
    include_bytes!("../../../studio-engine/tests/inspect/fixtures/syn-hrect-intra-64x64-q120.ivf");
// Lossless 64 × 64 luma and chroma blocks: each parses more than `MAX_ENTROPY_EVENTS` symbols.
const LOSSLESS: &[u8] = include_bytes!(
    "../../../studio-engine/tests/inspect/fixtures/syn-lossless-sdp-nondc-chroma-v-intra-64x64-10bit.ivf"
);

pub(super) fn inspect(rig: &mut Rig, scratch: &Scratch, clipboard: &RefCell<String>) {
    overlays::open(rig, scratch, "entropy symbols.ivf", RECTANGLE);
    rig.session.block_overlay_input(BlockOverlayInput::Mode(1));
    rig.idle("the coding block map");
    let pick = |rig: &mut Rig, y| {
        rig.session.block_overlay_input(BlockOverlayInput::Pick {
            x: 42,
            y,
            width: 160,
            height: 160,
        });
    };
    pick(rig, 42);
    rig.session.analysis_input(Input::EvidenceView(7));
    rig.idle("the entropy scope");
    assert!(view(rig).get_can_read());
    assert_eq!(view(rig).get_rows().row_count(), 0, "capture is opt-in");

    view(rig).invoke_read();
    rig.idle("the entropy symbols of the upper block");
    assert!(view(rig).get_scope().contains("Coding block (0, 0)"));
    assert!(view(rig).get_accounting().contains("estimated bits"));
    assert!(view(rig).get_accounting().contains("Tile payload"));
    assert!(view(rig).get_statistics().contains("captured blocks"));
    assert!(
        view(rig)
            .get_copied_capture()
            .starts_with("Entropy capture TSV 2\n")
    );
    assert!(view(rig).get_copied_capture().contains("Coverage\t"));

    view(rig).invoke_search("all_zero".into());
    rig.idle("the filtered transform start symbols");
    assert!(view(rig).get_rows().row_count() > 0);
    assert!(view(rig).get_can_read_transform());
    assert!(view(rig).get_detail().contains("Q15"));
    view(rig).invoke_read_transform();
    rig.idle("the owning transform");
    assert!(view(rig).get_scope().contains("transform"));
    let row = view(rig)
        .get_rows()
        .row_data(0)
        .unwrap_or_else(|| unreachable!("symbol"));
    assert_eq!(row.ordinal, "0");
    assert_eq!(row.operation, "all_zero");
    copy_with_keyboard(rig, clipboard);
    assert!(!view(rig).get_can_read_transform());
    assert!(view(rig).get_can_read_block());
    view(rig).invoke_read_block();
    rig.idle("the parent coding block");
    assert!(!view(rig).get_scope().contains(" transform"));
    assert!(view(rig).get_rows().row_count() > 0);

    pick(rig, 118);
    rig.idle("the new coding block");
    assert!(view(rig).get_scope().contains("Coding block (0, 32)"));
    assert_eq!(view(rig).get_rows().row_count(), 0);
    assert!(view(rig).get_copied_capture().is_empty());

    partial_capture(rig, scratch);
    read_next_part(rig);
    page_with_keys(rig, clipboard);
    stop_and_read_again(rig);
}

/// A capture that the event limit stopped says so in every view of it.
fn partial_capture(rig: &mut Rig, scratch: &Scratch) {
    use studio_domain::blocks::MAX_ENTROPY_EVENTS;

    overlays::open(rig, scratch, "lossless entropy.ivf", LOSSLESS);
    rig.session.block_overlay_input(BlockOverlayInput::Mode(1));
    rig.idle("the lossless coding block map");
    rig.session.block_overlay_input(BlockOverlayInput::Pick {
        x: 40,
        y: 40,
        width: 160,
        height: 160,
    });
    rig.session.analysis_input(Input::EvidenceView(7));
    rig.idle("the lossless entropy scope");
    view(rig).invoke_read();
    rig.idle("the partial entropy capture");

    let limit = MAX_ENTROPY_EVENTS as usize;
    assert_eq!(
        view(rig).get_status(),
        format!("Partial capture. A limit stopped capture after {limit} symbols.")
    );
    assert_eq!(usize::try_from(view(rig).get_count()).ok(), Some(limit));
    let accounting = view(rig).get_accounting();
    assert!(
        accounting.contains(&format!(
            "of {limit} captured symbols. The capture is partial, so this sum is a lower bound for this owner."
        )),
        "{accounting}"
    );
    assert!(accounting.contains("Captured syntax counts. The capture is partial."));
    let copied = view(rig).get_copied_capture();
    assert!(
        copied.contains(&format!(
            "\nCoverage\tPartial capture. A limit stopped capture after {limit} symbols.\n"
        )),
        "{copied}"
    );
    // One TSV line for each captured symbol, after the column header.
    let symbols = copied
        .lines()
        .skip_while(|line| !line.starts_with("Ordinal\t"))
        .skip(1)
        .take_while(|line| !line.is_empty())
        .count();
    assert_eq!(symbols, limit);
}

/// Read Next Symbols continues a partial capture after its last symbol. Every
/// view names the start, and Read reads the first part again.
fn read_next_part(rig: &mut Rig) {
    let first_status = view(rig).get_status();
    let copied = view(rig).get_copied_capture();
    assert!(copied.contains("\nStart ordinal\t0\n"));
    // The last TSV symbol line ends the part: a bypass read covers its bit count.
    let last = copied
        .lines()
        .skip_while(|line| !line.starts_with("Ordinal\t"))
        .skip(1)
        .take_while(|line| !line.is_empty())
        .last()
        .unwrap_or_else(|| unreachable!("symbol line"));
    let fields: Vec<&str> = last.split('\t').collect();
    let ordinal: u64 = fields[0]
        .parse()
        .unwrap_or_else(|_| unreachable!("ordinal"));
    let next = ordinal + fields[7].parse::<u64>().unwrap_or(1);
    assert!(view(rig).get_can_read_next());

    view(rig).invoke_read_next();
    rig.idle("the next entropy part");
    let row = view(rig)
        .get_rows()
        .row_data(0)
        .unwrap_or_else(|| unreachable!("symbol"));
    assert_eq!(row.ordinal, next.to_string());
    let status = view(rig).get_status();
    assert!(
        status.starts_with(&format!("Symbols from ordinal {next}. ")),
        "{status}"
    );
    let accounting = view(rig).get_accounting();
    assert!(
        accounting.contains("this sum is a lower bound for this owner"),
        "{accounting}"
    );
    assert!(accounting.contains(&format!("Captured scope ordinals [{next}, ")));
    assert!(
        view(rig)
            .get_copied_capture()
            .contains(&format!("\nStart ordinal\t{next}\n"))
    );
    assert!(view(rig).get_can_read());

    view(rig).invoke_read();
    rig.idle("the first entropy part again");
    assert_eq!(view(rig).get_status(), first_status);
    assert_eq!(view(rig).get_copied_capture(), copied);
}

/// Arrow, Page and Home/End keys on the focused symbol table move the
/// selection and the visible page together.
fn page_with_keys(rig: &mut Rig, clipboard: &RefCell<String>) {
    let count = usize::try_from(view(rig).get_count()).unwrap_or_default();
    let page = usize::try_from(view(rig).get_page_size()).unwrap_or_default();
    assert!(count > 2 * page, "{count} symbols do not fill three pages");
    assert_selected(rig, 0);
    copy_with_keyboard(rig, clipboard);
    for (key, expected) in [
        (Key::UpArrow, 0),
        (Key::DownArrow, 1),
        (Key::PageDown, 1 + page),
        (Key::PageDown, 1 + 2 * page),
        (Key::PageUp, 1 + page),
        (Key::UpArrow, page),
        (Key::UpArrow, page - 1),
        (Key::End, count - 1),
        (Key::DownArrow, count - 1),
        (Key::PageDown, count - 1),
        (Key::PageUp, count - 1 - page),
        (Key::Home, 0),
        (Key::PageUp, 0),
    ] {
        rig.press(key);
        rig.idle("the moved symbol selection");
        assert_selected(rig, expected);
    }
}

/// The table shows the page of `index`, marks only that row, and the detail
/// describes that symbol.
fn assert_selected(rig: &Rig, index: usize) {
    let page = usize::try_from(view(rig).get_page_size()).unwrap_or_default();
    let count = usize::try_from(view(rig).get_count()).unwrap_or_default();
    let first = index / page * page;
    assert_eq!(usize::try_from(view(rig).get_first()).ok(), Some(first));
    let rows: Vec<_> = view(rig).get_rows().iter().collect();
    assert_eq!(rows.len(), page.min(count - first), "page of {index}");
    let marked: Vec<_> = rows.iter().map(|row| row.selected).collect();
    let expected: Vec<_> = (first..first + rows.len()).map(|at| at == index).collect();
    assert_eq!(marked, expected, "selection of {index}");
    let ordinal = &rows[index - first].ordinal;
    let detail = view(rig).get_detail();
    assert!(detail.contains(&format!(" symbol {ordinal}. ")), "{detail}");
}

/// Stop shows a stopping state at once, then a stopped result, and the
/// inspector reads again.
fn stop_and_read_again(rig: &mut Rig) {
    // The engine can finish a small capture before the cancel reaches it. That
    // outcome is correct, so repeat the race until one capture stops: about
    // one try in ten finishes first.
    let stopped = (0..8).any(|_| read_and_stop(rig));
    assert!(stopped, "no capture stopped in eight tries");

    view(rig).invoke_read();
    rig.idle("the entropy capture after a stop");
    assert!(view(rig).get_status().starts_with("Partial capture."));
    assert!(view(rig).get_rows().row_count() > 0);
    assert_selected(rig, 0);
}

/// Sends Read and Stop in one turn and returns whether the capture stopped.
fn read_and_stop(rig: &mut Rig) -> bool {
    assert!(!view(rig).get_busy(), "Stop is shown only during a capture");
    view(rig).invoke_read();
    view(rig).invoke_stop();
    // Hand both inputs to the session, but no engine reply yet.
    let inputs: Vec<Input> = rig.inputs.borrow_mut().drain(..).collect();
    for input in inputs {
        rig.session.analysis_input(input);
    }
    studio_app::render_analysis(&rig.window, rig.session.analysis());
    assert!(view(rig).get_busy(), "Stop is shown during a capture");
    assert!(view(rig).get_stopping(), "Stop is disabled while stopping");
    assert!(!view(rig).get_can_read());
    assert_eq!(view(rig).get_count(), 0);
    assert_eq!(view(rig).get_status(), "Stopping entropy capture…");

    rig.idle("the stopped entropy capture");
    assert!(!view(rig).get_busy());
    assert!(!view(rig).get_stopping());
    assert!(view(rig).get_can_read());
    let status = view(rig).get_status();
    let stopped = status == "Entropy capture stopped." || status.starts_with("Capture stopped. ");
    assert!(
        stopped || status.starts_with("Partial capture."),
        "{status}"
    );
    stopped
}

/// Tabs to the symbol table, proven when Control+C copies the selected symbol.
fn copy_with_keyboard(rig: &mut Rig, clipboard: &RefCell<String>) {
    clipboard.borrow_mut().clear();
    let expected = view(rig).get_copied_symbol();
    assert!(expected.contains("CDF Q15 cutoffs"));
    assert!(expected.contains("Source SHA-256"));
    for _ in 0..60 {
        rig.press(Key::Tab);
        rig.chord(Key::Control, "c");
        if clipboard.borrow().as_str() == expected.as_str() {
            return;
        }
    }
    assert_eq!(clipboard.borrow().as_str(), expected.as_str());
}

fn view(rig: &Rig) -> EntropyView<'_> {
    rig.window.global::<EntropyView>()
}
