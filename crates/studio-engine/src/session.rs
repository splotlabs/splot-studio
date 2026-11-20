// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The handshake session loop: framed commands in, framed events out.

use std::io::{Read, Write};

use studio_domain::protocol::{
    Capabilities, Command, Envelope, ErrorCode, Event, ProtocolError, decode, read_frame,
    write_message,
};
use thiserror::Error;

use crate::build_identity;

/// The session ended because the framing is unrecoverable or a stream failed.
///
/// A bad *message* is not an error here: it is answered with
/// [`Event::Error`] and the session continues.
#[derive(Debug, Error)]
pub enum SessionError {
    /// The command stream is broken: bad length, truncated frame, or I/O failure.
    #[error("cannot read command: {0}")]
    Read(#[source] ProtocolError),
    /// The event stream could not be written.
    #[error("cannot write event: {0}")]
    Write(#[source] ProtocolError),
}

/// Serves commands from `input` until a clean end of stream.
///
/// Every decodable-or-not frame gets exactly one reply, echoing its
/// `request_id` (0 when the frame carries none). A version mismatch or an
/// unknown/malformed body is answered and the session continues, since the
/// framing is still intact. This build cannot encode, so `Hello` reports an
/// empty capability list.
///
/// # Errors
/// Returns [`SessionError`] on an invalid length prefix, a stream that ends
/// inside a frame, or any I/O failure. Nothing is written for those.
pub fn run(input: &mut impl Read, output: &mut impl Write) -> Result<(), SessionError> {
    while let Some(frame) = read_frame(input).map_err(SessionError::Read)? {
        let reply = match decode::<Command>(&frame) {
            Ok(Envelope {
                request_id,
                body: Command::Hello { .. },
                ..
            }) => Envelope::new(
                request_id,
                Event::Hello {
                    engine: build_identity(),
                    capabilities: Capabilities::default(),
                },
            ),
            Err(error) => {
                let code = match error {
                    ProtocolError::IncompatibleVersion { .. } => ErrorCode::IncompatibleVersion,
                    _ => ErrorCode::Malformed,
                };
                let event = Event::Error {
                    code,
                    message: error.to_string(),
                };
                Envelope::new(request_id_of(&frame), event)
            }
        };
        write_message(output, &reply).map_err(SessionError::Write)?;
    }
    Ok(())
}

/// Best-effort `request_id` of a frame that did not decode as a command.
fn request_id_of(frame: &[u8]) -> u64 {
    serde_json::from_slice::<serde_json::Value>(frame)
        .ok()
        .and_then(|value| value.get("request_id")?.as_u64())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use studio_domain::protocol::{BuildIdentity, read_message};

    use super::*;

    fn hello(id: u64) -> Envelope<Command> {
        let client = BuildIdentity {
            name: "t".into(),
            version: "0".into(),
        };
        Envelope::new(id, Command::Hello { client })
    }

    fn serve(input: &[u8]) -> (Result<(), SessionError>, Vec<u8>) {
        let mut out = Vec::new();
        let result = run(&mut { input }, &mut out);
        (result, out)
    }

    #[test]
    fn hello_is_answered_with_no_capabilities() {
        let mut input = Vec::new();
        assert!(write_message(&mut input, &hello(3)).is_ok());
        let (result, out) = serve(&input);
        assert!(result.is_ok());
        let reply = read_message::<Event>(&mut out.as_slice());
        assert!(matches!(
            reply,
            Ok(Some(Envelope { request_id: 3, body: Event::Hello { ref capabilities, .. }, .. }))
                if capabilities.encode_outputs.is_empty()
        ));
    }

    #[test]
    fn bad_message_is_answered_and_session_continues() {
        let bad = br#"{"protocol_version":1,"request_id":8,"body":{"type":"encode"}}"#;
        let mut input = Vec::new();
        input.extend_from_slice(&u32::try_from(bad.len()).unwrap_or(0).to_le_bytes());
        input.extend_from_slice(bad);
        assert!(write_message(&mut input, &hello(9)).is_ok());
        let (result, out) = serve(&input);
        assert!(result.is_ok());
        let mut reader = out.as_slice();
        assert!(matches!(
            read_message::<Event>(&mut reader),
            Ok(Some(Envelope {
                request_id: 8,
                body: Event::Error {
                    code: ErrorCode::Malformed,
                    ..
                },
                ..
            }))
        ));
        assert!(matches!(
            read_message::<Event>(&mut reader),
            Ok(Some(Envelope {
                request_id: 9,
                body: Event::Hello { .. },
                ..
            }))
        ));
    }

    #[test]
    fn truncated_frame_ends_the_session_with_nothing_written() {
        let (result, out) = serve(&[10, 0, 0, 0, b'{']);
        assert!(matches!(
            result,
            Err(SessionError::Read(ProtocolError::Truncated))
        ));
        assert_eq!(out, Vec::<u8>::new());
    }
}
