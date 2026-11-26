// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Replay one verified document prefix to observe one coded frame's reference state.

use std::io::{BufReader, Read};
use std::sync::{Arc, Mutex, PoisonError};

use splot_core::headers::frame::FrameSize;
use splot_core::stream_reader::{ReaderError, TemporalUnitReader};
use splot_decode::{
    DataProps, DecodeError, DecodeLimits, Decoder, ObuLocation, PictureProvenance,
    ReferenceFrameEvent, ReferenceMappingOrigin, ReferenceSlotState as NativeSlot, SendStatus,
};
use studio_domain::analysis::AnalysisFailure;
use studio_domain::identity::DecodeLayers;
use studio_domain::preview::GrainPolicy;
use studio_domain::references::{
    ReferenceDependency, ReferenceEvidence, ReferenceFrame, ReferenceOrigin, ReferenceQuery,
    ReferenceSlot, ReferenceSlotState,
};

use super::document::{Checked, MAX_UNIT_BYTES, Refusal};
use crate::stream::{operating_point_selection, preview_decoder_settings};

pub(super) fn replay(
    input: impl Read,
    limit: u64,
    partial: bool,
    query: ReferenceQuery,
    cancelled: &dyn Fn() -> bool,
) -> Result<ReferenceEvidence, Refusal> {
    let captured = Arc::new(Mutex::new(None));
    let output = Arc::clone(&captured);
    let selection = operating_point_selection(query.selection).map_err(decode_failed)?;
    let settings = preview_decoder_settings(1, DecodeLimits::DEFAULT, GrainPolicy::Skip)
        .with_operating_point_selection(selection);
    let mut decoder = Decoder::new_with_reference_observer(settings, move |event| {
        if absolute(event.frame.coded) == Some(query.coded_offset) {
            let mut captured = output.lock().unwrap_or_else(PoisonError::into_inner);
            *captured = Some(event);
        }
    })
    .map_err(decode_failed)?;

    let mut reader = TemporalUnitReader::with_max_unit_bytes(
        BufReader::new(Checked::new(input.take(limit), cancelled)),
        MAX_UNIT_BYTES,
    );
    loop {
        check_cancelled(cancelled)?;
        let unit = reader.next_temporal_unit().map_err(|error| {
            if cancelled() {
                cancelled_replay()
            } else {
                read_failed(error)
            }
        })?;
        check_cancelled(cancelled)?;
        let Some(unit) = unit else {
            return Err(unavailable(
                "The selected coded frame is outside replay coverage.",
            ));
        };
        let base = unit.offset.get();
        if partial && unit.pts.is_none() && base.checked_add(unit.bytes.len() as u64) == Some(limit)
        {
            return Err(unavailable(
                "The indexed prefix does not verify the end of the selected raw temporal unit.",
            ));
        }
        if base > query.coded_offset {
            break;
        }
        let contains_target = query.coded_offset - base < unit.bytes.len() as u64;
        let props = DataProps::default().with_user_data(base);
        if decoder
            .send_data(unit.bytes, props)
            .map_err(decode_failed)?
            != SendStatus::Accepted
        {
            return Err(unavailable("The decoder did not accept the replay unit."));
        }
        while !cancelled() {
            let picture = decoder.wait_picture();
            let observed = captured
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            if let Some(observed) = observed {
                check_cancelled(cancelled)?;
                return evidence(query, observed);
            }
            if picture.map_err(decode_failed)?.is_none() {
                break;
            }
        }
        check_cancelled(cancelled)?;
        if contains_target {
            break;
        }
    }
    Err(unavailable(
        "The decoder did not admit the selected coded frame under this operating point.",
    ))
}

fn evidence(
    query: ReferenceQuery,
    event: ReferenceFrameEvent,
) -> Result<ReferenceEvidence, Refusal> {
    let extended = event.extended_layer_id.get();
    let frame = frame(
        event.frame,
        extended,
        event.embedded_layer_id.get(),
        event.temporal_layer_id.get(),
        event.frame_size,
    )?;
    let before = &event.before.slots;
    let after = &event.after.slots;
    let slots = (0..before.len().max(after.len()))
        .map(|index| {
            Ok(ReferenceSlot {
                slot: slot_index(index)?,
                before: slot_state(before.get(index), extended)?,
                after: slot_state(after.get(index), extended)?,
            })
        })
        .collect::<Result<_, Refusal>>()?;

    let dependency = |slot: u32, index: Option<u8>, origin| {
        Ok(ReferenceDependency {
            index,
            slot: slot_index(slot as usize)?,
            origin,
            scaled: 1_u32
                .checked_shl(slot)
                .is_some_and(|bit| event.scaled_slots & bit != 0),
        })
    };
    let mut dependencies = Vec::new();
    if let Some(mapping) = event.mapping {
        let origin = match mapping.origin {
            ReferenceMappingOrigin::Signaled => ReferenceOrigin::Signaled,
            ReferenceMappingOrigin::Derived => ReferenceOrigin::Derived,
            ReferenceMappingOrigin::Bridge => ReferenceOrigin::Bridge,
            _ => {
                return Err(unavailable(
                    "The decoder returned an unknown reference origin.",
                ));
            }
        };
        for (index, &slot) in mapping.slots.iter().enumerate() {
            dependencies.push(dependency(slot, Some(slot_index(index)?), origin)?);
        }
    }
    if let Some(bridge) = event.bridge_slot
        && !dependencies.iter().any(|dependency| {
            u32::from(dependency.slot) == bridge && dependency.origin == ReferenceOrigin::Bridge
        })
    {
        dependencies.push(dependency(bridge, None, ReferenceOrigin::Bridge)?);
    }
    Ok(ReferenceEvidence {
        query,
        frame,
        frame_obu_type: event.obu_type.raw(),
        refresh_mask: event.refresh_frame_flags,
        resets_sequence: event.resets_sequence,
        allowed_input_slots: event.allowed_input_slots,
        required_long_term_ids: event.required_long_term_ids,
        slots,
        dependencies,
    })
}

