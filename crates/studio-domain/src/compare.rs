// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Comparison sessions: paired inputs, sample-domain compatibility, exact
//! output alignment and configuration differences.
//!
//! A [`ComparisonSetup`] names two immutable inputs and the policy the user
//! chose. Side B is always an AV2 stream. Side A is another AV2 stream or the
//! source that the encode job of side B read. No other source can be paired:
//! a bitstream does not identify its original source.
//!
//! The engine describes each side as [`SideFacts`]. [`ComparisonSession::resolve`]
//! checks the facts against the encode provenance and refuses a pair whose
//! files no longer match their job. All later results are pure functions of
//! the session:
//!
//! - [`ComparisonSession::compatibility`] says whether sample values can be
//!   compared without a conversion. No conversion is supported.
//! - [`ComparisonSession::alignment`] maps decoder output ordinals of side A
//!   to those of side B. It never uses a coded frame index.
//! - [`ComparisonSession::configuration`] lists the settings that differ.
//! - [`ComparisonSession::identity`] is the key of all evidence computed for
//!   the session. Evidence is valid only while the key is equal.

use std::fmt;

mod align;
mod config;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use self::align::{
    AlignmentRefusal, Clock, ClockBuilder, ClockFault, ClockRun, MAX_ALIGNMENT_SEGMENTS,
    MAX_CLOCK_RUNS, Mapping, Segment, Track, align_by_offset, align_by_timestamps,
};
pub use self::config::{
    ConfigCell, ConfigComparison, ConfigDifference, ConfigPair, ConfigRow, compare_configuration,
};

use crate::analysis::SourceDigest;
use crate::job::{ChromaLayout, JobPurpose, JobReport, JobSpec, SourceFingerprint, WirePath};
use crate::layers::LayerDecodeSelection;
use crate::options::{EffectiveOption, OptionRequest};
use crate::preview::{GrainPolicy, SourceRef};
use crate::protocol::OptionsIdentity;

/// The schema of comparison values and of the evidence keyed by them.
pub const COMPARE_SCHEMA: u32 = 1;

/// One side of a comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// The reference side.
    A,
    /// The result side.
    B,
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::A => "A",
            Self::B => "B",
        })
    }
}

/// What a completed encode job recorded about the stream it published.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncodeProvenance {
    /// The source the job read.
    pub source: SourceRef,
    /// The option schema and codec revision of the job. Historical jobs have none.
    pub codec: Option<OptionsIdentity>,
    /// The settings the job requested. A missing key inherits its value.
    pub requested: OptionRequest,
    /// The effective policy of the job. Empty when the job did not record it.
    pub effective: Vec<EffectiveOption>,
    /// The number of frames the job read and encoded.
    pub frames: u64,
    /// The size of the published file in bytes.
    pub bytes: u64,
}

impl EncodeProvenance {
    /// The provenance of a completed encode. A first-pass job publishes no stream.
    #[must_use]
    pub fn of(job: &JobSpec, report: &JobReport, policy: &[EffectiveOption]) -> Option<Self> {
        (job.purpose == JobPurpose::Encode).then(|| Self {
            source: SourceRef {
                path: job.source.clone(),
                fingerprint: job.fingerprint,
                video_stream: job.video_stream,
            },
            codec: job.identity.clone(),
            requested: job.settings.clone(),
            effective: policy.to_vec(),
            frames: report.frames,
            bytes: report.bytes,
        })
    }

    /// Whether `facts` describe the file this job published: the same size
    /// and the same number of pictures.
    #[must_use]
    pub fn published(&self, facts: &SideFacts) -> bool {
        facts.fingerprint.len == self.bytes && facts.outputs == Some(self.frames)
    }
}

/// One AV2 stream of a comparison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamInput {
    /// The local file. It is never modified.
    pub path: WirePath,
    /// The decoder operating point that gives output ordinals their meaning.
    #[serde(default)]
    pub selection: LayerDecodeSelection,
    /// The encode job that published this file, when one is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encode: Option<Box<EncodeProvenance>>,
}

/// Side A of a comparison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reference {
    /// The source that the encode job of side B read.
    EncodeSource,
    /// Another AV2 stream.
    Stream(StreamInput),
}

/// How output ordinals of side A map to those of side B.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AlignmentBasis {
    /// Both sides present the frames of one source in the order the encode job read them.
    Provenance,
    /// Outputs with equal exact presentation timestamps match.
    Timestamps,
    /// A user assumption: output `n` of side A matches output `n + offset` of side B.
    AssumedOffset {
        /// The index offset from side A to side B.
        offset: i64,
    },
}

