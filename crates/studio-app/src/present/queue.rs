// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Display text for the queue: rows, the run, notices, and what an
//! interrupted or lost job left behind. Every state is said in words; a
//! colour or icon only repeats it.

use std::fmt::Write as _;
use std::path::Path;

use studio_domain::job::JobPurpose;
use studio_domain::settings::OutputKind;

use super::{Tone, bytes_exact, count, failure_text_with, file_name, omitted, stage};
use crate::activity::Phase;
use crate::queue::{Cause, Ending, Interruption, Item, ItemState, PartialFate};
use crate::state::{Asked, JournalProblem, Restored};

/// A row's state, for its icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// Being read.
    Probing,
    /// Waiting to convert.
    Waiting,
    /// Cannot be converted.
    Unreadable,
    /// Converting.
    Converting,
    /// Converted.
    Done,
    /// Failed or lost.
    Failed,
    /// Cancelled.
    Cancelled,
    /// Interrupted when the app stopped.
    Interrupted,
}

/// `(icon kind, words)` for a row's state.
#[must_use]
pub fn row_state(item: &Item) -> (RowKind, String) {
    let words = |s: &str| s.to_owned();
    match item.state() {
        ItemState::Probing => (RowKind::Probing, words("Reading…")),
        ItemState::Waiting if item.destination().exists => {
            (RowKind::Waiting, words("Waiting · output exists"))
        }
        ItemState::Waiting if item.leaves_out_something() => {
            (RowKind::Waiting, words("Waiting · video only"))
        }
        ItemState::Waiting => (RowKind::Waiting, words("Waiting")),
        ItemState::Unreadable => (RowKind::Unreadable, words("Cannot convert")),
        ItemState::Running | ItemState::Publishing => {
            let text = match item.activity().map(crate::activity::Activity::phase) {
                Some(Phase::Cancelling { .. }) => words("Cancelling…"),
                Some(Phase::Running(s)) => match item
                    .activity()
                    .and_then(crate::activity::Activity::fraction)
                {
                    Some(f) => format!("{} · {}%", stage(*s), percent(f)),
                    None => stage(*s).to_owned(),
                },
                _ if is_first_pass(item) => words("Starting first pass"),
                _ => words("Starting"),
            };
            (RowKind::Converting, text)
        }
        ItemState::Completed if is_first_pass(item) => (RowKind::Done, words("First pass ready")),
        ItemState::Completed => (RowKind::Done, words("Done")),
        ItemState::Cancelled => (RowKind::Cancelled, words("Cancelled")),
        ItemState::Failed => (RowKind::Failed, words("Failed")),
        ItemState::Lost => (RowKind::Failed, words("Stopped unexpectedly")),
        ItemState::Interrupted => (RowKind::Interrupted, words("Interrupted")),
    }
}

/// A whole percentage of a display fraction, never 100 before the end.
fn percent(fraction: f32) -> u32 {
    let clamped = fraction.clamp(0.0, 0.99);
    // Display only; the counts stay exact elsewhere.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let whole = (clamped * 100.0).floor() as u32;
    whole
}

/// A row's second line: `1080p  •  412 MB`, or what is known so far.
#[must_use]
pub fn row_line(item: &Item) -> String {
    super::source_line(item.facts(), item.size())
}

/// The quiet action a row offers: Remove, Cancel, Retry or Show in Folder,
/// with its description.
#[must_use]
pub const fn row_action(item: &Item) -> (&'static str, &'static str) {
    match item.state() {
        ItemState::Probing | ItemState::Waiting | ItemState::Unreadable => (
            "Remove",
            "Remove this file from the list. The file itself stays.",
        ),
        ItemState::Running | ItemState::Publishing => {
            ("Cancel", "Stop converting this file. Nothing is published.")
        }
        ItemState::Cancelled | ItemState::Failed | ItemState::Lost | ItemState::Interrupted => (
            "Retry",
            "Read this file again and convert it in the next run.",
        ),
        ItemState::Completed => (
            "Show in Folder",
            "Show the converted file in the file manager",
        ),
    }
}

/// `4 converted, 1 failed`: what the finished files came to, or nothing
/// when none has finished.
#[must_use]
pub fn summary(items: &[Item]) -> String {
    let n = |states: &[ItemState]| items.iter().filter(|i| states.contains(&i.state())).count();
    let parts: Vec<String> = [
        (n(&[ItemState::Completed]), "converted"),
        (n(&[ItemState::Failed, ItemState::Lost]), "failed"),
        (n(&[ItemState::Cancelled]), "cancelled"),
        (n(&[ItemState::Interrupted]), "interrupted"),
    ]
    .into_iter()
    .filter(|(count, _)| *count > 0)
    .map(|(count, word)| format!("{count} {word}"))
    .collect();
    parts.join(", ")
}

