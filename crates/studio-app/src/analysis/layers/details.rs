// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Display native layer facts without deriving codec semantics in Studio.

use std::fmt::Write as _;

use studio_domain::layers::{
    ContentState, LayerDecodeSelection, LayerFact, LayerRow, LayerScope, MetadataDeclaration,
    OperatingPointLayerSource, OperatingPointLayers, ProfileTierLevel,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Detail {
    pub label: String,
    pub value: String,
    pub context: String,
    pub origin: Option<u64>,
    pub request: Option<LayerDecodeSelection>,
}

#[expect(
    clippy::too_many_lines,
    reason = "Each wire fact has a distinct set of display fields."
)]
pub(super) fn detail_rows(row: LayerRow) -> Vec<Detail> {
    let scope = scope_name(row.scope);
    let mut details = Vec::new();
    match row.fact {
        LayerFact::Presence {
            embedded,
            temporal,
            count,
            last,
            member,
        } => {
            let label = match row.scope {
                LayerScope::Global => scope.clone(),
                LayerScope::Extended { .. } => {
                    format!("{scope}, embedded {embedded}, temporal {temporal}")
                }
            };
            details.push(detail(
                label,
                format!("Observed OBUs {count} · Latest header byte {last}"),
                match (row.scope, member) {
                    (LayerScope::Global, _) => "Global scope has no layer membership.",
                    (_, Some(true)) => "The requested operating point includes this layer.",
                    (_, Some(false)) => "The requested operating point excludes this layer.",
                    (_, None) => "Operating-point membership is unknown.",
                },
                row.origin,
            ));
        }
        LayerFact::OperatingPoint {
            set,
            point,
            reset,
            xlayer_map,
            profiles,
            layers,
            layer_info_mode,
            priority,
            intent,
            reserved,
        } => {
            let mut details_row = detail(
                point.map_or_else(
                    || format!("Operating-point set {set}"),
                    |point| format!("Operating-point set {set}, point {point}"),
                ),
                with_codes(
                    if reset {
                        "Set reset signaled"
                    } else {
                        "Declared operating point"
                    },
                    &[
                        optional_hex("Extended-layer map", xlayer_map),
                        optional("Embedded-layer information mode", layer_info_mode),
                        optional("Priority code", priority),
                        optional("Intent code", intent),
                        optional_hex("Reserved bits", reserved.map(u32::from)),
                    ],
                ),
                &scope,
                row.origin,
            );
            details_row.request = point.map(|point| LayerDecodeSelection::OperatingPoint {
                scope: row.scope,
                set,
                point,
            });
            details.push(details_row);
            details.extend(layers.iter().map(|layer| point_layers(layer, &scope, row.origin)));

            details.extend(
                profiles
                    .iter()
                    .map(|profile| profile_row(profile, &scope, row.origin)),
            );
        }
        LayerFact::Configuration {
            global_id,
            local_id,
            xlayer_map,
            profiles,
            dependencies,
            reserved_nonzero,
        } => {
            details.push(detail(
                "Layer configuration record",
                with_codes(
                    "Declared configuration",
                    &[
                        Some(format!("Global ID {global_id}")),
                        optional("Local ID", local_id),
                        optional_hex("Extended-layer map", xlayer_map),
                        reserved_nonzero.then(|| "Nonzero reserved bits".into()),
                    ],
                ),
                &scope,
                row.origin,
            ));
            details.extend(
                profiles
                    .iter()
                    .map(|profile| profile_row(profile, &scope, row.origin)),
            );
            details.extend(dependencies.into_iter().map(|entry| {
                let layer = entry.mlayer.map_or_else(
                    || format!("Extended layer {}", entry.xlayer),
                    |embedded| format!("Extended layer {}, embedded {embedded}", entry.xlayer),
                );
                detail(
                    "Declared dependency map",
                    format!("{layer} · {:#010x}", entry.mask),
                    &scope,
                    row.origin,
                )
            }));
        }
        LayerFact::Composition {
            streams,
            profile,
            level,
            high_tier,
            substreams,
        } => {
            details.push(detail(
                "Multistream decoder operation",
                format!("Declared streams {streams} · Profile code {profile} · {} tier · Level index {level}", tier_name(high_tier)),
                &scope,
                row.origin,
            ));
            details.extend(
                substreams
                    .iter()
                    .map(|profile| profile_row(profile, &scope, row.origin)),
            );
        }
        LayerFact::SequenceHeader { id, max_width, max_height, profile, level, high_tier } => details.push(detail(
            format!("Sequence header {id}"),
            format!("Declared maximum coded size {max_width} × {max_height} · Profile code {profile} · {} tier · Level index {level}", tier_name(high_tier)),
            &scope,
            row.origin,
        )),
        LayerFact::ContentInterpretation { embedded, value } => {
            let scope = format!("{scope}, embedded {embedded}");
            content(&mut details, &scope, row.origin, value);
        }
        LayerFact::Metadata { name, declaration } => {
            details.push(detail("Observed metadata OBU", name, &scope, row.origin));
            if let Some(declaration) = declaration.as_ref() {
                details.push(metadata_declaration(declaration, &scope, row.origin));
            }
        }
    }
    details
}

