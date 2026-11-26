// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Native frame identities, checked clocks and scoped byte statistics.

use serde::{Deserialize, Serialize};

/// Maximum number of buckets in a timeline response.
pub const MAX_TIMELINE_BUCKETS: u32 = 96;

/// Which order and byte scope to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineView {
    /// Count the coded frame's OBU and its continuation OBUs.
    Coded,
    /// Count each display OBU once. Later outputs from that OBU add zero bytes.
    Output,
}

/// Exact time in seconds. A valid denominator is greater than zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineTime {
    /// Numerator.
    #[serde(with = "numerator")]
    pub num: u128,
    /// Denominator.
    pub den: u64,
}

/// State of the output presentation clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineClock {
    /// No valid absolute clock exists.
    Unavailable,
    /// All indexed output timestamps use the IVF time base and never decrease.
    Ivf,
    /// One IVF packet timestamp covers multiple temporal units.
    Conflicting,
    /// Output timestamps decrease. A time search or rate is ambiguous.
    Discontinuous,
}

/// A native quantizer value with its origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TimelineQuantizer {
    /// The native decoder did not provide this value.
    Unavailable,
    /// The frame header's base quantizer index.
    HeaderBase {
        /// Unscaled quantizer index from the frame header.
        value: u32,
    },
    /// Derived from TIP reference quantizers.
    DerivedBase {
        /// Base quantizer index before segment or block deltas.
        value: u32,
    },
    /// Inherited from a bridge or inactive BRU reference.
    InheritedBase {
        /// Base quantizer index before segment or block deltas.
        value: u32,
    },
}

/// Exact identities linked to a timeline row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineSelection {
    /// Index in coded order.
    pub coded_ordinal: u64,
    /// File offset of the first coded OBU header.
    pub coded_offset: u64,
    /// Index in output order, when one is known.
    pub output_ordinal: Option<u64>,
    /// File offset of the OBU header that requested this output.
    pub showing_offset: Option<u64>,
    /// Exact IVF presentation timestamp, if present.
    pub time: Option<TimelineTime>,
}

/// A request to locate an exact row and related identities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TimelineTarget {
    /// Resolve a coded row and its first known output.
    Coded {
        /// Index in coded order.
        index: u64,
    },
    /// Resolve an output row and its coded frame.
    Output {
        /// Index in output order.
        index: u64,
    },
    /// Resolve the first output at or after an exact time on a complete IVF clock.
    Time {
        /// Target time.
        time: TimelineTime,
    },
}

/// Facts available for one exact row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineEntry {
    /// Index in this page's order.
    pub ordinal: u64,
    /// File offset of the first coded OBU header.
    pub coded_offset: u64,
    /// File offset of the display OBU header, if applicable.
    pub showing_offset: Option<u64>,
    /// File offset of the sequence header.
    pub sequence_offset: u64,
    /// Index in coded order.
    pub coded_ordinal: u64,
    /// Index in output order, when one is known.
    pub output_ordinal: Option<u64>,
    /// Indexed OBU bytes in this page's scope.
    pub bytes: u64,
    /// Exact IVF presentation timestamp, if present.
    pub time: Option<TimelineTime>,
    /// Signaled duration between output temporal units when available.
    pub interval: Option<TimelineTime>,
    /// The native parser identified a random access point for this coded row.
    pub random_access: bool,
    /// Native frame header quantizer.
    pub quantizer: TimelineQuantizer,
}

/// Exact byte extrema and sum. Divide the sum by the row count for the mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ByteDistribution {
    /// Smallest byte count of one row.
    pub min: u64,
    /// Largest byte count of one row.
    pub max: u64,
    /// Sum of the byte counts of all rows in the range.
    pub sum: u64,
}

/// Counts for the file and the classified prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineFileBytes {
    /// Total source length in bytes.
    pub file: u64,
    /// OBU bytes, including OBU headers.
    pub obu: u64,
    /// OBU bytes assigned to coded frames or display events, counted once.
    pub assigned_obu: u64,
    /// OBU bytes outside those assignments, such as sequence headers and padding.
    pub unassigned_obu: u64,
    /// Length prefixes and IVF wrapper bytes.
    pub framing: u64,
    /// Bytes outside the classified prefix, including any unparsed tail.
    pub unclassified: u64,
}

