// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Verification of the finished temporary file, before it is published.
//!
//! Every pass re-reads the file from the start through the real Splot APIs,
//! one temporal unit at a time, and stops at the next read once the job is
//! cancelled. None of this is independent of the encoder's codebase: only
//! [`CheckKind::AvmDecode`] would be, and no AVM decoder is used here yet.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use splot_core::annexb::AnnexBObuCursor;
use splot_core::span::ByteOffset;
use splot_core::stream_reader::{ReaderError, StreamUnit, TemporalUnitReader};
use splot_core::types::ObuType;
use splot_decode::{DecodeContext, DecodeOptions, DecodeRuntimeConfig, ThreadCount};
use splot_encode::{BitDepth, Config};
use splot_validate::Validator;
use studio_domain::job::{Check, CheckKind, CheckOutcome};
use studio_domain::settings::OutputKind;

use crate::job::Stop;

/// Why [`CheckKind::AvmDecode`] does not run in this build.
pub const AVM_NOT_RUN: &str = "no AVM decoder is integrated in this build; the AV1 \
     decoder aomdec cannot decode AV2 and is not used";

/// What the file must contain.
#[derive(Debug, Clone, Copy)]
pub struct Expected<'a> {
    /// The encoder configuration of the job.
    pub config: &'a Config,
    /// The output format.
    pub kind: OutputKind,
    /// Frames the encoder returned.
    pub frames: u64,
}

/// Runs every check on `file`, the job's finished temporary file.
///
/// # Errors
/// [`Stop::Cancelled`], or an output failure when the file cannot be read
/// again. A failed check is a [`CheckOutcome::Failed`], not an error.
pub fn run(file: &File, expected: Expected<'_>, cancel: &AtomicBool) -> Result<Vec<Check>, Stop> {
    let structure = structure(file, expected, cancel)?;
    let decode = splot_decode(file, expected, cancel)?;
    Ok(vec![
        Check {
            kind: CheckKind::Structure,
            outcome: structure,
        },
        Check {
            kind: CheckKind::SplotDecode,
            outcome: decode,
        },
        Check {
            kind: CheckKind::AvmDecode,
            outcome: CheckOutcome::NotRun {
                reason: AVM_NOT_RUN.to_owned(),
            },
        },
    ])
}

/// Framing and counts with Splot's streaming demuxer, then the Splot
/// conformance validator. The validator's rules are not complete AV2
/// conformance coverage.
fn structure(
    file: &File,
    expected: Expected<'_>,
    cancel: &AtomicBool,
) -> Result<CheckOutcome, Stop> {
    if let Err(detail) = framing(file, expected, cancel)? {
        return Ok(failed(detail));
    }
    let report = match Validator::new(false).validate_reader(reader(file, cancel)?) {
        Ok(report) => report,
        Err(_) if cancel.load(Ordering::Relaxed) => return Err(Stop::Cancelled),
        Err(e) => {
            return Ok(failed(format!(
                "the validator could not read the file: {e}"
            )));
        }
    };
    if let Some(error) = report.errors().next() {
        return Ok(failed(format!(
            "the validator reports {} error(s), first {}: {}",
            report.errors().count(),
            error.rule_id,
            error.message
        )));
    }
    Ok(CheckOutcome::Passed)
}

/// `Ok(Err(detail))` when the framing is wrong.
fn framing(
    file: &File,
    expected: Expected<'_>,
    cancel: &AtomicBool,
) -> Result<Result<(), String>, Stop> {
    let mut reader = TemporalUnitReader::new(reader(file, cancel)?);
    let mut units = TemporalUnits::default();
    loop {
        let unit = match reader.next_unit() {
            Ok(Some(unit)) => unit,
            Ok(None) => break,
            Err(ReaderError::Io(_)) if cancel.load(Ordering::Relaxed) => {
                return Err(Stop::Cancelled);
            }
            Err(e) => return Ok(Err(format!("the stream does not parse: {e}"))),
        };
        let pushed = match (expected.kind, unit) {
            (
                OutputKind::Ivf,
                StreamUnit::IvfFrame {
                    index,
                    pts,
                    payload_offset,
                    payload,
                },
            ) => {
                // One record per temporal unit, each showing the next frame.
                let mut record = TemporalUnits::default();
                let checked = if pts == index as u64 {
                    record.push_all(payload, payload_offset)
                } else {
                    Err(format!("has pts {pts}"))
                };
                checked
                    .and_then(|()| match record.count {
                        1 => {
                            units.count += 1;
                            Ok(())
                        }
                        n => Err(format!("holds {n} temporal units")),
                    })
                    .map_err(|detail| format!("IVF frame {index} {detail}"))
            }
            (OutputKind::Av2AnnexB, StreamUnit::AnnexBObu { offset, bytes }) => {
                units.push_all(bytes, offset)
            }
            (_, StreamUnit::IvfWarning(warning)) => {
                Err(format!("IVF warning {}", warning.rule_id()))
            }
            (kind, _) => Err(format!("the file is not {}", kind.label())),
        };
        if let Err(detail) = pushed {
            return Ok(Err(detail));
        }
    }
    if expected.kind == OutputKind::Ivf {
        let config = expected.config.coded();
        let Some(header) = reader.ivf_header() else {
            return Ok(Err("the IVF file has no frames".into()));
        };
        let found = (
            header.fourcc,
            (u32::from(header.width), u32::from(header.height)),
            (header.timebase_denominator, header.timebase_numerator),
        );
        let wanted = (*b"AV02", (config.width, config.height), config.frame_rate);
        // The upstream writer leaves the header frame count at zero; the
        // records are counted instead.
        if found != wanted {
            return Ok(Err(format!(
                "IVF header {found:?} differs from the expected {wanted:?}"
            )));
        }
    }
    if units.count != expected.frames {
        return Ok(Err(format!(
            "{} temporal units for {} encoded frames",
            units.count, expected.frames
        )));
    }
    Ok(Ok(()))
}

