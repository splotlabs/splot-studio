// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Display text for domain values. Exact values first; any rounded figure is
//! a secondary hint next to the exact one, never a replacement.

use std::path::Path;
use std::time::Duration;

use studio_domain::job::{
    Check, CheckKind, CheckOutcome, ChromaLayout, Disclosure, Failure, FailureCode, Rational,
    SourceFacts, Stage,
};

use crate::activity::{Activity, Phase};

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

/// Label/value rows for the source sidebar, in reading order.
#[must_use]
pub fn facts(facts: &SourceFacts) -> Vec<(&'static str, String)> {
    let mut rows = vec![
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

/// The name of a stage as the activity area shows it.
#[must_use]
pub const fn stage(stage: Stage) -> &'static str {
    match stage {
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
        Phase::Starting | Phase::Running(_) => "Closing the window stops converting.".to_owned(),
        Phase::Cancelling { killing: false, .. } => {
            "Stopping at the next safe point. Nothing is published.".to_owned()
        }
        Phase::Cancelling { killing: true, .. } => {
            "The worker did not stop in time and is being terminated.".to_owned()
        }
        Phase::Completed(report) => format!(
            "{} frames · {} · in {}",
            count(report.frames),
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
    let remedy = match failure.code {
        FailureCode::InvalidSource => "Check the source file, then open it again.",
        FailureCode::UnsupportedSource => {
            "This build encodes progressive constant-frame-rate Y4M at 8 or 10 bits."
        }
        FailureCode::TooLarge => "Use a source with a smaller frame size.",
        FailureCode::InvalidSettings => "Change the settings, then press Convert again.",
        FailureCode::SourceIsDestination => "Choose a different output file.",
        FailureCode::DestinationExists => {
            "Choose another output file, or move the existing one, then press Convert again."
        }
        FailureCode::Output => {
            "Check free space and write permission for the output folder, then press Convert again."
        }
        FailureCode::Encoder | FailureCode::VerificationFailed => {
            "Press Convert to try again. If it fails again, report this message."
        }
    };
    let message = failure.message.trim_end_matches('.');
    format!("{message}. No file was published. {remedy}")
}

/// The name of a verification check.
#[must_use]
pub const fn check_name(kind: CheckKind) -> &'static str {
    match kind {
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

/// `45 of 90 frames encoded`, or `45 frames encoded` when the total is unknown.
#[must_use]
pub fn progress_text(activity: &Activity) -> String {
    let encoded = activity.progress().frames_encoded;
    match activity.total_frames() {
        Some(total) => format!("{} of {} frames encoded", count(encoded), count(total)),
        None => format!("{} frames encoded (total unknown)", count(encoded)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            width: 640,
            height: 360,
            frame_rate: Rational {
                num: 30000,
                den: 1001,
            },
            pixel_aspect: None,
            bit_depth: 10,
            chroma: ChromaLayout::Yuv420,
            frame_count: None,
            disclosures: Vec::new(),
        };
        let rows = facts(&f);
        assert!(rows.contains(&("Frames", "Unknown".to_owned())));
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
            width: 1280,
            height: 720,
            frame_rate: Rational { num: 25, den: 1 },
            pixel_aspect: None,
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
}
