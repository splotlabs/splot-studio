// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use studio_domain::job::{JobPurpose, JobSpec, SourceFacts, WirePath};
use studio_domain::options::{EffectiveOption, FieldError, OptionRequest, OptionValue};

use super::{AppState, Effect, Intent};
use crate::queue::{Ending, Item, ItemId, ItemState, Submission, Transition};
use crate::recovery::Disk;
use crate::source::default_destination;
use crate::workspace::Workspace;

/// An operation based on an existing source or job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelatedAction {
    /// Copy a job's requested options into the editable draft.
    CloneSettings,
    /// Encode with the captured requested settings in a new job.
    Reencode,
    /// Generate the pass artifact for the selected VBR settings.
    FirstPass,
    /// Encode with the artifact a completed first-pass job published.
    FinalPass,
}

impl RelatedAction {
    const fn title(self) -> &'static str {
        match self {
            Self::CloneSettings => "Clone settings",
            Self::Reencode => "Re-encode",
            Self::FirstPass => "Generate first pass",
            Self::FinalPass => "Encode with first pass",
        }
    }
}

/// The fresh source facts and effective policy returned by the engine.
pub type RelatedResolution = Result<(Box<SourceFacts>, Vec<EffectiveOption>), FieldError>;

/// The view of a related job before it enters the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelatedPreview {
    /// The engine checks the source and requested options.
    Checking,
    /// The source and options are valid. The user can approve the proposal.
    Ready {
        /// The proposed action.
        title: String,
        /// Source, codec, and policy information for this proposal.
        detail: String,
    },
    /// The engine could not prepare this proposal.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Related {
    request: u64,
    action: RelatedAction,
    job: JobSpec,
    previous: Option<Submission>,
    prepared: Option<(Box<SourceFacts>, Vec<EffectiveOption>)>,
    pub(super) preview: RelatedPreview,
}

impl AppState {
    /// The proposal the window currently shows.
    #[must_use]
    pub fn related(&self) -> Option<&RelatedPreview> {
        self.related.as_ref().map(|related| &related.preview)
    }

    /// Whether the selected source can begin first-pass preparation.
    #[must_use]
    pub fn can_generate_pass(&self) -> bool {
        self.related_idle()
            && self
                .selected()
                .is_some_and(|item| item.state == ItemState::Waiting && item.facts.is_some())
            && vbr(self.option_request())
    }

    fn related_idle(&self) -> bool {
        self.run.is_none()
            && self.active().is_none()
            && self.dialog.is_none()
            && self.confirmation.is_none()
            && self.related.is_none()
            && self
                .capabilities()
                .is_some_and(|capabilities| capabilities.options.is_some())
    }

    pub(super) fn prepare_related(
        &mut self,
        item: ItemId,
        action: RelatedAction,
    ) -> Option<Effect> {
        if !self.related_idle() {
            return None;
        }
        let original = self.item(item)?;
        let current = self.capabilities()?.options.clone()?;
        let previous = original
            .attempt()
            .filter(|attempt| attempt.ending.is_some())
            .map(|attempt| Submission {
                job: attempt.job.clone(),
                policy: attempt.policy.clone(),
            });
        let mut job = match action {
            RelatedAction::CloneSettings | RelatedAction::Reencode | RelatedAction::FinalPass => {
                let old = previous.as_ref()?;
                match action {
                    RelatedAction::Reencode if old.job.purpose != JobPurpose::Encode => {
                        return None;
                    }
                    RelatedAction::FinalPass if old.job.purpose != JobPurpose::FirstPass => {
                        return None;
                    }
                    _ => {}
                }
                old.job.clone()
            }
            RelatedAction::FirstPass => {
                let facts = original.facts()?;
                if original.state != ItemState::Waiting || !vbr(self.option_request()) {
                    return None;
                }
                let destination = original.destination.path.with_extension("splotpass");
                JobSpec {
                    source: WirePath::from_path(&original.source),
                    fingerprint: facts.fingerprint,
                    video_stream: facts.video_stream,
                    omissions_acknowledged: false,
                    destination: WirePath::from_path(&destination),
                    settings: self.option_request().clone(),
                    identity: None,
                    purpose: JobPurpose::FirstPass,
                    pass_artifact: None,
                    pass_digest: None,
                }
            }
        };
        if action == RelatedAction::FinalPass {
            let Some(Ending::Completed(report)) = original.ending() else {
                return None;
            };
            job.pass_digest.clone_from(&report.pass_digest);
            job.pass_digest.as_ref()?;
            job.pass_artifact = Some(job.destination);
            job.purpose = JobPurpose::Encode;
            let source = job.source.to_path_buf().ok()?;
            job.destination =
                WirePath::from_path(&default_destination(&source, job.settings.base.output));
        }
        let artifact_uses_old_encoder = job.pass_artifact.is_some()
            && job.identity.as_ref() != Some(&current)
            && action != RelatedAction::CloneSettings;
        job.identity = Some(current);
        let request = self.take_request();
        self.related = Some(Related {
            request,
            action,
            job: job.clone(),
            previous,
            prepared: None,
            preview: if artifact_uses_old_encoder {
                RelatedPreview::Failed(
                    "The first pass uses a different encoder. Generate a new first pass before encoding.".into(),
                )
            } else {
                RelatedPreview::Checking
            },
        });
        self.workspace = Workspace::Encode;
        (!artifact_uses_old_encoder).then_some(Effect::PrepareRelated {
            request,
            job: Box::new(job),
        })
    }

