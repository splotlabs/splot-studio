// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Child processes of the `FFmpeg` tools: spawned, bounded, and never left
//! behind.
//!
//! Every child is registered in a process-wide list until it is reaped.
//! Dropping a [`Running`] kills and reaps its child; [`kill_all`] does the
//! same for every child before the worker exits without unwinding (a forced
//! cancel), and refuses new children from then on. A worker killed outright
//! (`kill -9`, a crash) cannot run either: its children then stop at their
//! next write to the closed pipe (`EPIPE`), and the operating system reaps
//! them.

use std::io::{self, Read};
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Bytes of a child's diagnostics kept for error messages; the rest is
/// read and discarded so the child never blocks on a full pipe.
const STDERR_KEPT_BYTES: usize = 16 * 1024;

/// Characters of diagnostics quoted in a failure message.
const QUOTED_CHARS: usize = 1200;

/// A running child with its standard output taken and its diagnostics
/// captured in the background.
#[derive(Debug)]
pub struct Running {
    child: Arc<Mutex<Child>>,
    stderr: Option<JoinHandle<Vec<u8>>>,
    key: u64,
}

/// How a child ended, with what it said on its error output.
#[derive(Debug)]
pub struct Ended {
    /// The exit status.
    pub status: ExitStatus,
    /// The first [`STDERR_KEPT_BYTES`] of its error output, lossily decoded.
    pub stderr: String,
}

impl Ended {
    /// Whether the child exited successfully and printed no diagnostics.
    /// The tools run at `-v error`, so any output is an error.
    #[must_use]
    pub fn clean(&self) -> bool {
        self.status.success() && self.stderr.trim().is_empty()
    }

    /// The exit status and the start of the diagnostics, for a message.
    #[must_use]
    pub fn describe(&self) -> String {
        let text = self.stderr.trim();
        if text.is_empty() {
            format!("exited with {}", self.status)
        } else {
            format!("exited with {}: {}", self.status, quote(text))
        }
    }
}

/// The start of `text`, on one line.
#[must_use]
pub fn quote(text: &str) -> String {
    let mut quoted: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(QUOTED_CHARS)
        .collect();
    if text.chars().count() > QUOTED_CHARS {
        quoted.push('…');
    }
    quoted
}

#[derive(Debug)]
struct Live {
    next: u64,
    children: Vec<(u64, Arc<Mutex<Child>>)>,
    /// [`kill_all`] ran; the process is exiting.
    closed: bool,
}

static LIVE: Mutex<Live> = Mutex::new(Live {
    next: 0,
    children: Vec::new(),
    closed: false,
});

fn live() -> MutexGuard<'static, Live> {
    LIVE.lock().unwrap_or_else(PoisonError::into_inner)
}

fn lock(child: &Mutex<Child>) -> MutexGuard<'_, Child> {
    child.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Kills and reaps every child, and refuses new ones. For a process that
/// is about to exit without dropping its values.
pub fn kill_all() {
    let mut live = live();
    live.closed = true;
    for (_, child) in live.children.drain(..) {
        let mut child = lock(&child);
        // Already exited is fine; the wait reaps it either way.
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Starts `command` with no input, piped output and captured diagnostics.
///
/// # Errors
/// When the process cannot start, or the engine is exiting.
pub fn spawn(mut command: Command) -> io::Result<(Running, ChildStdout)> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A report file and colour codes are not wanted from a child.
        .env_remove("FFREPORT")
        .env("AV_LOG_FORCE_NOCOLOR", "1");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        /// `CREATE_NO_WINDOW`: no console window for a console child of an
        /// engine that itself has none.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    // Spawned under the registry lock, so `kill_all` never misses a child.
    let mut live = live();
    if live.closed {
        return Err(io::Error::other("the engine is exiting"));
    }
    let mut child = command.spawn()?;
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(io::Error::other("the child has no output pipes"));
    };
    let reader = thread::Builder::new()
        .name("studio-tool-stderr".to_owned())
        .spawn(move || capture(stderr));
    let reader = match reader {
        Ok(reader) => reader,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    let key = live.next;
    live.next += 1;
    let child = Arc::new(Mutex::new(child));
    live.children.push((key, Arc::clone(&child)));
    drop(live);
    Ok((
        Running {
            child,
            stderr: Some(reader),
            key,
        },
        stdout,
    ))
}

/// Reads all of `pipe`, keeping the first [`STDERR_KEPT_BYTES`].
fn capture(mut pipe: impl Read) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => return kept,
            Ok(n) => {
                let room = STDERR_KEPT_BYTES.saturating_sub(kept.len());
                kept.extend_from_slice(&buffer[..n.min(room)]);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return kept,
        }
    }
}

