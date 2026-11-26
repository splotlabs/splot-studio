// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The engine process client: one short-lived `splot-studio-engine worker`
//! process per request (the startup `Hello`, each probe, each job), one
//! long-lived `splot-studio-engine preview` process for pictures, and
//! `splot-studio-engine inspect` processes for read-only analysis documents.
//!
//! Commands are written from the caller's thread; events are read on a
//! dedicated thread and handed to a callback, so nothing here blocks the Slint
//! event loop except [`Worker::shutdown`], which is meant for after it. The
//! child is always reaped: by the reader thread once the event stream ends,
//! and, if the process keeps its output open, by killing it after a grace
//! period when the [`Worker`] is dropped or shut down.
//!
//! Pictures are converted into the window's pixel format on the reader
//! thread, never on the Slint thread, and are not queued: each process keeps
//! only its newest unclaimed picture in a [`PictureSlot`].

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command as Process, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use slint::{Rgb8Pixel, SharedPixelBuffer};
use studio_domain::options::{EffectiveOption, FieldError, OptionDescriptor};
use studio_domain::preview::PictureHeader;
use studio_domain::protocol::{
    BuildIdentity, Capabilities, Command, Envelope, Event, PROTOCOL_VERSION, ProtocolError,
    read_event, write_message,
};
use studio_domain::settings::OutputKind;

/// The engine executable's file name stem.
pub const ENGINE_NAME: &str = "splot-studio-engine";

/// How long a worker whose input was closed may take to exit before it is
/// killed. Closing the input cancels a running job, which then removes its
/// temporary file; this leaves room for that.
pub const EXIT_GRACE: Duration = Duration::from_secs(5);

/// How long a process that closed its output may take to exit before it is
/// killed.
const STREAM_END_GRACE: Duration = Duration::from_secs(2);

/// How often an exiting process is checked while waiting for it.
const EXIT_POLL: Duration = Duration::from_millis(10);

/// Which kind of engine process to start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// `worker`: one handshake, probe or job, then it exits.
    Worker,
    /// `preview`: serves picture requests until its input closes.
    Preview,
    /// `inspect`: serves analysis documents until its input closes.
    Inspect,
}

impl Role {
    const fn arg(self) -> &'static str {
        match self {
            Self::Worker => "worker",
            Self::Preview => "preview",
            Self::Inspect => "inspect",
        }
    }
}

/// One picture from the engine, its pixels already in the window's format.
#[derive(Clone)]
pub struct Picture {
    /// The request it was sent under.
    pub request_id: u64,
    /// What the pixels are.
    pub header: PictureHeader,
    /// `header.width` x `header.height` RGB pixels.
    pub pixels: SharedPixelBuffer<Rgb8Pixel>,
}

impl Picture {
    /// Wraps `bytes` (rows of R, G, B, no padding); `None` unless they are
    /// exactly what `header` promises.
    #[must_use]
    pub fn new(request_id: u64, header: PictureHeader, bytes: &[u8]) -> Option<Self> {
        let len = header.payload_len()? as usize;
        if bytes.len() != len {
            return None;
        }
        let pixels = SharedPixelBuffer::<Rgb8Pixel>::clone_from_slice(
            bytes,
            u32::from(header.width),
            u32::from(header.height),
        );
        Some(Self {
            request_id,
            header,
            pixels,
        })
    }

    /// The memory its pixels take.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.pixels.as_bytes().len()
    }
}

impl std::fmt::Debug for Picture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Picture")
            .field("request_id", &self.request_id)
            .field("header", &self.header)
            .finish_non_exhaustive()
    }
}

/// The newest picture a process sent that nobody has taken yet. A newer
/// picture replaces an untaken one: pictures are never queued.
#[derive(Debug, Clone, Default)]
pub struct PictureSlot(Arc<Mutex<Option<Picture>>>);

impl PictureSlot {
    /// Takes the newest picture, if one is waiting.
    #[must_use]
    pub fn take(&self) -> Option<Picture> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).take()
    }

    /// Stores `picture`; `true` when the slot was empty, so the consumer
    /// needs telling.
    fn put(&self, picture: Picture) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(picture)
            .is_none()
    }
}

/// What a worker or the journal writer hands to the session callback.
#[derive(Debug)]
pub enum Delivery {
    /// One event from the engine, other than a picture.
    Event(Envelope<Event>),
    /// A picture arrived: take the newest from this slot. Sent once for
    /// each run of pictures nobody took, so the consumer must always take
    /// it, or no further `Pictures` is delivered.
    Pictures(PictureSlot),
    /// The event stream is over and the process has been reaped. Always the
    /// last delivery.
    Ended(Ending),
    /// The journal writer finished saving a submitted batch.
    JournalSaved(Result<(), String>),
    /// The background Advanced client finished fetching options.
    Advanced(AdvancedReply),
    /// A related job source and settings preflight finished.
    Related(crate::state::RelatedResolution),
}