/// One contiguous group of rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineBucket {
    /// Index of its first row.
    pub first: u64,
    /// Number of rows.
    pub count: u64,
    /// Byte distribution over exactly these rows.
    pub bytes: Option<ByteDistribution>,
    /// Present for a bucket with one row.
    pub entry: Option<TimelineEntry>,
}

/// A bounded timeline response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelinePage {
    /// Order and byte scope.
    pub view: TimelineView,
    /// Index of the requested first row.
    pub first: u64,
    /// Number of currently indexed rows in this order.
    pub total: u64,
    /// The identity decoder reached end of input and emitted all output.
    pub complete: bool,
    /// The identity and structure scans read matching bytes for the full source.
    pub verified: bool,
    /// The state of the presentation clock.
    pub clock: TimelineClock,
    /// Byte statistics for the source.
    pub file_bytes: TimelineFileBytes,
    /// Distribution over the selected rows.
    pub bytes: Option<ByteDistribution>,
    /// Bits per second in the output-display scope over the selection.
    /// The window ends at the next output timestamp after the last selected row.
    pub rate: Option<TimelineTime>,
    /// Contiguous resolution buckets covering the selected rows.
    pub buckets: Vec<TimelineBucket>,
}

// Serde's tagged enums buffer values. Use a decimal string for the full precision of u128.
mod numerator {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(value: &u128, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(value)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u128, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        Command, Envelope, Event, ProtocolError, decode, read_frame, write_message,
    };

    #[test]
    fn a_full_resolution_page_fits_the_wire_budget() -> Result<(), ProtocolError> {
        let time = Some(TimelineTime {
            num: u128::MAX,
            den: u64::MAX,
        });
        let bytes = Some(ByteDistribution {
            min: u64::MAX,
            max: u64::MAX,
            sum: u64::MAX,
        });
        let entry = Some(TimelineEntry {
            ordinal: u64::MAX,
            coded_offset: u64::MAX,
            showing_offset: Some(u64::MAX),
            sequence_offset: u64::MAX,
            coded_ordinal: u64::MAX,
            output_ordinal: Some(u64::MAX),
            bytes: u64::MAX,
            time,
            interval: time,
            random_access: false,
            quantizer: TimelineQuantizer::InheritedBase { value: u32::MAX },
        });
        let page = TimelinePage {
            view: TimelineView::Output,
            first: u64::MAX,
            total: u64::MAX,
            complete: false,
            verified: false,
            clock: TimelineClock::Discontinuous,
            file_bytes: TimelineFileBytes {
                file: u64::MAX,
                obu: u64::MAX,
                assigned_obu: u64::MAX,
                unassigned_obu: u64::MAX,
                framing: u64::MAX,
                unclassified: u64::MAX,
            },
            bytes,
            rate: time,
            buckets: (0..MAX_TIMELINE_BUCKETS)
                .map(|_| TimelineBucket {
                    first: u64::MAX,
                    count: 1,
                    bytes,
                    entry,
                })
                .collect(),
        };
        let envelope = Envelope::new(
            u64::MAX,
            Event::AnalysisTimeline {
                document: u64::MAX,
                scan: u64::MAX,
                page: Box::new(page),
            },
        );
        let mut wire = Vec::new();
        write_message(&mut wire, &envelope)?;
        let frame = read_frame(&mut wire.as_slice())?.ok_or(ProtocolError::Truncated)?;
        let decoded: Envelope<Event> = decode(&frame)?;
        assert_eq!(decoded, envelope);
        Ok(())
    }

    #[test]
    fn tagged_timeline_request_round_trips_a_wide_rational() -> Result<(), ProtocolError> {
        let command = Command::AnalysisTimelineLocate {
            document: 5,
            scan: 8,
            target: TimelineTarget::Time {
                time: TimelineTime {
                    num: u128::MAX,
                    den: u64::MAX,
                },
            },
        };
        let envelope = Envelope::new(9, command);
        let mut bytes = Vec::new();
        write_message(&mut bytes, &envelope)?;
        let frame = read_frame(&mut bytes.as_slice())?.ok_or(ProtocolError::Truncated)?;
        let decoded: Envelope<Command> = decode(&frame)?;
        assert_eq!(decoded, envelope);
        Ok(())
    }
}
