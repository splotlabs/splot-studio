// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Jobs and the document lifecycle against the real engine, real files and
//! the real compiled window: an encode that keeps going while its first
//! result is inspected, Jobs driven with the keyboard, a cancelled and a
//! closed read, an inspect process that is killed, and restarts that
//! restore the documents only after checking their files (unchanged,
//! replaced, deleted, and with read-only or corrupt state storage).
//!
//! The session is driven as the window drives it, with worker events and
//! journal results brought back over channels instead of the Slint event
//! loop. Every scenario has its own data folder; the developer's real one
//! is never touched. Build the engine first (`cargo build -p
//! studio-engine`). Runs headless on Slint's software renderer: one test
//! function, because a process has one Slint platform, and because the
//! killed inspect process is found as this process's child.

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Key, Platform, WindowAdapter, WindowEvent};
use slint::{ComponentHandle, Model, PhysicalSize, PlatformError, Rgb8Pixel, SharedString};
use studio_app::analysis::{Input, Phase, Position, ReadEnd};
use studio_app::engine::{Delivery, ENGINE_NAME};
use studio_app::jobs::State;
use studio_app::journal;
use studio_app::queue::ItemState;
use studio_app::session::{Route, Session, UiEffect};
use studio_app::state::{AppState, Intent, JournalProblem};
use studio_app::workspace::Workspace;
use studio_app::{AnalysisState, AppWindow, Opening, Prepared};
use studio_domain::analysis::{ProblemKind, ScanOutcome};
use studio_domain::protocol::BuildIdentity;

/// Long enough for a debug-build encode of a small clip.
const JOB_TIMEOUT: Duration = Duration::from_secs(600);
const ANSWER_TIMEOUT: Duration = Duration::from_secs(60);
const SIZE: (u32, u32) = (900, 680);

struct Headless(Rc<MinimalSoftwareWindow>);

impl Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.0.clone())
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

/// A fresh directory: `media/` for sources and outputs, `data/` as the
/// app's data folder.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("studio-app-jobs-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for sub in ["media", "data"] {
            fs::create_dir_all(dir.join(sub)).unwrap_or_else(|e| unreachable!("{e}"));
        }
        Self(dir)
    }

    fn media(&self, name: &str) -> PathBuf {
        self.0.join("media").join(name)
    }

    fn data(&self) -> PathBuf {
        self.0.join("data")
    }

    fn journal(&self) -> PathBuf {
        self.data().join(journal::FILE_NAME)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        #[cfg(unix)]
        set_read_only(&self.data(), false);
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(unix)]
fn set_read_only(dir: &Path, read_only: bool) {
    use std::os::unix::fs::PermissionsExt;
    let mode = if read_only { 0o555 } else { 0o755 };
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(mode));
}