/// The inputs and the policy of one comparison. Edits change this value only.
/// The encode jobs it names stay immutable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComparisonSetup {
    /// Side A.
    pub a: Reference,
    /// Side B.
    pub b: StreamInput,
    /// The film grain policy for all stream sides.
    #[serde(default)]
    pub grain: GrainPolicy,
    /// The alignment policy.
    pub basis: AlignmentBasis,
}

/// What the engine reads to describe one side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DescribeInput {
    /// A source, probed again and checked against its fingerprint.
    Source {
        /// The source.
        source: SourceRef,
    },
    /// An AV2 stream, decoded once from its first byte to its last.
    Stream {
        /// The local file.
        path: WirePath,
        /// The decoder operating point.
        selection: LayerDecodeSelection,
    },
}

impl ComparisonSetup {
    /// A setup with the default alignment policy for its inputs: provenance
    /// when both sides come from one source, otherwise timestamps.
    #[must_use]
    pub fn new(a: Reference, b: StreamInput) -> Self {
        let mut setup = Self {
            a,
            b,
            grain: GrainPolicy::default(),
            basis: AlignmentBasis::Timestamps,
        };
        if setup.common_source().is_some() {
            setup.basis = AlignmentBasis::Provenance;
        }
        setup
    }

    /// What the engine must read for `side`.
    ///
    /// # Errors
    /// [`PairingRefusal::NoEncode`] when side A is the encode source and side B
    /// has no encode provenance.
    pub fn describe(&self, side: Side) -> Result<DescribeInput, PairingRefusal> {
        let stream = |input: &StreamInput| DescribeInput::Stream {
            path: input.path.clone(),
            selection: input.selection,
        };
        match (side, &self.a) {
            (Side::B, _) => Ok(stream(&self.b)),
            (Side::A, Reference::Stream(input)) => Ok(stream(input)),
            (Side::A, Reference::EncodeSource) => self
                .b
                .encode
                .as_ref()
                .map(|encode| DescribeInput::Source {
                    source: encode.source.clone(),
                })
                .ok_or(PairingRefusal::NoEncode),
        }
    }

    /// The encode provenance of `side`, when its stream has one.
    #[must_use]
    pub fn encode(&self, side: Side) -> Option<&EncodeProvenance> {
        match (side, &self.a) {
            (Side::B, _) => self.b.encode.as_deref(),
            (Side::A, Reference::Stream(input)) => input.encode.as_deref(),
            (Side::A, Reference::EncodeSource) => None,
        }
    }

    /// The one source both sides present, when their provenance proves it.
    #[must_use]
    pub fn common_source(&self) -> Option<&SourceRef> {
        let b = &self.b.encode.as_ref()?.source;
        match &self.a {
            Reference::EncodeSource => Some(b),
            Reference::Stream(input) => {
                let a = &input.encode.as_ref()?.source;
                (a == b).then_some(b)
            }
        }
    }
}

/// The sample format and declared interpretation of all outputs of one side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SampleDomain {
    /// Visible luma width.
    pub width: u32,
    /// Visible luma height.
    pub height: u32,
    /// Bits per sample.
    pub bit_depth: u8,
    /// The chroma layout.
    pub chroma: ChromaLayout,
    /// H.273 colour primaries, when declared.
    pub primaries: Option<u8>,
    /// H.273 transfer characteristics, when declared.
    pub transfer: Option<u8>,
    /// H.273 matrix coefficients, when declared.
    pub matrix: Option<u8>,
    /// Full range when true and limited range when false, when declared.
    pub full_range: Option<bool>,
    /// AV2 chroma sample position codes for the top and bottom field, when declared.
    pub chroma_position: Option<(u32, u32)>,
    /// The stream signals film grain for at least one output. False for a source.
    pub film_grain: bool,
}

/// What the engine read from one side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SideFacts {
    /// The file as it was read.
    pub fingerprint: SourceFingerprint,
    /// SHA-256 of the whole file. A source is not hashed.
    pub digest: Option<SourceDigest>,
    /// The number of outputs in display order. `None` when a source cannot
    /// count its frames without decoding.
    pub outputs: Option<u64>,
    /// The sample domain. `None` when the format changes inside the stream.
    pub domain: Option<SampleDomain>,
    /// The presentation clock.
    pub clock: Clock,
}

/// Why a pair of inputs cannot become a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PairingRefusal {
    /// Side A is the encode source, but side B has no encode job.
    #[error("Side B has no recorded encode job. Studio cannot identify its source.")]
    NoEncode,
    /// The source no longer matches the fingerprint its encode job recorded.
    #[error("The source changed after the encode. Studio cannot verify it.")]
    SourceChanged,
    /// The stream is not the file its encode job published.
    #[error("The file of side {0} is not the file its encode job published.")]
    ResultChanged(Side),
    /// The engine did not count the outputs of a stream.
    #[error("The output count of side {0} is unknown.")]
    Uncounted(Side),
}

