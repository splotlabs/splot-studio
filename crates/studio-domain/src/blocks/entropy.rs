// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Scoped decoder entropy observations for the inspection protocol.

use serde::{Deserialize, Serialize};

use super::{BlockId, BlockTransformId, valid_label};

/// Maximum number of entropy events in one replay and protocol reply.
pub const MAX_ENTROPY_EVENTS: u32 = 96;
/// Maximum native capture bytes in one replay.
pub const MAX_ENTROPY_BYTES: u32 = 32 * 1024;
/// Number of units in one estimated bit.
pub const MICROBITS_PER_BIT: u32 = 1_000_000;

/// Exact owner of entropy observations in the coded frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntropyScope {
    /// Observations in this coding block, including its transforms.
    Block(BlockId),
    /// Coefficient reads in this exact plane transform.
    Transform(BlockTransformId),
}

impl EntropyScope {
    /// Returns the coding block that owns this scope.
    #[must_use]
    pub const fn block(self) -> BlockId {
        match self {
            Self::Block(block) => block,
            Self::Transform(transform) => transform.block,
        }
    }

    pub(super) const fn is_valid(self) -> bool {
        self.block().coded.is_valid()
            && match self {
                Self::Block(_) => true,
                Self::Transform(id) => id.coded.is_valid(),
            }
    }
}

/// Coding operation observed in the tile entropy decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntropyCoding {
    /// A symbol read with a probability model.
    Cdf,
    /// One or more equiprobable binary reads.
    Bypass,
    /// A truncated unary value read with equiprobable bits.
    Unary,
}

/// Probability state immediately before the symbol read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntropyCdf {
    /// Ascending cumulative cutoffs in Q15 units.
    pub cutoffs_q15: Vec<u16>,
    /// Adaptation count before the read.
    pub adaptation_count: u8,
    /// Index in the AV2 adaptation rate table.
    pub rate_index: u8,
    /// The read updates the row.
    pub updates: bool,
}

/// One ordered entropy observation from the selected owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntropySymbol {
    /// Zero-based ordinal in the scope. A bypass read advances by its bit count.
    pub ordinal: u64,
    /// Transform whose coefficient parse owns this operation, when available.
    pub transform: Option<BlockTransformId>,
    /// Stable syntax name, when the decoder tags this read.
    pub syntax: Option<String>,
    /// Names and values for the decoder's CDF selector.
    pub context: Option<EntropyContext>,
    /// Coding operation.
    pub coding: EntropyCoding,
    /// Value produced by the decoder.
    pub value: u32,
    /// Width of a bypass or unary read. Adaptive symbols have no bypass width.
    pub bypass_bits: Option<u8>,
    /// Probability state for this adaptive read, when available.
    pub cdf: Option<EntropyCdf>,
    /// Estimated cost rounded once to one millionth of a bit, when available.
    pub estimated_microbits: Option<u32>,
    /// Range width before an adaptive read.
    pub range: u16,
    /// Chosen interval width before renormalization.
    pub interval: u16,
}

impl EntropySymbol {
    /// Returns the exclusive end ordinal for this observation.
    #[must_use]
    pub fn end_ordinal(&self) -> Option<u64> {
        self.ordinal
            .checked_add(u64::from(self.bypass_bits.unwrap_or(1).max(1)))
    }

    /// Reports a joint U/V transform-type read.
    #[must_use]
    pub fn is_shared_between_transforms(&self) -> bool {
        self.transform.is_none() && self.syntax.as_deref() == Some("cctx_type")
    }

    fn matches(&self, scope: EntropyScope) -> bool {
        self.transform
            .is_none_or(|id| id.block == scope.block() && id.coded.is_valid())
            && match scope {
                EntropyScope::Transform(id) => {
                    self.transform == Some(id)
                        || (id.plane == crate::preview::SamplePlane::U
                            && self.is_shared_between_transforms())
                }
                EntropyScope::Block(_) => true,
            }
            && self.syntax.as_deref().is_none_or(valid_identifier)
            && self.context.as_ref().is_none_or(|context| {
                self.syntax.is_some()
                    && context.names.len() <= 48
                    && context.names.split(',').count() <= 3
                    && context.names.split(',').all(valid_identifier)
            })
            && match self.coding {
                EntropyCoding::Cdf => self.valid_cdf(),
                EntropyCoding::Bypass | EntropyCoding::Unary => {
                    self.cdf.is_none()
                        && self.bypass_bits.is_some_and(|bits| {
                            bits <= 32
                                && (self.coding == EntropyCoding::Unary
                                    || bits == 32
                                    || self.value < (1 << bits))
                                && self.estimated_microbits
                                    == Some(u32::from(bits) * MICROBITS_PER_BIT)
                        })
                }
            }
    }