/// A progressive 8-bit 4:2:0 Y4M with a moving pattern.
fn write_y4m(path: &Path, width: usize, height: usize, frames: usize) {
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

/// A 32 MiB Annex B stream, a temporal delimiter then padding OBUs of
/// 1 MiB: reading it takes long enough to be cancelled.
fn write_long_stream(path: &Path) {
    let mut bytes = vec![0x01, 0x08];
    for _ in 0..32 {
        bytes.extend([0x81, 0x80, 0x40, 0x64]);
        bytes.resize(bytes.len() + (1 << 20), 0);
    }
    fs::write(path, bytes).unwrap_or_else(|e| unreachable!("{e}"));
}

/// What the window asked for through its Jobs callbacks.
#[derive(Debug)]
enum Asked {
    Show(i32),
    Cancel(i32),
}

/// The window, the session behind it, and the channels that bring its
/// workers' events and its journal results back.
struct Rig {
    platform: Rc<MinimalSoftwareWindow>,
    window: AppWindow,
    session: Option<Session>,
    rx: Receiver<(Route, Delivery)>,
    saved_tx: Sender<Result<(), String>>,
    saved_rx: Receiver<Result<(), String>>,
    asked: Rc<RefCell<Vec<Asked>>>,
}

impl Rig {
    fn new() -> Self {
        let platform = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(Headless(platform.clone())))
            .unwrap_or_else(|e| unreachable!("{e}"));
        let window = studio_app::new_window().unwrap_or_else(|e| unreachable!("{e}"));
        window.window().set_size(PhysicalSize::new(SIZE.0, SIZE.1));
        window.show().unwrap_or_else(|e| unreachable!("{e}"));
        let asked = Rc::new(RefCell::new(Vec::new()));
        window.on_job_show({
            let asked = asked.clone();
            move |i| asked.borrow_mut().push(Asked::Show(i))
        });
        window.on_job_cancel({
            let asked = asked.clone();
            move |i| asked.borrow_mut().push(Asked::Cancel(i))
        });
        let (_, rx) = mpsc::channel();
        let (saved_tx, saved_rx) = mpsc::channel();
        Self {
            platform,
            window,
            session: None,
            rx,
            saved_tx,
            saved_rx,
            asked,
        }
    }

    /// Starts the app on `scratch`'s data folder as `main` does (without
    /// its instance lock), stopping any session before.
    fn start(&mut self, scratch: &Scratch) {
        self.stop();
        let Opening::Owner(prepared) = studio_app::prepare(Ok(scratch.data())) else {
            unreachable!("the data folder is locked")
        };
        let Prepared {
            journal, startup, ..
        } = *prepared;
        let saved = self.saved_tx.clone();
        let writer = journal.map(|path| {
            journal::Writer::spawn(path, move |result| {
                let _ = saved.send(result);
            })
            .unwrap_or_else(|e| unreachable!("{e}"))
        });
        // A channel per session: a reader thread of the last one that ends
        // late reaches nobody, as in the app, where a process has one.
        let (tx, rx) = mpsc::channel();
        self.rx = rx;
        let post = Arc::new(move |route, delivery| {
            let _ = tx.send((route, delivery));
        });
        let mut session = Session::new(AppState::new(startup), Ok(engine()), post, writer);
        let client = BuildIdentity {
            name: "studio-app-jobs-test".into(),
            version: "0".into(),
        };
        session.start_hello(client, Instant::now());
        self.session = Some(session);
        self.settle("the engine answered", |s| {
            s.state().capabilities().is_some()
        });
        self.dispatch(Intent::PresetEdited(10));
        self.dispatch(Intent::CrfEdited(40));
    }

    /// Quits: the session shuts down; what its workers still send goes to
    /// its own channel and is dropped.
    fn stop(&mut self) {
        if let Some(mut session) = self.session.take() {
            session.shutdown();
        }
        while self.rx.try_recv().is_ok() {}
        while self.saved_rx.try_recv().is_ok() {}
    }

    fn session(&mut self) -> &mut Session {
        self.session
            .as_mut()
            .unwrap_or_else(|| unreachable!("no session"))
    }

    fn dispatch(&mut self, intent: Intent) {
        self.session().dispatch(intent, Instant::now());
    }

    fn input(&mut self, input: Input) -> Vec<UiEffect> {
        self.session().analysis_input(input).ui
    }

    /// Hands the window's Jobs requests, the workers' events and the
    /// journal results to the session until `done` holds, then draws the
    /// window from it.
    fn settle(&mut self, what: &str, done: impl Fn(&Session) -> bool) {
        let deadline = Instant::now() + JOB_TIMEOUT;
        loop {
            let asked: Vec<Asked> = self.asked.borrow_mut().drain(..).collect();
            for asked in asked {
                let now = Instant::now();
                match asked {
                    Asked::Show(i) => self.session().show_job(index(i), now),
                    Asked::Cancel(i) => self.session().cancel_job(index(i), now),
                };
            }
            while let Ok(result) = self.saved_rx.try_recv() {
                self.dispatch(Intent::JournalSaved(result));
            }
            while let Ok((route, delivery)) = self.rx.try_recv() {
                self.session().deliver(route, delivery, Instant::now());
            }
            if done(self.session()) {
                break;
            }
            assert!(Instant::now() < deadline, "waiting for {what}");
            if let Ok((route, delivery)) = self.rx.recv_timeout(Duration::from_millis(20)) {
                self.session().deliver(route, delivery, Instant::now());
            }
        }
        self.render();
    }

    fn render(&self) {
        let Some(session) = &self.session else {
            return;
        };
        let now = Instant::now();
        let window = &self.window;
        studio_app::render(window, session.state(), now);
        studio_app::render_analysis(window, session.analysis());
        studio_app::render_jobs(window, &session.jobs());
    }

    /// `(kind, name, state)` of every job, as the window lists them.
    fn rows(&self) -> Vec<(String, String, String)> {
        self.window
            .get_jobs()
            .iter()
            .map(|r| (r.kind.into(), r.name.into(), r.state.into()))
            .collect()
    }

    fn states(&mut self) -> Vec<State> {
        self.session().jobs().iter().map(|j| j.state).collect()
    }

    fn press(&self, key: impl Into<SharedString>) {
        let text = key.into();
        let window = self.window.window();
        window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        window.dispatch_event(WindowEvent::KeyReleased { text });
    }

    fn pixels(&self) -> Vec<Rgb8Pixel> {
        self.window.window().request_redraw();
        let mut buffer = vec![Rgb8Pixel::default(); (SIZE.0 * SIZE.1) as usize];
        self.platform.draw_if_needed(|renderer| {
            renderer.render(&mut buffer, SIZE.0 as usize);
        });
        buffer
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.stop();
    }
}

