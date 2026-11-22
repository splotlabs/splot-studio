// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Generated fixtures and job helpers shared by the integration tests.
//!
//! Media fixtures are made at test time by the `ffmpeg` on `PATH`, from
//! samples written here, into temporary directories. Nothing is committed
//! and nothing comes from third-party corpora. A missing `ffmpeg` or encoder
//! fails the test with the reason; it never skips.

#![allow(dead_code)]

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;

use studio_domain::job::{JobSpec, SourceFingerprint, WirePath};
use studio_domain::protocol::Event;
use studio_domain::settings::{Crf, EncoderSettings, OutputKind, SpeedPreset};

pub type TestResult = Result<(), Box<dyn Error>>;

/// A generated clip: its header tags and samples.
#[derive(Debug, Clone)]
pub struct Clip {
    pub width: usize,
    pub height: usize,
    /// The `C` tag, such as `420`, `420p10`, `444p10` or `mono`.
    pub colorspace: &'static str,
    pub frames: usize,
    /// Extra header tags, such as `It` or `A1:1`.
    pub tags: &'static str,
    pub rate: &'static str,
}

impl Clip {
    pub const fn new(width: usize, height: usize, colorspace: &'static str, frames: usize) -> Self {
        Self {
            width,
            height,
            colorspace,
            frames,
            tags: "Ip A1:1",
            rate: "30000:1001",
        }
    }

    pub fn depth(&self) -> u32 {
        if self.colorspace.contains("p10") || self.colorspace == "mono10" {
            10
        } else {
            8
        }
    }

    /// Plane sizes, chroma rounded up; monochrome has luma alone.
    pub fn planes(&self) -> Vec<(usize, usize)> {
        let (w, h) = (self.width, self.height);
        let half = |v: usize| v.div_ceil(2);
        let chroma = match &self.colorspace[..self.colorspace.len().min(3)] {
            "420" => Some((half(w), half(h))),
            "422" => Some((half(w), h)),
            "444" => Some((w, h)),
            _ => None,
        };
        let mut planes = vec![(w, h)];
        planes.extend(chroma.into_iter().flat_map(|c| [c, c]));
        planes
    }

    /// The samples of `frame`: ramps whose low bits all differ, so 10-bit
    /// truncation to 8 bits is visible.
    pub fn samples(&self, frame: usize) -> Vec<Vec<u16>> {
        let max = (1_usize << self.depth()) - 1;
        self.planes()
            .iter()
            .enumerate()
            .map(|(plane, &(w, h))| {
                (0..w * h)
                    .map(|i| {
                        let (x, y) = (i % w, i / w);
                        let v = (x * 37 + y * 11 + frame * 53 + plane * 101) % (max + 1);
                        u16::try_from(v).unwrap_or(0)
                    })
                    .collect()
            })
            .collect()
    }

    pub fn header(&self) -> String {
        format!(
            "YUV4MPEG2 W{} H{} F{} {} C{}\n",
            self.width, self.height, self.rate, self.tags, self.colorspace
        )
    }

    /// The whole file.
    pub fn bytes(&self) -> Vec<u8> {
        let mut bytes = self.header().into_bytes();
        for frame in 0..self.frames {
            bytes.extend_from_slice(b"FRAME\n");
            for plane in self.samples(frame) {
                for sample in plane {
                    if self.depth() > 8 {
                        bytes.extend_from_slice(&sample.to_le_bytes());
                    } else {
                        bytes.push(u8::try_from(sample).unwrap_or(0));
                    }
                }
            }
        }
        bytes
    }

    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        fs::write(path, self.bytes())
    }

    /// The frames as raw planar samples (`-f rawvideo`), little-endian at
    /// 10 bits.
    pub fn raw(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        for frame in 0..self.frames {
            for plane in self.samples(frame) {
                push_samples(&mut bytes, &plane, self.depth());
            }
        }
        bytes
    }

    /// The frames in the semi-planar layout of `nv12` (8-bit) or `p010le`
    /// (10-bit, samples in the high bits) for a 4:2:0 clip.
    pub fn semi_planar(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        for frame in 0..self.frames {
            let planes = self.samples(frame);
            let shift = if self.depth() > 8 { 6 } else { 0 };
            let lift = |v: u16| v << shift;
            let luma: Vec<u16> = planes[0].iter().copied().map(lift).collect();
            push_samples(&mut bytes, &luma, self.depth());
            let chroma: Vec<u16> = planes[1]
                .iter()
                .zip(&planes[2])
                .flat_map(|(&u, &v)| [lift(u), lift(v)])
                .collect();
            push_samples(&mut bytes, &chroma, self.depth());
        }
        bytes
    }

    /// `ffmpeg`'s name for the planar layout.
    pub fn pix_fmt(&self) -> &'static str {
        match (self.colorspace, self.depth()) {
            ("420", _) => "yuv420p",
            ("422", _) => "yuv422p",
            ("444", _) => "yuv444p",
            ("mono", _) => "gray",
            ("420p10", _) => "yuv420p10le",
            ("422p10", _) => "yuv422p10le",
            ("444p10", _) => "yuv444p10le",
            ("mono10", _) => "gray10le",
            other => unreachable!("{other:?}"),
        }
    }

    /// The `-f rawvideo` input options for this clip in `pix_fmt`.
    /// Greyscale is stated as limited range: `FFmpeg` takes unstated grey
    /// as full range, which the engine refuses.
    pub fn raw_input(&self, pix_fmt: &str, path: &Path) -> Vec<String> {
        let mut args: Vec<String> = if pix_fmt.starts_with("gray") {
            vec!["-color_range".into(), "tv".into()]
        } else {
            Vec::new()
        };
        args.extend([
            "-f".into(),
            "rawvideo".into(),
            "-pix_fmt".into(),
            pix_fmt.into(),
            "-s".into(),
            format!("{}x{}", self.width, self.height),
            "-framerate".into(),
            self.rate.replace(':', "/"),
            "-i".into(),
            path.to_string_lossy().into_owned(),
        ]);
        args
    }
}

