// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The control protocol between the app and the engine.
//!
//! A frame is a little-endian `u32` byte length followed by that many bytes of
//! UTF-8 JSON holding one [`Envelope`]. Pixels never travel inside JSON: an
//! [`Event::Picture`] frame is followed immediately by its raw pixel bytes,
//! whose length the header fixes and [`MAX_PICTURE_BYTES`] bounds. Use
//! [`write_picture`] and [`read_event`] for the engine-to-app direction.
//! The length is checked against [`MAX_MESSAGE_BYTES`] before any allocation,
//! and the protocol version before the body is interpreted, so a mismatched
//! app/engine pair fails with [`ProtocolError::IncompatibleVersion`] instead
//! of a confusing parse error.
//!
//! Three engine processes speak it: a job worker, the preview process and
//! the inspect process, which serves the [`crate::analysis`] commands.

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::analysis::{AnalysisFailure, DocumentSummary, Framing, LocateTarget, Row, RowFilter};
use crate::export::{
    ExportArtifact, ExportOutcome, ExportProgress, ExportRequest, ExportStage, ExportTemp,
};
use crate::job::{Failure, JobReport, JobSpec, Progress, SourceFacts, Stage, WirePath};
use crate::options::{EffectiveOption, FieldError, OptionDescriptor, OptionRequest};
use crate::preview::{
    GrainPolicy, MAX_PICTURE_BYTES, PictureBox, PictureFailure, PictureFrame, PictureHeader,
    PictureOptions, PictureStage, PictureTarget, PictureView, SamplePoint,
};
use crate::settings::OutputKind;
use crate::syntax::{SyntaxEvent, SyntaxSummary};
use crate::validation::{
    BatchOutcome, Finding, ReportFormat, ValidationSettings, ValidationSummary,
};

/// The protocol version this build speaks. Bump on any incompatible change.
pub const PROTOCOL_VERSION: u32 = 17;

/// The largest accepted message body. Control messages are small.
pub const MAX_MESSAGE_BYTES: u32 = 64 * 1024;

/// A framing or decoding failure.
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// The underlying stream failed.
    #[error("protocol stream failed: {0}")]
    Io(#[from] io::Error),
    /// The stream ended inside a frame.
    #[error("protocol stream ended inside a message")]
    Truncated,
    /// The declared length is zero or exceeds [`MAX_MESSAGE_BYTES`].
    #[error("message length {len} is outside 1..={MAX_MESSAGE_BYTES} bytes")]
    InvalidLength {
        /// The declared body length.
        len: u32,
    },
    /// The peer speaks another protocol version.
    #[error("peer speaks protocol version {theirs}, this build speaks {PROTOCOL_VERSION}")]
    IncompatibleVersion {
        /// The version the peer sent.
        theirs: u32,
    },
    /// A picture header's size is out of range or disagrees with its pixels.
    #[error("picture is {width}x{height} with {bytes} bytes of pixels")]
    InvalidPicture {
        /// The declared width.
        width: u16,
        /// The declared height.
        height: u16,
        /// The pixel bytes offered, or `0` when none were.
        bytes: usize,
    },
    /// The body is not a message this build understands.
    #[error("malformed or unknown message: {0}")]
    Malformed(#[from] serde_json::Error),
}

/// Who built one end of the connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildIdentity {
    /// The executable name, for example `splot-studio-engine`.
    pub name: String,
    /// The package version of that executable.
    pub version: String,
}

/// What an engine build can actually do. Empty lists mean "not implemented".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// The output kinds this engine can encode.
    pub encode_outputs: Vec<OutputKind>,
    /// The `FFmpeg` this engine reads ordinary media files with; `None` means
    /// only Y4M can be read.
    pub media_tool: Option<MediaTool>,
    /// The framings the inspect process reads.
    pub analysis_framings: Vec<Framing>,
    /// Syntax and validation schemas available from the inspect process.
    pub analysis_evidence: Option<AnalysisEvidenceIdentity>,
    /// The encoder option schema available from the job worker.
    pub options: Option<OptionsIdentity>,
}

/// Identifies the inspect process's syntax and validation evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalysisEvidenceIdentity {
    /// The syntax schema.
    pub syntax_schema: u16,
    /// The validation schema.
    pub validation_schema: u16,
    /// The full Git revision of Splot in this engine.
    pub codec_revision: String,
}

/// Identifies the job worker's encoder option metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptionsIdentity {
    /// The schema version used by the option commands.
    pub schema: u32,
    /// The full Git revision of the encoder in this engine.
    pub codec_revision: String,
}