fn index(i: i32) -> usize {
    usize::try_from(i).unwrap_or(usize::MAX)
}

fn phase(session: &Session, at: usize) -> Option<&Phase> {
    session
        .analysis()
        .documents()
        .get(at)
        .map(studio_app::analysis::Document::phase)
}

fn reading(session: &Session) -> bool {
    session
        .analysis()
        .documents()
        .iter()
        .any(|d| matches!(d.phase(), Phase::Opening | Phase::Cancelling))
        || session.analysis().waiting()
}

#[test]
fn jobs_and_documents_survive_cancel_close_worker_death_and_restart() {
    let mut rig = Rig::new();
    let scratch = Scratch::new("lifecycle");
    concurrent_encode_and_inspection(&mut rig, &scratch);
    cancel_and_close(&mut rig, &scratch);
    #[cfg(unix)]
    worker_death(&mut rig);
    restart(&mut rig, &scratch);
    corrupt_storage(&mut rig);
    #[cfg(unix)]
    read_only_storage(&mut rig);
}

/// Two files convert in one run; while the second converts, the first
/// one's result is inspected. Jobs lists the three operations, each with
/// its own state, and the window offers them to the keyboard.
fn concurrent_encode_and_inspection(rig: &mut Rig, scratch: &Scratch) {
    let small = scratch.media("small.y4m");
    let large = scratch.media("large.y4m");
    write_y4m(&small, 32, 32, 3);
    write_y4m(&large, 96, 64, 24);
    rig.start(scratch);
    rig.dispatch(Intent::FilesOpened(vec![small, large.clone()]));
    rig.settle("both files read", |s| {
        s.state()
            .items()
            .iter()
            .all(|i| i.state() == ItemState::Waiting)
    });
    rig.dispatch(Intent::ConvertRequested);
    rig.settle("the second file converting", |s| {
        s.state().items()[0].state() == ItemState::Completed
            && s.state().items()[1].state().is_active()
    });
    rig.dispatch(Intent::RowSelected(0));
    let result = rig.session().analyze_result(Instant::now());
    assert!(result.analysis, "Open in Analyze did nothing");
    rig.settle("the result read", |s| {
        matches!(phase(s, 0), Some(Phase::Open(_))) && !s.analysis().waiting()
    });
    // The encode is still running: three jobs, each with its own state.
    assert_eq!(
        rig.states(),
        [State::Completed, State::Running, State::Completed],
        "the encode ended before the inspection; use a longer clip"
    );
    assert_eq!(
        rig.rows(),
        [
            ("Convert".into(), "small.y4m".into(), "Completed".into()),
            ("Convert".into(), "large.y4m".into(), "Running".into()),
            ("Inspect".into(), "small.ivf".into(), "Completed".into()),
        ]
    );
    assert_eq!(rig.window.get_jobs_label(), "Jobs · 1");

    // The keyboard: Jobs is the stop after the workspace switch, and Space
    // opens the list over Analyze; Escape closes it.
    rig.press(Key::Tab);
    rig.press(Key::Tab);
    let closed = rig.pixels();
    rig.press(" ");
    let open = rig.pixels();
    assert!(open != closed, "the Jobs list did not open");
    rig.press(Key::Escape);
    assert!(rig.pixels() != open, "Escape did not close the Jobs list");
    // Show on the running conversion goes to Encode and selects its file.
    rig.window.invoke_job_show(1);
    rig.settle("Show handled", |_| true);
    assert_eq!(rig.session().state().workspace(), Workspace::Encode);
    assert_eq!(rig.session().state().chosen_index(), Some(1));
    // In the list, Close has the focus and the stop before it is the last
    // row's Show: the inspection, back in Analyze.
    rig.window.invoke_show_jobs();
    rig.press(Key::Backtab);
    rig.press(" ");
    rig.settle("Show handled", |_| true);
    assert_eq!(rig.session().state().workspace(), Workspace::Analyze);
    assert_eq!(rig.session().analysis().shown_index(), 0);

    rig.settle("the second file converted", |s| {
        s.state().items()[1].state() == ItemState::Completed
    });
    assert!(scratch.media("large.ivf").is_file());
    assert_eq!(
        rig.states(),
        [State::Completed, State::Completed, State::Completed]
    );
    assert_eq!(rig.window.get_jobs_label(), "Jobs");
}

