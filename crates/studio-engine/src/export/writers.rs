// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Decoded Y4M, packed planes, and frame hash output.

use std::cell::Cell;
use std::io::{self, Write};
use std::rc::Rc;

use serde::Serialize;
use splot_decode::{Picture, PictureFrame};
use splot_recon::{
    DecodedFrame, DecodedFrameHash, DecodedFrameHashInput, ReconSample, Y4mError, Y4mFrameRate,
    Y4mWriter,
};
use studio_domain::analysis::SourceDigest;
use studio_domain::export::{
    ExportArtifactKind, ExportFormat, ExportLayer, ExportRange, ExportRequest, ExportSampleFormat,
};
use studio_domain::job::Rational;
use studio_domain::preview::GrainPolicy;

use super::artifact::{self, Artifact, Verified};
use super::input::Output;
use super::samples::{self, Layout, Metadata, unsupported};
use super::timing::{Evidence, Status};
use crate::job::Stop;
use crate::output::{ExportControl, FileId};

pub(super) enum Writers<'a> {
    Y4m(Box<Video<'a>>),
    Raw(Box<Raw<'a>>),
    Hashes(Box<Report<'a>>),
}

impl<'a> Writers<'a> {
    pub fn prepare(
        request: &ExportRequest,
        source: &FileId,
        control: &'a dyn ExportControl,
    ) -> Result<Self, Stop> {
        let output =
            match &request.format {
                ExportFormat::Y4m { path } => Self::Y4m(Box::new(Video {
                    file: Some(Artifact::create(
                        control,
                        ExportArtifactKind::Video,
                        path,
                        source,
                    )?),
                    writer: None,
                    first: None,
                    rate: None,
                })),
                ExportFormat::Raw { samples, layout } => {
                    let description =
                        Artifact::create(control, ExportArtifactKind::Layout, layout, source)?;
                    let samples =
                        Artifact::create(control, ExportArtifactKind::Samples, samples, source)?;
                    Self::Raw(Box::new(Raw {
                        samples,
                        description: Report::new(description),
                        layout: None,
                    }))
                }
                ExportFormat::Hashes { path } => Self::Hashes(Box::new(Report::new(
                    Artifact::create(control, ExportArtifactKind::Hashes, path, source)?,
                ))),
            };
        Ok(output)
    }

    pub fn push(
        &mut self,
        output: Output,
        evidence: Evidence,
        request: &ExportRequest,
        timing: Status,
        control: &dyn ExportControl,
    ) -> Result<(), Stop> {
        let metadata = Metadata::of(&output.picture);
        let record = Record {
            ordinal: output.ordinal,
            layer: samples::layer(&output.picture),
            unit_offset: output.unit.offset,
            timing: evidence,
            metadata,
        };
        let header = Header::of(request);
        match self {
            Self::Y4m(video) => {
                metadata.check_y4m()?;
                video.push(output.picture, timing, control)
            }
            Self::Raw(raw) => match samples::frame(&output.picture)? {
                PictureFrame::Eight(frame) => raw.push(frame, record, header, control),
                PictureFrame::Ten(frame) => raw.push(frame, record, header, control),
            },
            Self::Hashes(report) => match samples::frame(&output.picture)? {
                PictureFrame::Eight(frame) => hash(report, frame, record, header, control),
                PictureFrame::Ten(frame) => hash(report, frame, record, header, control),
            },
        }
    }

    pub fn frames_written(&self) -> u64 {
        match self {
            Self::Y4m(video) => video
                .writer
                .as_ref()
                .map_or(0, |writer| writer.writer.frames_written()),
            Self::Raw(raw) => raw.description.frames,
            Self::Hashes(report) => report.frames,
        }
    }

    pub fn bytes_written(&self) -> u64 {
        match self {
            Self::Y4m(video) => video.writer.as_ref().map_or(0, |writer| writer.bytes.get()),
            Self::Raw(raw) => raw.samples.bytes_written() + raw.description.file.bytes_written(),
            Self::Hashes(report) => report.file.bytes_written(),
        }
    }

    pub fn finish(
        self,
        timing: Status,
        control: &dyn ExportControl,
    ) -> Result<Vec<Verified>, Stop> {
        match self {
            Self::Y4m(mut video) => {
                video.start(timing, control)?;
                let writer = video.writer.take().ok_or_else(no_output)?;
                Ok(vec![writer.writer.into_inner().verify()?])
            }
            Self::Raw(raw) => {
                let frame_bytes = raw.layout.as_ref().ok_or_else(no_output)?.frame_bytes;
                let expected_bytes = raw
                    .description
                    .frames
                    .checked_mul(frame_bytes)
                    .ok_or_else(|| unsupported("The packed sample file is too large."))?;
                let layout = raw.description.finish(timing, control)?.verify()?;
                let samples = raw.samples.verify()?;
                if samples.bytes != expected_bytes {
                    return Err(Stop::failed(
                        studio_domain::job::FailureCode::VerificationFailed,
                        "The raw sample count does not match the layout.",
                    ));
                }
                Ok(vec![layout, samples])
            }
            Self::Hashes(report) => Ok(vec![report.finish(timing, control)?.verify()?]),
        }
    }
}

pub(super) struct Video<'a> {
    file: Option<Artifact<'a>>,
    writer: Option<VideoWriter<'a>>,
    first: Option<Picture>,
    rate: Option<Rational>,
}

struct VideoWriter<'a> {
    writer: Y4mWriter<Artifact<'a>>,
    bytes: Rc<Cell<u64>>,
}

