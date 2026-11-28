// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The Compare workspace in the real compiled window, with the real engine
//! and real files: two encodes run through the session, a converted file
//! pairs with the source its job read, two results pair explicitly, a
//! stream of another size is refused for numerical comparison, the policy
//! comes back from the journal, and a replaced source is refused.
//!
//! Runs headless on Slint's software renderer: one test function, because a
//! process has one Slint platform. The engine is looked up next to the app
//! binary; build it first (`cargo build -p studio-engine`). Set
//! `SPLOT_STUDIO_TEST_SNAPSHOTS` to a folder to keep renders of the states.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Clipboard, Key, Platform, WindowAdapter, WindowEvent};
use slint::{ComponentHandle, Model, PhysicalSize, PlatformError, Rgb8Pixel, SharedString};
use studio_app::compare::{Input, Status};
use studio_app::engine::{Delivery, ENGINE_NAME};
use studio_app::jobs::{self, Work};
use studio_app::queue::ItemState;
use studio_app::session::{Route, Session, UiEffect};
use studio_app::state::{AppState, Intent, Startup};
use studio_app::workspace::Workspace;
use studio_app::{AppWindow, CompareState, Comparison, Opening, WorkspaceView};
use studio_app::{journal, present};
use studio_domain::compare::{
    AlignmentBasis, ComparisonSession, Compatibility, ConfigComparison, DomainMismatch,
    DomainProperty, Side,
};
use studio_domain::preview::GrainPolicy;
use studio_domain::protocol::BuildIdentity;

/// Long enough for a debug-build encode of a tiny clip.
const JOB_TIMEOUT: Duration = Duration::from_secs(600);
const ANSWER_TIMEOUT: Duration = Duration::from_secs(60);
const SIZE: (u32, u32) = (960, 680);

/// An AVM stream of the engine tests: four 64 x 64 pictures, no encode job.
const FOREIGN: &[u8] =
    include_bytes!("../../studio-engine/tests/inspect/fixtures/syn-lcr-64x64.ivf");

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

