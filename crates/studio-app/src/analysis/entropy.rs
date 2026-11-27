// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Explicit and bounded entropy replay for the selected decoder region.

use studio_domain::analysis::AnalysisFailure;
use studio_domain::blocks::{
    BlockCoverage, BlockEvidence, BlockEvidenceKind, BlockFrame, BlockQuery, EntropyCoding,
    EntropyScope, EntropySymbol, MAX_ENTROPY_BYTES, MAX_ENTROPY_EVENTS,
};
use studio_domain::layers::LayerDecodeSelection;
use studio_domain::protocol::{Command, Event};

use super::{Analysis, Asked, Effect, block_overlay::Key, fail_or_notice};

pub(crate) const TAB: i32 = 7;
pub(crate) const PAGE_ROWS: usize = 5;

pub(crate) const fn coding_name(kind: EntropyCoding) -> &'static str {
    match kind {
        EntropyCoding::Cdf => "CDF-coded",
        EntropyCoding::Bypass => "Bypass",
        EntropyCoding::Unary => "Unary",
    }
}

/// Operation text of one symbol. The table shows it and the search matches it.
pub(crate) fn operation(symbol: &EntropySymbol) -> &str {
    symbol
        .syntax
        .as_deref()
        .unwrap_or_else(|| coding_name(symbol.coding))
}