impl Video<'_> {
    fn push(
        &mut self,
        picture: Picture,
        timing: Status,
        control: &dyn ExportControl,
    ) -> Result<(), Stop> {
        if self.file.is_some() && self.first.is_none() {
            // A single IVF timestamp does not establish a frame rate.
            self.first = Some(picture);
            return Ok(());
        }
        self.start(timing, control)?;
        self.writer
            .as_mut()
            .ok_or_else(no_output)?
            .write(&picture, control)
    }

    fn start(&mut self, timing: Status, control: &dyn ExportControl) -> Result<(), Stop> {
        let rate = timing.y4m_rate().map_err(unsupported)?;
        if self.rate.is_some_and(|previous| previous != rate) {
            return Err(unsupported(
                "The selected outputs have variable timing. Choose raw samples.",
            ));
        }
        if self.writer.is_some() {
            return Ok(());
        }
        let file = self.file.take().ok_or_else(no_output)?;
        let first = self.first.take().ok_or_else(no_output)?;
        let y4m_rate = Y4mFrameRate::new(rate.num, rate.den).map_err(|e| y4m_failed(control, e))?;
        let mut writer = VideoWriter {
            bytes: file.byte_counter(),
            writer: match samples::frame(&first)? {
                PictureFrame::Eight(frame) => Y4mWriter::from_frame(file, frame, y4m_rate),
                PictureFrame::Ten(frame) => Y4mWriter::from_frame(file, frame, y4m_rate),
            }
            .map_err(|e| y4m_failed(control, e))?,
        };
        writer.write(&first, control)?;
        self.writer = Some(writer);
        self.rate = Some(rate);
        Ok(())
    }
}

impl VideoWriter<'_> {
    fn write(&mut self, picture: &Picture, control: &dyn ExportControl) -> Result<(), Stop> {
        match samples::frame(picture)? {
            PictureFrame::Eight(frame) => self.writer.write_frame(frame),
            PictureFrame::Ten(frame) => self.writer.write_frame(frame),
        }
        .map_err(|e| y4m_failed(control, e))
    }
}

fn y4m_failed(control: &dyn ExportControl, error: Y4mError) -> Stop {
    match error {
        Y4mError::Io { .. } => artifact::failed(control, error),
        _ => unsupported(format!(
            "Y4M cannot represent the selected outputs. {error}"
        )),
    }
}

pub(super) struct Raw<'a> {
    samples: Artifact<'a>,
    description: Report<'a>,
    layout: Option<Layout>,
}

impl Raw<'_> {
    fn push<T: ReconSample>(
        &mut self,
        frame: &DecodedFrame<T>,
        record: Record,
        common: Header<'_>,
        control: &dyn ExportControl,
    ) -> Result<(), Stop> {
        let layout = Layout::of(frame)?;
        if let Some(expected) = &self.layout {
            if expected != &layout {
                return Err(unsupported(
                    "The visible sample layout changes. Export each layout in its own range.",
                ));
            }
        } else {
            self.description.start(
                &RawHeader {
                    common,
                    sample_layout: DecodedFrameHash::BYTE_STREAM_ID,
                    layout: &layout,
                },
                control,
            )?;
            self.layout = Some(layout.clone());
        }
        let offset = self
            .description
            .frames
            .checked_mul(layout.frame_bytes)
            .ok_or_else(|| unsupported("The packed sample file is too large."))?;
        DecodedFrameHashInput::new(frame)
            .write_to(&mut self.samples)
            .map_err(|e| artifact::failed(control, e))?;
        self.description.add(&RawRecord { record, offset }, control)
    }
}