/// Counts temporal units in a run of OBUs, which must open with a temporal
/// delimiter. No OBU types beyond the delimiter are interpreted here: a unit
/// that holds no frame is the validator's finding.
#[derive(Debug, Default)]
struct TemporalUnits {
    count: u64,
}

impl TemporalUnits {
    /// Adds every OBU of `bytes`, which start at `offset` in the file.
    fn push_all(&mut self, bytes: &[u8], offset: ByteOffset) -> Result<(), String> {
        let mut cursor = AnnexBObuCursor::new(bytes, offset);
        while let Some(obu) = cursor
            .next_obu()
            .map_err(|e| format!("OBU near byte {}: {e}", offset.get()))?
        {
            if obu.header.obu_type == ObuType::TemporalDelimiter {
                self.count += 1;
            } else if self.count == 0 {
                return Err("does not start with a temporal delimiter".into());
            }
        }
        Ok(())
    }
}

/// A full decode with `splot-decode`, counting the displayed frames by
/// their raw size. The samples are discarded.
fn splot_decode(
    file: &File,
    expected: Expected<'_>,
    cancel: &AtomicBool,
) -> Result<CheckOutcome, Stop> {
    let frame_bytes = raw_frame_bytes(expected.config);
    let context = match DecodeContext::new(DecodeRuntimeConfig::new(ThreadCount::Auto)) {
        Ok(context) => context,
        Err(e) => return Ok(failed(format!("cannot start the decoder: {e}"))),
    };
    let mut sink = Discard { bytes: 0, cancel };
    let result =
        context.decode_raw_reader(reader(file, cancel)?, DecodeOptions::default(), &mut sink);
    if cancel.load(Ordering::Relaxed) {
        return Err(Stop::Cancelled);
    }
    if let Err(e) = result {
        return Ok(failed(format!("decoding failed: {e}")));
    }
    let frames = sink.bytes / frame_bytes;
    if !sink.bytes.is_multiple_of(frame_bytes) || frames != expected.frames {
        return Ok(failed(format!(
            "decoded {} bytes, not {} frames of {frame_bytes} bytes",
            sink.bytes, expected.frames
        )));
    }
    Ok(CheckOutcome::Passed)
}

/// Bytes of one frame in `splot-decode`'s raw output: the visible planes,
/// one byte per 8-bit sample and two per 10-bit sample.
fn raw_frame_bytes(config: &Config) -> u64 {
    let coded = config.coded();
    let samples: u64 = coded
        .plane_sizes()
        .unwrap_or_default()
        .iter()
        .map(|plane| (plane.width() * plane.height()) as u64)
        .sum();
    let per_sample = if coded.bit_depth == BitDepth::Eight {
        1
    } else {
        2
    };
    (samples * per_sample).max(1)
}

/// `file` from its start, buffered, failing reads once `cancel` is set.
fn reader<'a>(file: &File, cancel: &'a AtomicBool) -> Result<Cancellable<'a>, Stop> {
    // `try_clone` shares the cursor, so the passes run one after another.
    let file = file
        .try_clone()
        .and_then(|mut file| file.seek(SeekFrom::Start(0)).map(|_| file))
        .map_err(|e| crate::output::output(format!("cannot reread the output: {e}")))?;
    Ok(Cancellable {
        inner: BufReader::new(file),
        cancel,
    })
}

struct Cancellable<'a> {
    inner: BufReader<File>,
    cancel: &'a AtomicBool,
}

impl Read for Cancellable<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::other("cancelled"));
        }
        self.inner.read(buf)
    }
}

/// Counts decoded bytes and stops the decode once `cancel` is set.
struct Discard<'a> {
    bytes: u64,
    cancel: &'a AtomicBool,
}

impl Write for Discard<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::other("cancelled"));
        }
        self.bytes += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn failed(detail: impl Into<String>) -> CheckOutcome {
    CheckOutcome::Failed {
        detail: detail.into(),
    }
}