/// The inputs that evidence was computed from, and the policy it used.
/// Evidence is valid only for a session with an equal identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceIdentity {
    /// [`COMPARE_SCHEMA`].
    pub schema: u32,
    /// The Splot revision of the engine that described both sides.
    pub codec_revision: String,
    /// Side A.
    pub a: InputIdentity,
    /// Side B.
    pub b: InputIdentity,
    /// The film grain policy.
    pub grain: GrainPolicy,
    /// The alignment policy.
    pub basis: AlignmentBasis,
}

/// The identity of one input of a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputIdentity {
    /// The file and the selected track or layers.
    pub input: DescribeInput,
    /// The file as the engine read it.
    pub fingerprint: SourceFingerprint,
    /// SHA-256 of the whole file, when the engine hashed it.
    pub digest: Option<SourceDigest>,
}

/// A property that both sides must share for an exact sample comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DomainProperty {
    /// Visible luma width.
    Width,
    /// Visible luma height.
    Height,
    /// Bits per sample.
    BitDepth,
    /// Chroma layout.
    Chroma,
    /// Colour primaries.
    Primaries,
    /// Transfer characteristics.
    Transfer,
    /// Matrix coefficients.
    Matrix,
    /// Full or limited range.
    Range,
    /// Chroma sample position.
    ChromaPosition,
    /// Whether the samples include synthesized film grain under the session policy.
    Grain,
}

/// Why sample values cannot be compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DomainMismatch {
    /// The sample format changes inside the stream of this side.
    FormatVaries(Side),
    /// Both sides state this property and the values differ.
    Differs(DomainProperty),
}

/// Whether sample values of matched outputs can be compared. No conversion,
/// crop, scale or depth change is applied in either case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compatibility {
    /// Code values can be compared as they are.
    Exact {
        /// Colour properties that at least one side does not declare. They
        /// do not change code values.
        undeclared: Vec<DomainProperty>,
    },
    /// Numerical comparison is unavailable.
    Refused {
        /// Every mismatch found.
        reasons: Vec<DomainMismatch>,
    },
}

/// A pair of inputs with the facts the engine read from them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComparisonSession {
    /// The inputs and policy.
    pub setup: ComparisonSetup,
    /// The Splot revision of the engine that described both sides.
    pub codec_revision: String,
    /// What the engine read from side A.
    pub a: SideFacts,
    /// What the engine read from side B.
    pub b: SideFacts,
}

impl ComparisonSession {
    /// Checks the facts of both sides against the setup.
    ///
    /// # Errors
    /// When the source or a result no longer matches its encode job, or a
    /// stream has no output count.
    pub fn resolve(
        setup: ComparisonSetup,
        codec_revision: String,
        a: SideFacts,
        b: SideFacts,
    ) -> Result<Self, PairingRefusal> {
        let published = |side, facts: &SideFacts| {
            if facts.outputs.is_none() {
                return Err(PairingRefusal::Uncounted(side));
            }
            match setup.encode(side) {
                Some(encode) if !encode.published(facts) => {
                    Err(PairingRefusal::ResultChanged(side))
                }
                _ => Ok(()),
            }
        };
        published(Side::B, &b)?;
        match &setup.a {
            Reference::Stream(_) => published(Side::A, &a)?,
            Reference::EncodeSource => {
                let encode = setup.b.encode.as_ref().ok_or(PairingRefusal::NoEncode)?;
                // A source that counts its frames must still hold the frames the job read.
                if a.fingerprint != encode.source.fingerprint
                    || a.outputs.is_some_and(|frames| frames != encode.frames)
                {
                    return Err(PairingRefusal::SourceChanged);
                }
            }
        }
        Ok(Self {
            setup,
            codec_revision,
            a,
            b,
        })
    }

    /// The facts of `side`.
    #[must_use]
    pub const fn facts(&self, side: Side) -> &SideFacts {
        match side {
            Side::A => &self.a,
            Side::B => &self.b,
        }
    }

    /// The number of outputs of `side`. For the encode source this is the
    /// number of frames the job read.
    #[must_use]
    pub fn outputs(&self, side: Side) -> u64 {
        match (side, &self.setup.a, &self.setup.b.encode) {
            (Side::A, Reference::EncodeSource, Some(encode)) => encode.frames,
            _ => self.facts(side).outputs.unwrap_or(0),
        }
    }

