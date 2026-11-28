// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The workspaces of the window: which task area the shell shows.
//!
//! This is view state. [`crate::state::AppState`] owns the current
//! workspace; changing it never touches the queue, the global settings or
//! the immutable `JobSpec` a converting item took, and it is not journalled.
//!
//! Encode converts, Analyze inspects one stream, and Compare pairs a
//! source with its result, or two results.

/// A task area of the window. Jobs is shared by all of them, so it is not
/// one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Workspace {
    /// Convert video to AV2: the queue, settings, progress and preview.
    #[default]
    Encode,
    /// Read-only inspection of AV2 streams: OBUs, properties and bytes.
    Analyze,
    /// A pair of inputs: the source of an encode against its result, or one
    /// result against another, with their compatibility and alignment.
    Compare,
}

impl Workspace {
    /// The name in the toolbar.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Encode => "Encode",
            Self::Analyze => "Analyze",
            Self::Compare => "Compare",
        }
    }

    /// The workspaces the switch offers, in its order.
    pub const OFFERED: [Self; 3] = [Self::Encode, Self::Analyze, Self::Compare];

    /// The workspace at `index` of [`Self::OFFERED`].
    #[must_use]
    pub fn offered(index: i32) -> Option<Self> {
        usize::try_from(index)
            .ok()
            .and_then(|i| Self::OFFERED.get(i).copied())
    }
}
