// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The output transaction and the two packet formats.
//!
//! A job writes into an exclusively created temporary file next to the
//! destination, verifies it, and only then moves it into place with a
//! no-clobber rename. Dropping the [`NamedTempFile`] on any failure or
//! cancellation removes it; nothing is ever written at the destination path
//! itself, and an existing file there is never replaced.

use std::cell::Cell;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use splot_encode::ivf::IvfWriter;
use splot_encode::{Config, Packet};
use studio_domain::job::FailureCode;
use studio_domain::settings::OutputKind;
use tempfile::NamedTempFile;

use crate::job::Stop;

/// Which file a path names.
///
/// Unix compares device and inode, so hard links and symlinks to the source
/// are caught. Windows compares canonical paths, which resolves symlinks but
/// not hard links; since an existing destination is refused anyway, a missed
/// hard link is reported as [`FailureCode::DestinationExists`], never written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileId(#[cfg(unix)] (u64, u64), #[cfg(not(unix))] PathBuf);

impl FileId {
    /// The identity of the already opened `file` at `path`.
    ///
    /// # Errors
    /// Any error from reading its metadata or canonicalizing `path`.
    #[allow(unused_variables)] // each platform reads one of the two arguments
    pub fn of_file(file: &File, path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = file.metadata()?;
            Ok(Self((metadata.dev(), metadata.ino())))
        }
        #[cfg(not(unix))]
        {
            Ok(Self(fs::canonicalize(path)?))
        }
    }

    /// The identity of the file `path` names, following symlinks.
    ///
    /// # Errors
    /// Any error from reading its metadata or canonicalizing `path`.
    pub fn of_path(path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = fs::metadata(path)?;
            Ok(Self((metadata.dev(), metadata.ino())))
        }
        #[cfg(not(unix))]
        {
            Ok(Self(fs::canonicalize(path)?))
        }
    }
}

/// Checks `destination` and creates the temporary file beside it.
///
/// # Errors
/// [`FailureCode::SourceIsDestination`] when it names the source,
/// [`FailureCode::DestinationExists`] when anything exists there (including
/// a dangling symlink), or [`FailureCode::Output`] when the temporary file
/// cannot be created.
pub fn prepare(destination: &Path, source: &FileId) -> Result<NamedTempFile, Stop> {
    check_destination(destination, source)?;
    temporary(destination)
}

/// Starts a no-clobber transaction when there is no verified source identity.
pub(crate) fn prepare_new(destination: &Path) -> Result<NamedTempFile, Stop> {
    check_absent(destination)?;
    temporary(destination)
}

fn temporary(destination: &Path) -> Result<NamedTempFile, Stop> {
    let name = destination
        .file_name()
        .ok_or_else(|| output("the destination has no file name"))?;
    let dir = match destination.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let mut prefix = OsString::from(".");
    prefix.push(name);
    prefix.push(".");
    let mut builder = tempfile::Builder::new();
    builder.prefix(&prefix).suffix(".partial");
    // The published file gets the usual umask-limited mode, not 0600.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o666));
    }
    builder.tempfile_in(&dir).map_err(|e| {
        output(format!(
            "cannot create a temporary file in {}: {e}",
            dir.display()
        ))
    })
}

/// The checks of [`prepare`] without creating anything, so a long job can
/// fail early. [`prepare`] repeats them.
///
/// # Errors
/// As [`prepare`], except for creating the temporary file.
pub fn check_destination(destination: &Path, source: &FileId) -> Result<(), Stop> {
    if destination.file_name().is_none() {
        return Err(output("the destination has no file name"));
    }
    if FileId::of_path(destination).is_ok_and(|id| id == *source) {
        return Err(Stop::failed(
            FailureCode::SourceIsDestination,
            "the destination is the source file",
        ));
    }
    check_absent(destination)
}