    /// The key of all evidence computed for this session. `None` only for a
    /// session that [`Self::resolve`] did not make.
    #[must_use]
    pub fn identity(&self) -> Option<EvidenceIdentity> {
        let input = |side| {
            let facts = self.facts(side);
            Some(InputIdentity {
                input: self.setup.describe(side).ok()?,
                fingerprint: facts.fingerprint,
                digest: facts.digest.clone(),
            })
        };
        Some(EvidenceIdentity {
            schema: COMPARE_SCHEMA,
            codec_revision: self.codec_revision.clone(),
            a: input(Side::A)?,
            b: input(Side::B)?,
            grain: self.setup.grain,
            basis: self.setup.basis,
        })
    }

    /// Whether sample values of matched outputs can be compared as they are.
    #[must_use]
    pub fn compatibility(&self) -> Compatibility {
        let (Some(a), Some(b)) = (self.a.domain, self.b.domain) else {
            let reasons = [(Side::A, &self.a), (Side::B, &self.b)]
                .into_iter()
                .filter(|(_, facts)| facts.domain.is_none())
                .map(|(side, _)| DomainMismatch::FormatVaries(side))
                .collect();
            return Compatibility::Refused { reasons };
        };
        let grain =
            |domain: SampleDomain| domain.film_grain && self.setup.grain == GrainPolicy::Apply;
        let fixed = [
            (DomainProperty::Width, a.width == b.width),
            (DomainProperty::Height, a.height == b.height),
            (DomainProperty::BitDepth, a.bit_depth == b.bit_depth),
            (DomainProperty::Chroma, a.chroma == b.chroma),
            (DomainProperty::Grain, grain(a) == grain(b)),
        ];
        let declared = [
            (DomainProperty::Primaries, same(a.primaries, b.primaries)),
            (DomainProperty::Transfer, same(a.transfer, b.transfer)),
            (DomainProperty::Matrix, same(a.matrix, b.matrix)),
            (DomainProperty::Range, same(a.full_range, b.full_range)),
            (
                DomainProperty::ChromaPosition,
                same(a.chroma_position, b.chroma_position),
            ),
        ];
        let reasons: Vec<_> = fixed
            .into_iter()
            .map(|(property, equal)| (property, Some(equal)))
            .chain(declared)
            .filter(|(_, equal)| *equal == Some(false))
            .map(|(property, _)| DomainMismatch::Differs(property))
            .collect();
        if reasons.is_empty() {
            Compatibility::Exact {
                undeclared: declared
                    .into_iter()
                    .filter(|(_, equal)| equal.is_none())
                    .map(|(property, _)| property)
                    .collect(),
            }
        } else {
            Compatibility::Refused { reasons }
        }
    }

    fn track(&self, side: Side) -> Track<'_> {
        Track {
            outputs: self.outputs(side),
            clock: &self.facts(side).clock,
        }
    }

    /// The mapping of output ordinals under the session's basis.
    ///
    /// # Errors
    /// When the basis cannot map the outputs. The user must then choose
    /// another basis.
    pub fn alignment(&self) -> Result<Mapping, AlignmentRefusal> {
        let (a, b) = (self.track(Side::A), self.track(Side::B));
        match self.setup.basis {
            AlignmentBasis::Provenance if self.setup.common_source().is_none() => {
                Err(AlignmentRefusal::NoProvenance)
            }
            AlignmentBasis::Provenance => Ok(align_by_offset(a.outputs, b.outputs, 0)),
            AlignmentBasis::Timestamps => align_by_timestamps(a, b),
            AlignmentBasis::AssumedOffset { offset } => {
                Ok(align_by_offset(a.outputs, b.outputs, offset))
            }
        }
    }

    /// Whether exact timestamps give the same mapping as the session's
    /// basis. `None` when a side has no usable timestamps.
    #[must_use]
    pub fn timestamps_agree(&self) -> Option<bool> {
        let exact = |side| matches!(self.facts(side).clock, Clock::Exact { .. });
        if !(exact(Side::A) && exact(Side::B)) {
            return None;
        }
        let timed = align_by_timestamps(self.track(Side::A), self.track(Side::B));
        Some(self.alignment().is_ok_and(|mapping| Ok(mapping) == timed))
    }

    /// The differences between the recorded settings of both sides.
    #[must_use]
    pub fn configuration(&self) -> ConfigComparison {
        match (self.setup.encode(Side::A), self.setup.encode(Side::B)) {
            (Some(a), Some(b)) => ConfigComparison::Known(Box::new(compare_configuration(a, b))),
            (a, b) => ConfigComparison::Unknown {
                a: a.is_some(),
                b: b.is_some(),
            },
        }
    }
}

/// `Some(equal)` when both sides declare the value, `None` otherwise.
fn same<T: PartialEq>(a: Option<T>, b: Option<T>) -> Option<bool> {
    Some(a? == b?)
}

#[cfg(test)]
mod tests;
