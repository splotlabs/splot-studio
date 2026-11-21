// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The durable queue against the real engine, real files and real
//! processes: runs, a killed worker, a crashed app, an interrupted
//! publication, taken destinations, a changed source and a second instance.
//!
//! The session is driven as the window drives it, with worker events
//! brought back over a channel instead of the Slint event loop. Every test
//! has its own data folder; the developer's real one is never touched.
//! The engine is looked up next to the app binary, as in `worker.rs`; build
//! it first (`cargo build -p studio-engine`). Ordinary media files need the
//! `ffmpeg` on `PATH`.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use studio_app::engine::{Delivery, ENGINE_NAME};
use studio_app::instance::{self, Acquired, DATA_DIR_VAR};
use studio_app::journal::{self, Loaded};
use studio_app::queue::{Cause, Ending, Interruption, ItemState, PartialFate};
use studio_app::session::{Route, Session};
use studio_app::state::{AppState, Intent, Startup};
use studio_app::{Opening, Prepared};
use studio_domain::job::{FailureCode, Stage};
use studio_domain::protocol::{BuildIdentity, Event};

/// Long enough for a debug-build encode of a tiny clip.
const JOB_TIMEOUT: Duration = Duration::from_secs(600);
const ANSWER_TIMEOUT: Duration = Duration::from_secs(60);

fn app_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_splot-studio"))
}

fn engine() -> PathBuf {
    let path =
        app_binary().with_file_name(format!("{ENGINE_NAME}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.is_file(),
        "build studio-engine first (`cargo build -p studio-engine`, same profile): no engine \
         at {}",
        path.display()
    );
    path
}

/// A fresh directory for one test: `media/` for sources and outputs,
/// `data/` as the app's data folder.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "studio-app-queue-{}-{}-{name}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
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

    fn media_entries(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.0.join("media")).map_or_else(
            |_| Vec::new(),
            |dir| {
                dir.filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            },
        );
        names.sort();
        names
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
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

/// A tiny MP4 (six 64×48 MPEG-4 Part 2 frames) with one AAC track.
fn write_mp4(path: &Path) {
    let output = Process::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc2=size=64x48:rate=25"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"])
        .args([
            "-c:a",
            "aac",
            "-shortest",
            "-frames:v",
            "6",
            "-c:v",
            "mpeg4",
        ])
        .args(["-pix_fmt", "yuv420p"])
        .arg(path)
        .output()
        .unwrap_or_else(|e| unreachable!("these tests need FFmpeg on PATH ({e})"));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A session over a private data folder, its deliveries on a channel.
struct Harness {
    session: Session,
    rx: Receiver<(Route, Delivery)>,
    _instance: Option<instance::Instance>,
}

impl Harness {
    /// Starts like the app: the data folder's journal is read and
    /// reconciled (without the instance lock, which tests take separately).
    fn open(scratch: &Scratch) -> Self {
        let prepared = Prepared {
            instance: None,
            journal: Some(scratch.journal()),
            startup: startup(scratch),
        };
        Self::with(prepared)
    }

    fn with(prepared: Prepared) -> Self {
        let (tx, rx) = mpsc::channel();
        let post = Arc::new(move |route, delivery| {
            let _ = tx.send((route, delivery));
        });
        let writer = prepared.journal.map(|path| {
            journal::Writer::spawn(path, |_| {}).unwrap_or_else(|e| unreachable!("{e}"))
        });
        let state = AppState::new(prepared.startup);
        let session = Session::new(state, Ok(engine()), post, writer);
        let mut harness = Self {
            session,
            rx,
            _instance: prepared.instance,
        };
        let client = BuildIdentity {
            name: "studio-app-queue-test".into(),
            version: "0".into(),
        };
        harness.session.start_hello(client, Instant::now());
        harness.pump_until("the engine answered", ANSWER_TIMEOUT, |s| {
            s.capabilities().is_some()
        });
        // Fast, small settings for every test.
        harness.dispatch(Intent::PresetEdited(10));
        harness.dispatch(Intent::CrfEdited(40));
        harness
    }

