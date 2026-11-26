// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Display text for domain values. Exact values first; any rounded figure is
//! a secondary hint next to the exact one, never a replacement.

pub mod analysis;
pub mod jobs;
pub mod preview;
pub mod queue;

use std::path::Path;
use std::time::Duration;

use studio_domain::job::{
    Check, CheckKind, CheckOutcome, ChromaLayout, Disclosure, Failure, FailureCode, Omissions,
    Rational, SourceFacts, SourceReader, Stage, VideoTrack,
};
use studio_domain::protocol::{Capabilities, MediaToolOrigin};
use studio_domain::settings::OutputKind;

use crate::activity::{Activity, Phase};
use crate::source::ProbeFailure;

/// How a line of the activity area or a check should read; the words always
/// carry the meaning on their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Plain.
    Neutral,
    /// It worked.
    Success,
    /// Needs attention but is not a failure.
    Warning,
    /// It failed.
    Error,
}

/// A file's name, or the whole path when it has none. Lossy for display
/// only; the path itself stays exact.
#[must_use]
pub fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// `30000/1001 fps (≈ 29.97)`, or `25 fps` when the rate is a whole number.
#[must_use]
pub fn frame_rate(rate: Rational) -> String {
    match rate {
        Rational { den: 0, num } => format!("invalid ({num}/0)"),
        Rational { num, den: 1 } => format!("{num} fps"),
        Rational { num, den } if num % den == 0 => format!("{num}/{den} fps (= {})", num / den),
        Rational { num, den } => {
            format!("{num}/{den} fps (≈ {:.2})", f64::from(num) / f64::from(den))
        }
    }
}

/// `1:1 (square)` or `n:d`.
#[must_use]
pub fn pixel_aspect(aspect: Rational) -> String {
    if aspect.num == aspect.den && aspect.den != 0 {
        "1:1 (square pixels)".to_owned()
    } else {
        format!("{}:{}", aspect.num, aspect.den)
    }
}

/// The usual notation for a chroma layout.
#[must_use]
pub const fn chroma(layout: ChromaLayout) -> &'static str {
    match layout {
        ChromaLayout::Monochrome => "Monochrome (luma only)",
        ChromaLayout::Yuv420 => "4:2:0",
        ChromaLayout::Yuv422 => "4:2:2",
        ChromaLayout::Yuv444 => "4:4:4",
    }
}

/// A codec name as the reader reports it, in its usual written form when it
/// is a common one; otherwise exactly as reported.
#[must_use]
pub fn codec(name: &str) -> String {
    match name {
        "h264" => "H.264",
        "hevc" => "HEVC (H.265)",
        "av1" => "AV1",
        "vp8" => "VP8",
        "vp9" => "VP9",
        "mpeg4" => "MPEG-4 Part 2",
        "mpeg2video" => "MPEG-2",
        "mpeg1video" => "MPEG-1",
        "prores" => "ProRes",
        "ffv1" => "FFV1",
        "mjpeg" => "Motion JPEG",
        "rawvideo" => "Uncompressed",
        other => other,
    }
    .to_owned()
}

/// How the source is read: `H.264, read with FFmpeg`. Only the selected
/// track's codec is known, not the container, so none is named.
#[must_use]
pub fn reader(facts: &SourceFacts) -> String {
    match facts.reader {
        SourceReader::Y4m => "Y4M, read directly".to_owned(),
        SourceReader::Ffmpeg => facts
            .video_tracks
            .iter()
            .find(|t| t.stream == facts.video_stream)
            .map_or_else(
                || "Read with FFmpeg".to_owned(),
                |t| format!("{}, read with FFmpeg", codec(&t.codec)),
            ),
    }
}

/// One entry of the Video track picker: `Stream 1 · H.264 · 1920 × 1080`.
#[must_use]
pub fn track(track: &VideoTrack) -> String {
    format!(
        "Stream {} · {} · {} × {}",
        track.stream,
        codec(&track.codec),
        track.width,
        track.height
    )
}

