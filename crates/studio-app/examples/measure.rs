// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Measures the real window with the real engine (Linux, a display needed;
//! the engine must sit next to this executable, as next to the app):
//!
//! ```text
//! cargo run --release -p studio-app --example measure -- [WIDTHxHEIGHT FRAMES [PASSES]]
//! ```
//!
//! Writes a Y4M of that size into a scratch folder, opens the app on it with
//! a scratch data folder and drives it through its own window callbacks (no
//! OS input): steps through every frame four times, which overfills the picture
//! cache, recording how long each picture took to appear (polled every
//! millisecond) and the memory high-water mark after each pass; then, while
//! the file converts, posts a closure to the event loop every 20 ms from
//! another thread, as input would arrive, and records how long it waited.
//! Idle CPU is measured from outside, on the app itself, so that no timer of
//! this tool wakes it.
//!
//! ```text
//! cargo run --release -p studio-app --example measure -- concurrent [WIDTHxHEIGHT FRAMES OBUS]
//! ```
//!
//! Encode and inspection at the same time (any desktop OS with `ps`): the
//! journal of a scratch data folder restores one Annex B stream of OBUS
//! temporal delimiters as a document not read yet. Once the clip converts,
//! Analyze is shown, so the stream is read while the encoder runs; then the
//! navigator is moved down row by row through its own callback, recording
//! how long each move took until the window showed the new selection
//! (rows are paged from the inspect process, so some moves wait for it).
//! Closures are posted every 20 ms as above, and the resident memory of the
//! app and of each engine process is sampled with `ps` every 100 ms.

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Timer, TimerMode};
use studio_app::engine::ENGINE_NAME;
use studio_app::{AppWindow, JobView, Opening, StageState};

fn status(field: &str) -> String {
    fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .find(|l| l.starts_with(field))
        .map_or_else(String::new, |l| {
            l.split_whitespace().skip(1).collect::<Vec<_>>().join(" ")
        })
}

fn write_y4m(path: &Path, width: usize, height: usize, frames: usize) -> std::io::Result<()> {
    let mut bytes =
        format!("YUV4MPEG2 W{width} H{height} F30000:1001 Ip A1:1 C420jpeg\n").into_bytes();
    let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
    for f in 0..frames {
        bytes.extend_from_slice(b"FRAME\n");
        for (w, h, offset) in [(width, height, 0), (cw, ch, 60), (cw, ch, 120)] {
            for y in 0..h {
                for x in 0..w {
                    bytes.push(u8::try_from(16 + (x + 2 * y + 3 * f + offset) % 220).unwrap_or(16));
                }
            }
        }
    }
    fs::write(path, bytes)
}

fn percentile(sorted: &[Duration], p: usize) -> Duration {
    sorted
        .get(sorted.len().saturating_sub(1) * p / 100)
        .copied()
        .unwrap_or_default()
}

fn summary(times: &mut [Duration]) -> String {
    times.sort();
    format!(
        "p50 {:?}, p95 {:?}, max {:?}",
        percentile(times, 50),
        percentile(times, 95),
        times.last().copied().unwrap_or_default()
    )
}

/// Passes over every frame by default: the first fills the cache, the
/// others show whether memory keeps growing.
const PASSES: u32 = 4;

enum Phase {
    Poster(Instant),
    Step {
        pass: u32,
        frame: i32,
        asked: Instant,
        times: Vec<Duration>,
    },
    Encode(Instant),
}