/// A long read is cancelled from Jobs: the document stays, with what was
/// read. Closing every document lets the inspect process go; a new open
/// starts another.
fn cancel_and_close(rig: &mut Rig, scratch: &Scratch) {
    let long = scratch.media("long.obu");
    write_long_stream(&long);
    rig.input(Input::Open(vec![long]));
    let at = rig.session().jobs().len() - 1;
    assert!(matches!(
        rig.session().jobs()[at].state,
        State::Running | State::Queued
    ));
    rig.window.invoke_job_cancel(i32::try_from(at).unwrap_or(0));
    rig.settle("the cancelled read answered", |s| !reading(s));
    let session = rig.session();
    assert_eq!(
        session.analysis().documents().len(),
        2,
        "the document was deleted"
    );
    let doc = &session.analysis().documents()[1];
    // A real race: cancelled before reading (nothing to keep), while
    // reading (the start kept), or after the read ended (all kept). The
    // document never goes away, and Jobs says what happened.
    let job = session.jobs()[at].state;
    match (doc.phase(), doc.summary().map(|s| &s.outcome)) {
        (Phase::Failed { by, .. }, _) => {
            assert_eq!(*by, ReadEnd::Cancel);
            assert_eq!(job, State::Cancelled);
        }
        (_, Some(ScanOutcome::Stopped { problem })) => {
            assert_eq!(problem.kind, ProblemKind::Cancelled);
            assert!(doc.summary().is_some_and(|s| s.obus >= 1));
            assert_eq!(job, State::Cancelled);
            assert!(session.analysis().can_read_again());
        }
        (_, outcome) => {
            assert_eq!(outcome, Some(&ScanOutcome::Complete));
            assert_eq!(job, State::Completed);
        }
    }
    // The first document is still open there.
    assert!(session.inspecting());

    // Close both documents: no process is kept, and nothing late arrives.
    for _ in 0..2 {
        rig.input(Input::Close);
    }
    assert!(!rig.session().inspecting(), "the inspect process was kept");
    rig.settle("late answers delivered", |s| !s.analysis().waiting());
    assert_eq!(rig.session().analysis().documents().len(), 0);
    assert_eq!(
        rig.window.global::<studio_app::Analysis>().get_state(),
        AnalysisState::Empty
    );
}

/// The inspect process is killed while a document is open: the document
/// says so, its read is interrupted in Jobs, and Reopen in a new process
/// brings back its selection.
#[cfg(unix)]
fn worker_death(rig: &mut Rig) {
    let small = rig.session().state().items()[0].destination().path.clone();
    rig.input(Input::Open(vec![small]));
    rig.settle("the document read", |s| {
        matches!(phase(s, 0), Some(Phase::Open(_))) && !s.analysis().waiting()
    });
    rig.input(Input::RowClicked(2));
    rig.settle("the selection", |s| {
        s.analysis().shown().and_then(|d| d.selected()).is_some() && !s.analysis().waiting()
    });
    let pid = inspect_child();
    let killed = std::process::Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status();
    assert!(killed.is_ok_and(|s| s.success()), "could not kill {pid}");
    rig.settle("the death noticed", |s| {
        matches!(phase(s, 0), Some(Phase::Failed { .. }))
    });
    assert!(matches!(
        phase(rig.session(), 0),
        Some(Phase::Failed {
            by: ReadEnd::EngineStopped,
            ..
        })
    ));
    assert!(!rig.session().inspecting());
    let last = rig.session().jobs().len() - 1;
    assert_eq!(rig.session().jobs()[last].state, State::Interrupted);
    assert_eq!(rig.rows()[last].2, "Interrupted");
    rig.input(Input::Reopen);
    rig.settle("the document read again", |s| {
        s.analysis()
            .shown()
            .and_then(|d| d.selected())
            .is_some_and(|s| s.position == Position::Row(2))
            && !s.analysis().waiting()
    });
    assert!(rig.session().inspecting());
    assert!(inspect_child() != pid, "the dead process came back");
}