/// The catalog and validation result for one Advanced generation.
#[derive(Debug)]
pub struct AdvancedReply {
    /// Whether this result is for proposed settings.
    pub preview: bool,
    /// Descriptors when the app requested a new catalog.
    pub catalog: Option<Vec<OptionDescriptor>>,
    /// The complete policy, empty when there is no source, or its error.
    pub policy: Result<Vec<EffectiveOption>, FieldError>,
}

/// How a worker's event stream ended.
#[derive(Debug)]
pub struct Ending {
    /// `None` for a clean end of stream; otherwise why reading stopped. The
    /// process is killed before it is reaped in that case.
    pub error: Option<ProtocolError>,
    /// How the process exited, for example `exit status: 0`.
    pub exit: String,
}

/// Why a worker could not be started.
#[derive(Debug)]
pub enum SpawnError {
    /// There is no engine executable at this path.
    NotFound(PathBuf),
    /// The process or its reader thread could not be started.
    Io(io::Error),
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(path) => write!(f, "engine not found: {}", path.display()),
            Self::Io(e) => write!(f, "could not start the engine: {e}"),
        }
    }
}

impl std::error::Error for SpawnError {}

impl From<io::Error> for SpawnError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// A running engine process.
#[derive(Debug)]
pub struct Worker {
    stdin: Option<ChildStdin>,
    child: Arc<Mutex<Child>>,
    /// Disconnects when the reader thread has reaped the child.
    reaped: Option<Receiver<()>>,
}

impl Worker {
    /// Starts `engine worker` (or `engine preview`) and its reader thread.
    /// `deliver` receives every event and finally [`Delivery::Ended`], on the
    /// reader thread.
    ///
    /// # Errors
    /// Returns [`SpawnError`] when the process or its reader thread cannot be
    /// started; nothing is left running in that case.
    pub fn spawn(
        engine: &Path,
        role: Role,
        mut deliver: impl FnMut(Delivery) + Send + 'static,
    ) -> Result<Self, SpawnError> {
        let mut command = Process::new(engine);
        command
            .arg(role.arg())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Engine diagnostics stay visible to whoever launched the app.
            .stderr(Stdio::inherit());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            /// `CREATE_NO_WINDOW`: the engine is a console program and the
            /// release app has no console, so without this Windows would
            /// open a console window for every engine process.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let child = command.spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(SpawnError::NotFound(engine.to_path_buf()));
            }
            Err(e) => return Err(e.into()),
        };
        let (stdin, stdout) = (child.stdin.take(), child.stdout.take());
        let child = Arc::new(Mutex::new(child));
        let (Some(stdin), Some(mut stdout)) = (stdin, stdout) else {
            kill_and_reap(&child);
            return Err(io::Error::other("no pipes to the engine").into());
        };
        let (reaped_tx, reaped) = mpsc::channel::<()>();
        let reader_child = Arc::clone(&child);
        let reader = thread::Builder::new()
            .name("engine-events".to_owned())
            .spawn(move || {
                let slot = PictureSlot::default();
                let error = loop {
                    match read_event(&mut stdout) {
                        Ok(Some((envelope, None))) => deliver(Delivery::Event(envelope)),
                        Ok(Some((envelope, Some(bytes)))) => {
                            let Event::Picture { picture: header } = envelope.body else {
                                continue;
                            };
                            let (width, height) = (header.width, header.height);
                            // `read_event` checked the length; this cannot fail.
                            let Some(picture) = Picture::new(envelope.request_id, header, &bytes)
                            else {
                                break Some(ProtocolError::InvalidPicture {
                                    width,
                                    height,
                                    bytes: bytes.len(),
                                });
                            };
                            drop(bytes);
                            if slot.put(picture) {
                                deliver(Delivery::Pictures(slot.clone()));
                            }
                        }
                        Ok(None) => break None,
                        Err(e) => break Some(e),
                    }
                };
                drop(stdout);
                let exit = if error.is_some() {
                    // A peer that breaks the protocol is not trusted to stop.
                    kill_and_reap(&reader_child)
                } else {
                    reap(&reader_child, STREAM_END_GRACE)
                };
                deliver(Delivery::Ended(Ending { error, exit }));
                drop(reaped_tx);
            });
        if let Err(e) = reader {
            kill_and_reap(&child);
            return Err(e.into());
        }
        Ok(Self {
            stdin: Some(stdin),
            child,
            reaped: Some(reaped),
        })
    }

    /// Sends one command under `request_id`.
    ///
    /// # Errors
    /// Returns [`ProtocolError`] when the input is closed or the pipe fails,
    /// for example because the worker already exited.
    pub fn send(&mut self, request_id: u64, command: Command) -> Result<(), ProtocolError> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "worker input is closed"))?;
        write_message(stdin, &Envelope::new(request_id, command))
    }

    /// Ends the worker's input. An idle worker then exits; a running job is
    /// cancelled.
    pub fn close_input(&mut self) {
        self.stdin = None;
    }

    /// Kills the process now. The reader thread still reaps it and delivers
    /// [`Delivery::Ended`].
    pub fn kill(&self) {
        // Fails only when the process has already exited.
        let _ = lock(&self.child).kill();
    }

    /// Closes the input, waits up to `grace` for the process to exit, then
    /// kills it. Blocks for at most `grace` plus the time a kill takes.
    pub fn shutdown(mut self, grace: Duration) {
        self.close_input();
        if let Some(reaped) = self.reaped.take() {
            settle(&self.child, &reaped, grace);
        }
    }
}

