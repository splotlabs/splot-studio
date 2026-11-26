// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

use studio_domain::analysis::{Element, LocateTarget, Row};
use studio_domain::identity::IdentityCompletion;
use studio_domain::preview::GrainPolicy;
use studio_domain::protocol::{Command, Event};
use studio_domain::timeline::{TimelinePage, TimelineSelection, TimelineTarget, TimelineView};

use super::{
    Analysis, Asked, Effect, Input, Pick, Position, Purpose, Selected, extent, in_window, line_of,
};

/// The window the user wants in coded or output order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimelineRange {
    /// Coded or output order.
    pub view: TimelineView,
    /// The first ordinal.
    pub first: u64,
    /// The number of rows to include.
    pub limit: u64,
    /// The requested number of aggregation buckets.
    pub buckets: u32,
}

impl Default for TimelineRange {
    fn default() -> Self {
        Self {
            view: TimelineView::Coded,
            first: 0,
            limit: u64::MAX,
            buckets: 64,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Request {
    Build,
    Cancel,
    Range { scan: u64, range: TimelineRange },
    Locate { scan: u64, target: TimelineTarget },
}

/// The timeline state of one open source generation.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Timeline {
    scan: Option<u64>,
    collecting: bool,
    cancelling: bool,
    completion: Option<IdentityCompletion>,
    range: TimelineRange,
    reading: Option<u64>,
    refresh_after_finish: bool,
    locating: Option<u64>,
    queued_target: Option<TimelineTarget>,
    next_selection: Option<(TimelineSelection, u64)>,
    page: Option<Box<TimelinePage>>,
    selection: Option<TimelineSelection>,
    notice: Option<String>,
}

impl Timeline {
    /// Whether the timeline has been requested.
    #[must_use]
    pub const fn shown(&self) -> bool {
        self.scan.is_some() || self.notice.is_some()
    }

    /// The retained identity generation.
    #[must_use]
    pub const fn scan(&self) -> Option<u64> {
        self.scan
    }

    /// Whether an identity scan or timeline request is active.
    #[must_use]
    pub const fn busy(&self) -> bool {
        self.collecting
            || self.reading.is_some()
            || self.locating.is_some()
            || self.next_selection.is_some()
    }

    /// Whether the identity scan still accepts cancellation.
    #[must_use]
    pub const fn can_cancel(&self) -> bool {
        self.collecting && !self.cancelling
    }

    /// The final scan outcome, when the engine has returned it.
    #[must_use]
    pub const fn completion(&self) -> Option<&IdentityCompletion> {
        self.completion.as_ref()
    }

    /// Whether a visible timeline can refresh its partial rows.
    #[must_use]
    pub const fn can_refresh(&self) -> bool {
        self.can_cancel() && self.reading.is_none()
    }

    /// The range requested by the user.
    #[must_use]
    pub const fn range(&self) -> TimelineRange {
        self.range
    }

    /// The data for the current range, when available.
    #[must_use]
    pub fn page(&self) -> Option<&TimelinePage> {
        self.page.as_deref()
    }

    /// The selection in the active navigator and byte view.
    #[must_use]
    pub const fn selection(&self) -> Option<&TimelineSelection> {
        self.selection.as_ref()
    }

    /// The latest timeline error, when applicable.
    #[must_use]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    fn has_indexed_target(&self, target: TimelineTarget) -> bool {
        self.page().is_some_and(|page| match (page.view, target) {
            (TimelineView::Coded, TimelineTarget::Coded { index })
            | (TimelineView::Output, TimelineTarget::Output { index }) => index < page.total,
            _ => false,
        })
    }

    pub(super) fn history(&self) -> Option<(u64, TimelineSelection)> {
        self.scan.zip(self.selection)
    }

    pub(super) fn restore_selection(&mut self, selection: Option<(u64, TimelineSelection)>) {
        self.selection = selection.map(|(_, selection)| selection);
    }

    pub(super) fn fail_link(&mut self, message: String) {
        self.next_selection = None;
        self.notice = Some(message);
    }

    pub(super) fn clear_selection(&mut self) {
        self.selection = None;
    }

    pub(super) fn fail(&mut self, id: u64, request: Request, message: String) {
        match request {
            Request::Build if self.scan == Some(id) => {
                self.collecting = false;
                self.scan = None;
            }
            Request::Range { range, .. } if self.reading == Some(id) => {
                self.reading = None;
                if self.range != range {
                    return;
                }
            }
            Request::Locate { target, .. } if self.locating == Some(id) => {
                self.locating = None;
                if self.queued_target != Some(target) {
                    return;
                }
                self.queued_target = None;
            }
            _ => return,
        }
        self.notice = Some(message);
    }
}

impl Analysis {
    /// Take a picture navigation request for the currently shown document.
    pub(crate) fn take_timeline_picture(&mut self) -> Option<(u64, Option<u64>)> {
        self.timeline_picture
            .take()
            .filter(|(document, _)| self.shown().is_some_and(|doc| doc.id() == *document))
    }

