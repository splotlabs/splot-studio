// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! One encode job, from source to published file.
//!
//! [`run`] is synchronous and owns the encoder for its whole life. It reports
//! through a callback, also from a ticker thread while a codec call runs, so
//! the counters move inside a frame. It checks its [`Control`]'s cancel flag before every
//! frame read, between packets, and inside the scan and verification; it
//! hands the encoder's [`CancelToken`] to the [`Control`], which stops a
//! codec call already running. Publication must be
//! claimed from the [`Control`]; once claimed, a cancel is too late and the
//! job completes, and once a cancel is accepted, the claim is refused.

use std::fs::File;
use std::io::{self, BufWriter};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use splot_encode::{CancelToken, Config, ContentDescription, Encoder, PixelFormat, SourceFrame};
use studio_domain::job::{
    Check, CheckOutcome, Failure, FailureCode, JobReport, JobSpec, Progress, SourceReader, Stage,
    WirePath,
};
use studio_domain::options::{FieldError, OPTIONS_SCHEMA_VERSION};
use studio_domain::protocol::{Event, OptionsIdentity};
use studio_domain::settings::{EncoderSettings, OutputKind};
use thiserror::Error;

use crate::output::{self, PacketSink};
use crate::source::{self, Selection, Source};
use crate::{live, options, picture, verify};
use splot_encode::y4m::Y4mHeader;

/// The stack of a job thread. The decode driver used by verification
/// recurses through the partition walk; upstream sizes its own worker and
/// CLI threads the same way (`splot-parallel`, `WORKER_STACK_SIZE_BYTES`).
pub const JOB_STACK_BYTES: usize = 16 * 1024 * 1024;

/// How often the encoder's own frame count is read while a codec call runs.
/// A frame can take seconds, and a bar that moves four times a second reads
/// as alive.
const CODED_INTERVAL: Duration = Duration::from_millis(250);

/// Why a job or probe stopped early.
#[derive(Debug, Error)]
pub enum Stop {
    /// The job was cancelled.
    #[error("cancelled")]
    Cancelled,
    /// The job or probe failed.
    #[error("{}", .0.message)]
    Failed(Failure),
}

impl Stop {
    /// A failure with `code`.
    pub fn failed(code: FailureCode, message: impl Into<String>) -> Self {
        Self::Failed(Failure {
            code,
            message: message.into(),
        })
    }
}

/// How a job is stopped, and what someone stopping it from outside must know.
///
/// A plain [`AtomicBool`] is a `Control` that nobody can force: the job
/// stops only by itself, and a flag set during a codec call is seen when
/// that call returns.
pub trait Control: Sync {
    /// Set once the job should stop.
    fn cancel_flag(&self) -> &AtomicBool;

    /// The encoder exists, before its first call; `token` stops a call in
    /// progress. A `Control` that sets its flag from another thread cancels
    /// the token with it, and at once if the flag is already set.
    fn hold_encoder(&self, token: CancelToken) {
        drop(token);
    }

    /// Runs `create`, which makes the job's temporary file and returns its
    /// path, so the file can be removed if the job is abandoned.
    fn hold_temp(&self, create: &mut dyn FnMut() -> Option<PathBuf>) {
        create();
    }

    /// The temporary file is gone: removed, or published.
    fn release_temp(&self) {}

    /// Claims publication. `false` means a cancel was accepted first.
    fn claim_publish(&self) -> bool {
        !self.cancel_flag().load(Ordering::Relaxed)
    }

    /// Encoding starts: what live pictures need to know of the stream.
    fn stream_started(&self, stream: live::Stream) {
        let _ = stream;
    }

    /// A packet's temporal unit was written to the output. Must return at
    /// once and only read `unit`.
    fn packet_written(&self, unit: &[u8]) {
        let _ = unit;
    }
}

impl Control for AtomicBool {
    fn cancel_flag(&self) -> &AtomicBool {
        self
    }
}

/// Runs `job` to its terminal event: [`Event::Completed`],
/// [`Event::Cancelled`] or [`Event::Failed`]. Every other event goes to
/// `report` as it happens.
pub fn run(job: &JobSpec, control: &dyn Control, report: &mut (dyn FnMut(Event) + Send)) -> Event {
    let end = execute(job, control, report);
    // `execute` has dropped or published the temporary file by now.
    control.release_temp();
    match end {
        Ok(report) => Event::Completed { report },
        Err(Stop::Cancelled) => Event::Cancelled,
        Err(Stop::Failed(failure)) => Event::Failed { failure },
    }
}

