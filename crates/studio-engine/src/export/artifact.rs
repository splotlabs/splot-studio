// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Writes and verifies one temporary artifact before it can be published.

use std::cell::Cell;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::rc::Rc;

use sha2::{Digest, Sha256};
use studio_domain::analysis::SourceDigest;
use studio_domain::export::ExportArtifactKind;
use studio_domain::job::{FailureCode, WirePath};
use tempfile::NamedTempFile;

use super::check_cancelled;
use crate::inspect::document::{Checked, HashReader};
use crate::job::Stop;
use crate::output::{self, ExportControl, FileId};

pub(super) struct Artifact<'a> {
    kind: ExportArtifactKind,
    destination: PathBuf,
    writer: BufWriter<Tracked<'a, NamedTempFile>>,
    control: &'a dyn ExportControl,
}

impl<'a> Artifact<'a> {
    pub fn create(
        control: &'a dyn ExportControl,
        kind: ExportArtifactKind,
        destination: &WirePath,
        source: &FileId,
    ) -> Result<Self, Stop> {
        check_cancelled(control)?;
        let destination = destination
            .to_path_buf()
            .map_err(|e| output::output(e.to_string()))?;
        let file = control.create_temp(kind, &destination, source)?;
        Ok(Self {
            kind,
            destination,
            writer: BufWriter::new(Tracked::new(file, control)),
            control,
        })
    }

    pub fn bytes_written(&self) -> u64 {
        self.writer.get_ref().bytes.get()
    }

    pub fn byte_counter(&self) -> Rc<Cell<u64>> {
        Rc::clone(&self.writer.get_ref().bytes)
    }

    pub fn verify(mut self) -> Result<Verified, Stop> {
        self.flush().map_err(|e| failed(self.control, e))?;
        let tracked = self
            .writer
            .into_inner()
            .map_err(|e| failed(self.control, e))?;
        let (mut temp, expected) = tracked.finish();
        temp.as_file()
            .sync_all()
            .map_err(|e| failed(self.control, e))?;
        let file = temp.as_file_mut();
        file.seek(SeekFrom::Start(0))
            .map_err(|e| failed(self.control, e))?;
        verify_bytes(BufReader::new(file), &expected, self.control)?;
        Ok(Verified {
            kind: self.kind,
            destination: self.destination,
            temp,
            bytes: expected.bytes,
        })
    }
}

impl Write for Artifact<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        active(self.control)?;
        self.writer.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        active(self.control)?;
        self.writer.flush()
    }
}

pub(super) struct Verified {
    kind: ExportArtifactKind,
    destination: PathBuf,
    temp: NamedTempFile,
    pub bytes: u64,
}

impl Verified {
    pub fn publish(self, control: &dyn ExportControl) -> Result<(), Stop> {
        control.publish(self.kind, self.temp, &self.destination)
    }
}

/// Hashes only bytes that the underlying writer accepted.
struct Tracked<'a, W> {
    inner: W,
    hash: Sha256,
    bytes: Rc<Cell<u64>>,
    control: &'a dyn ExportControl,
}

impl<'a, W> Tracked<'a, W> {
    fn new(inner: W, control: &'a dyn ExportControl) -> Self {
        Self {
            inner,
            hash: Sha256::new(),
            bytes: Rc::new(Cell::new(0)),
            control,
        }
    }

    fn finish(self) -> (W, SourceDigest) {
        let digest = SourceDigest {
            bytes: self.bytes.get(),
            sha256: crate::artifact::hex(&self.hash.finalize()),
        };
        (self.inner, digest)
    }
}

impl<W: Write> Write for Tracked<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        active(self.control)?;
        let written = self.inner.write(bytes)?;
        self.hash.update(&bytes[..written]);
        self.bytes.set(self.bytes.get() + written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        active(self.control)?;
        self.inner.flush()
    }
}

fn active(control: &dyn ExportControl) -> io::Result<()> {
    if control.cancelled() {
        Err(io::Error::other("Export cancelled."))
    } else {
        Ok(())
    }
}