    pub(super) fn cancel_timeline_location(&mut self, at: usize) {
        if let Some(doc) = self.documents.get_mut(at) {
            doc.timeline.queued_target = None;
            if doc.timeline.next_selection.take().is_some() {
                doc.locating = None;
            }
        }
    }

    pub(super) fn cancel_timeline_if_selection_changes(&mut self, at: usize, input: &Input) {
        if matches!(
            input,
            Input::RowClicked(_)
                | Input::Move(_)
                | Input::ByteClicked(_)
                | Input::Cursor(_)
                | Input::SelectAtCursor
                | Input::Back
                | Input::Forward
                | Input::GoTo(_)
                | Input::EvidencePick(_)
                | Input::EvidenceMove(_)
                | Input::FindingStep(_)
        ) {
            self.cancel_timeline_location(at);
        }
    }

    pub(super) fn timeline_input(&mut self, at: usize, input: &Input, out: &mut Vec<Effect>) {
        let doc = &mut self.documents[at];
        match *input {
            Input::TimelineBuild if !doc.timeline.collecting => {
                let document = doc.id;
                doc.timeline = Timeline::default();
                let id = self.send(document, Asked::Timeline(Request::Build), out, |document| {
                    Command::AnalysisIdentityScan {
                        document,
                        grain: GrainPolicy::Skip,
                    }
                });
                let timeline = &mut self.documents[at].timeline;
                timeline.scan = Some(id);
                timeline.collecting = true;
            }
            Input::TimelineCancel if doc.timeline.can_cancel() => {
                if let Some(scan) = doc.timeline.scan {
                    let document = doc.id;
                    doc.timeline.cancelling = true;
                    self.send(
                        document,
                        Asked::Timeline(Request::Cancel),
                        out,
                        |document| Command::AnalysisCancelIdentity { document, scan },
                    );
                }
            }
            Input::TimelineRefresh if doc.timeline.can_refresh() => {
                self.request_timeline_range(at, out);
            }
            Input::TimelineRange {
                view,
                first,
                limit,
                buckets,
            } => {
                let range = TimelineRange {
                    view,
                    first,
                    limit,
                    buckets,
                };
                doc.timeline.range = range;
                doc.timeline.page = None;
                self.request_timeline_range(at, out);
            }
            Input::TimelineSelect(target) => self.request_timeline_target(at, target, out),
            Input::TimelineStep(delta) => {
                let t = &doc.timeline;
                let index = match t.range.view {
                    TimelineView::Coded => t.selection.map(|selection| selection.coded_ordinal),
                    TimelineView::Output => {
                        t.selection.and_then(|selection| selection.output_ordinal)
                    }
                };
                let index = index.map_or(t.range.first, |index| {
                    index.saturating_add_signed(i64::from(delta))
                });
                let index = t
                    .page()
                    .map_or(index, |page| index.min(page.total.saturating_sub(1)));
                let target = match t.range.view {
                    TimelineView::Coded => TimelineTarget::Coded { index },
                    TimelineView::Output => TimelineTarget::Output { index },
                };
                self.request_timeline_target(at, target, out);
            }
            Input::TimelineLinkFindings => {
                doc.evidence.tab = 2;
                doc.evidence.first = 0;
                self.link_timeline_finding(at);
            }
            _ => {}
        }
    }

