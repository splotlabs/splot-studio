// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Display text for the picture area. The side label is always taken from
//! the picture's own origin, so a source picture can never read as a result.

use std::fmt::Write as _;

use studio_domain::preview::{
    ColorMatrix, MediaTime, PictureFailure, PictureHeader, PictureOrigin,
};

use crate::preview::{StageState, StageView};

/// While the first picture is on its way.
pub const LOADING: &str = "Loading the picture…";
/// While converting, before the first encoded picture.
pub const WAITING_LIVE: &str = "The encoded picture appears after the first frames.";
/// On a result or live picture: what press and hold does.
pub const HOLD: &str = "Hold to see the original";
/// While held, before the original has arrived.
pub const LOADING_ORIGINAL: &str = "Loading the original…";
/// Why Play is not offered when the shown picture has no time.
pub const NO_TIMING: &str = "Playback needs frame timing, and this file has none.";
/// The document changed after the inspection engine read it.
pub const DOCUMENT_CHANGED: &str = "The file changed. Close the document and open the file again.";
/// Under Advanced: what the preview is and is not.
pub const SDR_NOTE: &str = "The preview is an 8-bit SDR approximation for comparing source and \
                            result, not a reference display. Its colours are not managed.";

/// Why a requested picture cannot be shown.
#[must_use]
pub fn picture_failure(code: PictureFailure, message: &str, result: bool) -> String {
    let what = if result {
        "the converted file"
    } else {
        "this file"
    };
    match code {
        PictureFailure::Unreadable if result => format!(
            "No preview: the converted file cannot be read. It may have been moved or deleted. \
             ({message})"
        ),
        PictureFailure::Unreadable => format!("No preview: {what} cannot be read. ({message})"),
        PictureFailure::SourceChanged => {
            "No preview: the source changed since it was read. Remove it and add it again.".into()
        }
        PictureFailure::Unsupported => format!("No preview: {what} cannot be shown. ({message})"),
        PictureFailure::OutOfRange => "There is no such frame.".into(),
        PictureFailure::NoTiming => NO_TIMING.into(),
        PictureFailure::MediaToolMissing => {
            "No preview: showing this file needs FFmpeg, which this build does not have.".into()
        }
        PictureFailure::InvalidRequest => {
            format!("No preview: the engine refused the request. ({message})")
        }
    }
}

/// Why playback stopped or cannot start.
#[must_use]
pub fn playback_failure(code: PictureFailure, message: &str) -> String {
    match code {
        PictureFailure::NoTiming => NO_TIMING.into(),
        other => format!(
            "Playback stopped: {}",
            picture_failure(other, message, false).trim_start_matches("No preview: ")
        ),
    }
}

/// The engine refused a preview command.
#[must_use]
pub fn refused(message: &str) -> String {
    format!("No preview: the engine refused a request ({message}).")
}

/// The preview process could not be started.
#[must_use]
pub fn not_started(why: &str) -> String {
    format!("No preview: the preview engine did not start ({why}).")
}

/// The preview process speaks another protocol.
#[must_use]
pub fn incompatible(why: &str) -> String {
    format!("No preview: the engine does not match this app ({why}). Reinstall Splot Studio.")
}

/// The preview process died too often.
#[must_use]
pub fn stopped(why: &str) -> String {
    format!("No preview: the preview engine stopped working ({why}).")
}

/// The job cannot send live pictures.
#[must_use]
pub fn live_failure(message: &str) -> String {
    format!("No live picture: {message}")
}

/// `Original`, `AV2 result · colours assumed`, `Live · frame 117 of 300`.
/// `waiting` says a source picture stands while the first encoded picture
/// is awaited.
#[must_use]
pub fn badge(header: &PictureHeader, frames: Option<u64>, waiting: bool) -> String {
    let mut badge = match header.origin {
        PictureOrigin::Source => "Original".to_owned(),
        PictureOrigin::Result => "AV2 result".to_owned(),
        PictureOrigin::Document => "Decoded AV2".to_owned(),
        PictureOrigin::Live => format!("Live · frame {}", frame_of_words(header.frame, frames)),
    };
    if waiting {
        badge.push_str(" · converting");
    }
    if header.basis.assumed {
        badge.push_str(" · colours assumed (");
        badge.push_str(matrix(header.basis.matrix));
        badge.push(')');
    }
    badge
}

