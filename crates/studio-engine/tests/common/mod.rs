// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Generated Y4M fixtures and job helpers shared by the integration tests.

#![allow(dead_code)]

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use studio_domain::job::{JobSpec, WirePath};
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
}

pub fn settings(output: OutputKind) -> EncoderSettings {
    EncoderSettings {
        output,
        preset: SpeedPreset::new(10).unwrap_or_else(|_| unreachable!()),
        crf: Crf::new(40).unwrap_or_else(|_| unreachable!()),
    }
}

pub fn spec(source: &Path, destination: &Path, output: OutputKind) -> JobSpec {
    JobSpec {
        source: WirePath::from_path(source),
        destination: WirePath::from_path(destination),
        settings: settings(output),
    }
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
