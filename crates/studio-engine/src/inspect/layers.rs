// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Scoped declarations and native metadata resolved at the selected temporal unit.
//! Splot reads AV2 v1.0.0 multistream operations (§ 5.6), layer configuration
//! records (§ 5.8), operating points (§ 5.10–5.11), content interpretation
//! (§ 5.15), and metadata scope (§ 5.17). Splot tracks the content interpretation
//! lifetime and inheritance at the boundary in § 7.3.8.11.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufReader, Read};

use splot_core::annexb::{AnnexBObuCursor, ObuEnvelope};
use splot_core::headers::layer_config_record::{
    LayerConfigurationRecord, LcrSeqProfileTierLevelInfo, LcrXlayerInfo,
};
use splot_core::headers::metadata::{MetadataGroupObu, MetadataShortObu};
use splot_core::headers::operating_point_set::{
    OperatingPointPayload, OperatingPointSet, OpsMlayerSource,
};
use splot_core::headers::sequence::Tier;
use splot_core::hls::MultistreamDecoderOperation;
use splot_core::obu::{ParsedObu, PayloadStatus};
use splot_core::span::ByteOffset;
use splot_core::stream_reader::{StreamUnit, TemporalUnitReader};
use splot_core::types::{EmbeddedLayerId, ExtendedLayerId, ObuType, TemporalLayerId};
use splot_decode::OperatingPointTracker;
use splot_validate::{EffectiveContentInterpretation, LayerMetadataTracker, ValidationOptions};
use studio_domain::analysis::AnalysisFailure;
use studio_domain::layers::{
    ContentState, LayerDecodeSelection, LayerDependency, LayerEvidence, LayerFact, LayerRow,
    LayerScope, MetadataDeclaration, OperatingPointLayerSource, OperatingPointLayers,
    ProfileTierLevel,
};

use super::document::{Checked, MAX_UNIT_BYTES, Refusal};

const REPLAY_BYTES: u64 = 512 << 20;
const REPLAY_OBUS: u64 = 1 << 20;
const MAX_DECLARATIONS: usize = 1024;
const MAX_DECLARATION_ROWS: usize = 8192;

#[derive(Debug, Clone, Copy)]
pub(super) struct Query {
    pub element: u64,
    pub offset: u64,
    pub source_len: u64,
    pub selection: LayerDecodeSelection,
}

pub(super) type Report = (LayerEvidence, Vec<LayerRow>);

pub(super) fn inspect(
    file: File,
    query: Query,
    cancelled: &dyn Fn() -> bool,
) -> Result<Report, Refusal> {
    inspect_input(file, query, cancelled)
}

fn inspect_input(
    input: impl Read,
    query: Query,
    cancelled: &dyn Fn() -> bool,
) -> Result<Report, Refusal> {
    if query.offset >= REPLAY_BYTES {
        return Err(Refusal::new(
            AnalysisFailure::LimitReached,
            "The selected OBU is beyond the 512 MiB layer replay limit.",
        ));
    }
    let selection = crate::stream::operating_point_selection(query.selection)
        .and_then(OperatingPointTracker::new)
        .map_err(|error| Refusal::new(AnalysisFailure::InvalidRequest, error.to_string()))?;
    let mut evidence = Replay {
        query,
        selection,
        metadata: LayerMetadataTracker::new(ValidationOptions::default()),
        presence: BTreeMap::new(),
        declarations: BTreeMap::new(),
        declaration_rows: 0,
        catalog_limited: false,
        seen_anchor: false,
        observed: 0,
        unit: None,
    };
    let read = Checked::new(input, cancelled).take(REPLAY_BYTES);
    let mut units = TemporalUnitReader::with_max_unit_bytes(BufReader::new(read), MAX_UNIT_BYTES);
    let mut clean_end = false;
    let mut framing_error = None;
    'replay: loop {
        check_cancel(cancelled)?;
        let (bytes, offset) = match units.next_unit() {
            Ok(Some(StreamUnit::AnnexBObu { offset, bytes })) => (bytes, offset),
            Ok(Some(StreamUnit::IvfFrame {
                payload_offset,
                payload,
                ..
            })) => (payload, payload_offset),
            Ok(Some(StreamUnit::IvfWarning(_))) => break,
            Ok(None) => {
                clean_end = true;
                break;
            }
            Err(error) => {
                check_cancel(cancelled)?;
                framing_error = Some(error.to_string());
                break;
            }
        };
        let mut obus = AnnexBObuCursor::new(bytes, offset);
        loop {
            check_cancel(cancelled)?;
            let obu = match obus.next_obu() {
                Ok(Some(obu)) => obu,
                Ok(None) => break,
                Err(error) => {
                    framing_error = Some(error.to_string());
                    break 'replay;
                }
            };
            if evidence.observe(&obu) {
                break 'replay;
            }
        }
    }
    let capped = query.source_len > REPLAY_BYTES && units.get_ref().get_ref().limit() == 0;
    if clean_end && !capped && evidence.seen_anchor {
        evidence.unit = evidence.metadata.finish();
    }
    evidence.finish(capped, framing_error)
}

