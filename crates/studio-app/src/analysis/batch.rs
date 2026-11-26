// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Attaches a batch report to the matching Analyze document.

use super::{Analysis, Asked, Command, Effect, Origin, Phase, same_file};
use crate::batch::Report;
use studio_domain::analysis::DocumentSummary;

impl Analysis {
    pub(crate) fn open_batch_result(&mut self, report: Report) -> Vec<Effect> {
        self.dirty = true;
        self.notice = None;
        let mut effects = Vec::new();
        let existing = self
            .documents
            .iter()
            .position(|doc| doc.path == report.path);
        let at = if let Some(at) = existing {
            self.shown = at;
            let doc = &self.documents[at];
            if doc.evidence.running.is_some() {
                self.documents[at].notice =
                    Some("Wait for the validation in Analyze to finish.".into());
                return effects;
            }
            // Recheck the current bytes. A saved digest cannot detect an
            // in-place rewrite that preserves both metadata fields.
            if !matches!(doc.phase, Phase::Opening | Phase::Cancelling) {
                let document = doc.id;
                if doc.in_engine() {
                    if doc
                        .summary()
                        .is_some_and(|summary| matches_source(summary, &report))
                    {
                        self.restore_from_live(at);
                    }
                    self.pending.retain(|_, (owner, _)| *owner != document);
                    self.send(document, Asked::Close, &mut effects, |document| {
                        Command::AnalysisClose { document }
                    });
                }
                self.documents[at].restore = self.documents[at]
                    .restore
                    .filter(|view| same_file(view.fingerprint, report.fingerprint));
                self.read(at, &mut effects);
            }
            at
        } else {
            let before = self.documents.len();
            self.open(report.path.clone(), Origin::File, &mut effects);
            if self.documents.len() == before {
                return effects;
            }
            self.shown
        };

        self.documents[at].pending_batch = Some(Box::new(report));
        effects
    }

    pub(super) fn attach_batch(&mut self, at: usize, report: Report) {
        let doc = &mut self.documents[at];
        if !doc
            .summary()
            .is_some_and(|summary| matches_source(summary, &report))
        {
            doc.notice = Some("Studio could not match the opened bytes to the validated source. Export the retained report from Jobs.".into());
            return;
        }
        if doc.evidence.running.is_some() {
            doc.notice = Some("Wait for the validation in Analyze to finish.".into());
            return;
        }
        doc.notice = None;
        doc.evidence.tab = 2;
        self.next_request += 1;
        doc.evidence.validation = Some((self.next_request, report.summary));
        doc.evidence.findings = report.findings;
    }
}

fn matches_source(summary: &DocumentSummary, report: &Report) -> bool {
    same_file(summary.fingerprint, report.fingerprint)
        && summary.digest.as_deref() == Some(&report.digest)
}
