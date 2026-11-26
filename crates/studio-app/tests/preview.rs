// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The picture area against the real engine: the session starts the preview
//! process, asks for pictures through the real state machine and shows what
//! comes back; a real job sends live pictures and its result is compared
//! with its source.
//!
//! The engine is looked up next to the app binary Cargo builds for this test
//! (`target/<profile>/splot-studio-engine`); build it first with
//! `cargo build -p studio-engine`. These tests fail, never skip, without it.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use studio_app::engine::{Delivery, ENGINE_NAME};
use studio_app::preview::{Input, StageState, StageView};
use studio_app::queue::ItemState;
use studio_app::session::{Route, Session};
use studio_app::state::{AppState, Intent};
use studio_domain::preview::{PictureBox, PictureOrigin, PictureView};
use studio_domain::protocol::BuildIdentity;

const ANSWER_TIMEOUT: Duration = Duration::from_secs(60);
const JOB_TIMEOUT: Duration = Duration::from_secs(600);

/// The picture area at 1x for the 416px card, and a row thumbnail.
const STAGE: PictureBox = PictureBox {
    width: 416,
    height: 234,
};
const THUMB: PictureBox = PictureBox {
    width: 64,
    height: 36,
};

fn engine() -> PathBuf {
    let path = Path::new(env!("CARGO_BIN_EXE_splot-studio"))
        .with_file_name(format!("{ENGINE_NAME}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.is_file(),
        "build studio-engine first (`cargo build -p studio-engine`, same profile): no engine \
         at {}",
        path.display()
    );
    path
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "studio-app-preview-{}-{}-{name}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap_or_else(|e| unreachable!("{e}"));
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A progressive 4:2:0 Y4M at 30000/1001 fps with a moving pattern.
fn write_y4m(path: &Path, width: usize, height: usize, frames: usize, ten_bit: bool) {
    let colour = if ten_bit {
        "C420p10 XYSCSS=420P10"
    } else {
        "C420jpeg XYSCSS=420JPEG"
    };
    let mut bytes =
        format!("YUV4MPEG2 W{width} H{height} F30000:1001 Ip A1:1 {colour}\n").into_bytes();
    let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
    for f in 0..frames {
        bytes.extend_from_slice(b"FRAME\n");
        for (w, h, offset) in [(width, height, 0), (cw, ch, 300), (cw, ch, 700)] {
            for y in 0..h {
                for x in 0..w {
                    // Kept inside the limited range at either depth.
                    let v = 64 + (x * 7 + y * 3 + f * 5 + offset) % 800;
                    if ten_bit {
                        bytes.extend_from_slice(&u16::try_from(v).unwrap_or(0).to_le_bytes());
                    } else {
                        bytes.push(u8::try_from(16 + v % 220).unwrap_or(0));
                    }
                }
            }
        }
    }
    fs::write(path, bytes).unwrap_or_else(|e| unreachable!("{e}"));
}

/// A session whose deliveries arrive on a channel, as the window's do.
struct Harness {
    session: Session,
    rx: Receiver<(Route, Delivery)>,
    pictures: u32,
}

impl Harness {
    fn open() -> Self {
        Self::with_engine(engine())
    }

    fn with_engine(engine: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel();
        let post = Arc::new(move |route, delivery| {
            let _ = tx.send((route, delivery));
        });
        let session = Session::new(AppState::default(), Ok(engine), post, None);
        let mut harness = Self {
            session,
            rx,
            pictures: 0,
        };
        let client = BuildIdentity {
            name: "studio-app-preview-test".into(),
            version: "0".into(),
        };
        harness.session.start_hello(client, Instant::now());
        harness.until("the engine answered", ANSWER_TIMEOUT, |h| {
            h.session.state().capabilities().is_some()
        });
        harness
            .session
            .dispatch(Intent::PresetEdited(10), Instant::now());
        harness
            .session
            .dispatch(Intent::CrfEdited(40), Instant::now());
        harness.session.preview_input(Input::Resized {
            stage: STAGE,
            thumb: THUMB,
        });
        harness
    }

    fn view(&self) -> StageView {
        self.session.preview().view()
    }

    fn input(&mut self, input: Input) {
        self.session.preview_input(input);
    }

    /// Delivers engine events until `done` holds; counts pictures shown.
    fn until(&mut self, what: &str, timeout: Duration, done: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + timeout;
        while !done(self) {
            let left = deadline.saturating_duration_since(Instant::now());
            let (route, delivery) = self
                .rx
                .recv_timeout(left)
                .unwrap_or_else(|e| unreachable!("waiting for {what}: {e}"));
            let outcome = self.session.deliver(route, delivery, Instant::now());
            if outcome.preview.picture {
                self.pictures += 1;
            }
        }
    }

    fn shown(&self) -> Option<(PictureOrigin, u64)> {
        self.view()
            .picture
            .map(|p| (p.header.origin, p.header.frame))
    }
}

#[cfg(unix)]
fn preview_pid_launcher(dir: &Scratch) -> PathBuf {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let launcher = dir.path("launch");
    symlink(engine(), dir.path("engine")).unwrap_or_else(|e| unreachable!("{e}"));
    fs::write(
        &launcher,
        "#!/bin/sh\nif [ \"$1\" = preview ]; then printf '%s' \"$$\" > \"$0.pid\"; fi\nexec \"${0%/*}/engine\" \"$@\"\n",
    )
    .unwrap_or_else(|e| unreachable!("{e}"));
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700))
        .unwrap_or_else(|e| unreachable!("{e}"));
    launcher
}

