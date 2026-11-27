// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The opt-in block query for the current coded frame.

use studio_domain::analysis::AnalysisFailure;
use studio_domain::blocks::{
    BlockEvidence, BlockEvidenceKind, BlockQuery, MAX_BLOCK_BYTES, MAX_BLOCK_RECORDS,
};
use studio_domain::protocol::Command;

use super::{Analysis, Asked, Document, Effect};

pub(crate) const TAB: i32 = 5;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Blocks {
    pending: Option<u64>,
    stopping: bool,
    evidence: Option<Box<BlockEvidence>>,
    notice: Option<String>,
}

impl Blocks {
    pub(crate) const fn busy(&self) -> bool {
        self.pending.is_some()
    }

    pub(crate) const fn stopping(&self) -> bool {
        self.stopping
    }

    pub(crate) fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    pub(super) fn accept(&mut self, request: u64, evidence: Box<BlockEvidence>) {
        if self.pending == Some(request) {
            self.pending = None;
            self.stopping = false;
            self.evidence = Some(evidence);
        }
    }

    pub(super) fn fail(&mut self, request: u64, code: Option<AnalysisFailure>, message: &str) {
        if self.pending == Some(request) {
            self.pending = None;
            self.stopping = false;
            self.notice = Some(match code {
                Some(AnalysisFailure::Cancelled) => "Block inspection stopped.".to_owned(),
                Some(AnalysisFailure::Unsupported) => {
                    format!("Block evidence is unavailable. {message}")
                }
                _ => format!("Studio could not inspect the blocks. {message}"),
            });
        }
    }
}

impl Document {
    pub(super) fn accept_block_query(
        &mut self,
        id: u64,
        query: BlockQuery,
        evidence: Box<BlockEvidence>,
    ) {
        if !evidence.matches(query) {
            self.fail_block_query(id, query, None, "The engine sent invalid decoder evidence.");
        } else if matches!(query.kind, BlockEvidenceKind::Stages(_)) {
            self.stages.accept(id, evidence);
        } else {
            self.blocks.accept(id, evidence);
        }
    }

    pub(super) fn fail_block_query(
        &mut self,
        id: u64,
        query: BlockQuery,
        code: Option<AnalysisFailure>,
        message: &str,
    ) {
        if matches!(query.kind, BlockEvidenceKind::Stages(_)) {
            self.stages.fail(id, code, message);
        } else {
            self.blocks.fail(id, code, message);
        }
    }

    pub(crate) fn block_evidence(&self) -> Option<&BlockEvidence> {
        let offset = self.coded_offset()?;
        self.blocks.evidence.as_deref().filter(|evidence| {
            evidence.query.coded_offset == offset
                && evidence.query.selection == self.layer_decode_selection()
        })
    }

    pub(crate) fn block_query_ready(&self) -> bool {
        !self.selection_pending()
            && self.coded_offset().is_some_and(|offset| {
                self.summary()
                    .and_then(|summary| summary.digest.as_deref())
                    .is_some_and(|digest| offset < digest.bytes)
            })
    }
}

impl Analysis {
    pub(super) fn request_blocks(&mut self, at: usize, out: &mut Vec<Effect>) {
        let doc = &self.documents[at];
        if doc.evidence.tab != TAB
            || doc.layers.shown
            || !doc.block_query_ready()
            || doc.blocks.busy()
        {
            return;
        }
        let Some(coded_offset) = doc.coded_offset() else {
            return;
        };
        let query = BlockQuery {
            coded_offset,
            selection: doc.layer_decode_selection(),
            kind: BlockEvidenceKind::Structure,
            region: None,
            detail: None,
            max_events: MAX_BLOCK_RECORDS,
            max_bytes: MAX_BLOCK_BYTES,
        };
        let request = self.send(doc.id, Asked::Blocks(query), out, |document| {
            Command::AnalysisBlocks { document, query }
        });
        self.documents[at].blocks = Blocks {
            pending: Some(request),
            ..Blocks::default()
        };
    }

    pub(super) fn stop_blocks(&mut self, at: usize, out: &mut Vec<Effect>) {
        let doc = &mut self.documents[at];
        if let Some(blocks) = doc.blocks.pending
            && !doc.blocks.stopping
        {
            doc.blocks.stopping = true;
            let id = doc.id;
            self.send(id, Asked::Cancel, out, |document| {
                Command::AnalysisCancelBlocks { document, blocks }
            });
        }
    }

    pub(super) fn cancel_blocks(&mut self, at: usize, out: &mut Vec<Effect>) {
        self.stop_blocks(at, out);
        if let Some(request) = self.documents[at].blocks.pending {
            self.pending.remove(&request);
        }
        self.documents[at].blocks = Blocks::default();
    }

    pub(super) fn clear_frame_evidence(&mut self, at: usize, out: &mut Vec<Effect>) {
        self.cancel_stages(at, out);
        self.cancel_references(at, out);
        self.cancel_blocks(at, out);
    }
}
