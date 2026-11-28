// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Presentation clocks and the mapping of output ordinals between two sides.
//!
//! A [`Clock`] holds the exact container timestamps of all outputs of one
//! side, in output order, as runs with a constant step. A [`Mapping`] puts
//! every output of both sides in one segment: matched, or without a match.
//! Nothing here reads a coded frame index.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::Side;
use crate::job::Rational;

/// The most cadence changes a [`Clock`] keeps.
pub const MAX_CLOCK_RUNS: usize = 64;

/// The most segments a [`Mapping`] keeps.
pub const MAX_ALIGNMENT_SEGMENTS: usize = 256;

/// A stretch of outputs with a constant timestamp step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClockRun {
    /// The output ordinal of the first output. The run ends at the next run or the last output.
    pub first: u64,
    /// The timestamp of the first output, in ticks.
    pub start: u64,
    /// Ticks between consecutive outputs. Zero for a run that holds one output.
    pub step: u64,
}

/// Why timestamps cannot place each output at one time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClockFault {
    /// Two consecutive outputs have the same timestamp.
    Repeated,
    /// A timestamp is earlier than the one before it.
    Decreasing,
    /// One container timestamp covers more than one temporal unit.
    SharedPacket,
    /// The cadence changes more than [`MAX_CLOCK_RUNS`] times.
    Irregular,
    /// The time base is zero, or the runs are not valid.
    Invalid,
}

/// The exact presentation times of all outputs of one side, in output order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Clock {
    /// The side carries no presentation timestamps.
    Absent,
    /// Strictly increasing timestamps for every output.
    Exact {
        /// Seconds per tick.
        tick: Rational,
        /// The runs, by increasing first ordinal. The first run starts at ordinal zero.
        runs: Vec<ClockRun>,
    },
    /// Timestamps exist, but they do not order the outputs.
    Unusable {
        /// The first fault found.
        fault: ClockFault,
    },
}

impl Clock {
    /// A constant rate from time zero: frame `n` of a source at `rate` frames per second.
    #[must_use]
    pub fn constant(rate: Rational) -> Self {
        Self::Exact {
            tick: Rational {
                num: rate.den,
                den: rate.num,
            },
            runs: vec![ClockRun {
                first: 0,
                start: 0,
                step: 1,
            }],
        }
    }
}

/// Collects the timestamps of consecutive outputs into a [`Clock`].
#[derive(Debug)]
pub struct ClockBuilder {
    tick: Rational,
    runs: Vec<ClockRun>,
    /// The number of outputs seen and the timestamp of the last one.
    last: Option<(u64, u64)>,
    fault: Option<ClockFault>,
}

impl ClockBuilder {
    /// A builder for timestamps in ticks of `tick` seconds.
    #[must_use]
    pub const fn new(tick: Rational) -> Self {
        Self {
            tick,
            runs: Vec::new(),
            last: None,
            fault: if tick.num == 0 || tick.den == 0 {
                Some(ClockFault::Invalid)
            } else {
                None
            },
        }
    }

    /// Records that the timestamps are unusable. The first fault stays.
    pub const fn fail(&mut self, fault: ClockFault) {
        if self.fault.is_none() {
            self.fault = Some(fault);
        }
    }

    /// Adds the timestamp of the next output.
    pub fn push(&mut self, ticks: u64) {
        let Some((count, last)) = self.last.replace((
            self.last.map_or(1, |(count, _)| count.saturating_add(1)),
            ticks,
        )) else {
            self.runs.push(ClockRun {
                first: 0,
                start: ticks,
                step: 0,
            });
            return;
        };
        if self.fault.is_some() {
            return;
        }
        let Some(step) = ticks.checked_sub(last).filter(|step| *step > 0) else {
            self.fail(if ticks == last {
                ClockFault::Repeated
            } else {
                ClockFault::Decreasing
            });
            return;
        };
        let Some(run) = self.runs.last_mut() else {
            return;
        };
        if run.first + 1 == count {
            run.step = step;
        } else if run.step != step {
            if self.runs.len() == MAX_CLOCK_RUNS {
                self.fail(ClockFault::Irregular);
                return;
            }
            self.runs.push(ClockRun {
                first: count,
                start: ticks,
                step: 0,
            });
        }
    }