    fn dispatch(&mut self, intent: Intent) {
        self.session.dispatch(intent, Instant::now());
    }

    fn state(&self) -> &AppState {
        self.session.state()
    }

    /// Delivers worker events until `done` holds.
    fn pump_until(&mut self, what: &str, timeout: Duration, done: impl Fn(&AppState) -> bool) {
        self.pump_inspecting(what, timeout, |_, _| {}, done);
    }

    /// As [`Self::pump_until`], showing every delivery to `seen` first.
    fn pump_inspecting(
        &mut self,
        what: &str,
        timeout: Duration,
        mut seen: impl FnMut(&Route, &Delivery),
        done: impl Fn(&AppState) -> bool,
    ) {
        let deadline = Instant::now() + timeout;
        while !done(self.session.state()) {
            let left = deadline.saturating_duration_since(Instant::now());
            let (route, delivery) = self
                .rx
                .recv_timeout(left)
                .unwrap_or_else(|e| unreachable!("waiting for {what}: {e}"));
            seen(&route, &delivery);
            self.session.deliver(route, delivery, Instant::now());
        }
    }

    fn add(&mut self, files: &[PathBuf]) {
        self.dispatch(Intent::FilesOpened(files.to_vec()));
        self.pump_until("every file read", ANSWER_TIMEOUT, |s| {
            s.items().iter().all(|i| i.state() != ItemState::Probing)
        });
    }

    fn states(&self) -> Vec<ItemState> {
        self.state()
            .items()
            .iter()
            .map(studio_app::queue::Item::state)
            .collect()
    }

