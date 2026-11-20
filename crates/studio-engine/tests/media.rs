// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Media-file tests: fixtures made by the `ffmpeg` on `PATH` from known
//! samples, read through the engine's `FFmpeg` route, in process. They need
//! `SPLOT_STUDIO_SYSTEM_FFMPEG=1`, which Cargo sets (`.cargo/config.toml`).

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use common::{Clip, TestResult, ffmpeg, leftovers, make, raw_decode, run_job, spec};
use splot_encode::SourceFrame;
use splot_encode::y4m::Y4mReader;
use studio_domain::job::{
    CheckKind, CheckOutcome, ChromaLayout, Disclosure, FailureCode, Omissions, Rational,
    SourceReader, Stage,
};
use studio_domain::protocol::{Event, MediaToolOrigin};
use studio_domain::settings::OutputKind;
use studio_engine::job::Stop;
use studio_engine::source::{self, Selection};

type Fallible<T> = Result<T, Box<dyn std::error::Error>>;

fn failure(end: &Event) -> Option<(FailureCode, String)> {
    match end {
        Event::Failed { failure } => Some((failure.code, failure.message.clone())),
        _ => None,
    }
}

fn probe_code(path: &Path, stream: Option<u32>) -> Option<FailureCode> {
    match source::probe(path, stream) {
        Err(Stop::Failed(failure)) => Some(failure.code),
        _ => None,
    }
}

/// Encodes stream `stream` of `src` and asserts a verified, published file
/// with `frames` frames.
fn encode_ok(src: &Path, stream: u32, kind: OutputKind, frames: u64) -> Fallible<Vec<Event>> {
    let dir = src.parent().ok_or("no parent")?;
    let dst = dir.join(format!("out-{stream}.{}", kind.extension()));
    let mut job = spec(src, &dst, kind);
    job.video_stream = stream;
    job.omissions_acknowledged = true;
    let (events, end) = run_job(job, |_, _| {});
    let Event::Completed { report } = end else {
        return Err(format!("{}: {end:?}", src.display()).into());
    };
    assert_eq!(report.frames, frames);
    let outcomes: Vec<_> = report.checks.iter().map(|c| (c.kind, &c.outcome)).collect();
    assert!(
        matches!(
            outcomes.as_slice(),
            [
                (CheckKind::Structure, CheckOutcome::Passed),
                (CheckKind::SplotDecode, CheckOutcome::Passed),
                (CheckKind::AvmDecode, CheckOutcome::NotRun { .. }),
            ]
        ),
        "{outcomes:?}"
    );
    assert_eq!(common::decode(&dst)?.len() as u64 % frames, 0);
    fs::remove_file(&dst)?;
    Ok(events)
}

/// Every frame the source hands to the encoder.
fn frames_of(src: &Path, stream: Option<u32>) -> Fallible<Vec<[Vec<u16>; 3]>> {
    let mut source = source::open(
        src,
        Selection {
            fingerprint: None,
            video_stream: stream,
        },
    )
    .map_err(|e| e.to_string())?;
    source
        .start(&AtomicBool::new(false))
        .map_err(|e| e.to_string())?;
    let config = studio_engine::job::config(&source.header, common::settings(OutputKind::Ivf))
        .map_err(|e| e.to_string())?;
    let mut frames = Vec::new();
    while let Some(planes) = source.read_frame().map_err(|e| e.to_string())? {
        // What the encoder is given, after the codec's own checks.
        let frame = SourceFrame::new(&config, frames.len() as u64, planes)?;
        frames.push(frame.planes().clone());
    }
    Ok(frames)
}

fn expected_planes(clip: &Clip, frame: usize) -> Vec<Vec<u16>> {
    let mut planes = clip.samples(frame);
    planes.resize(3, Vec::new());
    planes
}

/// Raw planar bytes as 16-bit samples, plane by plane, frame by frame.
fn split_raw(clip: &Clip, bytes: &[u8]) -> Vec<Vec<Vec<u16>>> {
    let wide = clip.depth() > 8;
    let mut samples = bytes.chunks(if wide { 2 } else { 1 }).map(|c| {
        if wide {
            u16::from_le_bytes([c[0], c[1]])
        } else {
            u16::from(c[0])
        }
    });
    (0..clip.frames)
        .map(|_| {
            let mut planes: Vec<Vec<u16>> = clip
                .planes()
                .iter()
                .map(|&(w, h)| samples.by_ref().take(w * h).collect())
                .collect();
            planes.resize(3, Vec::new());
            planes
        })
        .collect()
}

