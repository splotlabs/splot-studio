// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The Compare workspace: one pair of inputs and what the engine read from
//! them.
//!
//! A pair is made from a completed encode and the source its job recorded,
//! or from two AV2 files the user picks. Both sides are read by a dedicated
//! inspect process; [`Compare::event`] turns its answers into a
//! [`ComparisonSession`]. Policy edits change the session only. They never
//! touch a queue item or its job.
//!
//! The journal records the inputs, the policy and the identity of the facts
//! last read ([`SavedComparison`]). No fact is shown from the journal: a
//! restored pair is read again when Compare is shown. The stored identity
//! only detects a change, which is then reported.

use serde::{Deserialize, Serialize};
use studio_domain::analysis::AnalysisFailure;
use studio_domain::compare::{
    AlignmentBasis, ComparisonSession, ComparisonSetup, DescribeInput, EvidenceIdentity,
    PairingRefusal, Reference, Side, SideFacts, StreamInput,
};
use studio_domain::preview::GrainPolicy;
use studio_domain::protocol::Event;

/// The comparison the journal records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedComparison {
    /// The inputs and the policy.
    pub setup: ComparisonSetup,
    /// The identity of the facts last read for them, when both sides were read.
    pub identity: Option<EvidenceIdentity>,
}

impl SavedComparison {
    /// Whether every path of the comparison names a file on this kind of system.
    #[must_use]
    pub fn is_native(&self) -> bool {
        let stream = |input: &StreamInput| {
            input.path.to_path_buf().is_ok()
                && input
                    .encode
                    .as_ref()
                    .is_none_or(|encode| encode.source.path.to_path_buf().is_ok())
        };
        stream(&self.setup.b)
            && match &self.setup.a {
                Reference::EncodeSource => true,
                Reference::Stream(input) => stream(input),
            }
    }
}

/// Something the user did in the Compare workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// Compare with Source, on the shown converted file.
    PairResult,
    /// Choose File, for one side.
    Pick(Side),
    /// An alignment basis, by its index in the list the workspace shows.
    Basis(usize),
    /// The index offset of the assumed alignment.
    Offset(i64),
    /// Apply film grain, or compare the pictures before grain.
    Grain(bool),
    /// Close the comparison, or stop its read.
    Close,
}

/// What the session must do for the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Effect {
    /// Start a new inspect process and send these describe requests. A
    /// process that still reads for an older pair is let go first.
    Read(Vec<(u64, DescribeInput)>),
    /// Let the inspect process go.
    Stop,
}

/// The sides still being read.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reading {
    setup: ComparisonSetup,
    /// The request of each side, A then B.
    requests: [u64; 2],
    facts: [Option<SideFacts>; 2],
    codec_revision: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Pair {
    /// Restored from the journal and not read in this session.
    Saved(ComparisonSetup),
    Reading(Box<Reading>),
    Ready(Box<ComparisonSession>),
    Failed {
        setup: ComparisonSetup,
        message: String,
    },
}

impl Pair {
    fn setup(&self) -> &ComparisonSetup {
        match self {
            Self::Saved(setup) | Self::Failed { setup, .. } => setup,
            Self::Reading(reading) => &reading.setup,
            Self::Ready(session) => &session.setup,
        }
    }

    fn setup_mut(&mut self) -> &mut ComparisonSetup {
        match self {
            Self::Saved(setup) | Self::Failed { setup, .. } => setup,
            Self::Reading(reading) => &mut reading.setup,
            Self::Ready(session) => &mut session.setup,
        }
    }
}