fn point_layers(layer: &OperatingPointLayers, scope: &str, origin: Option<u64>) -> Detail {
    let label = format!(
        "Declared operating-point layer map · Extended layer {}",
        layer.xlayer
    );
    let value = match &layer.source {
        OperatingPointLayerSource::Absent => "Embedded and temporal maps are not signaled".into(),
        OperatingPointLayerSource::Unknown => "Layer map source unavailable".into(),
        OperatingPointLayerSource::Inherited { set, point } => {
            format!("References set {set}, point {point}")
        }
        OperatingPointLayerSource::Explicit {
            embedded_mask,
            temporal_masks,
        } => {
            let mut value = format!("Embedded-layer map {embedded_mask:#x}");
            for (embedded, temporal_mask) in temporal_masks {
                let _ = write!(
                    value,
                    " · Embedded {embedded} temporal map {temporal_mask:#x}"
                );
            }
            value
        }
    };
    detail(label, value, scope, origin)
}

fn metadata_declaration(
    metadata: &MetadataDeclaration,
    scope: &str,
    origin: Option<u64>,
) -> Detail {
    let embedded_maps = (!metadata.embedded_maps.is_empty()).then(|| {
        let maps = metadata
            .embedded_maps
            .iter()
            .map(|mask| format!("{mask:#x}"))
            .collect::<Vec<_>>();
        format!("Embedded-layer maps {}", maps.join(", "))
    });
    detail(
        "Declared metadata scope and lifetime",
        with_codes(
            &format!(
                "Type code {} · {}",
                metadata.type_id,
                if metadata.suffix { "Suffix" } else { "Prefix" }
            ),
            &[
                metadata.cancel.then(|| "Cancellation signaled".into()),
                optional("Scope code", metadata.layer_idc),
                optional("Persistence code", metadata.persistence_idc),
                optional_hex("Extended-layer map", metadata.xlayer_map),
                embedded_maps,
                optional_hex("Reserved bits", metadata.reserved.map(u32::from)),
            ],
        ),
        scope,
        origin,
    )
}

