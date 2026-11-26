// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Renders the real window once and writes a renderer snapshot as binary PPM.
//!
//! ```text
//! cargo run -p studio-app --example snapshot -- OUT.ppm light|dark [about] [advanced] [jobs] [long] [SCENARIO]
//! ```
//!
//! This is Slint's own `Window::take_snapshot`, not an OS screenshot: no
//! window decorations and no compositor. No engine runs: each SCENARIO drives
//! the app's real `AppState` with the intents and engine events listed below
//! and draws it with `studio_app::render`. Advanced's engine line says so.
//!
//! Scenarios: `empty` (default), `engine-missing`, `probing`, `probed`, `unreadable`,
//! `running` (known frame count), `running-unknown`, `cancelling`,
//! `completed`, `cancelled`, `failed`, `lost`; for ordinary files read with
//! `FFmpeg`: `ordinary` (audio, subtitles, chapters, two video tracks and cover
//! art), `confirm` (the video-only confirmation), `checking` (started, no
//! frame yet), `unsupported` (refused when opened), `unsupported-job`
//! (refused at Start), `changed` (the file changed before Start),
//! `tool-missing` (no `FFmpeg` in this build) and `tool-missing-file` (an MP4
//! opened without one). Several files: `list` (done, failed, converting,
//! waiting, refused; `list-advanced` selects the failed row), `running-list`
//! (the second of five converting), `summary` (a finished run),
//! `batch-confirm` (the confirmation for seven files), `save-failed` (the
//! journal cannot be written), `many` (eight rows, the list scrolls) and
//! `restored` (the list after the app stopped mid-job). `already-open` is the
//! window a second instance shows. The `analyze-*` scenarios show the Analyze
//! workspace with the real inspect engine; see `analyze.rs`. `jobs` opens
//! the Jobs list over any scenario; `analyze-jobs` lists the reads of
//! `analyze.rs` with the conversion of `running`.
//!
//! The picture area is drawn by the app's real `Preview`, its requests
//! answered with painted test pictures (a landscape, coarser on the result
//! side) instead of engine pictures; a converting file shows a live
//! picture. `preview-loading` leaves them unanswered; `preview-live`
//! (converting, as `running`),
//! `preview-compare` (held on a result), `preview-native` (actual pixels),
//! `preview-unavailable` (the output file is gone) and `preview-untimed` (no
//! frame timing) show its other states.

mod analyze;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Rgba8Pixel, SharedPixelBuffer};
use studio_app::activity::Input;
use studio_app::engine::Picture;
use studio_app::journal::{Journal, JournalCause, JournalItem, JournalPartial, JournalState};
use studio_app::preview::{self, Preview};
use studio_app::recovery::{Disk, Kind};
use studio_app::source::ProbeFailure;
use studio_app::state::{AppState, Effect, Intent, Startup};
use studio_app::{NoticeWindow, Palette};
use studio_domain::job::{
    Check, CheckKind, CheckOutcome, ChromaLayout, Disclosure, Failure, FailureCode, JobReport,
    JobSpec, Omissions, Progress, Rational, SourceColor, SourceFacts, SourceFingerprint,
    SourceReader, Stage, VideoTrack, WirePath,
};
use studio_domain::preview::{
    ColorBasis, ColorMatrix, MediaTime, PictureBox, PictureFailure, PictureFrame, PictureHeader,
    PictureOrigin, PictureTarget, PictureView,
};
use studio_domain::protocol::{Capabilities, Command, Event, MediaTool, MediaToolOrigin};
use studio_domain::settings::OutputKind;

const SOURCE: &str = "/home/user/Videos/park walk.y4m";
const LONG_SOURCE: &str = "/home/user/Videos/2026-11-20 family reunion at the lake house, drone pass over the jetty, take 3 (final).y4m";
const MP4: &str = "/home/user/Videos/harbour at dusk.mp4";

fn facts(frame_count: Option<u64>) -> SourceFacts {
    SourceFacts {
        reader: SourceReader::Y4m,
        fingerprint: SourceFingerprint {
            len: 1_866_247_800,
            modified: Some((1_779_000_000, 0)),
        },
        video_stream: 0,
        video_tracks: Vec::new(),
        omissions: Omissions::default(),
        width: 1920,
        height: 1080,
        frame_rate: Rational {
            num: 30000,
            den: 1001,
        },
        pixel_aspect: Some(Rational { num: 1, den: 1 }),
        color: SourceColor::default(),
        bit_depth: 10,
        chroma: ChromaLayout::Yuv420,
        frame_count,
        disclosures: vec![Disclosure::ColorUnspecified],
    }
}