    fn request_timeline_range(&mut self, at: usize, out: &mut Vec<Effect>) {
        let doc = &self.documents[at];
        let timeline = &doc.timeline;
        let Some(scan) = timeline.scan else { return };
        if timeline.reading.is_some() || timeline.cancelling {
            return;
        }
        let range = timeline.range;
        let id = self.send(
            doc.id,
            Asked::Timeline(Request::Range { scan, range }),
            out,
            |document| Command::AnalysisTimeline {
                document,
                scan,
                view: range.view,
                first: range.first,
                limit: range.limit,
                buckets: range.buckets,
            },
        );
        self.documents[at].timeline.reading = Some(id);
    }

    fn request_timeline_target(
        &mut self,
        at: usize,
        target: TimelineTarget,
        out: &mut Vec<Effect>,
    ) {
        let doc = &mut self.documents[at];
        let timeline = &mut doc.timeline;
        let Some(scan) = timeline.scan else { return };
        if timeline.collecting && !timeline.has_indexed_target(target) {
            timeline.notice = Some("Choose a frame from the indexed range.".into());
            return;
        }
        timeline.notice = None;
        timeline.queued_target = Some(target);
        timeline.next_selection = None;
        doc.locating = None;
        doc.wanted = None;
        if timeline.locating.is_none() {
            self.send_timeline_target(at, scan, out);
        }
    }

    fn send_timeline_target(&mut self, at: usize, scan: u64, out: &mut Vec<Effect>) {
        let doc = &self.documents[at];
        let Some(target) = doc.timeline.queued_target else {
            return;
        };
        let id = self.send(
            doc.id,
            Asked::Timeline(Request::Locate { scan, target }),
            out,
            |document| Command::AnalysisTimelineLocate {
                document,
                scan,
                target,
            },
        );
        self.documents[at].timeline.locating = Some(id);
    }

    pub(super) fn timeline_event(
        &mut self,
        at: usize,
        id: u64,
        request: Request,
        event: Event,
        out: &mut Vec<Effect>,
    ) {
        let timeline = &mut self.documents[at].timeline;
        match (request, event) {
            (Request::Build, Event::AnalysisIdentityScanned { summary, .. })
                if timeline.scan == Some(id) =>
            {
                timeline.collecting = false;
                timeline.cancelling = false;
                timeline.completion = Some(summary.completion);
                timeline.refresh_after_finish = timeline.reading.is_some();
                self.request_timeline_range(at, out);
            }
            (
                Request::Range { scan, range },
                Event::AnalysisTimeline {
                    scan: answered,
                    page,
                    ..
                },
            ) if scan == answered
                && timeline.scan == Some(scan)
                && timeline.reading == Some(id) =>
            {
                timeline.reading = None;
                let refresh = std::mem::take(&mut timeline.refresh_after_finish);
                if timeline.range != range || refresh {
                    self.request_timeline_range(at, out);
                } else if page.view == range.view && page.first == range.first {
                    timeline.page = Some(page);
                    timeline.notice = None;
                } else {
                    timeline.notice =
                        Some("The engine returned a different timeline range.".into());
                }
            }
            (
                Request::Locate { scan, target },
                Event::AnalysisTimelineLocated {
                    scan: answered,
                    selection,
                    ..
                },
            ) if scan == answered
                && timeline.scan == Some(scan)
                && timeline.locating == Some(id) =>
            {
                timeline.locating = None;
                if timeline.queued_target != Some(target) {
                    self.send_timeline_target(at, scan, out);
                    return;
                }
                timeline.queued_target = None;
                if !Self::timeline_target_matches(target, &selection) {
                    timeline.notice =
                        Some("The engine returned a different timeline selection.".into());
                    return;
                }
                let offset = match target {
                    TimelineTarget::Coded { .. } => selection.coded_offset,
                    _ => selection.showing_offset.unwrap_or(selection.coded_offset),
                };
                timeline.next_selection = Some((*selection, offset));
                self.locate(at, LocateTarget::Offset { offset }, Purpose::Timeline, out);
            }
            (Request::Cancel, Event::CancelRequested) => {}
            _ => {
                timeline.fail(
                    id,
                    request,
                    "The engine returned an unexpected timeline reply.".into(),
                );
                self.retry_timeline_after_failure(at, Asked::Timeline(request), out);
            }
        }
    }