#[cfg(unix)]
#[test]
fn a_killed_preview_cannot_show_old_pixels_in_a_new_document() {
    use std::process::Command;
    use studio_app::analysis::{Document, Input as Analyze};

    let dir = Scratch::new("preview-death");
    let launcher = preview_pid_launcher(&dir);
    let pid_path = launcher.with_extension("pid");
    let first = dir.path("first.ivf");
    let mut input = include_bytes!(
        "../../studio-engine/tests/preview/fixtures/syn-tlayer2-dependent-10bit.ivf"
    )
    .to_vec();
    // Give the test time to kill the process while playback still waits for its next picture.
    input[20..24].copy_from_slice(&1200_u32.to_le_bytes());
    fs::write(&first, input).unwrap_or_else(|e| unreachable!("{e}"));
    let second = dir.path("second.obu");
    fs::write(
        &second,
        include_bytes!(
            "../../studio-engine/tests/preview/fixtures/syn-mlayer2-derived-sef-8bit.obu"
        ),
    )
    .unwrap_or_else(|e| unreachable!("{e}"));

    let mut h = Harness::with_engine(launcher);
    let open = |h: &mut Harness, path| {
        h.session.analyze_open_requested(Instant::now());
        h.session.analyze_picked(vec![path]);
        h.until("the document was inspected", ANSWER_TIMEOUT, |h| {
            h.session
                .analysis()
                .shown()
                .and_then(Document::summary)
                .is_some()
        });
        h.session.analysis_input(Analyze::TogglePlayback);
        h.until("the document poster", ANSWER_TIMEOUT, |h| {
            h.shown() == Some((PictureOrigin::Document, 0))
        });
        h.session
            .analysis()
            .shown()
            .map_or_else(|| unreachable!(), Document::id)
    };
    let first_id = open(&mut h, first);
    let old = h.view().picture.unwrap_or_else(|| unreachable!());
    h.input(Input::PlayPause);
    assert!(h.view().playing);
    let (route, delayed) = loop {
        let (route, delivery) =
            h.rx.recv_timeout(ANSWER_TIMEOUT)
                .unwrap_or_else(|e| unreachable!("{e}"));
        if let (Route::Preview(0), Delivery::Pictures(_)) = (&route, &delivery) {
            break (route, delivery);
        }
        h.session.deliver(route, delivery, Instant::now());
    };
    let pid = fs::read_to_string(&pid_path)
        .unwrap_or_else(|e| unreachable!("{e}"))
        .parse::<u32>()
        .unwrap_or_else(|e| unreachable!("{e}"));
    assert!(pid > 1);
    let status = Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status();
    assert!(status.is_ok_and(|status| status.success()));
    loop {
        let (ended_route, delivery) =
            h.rx.recv_timeout(ANSWER_TIMEOUT)
                .unwrap_or_else(|e| unreachable!("{e}"));
        let ended = matches!((&ended_route, &delivery), (Route::Preview(0), Delivery::Ended(ending)) if ending.exit.contains("signal"));
        h.session.deliver(ended_route, delivery, Instant::now());
        if ended {
            break;
        }
    }
    assert_eq!(h.view().state, StageState::Unavailable);
    assert!(!h.view().playing);
    assert!(h.view().picture.is_none());
    assert_eq!(h.session.preview().output_byte(0), None);

    h.session.analysis_input(Analyze::Close);
    let second_id = open(&mut h, second);
    assert_ne!(first_id, second_id);
    assert_eq!(
        h.session.preview().output_byte(0).map(|(id, _)| id),
        Some(second_id)
    );
    let new = h.view().picture.unwrap_or_else(|| unreachable!());
    assert_ne!(new.pixels.as_bytes(), old.pixels.as_bytes());
    assert!(
        !h.session
            .deliver(route, delayed, Instant::now())
            .preview
            .picture
    );
    assert_eq!(
        h.view()
            .picture
            .unwrap_or_else(|| unreachable!())
            .pixels
            .as_bytes(),
        new.pixels.as_bytes()
    );
}