    /// The clock of the outputs added. No output gives [`Clock::Absent`].
    #[must_use]
    pub fn finish(self) -> Clock {
        match (self.fault, self.last) {
            (_, None) => Clock::Absent,
            (Some(fault), _) => Clock::Unusable { fault },
            (None, Some(_)) => Clock::Exact {
                tick: self.tick,
                runs: self.runs,
            },
        }
    }
}

/// A run of output ordinals in a [`Mapping`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Segment {
    /// Outputs `a + n` and `b + n` match for `n` below `count`.
    Matched {
        /// The first ordinal of side A.
        a: u64,
        /// The first ordinal of side B.
        b: u64,
        /// The number of pairs.
        count: u64,
    },
    /// Outputs of side A with no match.
    OnlyA {
        /// The first ordinal.
        first: u64,
        /// The number of outputs.
        count: u64,
    },
    /// Outputs of side B with no match.
    OnlyB {
        /// The first ordinal.
        first: u64,
        /// The number of outputs.
        count: u64,
    },
}

/// The alignment of every output of both sides. Each ordinal of each side is
/// in exactly one segment, and the ordinals of each side increase.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mapping {
    /// The segments in presentation order.
    pub segments: Vec<Segment>,
}

impl Mapping {
    /// The number of matched pairs.
    #[must_use]
    pub fn matched(&self) -> u64 {
        self.sum(|segment| match *segment {
            Segment::Matched { count, .. } => count,
            _ => 0,
        })
    }

    /// The number of unmatched outputs of `side`.
    #[must_use]
    pub fn unmatched(&self, side: Side) -> u64 {
        self.sum(|segment| match (*segment, side) {
            (Segment::OnlyA { count, .. }, Side::A) | (Segment::OnlyB { count, .. }, Side::B) => {
                count
            }
            _ => 0,
        })
    }

    fn sum(&self, count: impl Fn(&Segment) -> u64) -> u64 {
        self.segments.iter().map(count).sum()
    }

    /// The output of the other side that matches `ordinal` of `side`.
    #[must_use]
    pub fn partner(&self, side: Side, ordinal: u64) -> Option<u64> {
        self.segments.iter().find_map(|segment| {
            let Segment::Matched { a, b, count } = *segment else {
                return None;
            };
            let (from, to) = match side {
                Side::A => (a, b),
                Side::B => (b, a),
            };
            let within = ordinal.checked_sub(from).filter(|n| *n < count)?;
            Some(to + within)
        })
    }

    fn push(&mut self, segment: Segment) -> Result<(), AlignmentRefusal> {
        let joined = match (self.segments.last_mut(), segment) {
            (
                Some(Segment::Matched { a, b, count }),
                Segment::Matched {
                    a: next_a,
                    b: next_b,
                    count: more,
                },
            ) if *a + *count == next_a && *b + *count == next_b => {
                *count += more;
                true
            }
            (
                Some(Segment::OnlyA { first, count }),
                Segment::OnlyA {
                    first: next,
                    count: more,
                },
            )
            | (
                Some(Segment::OnlyB { first, count }),
                Segment::OnlyB {
                    first: next,
                    count: more,
                },
            ) if *first + *count == next => {
                *count += more;
                true
            }
            _ => false,
        };
        if !joined {
            if self.segments.len() == MAX_ALIGNMENT_SEGMENTS {
                return Err(AlignmentRefusal::Fragmented);
            }
            self.segments.push(segment);
        }
        Ok(())
    }
}

/// Why the chosen basis gives no mapping. The user must then choose another
/// basis, for example a labelled index offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum AlignmentRefusal {
    /// The sides do not come from one recorded source.
    #[error("No encode job links the two sides to one source.")]
    NoProvenance,
    /// A side has no presentation timestamps.
    #[error("Side {0} has no presentation timestamps.")]
    NoTimestamps(Side),
    /// The timestamps of a side do not order its outputs.
    #[error("The timestamps of side {0} do not give each output one time.")]
    UnusableClock(Side, ClockFault),
    /// No output of side A has the timestamp of an output of side B.
    #[error("The two sides have no equal timestamps.")]
    Disjoint,
    /// The mapping needs more than [`MAX_ALIGNMENT_SEGMENTS`] segments.
    #[error("The timestamps match in too many separate pieces.")]
    Fragmented,
}