fn track(stream: u32, codec: &str, width: u32, height: u32, attached_picture: bool) -> VideoTrack {
    VideoTrack {
        stream,
        codec: codec.into(),
        width,
        height,
        attached_picture,
    }
}

/// An MP4 from a phone editor: two H.264 renditions, cover art, two audio
/// tracks, three subtitle tracks and chapters; frame count unknown.
fn mp4_facts() -> SourceFacts {
    SourceFacts {
        reader: SourceReader::Ffmpeg,
        video_stream: 0,
        video_tracks: vec![
            track(0, "h264", 1920, 1080, false),
            track(1, "h264", 1280, 720, false),
            track(7, "mjpeg", 600, 600, true),
        ],
        omissions: Omissions {
            other_video: 2,
            audio: 2,
            subtitles: 3,
            chapters: 8,
            ..Omissions::default()
        },
        frame_rate: Rational { num: 25, den: 1 },
        bit_depth: 8,
        disclosures: vec![Disclosure::ColorNotCarried, Disclosure::LayoutRepacked],
        ..facts(None)
    }
}

/// What the engine says when it has `FFmpeg`: this machine's, as a
/// development build reports it.
fn capabilities(tool: bool) -> Capabilities {
    Capabilities {
        encode_outputs: OutputKind::ALL.to_vec(),
        media_tool: tool.then(|| MediaTool {
            version: "8.0.1-3ubuntu2".into(),
            origin: MediaToolOrigin::System,
        }),
        analysis_framings: Vec::new(),
        options: None,
    }
}

/// The snapshots' disk: nothing exists, so no name is taken.
struct NoFiles;

impl Disk for NoFiles {
    fn kind(&self, _: &Path) -> Kind {
        Kind::Missing
    }
}

/// A state driven as the session drives it, with probes answered from a
/// table instead of an engine.
struct Driver {
    state: AppState,
    now: Instant,
}

impl Driver {
    fn apply(&mut self, intent: Intent) -> Vec<Effect> {
        self.state.apply(intent, self.now, &NoFiles).effects
    }

    fn apply_at(&mut self, intent: Intent, at: Instant) -> Vec<Effect> {
        self.state.apply(intent, at, &NoFiles).effects
    }

    /// Answers the probes in `effects`, and those that follow, with
    /// `answer`; returns the other effects.
    fn answer(
        &mut self,
        mut effects: Vec<Effect>,
        answer: &dyn Fn(&Path) -> Result<SourceFacts, ProbeFailure>,
        size: u64,
    ) -> Vec<Effect> {
        let mut others = Vec::new();
        loop {
            let mut next = None;
            for effect in effects {
                match effect {
                    Effect::Probe { item, request } => next = Some((item, request)),
                    other => others.push(other),
                }
            }
            let Some((item, request)) = next else {
                return others;
            };
            // Plausible, different sizes for different files.
            let spread = request.path.as_os_str().len() as u64 % 7;
            self.state
                .note_size(item, Some(size / 7 * (spread + 3) / 4));
            effects = self.apply(Intent::ProbeAnswered {
                id: request.id,
                result: answer(&request.path),
            });
        }
    }

    fn add(
        &mut self,
        paths: &[&str],
        answer: &dyn Fn(&Path) -> Result<SourceFacts, ProbeFailure>,
        size: u64,
    ) -> Vec<Effect> {
        let effects = self.apply(Intent::FilesOpened(
            paths.iter().map(PathBuf::from).collect(),
        ));
        self.answer(effects, answer, size)
    }

    fn job(&mut self, id: u64, input: Input) -> Vec<Effect> {
        self.apply(Intent::Job { id, input })
    }

    fn started(effects: &[Effect]) -> Option<u64> {
        effects.iter().find_map(|e| match e {
            Effect::Start { id, .. } => Some(*id),
            _ => None,
        })
    }

    /// Publishes and completes job `id` with a verified report.
    fn complete(&mut self, id: u64, destination: &str, long: bool) -> Vec<Effect> {
        for stage in [Stage::Finalizing, Stage::Verifying, Stage::Publishing] {
            self.job(id, Input::Event(Event::Stage { stage }));
        }
        self.job(
            id,
            Input::Event(Event::Completed {
                report: report(destination, long),
            }),
        )
    }

