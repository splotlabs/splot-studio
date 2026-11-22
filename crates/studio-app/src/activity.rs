// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! One encode job as the activity area shows it: a state machine over the
//! engine's events, the user's Cancel and the worker process ending.
//!
//! Only a terminal event from the engine ends a job as Completed, Cancelled or
//! Failed. A worker that goes away without one is [`Phase::Lost`], never a
//! success. Once terminal, every later input is ignored, so a duplicate
//! terminal event or the worker exiting afterwards changes nothing.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use studio_domain::job::{Failure, JobReport, Progress, Stage};
use studio_domain::protocol::Event;

/// How long a job may take to stop after Cancel before its worker is killed.
/// A backstop only: the engine stops the codec call cooperatively, normally
/// well within a second, and abandons a job that ignores that after its own
/// grace (`studio_engine::worker::CANCEL_GRACE`, 3 s), so this stays above
/// that with margin.
pub const CANCEL_TIMEOUT: Duration = Duration::from_secs(5);

/// Where a job is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// `Start` was sent; no stage has been reported yet.
    Starting,
    /// The engine reported this stage.
    Running(Stage),
    /// The user asked to stop. The job is still running until a terminal
    /// event arrives.
    Cancelling {
        /// The engine acknowledged with `CancelRequested`.
        acknowledged: bool,
        /// [`CANCEL_TIMEOUT`] passed and the worker is being killed.
        killing: bool,
    },
    /// Terminal: the verified file is at its destination.
    Completed(JobReport),
    /// Terminal: the engine stopped the job on request.
    Cancelled,
    /// Terminal: the engine reported a failure.
    Failed(Failure),
    /// Terminal: the worker went away without a terminal event, or broke the
    /// protocol and was killed. Nothing was published by this app's request,
    /// but an unfinished temporary file may remain.
    Lost(String),
}

/// Something that happened to a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// An event from the job's worker.
    Event(Event),
    /// The user pressed Cancel.
    CancelPressed,
    /// [`CANCEL_TIMEOUT`] passed since Cancel was sent.
    CancelTimedOut,
    /// The worker's event stream ended and the process was reaped.
    WorkerEnded {
        /// How it ended, for the user.
        detail: String,
    },
}

/// What the caller must do for the job's worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Nothing.
    None,
    /// Send `Cancel` and arm the [`CANCEL_TIMEOUT`] timer.
    SendCancel,
    /// The job is over: close the worker's input so it exits.
    CloseInput,
    /// Kill the worker; [`Input::WorkerEnded`] follows.
    Kill,
}

/// Which part of the view an input changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Changed {
    /// Nothing visible.
    Nothing,
    /// Only the counters and the progress bar.
    Counters,
    /// The phase or stage, and with it possibly everything.
    Phase,
}

/// The result of one [`Activity::apply`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    /// What to redraw.
    pub changed: Changed,
    /// What to do for the worker.
    pub effect: Effect,
}

impl Step {
    const IGNORED: Self = Self {
        changed: Changed::Nothing,
        effect: Effect::None,
    };

    const fn redraw(changed: Changed) -> Self {
        Self {
            changed,
            effect: Effect::None,
        }
    }

    const fn phase(effect: Effect) -> Self {
        Self {
            changed: Changed::Phase,
            effect,
        }
    }
}

/// One job: its phase, last counters and timing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activity {
    destination: PathBuf,
    total_frames: Option<u64>,
    /// The engine checks the source's real timestamps before the first
    /// frame, which can take a moment on a large file.
    checks_timing: bool,
    phase: Phase,
    /// The last stage reported, kept while cancelling.
    stage: Option<Stage>,
    progress: Progress,
    started: Instant,
    ended: Option<Instant>,
}

