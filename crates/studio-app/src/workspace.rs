// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The workspaces of the window: which task area the shell shows.
//!
//! This is view state. [`crate::state::AppState`] owns the current
//! workspace; changing it never touches the queue, the global settings or
//! the immutable `JobSpec` a converting item took, and it is not journalled.
//!
//! Encode and Analyze exist. Compare is an approved direction, not a
//! feature: it is known here so the shell can refuse it, and the window
//! offers no way to reach it until its flow lands.

/// A task area of the window. Jobs is shared by all of them, so it is not
/// one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Workspace {
    /// Convert video to AV2: the queue, settings, progress and preview.
    #[default]
    Encode,
    /// Read-only inspection of AV2 streams: OBUs, properties and bytes.
    Analyze,
    /// Source against result, or result against result. Not built.
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

    /// The workspace has a working flow in this build and may be shown.
    #[must_use]
    pub const fn available(self) -> bool {
        matches!(self, Self::Encode | Self::Analyze)
    }

    /// The workspaces the switch offers, in its order: the available ones.
    pub const OFFERED: [Self; 2] = [Self::Encode, Self::Analyze];

    /// The workspace at `index` of [`Self::OFFERED`].
    #[must_use]
    pub fn offered(index: i32) -> Option<Self> {
        usize::try_from(index)
            .ok()
            .and_then(|i| Self::OFFERED.get(i).copied())
    }
}
