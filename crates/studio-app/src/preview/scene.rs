// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! What the selected file can show, read from the app state.

use studio_domain::analysis::SourceDigest;
use studio_domain::job::{SourceFingerprint, WirePath};
use studio_domain::layers::LayerDecodeSelection;
use studio_domain::preview::{PictureTarget, SourceRef};

use crate::queue::{Item, ItemId, ItemState};
use crate::state::{AppState, Mode};

/// What the selected file can show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subject {
    /// It is being read; nothing to show yet.
    Loading {
        /// The item.
        item: ItemId,
    },
    /// It cannot be converted, so there is nothing to show; the card says
    /// why.
    Unavailable {
        /// The item.
        item: ItemId,
    },
    /// The source, before converting or after an attempt that published
    /// nothing.
    Source {
        /// The item.
        item: ItemId,
        /// What to read.
        source: SourceRef,
        /// The source's frame count, when known.
        frames: Option<u64>,
        /// The frame size in luma pixels.
        size: (u32, u32),
    },
    /// Converting: live pictures of this job; compare reads the source.
    Live {
        /// The item.
        item: ItemId,
        /// The job's request id; live pictures carry it.
        job: u64,
        /// The source the job reads.
        source: SourceRef,
        /// The source's frame count, when known.
        frames: Option<u64>,
        /// The frame size in luma pixels.
        size: (u32, u32),
    },
    /// Converted: the output file; compare reads the source.
    Result {
        /// The item.
        item: ItemId,
        /// The output file.
        path: WirePath,
        /// The source it was made from.
        source: SourceRef,
        /// The source's frame count, when known.
        frames: Option<u64>,
        /// The frame size in luma pixels.
        size: (u32, u32),
    },
    /// An independently opened AV2 file in Analyze.
    Document {
        /// The current inspection read.
        document: u64,
        /// The source path.
        path: WirePath,
        /// The identity of the file when the inspection engine read it.
        fingerprint: SourceFingerprint,
        /// The digest of the bytes that the inspection engine read.
        digest: Box<SourceDigest>,
        /// The decoder policy requested for this document.
        selection: LayerDecodeSelection,
    },
}

impl Subject {
    /// The item it belongs to.
    #[must_use]
    pub const fn item(&self) -> Option<ItemId> {
        match self {
            Self::Loading { item }
            | Self::Unavailable { item }
            | Self::Source { item, .. }
            | Self::Live { item, .. }
            | Self::Result { item, .. } => Some(*item),
            Self::Document { .. } => None,
        }
    }

    pub(super) const fn source(&self) -> Option<&SourceRef> {
        match self {
            Self::Source { source, .. }
            | Self::Live { source, .. }
            | Self::Result { source, .. } => Some(source),
            Self::Loading { .. } | Self::Unavailable { .. } | Self::Document { .. } => None,
        }
    }

    /// What frame stepping and playback show.
    pub(super) fn target(&self) -> Option<PictureTarget> {
        match self {
            Self::Source { source, .. } => Some(PictureTarget::Source {
                source: source.clone(),
            }),
            Self::Result { path, source, .. } => Some(PictureTarget::Result {
                path: path.clone(),
                source: Some(source.clone()),
            }),
            Self::Document {
                document,
                path,
                fingerprint,
                digest,
                selection,
            } => Some(PictureTarget::Document {
                document: *document,
                path: path.clone(),
                fingerprint: *fingerprint,
                digest: digest.clone(),
                selection: *selection,
            }),
            Self::Loading { .. } | Self::Unavailable { .. } | Self::Live { .. } => None,
        }
    }

    pub(super) const fn frames(&self) -> Option<u64> {
        match self {
            Self::Source { frames, .. }
            | Self::Live { frames, .. }
            | Self::Result { frames, .. } => *frames,
            Self::Loading { .. } | Self::Unavailable { .. } | Self::Document { .. } => None,
        }
    }

    pub(super) const fn frame_size(&self) -> (u32, u32) {
        match self {
            Self::Source { size, .. } | Self::Live { size, .. } | Self::Result { size, .. } => {
                *size
            }
            Self::Loading { .. } | Self::Unavailable { .. } | Self::Document { .. } => (0, 0),
        }
    }
}

/// What the preview needs from the app state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scene {
    /// The selected file, when there is one.
    pub stage: Option<Subject>,
    /// The files whose rows show a thumbnail, in list order.
    pub thumbs: Vec<(ItemId, SourceRef)>,
}

/// The [`Scene`] of `state`: in the picture area the row the user selected,
/// otherwise the file converting, otherwise the file Advanced describes;
/// in the list, a thumbnail for every file that has been read.
#[must_use]
pub fn scene(state: &AppState) -> Scene {
    let thumbs = if state.mode() == Mode::List {
        state
            .items()
            .iter()
            .filter(|i| !matches!(i.state(), ItemState::Probing | ItemState::Unreadable))
            .filter_map(|i| Some((i.id(), source_ref(i)?)))
            .collect()
    } else {
        Vec::new()
    };
    Scene {
        stage: state
            .chosen_index()
            .and_then(|i| state.items().get(i))
            .or_else(|| state.active())
            .or_else(|| state.selected())
            .map(subject),
        thumbs,
    }
}

/// The source as the item's facts describe it.
fn source_ref(item: &Item) -> Option<SourceRef> {
    let facts = item.facts()?;
    Some(SourceRef {
        path: WirePath::from_path(item.source()),
        fingerprint: facts.fingerprint,
        video_stream: facts.video_stream,
    })
}

fn subject(item: &Item) -> Subject {
    let id = item.id();
    let Some(facts) = item.facts() else {
        return match item.state() {
            ItemState::Unreadable => Subject::Unavailable { item: id },
            _ => Subject::Loading { item: id },
        };
    };
    let frames = facts.frame_count;
    let size = (facts.width, facts.height);
    // What the job read, exactly as it was sent.
    let job = item
        .attempt()
        .filter(|a| a.job.purpose == studio_domain::job::JobPurpose::Encode)
        .map(|a| {
            (
                a.request,
                a.job.destination.clone(),
                SourceRef {
                    path: a.job.source.clone(),
                    fingerprint: a.job.fingerprint,
                    video_stream: a.job.video_stream,
                },
            )
        });
    match (item.state(), job) {
        (ItemState::Probing, _) => Subject::Loading { item: id },
        (ItemState::Unreadable, _) => Subject::Unavailable { item: id },
        (ItemState::Running | ItemState::Publishing, Some((job, _, source))) => Subject::Live {
            item: id,
            job,
            source,
            frames,
            size,
        },
        (ItemState::Completed, Some((_, path, source))) => Subject::Result {
            item: id,
            path,
            source,
            frames,
            size,
        },
        _ => source_ref(item).map_or(Subject::Loading { item: id }, |source| Subject::Source {
            item: id,
            source,
            frames,
            size,
        }),
    }
}