    fn progress(&mut self, id: u64, encoded: u64) {
        self.job(
            id,
            Input::Event(Event::Stage {
                stage: Stage::Encoding,
            }),
        );
        self.job(
            id,
            Input::Event(Event::Progress {
                progress: Progress {
                    frames_read: encoded + 14,
                    frames_encoded: encoded,
                    bytes_written: 2_481_337,
                    frames_coded_milli: encoded * 1000,
                },
            }),
        );
    }
}

fn report(destination: &str, long: bool) -> JobReport {
    JobReport {
        pass_digest: None,
        destination: WirePath::Utf8(destination.into()),
        frames: 300,
        bytes: 6_302_118,
        checks: vec![
            Check {
                kind: CheckKind::Structure,
                outcome: CheckOutcome::Passed,
            },
            Check {
                kind: CheckKind::SplotDecode,
                outcome: CheckOutcome::Passed,
            },
            Check {
                kind: CheckKind::AvmDecode,
                outcome: CheckOutcome::NotRun {
                    reason: if long {
                        "No AVM decoder is configured on this machine. Set one up to \
                         compare an independent decode; until then this file is \
                         checked only by Splot's own decoder."
                            .into()
                    } else {
                        "No AVM decoder is configured on this machine.".into()
                    },
                },
            },
        ],
        disclosures: vec![Disclosure::ColorUnspecified],
    }
}

fn ok_y4m(frames: Option<u64>) -> impl Fn(&Path) -> Result<SourceFacts, ProbeFailure> {
    move |_| Ok(facts(frames))
}

/// Answers each file of the list scenarios: `.mp4` as the phone clip,
/// `.hdr.mov` as refused, everything else as the Y4M.
fn list_answer(path: &Path) -> Result<SourceFacts, ProbeFailure> {
    let name = path.to_string_lossy();
    if name.ends_with(".hdr.mov") {
        return Err(ProbeFailure {
            code: Some(FailureCode::UnsupportedSource),
            message: "The video is HDR (PQ transfer). Only SDR video is encoded".into(),
        });
    }
    if name.ends_with(".mp4") {
        Ok(mp4_facts())
    } else {
        Ok(SourceFacts {
            height: if name.contains("drone") { 2160 } else { 1080 },
            width: if name.contains("drone") { 3840 } else { 1920 },
            ..facts(Some(300))
        })
    }
}

const LIST: [&str; 5] = [
    "/home/user/Videos/park walk.y4m",
    "/home/user/Videos/drone over the jetty.y4m",
    "/home/user/Videos/harbour at dusk.mp4",
    "/home/user/Videos/kitchen timelapse.y4m",
    "/home/user/Videos/birthday candles.y4m",
];