fn content(details: &mut Vec<Detail>, scope: &str, origin: Option<u64>, value: ContentState) {
    let label = "Effective content interpretation";
    match value {
        ContentState::Unavailable => {
            details.push(detail(label, "Unavailable at this position", scope, origin));
        }
        ContentState::Unspecified { reset } => details.push(detail(
            label,
            "Unspecified after the random-access reset",
            scope,
            Some(reset),
        )),
        ContentState::Signaled {
            reset,
            sequence_header,
            inherited_from,
            scan_type,
            color_description,
            color_codes,
            full_range,
            chroma_position,
            sample_aspect_ratio,
            timing,
            reserved,
        } => {
            let source = if inherited_from.is_empty() {
                "Signaled for this layer".to_owned()
            } else {
                format!(
                    "Inherited from embedded layers {}",
                    inherited_from
                        .iter()
                        .map(u8::to_string)
                        .collect::<Vec<_>>()
                        .join(" → ")
                )
            };
            details.push(detail(label, source, scope, origin));
            let color = color_codes.map_or_else(
                || "Color primaries, transfer and matrix unspecified".into(),
                |[primaries, transfer, matrix]| {
                    format!("Color primaries {primaries} · Transfer {transfer} · Matrix {matrix}")
                },
            );
            details.push(detail(
                "Color and scan",
                with_codes(
                    &color,
                    &[
                        Some(format!("Scan type code {scan_type}")),
                        optional("Color description code", color_description),
                        full_range
                            .map(|range| if range { "Full range" } else { "Limited range" }.into()),
                        chroma_position.map(|[x, y]| format!("Chroma positions {x}, {y}")),
                        (reserved != 0).then(|| format!("Reserved bits {reserved:#x}")),
                    ],
                ),
                scope,
                origin,
            ));
            let aspect = match sample_aspect_ratio {
                Some([0, 0]) => "Sample aspect ratio unspecified".into(),
                Some([width, height]) => format!("Sample aspect ratio {width}:{height}"),
                None => "Sample aspect ratio unavailable".into(),
            };
            let timing = timing.map_or_else(
                || "Timing unspecified".into(),
                |[units, rate]| format!("Display tick {units}/{rate} s"),
            );
            details.push(detail(
                "Aspect ratio and timing",
                format!("{aspect} · {timing}"),
                scope,
                origin,
            ));
            details.push(detail(
                "Metadata lifetime",
                "Random-access frame reset",
                scope,
                Some(reset),
            ));
            details.push(detail(
                "Activated sequence header",
                if sequence_header.is_some() {
                    "In-stream header"
                } else {
                    "In-stream header unavailable"
                },
                scope,
                sequence_header,
            ));
        }
    }
}

fn tier_name(high_tier: bool) -> &'static str {
    if high_tier { "High" } else { "Main" }
}

fn profile_row(profile: &ProfileTierLevel, scope: &str, origin: Option<u64>) -> Detail {
    detail(
        format!(
            "Declared profile, tier and level · Extended layer {}",
            profile.xlayer
        ),
        with_codes(
            &format!(
                "Profile code {} · {} tier · Level index {}",
                profile.profile,
                tier_name(profile.high_tier),
                profile.level
            ),
            &[optional_hex(
                "Reserved bits",
                profile.reserved.map(u32::from),
            )],
        ),
        scope,
        origin,
    )
}

fn detail(
    label: impl Into<String>,
    value: impl Into<String>,
    context: &str,
    origin: Option<u64>,
) -> Detail {
    Detail {
        label: label.into(),
        value: value.into(),
        context: context.into(),
        origin,
        request: None,
    }
}

fn scope_name(scope: LayerScope) -> String {
    match scope {
        LayerScope::Global => "Global scope".into(),
        LayerScope::Extended { id } => format!("Extended layer {id}"),
    }
}

pub(crate) fn selection_name(selection: LayerDecodeSelection) -> String {
    match selection {
        LayerDecodeSelection::Default => "Stream default".into(),
        LayerDecodeSelection::OperatingPoint { scope, set, point } => {
            format!("{} · Set {set}, point {point}", scope_name(scope))
        }
    }
}

fn optional(label: &str, value: Option<impl std::fmt::Display>) -> Option<String> {
    value.map(|value| format!("{label} {value}"))
}

fn optional_hex(label: &str, value: Option<u32>) -> Option<String> {
    value.map(|value| format!("{label} {value:#x}"))
}

fn with_codes(first: &str, codes: &[Option<String>]) -> String {
    let mut line = first.to_owned();
    for code in codes.iter().flatten() {
        line.push_str(" · ");
        line.push_str(code);
    }
    line
}
