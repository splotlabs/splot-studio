// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Authenticated input and sequential decoding in display order.

use std::fs::{self, File};
use std::io::{self, BufReader, Read};
use std::path::PathBuf;

use splot_core::annexb::AnnexBObuCursor;
use splot_core::ivf::IvfHeader;
use splot_core::stream_reader::{ReaderError, TemporalUnitReader};
use splot_core::types::ObuType;
use splot_decode::{DataProps, DecodeError, Decoder, Picture, SendStatus};
use studio_domain::analysis::Framing;
use studio_domain::export::{ExportSource, ExportStage};
use studio_domain::job::{FailureCode, Rational};

use super::check_cancelled;
use crate::inspect::document::{Checked, HashReader};
use crate::job::Stop;
use crate::output::{ExportControl, FileId};
use crate::stream::{MAX_DOCUMENT_UNITS, MAX_UNIT_BYTES};

pub(super) struct Source<'a> {
    path: PathBuf,
    file: File,
    declared: &'a ExportSource,
    pub id: FileId,
}

#[derive(Clone, Copy)]
pub(super) struct Unit {
    pub offset: u64,
    pub pts: Option<u64>,
}

pub(super) struct Output {
    pub ordinal: u64,
    pub picture: Picture,
    pub unit: Unit,
    pub ivf_base: Option<Rational>,
}

impl<'a> Source<'a> {
    pub fn open(declared: &'a ExportSource) -> Result<Self, Stop> {
        let path = declared.path.to_path_buf().map_err(invalid)?;
        let named = fs::metadata(&path).map_err(|_| changed())?;
        if !named.is_file() {
            return Err(invalid("Select a regular AV2 file."));
        }
        let file = File::open(&path).map_err(|_| changed())?;
        let actual = file.metadata().map_err(|_| changed())?;
        if !actual.is_file()
            || studio_domain::job::SourceFingerprint::of(&actual) != declared.fingerprint
        {
            return Err(changed());
        }
        let id = FileId::of_file(&file, &path).map_err(|_| changed())?;
        let source = Self {
            path,
            file,
            declared,
            id,
        };
        source.check()?;
        Ok(source)
    }

    pub fn check(&self) -> Result<(), Stop> {
        crate::source::still_the_same(&self.path, &self.id, self.declared.fingerprint)
            .map_err(|_| changed())
    }

    /// Decode the necessary prefix and verify the actual bytes from this handle.
    ///
    /// The callback returns true once the selected range has ended. Bytes after
    /// that point still enter the source digest. They do not enter the decoder.
    pub fn decode(
        &mut self,
        decoder: Decoder,
        control: &dyn ExportControl,
        mut on_output: impl FnMut(Output) -> Result<bool, Stop>,
    ) -> Result<u64, Stop> {
        let cancelled = || control.cancelled();
        let file = (&mut self.file).take(self.declared.digest.bytes);
        let mut hashed = HashReader::new(Checked::new(file, &cancelled));
        let count = {
            let reader = BufReader::new(&mut hashed);
            let mut decoding = Decoding {
                reader: TemporalUnitReader::with_max_unit_bytes(reader, MAX_UNIT_BYTES),
                decoder,
                units: Vec::new(),
                count: 0,
                framing: self.declared.framing,
            };
            decoding.read(control, &mut on_output)?;
            decoding.count
        };

        control.stage(ExportStage::Verifying);
        // The demuxer may have read beyond the last submitted unit. Read-ahead
        // bytes already passed through the hash reader.
        io::copy(&mut hashed, &mut io::sink()).map_err(|e| read_failed(control, e))?;
        if hashed.finish() != self.declared.digest {
            return Err(changed());
        }
        self.check()?;
        check_cancelled(control)?;
        Ok(count)
    }
}

struct Decoding<R: Read> {
    reader: TemporalUnitReader<R>,
    decoder: Decoder,
    units: Vec<Unit>,
    count: u64,
    framing: Framing,
}