#[test]
fn the_system_tool_is_found_through_the_opt_in() -> TestResult {
    let tool = studio_engine::media::tool().map_err(|e| e.to_string())?;
    assert_eq!(tool.identity.origin, MediaToolOrigin::System);
    assert_ne!(tool.identity.version, "");
    Ok(())
}

/// MP4/H.264 8-bit 4:2:0: the inventory, then a verified IVF and OBU.
#[test]
fn mp4_h264_8_bit_encodes_to_ivf_and_obu() -> TestResult {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("in.mp4");
    let clip = Clip::new(64, 48, "420", 8);
    make(
        &clip,
        dir.path(),
        &["-c:v", "libx264", "-pix_fmt", "yuv420p"],
        &src,
    )?;
    let facts = source::probe(&src, None).map_err(|e| e.to_string())?;
    assert_eq!(facts.reader, SourceReader::Ffmpeg);
    assert_eq!(facts.fingerprint, common::fingerprint(&src));
    assert_eq!((facts.width, facts.height, facts.bit_depth), (64, 48, 8));
    assert_eq!(facts.chroma, ChromaLayout::Yuv420);
    assert_eq!(
        facts.frame_rate,
        Rational {
            num: 30000,
            den: 1001
        }
    );
    assert_eq!(facts.frame_count, None);
    assert_eq!(facts.omissions, Omissions::default());
    assert_eq!(facts.video_tracks.len(), 1);
    assert_eq!(facts.video_tracks[0].codec, "h264");
    let events = encode_ok(&src, 0, OutputKind::Ivf, 8)?;
    let stages: Vec<Stage> = events
        .iter()
        .filter_map(|e| match e {
            Event::Stage { stage } => Some(*stage),
            _ => None,
        })
        .collect();
    assert_eq!(
        stages,
        [
            Stage::Checking,
            Stage::Encoding,
            Stage::Finalizing,
            Stage::Verifying,
            Stage::Publishing
        ]
    );
    encode_ok(&src, 0, OutputKind::Av2AnnexB, 8)?;
    Ok(())
}

/// Lossless 10-bit 4:2:0 in Matroska at 29.97 fps (millisecond ticks, a
/// fractional frame period), odd size: the samples reaching the encoder are
/// the fixture's, low bits included; then a verified OBU.
#[test]
fn mkv_10_bit_preserves_samples_and_encodes() -> TestResult {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("in.mkv");
    let clip = Clip::new(17, 9, "420p10", 6);
    make(&clip, dir.path(), &["-c:v", "ffv1"], &src)?;
    let facts = source::probe(&src, None).map_err(|e| e.to_string())?;
    assert_eq!((facts.width, facts.height, facts.bit_depth), (17, 9, 10));
    let frames = frames_of(&src, None)?;
    assert_eq!(frames.len(), clip.frames);
    for (i, frame) in frames.iter().enumerate() {
        assert_eq!(
            frame.as_slice(),
            expected_planes(&clip, i).as_slice(),
            "frame {i}"
        );
    }
    assert!(clip.samples(0)[0].iter().any(|s| s & 3 != 0));
    encode_ok(&src, 0, OutputKind::Av2AnnexB, 6)?;
    Ok(())
}

/// Every planar layout at both depths reaches the encoder unchanged.
#[test]
fn planar_layouts_at_both_depths_are_preserved() -> TestResult {
    for colorspace in ["420", "422", "444", "mono", "422p10", "444p10", "mono10"] {
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("in.mkv");
        let clip = Clip {
            rate: "25:1",
            ..Clip::new(16, 8, colorspace, 3)
        };
        make(&clip, dir.path(), &["-c:v", "ffv1"], &src)?;
        let frames = frames_of(&src, None)?;
        assert_eq!(frames.len(), 3, "{colorspace}");
        for (i, frame) in frames.iter().enumerate() {
            assert_eq!(
                frame.as_slice(),
                expected_planes(&clip, i).as_slice(),
                "{colorspace} frame {i}"
            );
        }
    }
    Ok(())
}