/// What the workspace shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status<'a> {
    /// No pair. The files picked so far, A then B.
    Empty([Option<&'a StreamInput>; 2]),
    /// The engine reads the files of this pair.
    Reading(&'a ComparisonSetup),
    /// Both sides are read.
    Ready(&'a ComparisonSession),
    /// The pair cannot be compared.
    Failed {
        /// The inputs.
        setup: &'a ComparisonSetup,
        /// Why.
        message: &'a str,
    },
}

impl<'a> Status<'a> {
    /// The inputs and policy of the pair, when there is one.
    #[must_use]
    pub const fn setup(&self) -> Option<&'a ComparisonSetup> {
        match self {
            Self::Empty(_) => None,
            Self::Reading(setup) | Self::Failed { setup, .. } => Some(setup),
            Self::Ready(session) => Some(&session.setup),
        }
    }
}

/// The Compare workspace's model.
#[derive(Debug, Default)]
pub struct Compare {
    /// Files picked while the pair is incomplete, A then B.
    picked: [Option<StreamInput>; 2],
    pair: Option<Pair>,
    /// The identity of the facts the journal recorded.
    expected: Option<EvidenceIdentity>,
    notice: Option<String>,
    next_request: u64,
    dirty: bool,
}

impl Compare {
    /// The model of a journalled comparison. Nothing is read yet.
    #[must_use]
    pub fn restored(saved: Option<SavedComparison>) -> Self {
        let mut compare = Self::default();
        if let Some(saved) = saved {
            compare.pair = Some(Pair::Saved(saved.setup));
            compare.expected = saved.identity;
        }
        compare
    }

    /// What the journal records.
    #[must_use]
    pub fn saved(&self) -> Option<SavedComparison> {
        let pair = self.pair.as_ref()?;
        Some(SavedComparison {
            setup: pair.setup().clone(),
            identity: match pair {
                Pair::Ready(session) => session.identity(),
                _ => self.expected.clone(),
            },
        })
    }

    /// What the workspace shows.
    #[must_use]
    pub fn status(&self) -> Status<'_> {
        match &self.pair {
            None => Status::Empty([self.picked[0].as_ref(), self.picked[1].as_ref()]),
            Some(Pair::Saved(setup)) => Status::Reading(setup),
            Some(Pair::Reading(reading)) => Status::Reading(&reading.setup),
            Some(Pair::Ready(session)) => Status::Ready(session),
            Some(Pair::Failed { setup, message }) => Status::Failed { setup, message },
        }
    }

    /// A message about the pair that is not a failure.
    #[must_use]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Whether the model changed since the last call.
    pub const fn take_dirty(&mut self) -> bool {
        std::mem::replace(&mut self.dirty, false)
    }

    /// Whether an inspect process must run.
    pub(crate) const fn reading(&self) -> bool {
        matches!(self.pair, Some(Pair::Reading(_)))
    }

    /// Pairs a completed encode with the source its job recorded.
    pub(crate) fn pair_encode(&mut self, result: StreamInput) -> Vec<Effect> {
        self.start(ComparisonSetup::new(Reference::EncodeSource, result))
    }

    /// Sets the stream of `side`. The pair is read once both sides are set.
    pub(crate) fn pick(&mut self, side: Side, input: StreamInput) -> Vec<Effect> {
        self.dirty = true;
        let (a, b) = match (self.pair.take(), side) {
            (Some(pair), Side::A) => (Some(input), Some(pair.setup().b.clone())),
            (Some(pair), Side::B) => match &pair.setup().a {
                Reference::Stream(a) => (Some(a.clone()), Some(input)),
                // The source belonged to the replaced result.
                Reference::EncodeSource => (None, Some(input)),
            },
            (None, Side::A) => (Some(input), self.picked[1].take()),
            (None, Side::B) => (self.picked[0].take(), Some(input)),
        };
        match (a, b) {
            (Some(a), Some(b)) => self.start(ComparisonSetup::new(Reference::Stream(a), b)),
            (a, b) => {
                self.picked = [a, b];
                self.expected = None;
                self.notice = None;
                vec![Effect::Stop]
            }
        }
    }

    fn start(&mut self, setup: ComparisonSetup) -> Vec<Effect> {
        self.dirty = true;
        self.picked = [None, None];
        self.notice = None;
        self.expected = None;
        self.read(setup)
    }

    /// Reads a restored pair, once Compare is shown.
    pub(crate) fn read_saved(&mut self) -> Vec<Effect> {
        match self.pair.take() {
            Some(Pair::Saved(setup)) => self.read(setup),
            pair => {
                self.pair = pair;
                Vec::new()
            }
        }
    }

    fn read(&mut self, setup: ComparisonSetup) -> Vec<Effect> {
        self.dirty = true;
        let inputs = setup
            .describe(Side::A)
            .and_then(|a| Ok((a, setup.describe(Side::B)?)));
        match inputs {
            Ok((a, b)) => {
                let requests = [self.next_request + 1, self.next_request + 2];
                self.next_request += 2;
                self.pair = Some(Pair::Reading(Box::new(Reading {
                    setup,
                    requests,
                    facts: [None, None],
                    codec_revision: None,
                })));
                vec![Effect::Read(vec![(requests[0], a), (requests[1], b)])]
            }
            Err(refusal) => {
                self.pair = Some(Pair::Failed {
                    setup,
                    message: refusal.to_string(),
                });
                vec![Effect::Stop]
            }
        }
    }

    /// Changes the alignment policy of the pair.
    pub(crate) fn set_basis(&mut self, basis: AlignmentBasis) {
        if let Some(pair) = &mut self.pair {
            pair.setup_mut().basis = basis;
            self.dirty = true;
        }
    }

    /// Changes the film grain policy of the pair.
    pub(crate) fn set_grain(&mut self, grain: GrainPolicy) {
        if let Some(pair) = &mut self.pair {
            pair.setup_mut().grain = grain;
            self.dirty = true;
        }
    }

    /// Forgets the pair and the picked files.
    pub(crate) fn close(&mut self) -> Vec<Effect> {
        *self = Self {
            next_request: self.next_request,
            dirty: true,
            ..Self::default()
        };
        vec![Effect::Stop]
    }

    fn fail(&mut self, message: String) -> Vec<Effect> {
        if let Some(pair) = self.pair.take() {
            self.pair = Some(Pair::Failed {
                setup: pair.setup().clone(),
                message,
            });
            self.dirty = true;
        }
        vec![Effect::Stop]
    }

    /// The inspect process could not start or ended while it read.
    pub(crate) fn interrupted(&mut self, detail: &str) -> Vec<Effect> {
        if self.reading() {
            self.fail(format!(
                "The engine stopped before it read both files. {detail}"
            ))
        } else {
            Vec::new()
        }
    }

    /// One answer from the inspect process. An answer to a request of an
    /// older pair changes nothing.
    pub(crate) fn event(&mut self, request: u64, event: Event) -> Vec<Effect> {
        let Some(Pair::Reading(reading)) = &mut self.pair else {
            return Vec::new();
        };
        let Some(at) = reading.requests.iter().position(|id| *id == request) else {
            return Vec::new();
        };
        let side = [Side::A, Side::B][at];
        match event {
            Event::CompareDescribed {
                codec_revision,
                facts,
            } => {
                reading.facts[at] = Some(*facts);
                reading.codec_revision = Some(codec_revision);
                self.dirty = true;
                self.resolve()
            }
            Event::CompareFailed { code, message } => {
                let source = matches!(reading.setup.a, Reference::EncodeSource) && side == Side::A;
                self.fail(if source && code == AnalysisFailure::SourceChanged {
                    PairingRefusal::SourceChanged.to_string()
                } else {
                    format!("Studio cannot read side {side}. {message}")
                })
            }
            Event::Error { message, .. } => {
                self.fail(format!("The engine refused the request. {message}"))
            }
            _ => Vec::new(),
        }
    }

    /// Makes the session once both sides are read.
    fn resolve(&mut self) -> Vec<Effect> {
        let Some(Pair::Reading(reading)) = self.pair.take() else {
            return Vec::new();
        };
        let Reading {
            mut setup,
            facts: [Some(a), Some(b)],
            codec_revision: Some(codec_revision),
            ..
        } = *reading
        else {
            self.pair = Some(Pair::Reading(reading));
            return Vec::new();
        };
        // The app attached the job of a picked file by its path. A file that
        // the job did not publish keeps no settings.
        if let Reference::Stream(input) = &mut setup.a {
            let dropped: Vec<&str> = [
                (drop_other_job(input, &a), "A"),
                (drop_other_job(&mut setup.b, &b), "B"),
            ]
            .into_iter()
            .filter_map(|(dropped, side)| dropped.then_some(side))
            .collect();
            if !dropped.is_empty() {
                self.notice = Some(format!(
                    "The file of side {} is not the file that its encode job published. \
                     Studio does not use the settings of that job.",
                    dropped.join(" and side ")
                ));
            }
            if setup.basis == AlignmentBasis::Provenance && setup.common_source().is_none() {
                setup.basis = AlignmentBasis::Timestamps;
            }
        }
        match ComparisonSession::resolve(setup.clone(), codec_revision, a, b) {
            Ok(session) => {
                let identity = session.identity();
                // A dropped job already says what changed.
                if self.expected.is_some() && self.expected != identity && self.notice.is_none() {
                    self.notice = Some(
                        "A file or the codec changed after Studio saved this comparison. \
                         Earlier results for it do not apply."
                            .into(),
                    );
                }
                self.expected = identity;
                self.pair = Some(Pair::Ready(Box::new(session)));
            }
            Err(refusal) => {
                self.pair = Some(Pair::Failed {
                    setup,
                    message: refusal.to_string(),
                });
            }
        }
        vec![Effect::Stop]
    }
}

/// Removes a job that did not publish the file that was read. The app
/// attached the job by the path alone.
fn drop_other_job(input: &mut StreamInput, facts: &SideFacts) -> bool {
    let other = input
        .encode
        .as_ref()
        .is_some_and(|encode| !encode.published(facts));
    if other {
        input.encode = None;
    }
    other
}

#[cfg(test)]
mod tests;