#[test]
fn source_pictures_stepping_and_playback_come_from_the_real_engine() {
    let dir = Scratch::new("source");
    for ten_bit in [false, true] {
        let source = dir.path(if ten_bit { "ten.y4m" } else { "eight.y4m" });
        write_y4m(&source, 320, 180, 24, ten_bit);
        let mut h = Harness::open();
        let asked = Instant::now();
        h.session
            .dispatch(Intent::FilesOpened(vec![source.clone()]), Instant::now());
        h.until("the poster", ANSWER_TIMEOUT, |h| {
            h.view().state == StageState::Picture
        });
        let first = asked.elapsed();
        let view = h.view();
        let picture = view.picture.unwrap_or_else(|| unreachable!());
        let header = &picture.header;
        assert_eq!(header.origin, PictureOrigin::Source);
        assert_eq!(header.view, PictureView::Fit);
        // Never enlarged, aspect kept.
        assert_eq!((header.width, header.height), (320, 180), "{header:?}");
        assert_eq!((header.frame_width, header.frame_height), (320, 180));
        assert_eq!(header.frame_count, Some(24));
        assert!(header.time.is_some(), "a Y4M has frame timing");
        assert!(view.badge.starts_with("Original"), "{}", view.badge);
        assert!(view.navigable && view.can_play && !view.can_compare);
        assert!(picture.pixels.as_bytes().iter().any(|&b| b != 0));

        let asked = Instant::now();
        h.input(Input::Seek(10));
        h.until("frame 10", ANSWER_TIMEOUT, |h| {
            h.shown() == Some((PictureOrigin::Source, 10))
        });
        let seek = asked.elapsed();
        h.input(Input::Step(1));
        h.until("frame 11", ANSWER_TIMEOUT, |h| {
            h.shown() == Some((PictureOrigin::Source, 11))
        });

        // Playback runs to the last frame at the media's timing.
        let before = h.pictures;
        let started = Instant::now();
        h.input(Input::PlayPause);
        assert!(h.view().playing);
        h.until("the end of playback", ANSWER_TIMEOUT, |h| !h.view().playing);
        let played = started.elapsed();
        assert_eq!(h.shown(), Some((PictureOrigin::Source, 23)));
        let shown = h.pictures - before;
        assert!(shown >= 2, "{shown} pictures during playback");
        // 12 frames at 30000/1001 fps take 0.4 s; late frames are skipped,
        // never shown early.
        assert!(played >= Duration::from_millis(350), "{played:?}");
        eprintln!(
            "{}-bit: poster after {first:?} (probe included), seek {seek:?}, playback of 12 \
             frames {played:?} with {shown} pictures",
            if ten_bit { 10 } else { 8 }
        );
    }
}