/// One step of the script; `None` once it is over.
fn advance(
    phase: Phase,
    win: &AppWindow,
    (frames, passes): (i32, u32),
    out: &mut Vec<String>,
) -> Option<Phase> {
    match phase {
        Phase::Poster(started) if win.get_stage_state() == StageState::Picture => {
            out.push(format!(
                "poster shown {:?} after launch (probe included); scale factor {}, picture area \
                 {}x{} px",
                started.elapsed(),
                win.window().scale_factor(),
                win.get_stage_px_width(),
                win.get_stage_px_height()
            ));
            win.invoke_stage_seek(0);
            Some(Phase::Step {
                pass: 1,
                frame: 0,
                asked: Instant::now(),
                times: Vec::new(),
            })
        }
        Phase::Step {
            pass,
            frame,
            asked,
            mut times,
        } if win
            .get_stage_frame_text()
            .starts_with(&format!("{} / ", frame + 1)) =>
        {
            times.push(asked.elapsed());
            if frame + 1 < frames {
                win.invoke_stage_step(1);
                return Some(Phase::Step {
                    pass,
                    frame: frame + 1,
                    asked: Instant::now(),
                    times,
                });
            }
            out.push(format!(
                "pass {pass}: {} frames stepped, picture shown after {}; RSS {}, high-water {}",
                times.len(),
                summary(&mut times),
                status("VmRSS"),
                status("VmHWM")
            ));
            if pass < passes {
                win.invoke_stage_seek(0);
                return Some(Phase::Step {
                    pass: pass + 1,
                    frame: 0,
                    asked: Instant::now(),
                    times: Vec::new(),
                });
            }
            // Fast: the codec's quickest preset.
            win.invoke_priority_selected(0);
            win.invoke_encode_requested();
            Some(Phase::Encode(Instant::now()))
        }
        Phase::Encode(from) => {
            let running = matches!(win.get_job_view(), JobView::Running | JobView::Cancelling);
            if from.elapsed() > Duration::from_secs(1)
                && (!running || from.elapsed() > Duration::from_secs(20))
            {
                return None;
            }
            Some(Phase::Encode(from))
        }
        other => Some(other),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "concurrent") {
        return concurrent(&args[1..]);
    }
    let (size, frames, passes) = match args.as_slice() {
        [size, frames] => (size.clone(), frames.parse::<usize>()?, PASSES),
        [size, frames, passes] => (size.clone(), frames.parse()?, passes.parse()?),
        _ => ("640x360".to_owned(), 150, PASSES),
    };
    let (w, h) = size.split_once('x').ok_or("size is WIDTHxHEIGHT")?;
    let (w, h): (usize, usize) = (w.parse()?, h.parse()?);
    let scratch: PathBuf =
        std::env::temp_dir().join(format!("studio-measure-{}", std::process::id()));
    fs::create_dir_all(scratch.join("data"))?;
    let clip = scratch.join("clip.y4m");
    write_y4m(&clip, w, h, frames)?;

    let Opening::Owner(prepared) = studio_app::prepare(Ok(scratch.join("data"))) else {
        return Err("the scratch data folder is taken".into());
    };
    let studio = studio_app::build(*prepared, vec![clip])?;
    let out = Rc::new(RefCell::new(vec![format!(
        "clip {w}x{h}, {frames} frames 8-bit; scale factor {}; picture area {}x{} px",
        studio.window().window().scale_factor(),
        studio.window().get_stage_px_width(),
        studio.window().get_stage_px_height()
    )]));
    let lags = Arc::new(Mutex::new(Vec::<Duration>::new()));
    let posting = Arc::new(AtomicBool::new(false));

    let driver = Timer::default();
    let frames = i32::try_from(frames)?;
    let window = studio.window().as_weak();
    let phase = RefCell::new(Some(Phase::Poster(Instant::now())));
    let (log, flag) = (out.clone(), posting.clone());
    driver.start(TimerMode::Repeated, Duration::from_millis(1), move || {
        let Some(win) = window.upgrade() else { return };
        let Some(current) = phase.borrow_mut().take() else {
            return;
        };
        let next = advance(current, &win, (frames, passes), &mut log.borrow_mut());
        flag.store(matches!(next, Some(Phase::Encode(_))), Ordering::SeqCst);
        if next.is_none() {
            let _ = slint::quit_event_loop();
        }
        *phase.borrow_mut() = next;
    });

    let (store, flag) = (lags.clone(), posting.clone());
    let done = Arc::new(AtomicBool::new(false));
    let stop = done.clone();
    let poster = std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(20));
            if !flag.load(Ordering::SeqCst) {
                continue;
            }
            let sent = Instant::now();
            let store = store.clone();
            let posted = slint::invoke_from_event_loop(move || {
                if let Ok(mut l) = store.lock() {
                    l.push(sent.elapsed());
                }
            });
            if posted.is_err() {
                break;
            }
        }
    });

    studio.run()?;
    done.store(true, Ordering::SeqCst);
    let _ = poster.join();
    let mut waits = lags.lock().map_or_else(|_| Vec::new(), |l| l.clone());
    let mut out = out.borrow_mut();
    out.push(format!(
        "while converting: {} closures posted every 20 ms waited {}; RSS high-water {}",
        waits.len(),
        summary(&mut waits),
        status("VmHWM")
    ));
    for line in out.iter() {
        println!("{line}");
    }
    let _ = fs::remove_dir_all(&scratch);
    Ok(())
}

/// Navigator moves timed while the clip converts.
const MOVES: u32 = 200;