type Triple = (LayerScope, u8, u8);
type DeclarationKey = (LayerScope, u8, u32, u8);

struct Replay {
    query: Query,
    selection: OperatingPointTracker,
    metadata: LayerMetadataTracker,
    presence: BTreeMap<Triple, (u64, u64, u64)>,
    declarations: BTreeMap<DeclarationKey, Vec<LayerRow>>,
    declaration_rows: usize,
    catalog_limited: bool,
    seen_anchor: bool,
    observed: u64,
    unit: Option<u64>,
}

impl Replay {
    fn observe(&mut self, obu: &ObuEnvelope<'_>) -> bool {
        if self.observed == REPLAY_OBUS {
            return true;
        }
        self.observed += 1;
        let offset = obu.offset.get();
        if let Some(tu) = self.metadata.observe(obu)
            && self.seen_anchor
            && offset > self.query.offset
        {
            self.unit = Some(tu);
            return true;
        }
        if offset > self.query.offset {
            return !self.seen_anchor;
        }
        if offset == self.query.offset {
            self.seen_anchor = true;
        }
        let _ = self.selection.observe(obu);
        let scope = scope(obu.header.extended_layer_id);
        let key = (
            scope,
            obu.header.embedded_layer_id.get(),
            obu.header.temporal_layer_id.get(),
        );
        let (_, last, count) = self.presence.entry(key).or_insert((offset, offset, 0));
        *last = offset;
        *count += 1;
        self.declare(scope, obu);
        false
    }

    fn declare(&mut self, scope: LayerScope, obu: &ObuEnvelope<'_>) {
        let kind = obu.header.obu_type;
        if !matches!(
            kind,
            ObuType::OperatingPointSet
                | ObuType::LayerConfigurationRecord
                | ObuType::Msdo
                | ObuType::SequenceHeader
                | ObuType::MetadataShort
                | ObuType::MetadataGroup
        ) {
            return;
        }
        let origin = Some(obu.offset.get());
        let row = |fact| LayerRow {
            scope,
            origin,
            fact,
        };
        let (id, sub_id, rows) = match obu.payload_status() {
            Ok(PayloadStatus::Parsed(ParsedObu::OperatingPointSet(ops))) => {
                let rows = if ops.payloads.is_empty() {
                    vec![row(operating_point(&ops, None))]
                } else {
                    ops.payloads
                        .iter()
                        .map(|point| row(operating_point(&ops, Some(point))))
                        .collect()
                };
                (u32::from(ops.ops_id), 0, rows)
            }
            Ok(PayloadStatus::Parsed(ParsedObu::LayerConfigurationRecord(lcr))) => {
                configuration(&lcr).map_or_else(
                    || (0, 0, vec![row(metadata_fact(kind, None))]),
                    |(global_id, local_id, fact)| {
                        (
                            u32::from(global_id),
                            local_id.unwrap_or_default(),
                            vec![row(fact)],
                        )
                    },
                )
            }
            Ok(PayloadStatus::Parsed(ParsedObu::Msdo(msdo))) => {
                (0, 0, vec![row(composition(msdo))])
            }
            Ok(PayloadStatus::Parsed(ParsedObu::SequenceHeader(sh))) => {
                let general = &sh.general;
                let id = general.seq_header_id.get();
                (
                    u32::from(id),
                    0,
                    vec![row(LayerFact::SequenceHeader {
                        id,
                        max_width: general.max_frame_width.get(),
                        max_height: general.max_frame_height.get(),
                        profile: general.seq_profile_idc.get(),
                        level: general.seq_level_idx.get(),
                        high_tier: general.seq_tier == Tier::High,
                    })],
                )
            }
            Ok(PayloadStatus::Parsed(ParsedObu::MetadataShort(metadata))) => {
                let metadata = short_metadata(&metadata);
                let type_id = metadata.type_id;
                let suffix = u8::from(metadata.suffix);
                (
                    type_id,
                    suffix,
                    vec![row(metadata_fact(kind, Some(metadata)))],
                )
            }
            Ok(PayloadStatus::Parsed(ParsedObu::MetadataGroup(group))) => {
                const MAX_GROUP_ROWS: usize = 64;
                self.catalog_limited |= group.units.len() > MAX_GROUP_ROWS;
                let rows = group_metadata(&group)
                    .take(MAX_GROUP_ROWS)
                    .map(|metadata| row(metadata_fact(kind, Some(metadata))))
                    .collect();
                (0, u8::from(group.metadata_is_suffix), rows)
            }
            _ => (0, 0, vec![row(metadata_fact(kind, None))]),
        };
        self.retain((scope, kind.raw(), id, sub_id), rows);
    }