#[test]
fn a_job_sends_live_pictures_and_its_result_is_compared_with_the_source() {
    let dir = Scratch::new("live");
    let source = dir.path("clip.y4m");
    write_y4m(&source, 160, 90, 30, false);
    let mut h = Harness::open();
    h.session
        .dispatch(Intent::FilesOpened(vec![source]), Instant::now());
    h.until("the poster", ANSWER_TIMEOUT, |h| {
        h.view().state == StageState::Picture
    });
    eprintln!("source poster: {:?}", h.shown());
    h.session.dispatch(Intent::ConvertRequested, Instant::now());
    h.until("a live picture", JOB_TIMEOUT, |h| {
        h.shown()
            .is_some_and(|(origin, _)| origin == PictureOrigin::Live)
            || h.session.state().items()[0].state() == ItemState::Completed
    });
    let live = h.view();
    let saw_live = live
        .picture
        .as_ref()
        .is_some_and(|p| p.header.origin == PictureOrigin::Live);
    if saw_live {
        assert!(live.badge.starts_with("Live · frame"), "{}", live.badge);
        assert!(live.can_compare && !live.navigable);
        // Holding while converting reads the source frame of the same index.
        let frame = live.picture.map_or(0, |p| p.header.frame);
        h.input(Input::Compare(true));
        h.until("the original of the live frame", ANSWER_TIMEOUT, |h| {
            h.shown() == Some((PictureOrigin::Source, frame))
                || h.session.state().items()[0].state() == ItemState::Completed
        });
        h.input(Input::Compare(false));
    }
    h.until("the job", JOB_TIMEOUT, |h| {
        h.session.state().items()[0].state() == ItemState::Completed
    });
    h.until("the result", ANSWER_TIMEOUT, |h| {
        h.shown()
            .is_some_and(|(origin, _)| origin == PictureOrigin::Result)
    });
    let result = h.view();
    assert!(result.badge.starts_with("AV2 result"), "{}", result.badge);
    assert!(result.can_compare && result.navigable);
    let frame = result.picture.as_ref().map_or(0, |p| p.header.frame);
    h.input(Input::Compare(true));
    h.until("the original", ANSWER_TIMEOUT, |h| {
        h.shown() == Some((PictureOrigin::Source, frame))
    });
    assert!(h.view().badge.starts_with("Original"));
    h.input(Input::Compare(false));
    assert_eq!(h.shown(), Some((PictureOrigin::Result, frame)));
    // Stepping the result decodes the output, not the source. The engine
    // chooses the poster, which may be the last frame.
    let (delta, next) = if frame + 1 < 30 {
        (1, frame + 1)
    } else {
        (-1, frame - 1)
    };
    eprintln!("result poster: frame {frame}");
    h.input(Input::Step(delta));
    h.until("another result frame", ANSWER_TIMEOUT, |h| {
        h.shown() == Some((PictureOrigin::Result, next))
    });
    assert!(
        saw_live,
        "the job finished before any live picture arrived; use a longer clip"
    );
}

#[test]
fn a_result_that_was_moved_away_says_so_inside_the_area() {
    let dir = Scratch::new("moved");
    let source = dir.path("clip.y4m");
    write_y4m(&source, 64, 36, 3, false);
    let mut h = Harness::open();
    h.session
        .dispatch(Intent::FilesOpened(vec![source]), Instant::now());
    h.until("the poster", ANSWER_TIMEOUT, |h| {
        h.view().state == StageState::Picture
    });
    h.session.dispatch(Intent::ConvertRequested, Instant::now());
    // The result is asked for when the job completes; it is gone by then.
    let destination = dir.path("clip.ivf");
    h.until("the job", JOB_TIMEOUT, |h| {
        let done = h.session.state().items()[0].state() == ItemState::Completed;
        if done {
            let _ = fs::remove_file(&destination);
        }
        done
    });
    h.until("the answer", ANSWER_TIMEOUT, |h| {
        h.view().state == StageState::Unavailable
            || h.shown()
                .is_some_and(|(origin, _)| origin == PictureOrigin::Result)
    });
    let view = h.view();
    // Racing the removal, the result may have been read first; it is never
    // shown as anything but the result.
    if view.state == StageState::Unavailable {
        assert!(
            view.message.contains("converted file cannot be read"),
            "{}",
            view.message
        );
        assert!(!view.can_compare);
    } else {
        assert!(view.badge.starts_with("AV2 result"));
    }
}
