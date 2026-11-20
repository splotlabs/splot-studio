// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Renders the real window once and writes a renderer snapshot as binary PPM.
//!
//! ```text
//! cargo run -p studio-app --example snapshot -- OUT.ppm light|dark [about] [advanced] [long] [SCENARIO]
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
//! opened without one).

use std::io::Write;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Rgba8Pixel, SharedPixelBuffer};
use studio_app::Palette;
use studio_app::activity::Input;
use studio_app::state::{AppState, Effect, Intent, ProbeFailure};
use studio_domain::job::{
    Check, CheckKind, CheckOutcome, ChromaLayout, Disclosure, Failure, FailureCode, JobReport,
    Omissions, Progress, Rational, SourceFacts, SourceFingerprint, SourceReader, Stage, VideoTrack,
    WirePath,
};
use studio_domain::protocol::{Capabilities, Event, MediaTool, MediaToolOrigin};
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
    }
}

/// Plays `scenario` into a fresh state, as the controller would. `long`
/// uses a long file name, a long failure message and long check details.
#[allow(clippy::too_many_lines)]
fn scenario(name: &str, long: bool, now: Instant) -> Result<AppState, String> {
    let mut state = AppState::default();
    if name == "engine-missing" {
        state.apply(
            Intent::EngineAnswered(Err(
                "The encoding engine is missing (/opt/splot-studio/splot-studio-engine). \
                 Reinstall Splot Studio."
                    .into(),
            )),
            now,
        );
        return Ok(state);
    }
    let tool = !name.starts_with("tool-missing");
    state.apply(Intent::EngineAnswered(Ok(capabilities(tool))), now);
    if matches!(name, "empty" | "tool-missing") {
        return Ok(state);
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
    let Effect::Probe(request) = state.apply(Intent::PickFinished(Some(source.into())), now) else {
        return Err("picking did not probe".into());
    };
    let id = request.id;
    // 300 frames of 1920x1080 10-bit 4:2:0 Y4M, or a phone clip.
    state.note_source_size(Some(if ordinary { 412_733_901 } else { 1_866_247_800 }));
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
        "probing" => return Ok(state),
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
    state.apply(Intent::ProbeAnswered { id, result }, now);
    if matches!(
        name,
        "probed" | "unreadable" | "ordinary" | "unsupported" | "tool-missing-file"
    ) {
        return Ok(state);
    }

    // The checking scenario is two seconds in; the others 1:23.
    let ago = if name == "checking" { 2 } else { 83 };
    let start = now.checked_sub(Duration::from_secs(ago)).unwrap_or(now);
    let id = match state.apply(Intent::EncodeRequested, start) {
        Effect::Start { id, .. } => id,
        Effect::None if state.confirmation().is_some() => {
            if name == "confirm" {
                return Ok(state);
            }
            let Effect::Start { id, .. } = state.apply(Intent::OmissionsConfirmed, start) else {
                return Err("confirming did not start".into());
            };
            id
        }
        _ => return Err("encode did not start".into()),
    };
    let mut job = |input: Input| {
        state.apply(Intent::Job { id, input }, now);
    };
    if ordinary {
        match name {
            // No stage and no counts yet: the engine is checking timestamps.
            "checking" => {}
            "unsupported-job" => job(Input::Event(Event::Failed {
                failure: Failure {
                    code: FailureCode::UnsupportedSource,
                    message: "The video has a variable frame rate: frame 412 lasts 1/30 s \
                              after frames of 1/25 s"
                        .into(),
                },
            })),
            "changed" => {
                let failed = Input::Event(Event::Failed {
                    failure: Failure {
                        code: FailureCode::SourceChanged,
                        message: "harbour at dusk.mp4 changed since it was read".into(),
                    },
                });
                // The app reads the file again by itself; show it read.
                let Effect::Job {
                    reprobe: Some(request),
                    ..
                } = state.apply(Intent::Job { id, input: failed }, now)
                else {
                    return Err("a changed source was not read again".into());
                };
                state.apply(
                    Intent::ProbeAnswered {
                        id: request.id,
                        result: Ok(mp4_facts()),
                    },
                    now,
                );
            }
            other => return Err(format!("unknown scenario {other:?}")),
        }
        return Ok(state);
    }
    job(Input::Event(Event::Stage {
        stage: Stage::Encoding,
    }));
    job(Input::Event(Event::Progress {
        progress: Progress {
            frames_read: 131,
            frames_encoded: 117,
            bytes_written: 2_481_337,
        },
    }));
    match name {
        "running" | "running-unknown" => {}
        "cancelling" => {
            job(Input::CancelPressed);
            job(Input::Event(Event::CancelRequested));
        }
        "completed" => {
            for stage in [Stage::Finalizing, Stage::Verifying, Stage::Publishing] {
                job(Input::Event(Event::Stage { stage }));
            }
            job(Input::Event(Event::Completed {
                report: JobReport {
                    destination: WirePath::Utf8(source.replace(".y4m", ".ivf")),
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
                },
            }));
        }
        "cancelled" => {
            job(Input::CancelPressed);
            job(Input::Event(Event::CancelRequested));
            job(Input::Event(Event::Cancelled));
        }
        "failed" => job(Input::Event(Event::Failed {
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
        })),
        "lost" => job(Input::WorkerEnded {
            detail: "signal: 9 (SIGKILL)".into(),
        }),
        other => return Err(format!("unknown scenario {other:?}")),
    }
    Ok(state)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [out, scheme, rest @ ..] = args.as_slice() else {
        return Err(
            "usage: snapshot OUT.ppm light|dark [about] [advanced] [long] [SCENARIO]".into(),
        );
    };
    let scheme = match scheme.as_str() {
        "light" => slint::language::ColorScheme::Light,
        "dark" => slint::language::ColorScheme::Dark,
        other => return Err(format!("unknown scheme {other:?}").into()),
    };

    let window = studio_app::new_window()?;
    window.global::<Palette>().set_color_scheme(scheme);
    let now = Instant::now();
    let long = rest.iter().any(|a| a == "long");
    let mut name = "empty";
    for arg in rest {
        match arg.as_str() {
            "about" => window.invoke_show_about(),
            "advanced" => window.set_advanced_open(true),
            "long" => {}
            other => name = other,
        }
    }
    let state = scenario(name, long, now)?;
    studio_app::render(&window, &state, now);
    window.set_engine_identity("renderer snapshot, no engine ran".into());
    window.show()?;

    let weak = window.as_weak();
    let out = out.clone();
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
