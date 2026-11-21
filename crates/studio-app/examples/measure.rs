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

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Timer, TimerMode};
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
    let studio = studio_app::build(prepared, vec![clip])?;
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
