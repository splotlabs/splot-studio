// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Scoped layer declarations and native membership and metadata evidence.

use serde::{Deserialize, Serialize};

/// Scope of an AV2 OBU. Global scope has no extended-layer number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LayerScope {
    /// Global AV2 scope.
    Global,
    /// One extended layer.
    Extended {
        /// Extended-layer ID.
        id: u8,
    },
}

/// Decoder operating-point request. A view filter is independent of this setting.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LayerDecodeSelection {
    /// Use the ordinary decoder policy.
    #[default]
    Default,
    /// Request a specific declared operating point.
    OperatingPoint {
        /// Global or local scope of the OPS declaration.
        scope: LayerScope,
        /// Operating-point set ID.
        set: u8,
        /// Index within the set.
        point: u8,
    },
}

/// Status of actual playback for the current selection generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayerDecodeStatus {
    /// The decoder has not yet returned output for this selection.
    Pending,
    /// The decoder returned a picture for this selection.
    Active,
    /// The decoder or preview path rejected this selection.
    Rejected(String),
}

/// Position and coverage of one layer inspection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerEvidence {
    /// Selected OBU element.
    pub element: u64,
    /// Source file offset of the selected OBU header.
    pub offset: u64,
    /// Native temporal-unit index, when the selected unit could be completed.
    pub temporal_unit: Option<u64>,
    /// Requested decoder operating point.
    pub selection: LayerDecodeSelection,
    /// Why the native operating-point filter could not resolve the request.
    pub membership_reason: Option<String>,
    /// Whether the CI observer reached the end of the selected temporal unit.
    pub metadata_complete: bool,
    /// Whether the declaration catalog omitted entries at its retention limit.
    pub catalog_limited: bool,
}

/// One evidence row with a file coordinate for syntax navigation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerRow {
    /// Declaring or affected scope.
    pub scope: LayerScope,
    /// OBU header byte offset, when there is an in-stream origin.
    pub origin: Option<u64>,
    /// Type and value of evidence.
    pub fact: LayerFact,
}

/// A profile, tier and level declaration from AV2 syntax.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileTierLevel {
    /// Referenced extended layer.
    pub xlayer: u8,
    /// Declared profile code.
    pub profile: u8,
    /// Declared level index.
    pub level: u8,
    /// Declared high-tier flag.
    pub high_tier: bool,
    /// Raw reserved bits, when present.
    pub reserved: Option<u8>,
}

/// One declared embedded or extended layer dependency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerDependency {
    /// Layer named by the declaration.
    pub xlayer: u8,
    /// Embedded layer for an embedded declaration.
    pub mlayer: Option<u8>,
    /// Raw declared dependency map. Bit meanings follow the originating syntax.
    pub mask: u32,
}

/// Declared source of embedded and temporal layer selection for one extended layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingPointLayers {
    /// Referenced extended-layer ID.
    pub xlayer: u8,
    /// Source of the embedded and temporal layer maps.
    pub source: OperatingPointLayerSource,
}

/// Raw operating-point layer syntax from AV2 § 5.11.5.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OperatingPointLayerSource {
    /// No embedded or temporal layer map was signaled.
    Absent,
    /// The point declares layer maps directly.
    Explicit {
        /// Embedded-layer map.
        embedded_mask: u8,
        /// Embedded layer IDs and their temporal-layer maps.
        temporal_masks: Vec<(u8, u8)>,
    },
    /// The declaration references another operating point.
    Inherited {
        /// Referenced set ID.
        set: u8,
        /// Referenced point index.
        point: u8,
    },
    /// The parser recognized a source with no Studio representation.
    Unknown,
}

/// Scope and lifetime declarations for one metadata unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataDeclaration {
    /// Raw metadata type code, including reserved values.
    pub type_id: u32,
    /// Whether this unit applies as a suffix.
    pub suffix: bool,
    /// The unit's cancellation flag.
    pub cancel: bool,
    /// Raw layer scope code.
    pub layer_idc: Option<u8>,
    /// Raw persistence code.
    pub persistence_idc: Option<u8>,
    /// Raw extended-layer map, when present.
    pub xlayer_map: Option<u32>,
    /// Raw embedded-layer maps, in declared order.
    pub embedded_maps: Vec<u8>,
    /// Raw reserved bits, when present.
    pub reserved: Option<u8>,
}

