// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! What the engine said about one source file: the probe request and its
//! answer, the video track choice and the default destination beside it.

use std::path::{Path, PathBuf};

use studio_domain::job::{Failure, FailureCode, SourceFacts, VideoTrack};
use studio_domain::settings::OutputKind;

/// A probe to run with a new worker; any older probe is obsolete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeRequest {
    /// Echoed in the answer.
    pub id: u64,
    /// The source.
    pub path: PathBuf,
    /// The video stream to describe; `None` lets the engine pick.
    pub video_stream: Option<u32>,
}

/// Why a probe produced no facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeFailure {
    /// The engine's reason; `None` when the engine could not be asked or
    /// broke off.
    pub code: Option<FailureCode>,
    /// The explanation, safe to show.
    pub message: String,
}

impl ProbeFailure {
    /// A failure without an engine reason, such as a worker that did not start.
    #[must_use]
    pub fn other(message: impl Into<String>) -> Self {
        Self {
            code: None,
            message: message.into(),
        }
    }
}

impl From<Failure> for ProbeFailure {
    fn from(failure: Failure) -> Self {
        Self {
            code: Some(failure.code),
            message: failure.message,
        }
    }
}

/// The video tracks a user may choose: every track that is not an attached
/// picture.
#[must_use]
pub fn selectable_tracks(tracks: &[VideoTrack]) -> Vec<&VideoTrack> {
    tracks.iter().filter(|t| !t.attached_picture).collect()
}

/// The attached pictures in `facts` other than the selected stream; they are
/// part of [`studio_domain::job::Omissions::other_video`].
#[must_use]
pub fn attached_pictures(facts: &SourceFacts) -> u32 {
    let count = facts
        .video_tracks
        .iter()
        .filter(|t| t.attached_picture && t.stream != facts.video_stream)
        .count();
    u32::try_from(count)
        .unwrap_or(u32::MAX)
        .min(facts.omissions.other_video)
}

/// The Video track choice, shown only when a file has more than one track a
/// user may choose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackPicker {
    /// The choosable tracks, in stream order.
    pub tracks: Vec<VideoTrack>,
    /// The index of the selected or requested track in `tracks`.
    pub selected: Option<usize>,
    /// Attached pictures, which are never encoded.
    pub attached: Vec<VideoTrack>,
}

impl TrackPicker {
    /// The picker for `tracks` with `stream` selected, or `None` when there
    /// is no choice to make.
    #[must_use]
    pub fn of(tracks: &[VideoTrack], stream: Option<u32>) -> Option<Self> {
        let choosable: Vec<VideoTrack> = selectable_tracks(tracks).into_iter().cloned().collect();
        if choosable.len() < 2 {
            return None;
        }
        Some(Self {
            selected: choosable.iter().position(|t| Some(t.stream) == stream),
            attached: tracks
                .iter()
                .filter(|t| t.attached_picture)
                .cloned()
                .collect(),
            tracks: choosable,
        })
    }
}

/// `<source stem>.<ext>` next to the source, never the source itself.
#[must_use]
pub fn default_destination(source: &Path, output: OutputKind) -> PathBuf {
    let path = source.with_extension(output.extension());
    if path != source {
        return path;
    }
    // The name stays exact where it is not Unicode; only the suffix is added.
    let mut name = source.file_stem().map_or_default(ToOwned::to_owned);
    name.push(format!("-av2.{}", output.extension()));
    source.with_file_name(name)
}

/// The `n`th alternative to `path`: `clip (2).ivf`, `clip (3).ivf`, …
/// The name stays exact where it is not Unicode; only the suffix is added.
#[must_use]
pub fn numbered(path: &Path, n: u32) -> PathBuf {
    let mut name = path.file_stem().map_or_default(ToOwned::to_owned);
    name.push(format!(" ({n})"));
    if let Some(extension) = path.extension() {
        name.push(".");
        name.push(extension);
    }
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::abs;

    #[test]
    fn the_default_destination_is_never_the_source() {
        assert_eq!(
            default_destination(&abs("/m/clip.ivf"), OutputKind::Ivf),
            abs("/m/clip-av2.ivf")
        );
        assert_eq!(
            default_destination(&abs("/m/clip"), OutputKind::Av2AnnexB),
            abs("/m/clip.obu")
        );
    }

    #[test]
    fn numbered_names_keep_the_folder_and_extension() {
        assert_eq!(
            numbered(&abs("/m/clip one.ivf"), 2),
            abs("/m/clip one (2).ivf")
        );
        assert_eq!(numbered(&abs("/m/clip"), 3), abs("/m/clip (3)"));
    }

    // Unix only: these are raw Unix bytes; Windows has its own test below.
    #[cfg(unix)]
    #[test]
    fn numbered_names_keep_non_unicode_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let raw = std::ffi::OsStr::from_bytes(b"/m/clip-\xff.ivf");
        let got = numbered(Path::new(raw), 2);
        assert_eq!(got.as_os_str().as_bytes(), b"/m/clip-\xff (2).ivf");
    }

    #[cfg(windows)]
    #[test]
    fn numbered_names_keep_unpaired_surrogates() {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let mut wide: Vec<u16> = "C:\\m\\clip-".encode_utf16().collect();
        wide.push(0xD800);
        wide.extend(".ivf".encode_utf16());
        let raw = std::ffi::OsString::from_wide(&wide);
        let got = numbered(Path::new(&raw), 2);
        let mut expected: Vec<u16> = "C:\\m\\clip-".encode_utf16().collect();
        expected.push(0xD800);
        expected.extend(" (2).ivf".encode_utf16());
        assert_eq!(got.as_os_str().encode_wide().collect::<Vec<_>>(), expected);
    }
}