    fn valid_cdf(&self) -> bool {
        self.bypass_bits.is_none()
            && self.cdf.as_ref().is_none_or(|cdf| {
                (1..=7).contains(&cdf.cutoffs_q15.len())
                    && cdf.cutoffs_q15.iter().all(|&cutoff| cutoff <= 32768)
                    && cdf.cutoffs_q15.windows(2).all(|pair| pair[0] <= pair[1])
                    && cdf.adaptation_count <= 32
                    && self.value as usize <= cdf.cutoffs_q15.len()
            })
            && match self.estimated_microbits {
                Some(cost) => {
                    cost <= 16 * MICROBITS_PER_BIT
                        && self.interval > 0
                        && self.interval <= self.range
                }
                None => self.interval == 0 || self.interval > self.range,
            }
    }
}

fn valid_identifier(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 48
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Context for a named syntax observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntropyContext {
    /// Selector argument names in schema order.
    pub names: String,
    /// Selector argument values in schema order.
    pub values: [u16; 3],
}

/// Bounded entropy observations from one scope within a frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntropyEvidence {
    /// Entropy schema supplied by the codec.
    pub schema: u16,
    /// AV2 version to which the syntax labels refer.
    pub spec: String,
    /// Exact codec revision from Studio's dependency lock.
    pub codec_revision: String,
    /// Exact scope requested.
    pub scope: EntropyScope,
    /// Exact start ordinal requested.
    pub start: u64,
    /// Exact byte length of the enclosing tile payload.
    pub tile_payload_bytes: u64,
    /// Exclusive ordinal of the observed parse, including units before the start
    /// and past the capture limit.
    pub end_ordinal: u64,
    /// Observed operations in decoder order, from the start ordinal.
    pub symbols: Vec<EntropySymbol>,
}

impl EntropyEvidence {
    /// Checks that the first read at or after the start can begin at `ordinal`.
    /// A scope's first read begins at 0. An earlier read can cover at most 32
    /// units, so it ends fewer than 32 units after a later start.
    fn begins_at_start(&self, ordinal: u64) -> bool {
        ordinal == self.start
            || (self.start > 0 && ordinal > self.start && ordinal - self.start < 32)
    }