/// The list scenarios: several files in one window.
#[allow(clippy::too_many_lines)]
fn list_scenario(name: &str, now: Instant) -> Result<AppState, String> {
    let start = now.checked_sub(Duration::from_secs(83)).unwrap_or(now);
    let mut d = Driver {
        state: AppState::default(),
        now: start,
    };
    d.apply(Intent::EngineAnswered(Ok(capabilities(true))));
    match name {
        "list" | "list-advanced" => {
            // A finished file, a failure, one converting with its run,
            // one waiting and one refused when it was read.
            let files = [
                LIST[0],
                LIST[3],
                LIST[1],
                LIST[4],
                "/home/user/Videos/sunset.hdr.mov",
            ];
            d.add(&files, &list_answer, 1_866_247_800);
            let a = Driver::started(&d.apply(Intent::ConvertRequested)).ok_or("no start")?;
            d.progress(a, 300);
            let b = Driver::started(&d.complete(a, "/home/user/Videos/park walk.ivf", false))
                .ok_or("no second start")?;
            d.job(
                b,
                Input::Event(Event::Failed {
                    failure: Failure {
                        code: FailureCode::Output,
                        message: "Writing the temporary file failed: No space left on device \
                                  (os error 28)"
                            .into(),
                    },
                }),
            );
            let c = d.state.items()[2]
                .attempt()
                .map(|a| a.request)
                .ok_or("no third")?;
            d.now = now;
            d.progress(c, 117);
            if name == "list-advanced" {
                d.apply(Intent::RowSelected(1));
            }
        }
        "running-list" => {
            d.add(&LIST, &list_answer, 1_866_247_800);
            d.apply(Intent::ConvertRequested);
            let a = Driver::started(&d.apply(Intent::OmissionsConfirmed)).ok_or("no start")?;
            d.progress(a, 300);
            let b = Driver::started(&d.complete(a, "/home/user/Videos/park walk.ivf", false))
                .ok_or("no second start")?;
            d.now = now;
            d.progress(b, 117);
        }
        "summary" => {
            d.add(&LIST, &list_answer, 412_733_901);
            d.apply(Intent::ConvertRequested);
            let mut id = Driver::started(&d.apply(Intent::OmissionsConfirmed));
            let mut n = 0;
            while let Some(job) = id {
                let effects = if n == 2 {
                    d.job(
                        job,
                        Input::Event(Event::Failed {
                            failure: Failure {
                                code: FailureCode::UnsupportedSource,
                                message: "The video has a variable frame rate".into(),
                            },
                        }),
                    )
                } else {
                    let destination = d.state.items()[n].destination().path.display().to_string();
                    d.complete(job, &destination, false)
                };
                id = Driver::started(&effects);
                n += 1;
            }
        }
        "batch-confirm" => {
            let files = [
                "/home/user/Videos/harbour at dusk.mp4",
                "/home/user/Videos/ferry crossing.mp4",
                "/home/user/Videos/park walk.y4m",
                "/home/user/Videos/market morning.mp4",
                "/home/user/Videos/street band.mp4",
                "/home/user/Videos/tram ride.mp4",
                "/home/user/Videos/rain on the window.mp4",
                "/home/user/Videos/night bus.mp4",
            ];
            d.add(&files, &list_answer, 412_733_901);
            d.apply(Intent::ConvertRequested);
        }
        "save-failed" => {
            d.add(&LIST[..3], &list_answer, 412_733_901);
            d.apply(Intent::JournalSaved(Err(
                "No space left on device (os error 28)".into(),
            )));
        }
        "many" => {
            let files: Vec<String> = (1..=8)
                .map(|n| format!("/home/user/Videos/lecture part {n}.y4m"))
                .collect();
            let files: Vec<&str> = files.iter().map(String::as_str).collect();
            d.add(&files, &ok_y4m(Some(300)), 1_866_247_800);
        }
        other => return Err(format!("unknown scenario {other:?}")),
    }
    d.now = now;
    Ok(d.state)
}

/// A queue restored after the app stopped mid-job: the job that was
/// converting is interrupted (its temporary file removed), the rest wait.
fn restored_scenario(now: Instant) -> AppState {
    let item = |id: u64, source: &str, state: JournalState| JournalItem {
        id,
        source: WirePath::Utf8(source.into()),
        size: Some(1_866_247_800),
        video_stream: None,
        facts: Some(facts(Some(300))),
        destination: WirePath::Utf8(source.replace(".y4m", ".ivf")),
        destination_chosen: false,
        acknowledgement: None,
        submitted: None,
        attempt_policy: Vec::new(),
        state,
    };
    let job = |source: &str| JobSpec {
        source: WirePath::Utf8(source.into()),
        fingerprint: facts(None).fingerprint,
        video_stream: 0,
        omissions_acknowledged: false,
        destination: WirePath::Utf8(source.replace(".y4m", ".ivf")),
        settings: studio_app::state::INITIAL_SETTINGS.into(),
        identity: None,
        purpose: studio_domain::job::JobPurpose::Encode,
        pass_digest: None,
        pass_artifact: None,
    };
    let journal = Journal {
        view: studio_app::journal::JournalView::default(),
        version: studio_app::journal::VERSION,
        next_id: 4,
        settings: studio_app::state::INITIAL_SETTINGS.into(),
        items: vec![
            item(
                1,
                LIST[0],
                JournalState::Interrupted {
                    job: job(LIST[0]),
                    publishing: false,
                    cause: JournalCause::AppStopped,
                    destination_present: false,
                    partial: JournalPartial::Removed {
                        path: WirePath::Utf8(
                            "/home/user/Videos/.park walk.ivf.Hq3xZ1.partial".into(),
                        ),
                    },
                },
            ),
            item(2, LIST[3], JournalState::Waiting),
            item(3, LIST[4], JournalState::Waiting),
        ],
    };
    let recovered = studio_app::recovery::recover(journal, &NoFiles);
    let mut state = AppState::new(Startup {
        recovered: Some(recovered),
        problem: None,
    });
    state.apply(
        Intent::EngineAnswered(Ok(capabilities(true))),
        now,
        &NoFiles,
    );
    state
}