/// The note under the Video track picker about attached pictures.
#[must_use]
pub fn attached_note(attached: &[VideoTrack]) -> String {
    let streams: Vec<String> = attached.iter().map(|t| t.stream.to_string()).collect();
    match streams.as_slice() {
        [] => String::new(),
        [one] => format!(
            "Stream {one} is an attached picture, not a video track. It cannot be chosen \
             and is not included."
        ),
        _ => format!(
            "Streams {} are attached pictures, not video tracks. They cannot be chosen and \
             are not included.",
            join(&streams)
        ),
    }
}

/// `a`, `a and b`, `a, b and c`.
#[must_use]
pub fn join(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

fn counted(n: u32, one: &str, many: &str) -> Option<String> {
    match n {
        0 => None,
        1 => Some(format!("1 {one}")),
        n => Some(format!("{n} {many}")),
    }
}

/// Everything a video-only output leaves out, in words, zero categories
/// omitted: `2 audio tracks, 3 subtitle tracks and 4 chapters`.
/// `attached_pictures` of `omissions.other_video` are named separately.
#[must_use]
pub fn omitted(omissions: &Omissions, attached_pictures: u32) -> String {
    let pictures = attached_pictures.min(omissions.other_video);
    let items: Vec<String> = [
        counted(
            omissions.other_video - pictures,
            "other video track",
            "other video tracks",
        ),
        counted(pictures, "attached picture", "attached pictures"),
        counted(omissions.audio, "audio track", "audio tracks"),
        counted(omissions.subtitles, "subtitle track", "subtitle tracks"),
        counted(omissions.attachments, "attachment", "attachments"),
        counted(omissions.other, "other stream", "other streams"),
        counted(omissions.chapters, "chapter", "chapters"),
    ]
    .into_iter()
    .flatten()
    .collect();
    join(&items)
}

/// The always-visible line under the file card, or nothing when the output
/// leaves nothing out.
#[must_use]
pub fn omission_summary(omissions: &Omissions, attached_pictures: u32) -> String {
    if omissions.any() {
        format!(
            "Video only — {} will not be included.",
            omitted(omissions, attached_pictures)
        )
    } else {
        String::new()
    }
}

/// The confirmation's text: exactly what is left out, and that the source
/// stays as it is.
#[must_use]
pub fn omission_question(
    omissions: &Omissions,
    attached_pictures: u32,
    output: OutputKind,
) -> String {
    format!(
        "This file also has {}, which will not be included in the {} file. The source file \
         is not modified.",
        omitted(omissions, attached_pictures),
        output.label()
    )
}

/// The `FFmpeg` line in Advanced: which one the engine reads ordinary files
/// with, and whether it is the one shipped with the app.
#[must_use]
pub fn media_tool(capabilities: Option<&Capabilities>) -> String {
    let Some(capabilities) = capabilities else {
        return "FFmpeg: not known yet.".to_owned();
    };
    match &capabilities.media_tool {
        None => "FFmpeg: not in this build. Only Y4M files can be read.".to_owned(),
        Some(tool) => {
            let version = tool.version.trim();
            // `ffmpeg -version` starts with "ffmpeg version"; do not repeat it.
            let version = version.strip_prefix("ffmpeg version ").unwrap_or(version);
            match tool.origin {
                MediaToolOrigin::Bundled => format!("FFmpeg {version}, shipped with the app."),
                MediaToolOrigin::System => format!(
                    "FFmpeg {version} from this computer: development setup — not a shipped \
                     FFmpeg."
                ),
            }
        }
    }
}

/// The empty state's caption. The engine decides what each file is; this
/// only says when it can read nothing but Y4M.
#[must_use]
pub fn empty_caption(capabilities: Option<&Capabilities>) -> &'static str {
    match capabilities {
        Some(Capabilities {
            media_tool: None, ..
        }) => "This build can read Y4M files only.",
        _ => "Splot Studio checks each file before converting it.",
    }
}

/// Why a source cannot be converted, for the file card: the engine's words
/// and, where one honestly exists, what to do.
#[must_use]
pub fn probe_problem(failure: &ProbeFailure) -> String {
    match failure.code {
        Some(FailureCode::MediaToolMissing) => "This build can read Y4M files only. Other video \
             files need FFmpeg, which is not included in this build."
            .to_owned(),
        Some(FailureCode::UnsupportedSource) => format!(
            "{}. {}",
            failure.message.trim_end_matches('.'),
            UNSUPPORTED_REMEDY
        ),
        _ => failure.message.clone(),
    }
}