fn slot_state(state: Option<&NativeSlot>, extended: u8) -> Result<ReferenceSlotState, Refusal> {
    Ok(match state {
        None => ReferenceSlotState::Absent,
        Some(NativeSlot::Empty) => ReferenceSlotState::Empty,
        Some(NativeSlot::Invalid) => ReferenceSlotState::Invalid,
        Some(NativeSlot::Retained(picture)) => ReferenceSlotState::Retained {
            frame: frame(
                picture.provenance,
                extended,
                picture.embedded_layer_id.get(),
                picture.temporal_layer_id.get(),
                picture.frame_size,
            )?,
            long_term_id: picture.long_term_id,
            restricted_order_hint: picture.restricted_order_hint,
        },
        Some(_) => ReferenceSlotState::Unavailable,
    })
}

fn frame(
    provenance: PictureProvenance,
    extended: u8,
    embedded: u8,
    temporal: u8,
    size: FrameSize,
) -> Result<ReferenceFrame, Refusal> {
    let location = |obu| {
        absolute(obu).ok_or_else(|| unavailable("The decoder returned an invalid source offset."))
    };
    Ok(ReferenceFrame {
        coded_offset: location(provenance.coded)?,
        sequence_offset: location(provenance.sequence_header)?,
        layers: DecodeLayers {
            extended,
            embedded,
            temporal,
        },
        width: size.width,
        height: size.height,
    })
}

fn absolute(location: ObuLocation) -> Option<u64> {
    location.user_data.checked_add(location.offset.get())
}

fn slot_index(index: usize) -> Result<u8, Refusal> {
    u8::try_from(index).map_err(|_| unavailable("The decoder returned an invalid slot index."))
}

fn check_cancelled(cancelled: &dyn Fn() -> bool) -> Result<(), Refusal> {
    if cancelled() {
        Err(cancelled_replay())
    } else {
        Ok(())
    }
}

fn cancelled_replay() -> Refusal {
    Refusal::new(
        AnalysisFailure::Cancelled,
        "The reference replay was cancelled.",
    )
}

fn unavailable(error: impl std::fmt::Display) -> Refusal {
    failure(AnalysisFailure::Unsupported, error)
}

fn read_failed(error: ReaderError) -> Refusal {
    match error {
        ReaderError::Io(_) => failure(AnalysisFailure::Io, error),
        ReaderError::UnitTooLarge { .. } => failure(AnalysisFailure::LimitReached, error),
        ReaderError::Ivf(_) => unavailable(error),
    }
}

fn decode_failed(error: DecodeError) -> Refusal {
    match error {
        DecodeError::Limit { .. } => failure(AnalysisFailure::LimitReached, error),
        DecodeError::Pool { .. } | DecodeError::Input { .. } | DecodeError::State { .. } => {
            failure(AnalysisFailure::Io, error)
        }
        _ => unavailable(error),
    }
}

fn failure(code: AnalysisFailure, error: impl std::fmt::Display) -> Refusal {
    Refusal::new(code, format!("Cannot inspect references. {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use studio_domain::layers::LayerDecodeSelection;

    #[test]
    fn ten_bit_replay_preserves_a_reference_across_embedded_layers()
    -> Result<(), Box<dyn std::error::Error>> {
        let source = include_bytes!("../../tests/preview/fixtures/syn-mlayer2-crossref-10bit.obu");
        let query = ReferenceQuery {
            coded_offset: 758,
            selection: LayerDecodeSelection::Default,
            generation: 1,
        };
        let evidence = replay(
            source.as_slice(),
            source.len() as u64,
            false,
            query,
            &|| false,
        )
        .map_err(|error| error.message)?;
        assert_eq!(evidence.frame.sequence_offset, 9);
        assert_eq!(evidence.frame.layers.embedded, 1);
        assert!(evidence.dependencies.iter().any(|dependency| {
            evidence.slots.iter().any(|slot| {
                slot.slot == dependency.slot
                    && matches!(
                        slot.before,
                        ReferenceSlotState::Retained { frame, .. }
                            if frame.coded_offset == 35 && frame.layers.embedded == 0
                    )
            })
        }));
        Ok(())
    }

    #[test]
    fn a_partial_raw_prefix_needs_the_next_temporal_delimiter()
    -> Result<(), Box<dyn std::error::Error>> {
        let source =
            include_bytes!("../../tests/preview/fixtures/syn-mlayer2-derived-sef-8bit.obu");
        let mut reader = TemporalUnitReader::new(source.as_slice());
        let first = reader
            .next_temporal_unit()?
            .ok_or("Expected the first unit.")?;
        let end = first.offset.get() + first.bytes.len() as u64;
        let query = ReferenceQuery {
            coded_offset: 44,
            selection: LayerDecodeSelection::Default,
            generation: 1,
        };

        let Err(refused) = replay(source.as_slice(), end, true, query, &|| false) else {
            return Err("A raw unit at a partial boundary must not reach the decoder.".into());
        };
        assert_eq!(refused.code, AnalysisFailure::Unsupported);
        assert!(refused.message.contains("does not verify the end"));

        let evidence = replay(
            source.as_slice(),
            source.len() as u64,
            false,
            query,
            &|| false,
        )
        .map_err(|error| error.message)?;
        assert_eq!(evidence.frame.coded_offset, query.coded_offset);
        Ok(())
    }
}