/// Plays `scenario` into a fresh state, as the controller would. `long`
/// uses a long file name, a long failure message and long check details.
#[allow(clippy::too_many_lines)]
fn scenario(name: &str, long: bool, now: Instant) -> Result<AppState, String> {
    if matches!(
        name,
        "list"
            | "list-advanced"
            | "running-list"
            | "summary"
            | "batch-confirm"
            | "save-failed"
            | "many"
    ) {
        return list_scenario(name, now);
    }
    if name == "restored" {
        return Ok(restored_scenario(now));
    }
    let mut d = Driver {
        state: AppState::default(),
        now,
    };
    if name == "engine-missing" {
        d.apply(Intent::EngineAnswered(Err(
            "The encoding engine is missing (/opt/splot-studio/splot-studio-engine). \
             Reinstall Splot Studio."
                .into(),
        )));
        return Ok(d.state);
    }
    let tool = !name.starts_with("tool-missing");
    d.apply(Intent::EngineAnswered(Ok(capabilities(tool))));
    if matches!(name, "empty" | "tool-missing") {
        return Ok(d.state);
    }
    let ordinary = matches!(
        name,
        "ordinary"
            | "confirm"
            | "checking"
            | "unsupported"
            | "unsupported-job"
            | "changed"
            | "tool-missing-file"
    );
    let source = match (ordinary, long) {
        (true, _) => MP4,
        (false, true) => LONG_SOURCE,
        (false, false) => SOURCE,
    };
    let effects = d.apply(Intent::FilesOpened(vec![source.into()]));
    let Some((item, request)) = effects.iter().find_map(|e| match e {
        Effect::Probe { item, request } => Some((*item, request.clone())),
        _ => None,
    }) else {
        return Err("picking did not probe".into());
    };
    // 300 frames of 1920x1080 10-bit 4:2:0 Y4M, or a phone clip.
    d.state.note_size(
        item,
        Some(if ordinary { 412_733_901 } else { 1_866_247_800 }),
    );
    let total = if name == "running-unknown" {
        None
    } else {
        Some(300)
    };
    let refused = |code, message: &str| {
        Err(ProbeFailure {
            code: Some(code),
            message: message.to_owned(),
        })
    };
    let result = match name {
        "probing" => return Ok(d.state),
        "unreadable" => refused(
            FailureCode::UnsupportedSource,
            "Unsupported Y4M: interlaced content (Itop) is not encoded by this build.",
        ),
        "unsupported" => refused(
            FailureCode::UnsupportedSource,
            "The video is HDR (PQ transfer). Only SDR video is encoded",
        ),
        "tool-missing-file" => refused(
            FailureCode::MediaToolMissing,
            "Reading this file needs FFmpeg, and this engine has none.",
        ),
        _ if ordinary => Ok(mp4_facts()),
        _ => Ok(facts(total)),
    };
    d.apply(Intent::ProbeAnswered {
        id: request.id,
        result,
    });
    if matches!(
        name,
        "probed" | "unreadable" | "ordinary" | "unsupported" | "tool-missing-file"
    ) {
        return Ok(d.state);
    }

    // The checking scenario is two seconds in; the others 1:23.
    let ago = if name == "checking" { 2 } else { 83 };
    let start = now.checked_sub(Duration::from_secs(ago)).unwrap_or(now);
    let effects = d.apply_at(Intent::ConvertRequested, start);
    let id = match Driver::started(&effects) {
        Some(id) => id,
        None if d.state.confirmation().is_some() => {
            if name == "confirm" {
                return Ok(d.state);
            }
            Driver::started(&d.apply_at(Intent::OmissionsConfirmed, start))
                .ok_or("confirming did not start")?
        }
        None => return Err("encode did not start".into()),
    };
    if ordinary {
        match name {
            // No stage and no counts yet: the engine is checking timestamps.
            "checking" => {}
            "unsupported-job" => {
                d.job(
                    id,
                    Input::Event(Event::Failed {
                        failure: Failure {
                            code: FailureCode::UnsupportedSource,
                            message: "The video has a variable frame rate: frame 412 lasts \
                                      1/30 s after frames of 1/25 s"
                                .into(),
                        },
                    }),
                );
            }
            "changed" => {
                let failed = Input::Event(Event::Failed {
                    failure: Failure {
                        code: FailureCode::SourceChanged,
                        message: "harbour at dusk.mp4 changed since it was read".into(),
                    },
                });
                // The app reads the file again by itself; show it read.
                let effects = d.job(id, failed);
                d.answer(effects, &|_| Ok(mp4_facts()), 412_733_901);
            }
            other => return Err(format!("unknown scenario {other:?}")),
        }
        return Ok(d.state);
    }
    d.progress(id, 117);
    match name {
        "running" | "running-unknown" => {}
        "cancelling" => {
            d.apply(Intent::CancelRequested);
            d.job(id, Input::Event(Event::CancelRequested));
        }
        "completed" => {
            let destination = source.replace(".y4m", ".ivf");
            d.complete(id, &destination, long);
        }
        "cancelled" => {
            d.apply(Intent::CancelRequested);
            d.job(id, Input::Event(Event::CancelRequested));
            d.job(id, Input::Event(Event::Cancelled));
        }
        "failed" => {
            d.job(
                id,
                Input::Event(Event::Failed {
                    failure: Failure {
                        code: FailureCode::Output,
                        message: if long {
                            "Writing the temporary file /home/user/Videos/.park walk.ivf.splot-tmp-4f1c \
                             failed after 2.48 GB: No space left on device (os error 28). The partial \
                             temporary file was removed"
                                .into()
                        } else {
                            "Writing the temporary file failed: No space left on device (os error 28)"
                                .into()
                        },
                    },
                }),
            );
        }
        "lost" => {
            d.job(
                id,
                Input::WorkerEnded {
                    detail: "signal: 9 (SIGKILL)".into(),
                },
            );
        }
        other => return Err(format!("unknown scenario {other:?}")),
    }
    Ok(d.state)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [out, scheme, rest @ ..] = args.as_slice() else {
        return Err(
            "usage: snapshot OUT.ppm light|dark [about] [advanced] [jobs] [long] [SCENARIO]".into(),
        );
    };
    let scheme = match scheme.as_str() {
        "light" => slint::language::ColorScheme::Light,
        "dark" => slint::language::ColorScheme::Dark,
        other => return Err(format!("unknown scheme {other:?}").into()),
    };

    let long = rest.iter().any(|a| a == "long");
    let mut name = "empty";
    for arg in rest {
        if !matches!(arg.as_str(), "about" | "advanced" | "jobs" | "long") {
            name = arg;
        }
    }
    if name == "already-open" {
        let window = NoticeWindow::new()?;
        window.set_heading(studio_app::ALREADY_OPEN.0.into());
        window.set_message(studio_app::ALREADY_OPEN.1.into());
        window.global::<Palette>().set_color_scheme(scheme);
        window.show()?;
        return capture(&window, out);
    }
    let window = studio_app::new_window()?;
    window.global::<Palette>().set_color_scheme(scheme);
    let now = Instant::now();
    for arg in rest {
        match arg.as_str() {
            "about" => window.invoke_show_about(),
            "advanced" => window.set_advanced_open(true),
            _ => {}
        }
    }
    let base = match name {
        "preview-loading" | "preview-native" | "preview-untimed" => "probed",
        "preview-live" => "running",
        "preview-compare" | "preview-unavailable" => "completed",
        other => other,
    };
    let jobs = rest.iter().any(|a| a == "jobs");
    if let Some(analyze) = analyze::scenario(name)? {
        let encode = if name == "analyze-jobs" {
            scenario("running", long, now)?
        } else {
            analyze.session.state().clone()
        };
        studio_app::render(&window, analyze.session.state(), now);
        studio_app::render_analysis(&window, analyze.session.analysis());
        studio_app::render_jobs(
            &window,
            &studio_app::jobs::list(&encode, analyze.session.analysis()),
        );
        window.show()?;
        if jobs {
            window.invoke_show_jobs();
        }
        return capture(&window, out);
    }
    let state = scenario(base, long, now)?;
    studio_app::render(&window, &state, now);
    studio_app::render_jobs(
        &window,
        &studio_app::jobs::list(&state, &studio_app::analysis::Analysis::new()),
    );
    let preview = picture_area(&window, &state, name);
    let all = preview::Dirty {
        picture: true,
        view: true,
        thumbs: true,
    };
    studio_app::render_preview(&window, &state, &preview, all);
    window.set_engine_identity("renderer snapshot, no engine ran".into());
    window.show()?;
    if jobs {
        window.invoke_show_jobs();
    }
    capture(&window, out)
}