impl Activity {
    /// A job whose `Start` was just sent. `checks_timing` says the engine
    /// verifies the source's timestamps before encoding (sources read with
    /// `FFmpeg`).
    #[must_use]
    pub const fn new(
        destination: PathBuf,
        total_frames: Option<u64>,
        checks_timing: bool,
        now: Instant,
    ) -> Self {
        Self {
            destination,
            total_frames,
            checks_timing,
            phase: Phase::Starting,
            stage: None,
            progress: Progress {
                frames_read: 0,
                frames_encoded: 0,
                bytes_written: 0,
                frames_coded_milli: 0,
            },
            started: now,
            ended: None,
        }
    }

    /// Applies one input at time `now`.
    pub fn apply(&mut self, input: Input, now: Instant) -> Step {
        if self.is_terminal() {
            return Step::IGNORED;
        }
        let step = match input {
            Input::Event(event) => self.event(event),
            Input::CancelPressed => match self.phase {
                Phase::Starting | Phase::Running(_) => {
                    self.phase = Phase::Cancelling {
                        acknowledged: false,
                        killing: false,
                    };
                    Step::phase(Effect::SendCancel)
                }
                _ => Step::IGNORED,
            },
            Input::CancelTimedOut => match self.phase {
                Phase::Cancelling {
                    acknowledged,
                    killing: false,
                } => {
                    self.phase = Phase::Cancelling {
                        acknowledged,
                        killing: true,
                    };
                    Step::phase(Effect::Kill)
                }
                _ => Step::IGNORED,
            },
            Input::WorkerEnded { detail } => {
                let why = if matches!(self.phase, Phase::Cancelling { killing: true, .. }) {
                    format!(
                        "The worker did not stop within {} s of Cancel and was terminated ({detail}).",
                        CANCEL_TIMEOUT.as_secs()
                    )
                } else {
                    format!("The worker exited without finishing the job ({detail}).")
                };
                self.phase = Phase::Lost(why);
                Step::phase(Effect::None)
            }
        };
        if self.is_terminal() {
            self.ended = Some(now);
        }
        step
    }

    fn event(&mut self, event: Event) -> Step {
        match event {
            Event::Stage { stage } => {
                self.stage = Some(stage);
                if matches!(self.phase, Phase::Starting | Phase::Running(_)) {
                    self.phase = Phase::Running(stage);
                }
                Step::redraw(Changed::Phase)
            }
            Event::Progress { progress } if progress == self.progress => Step::IGNORED,
            Event::Progress { progress } => {
                self.progress = progress;
                Step::redraw(Changed::Counters)
            }
            Event::CancelRequested => {
                let killing = matches!(self.phase, Phase::Cancelling { killing: true, .. });
                self.phase = Phase::Cancelling {
                    acknowledged: true,
                    killing,
                };
                Step::redraw(Changed::Phase)
            }
            Event::Completed { report } => {
                self.progress.frames_encoded = report.frames;
                self.progress.frames_coded_milli = report.frames.saturating_mul(1000);
                self.progress.bytes_written = report.bytes;
                self.phase = Phase::Completed(report);
                Step::phase(Effect::CloseInput)
            }
            Event::Cancelled => {
                self.phase = Phase::Cancelled;
                Step::phase(Effect::CloseInput)
            }
            Event::Failed { failure } => {
                self.phase = Phase::Failed(failure);
                Step::phase(Effect::CloseInput)
            }
            // A refused command mid-job means the two ends disagree about the
            // job; do not guess what the worker is doing.
            Event::Error { message, .. } => {
                self.phase = Phase::Lost(format!(
                    "The engine refused a command ({message}) and the worker was stopped."
                ));
                Step::phase(Effect::Kill)
            }
            // Answers to commands this job never sends, the temporary file's
            // name, which the queue records, and live pictures, which the
            // preview shows.
            Event::Hello { .. }
            | Event::Probed { .. }
            | Event::Working { .. }
            | Event::Picture { .. }
            | Event::PictureUnavailable { .. }
            | Event::PlaybackEnded { .. } => Step::IGNORED,
        }
    }

