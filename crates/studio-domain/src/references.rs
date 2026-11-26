// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Reference state and direct frame-level candidate dependencies.

use serde::{Deserialize, Serialize};

use crate::identity::DecodeLayers;
use crate::layers::LayerDecodeSelection;

/// One reference request in the current analysis selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceQuery {
    /// File offset of the first coded OBU.
    pub coded_offset: u64,
    /// Requested decoder operating point.
    pub selection: LayerDecodeSelection,
    /// The selection generation in the app.
    pub generation: u64,
}

/// Identity and luma dimensions of an admitted coded frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceFrame {
    /// File offset of the first coded OBU.
    pub coded_offset: u64,
    /// File offset of the active sequence header.
    pub sequence_offset: u64,
    /// Layer identity of the coded frame.
    pub layers: DecodeLayers,
    /// Luma width used by the reference buffer.
    pub width: u32,
    /// Luma height used by the reference buffer.
    pub height: u32,
}

/// A reference buffer slot at a frame boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReferenceSlotState {
    /// This buffer has no slot at this index.
    Absent,
    /// The decoder has never stored a picture in the slot.
    Empty,
    /// The slot has a clear validity flag.
    Invalid,
    /// The decoder cannot identify a retained picture for the slot.
    Unavailable,
    /// The slot holds the identified coded frame.
    Retained {
        /// Coded identity and geometry of the retained frame.
        frame: ReferenceFrame,
        /// The stored long-term ID, when available.
        long_term_id: Option<u32>,
        /// A layer switch restricts the slot's order hint.
        restricted_order_hint: bool,
    },
}

/// The input and output states of a slot, including unchanged slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceSlot {
    /// Slot index within the extended layer.
    pub slot: u8,
    /// State used as input to the selected frame.
    pub before: ReferenceSlotState,
    /// State after reference refresh.
    pub after: ReferenceSlotState,
}

/// How the selected coded frame obtains the slot index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceOrigin {
    /// The frame directly signals the reference index.
    Signaled,
    /// The decoder derives the reference index.
    Derived,
    /// The frame signals its bridge source slot.
    Bridge,
}

/// One input candidate in native reference-list order.
/// The list contains frame-level candidates, some of which block prediction may omit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceDependency {
    /// Index in the frame's reference list, when the frame has one.
    pub index: Option<u8>,
    /// Slot index in the input snapshot.
    pub slot: u8,
    /// How the decoder obtains the slot index.
    pub origin: ReferenceOrigin,
    /// The decoder uses a non-unit geometry scale for this slot.
    pub scaled: bool,
}

/// One bounded page of native reference state for a selected coded frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceEvidence {
    /// The selection and generation that produced this result.
    pub query: ReferenceQuery,
    /// Identity of the selected coded frame.
    pub frame: ReferenceFrame,
    /// The native OBU type of the selected frame.
    pub frame_obu_type: u8,
    /// Slots refreshed by the selected frame.
    pub refresh_mask: u32,
    /// The selected frame starts a new coded video sequence.
    pub resets_sequence: bool,
    /// Slots permitted by open-loop key state, when that state imposes a limit.
    pub allowed_input_slots: Option<u32>,
    /// Long-term IDs requested by the frame header.
    pub required_long_term_ids: Vec<u32>,
    /// All buffer slots in index order.
    pub slots: Vec<ReferenceSlot>,
    /// Direct frame-level reference candidates in input-list order.
    pub dependencies: Vec<ReferenceDependency>,
}