/// The output count and clock of one side.
#[derive(Debug, Clone, Copy)]
pub struct Track<'a> {
    /// The number of outputs in display order.
    pub outputs: u64,
    /// The presentation clock.
    pub clock: &'a Clock,
}

/// Maps output `n` of side A to output `n + offset` of side B.
#[must_use]
pub fn align_by_offset(a: u64, b: u64, offset: i64) -> Mapping {
    let (skip_a, skip_b) = if offset < 0 {
        (offset.unsigned_abs().min(a), 0)
    } else {
        (0, offset.unsigned_abs().min(b))
    };
    let count = (a - skip_a).min(b - skip_b);
    let segments = [
        Segment::OnlyA {
            first: 0,
            count: skip_a,
        },
        Segment::OnlyB {
            first: 0,
            count: skip_b,
        },
        Segment::Matched {
            a: skip_a,
            b: skip_b,
            count,
        },
        Segment::OnlyA {
            first: skip_a + count,
            count: a - skip_a - count,
        },
        Segment::OnlyB {
            first: skip_b + count,
            count: b - skip_b - count,
        },
    ];
    Mapping {
        segments: segments
            .into_iter()
            .filter(|segment| {
                !matches!(
                    segment,
                    Segment::Matched { count: 0, .. }
                        | Segment::OnlyA { count: 0, .. }
                        | Segment::OnlyB { count: 0, .. }
                )
            })
            .collect(),
    }
}

/// One run with its times in a unit both sides share.
#[derive(Debug, Clone, Copy)]
struct Stretch {
    first: u64,
    count: u64,
    start: u128,
    step: u128,
}

impl Stretch {
    fn time(&self, within: u64) -> u128 {
        // The last tick of every stretch fits 64 bits and the scale does too.
        self.start + self.step * u128::from(within)
    }
}

/// The next output of one side during a merge.
#[derive(Debug, Clone, Copy)]
struct Next {
    ordinal: u64,
    time: u128,
    step: u128,
    /// Outputs left in the stretch, this one included.
    left: u64,
}

/// A position in the stretches of one side.
#[derive(Debug)]
struct Cursor {
    stretches: Vec<Stretch>,
    at: usize,
    within: u64,
}

impl Cursor {
    const fn new(stretches: Vec<Stretch>) -> Self {
        Self {
            stretches,
            at: 0,
            within: 0,
        }
    }

    fn next(&self) -> Option<Next> {
        let stretch = self.stretches.get(self.at)?;
        Some(Next {
            ordinal: stretch.first + self.within,
            time: stretch.time(self.within),
            step: stretch.step,
            left: stretch.count - self.within,
        })
    }

    /// Moves past `count` outputs of the current stretch.
    fn take(&mut self, count: u64) {
        self.within += count;
        if self
            .stretches
            .get(self.at)
            .is_some_and(|stretch| stretch.count == self.within)
        {
            self.at += 1;
            self.within = 0;
        }
    }
}

/// The runs of `track` with times multiplied by `scale`, or the fault that
/// makes them unusable.
fn stretches(
    outputs: u64,
    runs: &[ClockRun],
    side: Side,
    scale: u64,
) -> Result<Vec<Stretch>, AlignmentRefusal> {
    let invalid = AlignmentRefusal::UnusableClock(side, ClockFault::Invalid);
    if outputs == 0 {
        return Ok(Vec::new());
    }
    if runs.first().is_none_or(|run| run.first != 0) || runs.len() > MAX_CLOCK_RUNS {
        return Err(invalid);
    }
    let mut out = Vec::with_capacity(runs.len());
    let mut after = None;
    for (at, run) in runs.iter().enumerate() {
        let end = runs.get(at + 1).map_or(outputs, |next| next.first);
        let count = end
            .checked_sub(run.first)
            .filter(|n| *n > 0)
            .ok_or(invalid)?;
        if count > 1 && run.step == 0 {
            return Err(invalid);
        }
        // Every timestamp is a real tick count, so the scaled times fit.
        let last = run
            .step
            .checked_mul(count - 1)
            .and_then(|span| run.start.checked_add(span))
            .ok_or(invalid)?;
        if after.is_some_and(|after| run.start <= after) {
            return Err(invalid);
        }
        after = Some(last);
        out.push(Stretch {
            first: run.first,
            count,
            start: u128::from(run.start) * u128::from(scale),
            step: u128::from(run.step) * u128::from(scale),
        });
    }
    Ok(out)
}