/// Every `UnsupportedSource` case (variable frame rate, interlaced, HDR,
/// full range, rotation, pixel aspect, pixel format) has the same honest
/// remedy: none in this app yet.
const UNSUPPORTED_REMEDY: &str = "Splot Studio does not support this yet.";

/// Label/value rows for the source sidebar, in reading order.
#[must_use]
pub fn facts(facts: &SourceFacts) -> Vec<(&'static str, String)> {
    let mut rows = vec![
        ("Video", reader(facts)),
        ("Size", format!("{} × {} pixels", facts.width, facts.height)),
        ("Frame rate", frame_rate(facts.frame_rate)),
        ("Bit depth", format!("{}-bit", facts.bit_depth)),
        ("Chroma", chroma(facts.chroma).to_owned()),
        (
            "Frames",
            facts
                .frame_count
                .map_or_else(|| "Unknown".to_owned(), count),
        ),
    ];
    if let Some(aspect) = facts.pixel_aspect {
        rows.push(("Pixel aspect", pixel_aspect(aspect)));
    }
    rows
}

/// The file card's line: `720p • 12.4 MB`, or whatever is known yet.
#[must_use]
pub fn source_line(facts: Option<&SourceFacts>, size: Option<u64>) -> String {
    let height = facts.map(|f| format!("{}p", f.height));
    let size = size.map(bytes);
    [height, size]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("  •  ")
}

/// What a disclosure means for the user, in full.
#[must_use]
pub const fn disclosure(disclosure: Disclosure) -> &'static str {
    match disclosure {
        Disclosure::ColorUnspecified => {
            "Colour is not stated in the file. Samples are encoded unchanged and the output \
             carries no colour description."
        }
        Disclosure::ScanUnspecified => {
            "The file does not say whether it is progressive or interlaced. It is encoded as \
             progressive frames."
        }
        Disclosure::ColorNotCarried => {
            "The file states a colour value the output has no code for. Samples are encoded \
             unchanged and the output leaves that value unspecified."
        }
        Disclosure::StartOffsetDropped => {
            "The video starts later than time zero in the file. The output starts at zero."
        }
        Disclosure::LayoutRepacked => {
            "Samples are moved from a semi-planar layout into separate planes, at the same bit \
             depth and subsampling. No value changes."
        }
    }
}

/// A count with thousands separators: `1,234,567`.
#[must_use]
pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A byte size in decimal units with three significant digits: `512 bytes`,
/// `1.23 MB`, `12.4 MB`, `124 MB`.
#[must_use]
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["kB", "MB", "GB", "TB"];
    if n < 1000 {
        return format!("{n} bytes");
    }
    // Display precision only; the exact count is shown next to it.
    #[allow(clippy::cast_precision_loss)]
    let mut value = n as f64 / 1000.0;
    let mut unit = 0;
    while value >= 999.5 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    let unit = UNITS[unit];
    if value >= 99.95 {
        format!("{value:.0} {unit}")
    } else if value >= 9.995 {
        format!("{value:.1} {unit}")
    } else {
        format!("{value:.2} {unit}")
    }
}

/// A size with its exact byte count: `1.23 MB (1,234,567 bytes)`.
#[must_use]
pub fn bytes_exact(n: u64) -> String {
    if n < 1000 {
        bytes(n)
    } else {
        format!("{} ({} bytes)", bytes(n), count(n))
    }
}

/// `0:07`, `12:34`, `1:02:03`.
#[must_use]
pub fn elapsed(duration: Duration) -> String {
    let s = duration.as_secs();
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// The name of a stage as the activity area shows it. A new engine stage
/// needs exactly one arm here.
#[must_use]
pub const fn stage(stage: Stage) -> &'static str {
    match stage {
        Stage::FirstPass => "Analyzing frames",
        Stage::Checking => "Checking the frames",
        Stage::Encoding => "Encoding",
        Stage::Finalizing => "Finishing the file",
        Stage::Verifying => "Verifying",
        Stage::Publishing => "Publishing",
    }
}

