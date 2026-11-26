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
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use splot_encode::ivf::IvfWriter;
use splot_encode::{Config, Packet};
use studio_domain::export::{ExportArtifactKind, ExportProgress, ExportStage, ExportTempIdentity};
use studio_domain::job::FailureCode;
use studio_domain::settings::OutputKind;
use tempfile::NamedTempFile;

use crate::job::Stop;

/// Synchronous worker services for the decoded export job.
pub(crate) trait ExportControl {
    /// Whether the worker accepted a cancellation.
    fn cancelled(&self) -> bool;

    /// Creates an exclusive temporary file and registers it with the worker.
    fn create_temp(
        &self,
        kind: ExportArtifactKind,
        destination: &Path,
        source: &FileId,
    ) -> Result<NamedTempFile, Stop>;

    /// Waits for the app to save the temporary paths. Call before writing output.
    fn await_prepared(&self) -> Result<(), Stop>;

    /// Reports the current phase. The publication claim reports `Publishing`.
    fn stage(&self, stage: ExportStage);

    /// Reports counts. The worker may replace a progress event that is still pending.
    fn progress(&self, progress: ExportProgress);

    /// Waits for permission to publish the full batch.
    /// Flush, sync and verify the files first.
    fn claim_publish(&self, frames: u64) -> Result<(), Stop>;

    /// Publishes one verified file. Records its path even if a directory sync fails.
    fn publish(
        &self,
        kind: ExportArtifactKind,
        temp: NamedTempFile,
        destination: &Path,
    ) -> Result<(), Stop>;
}

/// Which file a path names.
///
/// Unix compares device and inode. Windows compares volume and file index.
/// These identities also detect a hard link to the opened source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileId(
    #[cfg(any(unix, windows))] (u64, u64),
    #[cfg(not(any(unix, windows)))] PathBuf,
);

impl FileId {
    /// File identity that remains usable across a new process on this platform.
    #[cfg_attr(
        any(unix, windows),
        expect(
            clippy::unnecessary_wraps,
            reason = "other platforms can lack a stable identity"
        )
    )]
    pub(crate) fn export_identity(
        &self,
        created: Option<SystemTime>,
    ) -> Option<ExportTempIdentity> {
        #[cfg(any(unix, windows))]
        {
            let (device, inode) = self.0;
            Some(ExportTempIdentity {
                device,
                inode,
                created: created.filter(|time| *time >= UNIX_EPOCH),
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = created;
            None
        }
    }

    /// The identity of the already opened `file` at `path`.
    ///
    /// # Errors
    /// Any error from reading its identity.
    #[allow(unused_variables)] // each platform reads one of the two arguments
    pub fn of_file(file: &File, path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = file.metadata()?;
            Ok(Self((metadata.dev(), metadata.ino())))
        }
        #[cfg(windows)]
        {
            let info = winapi_util::file::information(file)?;
            Ok(Self((info.volume_serial_number(), info.file_index())))
        }
        #[cfg(not(any(unix, windows)))]
        {
            Ok(Self(fs::canonicalize(path)?))
        }
    }

    /// The identity of the file or directory `path` names, following symlinks.
    ///
    /// # Errors
    /// Any error from opening `path` or reading its identity.
    pub fn of_path(path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = fs::metadata(path)?;
            Ok(Self((metadata.dev(), metadata.ino())))
        }
        #[cfg(windows)]
        {
            let handle = winapi_util::Handle::from_path_any(path)?;
            Self::of_file(handle.as_file(), path)
        }
        #[cfg(not(any(unix, windows)))]
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

/// Whether two paths refer to the same directory entry, even before a file exists.
pub(crate) fn same_destination_slot(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    let parent_id = |path: &Path| {
        let parent = path.parent().filter(|dir| !dir.as_os_str().is_empty());
        FileId::of_path(parent.unwrap_or_else(|| Path::new(".")))
    };
    left.file_name()
        .zip(right.file_name())
        .is_some_and(|(left, right)| same_name(left, right))
        && parent_id(left).is_ok_and(|left| parent_id(right).is_ok_and(|right| left == right))
}

fn same_name(left: &OsStr, right: &OsStr) -> bool {
    if cfg!(any(windows, target_os = "macos")) {
        left.to_str().zip(right.to_str()).map_or_else(
            || {
                left.as_encoded_bytes()
                    .eq_ignore_ascii_case(right.as_encoded_bytes())
            },
            |(left, right)| left.to_uppercase() == right.to_uppercase(),
        )
    } else {
        left == right
    }
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
    let publication = publish_recorded(temp, destination)?;
    publication.sync?;
    Ok(publication.bytes)
}

/// The file exists at its final path. A sync error does not remove it.
pub(crate) struct Publication {
    pub(crate) bytes: u64,
    pub(crate) sync: Result<(), Stop>,
}

/// Publishes without replacing another file. Keeps sync errors separate from rename errors.
pub(crate) fn publish_recorded(
    temp: NamedTempFile,
    destination: &Path,
) -> Result<Publication, Stop> {
    publish_with_sync(temp, destination, sync_parent)
}

fn publish_with_sync(
    temp: NamedTempFile,
    destination: &Path,
    sync: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<Publication, Stop> {
    let bytes = temp
        .as_file()
        .metadata()
        .map_err(|e| output(format!("Cannot read the temporary file size. {e}")))?
        .len();
    let _file = temp.persist_noclobber(destination).map_err(|e| {
        if e.error.kind() == io::ErrorKind::AlreadyExists {
            exists()
        } else {
            output(format!("cannot publish the output: {}", e.error))
        }
    })?;
    let sync = sync(destination).map_err(|e| {
        output(format!(
            "The file was published, but its folder sync failed. {e}"
        ))
    });
    Ok(Publication { bytes, sync })
}

fn sync_parent(destination: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        File::open(parent)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = destination;
        Ok(())
    }
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

#[cfg(test)]
mod publication_tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn windows_identities_follow_open_handles_and_hard_links() -> io::Result<()> {
        let scratch = tempfile::tempdir()?;
        let source = scratch.path().join("source.obu");
        let link = scratch.path().join("link.obu");
        let other = scratch.path().join("other.obu");
        fs::write(&source, b"source")?;
        fs::write(&other, b"other")?;
        fs::hard_link(&source, &link)?;
        let opened = File::open(&source)?;
        let id = FileId::of_file(&opened, &source)?;
        assert_eq!(FileId::of_path(&source)?, id);
        assert_eq!(FileId::of_path(&link)?, id);
        assert_ne!(FileId::of_path(&other)?, id);
        assert!(
            matches!(check_destination(&link, &id), Err(Stop::Failed(failure))
            if failure.code == FailureCode::SourceIsDestination)
        );
        let directory = FileId::of_path(scratch.path())?;
        assert_ne!(directory, id);
        assert!(
            directory
                .export_identity(scratch.path().metadata()?.created().ok())
                .is_some()
        );
        Ok(())
    }

    #[test]
    fn a_post_rename_sync_failure_preserves_the_published_file()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let destination = dir.path().join("export.yuv");
        let mut temp = tempfile::NamedTempFile::new_in(dir.path())?;
        temp.write_all(b"verified samples")?;
        temp.as_file().sync_all()?;
        let publication = publish_with_sync(temp, &destination, |_| {
            Err(io::Error::other("injected folder sync failure"))
        })
        .map_err(|stop| io::Error::other(format!("{stop:?}")))?;
        assert_eq!(publication.bytes, 16);
        assert!(publication.sync.is_err());
        assert_eq!(fs::read(&destination)?, b"verified samples");
        Ok(())
    }
}
