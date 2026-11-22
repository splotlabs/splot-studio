// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The Analyze workspace in the real compiled window, against the real
//! inspect engine process: an IVF file and a truncated Annex B stream,
//! driven with the keyboard and the pointer as a user would.
//!
//! The window's callbacks reach the session through `bind_analysis`, as in
//! the app; engine events come back over a channel instead of the Slint
//! event loop. The fixtures are the hand-built ones of the engine tests,
//! whose offsets were worked out by hand. The engine is looked up next to
//! the app binary; build it first (`cargo build -p studio-engine`).
//!
//! Runs headless on Slint's software renderer, so it needs no display: one
//! test function, because a process has one Slint platform.

use std::cell::RefCell;
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Clipboard, Key, Platform, PointerEventButton, WindowAdapter, WindowEvent};
use slint::{
    ComponentHandle, Model, PhysicalPosition, PhysicalSize, PlatformError, Rgb8Pixel, SharedString,
};
use studio_app::analysis::{Input, Position, VISIBLE_ROWS};
use studio_app::engine::{Delivery, ENGINE_NAME};
use studio_app::session::{Route, Session};
use studio_app::state::{AppState, Intent};
use studio_app::workspace::Workspace;
use studio_app::{AnalysisState, AppWindow, ByteRegion};

const ANSWER_TIMEOUT: Duration = Duration::from_secs(60);
const SIZE: (u32, u32) = (960, 680);

struct Headless {
    window: Rc<MinimalSoftwareWindow>,
    clipboard: Rc<RefCell<String>>,
}

impl Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.window.clone())
    }

    fn set_clipboard_text(&self, text: &str, clipboard: Clipboard) {
        if clipboard == Clipboard::DefaultClipboard {
            text.clone_into(&mut self.clipboard.borrow_mut());
        }
    }
}

fn engine() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_BIN_EXE_splot-studio"))
        .with_file_name(format!("{ENGINE_NAME}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.is_file(),
        "build studio-engine first (`cargo build -p studio-engine`, same profile): no engine \
         at {}",
        path.display()
    );
    path
}

/// The engine tests' `ivf()`: three records (TD and sequence header, an
/// empty payload, TD and a tile group with the extension), 82 bytes.
fn ivf() -> Vec<u8> {
    let mut bytes = b"DKIF".to_vec();
    bytes.extend(0_u16.to_le_bytes());
    bytes.extend(32_u16.to_le_bytes());
    bytes.extend(b"AV02");
    bytes.extend(16_u16.to_le_bytes());
    bytes.extend(16_u16.to_le_bytes());
    bytes.extend(30_u32.to_le_bytes());
    bytes.extend(1_u32.to_le_bytes());
    bytes.extend(3_u32.to_le_bytes());
    bytes.extend(0_u32.to_le_bytes());
    for (pts, payload) in [
        (0_u64, &[0x01, 0x08, 0x04, 0x04, 0xAA, 0xBB, 0xCC][..]),
        (1, &[]),
        (2, &[0x01, 0x08, 0x04, 0x9D, 0x45, 0x11, 0x22]),
    ] {
        bytes.extend(u32::try_from(payload.len()).unwrap_or(0).to_le_bytes());
        bytes.extend(pts.to_le_bytes());
        bytes.extend(payload);
    }
    assert_eq!(bytes.len(), 82);
    bytes
}

/// TD, sequence header, then a padding OBU that declares 131 bytes and has
/// two. 11 bytes.
const TRUNCATED: [u8; 11] = [
    0x01, 0x08, 0x04, 0x04, 0xAA, 0xBB, 0xCC, 0x83, 0x01, 0x64, 0x5A,
];

/// A fresh directory, removed afterwards.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The window, the session behind it and the inputs the window sent.
struct Rig {
    platform: Rc<MinimalSoftwareWindow>,
    window: AppWindow,
    session: Session,
    rx: Receiver<(Route, Delivery)>,
    inputs: Rc<RefCell<Vec<Input>>>,
}