/// Lossy input: the planes reaching the encoder equal ffmpeg's own raw
/// decode of the stream, at both depths.
#[test]
fn lossy_import_equals_ffmpegs_raw_decode() -> TestResult {
    for (colorspace, args, name) in [
        (
            "420",
            &["-c:v", "libx264", "-pix_fmt", "yuv420p"][..],
            "in.mp4",
        ),
        (
            "420p10",
            &["-c:v", "libx264", "-pix_fmt", "yuv420p10le"],
            "in.mkv",
        ),
    ] {
        let dir = tempfile::tempdir()?;
        let src = dir.path().join(name);
        let clip = Clip::new(32, 16, colorspace, 5);
        make(&clip, dir.path(), args, &src)?;
        let raw = raw_decode(&src, 0, clip.pix_fmt())?;
        let reference = split_raw(&clip, &raw);
        let frames = frames_of(&src, None)?;
        assert_eq!(frames.len(), reference.len(), "{name}");
        for (i, frame) in frames.iter().enumerate() {
            assert_eq!(
                frame.as_slice(),
                reference[i].as_slice(),
                "{name} frame {i}"
            );
        }
    }
    Ok(())
}

/// `nv12` (AVI and Matroska) is repacked to planar 4:2:0 with every sample
/// unchanged, and disclosed.
#[test]
fn nv12_is_repacked_bit_exactly() -> TestResult {
    for name in ["in.avi", "in.mkv"] {
        let dir = tempfile::tempdir()?;
        let clip = Clip {
            rate: "25:1",
            ..Clip::new(18, 10, "420", 3)
        };
        let raw = dir.path().join("nv12.raw");
        fs::write(&raw, clip.semi_planar())?;
        let src = dir.path().join(name);
        let mut args = clip.raw_input("nv12", &raw);
        args.extend(["-c:v".into(), "rawvideo".into()]);
        args.push(src.to_string_lossy().into_owned());
        ffmpeg(&args.iter().map(String::as_str).collect::<Vec<_>>())?;
        let facts = source::probe(&src, None).map_err(|e| e.to_string())?;
        assert!(
            facts.disclosures.contains(&Disclosure::LayoutRepacked),
            "{name}: {:?}",
            facts.disclosures
        );
        let frames = frames_of(&src, None)?;
        assert_eq!(frames.len(), 3);
        for (i, frame) in frames.iter().enumerate() {
            assert_eq!(
                frame.as_slice(),
                expected_planes(&clip, i).as_slice(),
                "{name} {i}"
            );
        }
    }
    Ok(())
}

/// No accepted container stores `p010le` from a software decoder, so the
/// repack the engine requests (`-pix_fmt yuv420p10le` into a Y4M pipe, see
/// `media::decode`) is checked on raw `p010le` input directly: every 10-bit
/// value, low bits included, comes back unchanged.
#[test]
fn p010_repack_keeps_every_bit() -> TestResult {
    let dir = tempfile::tempdir()?;
    let clip = Clip::new(18, 10, "420p10", 3);
    let raw = dir.path().join("p010.raw");
    fs::write(&raw, clip.semi_planar())?;
    let y4m = dir.path().join("out.y4m");
    let mut args = clip.raw_input("p010le", &raw);
    args.extend(
        [
            "-pix_fmt",
            "yuv420p10le",
            "-strict",
            "-1",
            "-f",
            "yuv4mpegpipe",
        ]
        .map(str::to_owned),
    );
    args.push(y4m.to_string_lossy().into_owned());
    ffmpeg(&args.iter().map(String::as_str).collect::<Vec<_>>())?;
    let mut reader = Y4mReader::new(fs::File::open(&y4m)?)?;
    for frame in 0..clip.frames {
        let planes = reader.read_frame()?.ok_or("missing frame")?;
        assert_eq!(planes.as_slice(), expected_planes(&clip, frame).as_slice());
    }
    assert!(reader.read_frame()?.is_none());
    assert!(clip.samples(1)[1].iter().any(|s| s & 3 != 0));
    Ok(())
}