    fn retain(&mut self, key: DeclarationKey, rows: Vec<LayerRow>) {
        let previous = self.declarations.get(&key);
        let before = previous.map_or(0, Vec::len);
        let count = self.declaration_rows - before + rows.len();
        if count <= MAX_DECLARATION_ROWS
            && (previous.is_some() || self.declarations.len() < MAX_DECLARATIONS)
        {
            self.declarations.insert(key, rows);
            self.declaration_rows = count;
        } else {
            self.catalog_limited = true;
        }
    }

    fn finish(self, capped: bool, framing_error: Option<String>) -> Result<Report, Refusal> {
        if !self.seen_anchor {
            return Err(if capped || self.observed == REPLAY_OBUS {
                Refusal::new(
                    AnalysisFailure::LimitReached,
                    "Layer replay reached its read limit before the selected OBU.",
                )
            } else {
                let message = framing_error.map_or_else(
                    || "Layer replay stopped before the selected OBU.".to_owned(),
                    |error| format!("Cannot read framing before the selected OBU. {error}"),
                );
                Refusal::new(AnalysisFailure::Unsupported, message)
            });
        }
        let mut rows = Vec::new();
        let mut pairs = BTreeSet::new();
        for ((scope, embedded, temporal), (first, last, count)) in self.presence {
            let member = match scope {
                LayerScope::Global => None,
                LayerScope::Extended { id } => {
                    pairs.insert((id, embedded));
                    self.selection.contains(
                        ExtendedLayerId::from_bits(id),
                        EmbeddedLayerId::from_bits(embedded),
                        TemporalLayerId::from_bits(temporal),
                    )
                }
            };
            rows.push(LayerRow {
                scope,
                origin: Some(first),
                fact: LayerFact::Presence {
                    embedded,
                    temporal,
                    count,
                    last,
                    member,
                },
            });
        }
        rows.extend(self.declarations.into_values().flatten());
        // Include inherited state for layers that have not yet emitted an OBU.
        for xlayer in 0..31 {
            for embedded in 0..8 {
                let native = if self.unit.is_some() {
                    self.metadata.content_interpretation(
                        ExtendedLayerId::from_bits(xlayer),
                        EmbeddedLayerId::from_bits(embedded),
                    )
                } else {
                    EffectiveContentInterpretation::Unavailable
                };
                if !matches!(&native, EffectiveContentInterpretation::Unavailable)
                    || pairs.contains(&(xlayer, embedded))
                {
                    let (origin, value) = content(&native);
                    rows.push(LayerRow {
                        scope: LayerScope::Extended { id: xlayer },
                        origin,
                        fact: LayerFact::ContentInterpretation { embedded, value },
                    });
                }
            }
        }
        Ok((
            LayerEvidence {
                element: self.query.element,
                offset: self.query.offset,
                temporal_unit: self.unit,
                selection: self.query.selection,
                membership_reason: self.selection.unavailable_reason().map(str::to_owned),
                metadata_complete: self.unit.is_some(),
                catalog_limited: self.catalog_limited,
            },
            rows,
        ))
    }
}