    /// Simulates the app dying now: the journal as it is on disk at this
    /// moment is what the next start finds. The session is then closed (so
    /// no process is left behind) and the journal put back as it was.
    fn die(self, scratch: &Scratch, needle: &str) {
        let at_death = Self::wait_for_journal(scratch, needle);
        drop(self);
        fs::write(scratch.journal(), at_death).unwrap_or_else(|e| unreachable!("{e}"));
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
}

/// What the app's startup makes of the data folder now.
fn startup(scratch: &Scratch) -> Startup {
    match studio_app::prepare(Ok(scratch.data())) {
        Opening::Owner(prepared) => prepared.startup,
        Opening::AlreadyOpen(dir) => unreachable!("{} is locked", dir.display()),
    }
}

/// The worker process that has `partial` open (Linux), found through
/// `/proc`, so parallel tests never kill each other's workers.
#[cfg(target_os = "linux")]
fn worker_holding(partial: &Path) -> Option<u32> {
    for entry in fs::read_dir("/proc").ok()?.filter_map(Result::ok) {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(fds) = fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        if fds
            .filter_map(Result::ok)
            .any(|fd| fs::read_link(fd.path()).is_ok_and(|target| target == partial))
        {
            return Some(pid);
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn kill(pid: u32) {
    let status = Process::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status();
    assert!(status.is_ok_and(|s| s.success()), "could not kill {pid}");
}

fn partial_of(harness: &Harness, index: usize) -> Option<PathBuf> {
    harness.state().items()[index]
        .attempt()
        .and_then(|a| a.partial.clone())
}

#[test]
fn a_run_of_three_files_with_one_unreadable_publishes_two() {
    let scratch = Scratch::new("run");
    let a = scratch.media("a.y4m");
    write_y4m(&a, 64, 36, 3);
    let b = scratch.media("b.y4m");
    fs::write(&b, b"RIFF\0\0\0\0WAVEfmt ").unwrap_or_else(|e| unreachable!("{e}"));
    let c = scratch.media("c.mp4");
    write_mp4(&c);
    let mut h = Harness::open(&scratch);
    h.add(&[a, b, c]);
    assert_eq!(
        h.states(),
        [
            ItemState::Waiting,
            ItemState::Unreadable,
            ItemState::Waiting
        ]
    );
    h.dispatch(Intent::ConvertRequested);
    // c.mp4 has audio: one question for the run.
    let asked = h.state().confirmation().map(|c| c.asked.len());
    assert_eq!(asked, Some(1));
    h.dispatch(Intent::OmissionsConfirmed);
    assert!(h.state().running());
    h.pump_until("the run ended", JOB_TIMEOUT, |s| !s.running());
    assert_eq!(
        h.states(),
        [
            ItemState::Completed,
            ItemState::Unreadable,
            ItemState::Completed
        ]
    );
    assert_eq!(
        scratch.media_entries(),
        ["a.ivf", "a.y4m", "b.y4m", "c.ivf", "c.mp4"]
    );
    for (name, frames) in [("a.ivf", 3), ("c.ivf", 6)] {
        let item = h
            .state()
            .items()
            .iter()
            .find(|i| i.destination().path == scratch.media(name));
        let Some(Ending::Completed(report)) = item.and_then(|i| i.ending()) else {
            unreachable!("{name}: {item:?}")
        };
        assert_eq!(report.frames, frames);
        assert_eq!(
            fs::metadata(scratch.media(name)).map_or(0, |m| m.len()),
            report.bytes
        );
    }
    drop(h);
    // The journal says the same, with the settings the files ran with.
    let Loaded::Found(saved) = journal::load(&scratch.journal()) else {
        unreachable!("no journal")
    };
    let kinds: Vec<String> = saved
        .items
        .iter()
        .map(|i| {
            serde_json::to_value(&i.state)
                .ok()
                .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_owned))
                .unwrap_or_default()
        })
        .collect();
    assert_eq!(kinds, ["completed", "unreadable", "completed"]);
    assert_eq!(saved.settings.preset.get(), 10);
}

// Linux only: the worker is found (and killed) through `/proc`.
#[cfg(target_os = "linux")]
#[test]
fn a_killed_worker_is_lost_and_its_temporary_file_removed() {
    let scratch = Scratch::new("killed");
    let source = scratch.media("long.y4m");
    write_y4m(&source, 320, 180, 60);
    let mut h = Harness::open(&scratch);
    h.dispatch(Intent::PresetEdited(0));
    h.add(&[source]);
    h.dispatch(Intent::ConvertRequested);
    h.pump_until("the temporary file", JOB_TIMEOUT, |s| {
        s.items()[0].attempt().is_some_and(|a| a.partial.is_some())
    });
    let partial = partial_of(&h, 0).unwrap_or_default();
    assert!(partial.is_file(), "{}", partial.display());
    let pid = worker_holding(&partial).unwrap_or_else(|| unreachable!("no worker holds it"));
    kill(pid);
    h.pump_until("the worker ended", ANSWER_TIMEOUT, |s| {
        s.items()[0].state() == ItemState::Lost
            && !matches!(
                s.items()[0].ending().and_then(Ending::partial),
                Some(PartialFate::Pending(_))
            )
    });
    assert_eq!(
        h.state().items()[0].ending().and_then(Ending::partial),
        Some(&PartialFate::Removed(partial.clone()))
    );
    assert_eq!(scratch.media_entries(), ["long.y4m"]);
    assert!(h.state().items()[0].state().can_retry());
}

// Linux only: the worker is found (and killed) through `/proc`.
#[cfg(target_os = "linux")]
#[test]
fn after_a_crash_with_a_killed_worker_the_reload_cleans_up_and_retry_converts() {
    let scratch = Scratch::new("crash-kill");
    let source = scratch.media("long.y4m");
    write_y4m(&source, 320, 180, 60);
    let original = fs::read(&source).unwrap_or_default();
    let mut h = Harness::open(&scratch);
    h.dispatch(Intent::PresetEdited(0));
    h.add(std::slice::from_ref(&source));
    h.dispatch(Intent::ConvertRequested);
    h.pump_until("the temporary file", JOB_TIMEOUT, |s| {
        s.items()[0].attempt().is_some_and(|a| a.partial.is_some())
    });
    let partial = partial_of(&h, 0).unwrap_or_default();
    // The worker and the app die together: nothing more is delivered or
    // saved, and nobody removes the temporary file.
    let pid = worker_holding(&partial).unwrap_or_else(|| unreachable!("no worker holds it"));
    kill(pid);
    h.die(&scratch, &partial.to_string_lossy());
    assert!(partial.is_file());

    let mut h = Harness::open(&scratch);
    let item = &h.state().items()[0];
    assert_eq!(item.state(), ItemState::Interrupted);
    assert_eq!(
        item.ending(),
        Some(&Ending::Interrupted(Interruption {
            publishing: false,
            cause: Cause::AppStopped,
            destination_present: false,
            partial: PartialFate::Removed(partial.clone()),
        }))
    );
    assert_eq!(scratch.media_entries(), ["long.y4m"]);
    assert!(h.state().paused() || !h.state().running());
    // Nothing started by itself; Convert retries it from the start.
    assert!(!h.state().running());
    h.dispatch(Intent::PresetEdited(10));
    h.dispatch(Intent::ConvertRequested);
    h.pump_until("the retry ended", JOB_TIMEOUT, |s| !s.running());
    let Some(Ending::Completed(report)) = h.state().items()[0].ending() else {
        unreachable!("{:?}", h.state().items()[0])
    };
    assert_eq!(report.frames, 60);
    assert_eq!(scratch.media_entries(), ["long.ivf", "long.y4m"]);
    assert_eq!(fs::read(&source).unwrap_or_default(), original);
}

/// Run as a child process by the crash test: converts the file named by
/// `STUDIO_CRASH_SOURCE` with the data folder in `STUDIO_CRASH_DATA`,
/// writes the temporary file's path to `STUDIO_CRASH_READY` once the
/// journal records it, and waits to be killed.
#[test]
#[ignore = "a helper process for app_crash_leaves_an_interrupted_item_and_no_partial"]
fn crash_helper() {
    let (Some(data), Some(source), Some(ready)) = (
        std::env::var_os("STUDIO_CRASH_DATA"),
        std::env::var_os("STUDIO_CRASH_SOURCE"),
        std::env::var_os("STUDIO_CRASH_READY"),
    ) else {
        return;
    };
    let data = PathBuf::from(data);
    let prepared = match studio_app::prepare(Ok(data.clone())) {
        Opening::Owner(prepared) => prepared,
        Opening::AlreadyOpen(_) => unreachable!("locked"),
    };
    let mut h = Harness::with(prepared);
    h.dispatch(Intent::PresetEdited(0));
    h.dispatch(Intent::FilesOpened(vec![PathBuf::from(source)]));
    h.pump_until("read", ANSWER_TIMEOUT, |s| {
        s.items().iter().all(|i| i.state() != ItemState::Probing)
    });
    h.dispatch(Intent::ConvertRequested);
    h.pump_until("the temporary file", JOB_TIMEOUT, |s| {
        s.items()[0].attempt().is_some_and(|a| a.partial.is_some())
    });
    let partial = partial_of(&h, 0).unwrap_or_default();
    let needle = partial.to_string_lossy().into_owned();
    let deadline = Instant::now() + ANSWER_TIMEOUT;
    while !fs::read_to_string(data.join(journal::FILE_NAME))
        .unwrap_or_default()
        .contains(&needle)
    {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    // Written whole, then renamed, so the test never reads half a path.
    let temp = PathBuf::from(&ready).with_extension("tmp");
    fs::write(&temp, needle).unwrap_or_else(|e| unreachable!("{e}"));
    fs::rename(&temp, &ready).unwrap_or_else(|e| unreachable!("{e}"));
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

// Unix only for now: on the Windows CI runner the re-executed helper process
// never reports ready, and that has not been investigated on a real machine.
#[cfg_attr(
    windows,
    ignore = "the helper process never reports ready on the Windows runner"
)]
#[test]
fn app_crash_leaves_an_interrupted_item_and_no_partial() {
    let scratch = Scratch::new("crash");
    let source = scratch.media("long.y4m");
    write_y4m(&source, 320, 180, 60);
    let original = fs::read(&source).unwrap_or_default();
    let ready = scratch.0.join("ready");
    let mut child = Process::new(std::env::current_exe().unwrap_or_default())
        .args([
            "--ignored",
            "--exact",
            "crash_helper",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("STUDIO_CRASH_DATA", scratch.data())
        .env("STUDIO_CRASH_SOURCE", &source)
        .env("STUDIO_CRASH_READY", &ready)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| unreachable!("{e}"));
    let deadline = Instant::now() + JOB_TIMEOUT;
    let partial = loop {
        if let Ok(text) = fs::read_to_string(&ready)
            && !text.is_empty()
        {
            break PathBuf::from(text);
        }
        assert!(
            Instant::now() < deadline && child.try_wait().ok().flatten().is_none(),
            "the helper never got going"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    // The app dies without any cleanup (SIGKILL). Its worker sees its input
    // close, cancels cooperatively and removes its own temporary file.
    assert!(child.kill().is_ok());
    let _ = child.wait();
    let deadline = Instant::now() + ANSWER_TIMEOUT;
    while partial.exists() {
        assert!(
            Instant::now() < deadline,
            "the worker left {}",
            partial.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // Windows releases a dead process's file lock "depending on available
    // system resources"; give it a moment rather than fail on timing.
    let deadline = Instant::now() + ANSWER_TIMEOUT;
    let startup = loop {
        match studio_app::prepare(Ok(scratch.data())) {
            Opening::Owner(prepared) => break prepared.startup,
            Opening::AlreadyOpen(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Opening::AlreadyOpen(dir) => unreachable!("{} is still locked", dir.display()),
        }
    };
    let item = startup
        .recovered
        .as_ref()
        .and_then(|r| r.items.first())
        .unwrap_or_else(|| unreachable!("nothing restored"));
    assert_eq!(item.state(), ItemState::Interrupted);
    assert!(matches!(
        item.ending(),
        Some(Ending::Interrupted(Interruption {
            cause: Cause::AppStopped,
            publishing: false,
            destination_present: false,
            partial: PartialFate::Gone,
        }))
    ));
    assert_eq!(scratch.media_entries(), ["long.y4m"]);
    assert_eq!(fs::read(&source).unwrap_or_default(), original);
}

#[test]
fn a_publication_cut_short_is_reported_honestly_and_never_overwritten() {
    let scratch = Scratch::new("publishing");
    let source = scratch.media("clip.y4m");
    write_y4m(&source, 64, 36, 3);
    let mut h = Harness::open(&scratch);
    h.add(&[source]);
    h.dispatch(Intent::ConvertRequested);
    let published = Arc::new(Mutex::new(false));
    let seen = Arc::clone(&published);
    // Deliver up to and including the publication stage, then the app
    // "dies": the engine still finishes and publishes the file.
    h.pump_inspecting(
        "the publication",
        JOB_TIMEOUT,
        move |_, delivery| {
            if let Delivery::Event(e) = delivery
                && e.body
                    == (Event::Stage {
                        stage: Stage::Publishing,
                    })
                && let Ok(mut seen) = seen.lock()
            {
                *seen = true;
            }
        },
        |s| s.items()[0].state() == ItemState::Publishing,
    );
    assert!(published.lock().is_ok_and(|p| *p));
    // Closing the session lets the worker, which has claimed publication,
    // finish it.
    h.die(&scratch, "\"kind\": \"publishing\"");
    let destination = scratch.media("clip.ivf");
    assert!(destination.exists());
    let before = fs::read(&destination).unwrap_or_default();
    assert_ne!(before, [] as [u8; 0]);

    let mut h = Harness::open(&scratch);
    let item = &h.state().items()[0];
    assert_eq!(item.state(), ItemState::Interrupted);
    assert!(matches!(
        item.ending(),
        Some(Ending::Interrupted(Interruption {
            publishing: true,
            destination_present: true,
            ..
        }))
    ));
    let text = studio_app::present::queue::detail(item, "Retry");
    assert!(
        text.contains("while the file was being published; a file exists at")
            && text.contains("check it"),
        "{text}"
    );
    assert!(!text.contains("Completed"));
    // A retry writes to a new name; the file that is there is untouched.
    h.dispatch(Intent::ConvertRequested);
    h.pump_until("the retry ended", JOB_TIMEOUT, |s| !s.running());
    assert_eq!(h.states(), [ItemState::Completed]);
    assert_eq!(
        h.state().items()[0].destination().path,
        scratch.media("clip (2).ivf")
    );
    assert_eq!(fs::read(&destination).unwrap_or_default(), before);
    assert_eq!(
        scratch.media_entries(),
        ["clip (2).ivf", "clip.ivf", "clip.y4m"]
    );
}

#[test]
fn taken_destinations_get_new_names_and_are_never_replaced() {
    let scratch = Scratch::new("taken");
    let a = scratch.media("a.y4m");
    let b = scratch.media("b.y4m");
    write_y4m(&a, 64, 36, 2);
    write_y4m(&b, 64, 36, 2);
    // Taken before the file is added: the default becomes a new name.
    fs::write(scratch.media("a.ivf"), b"precious a").unwrap_or_else(|e| unreachable!("{e}"));
    let mut h = Harness::open(&scratch);
    h.add(&[a, b]);
    assert_eq!(
        h.state().items()[0].destination().path,
        scratch.media("a (2).ivf")
    );
    // Taken after it was added: that file waits ("output exists") and the
    // run converts the others.
    fs::write(scratch.media("b.ivf"), b"precious b").unwrap_or_else(|e| unreachable!("{e}"));
    h.dispatch(Intent::ConvertRequested);
    h.pump_until("the run ended", JOB_TIMEOUT, |s| !s.running());
    assert_eq!(h.states(), [ItemState::Completed, ItemState::Waiting]);
    assert!(h.state().items()[1].destination().exists);
    assert_eq!(
        fs::read(scratch.media("a.ivf")).ok().as_deref(),
        Some(&b"precious a"[..])
    );
    assert_eq!(
        fs::read(scratch.media("b.ivf")).ok().as_deref(),
        Some(&b"precious b"[..])
    );

    // Taken between attempts: the retry writes to a new name.
    let scratch = Scratch::new("taken-retry");
    let source = scratch.media("long.y4m");
    write_y4m(&source, 320, 180, 60);
    let mut h = Harness::open(&scratch);
    h.dispatch(Intent::PresetEdited(0));
    h.add(&[source]);
    h.dispatch(Intent::ConvertRequested);
    h.pump_until("running", JOB_TIMEOUT, |s| {
        s.items()[0].attempt().is_some_and(|a| a.partial.is_some())
    });
    h.dispatch(Intent::CancelRequested);
    h.pump_until("cancelled", JOB_TIMEOUT, |s| !s.running());
    assert_eq!(h.states(), [ItemState::Cancelled]);
    fs::write(scratch.media("long.ivf"), b"someone else's").unwrap_or_else(|e| unreachable!("{e}"));
    h.dispatch(Intent::PresetEdited(10));
    h.dispatch(Intent::ConvertRequested);
    h.pump_until("the retry ended", JOB_TIMEOUT, |s| !s.running());
    assert_eq!(h.states(), [ItemState::Completed]);
    assert_eq!(
        scratch.media_entries(),
        ["long (2).ivf", "long.ivf", "long.y4m"]
    );
    assert_eq!(
        fs::read(scratch.media("long.ivf")).ok().as_deref(),
        Some(&b"someone else's"[..])
    );
}

#[test]
fn a_source_changed_before_its_start_is_read_again_and_asked_about_again() {
    let scratch = Scratch::new("changed");
    let source = scratch.media("clip.mp4");
    write_mp4(&source);
    let mut h = Harness::open(&scratch);
    h.add(std::slice::from_ref(&source));
    let before = h.state().items()[0].facts().map(|f| f.fingerprint);
    // Edited after it was read, before Convert.
    let mut bytes = fs::read(&source).unwrap_or_default();
    bytes.extend_from_slice(&[0; 16]);
    fs::write(&source, bytes).unwrap_or_else(|e| unreachable!("{e}"));
    h.dispatch(Intent::ConvertRequested);
    h.dispatch(Intent::OmissionsConfirmed);
    assert!(h.state().items()[0].acknowledgement().is_some());
    h.pump_until("refused and read again", JOB_TIMEOUT, |s| {
        !s.running() && s.items()[0].state() == ItemState::Waiting
    });
    let item = &h.state().items()[0];
    assert!(
        matches!(item.ending(), Some(Ending::Failed(f)) if f.code == FailureCode::SourceChanged),
        "{item:?}"
    );
    assert_ne!(item.facts().map(|f| f.fingerprint), before);
    assert_eq!(
        item.acknowledgement(),
        None,
        "the acknowledgement was for the old file"
    );
    assert_eq!(scratch.media_entries(), ["clip.mp4"]);
    // Converting again asks again.
    h.dispatch(Intent::ConvertRequested);
    assert!(h.state().confirmation().is_some());
}

#[test]
fn a_second_app_process_says_already_open_and_leaves_the_journal_alone() {
    let scratch = Scratch::new("second");
    let source = scratch.media("a.y4m");
    write_y4m(&source, 64, 36, 2);
    // This process is the first instance: it holds the lock and a journal.
    let Acquired::Owner(owner) = instance::acquire(&scratch.data()) else {
        unreachable!("could not take the lock")
    };
    {
        let mut h = Harness::with(Prepared {
            instance: None,
            journal: Some(scratch.journal()),
            startup: Startup::default(),
        });
        h.add(std::slice::from_ref(&source));
    }
    let journal_before = fs::read(scratch.journal()).unwrap_or_default();
    let _ = &owner;
    assert_ne!(journal_before, [] as [u8; 0]);
    let entries = |dir: &Path| {
        let mut names: Vec<String> = fs::read_dir(dir).map_or_default(|d| {
            d.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        });
        names.sort();
        names
    };
    let data_before = entries(&scratch.data());

    let mut second = Process::new(app_binary())
        .arg(scratch.media("other.y4m"))
        .env(DATA_DIR_VAR, scratch.data())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| unreachable!("{e}"));
    let mut stderr = second.stderr.take().unwrap_or_else(|| unreachable!());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let mut buf = [0_u8; 256];
        while let Ok(n) = stderr.read(&mut buf) {
            if n == 0 {
                break;
            }
            text.push_str(&String::from_utf8_lossy(&buf[..n]));
            if text.contains("already open") {
                let _ = tx.send(text.clone());
            }
        }
    });
    let said = rx.recv_timeout(ANSWER_TIMEOUT);
    // With a display it now shows the small window; without one it exits.
    let exited = second.try_wait().ok().flatten();
    if exited.is_none() {
        let _ = second.kill();
    }
    let _ = second.wait();
    let said = said.unwrap_or_else(|e| unreachable!("the second process said nothing: {e}"));
    assert!(said.contains("Splot Studio is already open"), "{said}");
    if let Some(status) = exited {
        assert_eq!(status.code(), Some(3), "{said}");
    }
    assert_eq!(
        fs::read(scratch.journal()).unwrap_or_default(),
        journal_before
    );
    assert_eq!(entries(&scratch.data()), data_before);
    drop(owner);
}