/// Where the concurrent script is.
enum Step {
    /// Waiting for the clip to be read, then converts it.
    Ready,
    /// Converting; shows Analyze once a frame is under way.
    Converting(Instant),
    /// The restored stream is being read.
    Reading(Instant),
    /// Moving down the navigator: `n` moves done.
    Moving {
        n: u32,
        before: slint::SharedString,
        asked: Instant,
        times: Vec<Duration>,
        while_encoding: u32,
    },
    /// Waiting for the conversion to end.
    Draining(Instant),
}

/// Resident memory in KiB of `pid`, and of each engine process it started
/// by its role, from `ps`.
fn resident(pid: u32) -> (u64, Vec<(String, u64)>) {
    let Ok(out) = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,rss=,args="])
        .output()
    else {
        return (0, Vec::new());
    };
    let mut own = 0;
    let mut engines = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        let [p, parent, rss, program, rest @ ..] = words.as_slice() else {
            continue;
        };
        let number = |w: &str| w.parse::<u64>().unwrap_or(0);
        if number(p) == u64::from(pid) {
            own = number(rss);
        } else if number(parent) == u64::from(pid) && program.contains(ENGINE_NAME) {
            let role = rest.first().copied().unwrap_or("?");
            engines.push((format!("{role} {p}"), number(rss)));
        }
    }
    (own, engines)
}