/// Where the engine found its `FFmpeg`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaToolOrigin {
    /// Shipped beside the engine executable.
    Bundled,
    /// A development opt-in to whatever is installed on this machine.
    System,
}

/// The identity of the `FFmpeg` executables the engine runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaTool {
    /// The version string the tool reports.
    pub version: String,
    /// Where it was found.
    pub origin: MediaToolOrigin,
}

/// A request from the app to the engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    /// Opens a session and asks for the engine's identity and capabilities.
    Hello {
        /// The app build.
        client: BuildIdentity,
    },
    /// Job worker only. Lists encoder option descriptors in schema order.
    DescribeOptions {
        /// The schema version from the handshake.
        schema: u32,
        /// The first descriptor index, starting at zero.
        first: u32,
        /// One through [`crate::options::MAX_OPTIONS_PAGE_ITEMS`] descriptors.
        count: u32,
    },
    /// Job worker only. Validates settings and lists their effective values.
    /// The reply can arrive after replies to later commands. The worker
    /// returns [`ErrorCode::Busy`] if one request is already waiting.
    ResolveOptions {
        /// The schema version from the handshake.
        schema: u32,
        /// The source facts returned by a probe.
        source: Box<SourceFacts>,
        /// The requested encoder settings.
        settings: OptionRequest,
        /// The first effective value index, starting at zero.
        first: u32,
        /// One through [`crate::options::MAX_OPTIONS_PAGE_ITEMS`] values.
        count: u32,
    },
    /// Reads a source's header and stream inventory. Answered by
    /// [`Event::Probed`] or [`Event::Failed`]. Nothing is decoded or written.
    Probe {
        /// The file to inspect.
        source: WirePath,
        /// The video stream to describe; `None` picks the first one that is
        /// not an attached picture.
        video_stream: Option<u32>,
    },
    /// Starts the one job this worker process runs. Followed by
    /// [`Event::Stage`] and [`Event::Progress`] events and exactly one of
    /// [`Event::Completed`], [`Event::Cancelled`] or [`Event::Failed`].
    Start {
        /// The job.
        job: JobSpec,
        /// Ask for [`Event::Picture`]s of the output so far, no larger than
        /// this box. They never delay the job: the engine sends the newest
        /// picture it has when the app can take one and skips the rest.
        /// An invalid box does not refuse the job: it runs without pictures
        /// after one [`Event::PictureUnavailable`] with generation `0`.
        live: Option<PictureBox>,
    },
    /// Starts a job that waits for confirmation of its complete policy before writing.
    StartChecked {
        /// The job.
        job: JobSpec,
        /// Optional live picture size, as in [`Self::Start`].
        live: Option<PictureBox>,
    },
    /// Confirms the complete policy from [`Event::JobPolicy`] on the same request ID.
    /// The worker resumes the job without a separate acknowledgement.
    ConfirmPolicy,
    /// Worker only. Prepare to export decoded samples or hashes.
    ExportPrepare {
        /// The immutable export request.
        export: Box<ExportRequest>,
    },
    /// Worker only. Call after saving the paths from [`Event::ExportPrepared`].
    /// Use the request ID of [`Self::ExportPrepare`].
    ExportProceed,
    /// Asks the running job to stop. [`Event::CancelRequested`] acknowledges
    /// the request; only [`Event::Cancelled`] means the job has stopped.
    /// Closing the worker's input has the same effect.
    /// The inspect process refuses it: [`Command::AnalysisCancel`] names the
    /// document.
    Cancel,
    /// Preview process only. Asks for one picture. Answered by one
    /// [`Event::Picture`] or [`Event::PictureUnavailable`] with the same
    /// generation, unless a newer `Picture` or `Play` arrives first: then the
    /// engine may drop this request without answering. Stops playback.
    Picture {
        /// Chosen by the app and increasing; echoed on the answer.
        generation: u64,
        /// What to show.
        target: PictureTarget,
        /// Which frame.
        frame: PictureFrame,
        /// Which part of it.
        view: PictureView,
        /// Display surface and film grain policy.
        #[serde(default)]
        options: PictureOptions,
        /// The largest picture wanted.
        size: PictureBox,
    },
    /// Preview process only. Plays from `from` at the stream's own frame
    /// timing: a run of [`Event::Picture`]s with this generation, each sent
    /// when its frame is due, late frames skipped, ended by
    /// [`Event::PlaybackEnded`] or [`Event::PictureUnavailable`]. Video only.
    /// Any newer `Picture`, `Play` or `StopPlayback` ends it, and that end is
    /// still reported by [`Event::PlaybackEnded`] with this generation.
    Play {
        /// Chosen by the app and increasing; echoed on every answer.
        generation: u64,
        /// What to play.
        target: PictureTarget,
        /// The first frame, zero-based.
        from: u64,
        /// Which part of each frame.
        view: PictureView,
        /// Display surface and film grain policy.
        #[serde(default)]
        options: PictureOptions,
        /// The largest picture wanted.
        size: PictureBox,
    },
    /// Preview process only. Reads one document sample. Stops playback.
    PictureSample {
        /// Chosen by the app and echoed on the answer.
        generation: u64,
        /// The inspected document.
        target: PictureTarget,
        /// The decoder output ordinal.
        ordinal: u64,
        /// The same film grain policy as the displayed picture.
        grain: GrainPolicy,
        /// The visible coordinate to inspect.
        point: SamplePoint,
    },
    /// Preview process only. Stops playback; answered by
    /// [`Event::PlaybackEnded`] when something was playing.
    StopPlayback,
    /// Preview process only. Release the file and pictures for this document.
    CloseDocumentPlayback {
        /// The ID of the document to close.
        document: u64,
    },
    /// Inspect process only. Opens `source` read-only and reads all of its
    /// framing once. Answered by [`Event::AnalysisOpened`] or
    /// [`Event::AnalysisFailed`]. Requests are served one at a time, in
    /// order, so a page asked for after this open waits for it.
    AnalysisOpen {
        /// Chosen by the app; greater than every id used before in this
        /// process.
        document: u64,
        /// The file. It is never modified.
        source: WirePath,
    },
    /// Inspect process only. Stops the [`Command::AnalysisOpen`] of
    /// `document` if it is not answered yet; other documents are not
    /// affected. Answered by [`Event::CancelRequested`], after which reading
    /// stops at the next unit and the open is answered with what it read: an
    /// [`Event::AnalysisOpened`] whose outcome is stopped by
    /// [`crate::analysis::ProblemKind::Cancelled`] (the whole document when
    /// reading ended first), [`AnalysisFailure::Cancelled`] when nothing was
    /// read yet, or the failure the open met anyway. Refused with
    /// [`ErrorCode::WrongState`] when the open was already answered or never
    /// sent.
    AnalysisCancel {
        /// The document whose open to stop.
        document: u64,
    },
    /// Inspect process only. Asks for rows `first..first + count` of the
    /// elements `filter` admits, in file order. Answered by
    /// [`Event::AnalysisPage`] or [`Event::AnalysisFailed`].
    AnalysisPage {
        /// The document.
        document: u64,
        /// Which elements are rows.
        filter: RowFilter,
        /// The first row: below the number of rows, or `0` when there are
        /// none.
        first: u64,
        /// At most [`crate::analysis::MAX_PAGE_ELEMENTS`], at least one.
        count: u32,
    },
    /// Inspect process only. Finds an element and its row among those
    /// `filter` admits. Answered by [`Event::AnalysisLocated`] or
    /// [`Event::AnalysisFailed`].
    AnalysisLocate {
        /// The document.
        document: u64,
        /// Which elements are rows.
        filter: RowFilter,
        /// A file byte, or an element index.
        target: LocateTarget,
    },
    /// Inspect process only. Asks for the file's bytes
    /// `offset..offset + len`, which must lie inside the file. Answered by
    /// [`Event::AnalysisBytes`] or [`Event::AnalysisFailed`].
    AnalysisBytes {
        /// The document.
        document: u64,
        /// The first byte.
        offset: u64,
        /// At most [`crate::analysis::MAX_BYTE_RANGE`], at least one.
        len: u32,
    },
    /// Inspect process only. Collects syntax for one indexed OBU.
    /// Use increasing request IDs for successive traces of a document.
    AnalysisSyntax {
        /// The document generation.
        document: u64,
        /// The unfiltered index of the OBU element.
        element: u64,
    },
    /// Inspect process only. Reads retained events from a syntax trace.
    AnalysisSyntaxPage {
        /// The document generation.
        document: u64,
        /// The request ID of the latest successful syntax request.
        trace: u64,
        /// The first event, or zero for an empty trace.
        first: u64,
        /// The maximum number of events, in `1..=MAX_PAGE_ELEMENTS`.
        count: u32,
    },
    /// Inspect process only. Validates the source with the requested settings.
    /// Use increasing request IDs for successive validations of a document.
    AnalysisValidate {
        /// The document generation.
        document: u64,
        /// The requested checks and context.
        settings: ValidationSettings,
    },
    /// Inspect process only. Requests cancellation of a pending validation.
    /// Acknowledges a pending request with [`Event::CancelRequested`].
    /// The validation can return a completed report if its work already ended.
    AnalysisCancelValidation {
        /// The document generation.
        document: u64,
        /// The request ID of the validation to stop.
        validation: u64,
    },
    /// Inspect process only. Reads retained findings from a validation.
    AnalysisFindings {
        /// The document generation.
        document: u64,
        /// The request ID of the latest successful validation request.
        validation: u64,
        /// The first finding, or zero for an empty report.
        first: u64,
        /// The maximum number of findings, in `1..=MAX_PAGE_ELEMENTS`.
        count: u32,
    },
    /// Inspect process only. Checks validation options before starting a batch.
    BatchCheckSettings {
        /// The options to check.
        settings: ValidationSettings,
    },
    /// Inspect process only. Validates one file and retains its report.
    BatchValidate {
        /// The read-only source.
        source: WirePath,
        /// The immutable validation options for this file.
        settings: ValidationSettings,
    },
    /// Inspect process only. Cancels one pending file validation.
    BatchCancel {
        /// The request ID of the pending file validation.
        validation: u64,
    },
    /// Inspect process only. Reads findings from a retained batch report.
    BatchFindings {
        /// The request ID that identifies the report.
        validation: u64,
        /// The first finding, or zero for an empty report.
        first: u64,
        /// The maximum number of findings, in `1..=MAX_PAGE_ELEMENTS`.
        count: u32,
    },
    /// Inspect process only. Publishes a complete retained report without overwriting a file.
    BatchExport {
        /// The request ID that identifies the report.
        validation: u64,
        /// The new local report file.
        destination: WirePath,
        /// The output representation.
        format: ReportFormat,
    },
    /// Inspect process only. Removes one retained report.
    BatchForget {
        /// The request ID that identifies the report.
        validation: u64,
    },
    /// Inspect process only. Closes a document. Answered by
    /// [`Event::AnalysisClosed`] or [`Event::AnalysisFailed`].
    AnalysisClose {
        /// The document.
        document: u64,
    },
}