/// The primary button: Convert, Convert N Files or Continue.
#[must_use]
pub fn primary(single: bool, startable: usize, paused: bool) -> String {
    match (paused, single, startable) {
        (true, ..) => "Continue".to_owned(),
        (false, true, _) | (false, false, 0) => "Convert".to_owned(),
        (false, false, 1) => "Convert 1 File".to_owned(),
        (false, false, n) => format!("Convert {n} Files"),
    }
}

/// `2 of 5`.
#[must_use]
pub fn position((at, of): (usize, usize)) -> String {
    format!("{at} of {of}")
}

/// `4 files`.
#[must_use]
pub fn files(n: usize) -> String {
    match n {
        1 => "1 file".to_owned(),
        n => format!("{n} files"),
    }
}

/// The calm line after a restart.
#[must_use]
pub fn restored(restored: &Restored) -> String {
    let mut text = match restored.files {
        0 => String::new(),
        1 => "Restored 1 file from last time.".to_owned(),
        n => format!("Restored {n} files from last time."),
    };
    match restored.interrupted {
        0 => {}
        1 if restored.files == 1 => text.push_str(" It was interrupted."),
        1 => text.push_str(" 1 was interrupted."),
        n => {
            let _ = write!(text, " {n} were interrupted.");
        }
    }
    match restored.dropped {
        0 => {}
        1 => text.push_str(" 1 file could not be restored."),
        n => {
            let _ = write!(text, " {n} files could not be restored.");
        }
    }
    text.trim().to_owned()
}

/// What happened to the journal, for the notice line.
#[must_use]
pub fn journal_problem(problem: &JournalProblem) -> (String, Tone) {
    match problem {
        JournalProblem::MovedAside(to) => (
            format!(
                "The saved list could not be read and was moved to {}. Starting with an empty list.",
                to.display()
            ),
            Tone::Warning,
        ),
        JournalProblem::ViewUnreadable(copy) => (
            format!(
                "The saved Analyze documents could not be read and were not restored; the list \
                 of files was. A copy of the saved list is at {}.",
                copy.display()
            ),
            Tone::Warning,
        ),
        JournalProblem::TooNew { path, version } => (
            format!(
                "The saved list at {} is from a newer Splot Studio (format {version}). It was \
                 left untouched; this list will not be saved.",
                path.display()
            ),
            Tone::Warning,
        ),
        JournalProblem::Unreadable { path, error } => (
            format!(
                "The saved list at {} cannot be read ({error}). It was left untouched; this \
                 list will not be saved.",
                path.display()
            ),
            Tone::Warning,
        ),
        JournalProblem::NoFolder(why) => (
            format!("The list cannot be saved: {why}. It is kept until the app closes."),
            Tone::Warning,
        ),
    }
}

/// The persistent notice while saving fails.
#[must_use]
pub fn save_failed(reason: &str) -> String {
    format!("The list cannot be saved: {reason}. It is kept until the app closes.")
}

/// The confirmation for several files: a short sentence, then one line per
/// file (at most `shown`, then "and N more files").
#[must_use]
pub fn omission_lines(asked: &[Asked], shown: usize) -> Vec<String> {
    let mut lines: Vec<String> = asked
        .iter()
        .take(shown)
        .map(|a| {
            format!(
                "{} — {}",
                file_name(&a.source),
                omitted(&a.omissions, a.attached_pictures)
            )
        })
        .collect();
    match asked.len().saturating_sub(shown) {
        0 => {}
        1 => lines.push("and 1 more file".to_owned()),
        n => lines.push(format!("and {n} more files")),
    }
    lines
}

/// The sentence above [`omission_lines`].
#[must_use]
pub fn omission_question_many(files: usize, output: OutputKind) -> String {
    format!(
        "{files} files also have tracks or chapters that will not be included in the {} \
         files. The source files are not modified.",
        output.label()
    )
}