/// An action in the entropy inspector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntropyInput {
    /// Capture the selected block or transform from its first symbol.
    Read,
    /// Capture the shown scope from the end of the shown symbols.
    ReadNext,
    /// Capture the selected transform's parent block.
    ReadBlock,
    /// Read the exact transform that owns the selected symbol.
    ReadTransform,
    /// Stop the active replay and retain any partial results.
    Stop,
    /// Search the captured symbols.
    Search(String),
    /// Filter by coding operation.
    Kind(i32),
    /// Select a visible symbol.
    Pick(i32),
    /// Move relative to the selected symbol.
    Move(i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Selection {
    pub(crate) document: u64,
    pub(crate) frame: BlockFrame,
    policy: LayerDecodeSelection,
    pub(crate) scope: EntropyScope,
}

#[derive(Debug, Default)]
pub(crate) struct Entropy {
    selection: Option<Selection>,
    pending: Option<u64>,
    pub(crate) stopping: bool,
    pub(crate) evidence: Option<Box<BlockEvidence>>,
    pub(crate) scope: Option<EntropyScope>,
    pub(crate) notice: Option<String>,
    pub(crate) search: String,
    pub(crate) kind: i32,
    pub(crate) selected: usize,
}

impl Entropy {
    pub(crate) const fn busy(&self) -> bool {
        self.pending.is_some()
    }

    pub(crate) fn filtered(&self) -> Vec<&EntropySymbol> {
        let Some(evidence) = self
            .evidence
            .as_deref()
            .and_then(|evidence| evidence.entropy.as_ref())
        else {
            return Vec::new();
        };
        let query = self.search.trim().to_ascii_lowercase();
        evidence
            .symbols
            .iter()
            .filter(|symbol| {
                let kind = match symbol.coding {
                    EntropyCoding::Cdf => 1,
                    EntropyCoding::Bypass => 2,
                    EntropyCoding::Unary => 3,
                };
                (self.kind == 0 || kind == self.kind)
                    && (query.is_empty()
                        || operation(symbol).to_ascii_lowercase().contains(&query)
                        || symbol.ordinal.to_string().contains(&query)
                        || symbol.value.to_string().contains(&query))
            })
            .collect()
    }

    /// Returns the start of the part after the shown symbols, if the scope continues.
    pub(crate) fn next_start(&self) -> Option<u64> {
        let report = self.evidence.as_deref()?;
        let entropy = report.entropy.as_ref()?;
        let next = entropy
            .symbols
            .last()
            .map_or(Some(entropy.start), EntropySymbol::end_ordinal)?;
        (report.coverage != BlockCoverage::Complete && next < entropy.end_ordinal).then_some(next)
    }

    pub(crate) const fn first(&self) -> usize {
        self.selected / PAGE_ROWS * PAGE_ROWS
    }

    pub(super) fn fail(&mut self, request: u64, message: String) {
        if self.pending == Some(request) {
            self.pending = None;
            self.stopping = false;
            self.notice = Some(message);
        }
    }
}

impl Analysis {
    pub(crate) fn entropy_selection(&self) -> Option<Selection> {
        let doc = self.shown()?;
        if doc.evidence.tab != TAB || doc.layers.shown || doc.selection_pending() {
            return None;
        }
        if self.overlay.picture?.document != doc.id() {
            return None;
        }
        let selected = self.overlay.selected?;
        self.overlay.selected_record()?;
        let scope = match selected.key {
            Key::Coding(block) => EntropyScope::Block(block),
            Key::Transform(transform) => EntropyScope::Transform(transform),
            Key::Partition(_) => return None,
        };
        Some(Selection {
            document: doc.id(),
            frame: selected.frame,
            policy: selected.policy,
            scope,
        })
    }

    pub(crate) fn entropy_for(&self, selection: Selection) -> Option<&Entropy> {
        (self.entropy.selection == Some(selection)).then_some(&self.entropy)
    }

    pub(crate) fn sync_entropy(&mut self, out: &mut Vec<Effect>) {
        if self.entropy.selection == self.entropy_selection() {
            return;
        }
        self.stop_entropy(out);
        if let Some(request) = self.entropy.pending {
            self.pending.remove(&request);
        }
        self.entropy = Entropy::default();
    }

    fn stop_entropy(&mut self, out: &mut Vec<Effect>) {
        if let (Some(selection), Some(blocks)) = (self.entropy.selection, self.entropy.pending)
            && !self.entropy.stopping
        {
            self.entropy.stopping = true;
            self.send(selection.document, Asked::Cancel, out, |document| {
                Command::AnalysisCancelBlocks { document, blocks }
            });
        }
    }

    pub(super) fn entropy_input(&mut self, input: EntropyInput, out: &mut Vec<Effect>) {
        self.sync_entropy(out);
        let Some(selection) = self.entropy_selection() else {
            return;
        };
        let (scope, start) = match input {
            EntropyInput::Read => (selection.scope, 0),
            EntropyInput::ReadNext => {
                let (Some(scope), Some(start)) = (self.entropy.scope, self.entropy.next_start())
                else {
                    return;
                };
                (scope, start)
            }
            EntropyInput::ReadBlock => (EntropyScope::Block(selection.scope.block()), 0),
            EntropyInput::ReadTransform => {
                let Some(id) = self
                    .entropy
                    .filtered()
                    .get(self.entropy.selected)
                    .and_then(|row| row.transform)
                else {
                    return;
                };
                (EntropyScope::Transform(id), 0)
            }

            EntropyInput::Stop => return self.stop_entropy(out),
            EntropyInput::Search(mut query) => {
                if query.len() > 96 {
                    query.truncate(query.floor_char_boundary(96));
                }
                self.entropy.search = query;
                self.entropy.selected = 0;
                return;
            }
            EntropyInput::Kind(kind) => {
                self.entropy.kind = kind.clamp(0, 3);
                self.entropy.selected = 0;
                return;
            }
            EntropyInput::Pick(index) => {
                if let Ok(index) = usize::try_from(index)
                    && index < self.entropy.filtered().len()
                {
                    self.entropy.selected = index;
                }
                return;
            }
            EntropyInput::Move(distance) => {
                self.entropy.selected = self
                    .entropy
                    .selected
                    .saturating_add_signed(distance as isize)
                    .min(self.entropy.filtered().len().saturating_sub(1));
                return;
            }
        };
        if self.entropy.busy() {
            return;
        }
        let query = BlockQuery {
            coded_offset: selection.frame.coded_offset,
            selection: selection.policy,
            kind: BlockEvidenceKind::Entropy { scope, start },
            region: None,
            detail: None,
            max_events: MAX_ENTROPY_EVENTS,
            max_bytes: MAX_ENTROPY_BYTES,
        };
        let request = self.send(selection.document, Asked::Entropy(query), out, |document| {
            Command::AnalysisBlocks { document, query }
        });
        self.entropy = Entropy {
            selection: Some(selection),
            pending: Some(request),
            scope: Some(scope),
            ..Entropy::default()
        };
    }

    pub(super) fn entropy_event(
        &mut self,
        at: usize,
        request: u64,
        query: BlockQuery,
        event: Event,
        out: &mut Vec<Effect>,
    ) {
        match event {
            Event::AnalysisBlocksReady { evidence, .. } => {
                self.accept_entropy(request, query, evidence);
            }
            Event::AnalysisFailed { code, message, .. } => {
                if matches!(
                    code,
                    AnalysisFailure::SourceChanged | AnalysisFailure::UnknownDocument
                ) {
                    fail_or_notice(&mut self.documents[at], Some(code), &message);
                }
                self.fail_entropy(request, Some(code), &message);
            }
            Event::Error { message, .. } => self.fail_entropy(request, None, &message),
            _ => self.broken(at, "an unexpected entropy capture reply", out),
        }
    }

    fn accept_entropy(&mut self, request: u64, query: BlockQuery, evidence: Box<BlockEvidence>) {
        if self.entropy.pending != Some(request)
            || self.entropy.selection != self.entropy_selection()
        {
            return;
        }
        if evidence.matches(query)
            && evidence.frame.is_none_or(|frame| {
                self.entropy
                    .selection
                    .is_some_and(|selection| frame == selection.frame)
            })
        {
            self.entropy.pending = None;
            self.entropy.stopping = false;
            self.entropy.evidence = Some(evidence);
        } else {
            self.entropy.fail(
                request,
                "The engine returned entropy evidence for a different scope.".to_owned(),
            );
        }
    }

    fn fail_entropy(&mut self, request: u64, code: Option<AnalysisFailure>, message: &str) {
        let message = if code == Some(AnalysisFailure::Cancelled) {
            "Entropy capture stopped.".to_owned()
        } else {
            format!("Studio could not read the entropy symbols. {message}")
        };
        self.entropy.fail(request, message);
    }
}