/// Why the engine refused a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The command's protocol version differs from the engine's.
    IncompatibleVersion,
    /// The frame or its body could not be decoded.
    Malformed,
    /// The reply exceeds the control message byte limit.
    PayloadLimit,
    /// The encoder option resolver's one waiting slot is full.
    Busy,
    /// A job is already running, there is none to cancel, or this kind of
    /// engine process does not serve the command.
    WrongState,
}

/// A message from the engine to the app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Answers [`Command::Hello`].
    Hello {
        /// The engine build.
        engine: BuildIdentity,
        /// What this engine can do.
        capabilities: Capabilities,
    },
    /// The command was refused.
    Error {
        /// The machine-readable reason.
        code: ErrorCode,
        /// A human-readable explanation.
        message: String,
    },
    /// Answers [`Command::DescribeOptions`].
    OptionsDescribed {
        /// The first descriptor index.
        first: u32,
        /// The total number of descriptors.
        total: u32,
        /// The descriptors in this page.
        options: Vec<OptionDescriptor>,
    },
    /// Answers [`Command::ResolveOptions`].
    OptionsResolved {
        /// The first effective value index.
        first: u32,
        /// The total number of effective values.
        total: u32,
        /// The effective values in this page.
        values: Vec<EffectiveOption>,
    },
    /// Refuses an encoder option query.
    OptionsFailed {
        /// The field and reason for refusal.
        error: FieldError,
    },
    /// Answers [`Command::Probe`].
    Probed {
        /// What the source's header says.
        facts: SourceFacts,
    },
    /// The engine resolved this policy against the actual job source.
    JobPolicy {
        /// Index of the first option on this page.
        first: u32,
        /// Total number of effective options.
        total: u32,
        /// Effective options in canonical order.
        values: Vec<EffectiveOption>,
    },
    /// The worker has reserved the temporary files and is waiting for acknowledgement.
    ExportPrepared {
        /// The paths the client must save before sending [`Command::ExportProceed`].
        files: Vec<ExportTemp>,
    },
    /// The export entered a phase.
    ExportStage {
        /// The current phase.
        stage: ExportStage,
    },
    /// The latest counts from a running export.
    ExportProgress {
        /// The measured counts.
        progress: ExportProgress,
    },
    /// One file reached its destination.
    ExportPublished {
        /// The new file and the result of its sync check.
        file: ExportArtifact,
    },
    /// The only terminal event of an export.
    ExportFinished {
        /// The outcome and any published files.
        outcome: ExportOutcome,
    },
    /// The job created its temporary file, beside the destination. Sent once,
    /// before any output is written. If the worker dies without a terminal
    /// event, this file is what it leaves behind; after any terminal event it
    /// is gone.
    Working {
        /// The temporary file.
        partial: WirePath,
    },
    /// The job entered a stage.
    Stage {
        /// The stage.
        stage: Stage,
    },
    /// Counters of the running job. Coalesced: at most about ten a second,
    /// and any of them may be skipped.
    Progress {
        /// The counters.
        progress: Progress,
    },
    /// [`Command::Cancel`] was received. The job is still stopping. Also
    /// the acceptance of a [`Command::AnalysisCancel`], under its
    /// `request_id`.
    CancelRequested,
    /// One picture. Its pixels follow this frame on the stream; see
    /// [`PictureHeader`]. Pictures are skippable: the app shows only the
    /// newest of its newest generation.
    Picture {
        /// What the pixels are.
        picture: PictureHeader,
    },
    /// One exact value from a decoded plane.
    PictureSample {
        /// The generation of the sample request.
        generation: u64,
        /// The decoder output ordinal.
        ordinal: u64,
        /// The film grain policy used for this sample.
        grain: GrainPolicy,
        /// The visible coordinate sampled.
        point: SamplePoint,
        /// The decoded sample value.
        value: u16,
    },
    /// Progress while the preview worker prepares a document picture.
    PictureProgress {
        /// The generation that requested the picture.
        generation: u64,
        /// The operation and its unit of measure.
        stage: PictureStage,
        /// Bytes read or preceding output pictures decoded.
        completed: u64,
        /// The target in the same unit, when known.
        total: Option<u64>,
    },
    /// The requested picture or playback cannot be produced.
    PictureUnavailable {
        /// The generation of the request.
        generation: u64,
        /// The machine-readable reason.
        code: PictureFailure,
        /// A human-readable explanation.
        message: String,
    },
    /// Playback stopped, at the end of the stream or on request.
    PlaybackEnded {
        /// The generation of the `Play` request.
        generation: u64,
        /// The last frame that was sent, if any was.
        frame: Option<u64>,
    },
    /// Terminal: the verified file is at its destination.
    Completed {
        /// What was produced and checked.
        report: JobReport,
    },
    /// Terminal: the job stopped on request. No output file was published.
    Cancelled,
    /// Terminal for a job, or the answer to a failed probe. No output file
    /// was published.
    Failed {
        /// Why.
        failure: Failure,
    },
    /// Answers [`Command::AnalysisOpen`]: the document is open.
    AnalysisOpened {
        /// The document.
        document: u64,
        /// What reading the file found.
        summary: DocumentSummary,
    },
    /// Answers [`Command::AnalysisPage`].
    AnalysisPage {
        /// The document.
        document: u64,
        /// The first row.
        first: u64,
        /// The number of rows the filter admits.
        total: u64,
        /// Rows from `first` on: none only when `total` is zero.
        rows: Vec<Row>,
    },
    /// Answers [`Command::AnalysisLocate`].
    AnalysisLocated {
        /// The document.
        document: u64,
        /// Its row among those the filter admits; `None` when the filter
        /// does not admit it.
        row: Option<u64>,
        /// The element found. Boxed: it is the largest event.
        found: Box<Row>,
    },
    /// Answers [`Command::AnalysisBytes`].
    AnalysisBytes {
        /// The document.
        document: u64,
        /// The offset of the first byte.
        offset: u64,
        /// Exactly the bytes asked for.
        bytes: Vec<u8>,
    },
    /// Answers [`Command::AnalysisSyntax`]. The envelope request ID identifies the trace.
    AnalysisSyntaxReady {
        /// The document generation.
        document: u64,
        /// The unfiltered index of the OBU element.
        element: u64,
        /// The scope and outcome of observation.
        summary: Box<SyntaxSummary>,
        /// The number of retained events.
        total: u64,
    },
    /// Answers [`Command::AnalysisSyntaxPage`].
    AnalysisSyntaxPage {
        /// The document generation.
        document: u64,
        /// The request ID that identifies the trace.
        trace: u64,
        /// The unfiltered index of the OBU element.
        element: u64,
        /// The first event.
        first: u64,
        /// The total number of retained events.
        total: u64,
        /// The events from `first`. A page can start or end inside a scope.
        events: Vec<SyntaxEvent>,
    },
    /// Answers [`Command::AnalysisValidate`]. The envelope request ID identifies the report.
    AnalysisValidated {
        /// The document generation.
        document: u64,
        /// Requested checks, scope and completion.
        summary: Box<ValidationSummary>,
        /// The number of retained findings.
        total: u64,
    },
    /// Answers [`Command::AnalysisFindings`].
    AnalysisFindings {
        /// The document generation.
        document: u64,
        /// The request ID that identifies the report.
        validation: u64,
        /// The first finding.
        first: u64,
        /// The total number of retained findings.
        total: u64,
        /// The findings from `first`.
        findings: Vec<Finding>,
    },
    /// Answers [`Command::BatchCheckSettings`] when the options are valid.
    BatchSettingsChecked,
    /// The file is open. Native validation is starting.
    BatchValidating {
        /// The request ID of the file validation.
        validation: u64,
    },
    /// Terminal answer to [`Command::BatchValidate`].
    BatchValidated {
        /// The execution evidence or the source failure. The request ID identifies the report.
        outcome: Box<BatchOutcome>,
    },
    /// Answers [`Command::BatchFindings`].
    BatchFindings {
        /// The request ID that identifies the report.
        validation: u64,
        /// The first finding.
        first: u64,
        /// The total number of retained findings.
        total: u64,
        /// The findings from `first`.
        findings: Vec<Finding>,
    },
    /// The worker allocated the temporary file for a report export.
    BatchExportPrepared {
        /// The request ID that identifies the report.
        validation: u64,
        /// The precise temporary file the app can recover if the worker stops.
        partial: WirePath,
    },
    /// Answers [`Command::BatchExport`] after publication.
    BatchExported {
        /// The request ID that identifies the report.
        validation: u64,
        /// The report size.
        bytes: u64,
    },
    /// Answers [`Command::BatchForget`].
    BatchForgotten {
        /// The request ID of the report the worker removed.
        validation: u64,
    },
    /// Answers [`Command::AnalysisClose`].
    AnalysisClosed {
        /// The document.
        document: u64,
    },
    /// An analysis request has no answer.
    AnalysisFailed {
        /// The document of the request.
        document: u64,
        /// The machine-readable reason.
        code: AnalysisFailure,
        /// A human-readable explanation.
        message: String,
    },
}

