// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Which files are in the queue: adding, replacing and removing them,
//! reading them (one probe at a time), their video track and destination.

use std::path::{Path, PathBuf};

use super::{AppState, Dialog, Effect};
use crate::queue::{Item, ItemId, ItemState, Transition};
use crate::recovery::Disk;
use crate::source::{ProbeFailure, ProbeRequest, TrackPicker, default_destination, numbered};
use studio_domain::job::SourceFacts;

/// How far numbered alternatives go: `clip (2).ivf` … `clip (999).ivf`.
const MAX_NUMBERED: u32 = 999;

impl AppState {
    /// Adds files to the end of the list. A file already waiting (or about
    /// to) is not added twice.
    pub(super) fn add(&mut self, paths: Vec<PathBuf>, disk: &dyn Disk) {
        for path in paths {
            let duplicate = self.items.iter().any(|i| {
                i.source == path && matches!(i.state, ItemState::Probing | ItemState::Waiting)
            });
            if duplicate {
                continue;
            }
            let id = self.next_id;
            self.next_id += 1;
            let wanted = default_destination(&path, self.draft_settings().base.output);
            let destination = self.unique(wanted, id, Some(disk));
            self.items.push(Item::new(id, path, destination));
        }
    }

    /// Replaces the one file (Change…) unless it is converting.
    pub(super) fn replace(&mut self, paths: Vec<PathBuf>, disk: &dyn Disk) {
        if let [only] = self.items.as_slice()
            && !only.state.is_active()
        {
            self.remove(only.id);
            self.add(paths, disk);
        }
    }

    /// Removes an item unless it is converting.
    pub(super) fn remove(&mut self, id: ItemId) {
        let Some(index) = self.items.iter().position(|i| i.id == id) else {
            return;
        };
        if self.items[index].state.is_active() {
            return;
        }
        self.items.remove(index);
        if self.probing.is_some_and(|(_, item)| item == id) {
            self.probing = None;
        }
        if self.selected == Some(id) {
            self.selected = None;
        }
    }

    /// `wanted`, or the first numbered alternative that no other item uses
    /// and, when `disk` is given, where nothing exists yet. If all are
    /// taken, `wanted` itself (and the engine will refuse it).
    pub(super) fn unique(&self, wanted: PathBuf, id: ItemId, disk: Option<&dyn Disk>) -> PathBuf {
        let free = |path: &Path| {
            !self
                .items
                .iter()
                .any(|i| i.id != id && i.destination.path == path)
                && !disk.is_some_and(|d| d.exists(path))
        };
        if free(&wanted) {
            return wanted;
        }
        (2..=MAX_NUMBERED)
            .map(|n| numbered(&wanted, n))
            .find(|p| free(p))
            .unwrap_or(wanted)
    }

    /// The next probe to send, if none is in flight: files are read one at
    /// a time, in list order.
    pub(super) fn next_probe(&mut self) -> Option<Effect> {
        if let Some((request, id)) = self.probing {
            let current = self
                .item(id)
                .is_some_and(|i| i.state == ItemState::Probing && i.probe_request == Some(request));
            if current {
                return None;
            }
            self.probing = None;
        }
        let index = self
            .items
            .iter()
            .position(|i| i.state == ItemState::Probing && i.probe_request.is_none())?;
        let request = self.take_request();
        let item = &mut self.items[index];
        item.probe_request = Some(request);
        self.probing = Some((request, item.id));
        Some(Effect::Probe {
            item: item.id,
            request: ProbeRequest {
                id: request,
                path: item.source.clone(),
                video_stream: item.stream,
            },
        })
    }

    /// The engine answered probe `id`; a stale answer changes nothing.
    pub(super) fn probe_answered(&mut self, id: u64, result: Result<SourceFacts, ProbeFailure>) {
        if self.probing.is_some_and(|(request, _)| request == id) {
            self.probing = None;
        }
        let Some(item) = self
            .items
            .iter_mut()
            .find(|i| i.state == ItemState::Probing && i.probe_request == Some(id))
        else {
            return;
        };
        item.probe_request = None;
        let transition = match result {
            Ok(facts) => {
                item.facts = Some(facts);
                item.probe_failure = None;
                Transition::Probed
            }
            Err(failure) => {
                // The last facts stay, so another track can still be chosen.
                item.probe_failure = Some(failure);
                Transition::ProbeFailed
            }
        };
        let _ = item.transition(transition);
    }