/// The activity area's headline for a job, announced when it changes.
#[must_use]
pub fn headline(activity: &Activity) -> (String, Tone) {
    let name = file_name(activity.destination());
    match activity.phase() {
        Phase::Starting if activity.is_first_pass() => {
            (format!("Starting first pass — {name}"), Tone::Neutral)
        }
        Phase::Starting => (format!("Starting — {name}"), Tone::Neutral),
        Phase::Running(s) => (format!("{} — {name}", stage(*s)), Tone::Neutral),
        Phase::Cancelling {
            acknowledged: false,
            killing: false,
        } => (format!("Cancel sent — {name}"), Tone::Warning),
        Phase::Cancelling { killing: false, .. } => {
            (format!("Cancelling… — {name}"), Tone::Warning)
        }
        Phase::Cancelling { killing: true, .. } => {
            (format!("Stopping the worker — {name}"), Tone::Warning)
        }
        Phase::Completed(_) if activity.is_first_pass() => {
            (format!("First pass ready — {name}"), Tone::Success)
        }
        Phase::Completed(_) => (format!("Completed — {name}"), Tone::Success),
        Phase::Cancelled => (format!("Cancelled — {name}"), Tone::Neutral),
        Phase::Failed(_) => (format!("Failed — {name}"), Tone::Error),
        Phase::Lost(_) => (format!("Stopped unexpectedly — {name}"), Tone::Error),
    }
}

/// The explanation under the headline: what is true now and what to do.
#[must_use]
pub fn detail(activity: &Activity) -> String {
    match activity.phase() {
        Phase::Running(Stage::Checking) if activity.is_first_pass() => {
            "Checking source timing before the first pass.".into()
        }
        Phase::Starting | Phase::Running(_) if activity.is_first_pass() => {
            "Closing the window stops the first pass.".into()
        }
        Phase::Starting | Phase::Running(Stage::Encoding)
            if activity.checks_timing() && activity.before_first_frame() =>
        {
            "Checking the frame timing first; a long file takes a moment. Closing the window \
             stops converting."
                .to_owned()
        }
        Phase::Starting | Phase::Running(_) => "Closing the window stops converting.".to_owned(),
        Phase::Cancelling { killing: false, .. } => {
            "Stopping at the next safe point. Nothing is published.".to_owned()
        }
        Phase::Cancelling { killing: true, .. } => {
            "The worker did not stop in time and is being terminated.".to_owned()
        }
        Phase::Completed(report) => format!(
            "{} frames{} · {} · in {}",
            count(report.frames),
            if activity.is_first_pass() {
                " analyzed"
            } else {
                ""
            },
            bytes_exact(report.bytes),
            elapsed(activity.elapsed(std::time::Instant::now()))
        ),
        Phase::Cancelled => format!(
            "Nothing was written to {}. Press Convert to start again.",
            file_name(activity.destination())
        ),
        Phase::Failed(failure) => failure_text(failure),
        Phase::Lost(why) => format!(
            "{why} No file was published. An unfinished temporary file may remain next to {}. \
             Press Convert to try again.",
            file_name(activity.destination())
        ),
    }
}

/// The engine's message, that nothing was published, and what to do next.
#[must_use]
pub fn failure_text(failure: &Failure) -> String {
    failure_text_with(failure, "Convert")
}

/// [`failure_text`] naming `verb` as the button that tries again: Convert
/// for the one file, Retry for a row of the list.
#[must_use]
pub fn failure_text_with(failure: &Failure, verb: &str) -> String {
    let remedy = match failure.code {
        FailureCode::InvalidSource => "Check the source file, then open it again.".to_owned(),
        FailureCode::UnsupportedSource => UNSUPPORTED_REMEDY.to_owned(),
        FailureCode::MediaToolMissing => "This build can read Y4M files only.".to_owned(),
        FailureCode::SourceChanged => {
            format!("The file was read again; check it, then press {verb}.")
        }
        FailureCode::OmissionsNotAcknowledged => {
            format!("Press {verb} and confirm that only the video is converted.")
        }
        FailureCode::TooLarge => "Use a source with a smaller frame size.".to_owned(),
        FailureCode::InvalidSettings => {
            format!("Change the settings, then press {verb} again.")
        }
        FailureCode::SourceIsDestination => "Choose a different output file.".to_owned(),
        FailureCode::DestinationExists => format!(
            "Choose another output file, or move the existing one, then press {verb} again."
        ),
        FailureCode::Output => format!(
            "Check free space and write permission for the output folder, then press {verb} \
             again."
        ),
        FailureCode::Encoder | FailureCode::VerificationFailed => {
            format!("Press {verb} to try again. If it fails again, report this message.")
        }
    };
    let message = failure.message.trim_end_matches('.');
    format!("{message}. No file was published. {remedy}")
}