impl<R: Read> Decoding<R> {
    fn read(
        &mut self,
        control: &dyn ExportControl,
        on_output: &mut impl FnMut(Output) -> Result<bool, Stop>,
    ) -> Result<(), Stop> {
        loop {
            check_cancelled(control)?;
            let Some(unit) = self
                .reader
                .next_temporal_unit()
                .map_err(|e| reader_failed(control, e))?
            else {
                self.ivf_base()?;
                if let Some(warning) = self.reader.ivf_warning() {
                    return Err(invalid(format!(
                        "The IVF file ends inside a record. ({})",
                        warning.rule_id()
                    )));
                }
                self.decoder.drain();
                self.receive(control, on_output)?;
                return Ok(());
            };
            if self.units.len() >= MAX_DOCUMENT_UNITS {
                return Err(Stop::failed(
                    FailureCode::TooLarge,
                    format!("Export can read at most {MAX_DOCUMENT_UNITS} temporal units."),
                ));
            }
            if self.units.is_empty() {
                check_framing(self.framing, unit.pts, unit.bytes)?;
            }
            let token = self.units.len() as u64;
            self.units.push(Unit {
                offset: unit.offset.get(),
                pts: unit.pts,
            });
            let props = DataProps::default().with_user_data(token);
            check_cancelled(control)?;
            // get_picture below takes every available output before another
            // unit is read. The driver cannot fill the input queue meanwhile.
            if self
                .decoder
                .send_data(unit.bytes, props)
                .map_err(|e| decode_failed(&e))?
                != SendStatus::Accepted
            {
                return Err(invalid("The decoder refused a pending temporal unit."));
            }
            if self.receive(control, on_output)? {
                return Ok(());
            }
        }
    }

    fn ivf_base(&self) -> Result<Option<Rational>, Stop> {
        let header = self.reader.ivf_header();
        if header.is_some() != (self.framing == Framing::Ivf) {
            return Err(changed());
        }
        header.map(ivf_base).transpose()
    }

    fn receive(
        &mut self,
        control: &dyn ExportControl,
        on_output: &mut impl FnMut(Output) -> Result<bool, Stop>,
    ) -> Result<bool, Stop> {
        let ivf_base = self.ivf_base()?;
        loop {
            check_cancelled(control)?;
            let Some(picture) = self.decoder.get_picture().map_err(|e| decode_failed(&e))? else {
                return Ok(false);
            };
            let token = picture.provenance().display.user_data;
            let unit = usize::try_from(token)
                .ok()
                .and_then(|index| self.units.get(index))
                .copied()
                .ok_or_else(|| invalid("The decoder returned an unknown temporal unit."))?;
            let ordinal = self.count;
            self.count += 1;
            if on_output(Output {
                ordinal,
                picture,
                unit,
                ivf_base,
            })? {
                return Ok(true);
            }
        }
    }
}

fn check_framing(expected: Framing, pts: Option<u64>, unit: &[u8]) -> Result<(), Stop> {
    if (expected == Framing::Ivf) != pts.is_some() {
        return Err(changed());
    }
    if expected == Framing::AnnexB
        && !AnnexBObuCursor::new(unit, splot_core::span::ByteOffset::new(0))
            .next_obu()
            .is_ok_and(|obu| {
                obu.is_some_and(|obu| obu.header.obu_type == ObuType::TemporalDelimiter)
            })
    {
        return Err(invalid(
            "The Annex B stream must start with a temporal delimiter.",
        ));
    }
    Ok(())
}

fn ivf_base(header: IvfHeader) -> Result<Rational, Stop> {
    if header.fourcc != *b"AV02" {
        return Err(Stop::failed(
            FailureCode::UnsupportedSource,
            "Export requires AV02 IVF.",
        ));
    }
    Ok(Rational {
        num: header.timebase_numerator,
        den: header.timebase_denominator,
    })
}

fn changed() -> Stop {
    Stop::failed(
        FailureCode::SourceChanged,
        "The source changed. Open it again in Analyze.",
    )
}

fn invalid(error: impl std::fmt::Display) -> Stop {
    Stop::failed(FailureCode::InvalidSource, error.to_string())
}

fn read_failed(control: &dyn ExportControl, error: impl std::fmt::Display) -> Stop {
    if control.cancelled() {
        Stop::Cancelled
    } else {
        invalid(format!("Cannot read the source. {error}"))
    }
}

fn reader_failed(control: &dyn ExportControl, error: ReaderError) -> Stop {
    match error {
        ReaderError::UnitTooLarge { .. } => Stop::failed(FailureCode::TooLarge, error.to_string()),
        _ => read_failed(control, error),
    }
}

pub(super) fn decode_failed(error: &DecodeError) -> Stop {
    let code = match error {
        DecodeError::InvalidOperatingPointSelection { .. } => FailureCode::InvalidSettings,
        DecodeError::Limit { .. } => FailureCode::TooLarge,
        DecodeError::UnsupportedStructure { .. } | DecodeError::UnsupportedFeature { .. } => {
            FailureCode::UnsupportedSource
        }
        _ => FailureCode::InvalidSource,
    };
    Stop::failed(code, format!("Cannot decode the AV2 source. {error}"))
}