/// This process's one child that runs the inspect engine, once there is
/// exactly one (a released one may take a moment to exit).
#[cfg(unix)]
fn inspect_child() -> u32 {
    let deadline = Instant::now() + ANSWER_TIMEOUT;
    loop {
        if let Some(pid) = inspect_children().and_then(|c| (c.len() == 1).then(|| c[0])) {
            return pid;
        }
        assert!(Instant::now() < deadline, "not one inspect process");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
fn inspect_children() -> Option<Vec<u32>> {
    let listing = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,args="])
        .output()
        .ok()?;
    let me = std::process::id();
    let children: Vec<u32> = String::from_utf8_lossy(&listing.stdout)
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let pid = words.next()?.parse::<u32>().ok()?;
            let parent = words.next()?.parse::<u32>().ok()?;
            let args: Vec<&str> = words.collect();
            let inspect = args.first().is_some_and(|a| a.contains(ENGINE_NAME))
                && args.get(1) == Some(&"inspect");
            (parent == me && inspect).then_some(pid)
        })
        .collect();
    Some(children)
}

/// Quits with documents open in Analyze and starts again, three times:
/// unchanged files come back with their view; a replaced file waits until
/// it is read as new; a moved one waits until it is located, and is checked
/// there. A read under way at a crash is read again from the start.
fn restart(rig: &mut Rig, scratch: &Scratch) {
    let small = scratch.media("small.ivf");
    let large = scratch.media("large.ivf");
    rig.input(Input::Close);
    rig.dispatch(Intent::WorkspaceRequested(Workspace::Analyze));
    rig.input(Input::Open(vec![large.clone(), small.clone()]));
    rig.settle("both read", |s| !reading(s));
    rig.input(Input::RowClicked(3));
    rig.input(Input::ToggleBytes);
    rig.settle("the view set", |s| {
        s.analysis().shown().and_then(|d| d.selected()).is_some() && !s.analysis().waiting()
    });
    let selected = |s: &Session| {
        s.analysis()
            .shown()
            .and_then(|d| d.selected())
            .map(|s| s.row.index)
    };
    let chosen = selected(rig.session());

    // Unchanged: Analyze is shown again and each file is read before its
    // view comes back.
    rig.start(scratch);
    assert_eq!(rig.session().state().workspace(), Workspace::Analyze);
    rig.settle("the documents read again", |s| {
        !reading(s) && selected(s).is_some()
    });
    let analysis = rig.session().analysis();
    assert_eq!(analysis.documents().len(), 2);
    assert_eq!(analysis.shown_index(), 1);
    assert_eq!(selected(rig.session()), chosen);
    assert!(
        !rig.session()
            .analysis()
            .shown()
            .is_some_and(|d| d.bytes().shown)
    );

    // Replaced, and moved, while the app was closed.
    rig.stop();
    fs::write(&large, b"DKIF but not the same file").unwrap_or_else(|e| unreachable!("{e}"));
    let moved = scratch.media("moved.ivf");
    fs::rename(&small, &moved).unwrap_or_else(|e| unreachable!("{e}"));
    rig.start(scratch);
    rig.settle("the files checked", |s| !reading(s));
    let session = rig.session();
    assert!(matches!(phase(session, 0), Some(Phase::Changed(_))));
    assert_eq!(phase(session, 1), Some(&Phase::Missing));
    assert!(
        session
            .analysis()
            .documents()
            .iter()
            .all(|d| d.summary().is_none())
    );
    assert_eq!(
        rig.window.global::<studio_app::Analysis>().get_state(),
        AnalysisState::Attention
    );
    // Locate: the session asks where it is; there it is the same file,
    // checked by the engine, so its view comes back.
    assert_eq!(rig.input(Input::Resolve), [UiEffect::ShowLocateDialog]);
    rig.session().locate_picked(Some(moved));
    rig.settle("the moved file read", |s| {
        !reading(s) && selected(s).is_some()
    });
    assert_eq!(selected(rig.session()), chosen);
    // The replaced one is read as new only on request, with no old view.
    rig.input(Input::Show(0));
    rig.input(Input::Resolve);
    rig.settle("read as new", |s| !reading(s));
    assert!(matches!(phase(rig.session(), 0), Some(Phase::Open(_))));
    assert_eq!(selected(rig.session()), None);

    // A crash during a read: the journal has the document and no part of
    // its read, and the next start reads the whole file again.
    let long = scratch.media("long.obu");
    rig.input(Input::Open(vec![long]));
    let at_crash = wait_for_journal(scratch, "long.obu");
    rig.stop();
    fs::write(scratch.journal(), at_crash).unwrap_or_else(|e| unreachable!("{e}"));
    rig.start(scratch);
    rig.settle("the long file read", |s| !reading(s));
    let long_doc = rig
        .session()
        .analysis()
        .documents()
        .iter()
        .find(|d| d.path().ends_with("long.obu"))
        .and_then(|d| d.summary().cloned())
        .unwrap_or_else(|| unreachable!("the long file was not read again"));
    assert_eq!(
        (long_doc.obus, long_doc.outcome),
        (33, ScanOutcome::Complete)
    );
}