/// The headline for an item's last or current attempt, like the
/// activity area's, also for one restored from the journal.
#[must_use]
pub fn headline(item: &Item) -> (String, Tone) {
    if let Some(activity) = item.activity()
        && item.state() != ItemState::Interrupted
    {
        return super::headline(activity);
    }
    let name = file_name(&item.destination().path);
    match item.ending() {
        Some(Ending::Completed(_)) if is_first_pass(item) => {
            (format!("First pass ready — {name}"), Tone::Success)
        }
        Some(Ending::Completed(_)) => (format!("Completed — {name}"), Tone::Success),
        Some(Ending::Cancelled) => (format!("Cancelled — {name}"), Tone::Neutral),
        Some(Ending::Failed(_)) => (format!("Failed — {name}"), Tone::Error),
        Some(Ending::Lost { .. }) => (format!("Stopped unexpectedly — {name}"), Tone::Error),
        Some(Ending::Interrupted(_)) => (format!("Interrupted — {name}"), Tone::Warning),
        None => (String::new(), Tone::Neutral),
    }
}

/// The explanation under [`headline`]: what is true now and what to do.
/// `verb` names the button that tries again (Convert, or Retry in the list).
#[must_use]
pub fn detail(item: &Item, verb: &str) -> String {
    let destination = &item.destination().path;
    let name = file_name(destination);
    match item.ending() {
        None => item.activity().map_or_default(super::detail),
        Some(Ending::Completed(report)) => match item.activity() {
            Some(activity) => super::detail(activity),
            None => format!(
                "{} frames{} · {}",
                count(report.frames),
                if is_first_pass(item) { " analyzed" } else { "" },
                bytes_exact(report.bytes)
            ),
        },
        Some(Ending::Cancelled) => {
            format!("Nothing was written to {name}. Press {verb} to start again.")
        }
        Some(Ending::Failed(failure)) => failure_text_with(failure, verb),
        Some(Ending::Lost {
            why,
            publishing,
            partial,
        }) => {
            let mut text = format!("{}. ", why.trim_end_matches('.'));
            if *publishing && item.destination().exists {
                let _ = write!(
                    text,
                    "It stopped while the file was being published; a file exists at {} — \
                     check it.",
                    destination.display()
                );
            } else {
                text.push_str("No file was published.");
            }
            push(&mut text, &partial_note(partial, &name));
            push(&mut text, &format!("Press {verb} to try again."));
            text
        }
        Some(Ending::Interrupted(interruption)) => interrupted(interruption, destination, verb),
    }
}

fn push(text: &mut String, more: &str) {
    if !more.is_empty() {
        if !text.is_empty() && !text.ends_with(' ') {
            text.push(' ');
        }
        text.push_str(more);
    }
}

/// The honest account of an interrupted job.
#[must_use]
pub fn interrupted(i: &Interruption, destination: &Path, verb: &str) -> String {
    let who = match i.cause {
        Cause::AppClosed => "Splot Studio was closed",
        Cause::AppStopped => "Splot Studio stopped",
    };
    let mut text = if i.publishing && i.destination_present {
        format!(
            "{who} while the file was being published; a file exists at {} — check it.",
            destination.display()
        )
    } else if i.destination_present {
        format!(
            "{who} while this file was converting. A file exists at {} — check it.",
            destination.display()
        )
    } else {
        format!("{who} while this file was converting. Nothing was published.")
    };
    push(
        &mut text,
        &partial_note(&i.partial, &file_name(destination)),
    );
    let again = if i.destination_present {
        format!("Press {verb} to convert it again to a new name.")
    } else {
        format!("Press {verb} to convert it again from the start.")
    };
    push(&mut text, &again);
    text
}

/// What became of a temporary file, in one sentence, or nothing.
#[must_use]
pub fn partial_note(partial: &PartialFate, destination_name: &str) -> String {
    match partial {
        PartialFate::None | PartialFate::Gone => String::new(),
        PartialFate::Pending(_) => {
            format!("An unfinished temporary file may remain next to {destination_name}.")
        }
        PartialFate::Removed(_) => "Its unfinished temporary file was removed.".to_owned(),
        PartialFate::Kept { path, reason } if path.as_os_str().is_empty() => {
            format!("Its temporary file was left alone: {reason}.")
        }
        PartialFate::Kept { path, reason } => {
            format!("{} was left alone because {reason}.", path.display())
        }
    }
}

/// A row's whole state for a screen reader: name, facts, state.
#[must_use]
pub fn row_accessible(item: &Item) -> String {
    let (_, state) = row_state(item);
    let line = row_line(item);
    let name = file_name(item.source());
    let parts: Vec<String> = [name, line, state]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();
    parts.join(", ")
}