/// The picture area for `state`, its requests answered with painted
/// pictures as `name` says.
fn picture_area(window: &studio_app::AppWindow, state: &AppState, name: &str) -> Preview {
    let mut preview = Preview::new();
    let size = |w: i32, h: i32| {
        PictureBox::clamped(u32::try_from(w).unwrap_or(1), u32::try_from(h).unwrap_or(1))
    };
    let mut effects = preview.sync(preview::scene(state));
    effects.extend(preview.input(preview::Input::Resized {
        stage: size(window.get_stage_px_width(), window.get_stage_px_height()),
        thumb: size(window.get_thumb_px_width(), window.get_thumb_px_height()),
    }));
    match name {
        "preview-loading" => return preview,
        // The output file was moved away.
        "preview-unavailable" => {
            for effect in effects {
                if let Command::Picture { generation, .. } = effect.command {
                    preview.unavailable(
                        generation,
                        PictureFailure::Unreadable,
                        "No such file or directory (os error 2)",
                    );
                }
            }
            return preview;
        }
        _ => {}
    }
    let timed = name != "preview-untimed";
    answer(&mut preview, effects, timed);
    // A converting file shows its newest live picture.
    if let Some(preview::Subject::Live { job, .. }) = preview::scene(state).stage {
        let size = preview.live_box();
        preview.live(
            job,
            paint(0, PictureOrigin::Live, 116, PictureView::Fit, size, timed),
        );
    }
    match name {
        "preview-compare" => {
            let effects = preview.input(preview::Input::Compare(true));
            answer(&mut preview, effects, timed);
        }
        "preview-native" => {
            let effects = preview.input(preview::Input::ToggleNative);
            answer(&mut preview, effects, timed);
        }
        _ => {}
    }
    preview
}