    fn timeline_target_matches(target: TimelineTarget, selection: &TimelineSelection) -> bool {
        match target {
            TimelineTarget::Coded { index } => selection.coded_ordinal == index,
            TimelineTarget::Output { index } => selection.output_ordinal == Some(index),
            TimelineTarget::Time { .. } => {
                selection.time.is_some() && selection.output_ordinal.is_some()
            }
        }
    }

    pub(super) fn retry_timeline_after_failure(
        &mut self,
        at: usize,
        request: Asked,
        out: &mut Vec<Effect>,
    ) {
        let timeline = &mut self.documents[at].timeline;
        match request {
            Asked::Timeline(Request::Range { scan, range }) if timeline.scan == Some(scan) => {
                let refresh = std::mem::take(&mut timeline.refresh_after_finish);
                if timeline.range != range || refresh {
                    self.request_timeline_range(at, out);
                }
            }
            Asked::Timeline(Request::Locate { scan, .. })
                if timeline.scan == Some(scan) && timeline.locating.is_none() =>
            {
                self.send_timeline_target(at, scan, out);
            }
            _ => {}
        }
    }

    pub(super) fn timeline_located(
        &mut self,
        at: usize,
        request: u64,
        row: Row,
        position: Position,
        out: &mut Vec<Effect>,
    ) {
        let doc = &mut self.documents[at];
        if doc.locating != Some(request) {
            return;
        }
        doc.locating = None;
        let Some((selection, offset)) = doc.timeline.next_selection.take() else {
            return;
        };
        let Element::Obu { obu } = &row.element else {
            doc.timeline.notice = Some("The selected frame has no indexed OBU.".into());
            return;
        };
        if obu.obu.offset != offset {
            doc.timeline.notice = Some("The linked OBU offset differs from the index.".into());
            return;
        }
        let changed = doc.timeline.selection != Some(selection)
            || doc.bytes.cursor != offset
            || doc
                .selected
                .as_ref()
                .is_none_or(|selected| selected.row.index != row.index);
        doc.timeline.selection = Some(selection);
        doc.restore = None;
        doc.wanted = None;
        doc.bytes.shown = true;
        doc.bytes.cursor = offset;
        if !in_window(doc.bytes.offset, offset) {
            doc.bytes.offset = line_of(offset);
            doc.bytes.data = None;
        }
        doc.selected = Some(Selected { row, position });
        doc.evidence.selected = None;
        self.timeline_picture = Some((doc.id, selection.output_ordinal));
        self.link_timeline_finding(at);
        if changed {
            self.documents[at].remember_selection();
        }
        if let Position::Row(position) = position {
            self.reveal(at, position);
        } else if position == Position::Unknown {
            self.position_selection(at, out);
        }
        self.fill(at, out);
        self.fetch_bytes(at, out);
        self.ensure_syntax(at, out);
    }

    fn link_timeline_finding(&mut self, at: usize) {
        let doc = &mut self.documents[at];
        if doc.evidence.tab != 2 || doc.timeline.selection().is_none() {
            return;
        }
        let Some((report, _)) = doc.evidence.validation.as_ref() else {
            return;
        };
        let Some((start, end)) = doc
            .selected
            .as_ref()
            .map(|selection| extent(&selection.row.element))
        else {
            return;
        };
        let pick = doc
            .evidence
            .findings
            .iter()
            .position(|finding| {
                finding
                    .file_offset
                    .is_some_and(|offset| (start..end).contains(&offset))
            })
            .map(|index| Pick::Finding {
                report: *report,
                index,
            });
        doc.evidence.selected = pick;
        doc.evidence.reveal_selection();
    }
}