#[allow(clippy::too_many_lines)] // one script, read top to bottom
fn concurrent(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    use slint::ComponentHandle as _;
    use studio_app::journal::{
        self, JournalDocument, JournalOrigin, JournalView, JournalWorkspace,
    };
    use studio_app::{Analysis, AnalysisState, NavStep};
    use studio_domain::job::WirePath;

    let (size, frames, obus) = match args {
        [size, frames, obus] => (
            size.clone(),
            frames.parse::<usize>()?,
            obus.parse::<usize>()?,
        ),
        _ => ("640x360".to_owned(), 300, 200_000),
    };
    let (w, h) = size.split_once('x').ok_or("size is WIDTHxHEIGHT")?;
    let (w, h): (usize, usize) = (w.parse()?, h.parse()?);
    let scratch: PathBuf =
        std::env::temp_dir().join(format!("studio-measure-{}", std::process::id()));
    fs::create_dir_all(scratch.join("data"))?;
    let clip = scratch.join("clip.y4m");
    write_y4m(&clip, w, h, frames)?;
    // Annex B: a one-byte length, then a temporal delimiter header.
    let stream = scratch.join("stream.obu");
    fs::write(&stream, [0x01_u8, 0x08].repeat(obus))?;
    let saved = journal::Journal {
        version: journal::VERSION,
        next_id: 1,
        settings: studio_app::state::INITIAL_SETTINGS,
        items: Vec::new(),
        view: JournalView {
            workspace: JournalWorkspace::Encode,
            shown_document: 0,
            documents: vec![JournalDocument {
                source: WirePath::from_path(&stream),
                origin: JournalOrigin::File,
                read: None,
            }],
        },
    };
    fs::write(
        scratch.join("data").join(journal::FILE_NAME),
        journal::encode(&saved)?,
    )?;

    let Opening::Owner(prepared) = studio_app::prepare(Ok(scratch.join("data"))) else {
        return Err("the scratch data folder is taken".into());
    };
    let studio = studio_app::build(*prepared, vec![clip])?;
    let out = Rc::new(RefCell::new(vec![format!(
        "clip {w}x{h}, {frames} frames 8-bit; stream of {obus} OBUs ({} bytes); scale factor {}",
        obus * 2,
        studio.window().window().scale_factor()
    )]));
    let lags = Arc::new(Mutex::new(Vec::<Duration>::new()));
    let peaks = Arc::new(Mutex::new((0_u64, Vec::<(String, u64)>::new())));
    let posting = Arc::new(AtomicBool::new(false));

    let driver = Timer::default();
    let window = studio.window().as_weak();
    let script = RefCell::new(Some(Step::Ready));
    let (log, flag) = (out.clone(), posting.clone());
    driver.start(TimerMode::Repeated, Duration::from_millis(1), move || {
        let Some(win) = window.upgrade() else { return };
        let Some(current) = script.borrow_mut().take() else {
            return;
        };
        let analysis = win.global::<Analysis>();
        let encoding = matches!(win.get_job_view(), JobView::Running | JobView::Cancelling);
        let logged = log.borrow().len();
        let next = match current {
            Step::Ready if win.get_can_encode() => {
                win.invoke_priority_selected(0);
                win.invoke_encode_requested();
                flag.store(true, Ordering::SeqCst);
                Some(Step::Converting(Instant::now()))
            }
            Step::Converting(from) if encoding && win.get_progress_known() => {
                log.borrow_mut().push(format!(
                    "first frame counted {:?} after Convert",
                    from.elapsed()
                ));
                win.invoke_workspace_chosen(1);
                Some(Step::Reading(Instant::now()))
            }
            // A key before the first page answered only asks for it again.
            Step::Reading(from)
                if analysis.get_state() == AnalysisState::Open && analysis.get_total_rows() > 0 =>
            {
                log.borrow_mut().push(format!(
                    "stream read while converting: first rows shown {:?} after Analyze ({}); \
                     encoder still running: {encoding}",
                    from.elapsed(),
                    analysis.get_summary_line()
                ));
                let (before, asked) = (analysis.get_selected_line(), Instant::now());
                analysis.invoke_move(NavStep::Down);
                Some(Step::Moving {
                    n: 0,
                    before,
                    asked,
                    times: Vec::new(),
                    while_encoding: u32::from(encoding),
                })
            }
            Step::Moving {
                n,
                before,
                asked,
                mut times,
                while_encoding,
            } if analysis.get_selected_line() != before => {
                times.push(asked.elapsed());
                if n + 1 < MOVES {
                    // The move is handled inside the call when its row is
                    // there; the next tick sees it (1 ms timer).
                    let (before, asked) = (analysis.get_selected_line(), Instant::now());
                    analysis.invoke_move(NavStep::Down);
                    Some(Step::Moving {
                        n: n + 1,
                        before,
                        asked,
                        times,
                        while_encoding: while_encoding + u32::from(encoding),
                    })
                } else {
                    log.borrow_mut().push(format!(
                        "{} navigator moves ({while_encoding} while the encoder ran) shown \
                         after {}",
                        times.len(),
                        summary(&mut times)
                    ));
                    Some(Step::Draining(Instant::now()))
                }
            }
            Step::Draining(from) if !encoding || from.elapsed() > Duration::from_secs(120) => {
                log.borrow_mut().push(format!(
                    "conversion {}",
                    if encoding {
                        "still running; stopped waiting"
                    } else {
                        "ended"
                    }
                ));
                None
            }
            other => Some(other),
        };
        for line in &log.borrow()[logged..] {
            eprintln!("{line}");
        }
        if next.is_none() {
            flag.store(false, Ordering::SeqCst);
            let _ = slint::quit_event_loop();
        }
        *script.borrow_mut() = next;
    });

    let (store, flag, high) = (lags.clone(), posting.clone(), peaks.clone());
    let done = Arc::new(AtomicBool::new(false));
    let stop = done.clone();
    let poster = std::thread::spawn(move || {
        let mut tick = 0_u32;
        while !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(20));
            if !flag.load(Ordering::SeqCst) {
                continue;
            }
            tick += 1;
            if tick.is_multiple_of(5) {
                let (own, children) = resident(std::process::id());
                if let Ok(mut peak) = high.lock() {
                    peak.0 = peak.0.max(own);
                    for (engine, rss) in children {
                        match peak.1.iter_mut().find(|(e, _)| *e == engine) {
                            Some(seen) => seen.1 = seen.1.max(rss),
                            None => peak.1.push((engine, rss)),
                        }
                    }
                }
            }
            let sent = Instant::now();
            let store = store.clone();
            let posted = slint::invoke_from_event_loop(move || {
                if let Ok(mut l) = store.lock() {
                    l.push(sent.elapsed());
                }
            });
            if posted.is_err() {
                break;
            }
        }
    });

    studio.run()?;
    done.store(true, Ordering::SeqCst);
    let _ = poster.join();
    let mut waits = lags.lock().map_or_else(|_| Vec::new(), |l| l.clone());
    let mut out = out.borrow_mut();
    out.push(format!(
        "while converting and inspecting: {} closures posted every 20 ms waited {}",
        waits.len(),
        summary(&mut waits)
    ));
    if let Ok(peak) = peaks.lock() {
        out.push(format!(
            "resident memory high-water (sampled every 100 ms): app {} KiB; engine processes \
             {:?} (role pid, KiB)",
            peak.0, peak.1
        ));
    }
    for line in out.iter() {
        println!("{line}");
    }
    let _ = fs::remove_dir_all(&scratch);
    Ok(())
}
