// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The workspaces of the window: which task area the shell shows.
//!
//! This is view state. [`crate::state::AppState`] owns the current
//! workspace; changing it never touches the queue, the global settings or
//! the immutable `JobSpec` a converting item took, and it is not journalled.
//!
//! Only Encode exists. Analyze and Compare are approved directions, not
//! features: they are known here so the shell can refuse them, and the
//! window offers no way to reach them until their flows land.

/// A task area of the window. Jobs is shared by all of them, so it is not
/// one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Workspace {
    /// Convert video to AV2: the queue, settings, progress and preview.
    #[default]
    Encode,
    /// Read-only inspection of AV2 streams. Not built.
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
        matches!(self, Self::Encode)
    }
}