    pub(super) fn matches(
        &self,
        scope: EntropyScope,
        start: u64,
        limit: u32,
        complete: bool,
    ) -> bool {
        self.scope == scope
            && self.start == start
            && self.schema > 0
            && valid_label(&self.spec)
            && self.codec_revision.len() == 40
            && self
                .codec_revision
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            && self.tile_payload_bytes > 0
            && self.symbols.len() <= limit as usize
            && self
                .symbols
                .first()
                .is_none_or(|symbol| self.begins_at_start(symbol.ordinal))
            && self.symbols.last().is_none_or(|symbol| {
                symbol
                    .end_ordinal()
                    .is_some_and(|end| end <= self.end_ordinal)
            })
            && self
                .symbols
                .windows(2)
                .all(|pair| pair[0].end_ordinal() == Some(pair[1].ordinal))
            && (!complete
                || match self.symbols.last() {
                    Some(symbol) => symbol.end_ordinal() == Some(self.end_ordinal),
                    None => self.end_ordinal < self.start || self.begins_at_start(self.end_ordinal),
                })
            && self.symbols.iter().all(|symbol| symbol.matches(scope))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::{
        BlockCoverage, BlockEvidence, BlockEvidenceKind, BlockFrame, BlockQuery, BlockRect,
        BlockTree,
    };
    use crate::identity::DecodeLayers;
    use crate::layers::LayerDecodeSelection;
    use crate::preview::SamplePlane;
    use crate::protocol::{Envelope, Event, ProtocolError, read_message, write_message};

    // The longest ordinals that leave room for a full capture before `u64::MAX`.
    const START: u64 = u64::MAX - 2 * MAX_ENTROPY_EVENTS as u64;

    fn largest_case() -> (BlockQuery, EntropyScope, BlockEvidence) {
        let coded = BlockRect {
            x: u32::MAX - 1,
            y: u32::MAX - 1,
            width: 1,
            height: 1,
        };
        let scope = EntropyScope::Transform(BlockTransformId {
            block: BlockId {
                tile: u32::MAX,
                tree: BlockTree::Chroma,
                coded,
            },
            plane: SamplePlane::U,
            coded,
        });
        let query = BlockQuery {
            coded_offset: u64::MAX - 1,
            selection: LayerDecodeSelection::Default,
            kind: BlockEvidenceKind::Entropy {
                scope,
                start: START,
            },
            region: None,
            detail: None,
            max_events: MAX_ENTROPY_EVENTS,
            max_bytes: MAX_ENTROPY_BYTES,
        };
        let evidence = BlockEvidence {
            query,
            frame: Some(BlockFrame {
                coded_offset: query.coded_offset,
                sequence_offset: u64::MAX - 2,
                layers: DecodeLayers {
                    extended: 31,
                    embedded: 31,
                    temporal: 7,
                },
            }),
            coverage: BlockCoverage::Truncated,
            replayed_through: u64::MAX,
            records: Vec::new(),
            coefficients: None,
            stages: Vec::new(),
            entropy: Some(EntropyEvidence {
                schema: 1,
                spec: "1.0.0".to_owned(),
                codec_revision: "f".repeat(40),
                scope,
                start: START,
                tile_payload_bytes: 123,
                end_ordinal: u64::MAX,
                symbols: (0..MAX_ENTROPY_EVENTS)
                    .map(|index| EntropySymbol {
                        ordinal: START + u64::from(index),
                        transform: match scope {
                            EntropyScope::Transform(id) => Some(id),
                            EntropyScope::Block(_) => None,
                        },
                        syntax: Some("s".repeat(48)),
                        context: Some(EntropyContext {
                            names: ["a".repeat(15), "b".repeat(15), "c".repeat(16)].join(","),
                            values: [u16::MAX; 3],
                        }),
                        coding: EntropyCoding::Cdf,
                        value: 7,
                        bypass_bits: None,
                        cdf: Some(EntropyCdf {
                            cutoffs_q15: vec![3333, 6666, 9999, 13333, 16666, 19999, 23333],
                            adaptation_count: 32,
                            rate_index: 7,
                            updates: true,
                        }),
                        estimated_microbits: Some(2 * MICROBITS_PER_BIT),
                        range: 32768,
                        interval: 8192,
                    })
                    .collect(),
            }),
        };
        (query, scope, evidence)
    }

    #[test]
    fn maximum_entropy_reply_fits_and_rejects_wrong_scope_or_order() -> Result<(), ProtocolError> {
        let (query, scope, mut evidence) = largest_case();
        assert!(evidence.matches(query));
        let mut wrong_owner = evidence.clone();
        if let Some(payload) = &mut wrong_owner.entropy {
            payload.symbols[0].transform = None;
        }
        assert!(!wrong_owner.matches(query));
        if let Some(payload) = &mut wrong_owner.entropy {
            payload.symbols[0].syntax = Some("cctx_type".to_owned());
        }
        assert!(wrong_owner.matches(query));
        let mut wire = Vec::new();
        write_message(
            &mut wire,
            &Envelope::new(
                u64::MAX,
                Event::AnalysisBlocksReady {
                    document: u64::MAX,
                    evidence: Box::new(evidence.clone()),
                },
            ),
        )?;
        let reply = read_message::<Event>(&mut wire.as_slice())?;
        assert!(
            matches!(reply, Some(Envelope { body: Event::AnalysisBlocksReady { evidence: actual, .. }, .. }) if *actual == evidence)
        );
        if let Some(kept) = &mut evidence.entropy {
            kept.scope = EntropyScope::Block(scope.block());
        }
        assert!(!evidence.matches(query));
        if let Some(kept) = &mut evidence.entropy {
            kept.scope = scope;
            kept.symbols[1].ordinal = START;
        }
        assert!(!evidence.matches(query));
        Ok(())
    }

    #[test]
    fn entropy_reply_must_echo_the_start_and_begin_at_or_after_it() {
        let (query, scope, evidence) = largest_case();
        let with = |start: u64, first: u64, end: u64, count: u64| {
            let mut reply = evidence.clone();
            reply.query.kind = BlockEvidenceKind::Entropy { scope, start };
            reply.coverage = BlockCoverage::Complete;
            if let Some(kept) = &mut reply.entropy {
                kept.start = start;
                kept.end_ordinal = end;
                kept.symbols
                    .truncate(usize::try_from(count).unwrap_or_default());
                for (offset, symbol) in (0..).zip(&mut kept.symbols) {
                    symbol.ordinal = first + offset;
                }
            }
            reply
        };
        let matches = |reply: &BlockEvidence| reply.matches(reply.query);
        assert!(matches(&with(0, 0, 3, 3)));
        assert!(matches(&with(100, 100, 103, 3)));
        // A bypass read that began before the start can end up to 31 units after it.
        assert!(matches(&with(100, 131, 134, 3)));
        assert!(!matches(&with(100, 132, 135, 3)));
        assert!(!matches(&with(100, 99, 102, 3)), "a read before the start");
        assert!(
            !matches(&with(0, 1, 4, 3)),
            "the first read of a scope is at 0"
        );
        // A complete reply with no reads after the start.
        assert!(matches(&with(100, 0, 100, 0)));
        assert!(matches(&with(100, 0, 90, 0)));
        assert!(matches(&with(100, 0, 131, 0)));
        assert!(!matches(&with(100, 0, 132, 0)));
        assert!(!matches(&with(0, 0, 1, 0)));

        let mut other_start = with(100, 100, 103, 3);
        assert!(!other_start.matches(BlockQuery {
            kind: BlockEvidenceKind::Entropy { scope, start: 0 },
            ..query
        }));
        if let Some(kept) = &mut other_start.entropy {
            kept.start = 0;
        }
        assert!(!matches(&other_start), "the payload must echo the start");
    }
}