/// Answers every picture request, and those that follow, with a painted
/// picture of the frame asked for (the poster is frame 42).
fn answer(preview: &mut Preview, mut effects: Vec<preview::Effect>, timed: bool) {
    while let Some(effect) = effects.pop() {
        if let Command::Picture {
            generation,
            target,
            frame,
            view,
            size,
        } = effect.command
        {
            let origin = match target {
                PictureTarget::Source { .. } => PictureOrigin::Source,
                PictureTarget::Result { .. } => PictureOrigin::Result,
            };
            let index = match frame {
                PictureFrame::Poster => 42,
                PictureFrame::Index { index } => index,
            };
            effects.extend(preview.picture(paint(generation, origin, index, view, size, timed)));
        }
    }
}

/// The painted test frame is 1920x1080.
const FRAME: (u32, u32) = (1920, 1080);

/// A landscape at `frame`, as the engine would send it for `view` in
/// `size`: fitted (never enlarged) or a one-to-one crop. The result side is
/// coarser, as a lossy encode can be.
// A test picture: every value is bounded by the frame size or 0..=255.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn paint(
    generation: u64,
    origin: PictureOrigin,
    frame: u64,
    view: PictureView,
    size: PictureBox,
    timed: bool,
) -> Picture {
    let (fw, fh) = FRAME;
    let (width, height, x0, y0, step) = match view {
        PictureView::Fit => {
            let scale = (f64::from(size.width) / f64::from(fw))
                .min(f64::from(size.height) / f64::from(fh))
                .min(1.0);
            let w = (f64::from(fw) * scale).round().max(1.0);
            let h = (f64::from(fh) * scale).round().max(1.0);
            (w as u16, h as u16, 0, 0, 1.0 / scale)
        }
        PictureView::Native { x, y } => {
            let w = size.width.min(fw as u16);
            let h = size.height.min(fh as u16);
            let x = x.min(fw - u32::from(w));
            let y = y.min(fh - u32::from(h));
            (w, h, x, y, 1.0)
        }
    };
    let coarse = origin != PictureOrigin::Source;
    let shift = frame as f64 * 3.0;
    let mut bytes = Vec::with_capacity(usize::from(width) * usize::from(height) * 3);
    for row in 0..height {
        for col in 0..width {
            let mut fx = f64::from(x0) + f64::from(col) * step;
            let mut fy = f64::from(y0) + f64::from(row) * step;
            if coarse {
                fx = (fx / 6.0).floor() * 6.0;
                fy = (fy / 6.0).floor() * 6.0;
            }
            bytes.extend_from_slice(&landscape(fx, fy, shift, coarse));
        }
    }
    let header = PictureHeader {
        generation,
        origin,
        frame,
        time: timed.then_some(MediaTime {
            num: frame * 1001,
            den: 30000,
        }),
        frame_count: Some(300),
        width,
        height,
        frame_width: fw,
        frame_height: fh,
        view: match view {
            PictureView::Fit => PictureView::Fit,
            PictureView::Native { .. } => PictureView::Native { x: x0, y: y0 },
        },
        basis: ColorBasis {
            matrix: ColorMatrix::Bt709,
            assumed: true,
        },
    };
    Picture::new(generation, header, &bytes).unwrap_or_else(|| unreachable!("painted to size"))
}