impl Rig {
    /// Hands the window's inputs to the session and the engine's answers
    /// back, until `done` holds, then redraws the window.
    fn settle(&mut self, what: &str, done: impl Fn(&Session) -> bool) {
        let deadline = Instant::now() + ANSWER_TIMEOUT;
        loop {
            let inputs: Vec<Input> = self.inputs.borrow_mut().drain(..).collect();
            for input in inputs {
                self.session.analysis_input(input);
            }
            while let Ok((route, delivery)) = self.rx.try_recv() {
                self.session.deliver(route, delivery, Instant::now());
            }
            if done(&self.session) && self.inputs.borrow().is_empty() {
                break;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "waiting for {what}");
            if let Ok((route, delivery)) = self.rx.recv_timeout(Duration::from_millis(20)) {
                self.session.deliver(route, delivery, Instant::now());
            }
        }
        studio_app::render(&self.window, self.session.state(), Instant::now());
        studio_app::render_analysis(&self.window, self.session.analysis());
    }

    /// Waits until the shown document has every visible row and its bytes.
    fn idle(&mut self, what: &str) {
        self.settle(what, |s| {
            s.analysis().shown().is_some_and(|d| {
                let rows = d.total().is_some_and(|t| {
                    (d.top()..t.min(d.top() + VISIBLE_ROWS)).all(|p| d.row(p).is_some())
                });
                rows && d.bytes().data.is_some()
            }) && !s.analysis().waiting()
        });
    }

    fn press(&self, key: impl Into<SharedString>) {
        let text = key.into();
        let window = self.window.window();
        window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        window.dispatch_event(WindowEvent::KeyReleased { text });
    }

    fn chord(&self, modifier: Key, key: &str) {
        let window = self.window.window();
        window.dispatch_event(WindowEvent::KeyPressed {
            text: modifier.into(),
        });
        self.press(key);
        window.dispatch_event(WindowEvent::KeyReleased {
            text: modifier.into(),
        });
    }

    fn click(&self, (x, y): (usize, usize)) {
        let position = PhysicalPosition::new(
            i32::try_from(x).unwrap_or(i32::MAX),
            i32::try_from(y).unwrap_or(i32::MAX),
        )
        .to_logical(1.0);
        let window = self.window.window();
        window.dispatch_event(WindowEvent::PointerMoved { position });
        for pressed in [true, false] {
            window.dispatch_event(if pressed {
                WindowEvent::PointerPressed {
                    position,
                    button: PointerEventButton::Left,
                }
            } else {
                WindowEvent::PointerReleased {
                    position,
                    button: PointerEventButton::Left,
                }
            });
        }
    }

    fn pixels(&self) -> Vec<Rgb8Pixel> {
        self.window.window().request_redraw();
        let mut buffer = vec![Rgb8Pixel::default(); (SIZE.0 * SIZE.1) as usize];
        self.platform.draw_if_needed(|renderer| {
            renderer.render(&mut buffer, SIZE.0 as usize);
        });
        buffer
    }

    /// The top-left pixel of the first vertical run of `color` at least
    /// `tall` pixels high: the selected row's bar, not text or a tab mark.
    fn find_bar(&self, color: (u8, u8, u8), tall: usize) -> Option<(usize, usize)> {
        let (width, height) = (SIZE.0 as usize, SIZE.1 as usize);
        let pixels = self.pixels();
        let is = |x: usize, y: usize| {
            let p = pixels[y * width + x];
            (p.r, p.g, p.b) == color
        };
        (0..height - tall)
            .flat_map(|y| (1..width).map(move |x| (x, y)))
            .find(|&(x, y)| !is(x - 1, y) && (0..tall).all(|d| is(x, y + d)))
    }

    /// The top-left corner of the box around every pixel of `color`.
    fn corner(&self, color: (u8, u8, u8)) -> Option<(usize, usize)> {
        let width = SIZE.0 as usize;
        let spots: Vec<usize> = self
            .pixels()
            .iter()
            .enumerate()
            .filter(|(_, p)| (p.r, p.g, p.b) == color)
            .map(|(at, _)| at)
            .collect();
        let x = spots.iter().map(|at| at % width).min()?;
        let y = spots.iter().map(|at| at / width).min()?;
        Some((x, y))
    }

    /// Tabs until `focused` says the wanted control has the keys, and
    /// returns how many Tabs that took. `probe` must be a key only that
    /// control answers.
    fn tab_to(&mut self, probe: Key, focused: impl Fn(&Input) -> bool) -> usize {
        for tabs in 1..30 {
            self.press(Key::Tab);
            self.press(probe);
            let hit = self.inputs.borrow().iter().any(&focused);
            if hit {
                return tabs;
            }
        }
        unreachable!("no Tab stop answered {probe:?}");
    }