impl Running {
    /// Kills the child now. Its pipes close, so blocked reads return.
    pub fn kill(&self) {
        let _ = lock(&self.child).kill();
    }

    /// A handle that can kill the child from another thread.
    #[must_use]
    pub fn killer(&self) -> Killer {
        Killer(Arc::clone(&self.child))
    }

    /// Waits for the child to exit, after its output was read to the end.
    ///
    /// # Errors
    /// When waiting fails.
    pub fn finish(mut self) -> io::Result<Ended> {
        // The error pipe closes when the child exits (or closes it), so this
        // join comes before the wait and the lock is held only briefly.
        let stderr = self
            .stderr
            .take()
            .and_then(|reader| reader.join().ok())
            .unwrap_or_default();
        let status = lock(&self.child).wait()?;
        Ok(Ended {
            status,
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        })
    }

    /// Kills the child and returns what it had said.
    #[must_use]
    pub fn abort(self) -> String {
        self.kill();
        self.finish().map_or_default(|ended| ended.stderr)
    }
}

/// Kills one child from any thread; the child is still reaped by its
/// [`Running`].
#[derive(Debug, Clone)]
pub struct Killer(Arc<Mutex<Child>>);

impl Killer {
    /// Kills the child; its pipes close, so a blocked read returns.
    pub fn kill(&self) {
        let _ = lock(&self.0).kill();
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        {
            let mut child = lock(&self.child);
            if !matches!(child.try_wait(), Ok(Some(_))) {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        if let Some(reader) = self.stderr.take() {
            let _ = reader.join();
        }
        live().children.retain(|(key, _)| *key != self.key);
    }
}

/// What a bounded run produced.
#[derive(Debug)]
pub struct Output {
    /// Standard output, at most the requested bound.
    pub stdout: Vec<u8>,
    /// How the child ended.
    pub ended: Ended,
}

/// Why a bounded run did not produce an output.
#[derive(Debug)]
pub enum BoundedError {
    /// The child could not start or be waited for.
    Io(io::Error),
    /// It wrote more than the bound and was killed.
    TooMuchOutput(usize),
    /// It ran longer than the limit and was killed.
    TimedOut(Duration),
}

impl std::fmt::Display for BoundedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::TooMuchOutput(max) => write!(f, "it wrote more than {max} bytes and was stopped"),
            Self::TimedOut(limit) => write!(f, "it ran longer than {limit:?} and was stopped"),
        }
    }
}

/// Runs `command` to its end, reading at most `max_stdout` bytes of output
/// and killing it after `limit`.
///
/// # Errors
/// See [`BoundedError`].
pub fn run_bounded(
    command: Command,
    max_stdout: usize,
    limit: Duration,
) -> Result<Output, BoundedError> {
    let (running, stdout) = spawn(command).map_err(BoundedError::Io)?;
    let (done, expired) = mpsc::channel::<()>();
    let killer = running.killer().0;
    let watchdog = thread::Builder::new()
        .name("studio-tool-watchdog".to_owned())
        .spawn(move || {
            let timed_out = matches!(expired.recv_timeout(limit), Err(RecvTimeoutError::Timeout));
            if timed_out {
                let _ = lock(&killer).kill();
            }
            timed_out
        })
        .map_err(BoundedError::Io)?;
    let mut bytes = Vec::new();
    let read = stdout.take(max_stdout as u64 + 1).read_to_end(&mut bytes);
    let too_much = bytes.len() > max_stdout;
    if too_much {
        running.kill();
    }
    drop(done);
    let timed_out = watchdog.join().unwrap_or(false);
    let ended = running.finish().map_err(BoundedError::Io)?;
    if timed_out {
        return Err(BoundedError::TimedOut(limit));
    }
    if too_much {
        return Err(BoundedError::TooMuchOutput(max_stdout));
    }
    read.map_err(BoundedError::Io)?;
    Ok(Output {
        stdout: bytes,
        ended,
    })
}