/// Starts `body` on a thread with [`JOB_STACK_BYTES`] of stack.
///
/// # Errors
/// When the thread cannot start.
pub fn spawn<T: Send + 'static>(
    body: impl FnOnce() -> T + Send + 'static,
) -> io::Result<JoinHandle<T>> {
    thread::Builder::new()
        .name("studio-job".to_owned())
        .stack_size(JOB_STACK_BYTES)
        .spawn(body)
}

fn execute(
    job: &JobSpec,
    control: &dyn Control,
    report: &mut (dyn FnMut(Event) + Send),
) -> Result<JobReport, Stop> {
    let cancel = control.cancel_flag();
    let source_path = job
        .source
        .to_path_buf()
        .map_err(|e| Stop::failed(FailureCode::InvalidSource, e.to_string()))?;
    let destination = job
        .destination
        .to_path_buf()
        .map_err(|e| output::output(e.to_string()))?;
    let mut source = source::open(
        &source_path,
        Selection {
            fingerprint: Some(job.fingerprint),
            video_stream: Some(job.video_stream),
        },
    )?;
    if source.facts.omissions.any() && !job.omissions_acknowledged {
        return Err(Stop::failed(
            FailureCode::OmissionsNotAcknowledged,
            "the output is video only, and the job does not acknowledge the streams and \
             chapters it leaves out",
        ));
    }
    validate_policy_request(job)?;
    let config = source_config(&source.header, source.content, job.settings.base)?;
    let (config, policy) =
        options::job_policy(config, &source.facts, &job.settings).map_err(invalid_policy)?;
    let output_kind = job.settings.base.output;
    let coded = config.coded();
    if output_kind == OutputKind::Ivf
        && (u16::try_from(coded.width).is_err() || u16::try_from(coded.height).is_err())
    {
        return Err(Stop::failed(
            FailureCode::TooLarge,
            format!(
                "IVF holds at most 65535 samples per side, not {}x{}",
                coded.width, coded.height
            ),
        ));
    }
    output::check_destination(&destination, &source.id)?;
    // A decoded source is scanned first; that can take a while and writes
    // nothing.
    if source.facts.reader == SourceReader::Ffmpeg {
        report(Event::Stage {
            stage: Stage::Checking,
        });
    }
    source.start(cancel)?;
    for event in policy {
        stop_if(cancel)?;
        report(event);
    }
    control.stream_started(live::Stream {
        basis: picture::basis(
            source.matrix,
            source.header.format == PixelFormat::Monochrome,
            source.header.width,
            source.header.height,
        ),
        frame_rate: source.header.frame_rate,
        frame_count: source.frame_count(),
    });
    report(Event::Stage {
        stage: Stage::Encoding,
    });
    // Cancelled before the file exists: there is nothing to announce or remove.
    stop_if(cancel)?;
    let mut prepared = None;
    control.hold_temp(&mut || {
        let made = output::prepare(&destination, &source.id);
        let path = made.as_ref().ok().map(|temp| temp.path().to_path_buf());
        prepared = Some(made);
        path
    });
    let temp =
        prepared.unwrap_or_else(|| Err(output::output("the temporary file was not created")))?;
    report(Event::Working {
        partial: WirePath::from_path(temp.path()),
    });
    let frames = encode(
        &mut source,
        &config,
        output_kind,
        temp.as_file(),
        control,
        report,
    )?;
    report(Event::Stage {
        stage: Stage::Verifying,
    });
    let expected = verify::Expected {
        config: &config,
        kind: output_kind,
        frames,
    };
    let checks = checked(temp.as_file(), expected, cancel)?;
    if !control.claim_publish() {
        return Err(Stop::Cancelled);
    }
    report(Event::Stage {
        stage: Stage::Publishing,
    });
    let bytes = output::publish(temp, &destination)?;
    Ok(JobReport {
        destination: job.destination.clone(),
        frames,
        bytes,
        checks,
        disclosures: source.facts.disclosures,
    })
}

