// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The control protocol between the app and the engine.
//!
//! A frame is a little-endian `u32` byte length followed by that many bytes of
//! UTF-8 JSON holding one [`Envelope`]. Control only: pixels never travel here.
//! The length is checked against [`MAX_MESSAGE_BYTES`] before any allocation,
//! and the protocol version before the body is interpreted, so a mismatched
//! app/engine pair fails with [`ProtocolError::IncompatibleVersion`] instead
//! of a confusing parse error.

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::job::{Failure, JobReport, JobSpec, Progress, SourceFacts, Stage, WirePath};
use crate::settings::OutputKind;

/// The protocol version this build speaks. Bump on any incompatible change.
pub const PROTOCOL_VERSION: u32 = 3;

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
    },
    /// Asks the running job to stop. [`Event::CancelRequested`] acknowledges
    /// the request; only [`Event::Cancelled`] means the job has stopped.
    /// Closing the worker's input has the same effect.
    Cancel,
}

/// Why the engine refused a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The command's protocol version differs from the engine's.
    IncompatibleVersion,
    /// The frame or its body could not be decoded.
    Malformed,
    /// A job is already running, or there is none to cancel.
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
    /// Answers [`Command::Probe`].
    Probed {
        /// What the source's header says.
        facts: SourceFacts,
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
    /// [`Command::Cancel`] was received. The job is still stopping.
    CancelRequested,
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