/// Two videos, cover art, two audio tracks, subtitles, a font and chapters.
#[allow(clippy::too_many_lines)]
fn multi_track(dir: &Path) -> Fallible<PathBuf> {
    let main = Clip {
        rate: "25:1",
        ..Clip::new(32, 16, "420", 4)
    };
    let second = Clip {
        rate: "25:1",
        ..Clip::new(16, 8, "420", 4)
    };
    let (a, b) = (dir.join("a.yuv"), dir.join("b.yuv"));
    fs::write(&a, main.raw())?;
    fs::write(&b, second.raw())?;
    let audio = dir.join("audio.raw");
    fs::write(&audio, vec![0_u8; 8000 * 2 / 5])?;
    let cover = dir.join("cover.png");
    let rgb = dir.join("cover.rgb");
    fs::write(&rgb, vec![200_u8; 8 * 8 * 3])?;
    let rgb_s = rgb.to_string_lossy().into_owned();
    let cover_s = cover.to_string_lossy().into_owned();
    ffmpeg(&[
        "-f",
        "rawvideo",
        "-pix_fmt",
        "rgb24",
        "-s",
        "8x8",
        "-i",
        &rgb_s,
        "-frames:v",
        "1",
        &cover_s,
    ])?;
    let font = dir.join("font.ttf");
    fs::write(&font, b"not really a font")?;
    let subs = dir.join("subs.srt");
    fs::write(&subs, "1\n00:00:00,000 --> 00:00:00,100\nHello\n")?;
    let chapters = dir.join("chapters.txt");
    fs::write(
        &chapters,
        ";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=80\ntitle=One\n\
         [CHAPTER]\nTIMEBASE=1/1000\nSTART=80\nEND=160\ntitle=Two\n",
    )?;
    let src = dir.join("multi.mkv");
    let mut args = main.raw_input("yuv420p", &a);
    args.extend(second.raw_input("yuv420p", &b));
    let s = |p: &Path| p.to_string_lossy().into_owned();
    for _ in 0..2 {
        args.extend(["-f", "s16le", "-ar", "8000", "-ac", "1", "-i"].map(str::to_owned));
        args.push(s(&audio));
    }
    args.extend(["-i".to_owned(), s(&subs), "-i".to_owned(), s(&chapters)]);
    args.extend(
        [
            "-map",
            "0:v",
            "-map",
            "1:v",
            "-map",
            "2:a",
            "-map",
            "3:a",
            "-map",
            "4:s",
            "-map_chapters",
            "5",
            "-c:v",
            "ffv1",
            "-c:a",
            "flac",
            "-c:s",
            "srt",
            "-attach",
        ]
        .map(str::to_owned),
    );
    args.push(cover_s);
    args.extend(
        [
            "-metadata:s:t:0",
            "mimetype=image/png",
            "-metadata:s:t:0",
            "filename=cover.png",
            "-attach",
        ]
        .map(str::to_owned),
    );
    args.push(s(&font));
    args.extend(
        [
            "-metadata:s:t:1",
            "mimetype=application/x-truetype-font",
            "-metadata:s:t:1",
            "filename=font.ttf",
        ]
        .map(str::to_owned),
    );
    args.push(s(&src));
    ffmpeg(&args.iter().map(String::as_str).collect::<Vec<_>>())?;
    for leftover in [a, b, audio, rgb, cover, font, subs, chapters] {
        fs::remove_file(leftover)?;
    }
    Ok(src)
}

#[test]
fn multi_track_inventory_selection_and_acknowledgement() -> TestResult {
    let dir = tempfile::tempdir()?;
    let src = multi_track(dir.path())?;
    let facts = source::probe(&src, None).map_err(|e| e.to_string())?;
    let tracks: Vec<_> = facts
        .video_tracks
        .iter()
        .map(|t| (t.stream, t.width, t.height, t.attached_picture))
        .collect();
    assert_eq!(
        tracks,
        [(0, 32, 16, false), (1, 16, 8, false), (5, 8, 8, true)]
    );
    assert_eq!(facts.video_stream, 0);
    assert_eq!(
        facts.omissions,
        Omissions {
            other_video: 2,
            audio: 2,
            subtitles: 1,
            attachments: 1,
            other: 0,
            chapters: 2,
        }
    );
    let second = source::probe(&src, Some(1)).map_err(|e| e.to_string())?;
    assert_eq!(
        (second.video_stream, second.width, second.height),
        (1, 16, 8)
    );
    for refused in [5, 2, 4, 6, 99] {
        assert_eq!(
            probe_code(&src, Some(refused)),
            Some(FailureCode::UnsupportedSource),
            "stream {refused}"
        );
    }

    // Not acknowledged: refused before any output.
    let dst = dir.path().join("out.ivf");
    let (_, end) = run_job(spec(&src, &dst, OutputKind::Ivf), |_, _| {});
    assert_eq!(
        failure(&end).map(|f| f.0),
        Some(FailureCode::OmissionsNotAcknowledged),
        "{end:?}"
    );
    assert_eq!(leftovers(dir.path(), &[&src]), Vec::<PathBuf>::new());
    // A stream that is not there, or not video.
    for stream in [5, 2] {
        let mut job = spec(&src, &dst, OutputKind::Ivf);
        job.video_stream = stream;
        job.omissions_acknowledged = true;
        let (_, end) = run_job(job, |_, _| {});
        assert_eq!(
            failure(&end).map(|f| f.0),
            Some(FailureCode::UnsupportedSource),
            "{end:?}"
        );
    }
    // Acknowledged: both real videos encode.
    encode_ok(&src, 0, OutputKind::Ivf, 4)?;
    encode_ok(&src, 1, OutputKind::Av2AnnexB, 4)?;
    Ok(())
}