/// Identifies the encoder option schema and codec used by this engine.
#[must_use]
pub fn options_identity() -> OptionsIdentity {
    OptionsIdentity {
        schema: OPTIONS_SCHEMA_VERSION,
        codec_revision: env!("SPLOT_CODEC_REVISION").into(),
    }
}

fn validate_policy_request(job: &JobSpec) -> Result<(), Stop> {
    match &job.identity {
        Some(identity) if *identity == options_identity() => {}
        None if job.settings.values.is_empty() && job.pass_artifact.is_none() => {}
        _ => {
            return Err(invalid_policy(FieldError::new(
                "identity",
                "Use the option schema and codec revision reported by this engine.",
            )));
        }
    }
    if job.pass_artifact.is_some() {
        return Err(invalid_policy(FieldError::new(
            "pass_artifact",
            "This engine supports one-pass encoding. Remove the first-pass artifact.",
        )));
    }
    Ok(())
}

fn invalid_policy(error: FieldError) -> Stop {
    Stop::failed(
        FailureCode::InvalidSettings,
        format!("{}: {}", error.field, error.message),
    )
}

/// Runs the verification checks on the finished file.
///
/// # Errors
/// [`Stop::Cancelled`] when cancelled meanwhile, or
/// [`FailureCode::VerificationFailed`] naming every check that failed.
fn checked(
    file: &File,
    expected: verify::Expected<'_>,
    cancel: &AtomicBool,
) -> Result<Vec<Check>, Stop> {
    let checks = verify::run(file, expected, cancel)?;
    let failures: Vec<String> = checks
        .iter()
        .filter_map(|check| match &check.outcome {
            CheckOutcome::Failed { detail } => Some(format!("{:?}: {detail}", check.kind)),
            _ => None,
        })
        .collect();
    if failures.is_empty() {
        Ok(checks)
    } else {
        Err(Stop::failed(
            FailureCode::VerificationFailed,
            failures.join("; "),
        ))
    }
}

/// The encoder configuration of a job on a source with `header`: the
/// source's geometry, depth, chroma, rational frame rate and `content`
/// (the colour description the output carries), constant
/// quality at `settings.crf`, and codec defaults for everything else.
/// Built once per job and validated by the codec.
///
/// # Errors
/// [`FailureCode::InvalidSettings`] when the codec rejects it, or
/// [`FailureCode::UnsupportedSource`] for a depth other than 8 or 10.
pub fn config(
    header: &Y4mHeader,
    content: ContentDescription,
    settings: EncoderSettings,
) -> Result<Config, Stop> {
    let config = source_config(header, content, settings)?;
    config
        .validate()
        .map_err(|error| Stop::failed(FailureCode::InvalidSettings, error.to_string()))?;
    Ok(config)
}

fn source_config(
    header: &Y4mHeader,
    content: ContentDescription,
    settings: EncoderSettings,
) -> Result<Config, Stop> {
    options::config::from_header(header, content, settings).map_err(|error| {
        if error.field == "bit_depth" {
            Stop::failed(FailureCode::UnsupportedSource, error.message)
        } else {
            invalid_policy(error)
        }
    })
}

/// The counters of an encode and where they go. The encode loop and the
/// ticker that reads the encoder's frame count share one.
struct Reporter<'a> {
    progress: Progress,
    coded: splot_encode::Progress,
    report: &'a mut (dyn FnMut(Event) + Send),
}

impl Reporter<'_> {
    /// The encoder's frame count in thousandths. The codec steps back by a
    /// frame when it codes one again; the reported count does not.
    fn coded_milli(&self) -> u64 {
        // A frame count times 1000 is far inside `u64`, and never negative.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let milli = (self.coded.frames() * 1000.0) as u64;
        milli.max(self.progress.frames_coded_milli)
    }

    fn emit(&mut self) {
        self.progress.frames_coded_milli = self.coded_milli();
        (self.report)(Event::Progress {
            progress: self.progress,
        });
    }
}