/// The name of a verification check.
#[must_use]
pub const fn check_name(kind: CheckKind) -> &'static str {
    match kind {
        CheckKind::PassArtifact => "First-pass artifact integrity",
        CheckKind::Structure => "Structure",
        CheckKind::SplotDecode => "Splot decode (same codebase as the encoder)",
        CheckKind::AvmDecode => "Independent AVM decode",
    }
}

/// `(name, outcome word, detail, tone)` for one check. "Not run" is never
/// shown as a pass.
#[must_use]
pub fn check(check: &Check) -> (&'static str, &'static str, String, Tone) {
    let name = check_name(check.kind);
    match &check.outcome {
        CheckOutcome::Passed => (name, "Passed", String::new(), Tone::Success),
        CheckOutcome::Failed { detail } => (name, "Failed", detail.clone(), Tone::Error),
        CheckOutcome::NotRun { reason } => (name, "Not run", reason.clone(), Tone::Warning),
    }
}

/// The counters line under the bar: `45 of 90 frames encoded · 50 read ·
/// 1.2 MB written`, or that no frame has arrived yet.
#[must_use]
pub fn counters(activity: &Activity) -> String {
    if activity.before_first_frame() || activity.is_first_pass() {
        return progress_text(activity);
    }
    let progress = activity.progress();
    format!(
        "{} · {} read · {} written",
        progress_text(activity),
        count(progress.frames_read),
        bytes(progress.bytes_written)
    )
}