/// A source replaced or edited after the probe is refused, by both routes.
#[test]
fn a_source_changed_after_the_probe_is_refused() -> TestResult {
    let dir = tempfile::tempdir()?;
    let clip = Clip::new(16, 8, "420", 3);
    let mkv = dir.path().join("in.mkv");
    make(&clip, dir.path(), &["-c:v", "ffv1"], &mkv)?;
    let y4m = dir.path().join("in.y4m");
    clip.write(&y4m)?;
    for src in [&mkv, &y4m] {
        let job = spec(src, &dir.path().join("out.ivf"), OutputKind::Ivf);
        let other = Clip::new(16, 8, "420", 4);
        if src == &mkv {
            make(&other, dir.path(), &["-c:v", "ffv1"], src)?;
        } else {
            other.write(src)?;
        }
        let (events, end) = run_job(job, |_, _| {});
        assert_eq!(
            failure(&end).map(|f| f.0),
            Some(FailureCode::SourceChanged),
            "{src:?}: {end:?}"
        );
        assert!(events.is_empty(), "{events:?}");
    }
    assert_eq!(leftovers(dir.path(), &[&mkv, &y4m]), Vec::<PathBuf>::new());
    Ok(())
}

/// Real variable frame rate: one frame of the sequence is missing. Refused
/// during the scan, before a temporary file or a frame exists.
#[test]
fn variable_frame_rate_is_refused_before_any_output() -> TestResult {
    let gap = "setpts='if(gte(N,3),N+1,N)/25/TB'";
    for (name, codec) in [("vfr.mkv", "ffv1"), ("vfr.mp4", "libx264")] {
        let dir = tempfile::tempdir()?;
        let src = dir.path().join(name);
        let clip = Clip {
            rate: "25:1",
            ..Clip::new(32, 16, "420", 8)
        };
        make(
            &clip,
            dir.path(),
            &["-vf", gap, "-fps_mode", "passthrough", "-c:v", codec],
            &src,
        )?;
        // The header still declares 25 fps.
        let facts = source::probe(&src, None).map_err(|e| e.to_string())?;
        assert_eq!(facts.frame_rate, Rational { num: 25, den: 1 });
        let dst = dir.path().join("out.ivf");
        let (events, end) = run_job(spec(&src, &dst, OutputKind::Ivf), |_, _| {});
        let (code, message) = failure(&end).ok_or(format!("{end:?}"))?;
        assert_eq!(code, FailureCode::UnsupportedSource, "{message}");
        assert!(message.contains("variable frame rate"), "{message}");
        assert!(
            !events.iter().any(|e| matches!(e, Event::Progress { .. })),
            "{events:?}"
        );
        assert_eq!(leftovers(dir.path(), &[&src]), Vec::<PathBuf>::new());
    }
    Ok(())
}

/// The video starts two seconds in: disclosed, and encoded from zero.
#[test]
fn a_start_offset_is_disclosed() -> TestResult {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("late.mkv");
    let clip = Clip {
        rate: "25:1",
        ..Clip::new(16, 8, "420", 3)
    };
    make(
        &clip,
        dir.path(),
        &["-c:v", "ffv1", "-output_ts_offset", "2"],
        &src,
    )?;
    let facts = source::probe(&src, None).map_err(|e| e.to_string())?;
    assert!(
        facts.disclosures.contains(&Disclosure::StartOffsetDropped),
        "{:?}",
        facts.disclosures
    );
    encode_ok(&src, 0, OutputKind::Ivf, 3)?;
    Ok(())
}