    /// Reads `id` again for `stream`: a waiting or unreadable file directly,
    /// an ended one as a retry.
    fn reprobe(&mut self, id: ItemId, stream: Option<u32>, disk: &dyn Disk) {
        let Some(item) = self.item_mut(id) else {
            return;
        };
        if item.state.can_retry() {
            self.retry(id, stream, true, disk);
        } else if item.transition(Transition::Reprobe).is_ok() {
            item.stream = stream;
            item.probe_request = None;
        }
    }

    /// Converts an ended file again: it is read first, and a destination
    /// that now exists (or another file's) is replaced by a numbered one
    /// rather than failing. `by_user` lets a running run pick it up again.
    pub(super) fn retry(
        &mut self,
        id: ItemId,
        stream: Option<u32>,
        by_user: bool,
        disk: &dyn Disk,
    ) -> bool {
        let Some(item) = self.item_mut(id) else {
            return false;
        };
        let replay = item
            .attempt
            .as_ref()
            .filter(|_| by_user)
            .filter(|attempt| stream.is_none_or(|stream| stream == attempt.job.video_stream))
            .map(|attempt| crate::queue::Submission {
                job: attempt.job.clone(),
                policy: attempt.policy.clone(),
            });
        let stream = stream
            .or(item.stream)
            .or_else(|| item.facts.as_ref().map(|f| f.video_stream));
        if item.transition(Transition::Retry).is_err() {
            return false;
        }
        item.stream = stream;
        item.probe_request = None;
        item.probe_failure = None;
        let wanted = item.destination.path.clone();
        let path = self.unique(wanted, id, Some(disk));
        if let Some(item) = self.item_mut(id) {
            item.destination.path = path;
            item.destination.exists = false;
            item.submitted = replay.map(|mut submission| {
                submission.job.destination =
                    studio_domain::job::WirePath::from_path(&item.destination.path);
                submission
            });
        }
        if by_user && let Some(run) = &mut self.run {
            run.attempted.retain(|a| *a != id);
        }
        true
    }

    /// Re-reads the selected file for the chosen track, unless it is the
    /// current one.
    pub(super) fn select_track(&mut self, index: i32, disk: &dyn Disk) {
        let Some(item) = self.selected().filter(|i| Self::editable(i)) else {
            return;
        };
        let tracks = item.facts.as_ref().map_or(&[][..], |f| &f.video_tracks);
        let Some(picker) = TrackPicker::of(tracks, item.video_stream()) else {
            return;
        };
        let current = item.video_stream();
        let id = item.id;
        let stream = usize::try_from(index)
            .ok()
            .and_then(|i| picker.tracks.get(i))
            .map(|t| t.stream)
            .filter(|&s| Some(s) != current);
        if let Some(stream) = stream {
            self.reprobe(id, Some(stream), disk);
        }
    }

    /// The Save dialog for the selected file's destination.
    pub(super) fn save_dialog(&mut self) -> Option<Effect> {
        let item = self.selected().filter(|i| Self::editable(i))?;
        let id = item.id;
        let path = &item.destination.path;
        let effect = Effect::ShowSaveDialog {
            directory: path.parent().map(Path::to_path_buf),
            file_name: path
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
            output: self.draft_settings().base.output,
        };
        self.dialog = Some(Dialog::Save(id));
        Some(effect)
    }

    /// The user chose `path` for `id`. It never collides with another
    /// file's destination; an existing file there is reported, not replaced.
    pub(super) fn choose_destination(&mut self, id: ItemId, path: PathBuf) {
        if !self.item(id).is_some_and(Self::editable) {
            return;
        }
        let path = self.unique(path, id, None);
        if let Some(item) = self.item_mut(id) {
            item.destination.path = path;
            item.destination.chosen = true;
            item.destination.exists = false;
        }
    }

    /// Default destinations keep following the output kind's extension.
    pub(super) fn follow_output_kind(&mut self, disk: &dyn Disk) {
        let ids: Vec<ItemId> = self
            .items
            .iter()
            .filter(|i| Self::editable(i) && !i.destination.chosen)
            .map(|i| i.id)
            .collect();
        for id in ids {
            let Some(item) = self.item(id) else {
                continue;
            };
            let wanted = default_destination(&item.source, self.draft_settings().base.output);
            let path = self.unique(wanted, id, Some(disk));
            if let Some(item) = self.item_mut(id) {
                item.destination.path = path;
                item.destination.exists = false;
            }
        }
    }
}