fn scope(xlayer: ExtendedLayerId) -> LayerScope {
    if xlayer.is_global() {
        LayerScope::Global
    } else {
        LayerScope::Extended { id: xlayer.get() }
    }
}

fn operating_point(set: &OperatingPointSet, point: Option<&OperatingPointPayload>) -> LayerFact {
    let entries = point.map_or(&[][..], |point| point.xlayer_entries.as_slice());
    let profiles = entries
        .iter()
        .filter_map(|entry| entry.ptl_info.as_ref())
        .map(|ptl| ProfileTierLevel {
            xlayer: ptl.target_xlayer_id.get(),
            profile: ptl.seq_profile_idc.get(),
            level: ptl.level_idx,
            high_tier: ptl.tier_flag,
            reserved: Some(ptl.reserved_2bits),
        })
        .collect();
    let layers = entries
        .iter()
        .map(|entry| OperatingPointLayers {
            xlayer: entry.xlayer_id.get(),
            source: match &entry.mlayer {
                OpsMlayerSource::Absent => OperatingPointLayerSource::Absent,
                OpsMlayerSource::Explicit(info) => OperatingPointLayerSource::Explicit {
                    embedded_mask: info.mlayer_map,
                    temporal_masks: info.tlayer_maps.clone(),
                },
                OpsMlayerSource::Inherited {
                    embedded_ops_id,
                    embedded_op_index,
                } => OperatingPointLayerSource::Inherited {
                    set: *embedded_ops_id,
                    point: *embedded_op_index,
                },
                _ => OperatingPointLayerSource::Unknown,
            },
        })
        .collect();
    LayerFact::OperatingPoint {
        set: set.ops_id,
        point: point.map(|point| point.index),
        reset: set.reset_flag,
        xlayer_map: point.and_then(|point| point.xlayer_map),
        profiles,
        layers,
        layer_info_mode: set.mlayer_info_idc,
        priority: set.priority,
        intent: point.and_then(|point| point.op_intent).or(set.intent),
        reserved: set.local_reserved_2bits,
    }
}

fn composition(msdo: MultistreamDecoderOperation) -> LayerFact {
    LayerFact::Composition {
        streams: msdo.num_streams(),
        profile: msdo.multistream_profile_idc.get(),
        level: msdo.multistream_level_idx,
        high_tier: msdo.multistream_tier != 0,
        substreams: msdo
            .sub_streams()
            .iter()
            .map(|entry| ProfileTierLevel {
                xlayer: entry.sub_xlayer_id,
                profile: entry.sub_stream_max_profile,
                level: entry.sub_stream_max_level,
                high_tier: entry.sub_stream_max_tier != 0,
                reserved: None,
            })
            .collect(),
    }
}

fn short_metadata(metadata: &MetadataShortObu) -> MetadataDeclaration {
    MetadataDeclaration {
        type_id: metadata.metadata_type.value(),
        suffix: metadata.metadata_is_suffix,
        cancel: metadata.muh_cancel_flag,
        layer_idc: Some(metadata.muh_layer_idc),
        persistence_idc: Some(metadata.muh_persistence_idc),
        xlayer_map: None,
        embedded_maps: Vec::new(),
        reserved: None,
    }
}

fn group_metadata(group: &MetadataGroupObu) -> impl Iterator<Item = MetadataDeclaration> + '_ {
    group.units.iter().map(|entry| MetadataDeclaration {
        type_id: entry.metadata_type.value(),
        suffix: group.metadata_is_suffix,
        cancel: entry.muh_cancel_flag,
        layer_idc: entry.muh_layer_idc,
        persistence_idc: entry.muh_persistence_idc,
        xlayer_map: entry.muh_xlayer_map,
        embedded_maps: entry.muh_mlayer_maps.clone(),
        reserved: entry.muh_reserved_zero_2bits,
    })
}

fn metadata_fact(kind: ObuType, declaration: Option<MetadataDeclaration>) -> LayerFact {
    LayerFact::Metadata {
        name: kind.spec_name().to_owned(),
        declaration,
    }
}

