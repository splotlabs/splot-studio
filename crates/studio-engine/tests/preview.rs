// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Preview process tests: the real engine binary in `preview` mode, its
//! pictures compared with independently obtained frames.
//!
//! Source pictures are compared with the clip's own samples (Y4M, lossless
//! media) or with the frames the job's reader hands to the encoder (lossy
//! media). Result pictures are compared with a full sequential Splot
//! decode of the same file, so random access must land on the same frame.
//! Every comparison renders the reference through the same display
//! transform, which has its own tests against published values.

mod common;

use std::error::Error;
use std::fs;
use std::io::Write;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command as Process, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use common::{Clip, TestResult, make};
use splot_encode::{
    BitDepth, Config, Encoder, FrameType, PixelFormat, Preset, RateControlMode, SourceFrame,
};
use studio_domain::job::WirePath;
use studio_domain::preview::{
    ColorMatrix, PictureBox, PictureFailure, PictureFrame, PictureHeader, PictureOrigin,
    PictureTarget, PictureView, SourceRef,
};
use studio_domain::protocol::{
    BuildIdentity, Command, Envelope, ErrorCode, Event, read_event, write_message,
};
use studio_domain::settings::OutputKind;
use studio_engine::output::PacketSink;
use studio_engine::picture::{self, Frame};
use studio_engine::source::{self, Selection};

type Fallible<T> = Result<T, Box<dyn Error>>;

const BIN: &str = env!("CARGO_BIN_EXE_splot-studio-engine");

const BIG: PictureBox = PictureBox {
    width: 1280,
    height: 720,
};
const NATIVE: PictureView = PictureView::Native { x: 0, y: 0 };

/// A preview process driven step by step.
struct Preview {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
}

/// One answer: the event and, for a picture, its pixels.
type Answer = (Envelope<Event>, Option<Vec<u8>>);

impl Preview {
    fn spawn() -> Fallible<Self> {
        Self::spawn_with(true)
    }

    fn spawn_with(system_ffmpeg: bool) -> Fallible<Self> {
        let mut command = Process::new(BIN);
        command
            .arg("preview")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if !system_ffmpeg {
            command.env_remove(studio_engine::media::SYSTEM_OPT_IN);
        }
        let mut child = command.spawn()?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or("no stdout")?;
        Ok(Self {
            child,
            stdin,
            stdout,
        })
    }

    fn send(&mut self, request_id: u64, command: Command) -> TestResult {
        let mut bytes = Vec::new();
        write_message(&mut bytes, &Envelope::new(request_id, command))?;
        let pipe = self.stdin.as_mut().ok_or("stdin closed")?;
        pipe.write_all(&bytes)?;
        pipe.flush()?;
        Ok(())
    }

    fn next(&mut self) -> Fallible<Answer> {
        Ok(read_event(&mut self.stdout)?.ok_or("the preview closed its output")?)
    }

    /// Asks for one picture and returns its answer, skipping nothing else:
    /// with one request outstanding, the next event must answer it.
    fn ask(
        &mut self,
        generation: u64,
        target: &PictureTarget,
        frame: PictureFrame,
        view: PictureView,
        size: PictureBox,
    ) -> Fallible<Result<(PictureHeader, Vec<u8>), PictureFailure>> {
        self.send(
            generation,
            Command::Picture {
                generation,
                target: target.clone(),
                frame,
                view,
                size,
            },
        )?;
        let (envelope, pixels) = self.next()?;
        assert_eq!(envelope.request_id, generation);
        match envelope.body {
            Event::Picture { picture } => {
                assert_eq!(picture.generation, generation);
                Ok(Ok((picture, pixels.ok_or("a picture without pixels")?)))
            }
            Event::PictureUnavailable {
                generation: g,
                code,
                ..
            } => {
                assert_eq!(g, generation);
                Ok(Err(code))
            }
            other => Err(format!("unexpected answer {other:?}").into()),
        }
    }

    fn frame(
        &mut self,
        generation: u64,
        target: &PictureTarget,
        index: u64,
    ) -> Fallible<(PictureHeader, Vec<u8>)> {
        self.ask(
            generation,
            target,
            PictureFrame::Index { index },
            NATIVE,
            BIG,
        )?
        .map_err(|code| format!("frame {index}: {code:?}").into())
    }

    fn failure(
        &mut self,
        generation: u64,
        target: &PictureTarget,
        index: u64,
    ) -> Fallible<PictureFailure> {
        match self.ask(
            generation,
            target,
            PictureFrame::Index { index },
            NATIVE,
            BIG,
        )? {
            Err(code) => Ok(code),
            Ok((header, _)) => Err(format!("frame {index} was shown: {header:?}").into()),
        }
    }