/// The time base and runs of a usable clock.
fn exact(track: Track<'_>, side: Side) -> Result<(Rational, &[ClockRun]), AlignmentRefusal> {
    match track.clock {
        Clock::Exact { tick, runs } if tick.num > 0 && tick.den > 0 => Ok((*tick, runs)),
        Clock::Exact { .. } => Err(AlignmentRefusal::UnusableClock(side, ClockFault::Invalid)),
        Clock::Absent => Err(AlignmentRefusal::NoTimestamps(side)),
        Clock::Unusable { fault } => Err(AlignmentRefusal::UnusableClock(side, *fault)),
    }
}

/// Matches outputs with equal exact timestamps.
///
/// # Errors
/// When a side has no usable timestamps, no timestamp is common, or the
/// result needs too many segments.
pub fn align_by_timestamps(a: Track<'_>, b: Track<'_>) -> Result<Mapping, AlignmentRefusal> {
    let ((tick_a, runs_a), (tick_b, runs_b)) = (exact(a, Side::A)?, exact(b, Side::B)?);
    // One shared unit: 1 / (den_a * den_b) seconds.
    let scale_a = u64::from(tick_a.num) * u64::from(tick_b.den);
    let scale_b = u64::from(tick_b.num) * u64::from(tick_a.den);
    let mut cursor_a = Cursor::new(stretches(a.outputs, runs_a, Side::A, scale_a)?);
    let mut cursor_b = Cursor::new(stretches(b.outputs, runs_b, Side::B, scale_b)?);

    let mut mapping = Mapping::default();
    loop {
        let (taken_a, taken_b) = match (cursor_a.next(), cursor_b.next()) {
            (None, None) => break,
            (Some(rest), None) => {
                mapping.push(Segment::OnlyA {
                    first: rest.ordinal,
                    count: rest.left,
                })?;
                (rest.left, 0)
            }
            (None, Some(rest)) => {
                mapping.push(Segment::OnlyB {
                    first: rest.ordinal,
                    count: rest.left,
                })?;
                (0, rest.left)
            }
            (Some(left), Some(right)) => match left.time.cmp(&right.time) {
                Ordering::Equal => {
                    // Equal steps keep the two stretches together to their end.
                    let count = if left.step == right.step {
                        left.left.min(right.left)
                    } else {
                        1
                    };
                    mapping.push(Segment::Matched {
                        a: left.ordinal,
                        b: right.ordinal,
                        count,
                    })?;
                    (count, count)
                }
                Ordering::Less => {
                    let count = before(right.time - left.time, left.step, left.left);
                    mapping.push(Segment::OnlyA {
                        first: left.ordinal,
                        count,
                    })?;
                    (count, 0)
                }
                Ordering::Greater => {
                    let count = before(left.time - right.time, right.step, right.left);
                    mapping.push(Segment::OnlyB {
                        first: right.ordinal,
                        count,
                    })?;
                    (0, count)
                }
            },
        };
        cursor_a.take(taken_a);
        cursor_b.take(taken_b);
    }
    if mapping.matched() == 0 && a.outputs > 0 && b.outputs > 0 {
        return Err(AlignmentRefusal::Disjoint);
    }
    Ok(mapping)
}

/// How many of `left` outputs, `step` apart, come before a time `gap` ahead.
fn before(gap: u128, step: u128, left: u64) -> u64 {
    if step == 0 {
        return 1;
    }
    u64::try_from(gap.div_ceil(step)).map_or(left, |count| count.min(left))
}