    pub(super) fn related_prepared(&mut self, request: u64, result: RelatedResolution) {
        let Some(related) = self.related.as_mut().filter(|r| r.request == request) else {
            return;
        };
        match result {
            Ok((facts, policy)) => {
                if facts.fingerprint != related.job.fingerprint
                    || facts.video_stream != related.job.video_stream
                {
                    related.preview = RelatedPreview::Failed(
                        "The source or its selected track changed. Add the source again to use it."
                            .into(),
                    );
                    return;
                }
                let detail = related.description(&facts, &policy);
                related.preview = RelatedPreview::Ready {
                    title: related.action.title().into(),
                    detail,
                };
                related.prepared = Some((facts, policy));
            }
            Err(error) => {
                related.preview =
                    RelatedPreview::Failed(format!("{}. {}", error.field, error.message));
            }
        }
    }

    pub(super) fn approve_related(&mut self, disk: &dyn Disk) {
        let Some(mut related) = self.related.take() else {
            return;
        };
        let Some((facts, policy)) = related.prepared.take() else {
            self.related = Some(related);
            return;
        };
        if self.run.is_some()
            || self
                .capabilities()
                .and_then(|capabilities| capabilities.options.as_ref())
                != related.job.identity.as_ref()
        {
            self.related = Some(Related {
                prepared: None,
                preview: RelatedPreview::Failed(
                    "The queue or encoder changed during preparation. Prepare the action again."
                        .into(),
                ),
                ..related
            });
            return;
        }
        if related.action == RelatedAction::CloneSettings {
            self.selected = None;
            self.edit_options(Intent::ReplaceOptions(related.job.settings), disk);
            return;
        }
        let mut job = related.job;
        let (Ok(source), Ok(wanted)) = (job.source.to_path_buf(), job.destination.to_path_buf())
        else {
            return;
        };
        let id = self.next_id;
        self.next_id += 1;
        let destination = self.unique(wanted, id, Some(disk));
        job.destination = WirePath::from_path(&destination);
        job.omissions_acknowledged |= facts.omissions.any();
        let mut item = Item::new(id, source, destination);
        item.stream = Some(job.video_stream);
        item.size = Some(facts.fingerprint.len);
        item.facts = Some(*facts);
        item.submitted = Some(Submission { job, policy });
        let _ = item.transition(Transition::Probed);
        self.run_related(item);
    }

    fn run_related(&mut self, item: Item) {
        self.begin_run();
        if let Some(run) = &mut self.run {
            run.attempted.extend(self.items.iter().map(|item| item.id));
        }
        self.selected = Some(item.id);
        self.items.push(item);
    }
}

impl Related {
    fn description(&self, facts: &SourceFacts, policy: &[EffectiveOption]) -> String {
        let mut parts = Vec::new();
        if let Some(previous) = &self.previous {
            if previous.job.identity != self.job.identity {
                parts.push("The encoder revision or option schema changed. This action uses the current encoder.".into());
            }
            if previous.policy.is_empty() {
                parts.push("The original job has no captured policy to compare.".into());
            } else {
                parts.push(crate::advanced::differences(&previous.policy, policy));
                for row in &previous.policy {
                    if !policy.iter().any(|new| new.key == row.key) {
                        parts.push(format!("The current encoder omits {}.", row.key));
                    }
                }
                for row in policy {
                    if !previous.policy.iter().any(|old| old.key == row.key) {
                        parts.push(format!("The current encoder adds {}.", row.key));
                    }
                }
            }
        }
        if self.job.pass_artifact.is_some() {
            parts.push(
                "The engine checks the first-pass artifact before it opens the output.".into(),
            );
        }
        if facts.omissions.any()
            && !self.job.omissions_acknowledged
            && self.action != RelatedAction::CloneSettings
        {
            parts.push("This job uses only the selected video track. Approve to leave the other tracks and chapters out.".into());
        }
        if parts.is_empty() {
            parts.push("The current encoder checked the source and requested settings.".into());
        }
        parts.join("\n")
    }
}

fn vbr(request: &OptionRequest) -> bool {
    matches!(request.values.get("rate_control"),
        Some(OptionValue::Record(rate))
            if matches!(rate.get("mode"), Some(OptionValue::Choice(mode)) if mode == "vbr"))
}