/// One framed message: a [`Command`] or [`Event`] with its version and the
/// request it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope<T> {
    /// The sender's [`PROTOCOL_VERSION`].
    pub protocol_version: u32,
    /// Chosen by the app; the engine echoes it on every answer.
    pub request_id: u64,
    /// The command or event.
    pub body: T,
}

impl<T> Envelope<T> {
    /// Wraps `body` at this build's protocol version.
    pub const fn new(request_id: u64, body: T) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            request_id,
            body,
        }
    }
}

/// The envelope fields that must be readable at every protocol version.
#[derive(Deserialize)]
struct Header {
    protocol_version: u32,
}

/// Writes one frame and flushes it.
///
/// # Errors
/// Returns [`ProtocolError::InvalidLength`] when the encoded message exceeds
/// [`MAX_MESSAGE_BYTES`], or [`ProtocolError::Io`] when the stream fails.
pub fn write_message<T: Serialize>(
    writer: &mut impl Write,
    envelope: &Envelope<T>,
) -> Result<(), ProtocolError> {
    let body = serde_json::to_vec(envelope)?;
    let len = u32::try_from(body.len()).unwrap_or(u32::MAX);
    if len == 0 || len > MAX_MESSAGE_BYTES {
        return Err(ProtocolError::InvalidLength { len });
    }
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

/// Reads one frame's body. `Ok(None)` is a clean end of stream between frames.
///
/// # Errors
/// Returns [`ProtocolError::Truncated`] when the stream ends inside a frame,
/// [`ProtocolError::InvalidLength`] before allocating for a bad length, or
/// [`ProtocolError::Io`] when the stream fails.
pub fn read_frame(reader: &mut impl Read) -> Result<Option<Vec<u8>>, ProtocolError> {
    let mut prefix = [0_u8; 4];
    let mut filled = 0;
    while filled < prefix.len() {
        match reader.read(&mut prefix[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => return Err(ProtocolError::Truncated),
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    let len = u32::from_le_bytes(prefix);
    if len == 0 || len > MAX_MESSAGE_BYTES {
        return Err(ProtocolError::InvalidLength { len });
    }
    // `len` is at most MAX_MESSAGE_BYTES, so this allocation is bounded.
    let mut body = vec![0_u8; len as usize];
    reader.read_exact(&mut body).map_err(|e| match e.kind() {
        io::ErrorKind::UnexpectedEof => ProtocolError::Truncated,
        _ => ProtocolError::Io(e),
    })?;
    Ok(Some(body))
}

/// Decodes a frame body, checking the protocol version before the body.
///
/// # Errors
/// Returns [`ProtocolError::IncompatibleVersion`] when the peer's version
/// differs, or [`ProtocolError::Malformed`] for anything else unreadable,
/// including a message type this build does not know.
pub fn decode<T: DeserializeOwned>(frame: &[u8]) -> Result<Envelope<T>, ProtocolError> {
    let header: Header = serde_json::from_slice(frame)?;
    if header.protocol_version != PROTOCOL_VERSION {
        return Err(ProtocolError::IncompatibleVersion {
            theirs: header.protocol_version,
        });
    }
    Ok(serde_json::from_slice(frame)?)
}

/// Reads and decodes one message. `Ok(None)` is a clean end of stream.
///
/// # Errors
/// See [`read_frame`] and [`decode`].
pub fn read_message<T: DeserializeOwned>(
    reader: &mut impl Read,
) -> Result<Option<Envelope<T>>, ProtocolError> {
    read_frame(reader)?.map(|frame| decode(&frame)).transpose()
}

/// Writes an [`Event::Picture`] frame followed by its pixels, then flushes.
///
/// # Errors
/// Returns [`ProtocolError::InvalidPicture`] when the header's size is out of
/// range or `pixels` is not exactly the length it promises, before anything
/// is written; otherwise see [`write_message`].
pub fn write_picture(
    writer: &mut impl Write,
    request_id: u64,
    picture: PictureHeader,
    pixels: &[u8],
) -> Result<(), ProtocolError> {
    if picture.payload_len().map(|len| len as usize) != Some(pixels.len()) {
        return Err(ProtocolError::InvalidPicture {
            width: picture.width,
            height: picture.height,
            bytes: pixels.len(),
        });
    }
    let envelope = Envelope::new(request_id, Event::Picture { picture });
    let body = serde_json::to_vec(&envelope)?;
    let len = u32::try_from(body.len()).unwrap_or(u32::MAX);
    if len == 0 || len > MAX_MESSAGE_BYTES {
        return Err(ProtocolError::InvalidLength { len });
    }
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(&body)?;
    writer.write_all(pixels)?;
    writer.flush()?;
    Ok(())
}

/// An event and, for an [`Event::Picture`], its pixels.
pub type EventFrame = (Envelope<Event>, Option<Vec<u8>>);

/// Reads one event and, for an [`Event::Picture`], the pixels that follow it.
/// Every reader of engine output must use this, or a picture's pixels would
/// be taken for the next frame. `Ok(None)` is a clean end of stream.
///
/// # Errors
/// Returns [`ProtocolError::InvalidPicture`] before allocating for a picture
/// whose size is out of range, [`ProtocolError::Truncated`] when the stream
/// ends inside the pixels, and otherwise see [`read_message`].
pub fn read_event(reader: &mut impl Read) -> Result<Option<EventFrame>, ProtocolError> {
    let Some(envelope) = read_message::<Event>(reader)? else {
        return Ok(None);
    };
    let Event::Picture { picture } = &envelope.body else {
        return Ok(Some((envelope, None)));
    };
    let len = picture
        .payload_len()
        .filter(|len| *len <= MAX_PICTURE_BYTES)
        .ok_or(ProtocolError::InvalidPicture {
            width: picture.width,
            height: picture.height,
            bytes: 0,
        })?;
    // `len` is at most MAX_PICTURE_BYTES, so this allocation is bounded.
    let mut pixels = vec![0_u8; len as usize];
    reader.read_exact(&mut pixels).map_err(|e| match e.kind() {
        io::ErrorKind::UnexpectedEof => ProtocolError::Truncated,
        _ => ProtocolError::Io(e),
    })?;
    Ok(Some((envelope, Some(pixels))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello() -> Envelope<Command> {
        Envelope::new(
            7,
            Command::Hello {
                client: BuildIdentity {
                    name: "test".into(),
                    version: "0".into(),
                },
            },
        )
    }

    fn framed(envelope: &Envelope<Command>) -> Vec<u8> {
        let mut bytes = Vec::new();
        assert!(write_message(&mut bytes, envelope).is_ok());
        bytes
    }

    /// Hands out one byte per `read`, as a slow pipe may.
    struct OneByte<'a>(&'a [u8]);

    impl Read for OneByte<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let Some((first, rest)) = self.0.split_first() else {
                return Ok(0);
            };
            buf[0] = *first;
            self.0 = rest;
            Ok(1)
        }
    }

    fn picture(width: u16, height: u16) -> PictureHeader {
        use crate::preview::{ColorBasis, ColorMatrix, PictureOrigin};
        PictureHeader {
            generation: 3,
            origin: PictureOrigin::Live,
            options: PictureOptions::default(),
            document_info: None,
            output: None,
            frame: 9,
            time: None,
            frame_count: None,
            width,
            height,
            frame_width: 64,
            frame_height: 32,
            view: PictureView::Fit,
            basis: ColorBasis {
                matrix: ColorMatrix::Bt709,
                assumed: false,
            },
        }
    }

    #[test]
    fn a_picture_carries_its_pixels_and_the_next_event_follows() {
        let pixels: Vec<u8> = (0..24).collect();
        let mut bytes = Vec::new();
        assert!(write_picture(&mut bytes, 5, picture(4, 2), &pixels).is_ok());
        assert!(write_message(&mut bytes, &Envelope::new(5, Event::Cancelled)).is_ok());
        let mut reader = OneByte(&bytes);
        let first = read_event(&mut reader);
        assert!(matches!(
            first,
            Ok(Some((ref envelope, Some(ref got))))
                if *got == pixels
                    && envelope.body == Event::Picture { picture: picture(4, 2) }
        ));
        assert!(matches!(
            read_event(&mut reader),
            Ok(Some((ref envelope, None))) if envelope.body == Event::Cancelled
        ));
        assert!(matches!(read_event(&mut reader), Ok(None)));
    }

    #[test]
    fn a_picture_with_the_wrong_pixels_is_never_written() {
        let mut bytes = Vec::new();
        for (header, pixels) in [(picture(4, 2), 23), (picture(0, 2), 0), (picture(4, 2), 0)] {
            let got = write_picture(&mut bytes, 1, header, &vec![0; pixels]);
            assert!(matches!(got, Err(ProtocolError::InvalidPicture { .. })));
        }
        assert_eq!(bytes, [] as [u8; 0]);
    }

    #[test]
    fn a_bad_picture_header_is_rejected_before_its_pixels_are_read() {
        let envelope = Envelope::new(
            1,
            Event::Picture {
                picture: picture(0, 9),
            },
        );
        let mut bytes = Vec::new();
        assert!(write_message(&mut bytes, &envelope).is_ok());
        assert!(matches!(
            read_event(&mut bytes.as_slice()),
            Err(ProtocolError::InvalidPicture {
                width: 0,
                height: 9,
                bytes: 0
            })
        ));
    }

    #[test]
    fn a_stream_that_ends_inside_the_pixels_is_truncated() {
        let mut bytes = Vec::new();
        assert!(write_picture(&mut bytes, 1, picture(4, 2), &[7; 24]).is_ok());
        bytes.truncate(bytes.len() - 1);
        assert!(matches!(
            read_event(&mut bytes.as_slice()),
            Err(ProtocolError::Truncated)
        ));
    }

    #[test]
    fn round_trips_through_partial_reads() {
        let bytes = framed(&hello());
        let mut reader = OneByte(&bytes);
        let got = read_message::<Command>(&mut reader);
        assert!(matches!(got, Ok(Some(ref envelope)) if *envelope == hello()));
        assert!(matches!(read_message::<Command>(&mut reader), Ok(None)));
    }

    #[test]
    fn truncation_is_not_a_clean_end() {
        let bytes = framed(&hello());
        for cut in [1, 3, 4, bytes.len() - 1] {
            let got = read_frame(&mut &bytes[..cut]);
            assert!(matches!(got, Err(ProtocolError::Truncated)), "cut at {cut}");
        }
    }

    #[test]
    fn bad_lengths_are_rejected_before_allocation() {
        for len in [0, MAX_MESSAGE_BYTES + 1, u32::MAX] {
            let got = read_frame(&mut len.to_le_bytes().as_slice());
            assert!(matches!(got, Err(ProtocolError::InvalidLength { len: l }) if l == len));
        }
    }

    #[test]
    fn version_is_checked_before_the_body() {
        let frame = br#"{"protocol_version":999,"request_id":1,"body":{"type":"from_the_future"}}"#;
        let got = decode::<Command>(frame);
        assert!(matches!(
            got,
            Err(ProtocolError::IncompatibleVersion { theirs: 999 })
        ));
    }

    #[test]
    fn unknown_messages_are_malformed() {
        let frame = format!(
            r#"{{"protocol_version":{PROTOCOL_VERSION},"request_id":1,"body":{{"type":"transmogrify"}}}}"#
        );
        assert!(matches!(
            decode::<Command>(frame.as_bytes()),
            Err(ProtocolError::Malformed(_))
        ));
        assert!(matches!(
            decode::<Command>(b"not json"),
            Err(ProtocolError::Malformed(_))
        ));
    }
}