/// What each container route reports and decodes: `.webm` (VP9), `.mov`
/// (`ProRes` 4:2:2 10-bit) and `.ts` (H.264).
#[test]
fn webm_mov_and_mpegts_are_read() -> TestResult {
    let cases: [(&str, &str, &[&str], ChromaLayout, u8); 3] = [
        (
            "420",
            "in.webm",
            &["-c:v", "libvpx-vp9", "-b:v", "0", "-crf", "40"],
            ChromaLayout::Yuv420,
            8,
        ),
        (
            "422p10",
            "in.mov",
            &["-c:v", "prores"],
            ChromaLayout::Yuv422,
            10,
        ),
        (
            "420",
            "in.ts",
            &["-c:v", "libx264"],
            ChromaLayout::Yuv420,
            8,
        ),
    ];
    for (colorspace, name, args, chroma, depth) in cases {
        let dir = tempfile::tempdir()?;
        let src = dir.path().join(name);
        let clip = Clip {
            rate: "25:1",
            ..Clip::new(32, 16, colorspace, 4)
        };
        make(&clip, dir.path(), args, &src)?;
        let facts = source::probe(&src, None).map_err(|e| e.to_string())?;
        assert_eq!((facts.chroma, facts.bit_depth), (chroma, depth), "{name}");
        assert_eq!(frames_of(&src, None)?.len(), 4, "{name}");
    }
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("in.webm");
    make(
        &Clip::new(32, 16, "420", 3),
        dir.path(),
        &["-c:v", "libvpx-vp9"],
        &src,
    )?;
    encode_ok(&src, 0, OutputKind::Ivf, 3)?;
    Ok(())
}

/// Makes the fixture for one refused case in `dir`.
type Maker = fn(&Path) -> Fallible<PathBuf>;

fn with_args(dir: &Path, name: &str, colorspace: &'static str, args: &[&str]) -> Fallible<PathBuf> {
    let src = dir.join(name);
    let clip = Clip {
        rate: "25:1",
        ..Clip::new(32, 16, colorspace, 4)
    };
    make(&clip, dir, args, &src)?;
    Ok(src)
}

fn interlaced(dir: &Path) -> Fallible<PathBuf> {
    with_args(
        dir,
        "tff.mkv",
        "420",
        &["-vf", "setfield=tff", "-c:v", "ffv1"],
    )
}

fn pq(dir: &Path) -> Fallible<PathBuf> {
    with_args(
        dir,
        "pq.mkv",
        "420p10",
        &[
            "-vf",
            "setparams=color_trc=smpte2084:color_primaries=bt2020:colorspace=bt2020nc",
            "-c:v",
            "ffv1",
        ],
    )
}

fn hlg(dir: &Path) -> Fallible<PathBuf> {
    with_args(
        dir,
        "hlg.mkv",
        "420p10",
        &[
            "-vf",
            "setparams=color_trc=arib-std-b67:color_primaries=bt2020:colorspace=bt2020nc",
            "-c:v",
            "ffv1",
        ],
    )
}

fn full_range(dir: &Path) -> Fallible<PathBuf> {
    with_args(
        dir,
        "full.mkv",
        "420",
        &["-color_range", "pc", "-c:v", "ffv1"],
    )
}

fn jpeg_range(dir: &Path) -> Fallible<PathBuf> {
    with_args(
        dir,
        "yuvj.mp4",
        "420",
        &["-c:v", "libx264", "-pix_fmt", "yuvj420p"],
    )
}

fn non_square(dir: &Path) -> Fallible<PathBuf> {
    with_args(
        dir,
        "sar.mkv",
        "420",
        &["-vf", "setsar=16/15", "-c:v", "ffv1"],
    )
}

fn rotated(dir: &Path) -> Fallible<PathBuf> {
    let plain = with_args(dir, "plain.mp4", "420", &["-c:v", "libx264"])?;
    let src = dir.join("rotated.mp4");
    let (p, s) = (plain.to_string_lossy(), src.to_string_lossy());
    ffmpeg(&["-display_rotation", "90", "-i", &p, "-c", "copy", &s])?;
    fs::remove_file(plain)?;
    Ok(src)
}

fn encrypted(dir: &Path) -> Fallible<PathBuf> {
    let key = "00112233445566778899aabbccddeeff";
    with_args(
        dir,
        "cenc.mp4",
        "420",
        &[
            "-c:v",
            "libx264",
            "-encryption_scheme",
            "cenc-aes-ctr",
            "-encryption_key",
            key,
            "-encryption_kid",
            key,
        ],
    )
}