/// The count of completed or accepted frames for the current job.
#[must_use]
pub fn progress_text(activity: &Activity) -> String {
    if activity.before_first_frame() {
        return "Waiting for the first frame".into();
    }
    let progress = activity.progress();
    let (frames, verb) = if activity.is_first_pass() {
        // The pass API reports accepted frames while it runs and costs for
        // all frames after finish. Only those costs prove completed analysis.
        let verb = if progress.frames_coded_milli >= progress.frames_read.saturating_mul(1000) {
            "analyzed"
        } else {
            "accepted"
        };
        (progress.frames_read, verb)
    } else {
        (progress.frames_encoded, "encoded")
    };
    match activity.total_frames() {
        Some(total) => format!("{} of {} frames {verb}", count(frames), count(total)),
        None if activity.is_first_pass() => format!("{} frames {verb}", count(frames)),
        None => format!("{} frames {verb} (total unknown)", count(frames)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use studio_domain::job::{SourceColor, SourceFingerprint};
    use studio_domain::protocol::MediaTool;

    #[test]
    fn first_pass_counts_accepted_and_analyzed_frames() {
        use crate::activity::Input;
        use std::time::Instant;
        use studio_domain::job::{JobPurpose, JobReport, Progress, WirePath};
        use studio_domain::protocol::Event;

        let now = Instant::now();
        let mut pass = Activity::new("sample.splot-pass.json".into(), Some(8), false, now)
            .with_purpose(JobPurpose::FirstPass);
        assert!(headline(&pass).0.starts_with("Starting first pass"));
        assert_eq!(counters(&pass), "Waiting for the first frame");
        assert_eq!(pass.fraction(), None);
        pass.apply(
            Input::Event(Event::Progress {
                progress: Progress {
                    frames_read: 2,
                    ..Progress::default()
                },
            }),
            now,
        );
        assert_eq!(pass.fraction(), Some(0.25));
        assert_eq!(progress_text(&pass), "2 of 8 frames accepted");
        assert_eq!(counters(&pass), "2 of 8 frames accepted");
        pass.apply(
            Input::Event(Event::Completed {
                report: JobReport {
                    destination: WirePath::Utf8("sample.splot-pass.json".into()),
                    frames: 8,
                    bytes: 32,
                    checks: Vec::new(),
                    disclosures: Vec::new(),
                    pass_digest: Some("example".into()),
                },
            }),
            now,
        );
        assert_eq!(pass.progress().frames_encoded, 0);
        assert_eq!(pass.fraction(), Some(1.0));
        assert_eq!(progress_text(&pass), "8 of 8 frames analyzed");
        assert!(headline(&pass).0.starts_with("First pass ready"));
    }

    #[test]
    fn frame_rates_stay_exact() {
        let r = |num, den| frame_rate(Rational { num, den });
        assert_eq!(r(30000, 1001), "30000/1001 fps (≈ 29.97)");
        assert_eq!(r(24000, 1001), "24000/1001 fps (≈ 23.98)");
        assert_eq!(r(25, 1), "25 fps");
        assert_eq!(r(50, 2), "50/2 fps (= 25)");
        assert_eq!(r(1, 0), "invalid (1/0)");
    }

    #[test]
    fn counts_and_sizes() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1000), "1,000");
        assert_eq!(count(1_234_567), "1,234,567");
        assert_eq!(bytes(999), "999 bytes");
        assert_eq!(bytes(1000), "1.00 kB");
        assert_eq!(bytes(1_234_567), "1.23 MB");
        assert_eq!(bytes(999_999), "1.00 MB");
        assert_eq!(bytes(12_400_000), "12.4 MB");
        assert_eq!(bytes(124_000_000), "124 MB");
        assert_eq!(bytes(99_960), "100 kB");
        assert_eq!(bytes_exact(1_234_567), "1.23 MB (1,234,567 bytes)");
        assert_eq!(bytes_exact(12), "12 bytes");
    }

    #[test]
    fn elapsed_time() {
        assert_eq!(elapsed(Duration::from_millis(7900)), "0:07");
        assert_eq!(elapsed(Duration::from_secs(754)), "12:34");
        assert_eq!(elapsed(Duration::from_secs(3723)), "1:02:03");
    }

    #[test]
    fn not_run_never_reads_as_a_pass() {
        let not_run = Check {
            kind: CheckKind::AvmDecode,
            outcome: CheckOutcome::NotRun {
                reason: "AVM is not installed".into(),
            },
        };
        let (_, word, detail, tone) = check(&not_run);
        assert_eq!(word, "Not run");
        assert_eq!(detail, "AVM is not installed");
        assert_ne!(tone, Tone::Success);
    }

    #[test]
    fn facts_list_unknowns_and_optional_aspect() {
        let mut f = SourceFacts {
            reader: SourceReader::Y4m,
            fingerprint: SourceFingerprint {
                len: 1,
                modified: None,
            },
            video_stream: 0,
            video_tracks: Vec::new(),
            omissions: Omissions::default(),
            width: 640,
            height: 360,
            frame_rate: Rational {
                num: 30000,
                den: 1001,
            },
            pixel_aspect: None,
            color: SourceColor::default(),
            bit_depth: 10,
            chroma: ChromaLayout::Yuv420,
            frame_count: None,
            disclosures: Vec::new(),
        };
        let rows = facts(&f);
        assert!(rows.contains(&("Frames", "Unknown".to_owned())));
        assert_eq!(rows[0], ("Video", "Y4M, read directly".to_owned()));
        assert!(!rows.iter().any(|(label, _)| *label == "Pixel aspect"));
        f.pixel_aspect = Some(Rational { num: 1, den: 1 });
        f.frame_count = Some(1200);
        let rows = facts(&f);
        assert!(rows.contains(&("Frames", "1,200".to_owned())));
        assert!(rows.contains(&("Pixel aspect", "1:1 (square pixels)".to_owned())));
    }

    #[test]
    fn the_file_card_line_shows_what_is_known() {
        assert_eq!(source_line(None, None), "");
        assert_eq!(source_line(None, Some(12_400_000)), "12.4 MB");
        let f = SourceFacts {
            reader: SourceReader::Y4m,
            fingerprint: SourceFingerprint {
                len: 1,
                modified: None,
            },
            video_stream: 0,
            video_tracks: Vec::new(),
            omissions: Omissions::default(),
            width: 1280,
            height: 720,
            frame_rate: Rational { num: 25, den: 1 },
            pixel_aspect: None,
            color: SourceColor::default(),
            bit_depth: 8,
            chroma: ChromaLayout::Yuv420,
            frame_count: None,
            disclosures: Vec::new(),
        };
        assert_eq!(source_line(Some(&f), Some(12_400_000)), "720p  •  12.4 MB");
    }

    #[test]
    fn failures_say_nothing_was_published_and_what_to_do() {
        let text = failure_text(&Failure {
            code: FailureCode::DestinationExists,
            message: "clip.ivf already exists.".into(),
        });
        assert!(text.starts_with("clip.ivf already exists. No file was published."));
        assert!(text.contains("Choose another output file"));
    }

    fn omissions(audio: u32, subtitles: u32, chapters: u32) -> Omissions {
        Omissions {
            audio,
            subtitles,
            chapters,
            ..Omissions::default()
        }
    }

    #[test]
    fn lists_read_naturally() {
        let items = |n: usize| (1..=n).map(|i| i.to_string()).collect::<Vec<_>>();
        assert_eq!(join(&items(0)), "");
        assert_eq!(join(&items(1)), "1");
        assert_eq!(join(&items(2)), "1 and 2");
        assert_eq!(join(&items(4)), "1, 2, 3 and 4");
    }

    #[test]
    fn the_omission_summary_names_every_category_with_its_count() {
        assert_eq!(omission_summary(&Omissions::default(), 0), "");
        assert_eq!(
            omission_summary(&omissions(2, 3, 4), 0),
            "Video only — 2 audio tracks, 3 subtitle tracks and 4 chapters will not be included."
        );
        assert_eq!(
            omission_summary(&omissions(1, 0, 0), 0),
            "Video only — 1 audio track will not be included."
        );
        assert_eq!(
            omission_summary(&omissions(0, 1, 1), 0),
            "Video only — 1 subtitle track and 1 chapter will not be included."
        );
        let all_one = Omissions {
            other_video: 2,
            audio: 1,
            subtitles: 1,
            attachments: 1,
            other: 1,
            chapters: 1,
        };
        assert_eq!(
            omitted(&all_one, 1),
            "1 other video track, 1 attached picture, 1 audio track, 1 subtitle track, \
             1 attachment, 1 other stream and 1 chapter"
        );
        let all_many = Omissions {
            other_video: 5,
            audio: 2,
            subtitles: 3,
            attachments: 4,
            other: 6,
            chapters: 12,
        };
        assert_eq!(
            omitted(&all_many, 2),
            "3 other video tracks, 2 attached pictures, 2 audio tracks, 3 subtitle tracks, \
             4 attachments, 6 other streams and 12 chapters"
        );
        // Only pictures, and a picture count the engine does not back.
        let art = Omissions {
            other_video: 1,
            ..Omissions::default()
        };
        assert_eq!(omitted(&art, 1), "1 attached picture");
        assert_eq!(omitted(&art, 7), "1 attached picture");
        assert_eq!(omitted(&art, 0), "1 other video track");
    }

    #[test]
    fn every_single_category_reads_alone_in_singular_and_plural() {
        type Make = fn(u32) -> Omissions;
        let cases: [(Make, &str, &str); 6] = [
            (
                |n| Omissions {
                    other_video: n,
                    ..Omissions::default()
                },
                "other video track",
                "other video tracks",
            ),
            (|n| omissions(n, 0, 0), "audio track", "audio tracks"),
            (|n| omissions(0, n, 0), "subtitle track", "subtitle tracks"),
            (
                |n| Omissions {
                    attachments: n,
                    ..Omissions::default()
                },
                "attachment",
                "attachments",
            ),
            (
                |n| Omissions {
                    other: n,
                    ..Omissions::default()
                },
                "other stream",
                "other streams",
            ),
            (|n| omissions(0, 0, n), "chapter", "chapters"),
        ];
        for (make, one, many) in cases {
            assert_eq!(omitted(&make(1), 0), format!("1 {one}"));
            assert_eq!(omitted(&make(2), 0), format!("2 {many}"));
            assert!(make(1).any());
        }
    }

    #[test]
    fn the_question_repeats_the_omissions_the_output_and_that_the_source_stays() {
        let text = omission_question(&omissions(2, 0, 1), 0, OutputKind::Ivf);
        assert_eq!(
            text,
            "This file also has 2 audio tracks and 1 chapter, which will not be included in \
             the AV2 IVF file. The source file is not modified."
        );
    }

    #[test]
    fn the_reader_names_the_codec_but_never_a_container() {
        let track = VideoTrack {
            stream: 1,
            codec: "h264".into(),
            width: 1920,
            height: 1080,
            attached_picture: false,
        };
        let f = SourceFacts {
            reader: SourceReader::Ffmpeg,
            fingerprint: SourceFingerprint {
                len: 1,
                modified: None,
            },
            video_stream: 1,
            video_tracks: vec![track.clone()],
            omissions: Omissions::default(),
            width: 1920,
            height: 1080,
            frame_rate: Rational { num: 25, den: 1 },
            pixel_aspect: None,
            color: SourceColor::default(),
            bit_depth: 8,
            chroma: ChromaLayout::Yuv420,
            frame_count: None,
            disclosures: Vec::new(),
        };
        assert_eq!(reader(&f), "H.264, read with FFmpeg");
        assert_eq!(super::track(&track), "Stream 1 · H.264 · 1920 × 1080");
        assert_eq!(codec("theora"), "theora");
        let other = SourceFacts {
            video_stream: 9,
            ..f
        };
        assert_eq!(reader(&other), "Read with FFmpeg");
    }

    #[test]
    fn attached_pictures_are_explained_once() {
        let picture = |stream| VideoTrack {
            stream,
            codec: "mjpeg".into(),
            width: 600,
            height: 600,
            attached_picture: true,
        };
        assert_eq!(attached_note(&[]), "");
        assert!(attached_note(&[picture(3)]).starts_with("Stream 3 is an attached picture"));
        assert!(
            attached_note(&[picture(3), picture(5)]).starts_with("Streams 3 and 5 are attached")
        );
    }

    #[test]
    fn the_media_tool_line_says_where_ffmpeg_comes_from() {
        let with = |media_tool| Capabilities {
            encode_outputs: Vec::new(),
            media_tool,
            analysis_framings: Vec::new(),
            options: None,
        };
        assert_eq!(media_tool(None), "FFmpeg: not known yet.");
        assert_eq!(
            media_tool(Some(&with(None))),
            "FFmpeg: not in this build. Only Y4M files can be read."
        );
        let system = with(Some(MediaTool {
            version: "ffmpeg version 8.0.1-3ubuntu2".into(),
            origin: MediaToolOrigin::System,
        }));
        assert_eq!(
            media_tool(Some(&system)),
            "FFmpeg 8.0.1-3ubuntu2 from this computer: development setup — not a shipped FFmpeg."
        );
        let bundled = with(Some(MediaTool {
            version: "7.1.1".into(),
            origin: MediaToolOrigin::Bundled,
        }));
        assert_eq!(
            media_tool(Some(&bundled)),
            "FFmpeg 7.1.1, shipped with the app."
        );
        assert_eq!(
            empty_caption(Some(&with(None))),
            "This build can read Y4M files only."
        );
        assert_ne!(
            empty_caption(Some(&bundled)),
            empty_caption(Some(&with(None)))
        );
        assert_eq!(empty_caption(None), empty_caption(Some(&bundled)));
    }

    #[test]
    fn probe_problems_explain_a_missing_tool_and_keep_engine_words() {
        let missing = ProbeFailure {
            code: Some(FailureCode::MediaToolMissing),
            message: "no ffprobe".into(),
        };
        assert!(probe_problem(&missing).starts_with("This build can read Y4M files only."));
        let vfr = ProbeFailure {
            code: Some(FailureCode::UnsupportedSource),
            message: "The video has a variable frame rate.".into(),
        };
        assert_eq!(
            probe_problem(&vfr),
            "The video has a variable frame rate. Splot Studio does not support this yet."
        );
        assert_eq!(
            probe_problem(&ProbeFailure::other("worker died")),
            "worker died"
        );
        let text = failure_text(&Failure {
            code: FailureCode::UnsupportedSource,
            message: "Interlaced video (top field first)".into(),
        });
        assert_eq!(
            text,
            "Interlaced video (top field first). No file was published. Splot Studio does not \
             support this yet."
        );
        let changed = failure_text(&Failure {
            code: FailureCode::SourceChanged,
            message: "The source file changed since it was read".into(),
        });
        assert!(changed.ends_with("The file was read again; check it, then press Convert."));
    }
}