/// Waits until the journal on disk contains `needle`.
fn wait_for_journal(scratch: &Scratch, needle: &str) -> String {
    let deadline = Instant::now() + ANSWER_TIMEOUT;
    loop {
        let text = fs::read_to_string(scratch.journal()).unwrap_or_default();
        if text.contains(needle) {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "the journal never said {needle:?}: {text}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A journal that does not parse restores nothing and says so; the file
/// is kept beside it.
fn corrupt_storage(rig: &mut Rig) {
    let scratch = Scratch::new("corrupt");
    fs::write(scratch.journal(), br#"{"version": 2, "items": "#)
        .unwrap_or_else(|e| unreachable!("{e}"));
    rig.start(&scratch);
    let session = rig.session();
    assert_eq!(session.state().workspace(), Workspace::Encode);
    assert_eq!(session.analysis().documents().len(), 0);
    assert!(session.state().restored().is_none());
    assert!(matches!(
        session.state().journal_problem(),
        Some(JournalProblem::MovedAside(_))
    ));
    assert!(scratch.data().join("queue.json.corrupt-1").is_file());
    rig.stop();
}

/// A data folder that cannot be written: what was saved is restored and
/// checked as usual, but no save is claimed; Encode and Analyze both say
/// that nothing is saved, and the journal stays as it was.
#[cfg(unix)]
fn read_only_storage(rig: &mut Rig) {
    let scratch = Scratch::new("read-only");
    let stream = scratch.media("small.obu");
    fs::write(&stream, [0x01, 0x08]).unwrap_or_else(|e| unreachable!("{e}"));
    rig.start(&scratch);
    rig.dispatch(Intent::WorkspaceRequested(Workspace::Analyze));
    rig.input(Input::Open(vec![stream.clone()]));
    rig.settle("read", |s| !reading(s));
    rig.stop();
    let before = fs::read(scratch.journal()).unwrap_or_default();
    assert!(String::from_utf8_lossy(&before).contains("small.obu"));
    set_read_only(&scratch.data(), true);
    rig.start(&scratch);
    rig.settle("read again", |s| !reading(s));
    assert!(matches!(phase(rig.session(), 0), Some(Phase::Open(_))));
    rig.input(Input::Close);
    rig.settle("the save failed", |s| s.state().save_error().is_some());
    assert!(
        rig.session()
            .analysis()
            .notice_line()
            .is_some_and(|n| n.contains("cannot be saved")),
        "{:?}",
        rig.session().analysis().notice_line()
    );
    assert_eq!(fs::read(scratch.journal()).unwrap_or_default(), before);
    rig.stop();
    set_read_only(&scratch.data(), false);
}