/// Two H.264 segments of different sizes, back to back in one transport
/// stream: the frame size changes mid-stream.
fn size_change(dir: &Path) -> Fallible<PathBuf> {
    let a = with_args(dir, "a.ts", "420", &["-c:v", "libx264"])?;
    let b = dir.join("b.ts");
    let small = Clip {
        rate: "25:1",
        ..Clip::new(16, 16, "420", 4)
    };
    make(
        &small,
        dir,
        &["-c:v", "libx264", "-output_ts_offset", "0.16"],
        &b,
    )?;
    let src = dir.join("change.ts");
    fs::write(&src, [fs::read(&a)?, fs::read(&b)?].concat())?;
    fs::remove_file(a)?;
    fs::remove_file(b)?;
    Ok(src)
}

fn playlist(dir: &Path) -> Fallible<PathBuf> {
    let segment = with_args(dir, "segment.ts", "420", &["-c:v", "libx264"])?;
    let src = dir.join("list.m3u8");
    fs::write(
        &src,
        format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\n{}\n#EXT-X-ENDLIST\n",
            segment.to_string_lossy()
        ),
    )?;
    Ok(src)
}

fn concat_list(dir: &Path) -> Fallible<PathBuf> {
    with_args(dir, "part.mkv", "420", &["-c:v", "ffv1"])?;
    let src = dir.join("list.ffconcat");
    fs::write(&src, "ffconcat version 1.0\nfile part.mkv\n")?;
    Ok(src)
}

fn image(dir: &Path) -> Fallible<PathBuf> {
    with_args(dir, "still.png", "444", &["-frames:v", "1"])
}

fn truncated_mkv(dir: &Path) -> Fallible<PathBuf> {
    let clip = Clip {
        rate: "25:1",
        ..Clip::new(64, 48, "420", 30)
    };
    let src = dir.join("cut.mkv");
    make(&clip, dir, &["-c:v", "libx264", "-g", "10"], &src)?;
    let bytes = fs::read(&src)?;
    fs::write(&src, &bytes[..bytes.len() * 6 / 10])?;
    Ok(src)
}

fn truncated_mp4(dir: &Path) -> Fallible<PathBuf> {
    let src = with_args(dir, "cut.mp4", "420", &["-c:v", "libx264"])?;
    let bytes = fs::read(&src)?;
    fs::write(&src, &bytes[..bytes.len() / 2])?;
    Ok(src)
}

fn damaged(dir: &Path) -> Fallible<PathBuf> {
    let clip = Clip {
        rate: "25:1",
        ..Clip::new(64, 48, "420", 30)
    };
    let src = dir.join("damaged.mkv");
    make(&clip, dir, &["-c:v", "libx264", "-g", "10"], &src)?;
    let mut bytes = fs::read(&src)?;
    let middle = bytes.len() / 2;
    for byte in &mut bytes[middle..middle + 300] {
        *byte ^= 0x5a;
    }
    fs::write(&src, bytes)?;
    Ok(src)
}