impl Drop for Worker {
    /// Never blocks: a worker that does not exit within [`EXIT_GRACE`] of
    /// losing its input is killed from a short-lived thread.
    fn drop(&mut self) {
        self.close_input();
        let Some(reaped) = self.reaped.take() else {
            return;
        };
        let child = Arc::clone(&self.child);
        let watchdog = thread::Builder::new()
            .name("engine-reaper".to_owned())
            .spawn(move || settle(&child, &reaped, EXIT_GRACE));
        if watchdog.is_err() {
            // No thread to wait with: do not leave the process behind.
            let _ = lock(&self.child).kill();
        }
    }
}

/// Waits for the reader thread to reap the child, killing it after `grace`.
fn settle(child: &Mutex<Child>, reaped: &Receiver<()>, grace: Duration) {
    if reaped.recv_timeout(grace) == Err(RecvTimeoutError::Timeout) {
        let _ = lock(child).kill();
        // Killing closes the output, so the reader thread finishes promptly.
        let _ = reaped.recv_timeout(STREAM_END_GRACE);
    }
}

/// Waits up to `grace` for an exiting process, then kills it, and reaps it.
fn reap(child: &Mutex<Child>, grace: Duration) -> String {
    let deadline = Instant::now() + grace;
    loop {
        match lock(child).try_wait() {
            Ok(Some(status)) => return describe(status),
            Ok(None) if Instant::now() < deadline => {}
            Ok(None) => return kill_and_reap(child),
            Err(e) => return format!("could not be waited for: {e}"),
        }
        thread::sleep(EXIT_POLL);
    }
}

fn kill_and_reap(child: &Mutex<Child>) -> String {
    let mut child = lock(child);
    let _ = child.kill();
    match child.wait() {
        Ok(status) => describe(status),
        Err(e) => format!("could not be waited for: {e}"),
    }
}

fn describe(status: ExitStatus) -> String {
    status.to_string()
}

fn lock(child: &Mutex<Child>) -> MutexGuard<'_, Child> {
    // A panic while holding the lock cannot leave a `Child` inconsistent.
    child.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The engine executable next to the running app.
///
/// # Errors
/// Returns the error from [`std::env::current_exe`].
pub fn sibling_path() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(exe.with_file_name(format!("{ENGINE_NAME}{}", std::env::consts::EXE_SUFFIX)))
}

/// What the startup `Hello` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineStatus {
    /// The engine answered and speaks this app's protocol.
    Ready {
        /// Who built the engine.
        engine: BuildIdentity,
        /// What it says it can do.
        capabilities: Capabilities,
    },
    /// No engine executable at the expected place.
    NotFound(PathBuf),
    /// The engine speaks another protocol version.
    Incompatible(String),
    /// The engine could not be started or did not answer sensibly.
    Failed(String),
}

impl EngineStatus {
    /// Interprets the first delivery after a `Hello`.
    #[must_use]
    pub fn from_hello(delivery: Delivery) -> Self {
        match delivery {
            Delivery::Event(envelope) => match envelope.body {
                Event::Hello {
                    engine,
                    capabilities,
                } => Self::Ready {
                    engine,
                    capabilities,
                },
                Event::Error { message, .. } => Self::Failed(message),
                other => Self::Failed(format!("answered Hello with {other:?}")),
            },
            Delivery::Advanced(_) | Delivery::JournalSaved(_) | Delivery::Related(_) => {
                Self::Failed("The engine did not answer Hello.".into())
            }
            Delivery::Pictures(_) => Self::Failed("answered Hello with a picture".to_owned()),
            Delivery::Ended(Ending {
                error: Some(ProtocolError::IncompatibleVersion { theirs }),
                ..
            }) => Self::Incompatible(format!(
                "it speaks protocol {theirs}, this app speaks {PROTOCOL_VERSION}"
            )),
            Delivery::Ended(Ending {
                error: Some(error), ..
            }) => Self::Failed(error.to_string()),
            Delivery::Ended(Ending { error: None, exit }) => {
                Self::Failed(format!("exited without answering ({exit})"))
            }
        }
    }