    /// Closes the input and waits for a clean exit.
    fn finish(mut self) -> TestResult {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self.child.try_wait()? {
                assert!(status.success(), "{status}");
                return Ok(());
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                return Err("the preview did not exit at the end of its input".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn source_ref(path: &Path, stream: u32) -> SourceRef {
    SourceRef {
        path: WirePath::from_path(path),
        fingerprint: common::fingerprint(path),
        video_stream: stream,
    }
}

fn source_target(path: &Path) -> PictureTarget {
    PictureTarget::Source {
        source: source_ref(path, 0),
    }
}

fn result_target(path: &Path, source: Option<&Path>) -> PictureTarget {
    PictureTarget::Result {
        path: WirePath::from_path(path),
        source: source.map(|s| source_ref(s, 0)),
    }
}

/// The planes as `[Y, Cb, Cr]`, chroma empty for monochrome.
fn three(planes: Vec<Vec<u16>>) -> [Vec<u16>; 3] {
    let mut planes = planes.into_iter();
    [
        planes.next().unwrap_or_default(),
        planes.next().unwrap_or_default(),
        planes.next().unwrap_or_default(),
    ]
}

fn sizes(clip: &Clip) -> [(usize, usize); 3] {
    let planes = clip.planes();
    [
        planes[0],
        planes.get(1).copied().unwrap_or((0, 0)),
        planes.get(2).copied().unwrap_or((0, 0)),
    ]
}

/// `planes` through the display transform with the unstated-matrix rule.
fn rendered(
    planes: &[Vec<u16>; 3],
    sizes: [(usize, usize); 3],
    depth: u8,
    view: PictureView,
    size: PictureBox,
) -> Fallible<Vec<u8>> {
    let frame = Frame::packed(planes, sizes, depth).ok_or("bad planes")?;
    let (w, h) = frame.size();
    let basis = picture::basis(
        None,
        frame.chroma.is_none(),
        u32::try_from(w)?,
        u32::try_from(h)?,
    );
    Ok(picture::render(&frame, basis, view, size).pixels)
}

fn y4m(dir: &Path, name: &str, clip: &Clip) -> Fallible<PathBuf> {
    let path = dir.join(name);
    clip.write(&path)?;
    Ok(path)
}

/// Every source frame at full size, in a scrambled order with jumps back
/// and repeats, equals the clip's own samples, at both depths; the poster
/// is one second in or the last frame.
#[test]
fn y4m_source_frames_are_exact() -> TestResult {
    let dir = tempfile::tempdir()?;
    let mut preview = Preview::spawn()?;
    let mut generation = 0;
    for colorspace in ["420", "420p10", "444p10", "mono"] {
        let clip = Clip::new(64, 48, colorspace, 12);
        let path = y4m(dir.path(), &format!("{colorspace}.y4m"), &clip)?;
        let target = source_target(&path);
        for index in [5_u64, 0, 11, 6, 6, 1, 10] {
            generation += 1;
            let (header, pixels) = preview.frame(generation, &target, index)?;
            let want = rendered(
                &three(clip.samples(usize::try_from(index)?)),
                sizes(&clip),
                u8::try_from(clip.depth())?,
                NATIVE,
                BIG,
            )?;
            assert!(pixels == want, "{colorspace} frame {index}");
            assert_eq!((header.frame, header.frame_count), (index, Some(12)));
            assert_eq!(header.origin, PictureOrigin::Source);
            assert_eq!(
                (
                    header.frame_width,
                    header.frame_height,
                    header.width,
                    header.height
                ),
                (64, 48, 64, 48)
            );
            let time = header.time.ok_or("no time")?;
            assert_eq!((time.num, time.den), (index * 1001, 30000));
            let matrix = if colorspace == "mono" {
                ColorMatrix::Monochrome
            } else {
                ColorMatrix::Bt601
            };
            assert_eq!(
                (header.basis.matrix, header.basis.assumed),
                (matrix, colorspace != "mono")
            );
        }
        generation += 1;
        let poster = preview.ask(
            generation,
            &target,
            PictureFrame::Poster,
            PictureView::Fit,
            BIG,
        )?;
        assert!(
            matches!(poster, Ok((ref h, _)) if h.frame == 11),
            "{poster:?}"
        );
    }
    // A long clip's poster is one second in.
    let clip = Clip::new(16, 16, "420", 40);
    let path = y4m(dir.path(), "long.y4m", &clip)?;
    let poster = preview.ask(
        100,
        &source_target(&path),
        PictureFrame::Poster,
        PictureView::Fit,
        BIG,
    )?;
    assert!(
        matches!(poster, Ok((ref h, _)) if h.frame == 30),
        "{poster:?}"
    );
    preview.finish()
}

/// Scaling and cropping: a Fit picture is the frame scaled into the box,
/// a Native one the region at the clamped origin.
#[test]
fn views_follow_the_box() -> TestResult {
    let dir = tempfile::tempdir()?;
    let clip = Clip::new(64, 48, "420p10", 2);
    let path = y4m(dir.path(), "in.y4m", &clip)?;
    let target = source_target(&path);
    let mut preview = Preview::spawn()?;
    let small = PictureBox {
        width: 20,
        height: 20,
    };
    let (header, pixels) = preview
        .ask(
            1,
            &target,
            PictureFrame::Index { index: 1 },
            PictureView::Fit,
            small,
        )?
        .map_err(|c| format!("{c:?}"))?;
    assert_eq!(
        (header.width, header.height, header.view),
        (20, 15, PictureView::Fit)
    );
    assert_eq!(
        pixels,
        rendered(
            &three(clip.samples(1)),
            sizes(&clip),
            10,
            PictureView::Fit,
            small
        )?
    );
    let (header, pixels) = preview
        .ask(
            2,
            &target,
            PictureFrame::Index { index: 1 },
            PictureView::Native { x: 60, y: 7 },
            small,
        )?
        .map_err(|c| format!("{c:?}"))?;
    assert_eq!((header.width, header.height), (20, 20));
    assert_eq!(header.view, PictureView::Native { x: 44, y: 7 });
    let native = PictureView::Native { x: 44, y: 7 };
    assert_eq!(
        pixels,
        rendered(&three(clip.samples(1)), sizes(&clip), 10, native, small)?
    );
    preview.finish()
}

/// Every frame the job's own reader hands to the encoder.
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

/// The `FFmpeg` route: lossless 8-bit and 10-bit Matroska equal the clip's
/// samples, and an H.264 MP4 with B-frames equals the frames the encoder
/// gets, for any frame in any order. The poster without a known count is
/// found by reading up to one second.
#[test]
fn media_source_frames_are_exact() -> TestResult {
    let dir = tempfile::tempdir()?;
    let mut preview = Preview::spawn()?;
    let mut generation = 0;
    let cases: [(&str, &'static str, &[&str]); 3] = [
        ("ffv1-8.mkv", "420", &["-c:v", "ffv1"]),
        ("ffv1-10.mkv", "420p10", &["-c:v", "ffv1"]),
        (
            "x264.mp4",
            "420",
            &[
                "-c:v", "libx264", "-bf", "3", "-g", "8", "-pix_fmt", "yuv420p",
            ],
        ),
    ];
    for (name, colorspace, args) in cases {
        let clip = Clip::new(64, 48, colorspace, 20);
        let path = dir.path().join(name);
        make(&clip, dir.path(), args, &path)?;
        let frames = encoder_frames(&path)?;
        assert_eq!(frames.len(), 20);
        if Path::new(name).extension().is_some_and(|e| e == "mkv") {
            for (index, frame) in frames.iter().enumerate() {
                assert!(
                    *frame == three(clip.samples(index)),
                    "{name}: lossless frame {index}"
                );
            }
        }
        let target = source_target(&path);
        // Forward steps reuse the decode, jumps restart it.
        for index in [7_u64, 8, 9, 15, 2, 19, 0, 3] {
            generation += 1;
            let (header, pixels) = preview.frame(generation, &target, index)?;
            let want = rendered(
                &frames[usize::try_from(index)?],
                sizes(&clip),
                u8::try_from(clip.depth())?,
                NATIVE,
                BIG,
            )?;
            assert!(pixels == want, "{name} frame {index}");
            assert_eq!(header.frame, index);
        }
        generation += 1;
        let poster = preview.ask(
            generation,
            &target,
            PictureFrame::Poster,
            PictureView::Fit,
            BIG,
        )?;
        let Ok((header, pixels)) = poster else {
            return Err(format!("{name} poster: {poster:?}").into());
        };
        assert_eq!(
            header.frame, 19,
            "{name}: a 20-frame clip's poster is its last frame"
        );
        assert_eq!(
            pixels,
            rendered(
                &frames[19],
                sizes(&clip),
                u8::try_from(clip.depth())?,
                PictureView::Fit,
                BIG
            )?
        );
        generation += 1;
        assert_eq!(
            preview.failure(generation, &target, 20)?,
            PictureFailure::OutOfRange
        );
        generation += 1;
        assert_eq!(
            preview.failure(generation, &target, 500)?,
            PictureFailure::OutOfRange
        );
    }
    preview.finish()
}

/// Encodes `clip` with key frames every `keyint` frames into `path`, and
/// returns the display indices of its key frames. Each packet the encoder
/// calls a key frame is a random access point to the engine, and no other.
fn encode_with_keyint(
    clip: &Clip,
    kind: OutputKind,
    keyint: u32,
    path: &Path,
) -> Fallible<Vec<u64>> {
    let mut config = Config::new(u32::try_from(clip.width)?, u32::try_from(clip.height)?);
    config.bit_depth = if clip.depth() > 8 {
        BitDepth::Ten
    } else {
        BitDepth::Eight
    };
    config.chroma = PixelFormat::Yuv420;
    config.frame_rate = (30000, 1001);
    config.rate_control = RateControlMode::Crf { crf: 40 };
    config.preset = Preset::new(10)?;
    config.keyint = NonZeroU32::new(keyint).ok_or("keyint")?;
    let handle = studio_engine::job::spawn({
        let clip = clip.clone();
        let path = path.to_path_buf();
        move || -> Result<Vec<u64>, String> {
            let mut keys = Vec::new();
            let file = fs::File::create(&path).map_err(|e| e.to_string())?;
            let mut sink = PacketSink::new(kind, std::io::BufWriter::new(file), &config)
                .map_err(|e| e.to_string())?;
            let mut encoder = Encoder::new(config.clone()).map_err(|e| e.to_string())?;
            let mut write = |encoder: &mut Encoder| -> Result<(), String> {
                while let Some(packet) = encoder.receive_packet().map_err(|e| e.to_string())? {
                    let key = packet.frame_type == FrameType::Key;
                    if key != studio_engine::stream::is_random_access(&packet.data) {
                        return Err(format!("packet {} misjudged", packet.pts));
                    }
                    if key {
                        keys.push(packet.pts);
                    }
                    sink.write(&packet).map_err(|e| e.to_string())?;
                }
                Ok(())
            };
            for index in 0..clip.frames {
                let frame = SourceFrame::new(&config, index as u64, three(clip.samples(index)))
                    .map_err(|e| e.to_string())?;
                encoder.send_frame(frame).map_err(|e| e.to_string())?;
                write(&mut encoder)?;
            }
            encoder.flush().map_err(|e| e.to_string())?;
            write(&mut encoder)?;
            drop(write);
            sink.finish().map_err(|e| e.to_string())?;
            Ok(keys)
        }
    })?;
    Ok(handle.join().map_err(|_| "the encode panicked")??)
}

/// Splot's full sequential decode of `path`, frame by frame.
fn decoded_frames(path: &Path, clip: &Clip) -> Fallible<Vec<[Vec<u16>; 3]>> {
    let raw = common::decode(path)?;
    let wide = clip.depth() > 8;
    let mut samples = raw.chunks(if wide { 2 } else { 1 }).map(|c| {
        if wide {
            u16::from_le_bytes([c[0], c[1]])
        } else {
            u16::from(c[0])
        }
    });
    let per_frame: usize = clip.planes().iter().map(|(w, h)| w * h).sum();
    let count = raw.len() / per_frame / if wide { 2 } else { 1 };
    Ok((0..count)
        .map(|_| {
            three(
                clip.planes()
                    .iter()
                    .map(|&(w, h)| samples.by_ref().take(w * h).collect())
                    .collect(),
            )
        })
        .collect())
}

/// Result frames, decoded from the nearest key frame in any order, equal
/// the full decode at both depths in both formats. Timing comes from IVF,
/// or for `.obu` from the source; without it, a raw OBU has none.
#[test]
fn result_frames_are_exact_with_random_access() -> TestResult {
    let dir = tempfile::tempdir()?;
    let mut preview = Preview::spawn()?;
    let mut generation = 0;
    for (colorspace, kind) in [
        ("420", OutputKind::Ivf),
        ("420p10", OutputKind::Av2AnnexB),
        ("420p10", OutputKind::Ivf),
        ("420", OutputKind::Av2AnnexB),
    ] {
        let clip = Clip::new(48, 32, colorspace, 14);
        let source = y4m(
            dir.path(),
            &format!("{colorspace}-{}.y4m", kind.extension()),
            &clip,
        )?;
        let out = dir
            .path()
            .join(format!("{colorspace}.{}", kind.extension()));
        let keys = encode_with_keyint(&clip, kind, 4, &out)?;
        // Every fourth frame, and perhaps scene cuts.
        assert!([0, 4, 8, 12].iter().all(|k| keys.contains(k)), "{keys:?}");
        let reference = decoded_frames(&out, &clip)?;
        assert_eq!(reference.len(), 14);
        let depth = u8::try_from(clip.depth())?;
        for target in [
            result_target(&out, None),
            result_target(&out, Some(&source)),
        ] {
            for index in [9_u64, 10, 2, 3, 13, 0, 7, 4, 12, 1] {
                generation += 1;
                let (header, pixels) = preview.frame(generation, &target, index)?;
                let want = rendered(
                    &reference[usize::try_from(index)?],
                    sizes(&clip),
                    depth,
                    NATIVE,
                    BIG,
                )?;
                assert!(pixels == want, "{colorspace} {kind:?} frame {index}");
                assert_eq!(header.origin, PictureOrigin::Result);
                assert_eq!((header.frame, header.frame_count), (index, Some(14)));
                let has_source = matches!(
                    &target,
                    PictureTarget::Result {
                        source: Some(_),
                        ..
                    }
                );
                let time = header.time.map(|t| (t.num, t.den));
                let timed = kind == OutputKind::Ivf || has_source;
                assert_eq!(time, timed.then_some((index * 1001, 30000)));
            }
            generation += 1;
            assert_eq!(
                preview.failure(generation, &target, 14)?,
                PictureFailure::OutOfRange
            );
        }
    }
    preview.finish()
}

/// A real job's output (one key frame) is shown, first to last frame.
#[test]
fn a_job_output_is_shown() -> TestResult {
    let dir = tempfile::tempdir()?;
    let clip = Clip::new(64, 48, "420", 6);
    let src = y4m(dir.path(), "in.y4m", &clip)?;
    let out = dir.path().join("out.ivf");
    let (_, end) = common::run_job(common::spec(&src, &out, OutputKind::Ivf), |_, _| {});
    assert!(matches!(end, Event::Completed { .. }), "{end:?}");
    let reference = decoded_frames(&out, &clip)?;
    let target = result_target(&out, Some(&src));
    let mut preview = Preview::spawn()?;
    for index in [5_u64, 0, 3] {
        let (header, pixels) = preview.frame(index + 1, &target, index)?;
        assert_eq!(
            pixels,
            rendered(
                &reference[usize::try_from(index)?],
                sizes(&clip),
                8,
                NATIVE,
                BIG
            )?
        );
        // The source states no matrix: the same assumption as its pictures.
        assert_eq!(
            (header.basis.matrix, header.basis.assumed),
            (ColorMatrix::Bt601, true)
        );
    }
    preview.finish()
}

/// A truncated file shows the frames before the damage and reports the
/// rest unreadable; garbage in a frame is unreadable; a file that is not
/// AV2 at all is unreadable.
#[test]
fn damaged_results_are_reported() -> TestResult {
    let dir = tempfile::tempdir()?;
    let clip = Clip::new(48, 32, "420", 12);
    let mut preview = Preview::spawn()?;
    let mut generation = 0;
    for kind in [OutputKind::Ivf, OutputKind::Av2AnnexB] {
        let out = dir.path().join(format!("whole.{}", kind.extension()));
        encode_with_keyint(&clip, kind, 4, &out)?;
        let reference = decoded_frames(&out, &clip)?;
        let bytes = fs::read(&out)?;
        let cut = dir.path().join(format!("cut.{}", kind.extension()));
        fs::write(&cut, &bytes[..bytes.len() - 7])?;
        let target = result_target(&cut, None);
        generation += 1;
        let (header, pixels) = preview.frame(generation, &target, 2)?;
        assert_eq!(
            pixels,
            rendered(&reference[2], sizes(&clip), 8, NATIVE, BIG)?
        );
        assert_eq!(
            header.frame_count, None,
            "{kind:?}: a damaged file's count is unknown"
        );
        generation += 1;
        assert_eq!(
            preview.failure(generation, &target, 11)?,
            PictureFailure::Unreadable,
            "{kind:?}"
        );
        // Damage inside a frame's payload.
        let mut broken = bytes.clone();
        let middle = bytes.len() * 2 / 3;
        for byte in &mut broken[middle..middle + 40] {
            *byte ^= 0x5a;
        }
        let damaged = dir.path().join(format!("damaged.{}", kind.extension()));
        fs::write(&damaged, &broken)?;
        let target = result_target(&damaged, None);
        let mut unreadable = 0;
        for index in 0..12 {
            generation += 1;
            match preview.ask(
                generation,
                &target,
                PictureFrame::Index { index },
                NATIVE,
                BIG,
            )? {
                Ok(_) => {}
                Err(code) => {
                    assert_eq!(code, PictureFailure::Unreadable, "{kind:?} frame {index}");
                    unreadable += 1;
                }
            }
        }
        assert!(unreadable > 0, "{kind:?}: damage went unnoticed");
    }
    let junk = dir.path().join("junk.ivf");
    fs::write(&junk, b"DKIF this is not an IVF file at all")?;
    assert_eq!(
        preview.failure(1000, &result_target(&junk, None), 0)?,
        PictureFailure::Unreadable
    );
    let empty = dir.path().join("empty.obu");
    fs::write(&empty, b"")?;
    assert_eq!(
        preview.failure(1001, &result_target(&empty, None), 0)?,
        PictureFailure::Unreadable
    );
    let missing = dir.path().join("missing.ivf");
    assert_eq!(
        preview.failure(1002, &result_target(&missing, None), 0)?,
        PictureFailure::Unreadable
    );
    preview.finish()
}

/// Out of range, a changed source (before and after it was opened), an
/// unsupported source, an invalid box.
#[test]
fn refusals_name_their_reason() -> TestResult {
    let dir = tempfile::tempdir()?;
    let clip = Clip::new(32, 32, "420", 4);
    let path = y4m(dir.path(), "in.y4m", &clip)?;
    let target = source_target(&path);
    let mut preview = Preview::spawn()?;
    assert_eq!(preview.failure(1, &target, 4)?, PictureFailure::OutOfRange);
    preview.frame(2, &target, 3)?;
    // Changed after it was opened: the open file is refused and closed.
    let mut changed = clip.clone();
    changed.frames = 5;
    changed.write(&path)?;
    assert_eq!(
        preview.failure(3, &target, 0)?,
        PictureFailure::SourceChanged
    );
    assert_eq!(
        preview.failure(4, &target, 0)?,
        PictureFailure::SourceChanged
    );
    // The new fingerprint is accepted.
    preview.frame(5, &source_target(&path), 4)?;
    let mut interlaced = Clip::new(32, 32, "420", 2);
    interlaced.tags = "It A1:1";
    let path = y4m(dir.path(), "interlaced.y4m", &interlaced)?;
    assert_eq!(
        preview.failure(6, &source_target(&path), 0)?,
        PictureFailure::Unsupported
    );
    let not_media = dir.path().join("notes.txt");
    fs::write(&not_media, b"hello")?;
    assert_eq!(
        preview.failure(7, &source_target(&not_media), 0)?,
        PictureFailure::Unsupported
    );
    for (generation, size) in [
        (
            8,
            PictureBox {
                width: 0,
                height: 10,
            },
        ),
        (
            9,
            PictureBox {
                width: 1281,
                height: 10,
            },
        ),
        (
            10,
            PictureBox {
                width: 10,
                height: 721,
            },
        ),
    ] {
        let got = preview.ask(
            generation,
            &target,
            PictureFrame::Poster,
            PictureView::Fit,
            size,
        )?;
        assert_eq!(got.err(), Some(PictureFailure::InvalidRequest));
    }
    preview.finish()
}

/// Without `FFmpeg`, media sources say so; Y4M still works.
#[test]
fn without_ffmpeg_media_needs_the_tool() -> TestResult {
    let dir = tempfile::tempdir()?;
    let clip = Clip::new(32, 32, "420", 3);
    let mp4 = dir.path().join("in.mp4");
    make(
        &clip,
        dir.path(),
        &["-c:v", "libx264", "-pix_fmt", "yuv420p"],
        &mp4,
    )?;
    let y4m = y4m(dir.path(), "in.y4m", &clip)?;
    let mut preview = Preview::spawn_with(false)?;
    assert_eq!(
        preview.failure(1, &source_target(&mp4), 0)?,
        PictureFailure::MediaToolMissing
    );
    preview.frame(2, &source_target(&y4m), 1)?;
    preview.finish()
}

/// Job commands are refused; Hello is answered.
#[test]
fn the_preview_runs_no_jobs() -> TestResult {
    let dir = tempfile::tempdir()?;
    let mut preview = Preview::spawn()?;
    let client = BuildIdentity {
        name: "test".into(),
        version: "0".into(),
    };
    preview.send(1, Command::Hello { client })?;
    assert!(matches!(preview.next()?.0.body, Event::Hello { .. }));
    let job = common::spec(
        &dir.path().join("a.y4m"),
        &dir.path().join("b.ivf"),
        OutputKind::Ivf,
    );
    for (id, command) in [
        (2, Command::Start { job, live: None }),
        (3, Command::Cancel),
        (
            4,
            Command::Probe {
                source: WirePath::from_path(dir.path()),
                video_stream: None,
            },
        ),
    ] {
        preview.send(id, command)?;
        let (envelope, _) = preview.next()?;
        assert_eq!(envelope.request_id, id);
        assert!(
            matches!(
                envelope.body,
                Event::Error {
                    code: ErrorCode::WrongState,
                    ..
                }
            ),
            "{:?}",
            envelope.body
        );
    }
    // StopPlayback with nothing playing is not answered: Hello comes next.
    preview.send(5, Command::StopPlayback)?;
    let client = BuildIdentity {
        name: "test".into(),
        version: "0".into(),
    };
    preview.send(6, Command::Hello { client })?;
    assert_eq!(preview.next()?.0.request_id, 6);
    preview.finish()
}

/// Requests sent faster than they are served: older ones may go
/// unanswered, but every answer is for a requested generation, answers
/// never go back to an older generation, and the newest is answered.
#[test]
fn stale_requests_are_dropped_and_the_newest_wins() -> TestResult {
    let dir = tempfile::tempdir()?;
    let clip = Clip::new(64, 48, "420", 30);
    let mp4 = dir.path().join("in.mkv");
    make(&clip, dir.path(), &["-c:v", "ffv1"], &mp4)?;
    let target = source_target(&mp4);
    let mut preview = Preview::spawn()?;
    for generation in 1..=40_u64 {
        preview.send(
            generation,
            Command::Picture {
                generation,
                target: target.clone(),
                frame: PictureFrame::Index {
                    index: (generation * 7) % 30,
                },
                view: PictureView::Fit,
                size: BIG,
            },
        )?;
    }
    let mut seen = Vec::new();
    loop {
        let (envelope, pixels) = preview.next()?;
        let Event::Picture { picture } = envelope.body else {
            return Err(format!("unexpected {:?}", envelope.body).into());
        };
        assert_eq!(envelope.request_id, picture.generation);
        assert_eq!(picture.frame, (picture.generation * 7) % 30);
        let want = rendered(
            &three(clip.samples(usize::try_from(picture.frame)?)),
            sizes(&clip),
            8,
            PictureView::Fit,
            BIG,
        )?;
        assert_eq!(pixels.as_deref(), Some(want.as_slice()));
        seen.push(picture.generation);
        if picture.generation == 40 {
            break;
        }
    }
    assert!(seen.windows(2).all(|w| w[0] < w[1]), "{seen:?}");
    assert!(seen.len() < 40, "nothing was dropped: {seen:?}");
    preview.finish()
}

/// Many targets opened in turn, more than stay open: each answers, and the
/// closed ones leave no `ffmpeg` behind.
#[test]
fn targets_open_and_close_repeatedly() -> TestResult {
    let dir = tempfile::tempdir()?;
    let mut targets = Vec::new();
    for i in 0..6 {
        let clip = Clip::new(32, 32, "420", 40);
        let path = dir.path().join(format!("in-{i}.mkv"));
        make(&clip, dir.path(), &["-c:v", "ffv1"], &path)?;
        targets.push((source_target(&path), clip));
    }
    let mut preview = Preview::spawn()?;
    let mut generation = 0;
    for round in 0..3 {
        for (target, clip) in &targets {
            generation += 1;
            let index = 5 + round;
            let (_, pixels) = preview.frame(generation, target, index)?;
            assert_eq!(
                pixels,
                rendered(
                    &three(clip.samples(usize::try_from(index)?)),
                    sizes(clip),
                    8,
                    NATIVE,
                    BIG
                )?
            );
        }
        #[cfg(target_os = "linux")]
        {
            let children = common::tool_children(preview.child.id(), "in-");
            assert!(
                children.len() <= studio_engine::preview::OPEN_TARGETS,
                "{} ffmpeg children for {} open targets",
                children.len(),
                studio_engine::preview::OPEN_TARGETS
            );
        }
    }
    preview.finish()
}

fn play(generation: u64, target: &PictureTarget, from: u64) -> Command {
    Command::Play {
        generation,
        target: target.clone(),
        from,
        view: PictureView::Fit,
        size: BIG,
    }
}

/// Reads playback until it ends; returns the frames shown and the end.
fn playback(preview: &mut Preview, generation: u64) -> Fallible<(Vec<u64>, Event)> {
    let mut frames = Vec::new();
    loop {
        let (envelope, _) = preview.next()?;
        match envelope.body {
            Event::Picture { picture } if picture.generation == generation => {
                frames.push(picture.frame);
            }
            Event::Picture { .. } => {}
            end @ (Event::PlaybackEnded { .. } | Event::PictureUnavailable { .. }) => {
                return Ok((frames, end));
            }
            other => return Err(format!("unexpected {other:?}").into()),
        }
    }
}

#[allow(clippy::too_many_lines)]
/// Playback runs at the stream's timing to the end, in order; a stop or a
/// newer request ends it promptly; a raw OBU without its source cannot be
/// played.
#[test]
fn playback_starts_stops_and_ends() -> TestResult {
    let dir = tempfile::tempdir()?;
    let mut clip = Clip::new(32, 32, "420", 15);
    clip.rate = "30:1";
    let path = y4m(dir.path(), "in.y4m", &clip)?;
    let target = source_target(&path);
    let mut preview = Preview::spawn()?;
    let started = Instant::now();
    preview.send(1, play(1, &target, 3))?;
    let (frames, end) = playback(&mut preview, 1)?;
    let took = started.elapsed();
    assert_eq!(
        end,
        Event::PlaybackEnded {
            generation: 1,
            frame: Some(14)
        }
    );
    assert!(
        frames.windows(2).all(|w| w[0] < w[1]) && frames.first() == Some(&3),
        "{frames:?}"
    );
    // Frames 3 to 14 at 30 fps: 11 frame periods after the first.
    assert!(
        took >= Duration::from_millis(360),
        "played too fast: {took:?}"
    );

    // Stop: a longer clip, stopped early.
    let mut long = Clip::new(32, 32, "420", 300);
    long.rate = "30:1";
    let path = y4m(dir.path(), "long.y4m", &long)?;
    let target = source_target(&path);
    preview.send(2, play(2, &target, 0))?;
    let (envelope, _) = preview.next()?;
    assert!(matches!(envelope.body, Event::Picture { ref picture } if picture.frame == 0));
    let asked = Instant::now();
    preview.send(3, Command::StopPlayback)?;
    let (_, end) = playback(&mut preview, 2)?;
    assert!(
        matches!(
            end,
            Event::PlaybackEnded {
                generation: 2,
                frame: Some(_)
            }
        ),
        "{end:?}"
    );
    assert!(
        asked.elapsed() < Duration::from_millis(500),
        "stop took {:?}",
        asked.elapsed()
    );

    // A newer picture ends playback, then is answered.
    preview.send(4, play(4, &target, 10))?;
    preview.next()?;
    preview.send(
        5,
        Command::Picture {
            generation: 5,
            target: target.clone(),
            frame: PictureFrame::Index { index: 1 },
            view: NATIVE,
            size: BIG,
        },
    )?;
    let (_, end) = playback(&mut preview, 4)?;
    assert!(
        matches!(end, Event::PlaybackEnded { generation: 4, .. }),
        "{end:?}"
    );
    let (envelope, _) = preview.next()?;
    assert!(
        matches!(envelope.body, Event::Picture { ref picture } if picture.generation == 5 && picture.frame == 1)
    );

    // Past the end.
    preview.send(6, play(6, &target, 300))?;
    let (_, end) = playback(&mut preview, 6)?;
    assert!(
        matches!(
            end,
            Event::PictureUnavailable {
                code: PictureFailure::OutOfRange,
                ..
            }
        ),
        "{end:?}"
    );

    // A raw OBU: no timing without its source, played with it.
    let small = Clip::new(32, 32, "420", 6);
    let src = y4m(dir.path(), "small.y4m", &small)?;
    let obu = dir.path().join("out.obu");
    encode_with_keyint(&small, OutputKind::Av2AnnexB, 120, &obu)?;
    preview.send(7, play(7, &result_target(&obu, None), 0))?;
    let (_, end) = playback(&mut preview, 7)?;
    assert!(
        matches!(
            end,
            Event::PictureUnavailable {
                code: PictureFailure::NoTiming,
                ..
            }
        ),
        "{end:?}"
    );
    preview.send(8, play(8, &result_target(&obu, Some(&src)), 0))?;
    let (frames, end) = playback(&mut preview, 8)?;
    assert_eq!(
        end,
        Event::PlaybackEnded {
            generation: 8,
            frame: Some(5)
        }
    );
    assert_eq!(frames.first(), Some(&0));
    preview.finish()
}

/// The end of input ends the process promptly, also during playback.
#[test]
fn the_end_of_input_ends_playback() -> TestResult {
    let dir = tempfile::tempdir()?;
    let mut clip = Clip::new(32, 32, "420", 600);
    clip.rate = "30:1";
    let path = y4m(dir.path(), "in.y4m", &clip)?;
    let mut preview = Preview::spawn()?;
    preview.send(1, play(1, &source_target(&path), 0))?;
    preview.next()?;
    let asked = Instant::now();
    preview.finish()?;
    assert!(
        asked.elapsed() < Duration::from_secs(2),
        "{:?}",
        asked.elapsed()
    );
    Ok(())
}

/// Far frames of a media file, over the protocol: a single picture and a
/// playback started there show the encoder's frame at that index.
#[test]
fn far_media_frames_and_playback_from_them() -> TestResult {
    let dir = tempfile::tempdir()?;
    let clip = Clip::new(64, 48, "420", 200);
    let path = dir.path().join("far.mp4");
    make(
        &clip,
        dir.path(),
        &[
            "-c:v", "libx264", "-bf", "3", "-g", "48", "-pix_fmt", "yuv420p",
        ],
        &path,
    )?;
    let frames = encoder_frames(&path)?;
    let target = source_target(&path);
    let mut preview = Preview::spawn()?;
    for (generation, index) in [(1, 170_u64), (2, 199), (3, 90)] {
        let (_, pixels) = preview.frame(generation, &target, index)?;
        let want = rendered(
            &frames[usize::try_from(index)?],
            sizes(&clip),
            8,
            NATIVE,
            BIG,
        )?;
        assert!(pixels == want, "frame {index}");
    }
    preview.send(4, play(4, &target, 150))?;
    let (envelope, pixels) = preview.next()?;
    let Event::Picture { picture } = envelope.body else {
        return Err(format!("{:?}", envelope.body).into());
    };
    assert_eq!(picture.frame, 150);
    let want = rendered(&frames[150], sizes(&clip), 8, PictureView::Fit, BIG)?;
    assert_eq!(pixels, Some(want));
    preview.send(5, Command::StopPlayback)?;
    let (_, end) = playback(&mut preview, 4)?;
    assert!(
        matches!(end, Event::PlaybackEnded { generation: 4, .. }),
        "{end:?}"
    );
    preview.finish()
}