/// Every refused source fails with its code, before or during the job,
/// publishes nothing and leaves no temporary file.
#[test]
fn refused_media() -> TestResult {
    let cases: [(&str, Maker, FailureCode, &str); 17] = [
        (
            "interlaced",
            interlaced,
            FailureCode::UnsupportedSource,
            "interlaced",
        ),
        ("HDR PQ", pq, FailureCode::UnsupportedSource, "HDR"),
        ("HDR HLG", hlg, FailureCode::UnsupportedSource, "HDR"),
        (
            "full range",
            full_range,
            FailureCode::UnsupportedSource,
            "full-range",
        ),
        (
            "JPEG range",
            jpeg_range,
            FailureCode::UnsupportedSource,
            "full-range",
        ),
        (
            "non-square",
            non_square,
            FailureCode::UnsupportedSource,
            "non-square",
        ),
        (
            "rotated",
            rotated,
            FailureCode::UnsupportedSource,
            "rotated",
        ),
        (
            "encrypted",
            encrypted,
            FailureCode::UnsupportedSource,
            "encrypted",
        ),
        (
            "size change",
            size_change,
            FailureCode::UnsupportedSource,
            "changes",
        ),
        (
            "playlist",
            playlist,
            FailureCode::UnsupportedSource,
            "Playlists",
        ),
        (
            "concat list",
            concat_list,
            FailureCode::UnsupportedSource,
            "Playlists",
        ),
        ("image", image, FailureCode::UnsupportedSource, "Playlists"),
        (
            "truncated mkv",
            truncated_mkv,
            FailureCode::InvalidSource,
            "decode",
        ),
        (
            "truncated mp4",
            truncated_mp4,
            FailureCode::InvalidSource,
            "ffprobe",
        ),
        ("damaged", damaged, FailureCode::InvalidSource, "decode"),
        (
            "not media",
            not_media,
            FailureCode::InvalidSource,
            "ffprobe",
        ),
        ("empty", empty, FailureCode::InvalidSource, "empty"),
    ];
    for (name, maker, expected, words) in cases {
        let dir = tempfile::tempdir()?;
        let src = maker(dir.path())?;
        let before = leftovers(dir.path(), &[]);
        let dst = dir.path().join("out.ivf");
        let mut job = spec(&src, &dst, OutputKind::Ivf);
        job.omissions_acknowledged = true;
        let (_, end) = run_job(job, |_, _| {});
        let (code, message) = failure(&end).ok_or(format!("{name}: {end:?}"))?;
        assert_eq!(code, expected, "{name}: {message}");
        assert!(message.contains(words), "{name}: {message}");
        assert_eq!(leftovers(dir.path(), &[]), before, "{name}");
    }
    Ok(())
}

fn not_media(dir: &Path) -> Fallible<PathBuf> {
    let src = dir.join("notes.mp4");
    fs::write(&src, "these are not the frames you are looking for")?;
    Ok(src)
}

fn empty(dir: &Path) -> Fallible<PathBuf> {
    let src = dir.join("empty.mkv");
    fs::write(&src, "")?;
    Ok(src)
}

/// File names with spaces, non-ASCII letters, a leading dash, a colon, and
/// (on Unix) bytes that are not UTF-8 reach `ffmpeg` intact.
#[test]
fn odd_file_names_reach_the_tools_intact() -> TestResult {
    let dir = tempfile::tempdir()?;
    let nested = dir.path().join("zażółć gęślą jaźń");
    fs::create_dir(&nested)?;
    let made = nested.join("made.mkv");
    make(
        &Clip {
            rate: "25:1",
            ..Clip::new(16, 8, "420", 2)
        },
        &nested,
        &["-c:v", "ffv1"],
        &made,
    )?;
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut names: Vec<PathBuf> = vec![
        nested.join("clip 01 – ünïcödé.mkv"),
        nested.join("-i x.mkv"),
    ];
    // Windows names cannot hold a colon or non-Unicode units here.
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        names.push(nested.join("-i: x.mkv"));
        names.push(nested.join(std::ffi::OsStr::from_bytes(b"clip-\xff\xfe.mkv")));
    }
    for name in names {
        fs::copy(&made, &name)?;
        let facts = source::probe(&name, None).map_err(|e| format!("{name:?}: {e}"))?;
        assert_eq!(facts.width, 16);
        encode_ok(&name, 0, OutputKind::Ivf, 2)?;
    }
    Ok(())
}

/// Cancelling while decoding stops at the next frame, leaves no temporary
/// file, and (checked on Linux) no `ffmpeg` or `ffprobe` child.
#[test]
fn cancel_during_decode_leaves_no_child_and_no_file() -> TestResult {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("in.mkv");
    let clip = Clip {
        rate: "25:1",
        ..Clip::new(64, 48, "420", 24)
    };
    make(&clip, dir.path(), &["-c:v", "ffv1"], &src)?;
    let dst = dir.path().join("out.ivf");
    let (events, end) = run_job(spec(&src, &dst, OutputKind::Ivf), |event, cancel| {
        if matches!(event, Event::Progress { progress } if progress.frames_read == 3) {
            cancel.store(true, Ordering::Relaxed);
        }
    });
    assert!(matches!(end, Event::Cancelled), "{end:?}");
    let read = events.iter().filter_map(|e| match e {
        Event::Progress { progress } => Some(progress.frames_read),
        _ => None,
    });
    assert_eq!(read.max(), Some(3));
    assert_eq!(leftovers(dir.path(), &[&src]), Vec::<PathBuf>::new());
    #[cfg(target_os = "linux")]
    assert_eq!(
        common::tool_children(std::process::id(), &src.to_string_lossy()),
        Vec::<u32>::new()
    );
    Ok(())
}