fn push_samples(bytes: &mut Vec<u8>, samples: &[u16], depth: u32) {
    for &sample in samples {
        if depth > 8 {
            bytes.extend_from_slice(&sample.to_le_bytes());
        } else {
            bytes.push(u8::try_from(sample).unwrap_or(0));
        }
    }
}

/// Runs the `ffmpeg` on `PATH` with `args` (after quiet, overwrite
/// options) and fails the test with its diagnostics if it does not succeed.
pub fn ffmpeg(args: &[&str]) -> Result<(), Box<dyn Error>> {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
        .args(args)
        .output()
        .map_err(|e| {
            format!(
                "these tests generate media with ffmpeg, which could not run ({e}); \
                 install FFmpeg and see `cargo xtask doctor`"
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "ffmpeg {args:?} failed ({}); a missing encoder or option in this FFmpeg build? {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(())
}

/// Encodes `clip` with ffmpeg: raw planar input, then `output_args`
/// (codec, filters, container options) and `destination`.
pub fn make(
    clip: &Clip,
    dir: &Path,
    output_args: &[&str],
    destination: &Path,
) -> Result<(), Box<dyn Error>> {
    let raw = dir.join(format!("raw-{}.yuv", std::process::id()));
    fs::write(&raw, clip.raw())?;
    let mut args: Vec<String> = clip.raw_input(clip.pix_fmt(), &raw);
    args.extend(output_args.iter().map(|&a| a.to_owned()));
    args.push(destination.to_string_lossy().into_owned());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let made = ffmpeg(&args);
    fs::remove_file(&raw)?;
    made
}

/// `ffmpeg`'s own raw planar decode of the selected stream.
pub fn raw_decode(source: &Path, stream: u32, pix_fmt: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-i"])
        .arg(source)
        .args(["-map", &format!("0:{stream}"), "-pix_fmt", pix_fmt])
        .args(["-f", "rawvideo", "pipe:1"])
        .output()?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned().into());
    }
    Ok(output.stdout)
}

/// The file's fingerprint now, or a placeholder for a missing file.
pub fn fingerprint(path: &Path) -> SourceFingerprint {
    fs::metadata(path).map_or(
        SourceFingerprint {
            len: 0,
            modified: None,
        },
        |metadata| SourceFingerprint::of(&metadata),
    )
}

pub fn settings(output: OutputKind) -> EncoderSettings {
    EncoderSettings {
        output,
        preset: SpeedPreset::new(10).unwrap_or_else(|_| unreachable!()),
        crf: Crf::new(40).unwrap_or_else(|_| unreachable!()),
    }
}

/// A job on stream 0 of the file as it is now, acknowledging nothing.
pub fn spec(source: &Path, destination: &Path, output: OutputKind) -> JobSpec {
    JobSpec {
        source: WirePath::from_path(source),
        fingerprint: fingerprint(source),
        video_stream: 0,
        omissions_acknowledged: false,
        destination: WirePath::from_path(destination),
        settings: settings(output),
    }
}

/// The process ids of `pid`'s live `ffmpeg` or `ffprobe` children whose
/// command line mentions `needle` (tests in one process run concurrently).
#[cfg(target_os = "linux")]
pub fn tool_children(pid: u32, needle: &str) -> Vec<u32> {
    let mut found = Vec::new();
    let Ok(tasks) = fs::read_dir(format!("/proc/{pid}/task")) else {
        return found;
    };
    for task in tasks.flatten() {
        let children = fs::read_to_string(task.path().join("children")).unwrap_or_default();
        for child in children.split_whitespace().filter_map(|c| c.parse().ok()) {
            let cmdline = fs::read(format!("/proc/{child}/cmdline")).unwrap_or_default();
            if alive_tool(child) && String::from_utf8_lossy(&cmdline).contains(needle) {
                found.push(child);
            }
        }
    }
    found
}

/// Whether `pid` is a running (not zombie) `ffmpeg` or `ffprobe`.
#[cfg(target_os = "linux")]
pub fn alive_tool(pid: u32) -> bool {
    let comm = fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    // The state follows the parenthesized command name.
    let state = stat.rsplit_once(") ").map(|(_, rest)| rest.chars().next());
    matches!(comm.trim(), "ffmpeg" | "ffprobe") && !matches!(state, Some(Some('Z' | 'X')))
}

/// Runs `job` on a job thread, as the worker does, collecting its events;
/// `hook` sees each one first and may set `cancel` or touch files.
pub fn run_job(
    job: JobSpec,
    hook: impl FnMut(&Event, &AtomicBool) + Send + 'static,
) -> (Vec<Event>, Event) {
    let handle = studio_engine::job::spawn(move || {
        let cancel = AtomicBool::new(false);
        let mut hook = hook;
        let mut events = Vec::new();
        let end = studio_engine::job::run(&job, &cancel, &mut |event| {
            hook(&event, &cancel);
            events.push(event);
        });
        (events, end)
    });
    match handle.map(std::thread::JoinHandle::join) {
        Ok(Ok(result)) => result,
        _ => unreachable!("the job thread failed"),
    }
}

/// Everything in `dir` except `keep`, which must be empty after a job that
/// did not publish: no temporary file may remain.
pub fn leftovers(dir: &Path, keep: &[&Path]) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(dir).map_or_else(
        |_| Vec::new(),
        |entries| entries.filter_map(|e| Some(e.ok()?.path())).collect(),
    );
    found.retain(|path| !keep.iter().any(|k| path == k));
    found.sort();
    found
}