fn configuration(lcr: &LayerConfigurationRecord) -> Option<(u8, Option<u8>, LayerFact)> {
    let mut dependencies = Vec::new();
    let (global_id, local_id, xlayer_map, profiles) = match lcr {
        LayerConfigurationRecord::Global(info) => {
            for layer in &info.payloads {
                if let Some(mask) = layer.num_dependent_xlayer_map {
                    dependencies.push(LayerDependency {
                        xlayer: layer.xlayer_id,
                        mlayer: None,
                        mask,
                    });
                }
                embedded_dependencies(layer.xlayer_id, &layer.xlayer_info, &mut dependencies);
            }
            (
                info.global_config_record_id,
                None,
                Some(info.xlayer_map),
                info.seq_ptl_infos
                    .iter()
                    .copied()
                    .map(lcr_profile)
                    .collect(),
            )
        }
        LayerConfigurationRecord::Local(info) => {
            embedded_dependencies(info.xlayer_id, &info.xlayer_info, &mut dependencies);
            (
                info.global_id,
                Some(info.local_id),
                None,
                info.seq_ptl_info.iter().copied().map(lcr_profile).collect(),
            )
        }
        _ => return None,
    };
    Some((
        global_id,
        local_id,
        LayerFact::Configuration {
            global_id,
            local_id,
            xlayer_map,
            profiles,
            dependencies,
            reserved_nonzero: lcr.has_nonzero_reserved_bits(),
        },
    ))
}

fn lcr_profile(info: LcrSeqProfileTierLevelInfo) -> ProfileTierLevel {
    ProfileTierLevel {
        xlayer: info.xlayer_id,
        profile: info.seq_profile_idc.get(),
        level: info.max_level_idx,
        high_tier: info.tier_flag,
        reserved: Some(info.reserved_2bits),
    }
}

fn embedded_dependencies(
    xlayer: u8,
    info: &LcrXlayerInfo,
    dependencies: &mut Vec<LayerDependency>,
) {
    if let Some(embedded) = &info.embedded_layer_info {
        dependencies.extend(embedded.layers.iter().filter_map(|layer| {
            layer.dependent_layer_map.map(|mask| LayerDependency {
                xlayer,
                mlayer: Some(layer.mlayer_index),
                mask,
            })
        }));
    }
}

fn content(native: &EffectiveContentInterpretation<'_>) -> (Option<u64>, ContentState) {
    match native {
        EffectiveContentInterpretation::Unspecified { reset } => {
            (None, ContentState::Unspecified { reset: reset.get() })
        }
        EffectiveContentInterpretation::Signaled {
            reset,
            origin,
            inherited_from,
            sequence_header,
            content,
        } => {
            let color = content.color_description.as_ref();
            let color_codes = color.map(|description| {
                let codes = description.derived().code_points;
                [
                    codes.color_primaries,
                    codes.transfer_characteristics,
                    codes.matrix_coefficients,
                ]
            });
            (
                Some(origin.get()),
                ContentState::Signaled {
                    reset: reset.get(),
                    sequence_header: sequence_header.map(ByteOffset::get),
                    inherited_from: inherited_from.iter().map(|id| id.get()).collect(),
                    scan_type: content.scan_type_idc.get(),
                    color_description: color.map(|description| description.color_description_idc),
                    color_codes,
                    full_range: color.map(|description| description.full_range_flag),
                    chroma_position: content
                        .chroma_sample_position
                        .as_ref()
                        .map(|position| [position.top, position.bottom]),
                    sample_aspect_ratio: content
                        .derived_sample_aspect_ratio()
                        .map(|(width, height)| [width, height]),
                    timing: content
                        .timing_info
                        .as_ref()
                        .map(|timing| [timing.num_units_in_display_tick, timing.time_scale]),
                    reserved: content.reserved_2bit,
                },
            )
        }
        _ => (None, ContentState::Unavailable),
    }
}

fn check_cancel(cancelled: &dyn Fn() -> bool) -> Result<(), Refusal> {
    if cancelled() {
        Err(Refusal::new(
            AnalysisFailure::Cancelled,
            "Layer inspection stopped.",
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_prefix_of_a_large_source_is_not_a_replay_limit() {
        let bytes = [1, 8, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80];
        let result = inspect_input(
            bytes.as_slice(),
            Query {
                element: 20,
                offset: 40,
                source_len: REPLAY_BYTES + 1,
                selection: LayerDecodeSelection::Default,
            },
            &|| false,
        );
        assert!(matches!(
            result,
            Err(Refusal {
                code: AnalysisFailure::Unsupported,
                ..
            })
        ));
    }
}