fn lock<'a, 'b>(reporter: &'a Mutex<Reporter<'b>>) -> MutexGuard<'a, Reporter<'b>> {
    reporter.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Reports the encoder's frame count whenever it has moved, until `stopped`
/// closes. Sleeps between readings.
fn tick(reporter: &Mutex<Reporter<'_>>, stopped: &mpsc::Receiver<()>) {
    while stopped.recv_timeout(CODED_INTERVAL) == Err(RecvTimeoutError::Timeout) {
        let mut reporter = lock(reporter);
        if reporter.coded_milli() > reporter.progress.frames_coded_milli {
            reporter.emit();
        }
    }
}

/// Streams every source frame through the encoder into `file` and returns
/// the frame count. Frames get consecutive zero-based PTS in the source
/// frame rate's time base.
fn encode(
    source: &mut Source,
    config: &Config,
    kind: OutputKind,
    file: &File,
    control: &dyn Control,
    report: &mut (dyn FnMut(Event) + Send),
) -> Result<u64, Stop> {
    let output_error = |e: io::Error| output::output(format!("cannot write the output: {e}"));
    let mut encoder = Encoder::new(config.clone()).map_err(encoder_error)?;
    control.hold_encoder(encoder.cancel_token());
    let mut sink = PacketSink::new(kind, BufWriter::new(file), config).map_err(output_error)?;
    let reporter = Mutex::new(Reporter {
        progress: Progress::default(),
        coded: encoder.progress(),
        report,
    });
    let (stop, stopped) = mpsc::channel();
    let frames = thread::scope(|scope| {
        let reporter = &reporter;
        // Without the ticker the counters still move with every frame and packet.
        let _ = thread::Builder::new()
            .name("studio-progress".to_owned())
            .spawn_scoped(scope, move || tick(reporter, &stopped));
        let frames = encode_frames(source, config, &mut encoder, &mut sink, control, reporter);
        drop(stop);
        frames
    })?;
    sink.finish()
        .and_then(|writer| writer.into_inner().map_err(io::IntoInnerError::into_error))
        .and_then(File::sync_all)
        .map_err(output_error)?;
    Ok(frames)
}

/// The encode loop: reads every frame, drains the encoder, and returns the
/// frame count once the encoder has returned a packet for each.
fn encode_frames(
    source: &mut Source,
    config: &Config,
    encoder: &mut Encoder,
    sink: &mut PacketSink<BufWriter<&File>>,
    control: &dyn Control,
    reporter: &Mutex<Reporter<'_>>,
) -> Result<u64, Stop> {
    let cancel = control.cancel_flag();
    let mut frames_read = 0;
    loop {
        stop_if(cancel)?;
        let Some(planes) = source.read_frame()? else {
            break;
        };
        let frame = SourceFrame::new(config, frames_read, planes).map_err(|e| {
            Stop::failed(
                FailureCode::InvalidSource,
                format!("frame {frames_read}: {e}"),
            )
        })?;
        encoder.send_frame(frame).map_err(encoder_error)?;
        frames_read += 1;
        {
            let mut reporter = lock(reporter);
            reporter.progress.frames_read = frames_read;
            reporter.emit();
        }
        drain(encoder, sink, control, reporter)?;
    }
    if frames_read == 0 {
        return Err(Stop::failed(
            FailureCode::InvalidSource,
            "the source has no frames",
        ));
    }
    (lock(reporter).report)(Event::Stage {
        stage: Stage::Finalizing,
    });
    encoder.flush().map_err(encoder_error)?;
    drain(encoder, sink, control, reporter)?;
    let frames_encoded = lock(reporter).progress.frames_encoded;
    if frames_encoded != frames_read {
        return Err(Stop::failed(
            FailureCode::Encoder,
            format!("the encoder returned {frames_encoded} packets for {frames_read} frames"),
        ));
    }
    Ok(frames_read)
}

/// Writes every packet the encoder has ready.
fn drain(
    encoder: &mut Encoder,
    sink: &mut PacketSink<BufWriter<&File>>,
    control: &dyn Control,
    reporter: &Mutex<Reporter<'_>>,
) -> Result<(), Stop> {
    let cancel = control.cancel_flag();
    while let Some(packet) = encoder.receive_packet().map_err(encoder_error)? {
        sink.write(&packet)
            .map_err(|e| output::output(format!("cannot write the output: {e}")))?;
        control.packet_written(&packet.data);
        {
            let mut reporter = lock(reporter);
            reporter.progress.frames_encoded += 1;
            reporter.progress.bytes_written = sink.bytes();
            reporter.emit();
        }
        stop_if(cancel)?;
    }
    Ok(())
}

fn stop_if(cancel: &AtomicBool) -> Result<(), Stop> {
    if cancel.load(Ordering::Relaxed) {
        Err(Stop::Cancelled)
    } else {
        Ok(())
    }
}

#[allow(clippy::needless_pass_by_value)]
fn encoder_error(error: splot_encode::Error) -> Stop {
    match error {
        // Only the job's `Control` cancels the token, after setting its flag.
        splot_encode::Error::Cancelled => Stop::Cancelled,
        error => Stop::failed(FailureCode::Encoder, error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use studio_domain::options::{OptionRequest, OptionValue};

    use super::*;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn job() -> TestResult<(tempfile::TempDir, JobSpec)> {
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("input.y4m");
        let mut bytes = b"YUV4MPEG2 W64 H48 F30:1 Ip C420\nFRAME\n".to_vec();
        bytes.resize(bytes.len() + 64 * 48 * 3 / 2, 128);
        fs::write(&source, bytes)?;
        let facts = source::probe(&source, None)?;
        let spec = JobSpec {
            source: WirePath::from_path(&source),
            fingerprint: facts.fingerprint,
            video_stream: facts.video_stream,
            omissions_acknowledged: false,
            destination: WirePath::from_path(&dir.path().join("output.ivf")),
            settings: OptionRequest {
                base: EncoderSettings {
                    output: OutputKind::Ivf,
                    preset: studio_domain::settings::SpeedPreset::new(10)?,
                    crf: crate::cli::DEFAULT_CRF,
                },
                values: BTreeMap::new(),
            },
            identity: Some(options_identity()),
            pass_artifact: None,
        };
        Ok((dir, spec))
    }

    #[test]
    fn emits_actual_policy_before_working() -> TestResult {
        let (_dir, mut spec) = job()?;
        let rate = OptionValue::Record(BTreeMap::from([
            ("mode".into(), OptionValue::Choice("constant_qp".into())),
            ("qp".into(), OptionValue::Integer(0)),
        ]));
        spec.settings
            .values
            .insert("rate_control".into(), rate.clone());
        let mut events = Vec::new();
        let end = run(&spec, &AtomicBool::new(false), &mut |event| {
            events.push(event)
        });
        assert!(matches!(end, Event::Completed { .. }), "{end:?}");

        let mut next = 0;
        let mut total = None;
        let mut found_rate = false;
        for event in events {
            match event {
                Event::JobPolicy {
                    first,
                    total: count,
                    values,
                } => {
                    assert_eq!(first, next);
                    assert_eq!(*total.get_or_insert(count), count);
                    next += u32::try_from(values.len())?;
                    found_rate |= values
                        .iter()
                        .any(|option| option.key == "rate_control" && option.value == rate);
                }
                Event::Working { .. } => {
                    assert!(found_rate);
                    assert_eq!(total, Some(next));
                    return Ok(());
                }
                _ => {}
            }
        }
        Err("The job produced no Working event.".into())
    }

    #[test]
    fn rejects_stale_identity_pass_artifact_and_invalid_option_before_output() -> TestResult {
        let (dir, spec) = job()?;
        for field in ["identity", "pass_artifact", "rate_control.qp"] {
            let mut spec = spec.clone();
            match field {
                "identity" => {
                    let mut stale = options_identity();
                    stale.codec_revision.push_str("-old");
                    spec.identity = Some(stale);
                }
                "pass_artifact" => spec.pass_artifact = Some(spec.source.clone()),
                _ => {
                    spec.settings.values.insert(
                        "rate_control".into(),
                        OptionValue::Record(BTreeMap::from([
                            ("mode".into(), OptionValue::Choice("constant_qp".into())),
                            ("qp".into(), OptionValue::Integer(256)),
                        ])),
                    );
                }
            }
            let mut events = Vec::new();
            let end = run(&spec, &AtomicBool::new(false), &mut |event| {
                events.push(event)
            });
            assert!(
                matches!(
                    &end,
                    Event::Failed { failure } if failure.code == FailureCode::InvalidSettings
                        && failure.message.starts_with(field)
                ),
                "{end:?}"
            );
            assert!(events.is_empty());
            assert_eq!(dir.path().read_dir()?.count(), 1);
        }
        Ok(())
    }
}