/// One raw OBU declaration, observed layer, or effective metadata state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LayerFact {
    /// OBU headers observed through the selected byte.
    Presence {
        /// Embedded layer.
        embedded: u8,
        /// Temporal layer.
        temporal: u8,
        /// Number of OBU headers observed for this triple.
        count: u64,
        /// Most recent observed OBU header byte offset.
        last: u64,
        /// Native decoder OPS membership. Global headers have no membership.
        member: Option<bool>,
    },
    /// An AV2 § 5.10 operating-point declaration or a set reset.
    OperatingPoint {
        /// Operating-point set ID.
        set: u8,
        /// Point index, when declared.
        point: Option<u8>,
        /// Whether the declaration resets its set namespace.
        reset: bool,
        /// Signaled extended-layer map.
        xlayer_map: Option<u32>,
        /// Declared profile, tier and level entries.
        profiles: Vec<ProfileTierLevel>,
        /// Declared layer rules for this point.
        layers: Vec<OperatingPointLayers>,
        /// Raw global embedded-layer information mode.
        layer_info_mode: Option<u8>,
        /// Set priority code, when present.
        priority: Option<u8>,
        /// Point intent code, or set intent code when only the set declares one.
        intent: Option<u8>,
        /// Raw reserved bits, when present.
        reserved: Option<u8>,
    },
    /// An AV2 § 5.8 layer configuration record declaration.
    Configuration {
        /// Global configuration record ID.
        global_id: u8,
        /// Local configuration record ID.
        local_id: Option<u8>,
        /// Signaled extended-layer map.
        xlayer_map: Option<u32>,
        /// Declared profile, tier and level entries.
        profiles: Vec<ProfileTierLevel>,
        /// Declared dependency maps.
        dependencies: Vec<LayerDependency>,
        /// Whether the native parser detected nonzero reserved bits.
        reserved_nonzero: bool,
    },
    /// An AV2 § 5.6 multistream decoder operation declaration.
    Composition {
        /// Declared count of independent streams.
        streams: u32,
        /// Profile code for the multistream operation.
        profile: u8,
        /// Level index for the multistream operation.
        level: u8,
        /// High-tier flag for the multistream operation.
        high_tier: bool,
        /// Declared substream profile, tier and level entries.
        substreams: Vec<ProfileTierLevel>,
    },
    /// A sequence header declaration.
    SequenceHeader {
        /// Sequence-header ID.
        id: u8,
        /// Declared maximum coded picture width.
        max_width: u32,
        /// Declared maximum coded picture height.
        max_height: u32,
        /// Declared profile code.
        profile: u8,
        /// Declared level index.
        level: u8,
        /// Declared high-tier flag.
        high_tier: bool,
    },
    /// Content interpretation state at the completed temporal unit (AV2 § 6.14).
    ContentInterpretation {
        /// Affected embedded layer.
        embedded: u8,
        /// Native effective state.
        value: ContentState,
    },
    /// An AV2 § 5.17 metadata OBU declaration.
    Metadata {
        /// Native OBU type name.
        name: String,
        /// Parsed scope and lifetime, if available. This does not report an effective value.
        declaration: Option<MetadataDeclaration>,
    },
}

/// Content interpretation state has an explicit lifetime and source chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContentState {
    /// No effective state is available for this scope.
    Unavailable,
    /// Native random-access processing established unspecified values.
    Unspecified {
        /// Header offset of the frame that initialized this state.
        reset: u64,
        /// Layers from the nearest inheritance source to the original source.
        inherited_from: Vec<u8>,
    },
    /// Native random-access processing established a signaled value.
    Signaled {
        /// Header offset of the frame that initialized this state.
        reset: u64,
        /// Header offset of the activated sequence header, when in-band.
        sequence_header: Option<u64>,
        /// Layers from the nearest inheritance source to the original source.
        inherited_from: Vec<u8>,
        /// Signaled scan type code.
        scan_type: u8,
        /// Signaled color description code.
        color_description: Option<u32>,
        /// Effective color primaries, transfer and matrix codes when available.
        color_codes: Option<[u8; 3]>,
        /// Signaled full-range flag.
        full_range: Option<bool>,
        /// Signaled chroma sample positions.
        chroma_position: Option<[u32; 2]>,
        /// Derived sample aspect ratio when specified.
        sample_aspect_ratio: Option<[u32; 2]>,
        /// Timing units per tick and ticks per second when present.
        timing: Option<[u32; 2]>,
        /// Raw reserved bits.
        reserved: u8,
    },
}