/// A fresh directory, removed afterwards: `media/` and the app's `data/`.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("studio-app-compare-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for sub in ["media", "data"] {
            fs::create_dir_all(dir.join(sub)).unwrap_or_else(|e| unreachable!("{e}"));
        }
        Self(dir)
    }

    fn media(&self, name: &str) -> PathBuf {
        self.0.join("media").join(name)
    }

    fn startup(&self) -> (Startup, PathBuf) {
        match studio_app::prepare(Ok(self.0.join("data"))) {
            Opening::Owner(prepared) => (
                prepared.startup,
                self.0.join("data").join(journal::FILE_NAME),
            ),
            Opening::AlreadyOpen(dir) => unreachable!("{} is locked", dir.display()),
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A progressive 8-bit 4:2:0 Y4M with a moving pattern, at 25 frames per second.
fn write_y4m(path: &Path, frames: usize) {
    let (width, height) = (64_usize, 36_usize);
    let mut bytes = format!("YUV4MPEG2 W{width} H{height} F25:1 Ip A1:1 C420jpeg\n").into_bytes();
    let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
    for f in 0..frames {
        bytes.extend_from_slice(b"FRAME\n");
        for (w, h, offset) in [(width, height, 0), (cw, ch, 60), (cw, ch, 130)] {
            for y in 0..h {
                for x in 0..w {
                    bytes.push(u8::try_from((x * 7 + y * 3 + f * 5 + offset) % 256).unwrap_or(0));
                }
            }
        }
    }
    fs::write(path, bytes).unwrap_or_else(|e| unreachable!("{e}"));
}

/// The window, the session behind it, the inputs the window sent and the
/// files the next Compare dialogs answer with.
struct Rig {
    platform: Rc<MinimalSoftwareWindow>,
    window: AppWindow,
    session: Session,
    rx: Receiver<(Route, Delivery)>,
    inputs: Rc<RefCell<Vec<Input>>>,
    picks: VecDeque<PathBuf>,
}

impl Rig {
    fn open(platform: &Rc<MinimalSoftwareWindow>, window: &AppWindow, scratch: &Scratch) -> Self {
        let (tx, rx) = mpsc::channel();
        let post = Arc::new(move |route, delivery| {
            let _ = tx.send((route, delivery));
        });
        let (startup, journal) = scratch.startup();
        let writer =
            journal::Writer::spawn(journal, |_| {}).unwrap_or_else(|e| unreachable!("{e}"));
        let session = Session::new(AppState::new(startup), Ok(engine()), post, Some(writer));
        let inputs: Rc<RefCell<Vec<Input>>> = Rc::default();
        studio_app::bind_compare(window, {
            let inputs = inputs.clone();
            move |input| inputs.borrow_mut().push(input)
        });
        let mut rig = Self {
            platform: platform.clone(),
            window: window.clone_strong(),
            session,
            rx,
            inputs,
            picks: VecDeque::new(),
        };
        let client = BuildIdentity {
            name: "studio-app-compare-test".into(),
            version: "0".into(),
        };
        rig.session.start_hello(client, Instant::now());
        rig.settle("the engine answered", ANSWER_TIMEOUT, |s| {
            s.state().capabilities().is_some()
        });
        rig
    }

    /// Hands the window's inputs to the session, answers its dialogs and
    /// delivers the engine's answers until `done` holds. Then redraws.
    fn settle(&mut self, what: &str, timeout: Duration, done: impl Fn(&Session) -> bool) {
        let deadline = Instant::now() + timeout;
        loop {
            let inputs: Vec<Input> = self.inputs.borrow_mut().drain(..).collect();
            for input in inputs {
                let outcome = self.session.compare_input(input, Instant::now());
                if outcome.ui.contains(&UiEffect::ShowCompareDialog) {
                    let picked = self.picks.pop_front();
                    self.session.compare_picked(picked);
                }
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
        studio_app::render_compare(&self.window, self.session.compare());
    }

    fn dispatch(&mut self, intent: Intent) {
        self.session.dispatch(intent, Instant::now());
    }

    /// Converts `source` with the current settings and returns its row.
    fn convert(&mut self, source: &Path) -> usize {
        self.dispatch(Intent::FilesOpened(vec![source.to_path_buf()]));
        self.settle("the file read", ANSWER_TIMEOUT, |s| {
            s.state()
                .items()
                .iter()
                .all(|i| i.state() != ItemState::Probing)
        });
        self.dispatch(Intent::ConvertRequested);
        self.settle("the encode", JOB_TIMEOUT, |s| !s.state().running());
        let row = self.session.state().items().len() - 1;
        assert_eq!(
            self.session.state().items()[row].state(),
            ItemState::Completed
        );
        row
    }

    /// Waits until the comparison is read or refused.
    fn compared(&mut self, what: &str) {
        self.settle(what, ANSWER_TIMEOUT, |s| {
            matches!(
                s.compare().status(),
                Status::Ready(_) | Status::Failed { .. }
            )
        });
        // An idle comparison holds no process.
        assert!(!self.session.comparing(), "{what}");
    }

    fn session(&self) -> &ComparisonSession {
        match self.session.compare().status() {
            Status::Ready(session) => session,
            other => unreachable!("{other:?}"),
        }
    }

    fn global(&self) -> Comparison<'_> {
        self.window.global::<Comparison>()
    }

    fn lines(&self, side: Side) -> Vec<String> {
        let side = match side {
            Side::A => self.global().get_a(),
            Side::B => self.global().get_b(),
        };
        side.lines
            .iter()
            .chain(side.exact.iter())
            .map(|line| line.to_string())
            .collect()
    }

    fn press(&self, key: impl Into<SharedString>) {
        let text = key.into();
        let window = self.window.window();
        window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        window.dispatch_event(WindowEvent::KeyReleased { text });
    }

    /// Moves the keyboard focus one stop back, presses `probe` there and
    /// returns what the window sent. The inputs are not sent to the session.
    fn back(&self, probe: impl Into<SharedString>) -> Vec<Input> {
        self.press(Key::Backtab);
        self.press(probe);
        self.inputs.borrow_mut().drain(..).collect()
    }

    fn snapshot(&self, scheme: slint::language::ColorScheme, scale: f32, file_name: &str) {
        use std::io::Write;
        let Some(folder) = std::env::var_os("SPLOT_STUDIO_TEST_SNAPSHOTS").map(PathBuf::from)
        else {
            return;
        };
        fs::create_dir_all(&folder).unwrap_or_else(|e| unreachable!("{e}"));
        let native = self.window.window();
        let size = native.size();
        let palette = self.window.global::<studio_app::Palette>();
        palette.set_color_scheme(scheme);
        native.dispatch_event(WindowEvent::ScaleFactorChanged {
            scale_factor: scale,
        });
        native.set_size(slint::LogicalSize::new(800.0, 900.0));
        native.request_redraw();
        let (width, height) = (native.size().width, native.size().height);
        let mut pixels = vec![Rgb8Pixel::default(); (width * height) as usize];
        assert!(self.platform.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, width as usize);
        }));
        let mut out =
            fs::File::create(folder.join(file_name)).unwrap_or_else(|e| unreachable!("{e}"));
        write!(out, "P6\n{width} {height}\n255\n").unwrap_or_else(|e| unreachable!("{e}"));
        let rgb: Vec<_> = pixels.iter().flat_map(|p| [p.r, p.g, p.b]).collect();
        out.write_all(&rgb).unwrap_or_else(|e| unreachable!("{e}"));
        native.dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: 1.0 });
        native.set_size(size);
        palette.set_color_scheme(slint::language::ColorScheme::Light);
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn encodes_pair_with_their_source_and_with_each_other_in_the_running_window() {
    let scratch = Scratch::new();
    let (first, second) = (scratch.media("first.y4m"), scratch.media("second.y4m"));
    write_y4m(&first, 5);
    write_y4m(&second, 7);
    let foreign = scratch.media("foreign.ivf");
    fs::write(&foreign, FOREIGN).unwrap_or_else(|e| unreachable!("{e}"));

    let platform = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    let clipboard: Rc<RefCell<String>> = Rc::default();
    slint::platform::set_platform(Box::new(Headless {
        window: platform.clone(),
        clipboard: clipboard.clone(),
    }))
    .unwrap_or_else(|e| unreachable!("{e}"));
    let window = studio_app::new_window().unwrap_or_else(|e| unreachable!("{e}"));
    window
        .global::<studio_app::Palette>()
        .set_color_scheme(slint::language::ColorScheme::Light);
    window.window().set_size(PhysicalSize::new(SIZE.0, SIZE.1));
    window.show().unwrap_or_else(|e| unreachable!("{e}"));

    // Two real encodes with different settings.
    let mut rig = Rig::open(&platform, &window, &scratch);
    rig.dispatch(Intent::PresetEdited(10));
    rig.dispatch(Intent::CrfEdited(40));
    let first_row = rig.convert(&first);
    rig.dispatch(Intent::CrfEdited(50));
    rig.convert(&second);
    let jobs = rig.session.state().items().to_vec();
    let (first_ivf, second_ivf) = (scratch.media("first.ivf"), scratch.media("second.ivf"));
    assert!(first_ivf.is_file() && second_ivf.is_file());

    // Compare lists no pair yet and says how to make one.
    rig.dispatch(Intent::WorkspaceRequested(Workspace::Compare));
    rig.settle("the empty workspace", ANSWER_TIMEOUT, |_| true);
    assert_eq!(window.get_workspace(), WorkspaceView::Compare);
    assert_eq!(window.get_workspace_index(), 2);
    assert_eq!(rig.global().get_state(), CompareState::Empty);
    assert!(rig.global().get_message().contains("Compare with Source"));
    rig.snapshot(
        slint::language::ColorScheme::Light,
        1.0,
        "compare-empty.ppm",
    );

    // Compare with Source on the first converted file: no file is chosen.
    rig.dispatch(Intent::WorkspaceRequested(Workspace::Encode));
    rig.dispatch(Intent::RowSelected(i32::try_from(first_row).unwrap_or(0)));
    rig.settle("the selected row", ANSWER_TIMEOUT, |_| true);
    assert!(window.get_can_compare_result());
    window.invoke_compare_result_requested();
    rig.compared("the encode pair");
    assert_eq!(rig.session.state().workspace(), Workspace::Compare);
    assert_eq!(rig.global().get_state(), CompareState::Ready);
    let session = rig.session().clone();
    let job = jobs[first_row].attempt().map(|attempt| attempt.job.clone());
    assert_eq!(
        Some(session.a.fingerprint),
        job.as_ref().map(|job| job.fingerprint),
        "the source is the file the job read"
    );
    assert_eq!((session.outputs(Side::A), session.outputs(Side::B)), (5, 5));
    assert_eq!(session.setup.basis, AlignmentBasis::Provenance);
    assert_eq!(session.alignment().map(|m| m.matched()), Ok(5));
    assert_eq!(session.timestamps_agree(), Some(true));
    assert!(matches!(
        session.compatibility(),
        Compatibility::Exact { .. }
    ));
    assert_eq!(
        session.configuration(),
        ConfigComparison::Unknown { a: false, b: true }
    );
    // The window says the same, in words.
    assert_eq!(rig.global().get_a().name, "first.y4m");
    assert_eq!(rig.global().get_b().name, "first.ivf");
    assert!(
        rig.lines(Side::A)
            .contains(&"The size and the modification time match the encode job.".to_owned()),
        "{:?}",
        rig.lines(Side::A)
    );
    assert_eq!(
        rig.global().get_verdict(),
        "Sample values can be compared without a conversion."
    );
    assert!(!rig.global().get_verdict_problem());
    assert!(
        rig.global()
            .get_alignment()
            .starts_with("5 pictures match. Without a match: 0 of side A, 0 of side B."),
        "{}",
        rig.global().get_alignment()
    );
    let bases: Vec<String> = rig
        .global()
        .get_basis_labels()
        .iter()
        .map(Into::into)
        .collect();
    assert_eq!(
        bases,
        [
            "Frame order of the encode job",
            "Exact timestamps",
            "Assumed index offset"
        ]
    );
    assert_eq!(
        rig.global().get_settings_status(),
        "Side A is the source. It has no encoder settings."
    );
    rig.snapshot(
        slint::language::ColorScheme::Light,
        1.0,
        "compare-encode-light.ppm",
    );
    rig.snapshot(
        slint::language::ColorScheme::Dark,
        2.0,
        "compare-encode-dark-2x.ppm",
    );

    // Keyboard only, from the last control back: Close Comparison, Copy
    // Summary, the alignment basis and the film grain policy.
    assert_eq!(rig.back(Key::Return), [Input::Close]);
    assert_eq!(rig.back(Key::Return), []);
    assert!(
        clipboard.borrow().contains("Side A: first.y4m")
            && clipboard
                .borrow()
                .contains("Alignment (Frame order of the encode job): 5 pictures match."),
        "{}",
        clipboard.borrow()
    );
    assert_eq!(rig.back(Key::DownArrow), [Input::Basis(1)]);
    assert_eq!(rig.back(" "), [Input::Grain(false)]);
    rig.inputs
        .borrow_mut()
        .extend([Input::Basis(1), Input::Grain(false)]);
    rig.settle("the policy edits", ANSWER_TIMEOUT, |_| true);
    assert_eq!(rig.session().setup.basis, AlignmentBasis::Timestamps);
    assert_eq!(rig.session().setup.grain, GrainPolicy::Skip);
    assert_eq!(rig.session().alignment().map(|m| m.matched()), Ok(5));
    assert!(!rig.global().get_grain());
    assert_eq!(rig.global().get_basis_index(), 1);
    // The jobs are what they were: the policy belongs to the session.
    assert_eq!(rig.session.state().items(), jobs);
    // Jobs lists the read of the pair with the two conversions.
    let read = rig
        .session
        .jobs()
        .into_iter()
        .find_map(|job| match job.work {
            Work::Compare(_) => Some((job.state, present::jobs::line(&job).name)),
            _ => None,
        });
    assert_eq!(
        read,
        Some((jobs::State::Completed, "first.y4m and first.ivf".to_owned()))
    );

    // Two results, chosen one by one. Both keep the settings of their jobs.
    rig.picks.extend([first_ivf.clone(), second_ivf.clone()]);
    rig.inputs.borrow_mut().push(Input::Pick(Side::A));
    rig.settle("side A", ANSWER_TIMEOUT, |_| true);
    rig.inputs.borrow_mut().push(Input::Pick(Side::B));
    rig.compared("the result pair");
    let session = rig.session().clone();
    assert_eq!(session.setup.common_source(), None);
    assert_eq!(session.setup.basis, AlignmentBasis::Timestamps);
    // Unequal lengths at one rate: two pictures of side B have no match.
    assert_eq!(
        session
            .alignment()
            .map(|m| (m.matched(), m.unmatched(Side::A), m.unmatched(Side::B))),
        Ok((5, 0, 2))
    );
    let ConfigComparison::Known(pair) = session.configuration() else {
        unreachable!("{:?}", session.configuration())
    };
    assert_eq!(pair.effective_known, (true, true));
    assert!(pair.rows.iter().any(|row| row.key == "rate_control"));
    let settings: Vec<_> = rig.global().get_settings().iter().collect();
    assert!(
        settings
            .iter()
            .any(|row| row.key == "rate_control" && row.difference == "Different value"),
        "{settings:?}"
    );
    assert!(rig.global().get_settings_status().contains("differ"));
    rig.snapshot(
        slint::language::ColorScheme::Light,
        1.0,
        "compare-results-light.ppm",
    );

    // A stream that no job of the list published, with another picture size.
    rig.picks.push_back(foreign.clone());
    rig.inputs.borrow_mut().push(Input::Pick(Side::B));
    rig.compared("the foreign pair");
    let session = rig.session().clone();
    assert_eq!(
        session.configuration(),
        ConfigComparison::Unknown { a: true, b: false }
    );
    assert_eq!(
        session.compatibility(),
        Compatibility::Refused {
            reasons: vec![DomainMismatch::Differs(DomainProperty::Height)]
        }
    );
    assert_eq!(
        rig.global().get_verdict(),
        "Numerical comparison is not available."
    );
    assert!(rig.global().get_verdict_problem());
    let details: Vec<_> = rig.global().get_verdict_details().iter().collect();
    assert!(
        details.contains(&"The height differs: side A has 36, side B has 64.".into()),
        "{details:?}"
    );
    assert_eq!(
        rig.global().get_settings_status(),
        "No encode job is recorded for side B. Studio does not infer settings from a stream."
    );
    // 25 and 30 pictures per second share only the first timestamp of these
    // short streams. A labelled offset is the user's assumption.
    rig.inputs.borrow_mut().push(Input::Basis(1));
    rig.inputs.borrow_mut().push(Input::Offset(-1));
    rig.settle("the assumed offset", ANSWER_TIMEOUT, |_| true);
    assert_eq!(
        rig.session().setup.basis,
        AlignmentBasis::AssumedOffset { offset: -1 }
    );
    assert!(rig.global().get_offset_shown());
    assert_eq!(rig.global().get_offset(), -1);
    assert!(
        rig.global()
            .get_alignment()
            .contains("This is your assumption"),
        "{}",
        rig.global().get_alignment()
    );
    rig.snapshot(
        slint::language::ColorScheme::Light,
        1.0,
        "compare-refused-light.ppm",
    );
    let saved = rig.session.compare().saved();
    assert!(saved.as_ref().is_some_and(|s| s.identity.is_some()));

    // The app closes and starts again: the pair is read again with its policy.
    let Rig { session, .. } = rig;
    drop(session);
    let mut rig = Rig::open(&platform, &window, &scratch);
    assert_eq!(rig.session.state().workspace(), Workspace::Compare);
    assert_eq!(rig.session.compare().saved(), saved, "nothing was lost");
    rig.compared("the restored pair");
    assert_eq!(
        rig.session().setup.basis,
        AlignmentBasis::AssumedOffset { offset: -1 }
    );
    assert_eq!(rig.session.compare().notice(), None);
    assert_eq!(rig.session.compare().saved(), saved);
    assert_eq!(rig.global().get_offset(), -1);

    // The source is replaced after its encode: the pair is refused.
    write_y4m(&first, 6);
    rig.dispatch(Intent::WorkspaceRequested(Workspace::Encode));
    rig.dispatch(Intent::RowSelected(i32::try_from(first_row).unwrap_or(0)));
    rig.inputs.borrow_mut().push(Input::PairResult);
    rig.compared("the replaced source");
    assert_eq!(rig.global().get_state(), CompareState::Failed);
    assert_eq!(
        rig.global().get_message(),
        "The source changed after the encode. Studio cannot verify it."
    );
    rig.snapshot(
        slint::language::ColorScheme::Light,
        1.0,
        "compare-failed-light.ppm",
    );

    rig.inputs.borrow_mut().push(Input::Close);
    rig.settle("the closed comparison", ANSWER_TIMEOUT, |_| true);
    assert_eq!(rig.global().get_state(), CompareState::Empty);
    assert_eq!(rig.session.compare().saved(), None);
}