    /// The phase.
    #[must_use]
    pub const fn phase(&self) -> &Phase {
        &self.phase
    }

    /// The last stage the engine reported.
    #[must_use]
    pub const fn stage(&self) -> Option<Stage> {
        self.stage
    }

    /// The latest counters.
    #[must_use]
    pub const fn progress(&self) -> Progress {
        self.progress
    }

    /// The source's frame count, when known.
    #[must_use]
    pub const fn total_frames(&self) -> Option<u64> {
        self.total_frames
    }

    /// Whether the engine checks the source's timestamps before encoding.
    #[must_use]
    pub const fn checks_timing(&self) -> bool {
        self.checks_timing
    }

    /// No frame has been read yet: the job is starting or checking the
    /// source, and only the elapsed time moves.
    #[must_use]
    pub const fn before_first_frame(&self) -> bool {
        self.progress.frames_read == 0 && self.progress.frames_encoded == 0
    }

    /// The requested destination.
    #[must_use]
    pub fn destination(&self) -> &Path {
        &self.destination
    }

    /// Whether the job is over.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(
            self.phase,
            Phase::Completed(_) | Phase::Cancelled | Phase::Failed(_) | Phase::Lost(_)
        )
    }

    /// Time since `Start`, frozen once the job is over.
    #[must_use]
    pub fn elapsed(&self, now: Instant) -> Duration {
        self.ended
            .unwrap_or(now)
            .saturating_duration_since(self.started)
    }

    /// Coded frames as a fraction of the total, only when the total is
    /// known and counts have arrived. Frames still inside the encoder count
    /// by their coded share, so the bar moves between packets. Before the
    /// first frame a share of zero would look frozen; the bar stays
    /// indeterminate instead.
    #[must_use]
    pub fn fraction(&self) -> Option<f32> {
        let total = self.total_frames.filter(|&t| t > 0)?;
        if self.before_first_frame() {
            return None;
        }
        let coded = self
            .progress
            .frames_coded_milli
            .max(self.progress.frames_encoded.saturating_mul(1000));
        let total = total.saturating_mul(1000);
        // Display precision only; counts stay exact everywhere else.
        #[allow(clippy::cast_precision_loss)]
        let fraction = coded.min(total) as f64 / total as f64;
        #[allow(clippy::cast_possible_truncation)]
        Some(fraction as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use studio_domain::job::{Check, CheckKind, CheckOutcome, FailureCode, WirePath};

    fn job(total: Option<u64>) -> (Activity, Instant) {
        let now = Instant::now();
        (
            Activity::new("/out/clip.ivf".into(), total, false, now),
            now,
        )
    }

    fn progress(read: u64, encoded: u64, bytes: u64) -> Input {
        Input::Event(Event::Progress {
            progress: Progress {
                frames_read: read,
                frames_encoded: encoded,
                bytes_written: bytes,
                frames_coded_milli: encoded * 1000,
            },
        })
    }

    fn stage(stage: Stage) -> Input {
        Input::Event(Event::Stage { stage })
    }

    fn report() -> JobReport {
        JobReport {
            destination: WirePath::Utf8("/out/clip.ivf".into()),
            frames: 90,
            bytes: 12_345,
            checks: vec![Check {
                kind: CheckKind::AvmDecode,
                outcome: CheckOutcome::NotRun {
                    reason: "AVM not available".into(),
                },
            }],
            disclosures: Vec::new(),
        }
    }

    fn completed() -> Input {
        Input::Event(Event::Completed { report: report() })
    }

    fn failed() -> Input {
        Input::Event(Event::Failed {
            failure: Failure {
                code: FailureCode::Output,
                message: "disk full".into(),
            },
        })
    }

    fn ended() -> Input {
        Input::WorkerEnded {
            detail: "exit status: 0".into(),
        }
    }

    /// Frames still inside the encoder move the bar between packets, and a
    /// count below the packets written cannot pull it back.
    #[test]
    fn the_bar_moves_with_frames_in_coding() {
        let (mut a, now) = job(Some(90));
        a.apply(stage(Stage::Encoding), now);
        let coding = |frames_coded_milli| {
            Input::Event(Event::Progress {
                progress: Progress {
                    frames_read: 50,
                    frames_encoded: 45,
                    bytes_written: 1000,
                    frames_coded_milli,
                },
            })
        };
        a.apply(coding(49_500), now);
        assert_eq!(a.fraction(), Some(0.55));
        a.apply(coding(0), now);
        assert_eq!(a.fraction(), Some(0.5));
    }

    #[test]
    fn a_normal_job_runs_through_its_stages_to_completion() {
        let (mut a, now) = job(Some(90));
        // No counts yet: indeterminate, not a frozen 0 %.
        assert_eq!(a.fraction(), None);
        assert!(a.before_first_frame());
        assert_eq!(a.apply(stage(Stage::Encoding), now).changed, Changed::Phase);
        assert_eq!(a.fraction(), None);
        assert_eq!(a.phase(), &Phase::Running(Stage::Encoding));
        assert_eq!(
            a.apply(progress(50, 45, 1000), now),
            Step::redraw(Changed::Counters)
        );
        assert_eq!(a.fraction(), Some(0.5));
        // An identical coalesced update redraws nothing.
        assert_eq!(a.apply(progress(50, 45, 1000), now), Step::IGNORED);
        for s in [Stage::Finalizing, Stage::Verifying, Stage::Publishing] {
            a.apply(stage(s), now);
            assert_eq!(a.phase(), &Phase::Running(s));
        }
        let later = now + Duration::from_secs(3);
        assert_eq!(a.apply(completed(), later), Step::phase(Effect::CloseInput));
        assert_eq!(a.phase(), &Phase::Completed(report()));
        assert_eq!(a.progress().frames_encoded, 90);
        // Elapsed time stops at the terminal event.
        assert_eq!(
            a.elapsed(later + Duration::from_secs(60)),
            Duration::from_secs(3)
        );
    }

    #[test]
    fn an_unknown_total_has_no_fraction() {
        let (mut a, now) = job(None);
        a.apply(progress(10, 5, 100), now);
        assert_eq!(a.fraction(), None);
        let (zero, _) = job(Some(0));
        assert_eq!(zero.fraction(), None);
    }

    #[test]
    fn the_fraction_never_exceeds_one() {
        let (mut a, now) = job(Some(10));
        a.apply(progress(20, 20, 1), now);
        assert_eq!(a.fraction(), Some(1.0));
    }

    #[test]
    fn cancel_is_pending_until_the_engine_says_cancelled() {
        let (mut a, now) = job(Some(90));
        a.apply(stage(Stage::Encoding), now);
        assert_eq!(
            a.apply(Input::CancelPressed, now),
            Step::phase(Effect::SendCancel)
        );
        // Pressing again sends nothing more.
        assert_eq!(a.apply(Input::CancelPressed, now), Step::IGNORED);
        a.apply(Input::Event(Event::CancelRequested), now);
        assert_eq!(
            a.phase(),
            &Phase::Cancelling {
                acknowledged: true,
                killing: false
            }
        );
        // Stages and counters still arrive while stopping; the phase stays.
        a.apply(stage(Stage::Finalizing), now);
        a.apply(progress(60, 50, 2000), now);
        assert!(matches!(a.phase(), Phase::Cancelling { .. }));
        assert_eq!(a.stage(), Some(Stage::Finalizing));
        assert_eq!(
            a.apply(Input::Event(Event::Cancelled), now),
            Step::phase(Effect::CloseInput)
        );
        assert_eq!(a.phase(), &Phase::Cancelled);
    }

    #[test]
    fn completion_that_races_cancel_is_reported_as_completed() {
        let (mut a, now) = job(Some(90));
        a.apply(stage(Stage::Publishing), now);
        a.apply(Input::CancelPressed, now);
        a.apply(completed(), now);
        assert_eq!(a.phase(), &Phase::Completed(report()));
        // The late acknowledgement and refusal of Cancel change nothing.
        assert_eq!(
            a.apply(Input::Event(Event::CancelRequested), now),
            Step::IGNORED
        );
        assert_eq!(a.apply(Input::CancelTimedOut, now), Step::IGNORED);
        assert_eq!(a.apply(ended(), now), Step::IGNORED);
        assert_eq!(a.phase(), &Phase::Completed(report()));
    }

    #[test]
    fn duplicate_or_conflicting_terminal_events_are_ignored() {
        let (mut a, now) = job(None);
        a.apply(failed(), now);
        for late in [
            completed(),
            Input::Event(Event::Cancelled),
            failed(),
            progress(1, 1, 1),
            stage(Stage::Encoding),
            Input::CancelPressed,
            ended(),
        ] {
            assert_eq!(a.apply(late.clone(), now), Step::IGNORED, "{late:?}");
        }
        assert!(matches!(a.phase(), Phase::Failed(f) if f.message == "disk full"));
    }

    #[test]
    fn a_worker_that_dies_mid_job_is_lost_not_completed() {
        let (mut a, now) = job(Some(90));
        a.apply(stage(Stage::Encoding), now);
        a.apply(progress(90, 90, 5000), now);
        assert_eq!(a.apply(ended(), now), Step::phase(Effect::None));
        assert!(matches!(a.phase(), Phase::Lost(why) if why.contains("without finishing")));
        assert!(a.is_terminal());
    }

    #[test]
    fn a_worker_dying_before_any_event_is_lost() {
        let (mut a, now) = job(None);
        a.apply(ended(), now);
        assert!(matches!(a.phase(), Phase::Lost(_)));
    }

    #[test]
    fn an_unresponsive_cancel_escalates_to_a_kill() {
        let (mut a, now) = job(Some(90));
        a.apply(stage(Stage::Encoding), now);
        a.apply(Input::CancelPressed, now);
        assert_eq!(
            a.apply(Input::CancelTimedOut, now),
            Step::phase(Effect::Kill)
        );
        // A second timeout does not kill twice.
        assert_eq!(a.apply(Input::CancelTimedOut, now), Step::IGNORED);
        // The kill ends the stream; that is Lost, with the reason.
        a.apply(ended(), now);
        assert!(matches!(a.phase(), Phase::Lost(why) if why.contains("did not stop")));
    }

    #[test]
    fn a_terminal_event_that_beats_the_kill_wins() {
        let (mut a, now) = job(Some(90));
        a.apply(Input::CancelPressed, now);
        a.apply(Input::CancelTimedOut, now);
        a.apply(Input::Event(Event::Cancelled), now);
        assert_eq!(a.phase(), &Phase::Cancelled);
        assert_eq!(a.apply(ended(), now), Step::IGNORED);
    }

    #[test]
    fn a_timeout_without_cancel_does_nothing() {
        let (mut a, now) = job(None);
        assert_eq!(a.apply(Input::CancelTimedOut, now), Step::IGNORED);
        assert_eq!(a.phase(), &Phase::Starting);
    }

    #[test]
    fn a_refused_command_stops_the_worker() {
        let (mut a, now) = job(None);
        let refused = Input::Event(Event::Error {
            code: studio_domain::protocol::ErrorCode::WrongState,
            message: "a job is already running".into(),
        });
        assert_eq!(a.apply(refused, now), Step::phase(Effect::Kill));
        assert!(matches!(a.phase(), Phase::Lost(_)));
    }

    #[test]
    fn cancel_before_the_first_stage_is_allowed() {
        let (mut a, now) = job(None);
        assert_eq!(
            a.apply(Input::CancelPressed, now),
            Step::phase(Effect::SendCancel)
        );
        a.apply(Input::Event(Event::Cancelled), now);
        assert_eq!(a.phase(), &Phase::Cancelled);
    }
}