const fn matrix(matrix: ColorMatrix) -> &'static str {
    match matrix {
        ColorMatrix::Bt601 => "BT.601",
        ColorMatrix::Bt709 => "BT.709",
        ColorMatrix::Bt2020 => "BT.2020",
        ColorMatrix::Monochrome => "grey",
    }
}

/// `117 of 300`, one-based; `117` when the count is unknown.
fn frame_of_words(frame: u64, frames: Option<u64>) -> String {
    match frames {
        Some(n) => format!("{} of {n}", frame.saturating_add(1)),
        None => frame.saturating_add(1).to_string(),
    }
}

/// `12 / 300`, one-based; `12` when the count is unknown.
#[must_use]
pub fn frame_of(frame: u64, frames: Option<u64>) -> String {
    match frames {
        Some(n) => format!("{} / {n}", frame.saturating_add(1)),
        None => frame.saturating_add(1).to_string(),
    }
}

/// `0:04.20`, `1:02:03.04`: the presentation time to the hundredth.
#[must_use]
pub fn time(time: MediaTime) -> String {
    if time.den == 0 {
        return String::new();
    }
    let hundredths = u128::from(time.num) * 100 / u128::from(time.den);
    let (s, cs) = (hundredths / 100, hundredths % 100);
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}.{cs:02}")
    } else {
        format!("{m}:{s:02}.{cs:02}")
    }
}

/// The picture area in one sentence.
#[must_use]
pub fn accessible(view: &StageView) -> String {
    match view.state {
        StageState::None => String::new(),
        StageState::Loading | StageState::Unavailable => format!("Preview: {}", view.message),
        StageState::Picture => {
            let mut line = format!("Preview: {}", view.badge);
            if view.navigable {
                if view.output.is_some() {
                    let _ = write!(line, ", {}", view.frame_text);
                } else {
                    let _ = write!(line, ", frame {}", view.frame_text);
                }
                if !view.time_text.is_empty() {
                    let _ = write!(line, " at {}", view.time_text);
                }
            }
            if view.native {
                line.push_str(", actual pixels");
            }
            if view.playing {
                line.push_str(", playing");
            }
            line.push('.');
            if !view.play_note.is_empty() {
                line.push(' ');
                line.push_str(&view.play_note);
            }
            line
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use studio_domain::preview::{ColorBasis, PictureView};

    fn header(origin: PictureOrigin, assumed: bool) -> PictureHeader {
        PictureHeader {
            generation: 0,
            origin,
            frame: 116,
            time: None,
            frame_count: Some(300),
            output: None,
            width: 4,
            height: 2,
            frame_width: 1920,
            frame_height: 1080,
            view: PictureView::Fit,
            basis: ColorBasis {
                matrix: ColorMatrix::Bt709,
                assumed,
            },
        }
    }

    #[test]
    fn the_badge_names_the_side_of_the_picture_itself() {
        assert_eq!(
            badge(&header(PictureOrigin::Source, false), None, false),
            "Original"
        );
        assert_eq!(
            badge(&header(PictureOrigin::Result, true), None, false),
            "AV2 result · colours assumed (BT.709)"
        );
        assert_eq!(
            badge(&header(PictureOrigin::Live, false), Some(300), false),
            "Live · frame 117 of 300"
        );
        assert_eq!(
            badge(&header(PictureOrigin::Source, false), None, true),
            "Original · converting"
        );
    }

    #[test]
    fn times_and_frames_read_exactly() {
        assert_eq!(time(MediaTime { num: 21, den: 5 }), "0:04.20");
        assert_eq!(time(MediaTime { num: 3723, den: 1 }), "1:02:03.00");
        assert_eq!(
            time(MediaTime {
                num: 1001,
                den: 30000
            }),
            "0:00.03"
        );
        assert_eq!(time(MediaTime { num: 1, den: 0 }), "");
        assert_eq!(frame_of(0, Some(300)), "1 / 300");
        assert_eq!(frame_of(41, None), "42");
    }
}