/// One pixel of the landscape at frame coordinates `x`, `y`.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::many_single_char_names
)]
fn landscape(x: f64, y: f64, shift: f64, coarse: bool) -> [u8; 3] {
    let (w, h) = (f64::from(FRAME.0), f64::from(FRAME.1));
    let mix = |a: [f64; 3], b: [f64; 3], t: f64| {
        let t = t.clamp(0.0, 1.0);
        [
            a[0] + (b[0] - a[0]) * t,
            a[1] + (b[1] - a[1]) * t,
            a[2] + (b[2] - a[2]) * t,
        ]
    };
    let horizon = h * 0.58;
    // Sky to haze, with a low sun.
    let mut c = mix([46.0, 92.0, 158.0], [244.0, 196.0, 150.0], y / horizon);
    let (sx, sy) = (w * 0.68, h * 0.47);
    let d = ((x - sx).powi(2) + (y - sy).powi(2)).sqrt();
    if d < 70.0 {
        c = [255.0, 236.0, 196.0];
    } else if d < 260.0 {
        c = mix([255.0, 222.0, 170.0], c, (d - 70.0) / 190.0);
    }
    // Far hills, near hills, then water with the sun's reflection.
    let far = horizon - 60.0 - 50.0 * ((x + shift) / 260.0).sin() - 25.0 * (x / 97.0).cos();
    let near = horizon + 40.0 - 70.0 * ((x - shift * 2.0) / 410.0).sin().abs();
    if y > far {
        c = mix([96.0, 112.0, 150.0], [70.0, 84.0, 110.0], (y - far) / 120.0);
    }
    if y > near {
        c = mix([52.0, 88.0, 60.0], [24.0, 46.0, 34.0], (y - near) / 200.0);
    }
    if y > h * 0.8 {
        let ripple = 0.5 + 0.5 * ((y * 0.35) + (x / 40.0).sin() * 2.0).sin();
        c = mix([30.0, 58.0, 92.0], [70.0, 110.0, 150.0], ripple * 0.6);
        if (x - sx).abs() < 60.0 + (y - h * 0.8) * 0.4 && ripple > 0.55 {
            c = mix(c, [255.0, 220.0, 170.0], 0.7);
        }
    }
    if coarse {
        // Fewer levels, as banding in a low-rate encode.
        c = c.map(|v| (v / 12.0).round() * 12.0);
    }
    c.map(|v| v.clamp(0.0, 255.0) as u8)
}

/// Waits for the first frames, then writes `component`'s pixels to `out`.
fn capture<T: ComponentHandle + 'static>(
    component: &T,
    out: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let weak = component.as_weak();
    let out = out.to_owned();
    let (tx, rx) = std::sync::mpsc::channel();
    // Long enough for the first frames to land.
    slint::Timer::single_shot(Duration::from_millis(800), move || {
        let result = weak
            .upgrade()
            .ok_or_else(|| "window closed".to_owned())
            .and_then(|w| w.window().take_snapshot().map_err(|e| e.to_string()))
            .and_then(|pixels| write_ppm(&out, &pixels).map_err(|e| e.to_string()));
        let _ = tx.send(result);
        let _ = slint::quit_event_loop();
    });
    slint::run_event_loop()?;
    rx.recv()??;
    Ok(())
}

fn write_ppm(path: &str, pixels: &SharedPixelBuffer<Rgba8Pixel>) -> std::io::Result<()> {
    let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
    write!(file, "P6\n{} {}\n255\n", pixels.width(), pixels.height())?;
    for p in pixels.as_slice() {
        file.write_all(&[p.r, p.g, p.b])?;
    }
    file.flush()
}
