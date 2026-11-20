// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Finding the engine and asking who it is.
//!
//! One `Hello` exchange with the sibling `splot-studio-engine handshake`
//! process: no polling, no retry, and the child is always reaped. Blocking,
//! so it runs on a worker thread, never on the Slint event loop.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};

use studio_domain::protocol::{
    BuildIdentity, Command, Envelope, ErrorCode, Event, PROTOCOL_VERSION, ProtocolError,
    read_message, write_message,
};
use studio_domain::settings::OutputKind;

/// The engine executable's file name stem.
pub const ENGINE_NAME: &str = "splot-studio-engine";

/// The request id of the one handshake this app sends.
const HELLO_ID: u64 = 1;

/// What the handshake found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineStatus {
    /// The engine answered and speaks this app's protocol.
    Ready {
        /// Who built the engine.
        engine: BuildIdentity,
        /// The output kinds it says it can encode; empty in this milestone.
        encode_outputs: Vec<OutputKind>,
    },
    /// No engine executable at the expected place.
    NotFound(PathBuf),
    /// The engine speaks another protocol version.
    Incompatible(String),
    /// The engine could not be started or did not answer sensibly.
    Failed(String),
}

impl EngineStatus {
    /// A one-line, user-facing description.
    #[must_use]
    pub fn summary(&self) -> String {
        match self {
            Self::Ready {
                engine,
                encode_outputs,
            } => {
                let encoding = if encode_outputs.is_empty() {
                    "encoding not available".to_owned()
                } else {
                    let labels: Vec<_> = encode_outputs.iter().map(|kind| kind.label()).collect();
                    format!("encodes {}", labels.join(", "))
                };
                format!(
                    "Engine {} {} · protocol {PROTOCOL_VERSION} · {encoding}",
                    engine.name, engine.version
                )
            }
            Self::NotFound(path) => format!("Engine not found: {}", path.display()),
            Self::Incompatible(why) => format!("Engine incompatible: {why}"),
            Self::Failed(why) => format!("Engine failed: {why}"),
        }
    }
}

/// The engine executable next to the running app.
///
/// # Errors
/// Returns the error from [`std::env::current_exe`].
pub fn sibling_path() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(exe.with_file_name(format!("{ENGINE_NAME}{}", std::env::consts::EXE_SUFFIX)))
}

/// Starts `engine handshake`, sends one `Hello` and reports the answer.
#[must_use]
pub fn handshake(engine: &Path, client: BuildIdentity) -> EngineStatus {
    let child = Process::new(engine)
        .arg("handshake")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Engine diagnostics stay visible to whoever launched the app.
        .stderr(Stdio::inherit())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return EngineStatus::NotFound(engine.to_path_buf());
        }
        Err(e) => return EngineStatus::Failed(format!("could not start: {e}")),
    };
    let status = exchange(&mut child, client);
    if !matches!(status, EngineStatus::Ready { .. }) {
        // A confused engine may still be waiting for input; do not wait on it.
        let _ = child.kill();
    }
    // Reap the child either way. Its exit status adds nothing to the answer.
    let _ = child.wait();
    status
}

fn exchange(child: &mut Child, client: BuildIdentity) -> EngineStatus {
    let (Some(mut stdin), Some(mut stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return EngineStatus::Failed("no pipes to the engine".to_owned());
    };
    let hello = Envelope::new(HELLO_ID, Command::Hello { client });
    if let Err(e) = write_message(&mut stdin, &hello) {
        return EngineStatus::Failed(format!("could not send the handshake: {e}"));
    }
    // End of input tells the engine this session is over once it has answered.
    drop(stdin);
    match read_message::<Event>(&mut stdout) {
        Ok(Some(reply)) if reply.request_id != HELLO_ID => EngineStatus::Failed(format!(
            "answered request {} instead of {HELLO_ID}",
            reply.request_id
        )),
        Ok(Some(reply)) => match reply.body {
            Event::Hello {
                engine,
                capabilities,
            } => EngineStatus::Ready {
                engine,
                encode_outputs: capabilities.encode_outputs,
            },
            Event::Error {
                code: ErrorCode::IncompatibleVersion,
                message,
            } => EngineStatus::Incompatible(message),
            Event::Error { message, .. } => EngineStatus::Failed(message),
        },
        Ok(None) => EngineStatus::Failed("exited without answering".to_owned()),
        Err(ProtocolError::IncompatibleVersion { theirs }) => EngineStatus::Incompatible(format!(
            "it speaks protocol {theirs}, this app speaks {PROTOCOL_VERSION}"
        )),
        Err(e) => EngineStatus::Failed(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> BuildIdentity {
        BuildIdentity {
            name: "test".into(),
            version: "0".into(),
        }
    }

    #[test]
    fn a_missing_engine_is_reported_as_not_found() {
        let path = Path::new("/nonexistent/splot-studio-engine");
        assert_eq!(
            handshake(path, client()),
            EngineStatus::NotFound(path.to_path_buf())
        );
    }

    #[test]
    fn the_engine_sits_next_to_the_app() {
        let path = sibling_path().unwrap_or_else(|e| unreachable!("{e}"));
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        assert_eq!(
            name.as_deref(),
            Some(format!("{ENGINE_NAME}{}", std::env::consts::EXE_SUFFIX).as_str())
        );
    }

    #[test]
    fn summary_never_claims_encoding_without_capabilities() {
        let ready = EngineStatus::Ready {
            engine: BuildIdentity {
                name: ENGINE_NAME.into(),
                version: "0.0.0".into(),
            },
            encode_outputs: Vec::new(),
        };
        assert_eq!(
            ready.summary(),
            format!(
                "Engine {ENGINE_NAME} 0.0.0 · protocol {PROTOCOL_VERSION} · encoding not available"
            )
        );
        let capable = EngineStatus::Ready {
            engine: BuildIdentity {
                name: ENGINE_NAME.into(),
                version: "9".into(),
            },
            encode_outputs: vec![OutputKind::Ivf],
        };
        assert!(capable.summary().ends_with("encodes AV2 IVF"));
    }

    // A real child that exits without answering: the app must report failure,
    // not hang, and reap it. `sh handshake` fails to open a script named
    // "handshake" and exits, standing in for a broken engine on Unix hosts.
    #[cfg(unix)]
    #[test]
    fn an_engine_that_exits_silently_is_a_failure() {
        let status = handshake(Path::new("/bin/sh"), client());
        assert!(matches!(status, EngineStatus::Failed(_)), "{status:?}");
    }
}