/// Splot's raw decode of an output file.
pub fn decode(path: &Path) -> Result<Vec<u8>, Box<dyn Error>> {
    use splot_decode::{DecodeContext, DecodeOptions, DecodeRuntimeConfig, ThreadCount};
    let bytes = fs::read(path)?;
    let handle = studio_engine::job::spawn(move || {
        let context = DecodeContext::new(DecodeRuntimeConfig::new(ThreadCount::Auto))?;
        let mut out = Vec::new();
        context.decode_raw_bytes(&bytes, DecodeOptions::default(), &mut out)?;
        Ok::<_, splot_decode::DecodeError>(out)
    })?;
    match handle.join() {
        Ok(result) => Ok(result?),
        Err(_) => Err("the decode thread panicked".into()),
    }
}

/// The path `Working` announced, asserting it came exactly once, after
/// `Stage::Encoding` and before the first `Progress` or later stage, and
/// that it lies in `dir`. `None` when no `Working` was sent.
pub fn announced(events: &[Event], dir: &Path) -> Option<PathBuf> {
    use studio_domain::job::Stage;
    let at: Vec<usize> = (0..events.len())
        .filter(|&i| matches!(events[i], Event::Working { .. }))
        .collect();
    let [at] = at.as_slice() else {
        assert!(at.is_empty(), "Working sent {} times", at.len());
        return None;
    };
    let before = |f: fn(&Event) -> bool| events.iter().position(f).is_none_or(|i| *at < i);
    assert!(
        events[..*at].iter().any(|e| matches!(
            e,
            Event::Stage {
                stage: Stage::Encoding
            }
        )),
        "Working before Stage::Encoding"
    );
    assert!(
        before(|e| matches!(e, Event::Progress { .. })),
        "Working after Progress"
    );
    assert!(
        before(
            |e| matches!(e, Event::Stage { stage } if *stage != Stage::Encoding && *stage != Stage::Checking)
        ),
        "Working after a later stage"
    );
    let Event::Working { partial } = &events[*at] else {
        return None;
    };
    let path = partial.to_path_buf().ok()?;
    assert_eq!(
        path.parent(),
        Some(dir),
        "{} is not beside the destination",
        path.display()
    );
    Some(path)
}

/// Like [`run_job`], also checking while the job runs that the announced
/// file is an existing regular file. Returns the events, the end and the
/// announced path.
pub fn run_announced(
    job: JobSpec,
    mut hook: impl FnMut(&Event, &AtomicBool) + Send + 'static,
) -> (Vec<Event>, Event, Option<PathBuf>) {
    let destination = job.destination.to_path_buf().unwrap_or_default();
    let dir = destination.parent().map_or_default(Path::to_path_buf);
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = std::sync::Arc::clone(&seen);
    let (events, end) = run_job(job, move |event, cancel| {
        if let Event::Working { partial } = event {
            let live = partial
                .to_path_buf()
                .ok()
                .and_then(|p| fs::symlink_metadata(p).ok())
                .is_some_and(|m| m.is_file());
            if let Ok(mut seen) = sink.lock() {
                seen.push(live);
            }
        }
        hook(event, cancel);
    });
    let seen = seen.lock().map_or_default(|s| s.clone());
    let path = announced(&events, &dir);
    assert_eq!(seen.is_empty(), path.is_none());
    assert!(
        seen.iter().all(|live| *live),
        "the announced file did not exist"
    );
    if let Some(path) = &path {
        assert!(
            !path.exists(),
            "{} remains after the terminal event",
            path.display()
        );
    }
    (events, end, path)
}
