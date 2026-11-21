// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Far frames on the `FFmpeg` route, found by time: every container family
//! the engine reads, B-frames and reordering, start offsets, both depths.
//! MPEG-TS falls back to counting, which must be exact too.
//! Each frame must equal the frame the job's reader hands the encoder at
//! that index (counted from the first), and the clip's frames all differ.

mod common;

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use common::{Clip, TestResult, make};
use studio_engine::source::{self, Selection};

type Fallible<T> = Result<T, Box<dyn std::error::Error>>;

/// Frames a running decode reads through before restarting, as the
/// preview uses.
const READ_AHEAD: u64 = 48;

fn encoder_frames(path: &Path) -> Fallible<Vec<[Vec<u16>; 3]>> {
    let mut source = source::open(path, Selection::default()).map_err(|e| e.to_string())?;
    source
        .start(&AtomicBool::new(false))
        .map_err(|e| e.to_string())?;
    let mut frames = Vec::new();
    while let Some(planes) = source.read_frame().map_err(|e| e.to_string())? {
        frames.push(planes);
    }
    Ok(frames)
}

/// Opens `path` and reads each of `indices` by positioning first.
fn check(path: &Path, frames: &[[Vec<u16>; 3]], indices: &[u64]) -> Fallible<bool> {
    let mut source = source::open(path, Selection::default()).map_err(|e| e.to_string())?;
    for &index in indices {
        source
            .position_at(index, READ_AHEAD)
            .map_err(|e| e.to_string())?;
        let got = source.read_frame().map_err(|e| e.to_string())?;
        let want = usize::try_from(index).ok().and_then(|i| frames.get(i));
        assert!(
            got.as_ref() == want,
            "{}: frame {index} is not the encoder's frame {index}",
            path.display()
        );
    }
    Ok(source.reads_by_time() == Some(true))
}

#[test]
fn far_frames_are_exact_in_every_container() -> TestResult {
    let dir = tempfile::tempdir()?;
    let x264 = ["-c:v", "libx264", "-bf", "3", "-g", "48"];
    let cases: Vec<(&str, &'static str, Vec<&str>)> = vec![
        (
            "h264-8.mp4",
            "420",
            [&x264[..], &["-pix_fmt", "yuv420p"]].concat(),
        ),
        (
            "h264-8-offset.mp4",
            "420",
            [
                &x264[..],
                &["-pix_fmt", "yuv420p", "-output_ts_offset", "2"],
            ]
            .concat(),
        ),
        (
            "h264-10.mov",
            "420p10",
            [&x264[..], &["-pix_fmt", "yuv420p10le"]].concat(),
        ),
        (
            "h264-8-offset.mkv",
            "420",
            [
                &x264[..],
                &["-pix_fmt", "yuv420p", "-output_ts_offset", "3"],
            ]
            .concat(),
        ),
        (
            "vp9-8.webm",
            "420",
            vec!["-c:v", "libvpx-vp9", "-g", "48", "-b:v", "0", "-crf", "40"],
        ),
        (
            "h264-10.ts",
            "420p10",
            [&x264[..], &["-pix_fmt", "yuv420p10le"]].concat(),
        ),
        (
            "mpeg4-8.avi",
            "420",
            // AVI stores no timestamps for reordered frames, so a job
            // refuses B-frames there; this one has P-frames only.
            vec!["-c:v", "mpeg4", "-bf", "0", "-g", "48", "-q:v", "2"],
        ),
    ];
    let indices = [239, 0, 150, 151, 152, 100, 1, 238, 49, 120, 200, 60];
    for (name, colorspace, args) in cases {
        let clip = Clip::new(64, 48, colorspace, 240);
        let path = dir.path().join(name);
        make(&clip, dir.path(), &args, &path)?;
        let frames = encoder_frames(&path)?;
        assert_eq!(frames.len(), 240, "{name}");
        // Index-coded: no two frames are alike.
        assert!(frames.windows(2).all(|w| w[0] != w[1]), "{name}");
        let started = Instant::now();
        let by_time = check(&path, &frames, &indices)?;
        // An MPEG-TS seek lands on any packet, so its far frames are
        // counted; every other family is read by time.
        let ts = path.extension().is_some_and(|e| e == "ts");
        assert_eq!(by_time, !ts, "{name}: read by time");
        eprintln!(
            "{name}: {} far reads in {:?}",
            indices.len(),
            started.elapsed()
        );
        // Past the end: nothing, not a wrong frame.
        let mut source = source::open(&path, Selection::default()).map_err(|e| e.to_string())?;
        source
            .position_at(240, READ_AHEAD)
            .map_err(|e| e.to_string())?;
        assert!(
            source.read_frame().map_err(|e| e.to_string())?.is_none(),
            "{name}"
        );
    }
    Ok(())
}