    /// The output kinds the engine can encode; empty unless it is ready.
    #[must_use]
    pub fn encode_outputs(&self) -> &[OutputKind] {
        match self {
            Self::Ready { capabilities, .. } => &capabilities.encode_outputs,
            _ => &[],
        }
    }

    /// A one-line, user-facing description.
    #[must_use]
    pub fn summary(&self) -> String {
        match self {
            Self::Ready {
                engine,
                capabilities,
            } => {
                let encode_outputs = &capabilities.encode_outputs;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_engine_is_reported_as_not_found() {
        let path = Path::new("/nonexistent/splot-studio-engine");
        let got = Worker::spawn(path, Role::Worker, |_| {});
        assert!(
            matches!(got, Err(SpawnError::NotFound(ref p)) if p == path),
            "{got:?}"
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
            capabilities: Capabilities::default(),
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
            capabilities: Capabilities {
                encode_outputs: vec![OutputKind::Ivf],
                media_tool: None,
                analysis_framings: Vec::new(),
                analysis_evidence: None,
                options: None,
            },
        };
        assert!(capable.summary().ends_with("encodes AV2 IVF"));
        assert_eq!(capable.encode_outputs(), [OutputKind::Ivf]);
    }

    #[test]
    fn an_incompatible_engine_is_named_as_such() {
        let status = EngineStatus::from_hello(Delivery::Ended(Ending {
            error: Some(ProtocolError::IncompatibleVersion { theirs: 1 }),
            exit: "signal: 9 (SIGKILL)".into(),
        }));
        assert!(
            matches!(status, EngineStatus::Incompatible(_)),
            "{status:?}"
        );
    }

    /// A real child that exits without answering: the client must deliver
    /// `Ended` (not hang) after reaping it. `sh worker` fails to open a script
    /// named "worker" and exits, standing in for a broken engine.
    #[cfg(unix)]
    #[test]
    fn a_silent_exit_ends_the_stream() {
        let (tx, rx) = mpsc::channel();
        let worker = Worker::spawn(Path::new("/bin/sh"), Role::Worker, move |d| {
            let _ = tx.send(d);
        });
        assert!(worker.is_ok(), "{worker:?}");
        let got = rx.recv_timeout(Duration::from_secs(10));
        assert!(
            matches!(got, Ok(Delivery::Ended(Ending { error: None, ref exit })) if !exit.is_empty()),
            "{got:?}"
        );
        let status = got.map(EngineStatus::from_hello);
        assert!(matches!(status, Ok(EngineStatus::Failed(_))), "{status:?}");
    }

    /// A process that ignores its input and keeps its output open is killed
    /// once the grace period runs out; shutdown returns and `Ended` follows.
    #[cfg(unix)]
    #[test]
    fn shutdown_kills_a_worker_that_does_not_exit() {
        use std::os::unix::fs::PermissionsExt;
        let script = std::env::temp_dir().join(format!("studio-hang-{}", std::process::id()));
        let written = std::fs::write(&script, "#!/bin/sh\nexec sleep 30\n").and_then(|()| {
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
        });
        assert!(written.is_ok(), "{written:?}");
        let (tx, rx) = mpsc::channel();
        // Another test forking at this moment can still hold the script open
        // for writing, and running it then fails as "text file busy".
        let mut tries = 0;
        let worker = loop {
            let tx = tx.clone();
            let worker = Worker::spawn(&script, Role::Worker, move |d| {
                let _ = tx.send(d);
            });
            match &worker {
                Err(e) if e.to_string().contains("busy") && tries < 50 => {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(20));
                }
                _ => break worker,
            }
        };
        let started = Instant::now();
        match worker {
            Ok(worker) => worker.shutdown(Duration::from_millis(200)),
            Err(e) => unreachable!("{e}"),
        }
        let got = rx.recv_timeout(Duration::from_secs(10));
        let _ = std::fs::remove_file(&script);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(
            matches!(got, Ok(Delivery::Ended(Ending { error: None, ref exit })) if exit.contains("signal")),
            "{got:?}"
        );
    }
}