fn verify_bytes(
    reader: impl Read,
    expected: &SourceDigest,
    control: &dyn ExportControl,
) -> Result<(), Stop> {
    let cancelled = || control.cancelled();
    let mut hashed = HashReader::new(Checked::new(
        reader.take(expected.bytes.saturating_add(1)),
        &cancelled,
    ));
    io::copy(&mut hashed, &mut io::sink()).map_err(|e| {
        if control.cancelled() {
            Stop::Cancelled
        } else {
            Stop::failed(
                FailureCode::VerificationFailed,
                format!("Cannot read back the export. {e}"),
            )
        }
    })?;
    if &hashed.finish() == expected {
        Ok(())
    } else {
        Err(Stop::failed(
            FailureCode::VerificationFailed,
            "The verified output differs from the bytes written.",
        ))
    }
}

pub(super) fn failed(control: &dyn ExportControl, error: impl std::fmt::Display) -> Stop {
    if control.cancelled() {
        Stop::Cancelled
    } else {
        output::output(format!("Cannot write or verify the export. {error}"))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use studio_domain::export::{ExportProgress, ExportStage};

    use super::*;

    struct Control;

    impl ExportControl for Control {
        fn cancelled(&self) -> bool {
            false
        }
        fn create_temp(
            &self,
            _: ExportArtifactKind,
            path: &Path,
            source: &FileId,
        ) -> Result<NamedTempFile, Stop> {
            output::prepare(path, source)
        }
        fn await_prepared(&self) -> Result<(), Stop> {
            Ok(())
        }
        fn stage(&self, _: ExportStage) {}
        fn progress(&self, _: ExportProgress) {}
        fn claim_publish(&self, _: u64) -> Result<(), Stop> {
            Ok(())
        }
        fn publish(
            &self,
            _: ExportArtifactKind,
            file: NamedTempFile,
            path: &Path,
        ) -> Result<(), Stop> {
            output::publish(file, path).map(|_| ())
        }
    }

    struct FailingIo {
        write_error: Option<io::ErrorKind>,
        flush_error: bool,
    }

    impl Write for FailingIo {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            match self.write_error {
                Some(error) => Err(io::Error::from(error)),
                None => Ok(buffer.len()),
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.flush_error {
                Err(io::Error::other("Injected flush failure."))
            } else {
                Ok(())
            }
        }
    }

    impl Read for FailingIo {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("Injected read failure."))
        }
    }

    #[test]
    fn low_disk_and_write_errors_do_not_report_written_bytes() {
        for error in [io::ErrorKind::StorageFull, io::ErrorKind::PermissionDenied] {
            let io = FailingIo {
                write_error: Some(error),
                flush_error: false,
            };
            let mut writer = Tracked::new(io, &Control);
            let error = writer.write_all(b"samples").expect_err("The writer fails.");
            assert!(
                matches!(failed(&Control, error), Stop::Failed(problem) if problem.code == FailureCode::Output)
            );
            assert_eq!(writer.finish().1.bytes, 0);
        }
    }

    #[test]
    fn flush_failure_is_a_write_failure_even_after_successful_writes() -> io::Result<()> {
        let io = FailingIo {
            write_error: None,
            flush_error: true,
        };
        let mut writer = Tracked::new(io, &Control);
        writer.write_all(b"samples")?;
        let error = writer.flush().expect_err("The flush fails.");
        assert!(
            matches!(failed(&Control, error), Stop::Failed(problem) if problem.code == FailureCode::Output)
        );
        assert_eq!(writer.finish().1.bytes, 7);
        Ok(())
    }

    #[test]
    fn verification_rejects_read_failure_and_same_size_corruption() -> io::Result<()> {
        let mut writer = Tracked::new(Vec::new(), &Control);
        writer.write_all(b"samples")?;
        let (bytes, digest) = writer.finish();
        assert!(verify_bytes(bytes.as_slice(), &digest, &Control).is_ok());
        let io = FailingIo {
            write_error: None,
            flush_error: false,
        };
        for result in [
            verify_bytes(io, &digest, &Control),
            verify_bytes(b"example".as_slice(), &digest, &Control),
        ] {
            assert!(
                matches!(result, Err(Stop::Failed(problem)) if problem.code == FailureCode::VerificationFailed)
            );
        }
        Ok(())
    }
}