    fn selected(&self) -> Option<(u64, Position)> {
        self.session
            .analysis()
            .shown()
            .and_then(|d| d.selected())
            .map(|s| (s.row.index, s.position))
    }

    fn global(&self) -> studio_app::Analysis<'_> {
        self.window.global::<studio_app::Analysis>()
    }

    /// The region and cursor flag of byte `offset` in the byte view.
    fn byte(&self, offset: usize) -> (String, ByteRegion, bool) {
        let lines = self.global().get_byte_lines();
        let window = self
            .session
            .analysis()
            .shown()
            .map_or(0, |d| d.bytes().offset);
        let window = usize::try_from(window).unwrap_or(usize::MAX);
        let at = offset - window;
        let line = lines
            .row_data(at / 16)
            .unwrap_or_else(|| unreachable!("line"));
        let cell = line
            .cells
            .row_data(at % 16)
            .unwrap_or_else(|| unreachable!("cell"));
        (cell.text.to_string(), cell.region, cell.cursor)
    }

    fn row_titles(&self) -> Vec<String> {
        self.global()
            .get_rows()
            .iter()
            .map(|r| {
                format!(
                    "{}{}",
                    "  ".repeat(usize::try_from(r.depth).unwrap_or(0)),
                    r.title
                )
            })
            .collect()
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn ivf_and_annex_b_documents_in_the_running_window() {
    let scratch =
        Scratch(std::env::temp_dir().join(format!("studio-app-analyze-{}", std::process::id())));
    let _ = fs::remove_dir_all(&scratch.0);
    fs::create_dir_all(&scratch.0).unwrap_or_else(|e| unreachable!("{e}"));
    let ivf_path = scratch.0.join("park walk.ivf");
    fs::write(&ivf_path, ivf()).unwrap_or_else(|e| unreachable!("{e}"));
    let obu_path = scratch.0.join("cut short.obu");
    fs::write(&obu_path, TRUNCATED).unwrap_or_else(|e| unreachable!("{e}"));
    let text_path = scratch.0.join("notes.txt");
    fs::write(&text_path, b"not a stream").unwrap_or_else(|e| unreachable!("{e}"));

    let platform = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    let clipboard: Rc<RefCell<String>> = Rc::default();
    slint::platform::set_platform(Box::new(Headless {
        window: platform.clone(),
        clipboard: clipboard.clone(),
    }))
    .unwrap_or_else(|e| unreachable!("{e}"));
    let window = studio_app::new_window().unwrap_or_else(|e| unreachable!("{e}"));
    window.window().set_size(PhysicalSize::new(SIZE.0, SIZE.1));
    window.show().unwrap_or_else(|e| unreachable!("{e}"));
    let (tx, rx) = mpsc::channel();
    let post = Arc::new(move |route, delivery| {
        let _ = tx.send((route, delivery));
    });
    let session = Session::new(AppState::default(), Ok(engine()), post, None);
    let inputs: Rc<RefCell<Vec<Input>>> = Rc::default();
    studio_app::bind_analysis(&window, {
        let inputs = inputs.clone();
        move |input| inputs.borrow_mut().push(input)
    });
    let mut rig = Rig {
        platform,
        window,
        session,
        rx,
        inputs,
    };
    rig.session.dispatch(
        Intent::WorkspaceRequested(Workspace::Analyze),
        Instant::now(),
    );

    // Not a stream: a failed document that says why, not an empty one.
    rig.session.analysis_input(Input::Open(vec![text_path]));
    rig.settle("the refusal", |s| {
        s.analysis().shown().is_some_and(|d| {
            d.summary().is_none() && !matches!(d.phase(), studio_app::analysis::Phase::Opening)
        })
    });
    assert_eq!(rig.global().get_state(), AnalysisState::Failed);
    assert!(
        rig.global().get_message().contains("neither an IVF file"),
        "{}",
        rig.global().get_message()
    );
    rig.session.analysis_input(Input::Close);

    // The IVF file: its rows, nested under their packets.
    rig.session
        .analysis_input(Input::Open(vec![ivf_path.clone()]));
    rig.idle("the IVF document");
    assert_eq!(rig.global().get_state(), AnalysisState::Open);
    assert_eq!(
        rig.global().get_summary_line(),
        "IVF · 82 bytes · 3 packets, 4 OBUs"
    );
    assert_eq!(
        rig.row_titles(),
        [
            "IVF file header",
            "IVF record 0",
            "  OBU 0 · OBU_TEMPORAL_DELIMITER",
            "  OBU 1 · OBU_SEQUENCE_HEADER",
            "IVF record 1",
            "IVF record 2",
            "  OBU 2 · OBU_TEMPORAL_DELIMITER",
            "  OBU 3 · OBU_REGULAR_TILE_GROUP",
        ]
    );
    // Nothing is selected: the inspector shows the file.
    assert_eq!(rig.global().get_inspector_title(), "File");

    // Keyboard only: Tab to the navigator, End selects the last row.
    let tabs = rig.tab_to(Key::End, |i| {
        *i == Input::Move(studio_app::analysis::Step::End)
    });
    rig.idle("the last row");
    assert_eq!(rig.selected(), Some((7, Position::Row(7))));
    assert_eq!(
        rig.global().get_inspector_title(),
        "OBU 3 · OBU_REGULAR_TILE_GROUP"
    );
    let props: Vec<(String, String, String)> = rig
        .global()
        .get_properties()
        .iter()
        .map(|p| (p.label.to_string(), p.value.to_string(), p.note.to_string()))
        .collect();
    let find = |label: &str| props.iter().find(|p| p.0 == label).cloned();
    assert_eq!(
        find("tlayer"),
        Some(("tlayer".into(), "1".into(), "signaled in the file".into()))
    );
    assert_eq!(
        find("Header").map(|p| p.1),
        Some("2 bytes at 78 (0x4E)".into())
    );
    assert_eq!(
        find("In packet").map(|p| p.1),
        Some("IVF record 2, element 5".into())
    );
    // Its bytes, by part: prefix 77, header 78-79, payload 80-81; 76 is
    // the TD before it. The cursor is on its first byte.
    assert_eq!(rig.byte(76), ("08".into(), ByteRegion::Outside, false));
    assert_eq!(rig.byte(77), ("04".into(), ByteRegion::Prefix, true));
    assert_eq!(rig.byte(78), ("9D".into(), ByteRegion::Header, false));
    assert_eq!(rig.byte(79), ("45".into(), ByteRegion::Header, false));
    assert_eq!(rig.byte(81), ("22".into(), ByteRegion::Payload, false));
    assert_eq!(
        rig.global().get_byte_lines().row_count(),
        6,
        "82 bytes, 16 a line"
    );

    // Left selects the packet that holds it; Up the TD before that.
    rig.press(Key::LeftArrow);
    rig.idle("the packet");
    assert_eq!(rig.selected(), Some((5, Position::Row(5))));
    assert_eq!(rig.byte(63), ("07".into(), ByteRegion::RecordHeader, true));
    assert_eq!(rig.byte(75), ("01".into(), ByteRegion::Payload, false));
    rig.press(Key::UpArrow);
    rig.idle("record 1");
    assert_eq!(rig.selected(), Some((4, Position::Row(4))));

    // A pointer click on a row is the same selection as the keyboard's.
    // The selected row's accent bar marks where the rows are.
    let bar = rig
        .find_bar((0x63, 0x42, 0xD6), 20)
        .unwrap_or_else(|| unreachable!("no selection bar"));
    rig.click((bar.0 + 60, bar.1 + 12 - 24 - 24));
    rig.idle("the clicked row");
    assert_eq!(rig.selected(), Some((2, Position::Row(2))));

    // The byte view: Tab to it, then the cursor and Enter. Byte 47 is the
    // sequence header's first byte: Enter selects that OBU.
    let byte_tabs = rig.tab_to(Key::Home, |i| {
        *i == Input::Cursor(studio_app::analysis::Step::Home)
    });
    rig.idle("the cursor");
    for _ in 0..2 {
        rig.press(Key::DownArrow);
    }
    for _ in 0..15 {
        rig.press(Key::RightArrow);
    }
    rig.idle("the cursor moves");
    assert_eq!(
        rig.session.analysis().shown().map(|d| d.bytes().cursor),
        Some(47)
    );
    rig.press(Key::Return);
    rig.idle("the byte's element");
    assert_eq!(rig.selected(), Some((3, Position::Row(3))));
    assert!(
        rig.global()
            .get_cursor_line()
            .contains("in the selection's header")
    );

    // A click on another byte of the same OBU changes nothing: no history.
    // The byte view's focus ring is 3px outside it; in it, line 3 starts at
    // y 54 and byte 1 of a line at x 92 (64 of offsets, 8 of space, 20 a
    // byte).
    let view = rig
        .corner((0x0B, 0x72, 0xA8))
        .unwrap_or_else(|| unreachable!("no focus ring"));
    rig.click((view.0 + 3 + 92 + 10, view.1 + 3 + 54 + 9));
    rig.idle("the clicked byte");
    assert_eq!(
        rig.session.analysis().shown().map(|d| d.bytes().cursor),
        Some(49)
    );
    assert_eq!(rig.selected(), Some((3, Position::Row(3))));
    // Back: the clicked row, then record 1, then the packet.
    rig.global().invoke_back();
    rig.idle("back");
    assert_eq!(rig.selected(), Some((2, Position::Row(2))));
    rig.global().invoke_back();
    rig.idle("back again");
    assert_eq!(rig.selected(), Some((4, Position::Row(4))));
    rig.global().invoke_forward();
    rig.idle("forward");
    assert_eq!(rig.selected(), Some((2, Position::Row(2))));
    assert_eq!(rig.byte(44), ("01".into(), ByteRegion::Prefix, true));

    // Copy: Shift+Tab back to the inspector; Control+C copies a value.
    rig.press(Key::Backtab);
    rig.press(Key::Backtab);
    rig.chord(Key::Control, "c");
    assert_eq!(*clipboard.borrow(), "2", "the element index");
    rig.press(Key::DownArrow);
    rig.chord(Key::Control, "c");
    assert_eq!(*clipboard.borrow(), "0", "the OBU index");

    // A filter that hides the selection keeps it, and says so.
    let types: Vec<String> = rig
        .global()
        .get_type_labels()
        .iter()
        .map(|s| s.to_string())
        .collect();
    let tiles = types
        .iter()
        .position(|t| t.starts_with("OBU_REGULAR_TILE_GROUP"))
        .and_then(|i| i32::try_from(i).ok())
        .unwrap_or_else(|| unreachable!("{types:?}"));
    rig.global().invoke_type_filter(tiles);
    rig.idle("the filter");
    assert_eq!(rig.selected(), Some((2, Position::Hidden)));
    assert_eq!(rig.row_titles(), ["  OBU 3 · OBU_REGULAR_TILE_GROUP"]);
    assert!(rig.global().get_hidden_note().contains("OBU 0"));
    assert_eq!(rig.global().get_rows_line(), "1 of 8 elements match");
    rig.global().invoke_clear_filters();
    rig.idle("no filter");
    assert_eq!(rig.selected(), Some((2, Position::Row(2))));
    // Go to a byte: 0x20 is record 0's header.
    rig.global().invoke_go_to("0x20".into());
    rig.idle("the offset");
    assert_eq!(rig.selected(), Some((1, Position::Row(1))));

    // A second document: the truncated Annex B stream.
    rig.session.analysis_input(Input::Open(vec![obu_path]));
    rig.idle("the Annex B document");
    assert!(rig.global().get_outcome_problem());
    assert!(
        rig.global()
            .get_outcome_line()
            .starts_with("Reading stopped at byte 7 (0x7) (malformed)")
    );
    assert_eq!(
        rig.row_titles(),
        [
            "OBU 0 · OBU_TEMPORAL_DELIMITER",
            "OBU 1 · OBU_SEQUENCE_HEADER",
            "Unparsed · malformed",
        ]
    );
    assert!(
        rig.selected().is_none(),
        "documents do not share a selection"
    );
    rig.global().invoke_move(studio_app::NavStep::End);
    rig.idle("the unparsed region");
    assert_eq!(rig.selected(), Some((2, Position::Row(2))));
    // Every byte of the region, and nothing past the file's end.
    for at in 7..11 {
        assert_eq!(rig.byte(at).1, ByteRegion::Unparsed, "byte {at}");
    }
    assert_eq!(rig.global().get_byte_lines().row_count(), 1);
    assert_eq!(
        rig.global()
            .get_byte_lines()
            .row_data(0)
            .map(|l| l.cells.row_count()),
        Some(11)
    );
    // The IVF document kept its own selection.
    rig.global().invoke_show(0);
    rig.idle("the IVF document again");
    assert_eq!(rig.selected(), Some((1, Position::Row(1))));
    assert_eq!(rig.global().get_document_index(), 0);

    eprintln!("Tab stops: navigator after {tabs}, byte view {byte_tabs} later");
}