/// `Converted with these settings` and the like, above the settings when
/// they show a file's snapshot.
#[must_use]
pub fn settings_note(item: &Item) -> String {
    let name = file_name(item.source());
    match item.state() {
        ItemState::Completed if is_first_pass(item) => {
            format!("Studio analyzed {name} with these settings.")
        }
        ItemState::Running | ItemState::Publishing if is_first_pass(item) => {
            format!("The first pass uses these settings for {name}.")
        }
        ItemState::Completed => format!("{name} was converted with these settings."),
        ItemState::Running | ItemState::Publishing => {
            format!("{name} is converting with these settings.")
        }
        _ => String::new(),
    }
}

pub(super) fn is_first_pass(item: &Item) -> bool {
    item.submitted
        .as_ref()
        .map(|submission| submission.job.purpose)
        .or_else(|| item.attempt().map(|attempt| attempt.job.purpose))
        == Some(JobPurpose::FirstPass)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::abs;

    #[test]
    fn the_primary_button_names_what_it_does() {
        assert_eq!(primary(true, 1, false), "Convert");
        assert_eq!(primary(false, 1, false), "Convert 1 File");
        assert_eq!(primary(false, 4, false), "Convert 4 Files");
        assert_eq!(primary(false, 4, true), "Continue");
        assert_eq!(primary(true, 1, true), "Continue");
        assert_eq!(primary(false, 0, false), "Convert");
    }

    #[test]
    fn the_restored_notice_is_one_calm_line() {
        let r = |files, interrupted, dropped| {
            restored(&Restored {
                files,
                interrupted,
                dropped,
            })
        };
        assert_eq!(
            r(3, 1, 0),
            "Restored 3 files from last time. 1 was interrupted."
        );
        assert_eq!(
            r(1, 1, 0),
            "Restored 1 file from last time. It was interrupted."
        );
        assert_eq!(
            r(4, 2, 0),
            "Restored 4 files from last time. 2 were interrupted."
        );
        assert_eq!(
            r(2, 0, 1),
            "Restored 2 files from last time. 1 file could not be restored."
        );
    }

    #[test]
    fn many_omissions_are_listed_then_counted() {
        use studio_domain::job::Omissions;
        let asked = |name: &str| Asked {
            item: 1,
            ack: crate::queue::Acknowledgement {
                fingerprint: studio_domain::job::SourceFingerprint {
                    len: 1,
                    modified: None,
                },
                video_stream: 0,
                omissions: Omissions::default(),
                settings: crate::state::INITIAL_SETTINGS,
            },
            source: abs(&format!("/m/{name}")),
            omissions: Omissions {
                audio: 2,
                ..Omissions::default()
            },
            attached_pictures: 0,
        };
        let all: Vec<Asked> = (1..=7).map(|n| asked(&format!("{n}.mp4"))).collect();
        let lines = omission_lines(&all, 5);
        assert_eq!(lines.len(), 6);
        assert_eq!(lines[0], "1.mp4 — 2 audio tracks");
        assert_eq!(lines[5], "and 2 more files");
        assert_eq!(omission_lines(&all[..6], 5)[5], "and 1 more file");
        assert_eq!(omission_lines(&all[..2], 5).len(), 2);
    }

    #[test]
    fn an_interruption_while_publishing_with_a_file_there_says_check_it() {
        let i = Interruption {
            publishing: true,
            cause: Cause::AppStopped,
            destination_present: true,
            partial: PartialFate::Gone,
        };
        let destination = abs("/out/clip.ivf");
        let text = interrupted(&i, &destination, "Retry");
        assert_eq!(
            text,
            format!(
                "Splot Studio stopped while the file was being published; a file exists at {} \
                 — check it. Press Retry to convert it again to a new name.",
                destination.display()
            )
        );
        assert!(!text.contains("Completed"));
        let plain = Interruption {
            publishing: true,
            destination_present: false,
            cause: Cause::AppClosed,
            partial: PartialFate::Removed(abs("/out/.clip.ivf.a.partial")),
        };
        assert_eq!(
            interrupted(&plain, &destination, "Retry"),
            "Splot Studio was closed while this file was converting. Nothing was published. \
             Its unfinished temporary file was removed. Press Retry to convert it again from \
             the start."
        );
    }

    #[test]
    fn a_kept_temporary_file_is_named_with_why() {
        let path = abs("/out/.clip.ivf.a.partial");
        let kept = PartialFate::Kept {
            path: path.clone(),
            reason: "it is a symbolic link".into(),
        };
        assert_eq!(
            partial_note(&kept, "clip.ivf"),
            format!(
                "{} was left alone because it is a symbolic link.",
                path.display()
            )
        );
        assert_eq!(partial_note(&PartialFate::Gone, "clip.ivf"), "");
    }
}