fn check_absent(destination: &Path) -> Result<(), Stop> {
    match fs::symlink_metadata(destination) {
        Ok(_) => Err(exists()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(output(format!("cannot inspect the destination: {e}"))),
    }
}

/// Moves the verified `temp` to `destination` without replacing anything
/// that appeared there meanwhile, and returns the published size.
///
/// # Errors
/// [`FailureCode::DestinationExists`] when the destination appeared, or
/// [`FailureCode::Output`]. The temporary file is removed in both cases.
pub fn publish(temp: NamedTempFile, destination: &Path) -> Result<u64, Stop> {
    let file = temp.persist_noclobber(destination).map_err(|e| {
        if e.error.kind() == io::ErrorKind::AlreadyExists {
            exists()
        } else {
            output(format!("cannot publish the output: {}", e.error))
        }
    })?;
    // Make the new name durable too. Directories cannot be opened for this
    // outside Unix; there the rename's own durability applies.
    #[cfg(unix)]
    if let Some(dir) = destination.parent().filter(|d| !d.as_os_str().is_empty()) {
        File::open(dir)
            .and_then(|dir| dir.sync_all())
            .map_err(|e| output(format!("published, but cannot sync its directory: {e}")))?;
    }
    file.metadata()
        .map(|metadata| metadata.len())
        .map_err(|e| output(format!("published, but cannot read its size: {e}")))
}

/// Writes packets in one output format and counts the bytes.
#[derive(Debug)]
pub struct PacketSink<W: Write> {
    format: Format<Counted<W>>,
    written: Rc<Cell<u64>>,
}

#[derive(Debug)]
enum Format<W: Write> {
    Ivf(IvfWriter<W>),
    /// [`Packet::data`] is one complete AV2 Annex B temporal unit: a
    /// temporal delimiter and the length-prefixed OBUs after it. Writing the
    /// packets in order is the length-delimited stream; no prefix is added.
    AnnexB(W),
}

impl<W: Write> PacketSink<W> {
    /// Starts an output of `kind` for `config` on `writer`. IVF writes its
    /// file header now.
    ///
    /// # Errors
    /// Any write error, or [`io::ErrorKind::InvalidInput`] when IVF cannot
    /// hold the frame size.
    pub fn new(kind: OutputKind, writer: W, config: &Config) -> io::Result<Self> {
        let written = Rc::new(Cell::new(0));
        let writer = Counted {
            inner: writer,
            written: Rc::clone(&written),
        };
        let format = match kind {
            OutputKind::Ivf => Format::Ivf(IvfWriter::new(writer, config)?),
            OutputKind::Av2AnnexB => Format::AnnexB(writer),
        };
        Ok(Self { format, written })
    }

    /// Appends one packet.
    ///
    /// # Errors
    /// Any write error.
    pub fn write(&mut self, packet: &Packet) -> io::Result<()> {
        match &mut self.format {
            Format::Ivf(writer) => writer.write_packet(packet),
            Format::AnnexB(writer) => writer.write_all(&packet.data),
        }
    }

    /// Bytes handed to the writer so far.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.written.get()
    }

    /// Flushes and returns the writer.
    ///
    /// # Errors
    /// Any flush error.
    pub fn finish(self) -> io::Result<W> {
        let mut writer = match self.format {
            Format::Ivf(writer) => writer.finish()?,
            Format::AnnexB(writer) => writer,
        };
        writer.flush()?;
        Ok(writer.inner)
    }
}

/// Counts the bytes its writer accepts.
#[derive(Debug)]
struct Counted<W> {
    inner: W,
    written: Rc<Cell<u64>>,
}

impl<W: Write> Write for Counted<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.written.set(self.written.get() + n as u64);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn exists() -> Stop {
    Stop::failed(
        FailureCode::DestinationExists,
        "the destination already exists",
    )
}

/// An [`FailureCode::Output`] failure.
pub fn output(message: impl Into<String>) -> Stop {
    Stop::failed(FailureCode::Output, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Accepts `room` bytes, then fails like a full disk.
    struct Full {
        room: usize,
    }

    impl Write for Full {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.room == 0 {
                return Err(io::Error::other("no space left"));
            }
            let n = buf.len().min(self.room);
            self.room -= n;
            Ok(n)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn write_errors_surface_in_both_formats() {
        let config = Config::new(16, 16);
        assert!(PacketSink::new(OutputKind::Ivf, Full { room: 8 }, &config).is_err());
        let mut sink = PacketSink::new(OutputKind::Av2AnnexB, Full { room: 8 }, &config);
        let mut packet = placeholder_packet();
        packet.data = vec![0; 16];
        let failed = sink.as_mut().map(|sink| sink.write(&packet));
        assert!(matches!(failed, Ok(Err(_))));
        assert_eq!(sink.map(|s| s.bytes()).ok(), Some(8));
    }

    /// A packet value; `Packet` is non-exhaustive, so it comes from a real
    /// one-frame encode.
    fn placeholder_packet() -> Packet {
        let config = Config::new(16, 16);
        let frame =
            splot_encode::SourceFrame::new(&config, 0, [vec![0; 256], vec![0; 64], vec![0; 64]]);
        let packet = frame
            .and_then(|frame| {
                let mut encoder = splot_encode::Encoder::new(config)?;
                encoder.send_frame(frame)?;
                encoder.flush()?;
                encoder.receive_packet()
            })
            .ok()
            .flatten();
        packet.unwrap_or_else(|| unreachable!("a one-frame encode yields a packet"))
    }

    #[test]
    fn the_obu_format_is_the_packet_bytes_in_order() {
        let config = Config::new(16, 16);
        let packet = placeholder_packet();
        let mut sink = PacketSink::new(OutputKind::Av2AnnexB, Vec::new(), &config)
            .unwrap_or_else(|_| unreachable!());
        assert!(sink.write(&packet).is_ok() && sink.write(&packet).is_ok());
        assert_eq!(sink.bytes(), 2 * packet.data.len() as u64);
        let bytes = sink.finish().unwrap_or_default();
        assert_eq!(bytes, [packet.data.clone(), packet.data].concat());
    }
}