fn hash<T: ReconSample>(
    report: &mut Report<'_>,
    frame: &DecodedFrame<T>,
    record: Record,
    common: Header<'_>,
    control: &dyn ExportControl,
) -> Result<(), Stop> {
    if report.frames == 0 {
        report.start(
            &HashHeader {
                common,
                contract: DecodedFrameHash::CONTRACT_ID,
                version: DecodedFrameHash::CONTRACT_VERSION,
                algorithm: DecodedFrameHash::ALGORITHM_ID,
                byte_stream: DecodedFrameHash::BYTE_STREAM_ID,
                variant: DecodedFrameHash::VARIANT_ID,
            },
            control,
        )?;
    }
    let hash = DecodedFrameHashInput::new(frame).compute_hash().to_hex();
    report.add(
        &HashRecord {
            record,
            layout: Layout::of(frame)?,
            hash,
        },
        control,
    )
}

pub(super) struct Report<'a> {
    file: Artifact<'a>,
    frames: u64,
}

impl<'a> Report<'a> {
    const fn new(file: Artifact<'a>) -> Self {
        Self { file, frames: 0 }
    }

    fn start(&mut self, header: &impl Serialize, control: &dyn ExportControl) -> Result<(), Stop> {
        let serialized = serde_json::to_vec(header).map_err(|e| artifact::failed(control, e))?;
        let fields = serialized
            .strip_suffix(b"}")
            .ok_or_else(|| artifact::failed(control, "Invalid export header."))?;
        self.file
            .write_all(fields)
            .and_then(|()| self.file.write_all(b",\"outputs\":[\n"))
            .map_err(|e| artifact::failed(control, e))
    }

    fn add(&mut self, item: &impl Serialize, control: &dyn ExportControl) -> Result<(), Stop> {
        if self.frames != 0 {
            self.file
                .write_all(b",\n")
                .map_err(|e| artifact::failed(control, e))?;
        }
        serde_json::to_writer(&mut self.file, item).map_err(|e| artifact::failed(control, e))?;
        self.frames += 1;
        Ok(())
    }

    fn finish(mut self, timing: Status, control: &dyn ExportControl) -> Result<Artifact<'a>, Stop> {
        let finish = |file: &mut Artifact<'_>| -> io::Result<()> {
            write!(file, "\n],\"frames\":{},\"timing\":", self.frames)?;
            serde_json::to_writer(&mut *file, &timing)?;
            file.write_all(b"}\n")
        };
        finish(&mut self.file).map_err(|e| artifact::failed(control, e))?;
        Ok(self.file)
    }
}

#[derive(Serialize)]
struct Header<'a> {
    schema: u16,
    source_digest: &'a SourceDigest,
    range: ExportRange,
    layer: Option<ExportLayer>,
    grain: GrainPolicy,
    selection: studio_domain::layers::LayerDecodeSelection,
    sample_format: ExportSampleFormat,
}

impl<'a> Header<'a> {
    const fn of(request: &'a ExportRequest) -> Self {
        Self {
            schema: 1,
            source_digest: &request.source.digest,
            range: request.range,
            layer: request.layer,
            grain: request.grain,
            selection: request.selection,
            sample_format: request.expected,
        }
    }
}

#[derive(Serialize)]
struct RawHeader<'a> {
    #[serde(flatten)]
    common: Header<'a>,
    sample_layout: &'static str,
    #[serde(flatten)]
    layout: &'a Layout,
}

#[derive(Serialize)]
struct HashHeader<'a> {
    #[serde(flatten)]
    common: Header<'a>,
    contract: &'static str,
    version: u32,
    algorithm: &'static str,
    byte_stream: &'static str,
    variant: &'static str,
}

#[derive(Serialize)]
struct Record {
    ordinal: u64,
    layer: ExportLayer,
    unit_offset: u64,
    timing: Evidence,
    #[serde(flatten)]
    metadata: Metadata,
}

#[derive(Serialize)]
struct RawRecord {
    #[serde(flatten)]
    record: Record,
    offset: u64,
}

#[derive(Serialize)]
struct HashRecord {
    #[serde(flatten)]
    record: Record,
    #[serde(flatten)]
    layout: Layout,
    hash: String,
}

fn no_output() -> Stop {
    unsupported("No decoded outputs match the selection.")
}
