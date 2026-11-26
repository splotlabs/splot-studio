// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Exact timing evidence for the selected decoder outputs.

use serde::Serialize;
use splot_core::headers::metadata::MetadataPayload;
use studio_domain::job::Rational;

use super::input::Output;

#[derive(Clone, Copy, Serialize)]
pub(super) struct IvfTime {
    pts: u64,
    time_base: Rational,
}

#[derive(Clone, Copy, Serialize)]
pub(super) struct Av2Time {
    num_units_in_display_tick: u32,
    time_scale: u32,
    equal_picture_interval: bool,
    num_ticks_per_picture_minus_1: Option<u32>,
}

impl Av2Time {
    fn interval(self) -> Option<Fraction> {
        if !self.equal_picture_interval {
            return None;
        }
        let ticks = u128::from(self.num_ticks_per_picture_minus_1?) + 1;
        Fraction::new(
            u128::from(self.num_units_in_display_tick) * ticks,
            u128::from(self.time_scale),
        )
    }
}

#[derive(Clone, Copy, Serialize)]
pub(super) struct Evidence {
    ivf: Option<IvfTime>,
    av2: Option<Av2Time>,
    frame_presentation_time: Option<u32>,
}

impl Evidence {
    pub fn of(output: &Output) -> Self {
        let ivf = output
            .unit
            .pts
            .zip(output.ivf_base)
            .map(|(pts, time_base)| IvfTime { pts, time_base });
        let av2 = output
            .picture
            .content_interpretation()
            .and_then(|content| content.timing_info)
            .map(|t| Av2Time {
                num_units_in_display_tick: t.num_units_in_display_tick,
                time_scale: t.time_scale,
                equal_picture_interval: t.equal_picture_interval,
                num_ticks_per_picture_minus_1: t.num_ticks_per_picture_minus_1,
            });
        let frame_presentation_time = output.picture.metadata().iter().find_map(|unit| {
            if let MetadataPayload::TemporalPointInfo(timing) = &unit.payload {
                Some(timing.frame_presentation_time)
            } else {
                None
            }
        });
        Self {
            ivf,
            av2,
            frame_presentation_time,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fraction {
    num: u128,
    den: u128,
}

impl Fraction {
    const ZERO: Self = Self { num: 0, den: 1 };

    fn new(num: u128, den: u128) -> Option<Self> {
        if den == 0 {
            return None;
        }
        let factor = gcd(num, den);
        Some(Self {
            num: num / factor,
            den: den / factor,
        })
    }

    fn add(self, other: Self) -> Option<Self> {
        let common = gcd(self.den, other.den);
        let a = self.num.checked_mul(other.den / common)?;
        let b = other.num.checked_mul(self.den / common)?;
        Self::new(a.checked_add(b)?, self.den.checked_mul(other.den / common)?)
    }

    fn frame_rate(self) -> Option<Rational> {
        if self.num == 0 {
            return None;
        }
        Some(Rational {
            num: u32::try_from(self.den).ok()?,
            den: u32::try_from(self.num).ok()?,
        })
    }
}

fn gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

#[derive(Clone, Copy)]
enum Gap {
    Known(Fraction),
    Unknown,
    Variable,
    Unrepresentable,
}

impl Gap {
    fn add(self, rhs: Self) -> Self {
        match (self, rhs) {
            (Self::Variable, _) | (_, Self::Variable) => Self::Variable,
            (Self::Unrepresentable, _) | (_, Self::Unrepresentable) => Self::Unrepresentable,
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            (Self::Known(a), Self::Known(b)) => a.add(b).map_or(Self::Unrepresentable, Self::Known),
        }
    }
}

fn ivf_gap(start: Option<IvfTime>, end: Option<IvfTime>) -> Gap {
    let Some((start, end)) = start.zip(end) else {
        return Gap::Unknown;
    };
    if end.time_base.num == 0 || end.time_base.den == 0 {
        return Gap::Unknown;
    }
    let Some(ticks) = end.pts.checked_sub(start.pts) else {
        return Gap::Variable;
    };
    let num = u128::from(ticks) * u128::from(end.time_base.num);
    Fraction::new(num, u128::from(end.time_base.den)).map_or(Gap::Unknown, Gap::Known)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum Status {
    Constant { frame_rate: Rational },
    Assumed { frame_rate: Rational },
    Unknown,
    Variable,
    Contradictory,
    Unrepresentable,
}

impl Status {
    pub fn y4m_rate(self) -> Result<Rational, &'static str> {
        match self {
            Self::Constant { frame_rate } | Self::Assumed { frame_rate } => Ok(frame_rate),
            Self::Unknown => Err(
                "The selected outputs have unknown timing. Enter a frame rate or choose raw samples.",
            ),
            Self::Variable => {
                Err("The selected outputs have variable or repeated timing. Choose raw samples.")
            }
            Self::Contradictory => {
                Err("IVF timestamps and AV2 timing disagree. Choose raw samples.")
            }
            Self::Unrepresentable => {
                Err("Y4M cannot represent the timing of the selected outputs. Choose raw samples.")
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Issue {
    Unrepresentable,
    Variable,
    Contradictory,
}

/// Tracks timing over all decoded outputs. A layer filter still accounts for
/// intervening output temporal units when projecting AV2 cadence.
pub(super) struct Timing {
    previous_output: Option<(u64, Option<Fraction>)>,
    previous_selected: Option<Evidence>,
    av2_gap: Gap,
    period: Option<Fraction>,
    unknown: bool,
    variable_evidence: bool,
    pending_variable_evidence: bool,
    issue: Option<Issue>,
    count: u64,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            previous_output: None,
            previous_selected: None,
            av2_gap: Gap::Known(Fraction::ZERO),
            period: None,
            unknown: false,
            variable_evidence: false,
            pending_variable_evidence: false,
            issue: None,
            count: 0,
        }
    }
}

impl Timing {
    pub fn observe(&mut self, offset: u64, timing: Evidence, selected: bool) {
        let interval = timing.av2.and_then(Av2Time::interval);
        let variable_point = interval.is_none() && timing.frame_presentation_time.is_some();
        if self.previous_selected.is_some() {
            self.pending_variable_evidence |= variable_point;
        }
        if let Some((previous_unit, previous_interval)) =
            self.previous_output.replace((offset, interval))
            && self.previous_selected.is_some()
        {
            let step = match (previous_interval, interval) {
                (_, _) if previous_unit == offset => Gap::Known(Fraction::ZERO),
                (Some(a), Some(b)) if a == b => Gap::Known(b),
                (Some(_), Some(_)) => Gap::Variable,
                _ => Gap::Unknown,
            };
            self.av2_gap = self.av2_gap.add(step);
        }
        if !selected {
            return;
        }
        self.count += 1;
        let intervening_variable = std::mem::take(&mut self.pending_variable_evidence);
        self.variable_evidence |= variable_point || intervening_variable;
        if let Some(previous) = self.previous_selected.replace(timing) {
            let container = ivf_gap(previous.ivf, timing.ivf);
            let bitstream = std::mem::replace(&mut self.av2_gap, Gap::Known(Fraction::ZERO));
            let gap = match (container, bitstream) {
                (Gap::Known(a), Gap::Known(b)) if a != b => {
                    self.record_issue(Issue::Contradictory);
                    return;
                }
                (Gap::Variable, _) | (_, Gap::Variable) => Gap::Variable,
                (Gap::Known(gap), _) | (_, Gap::Known(gap)) => Gap::Known(gap),
                (Gap::Unrepresentable, _) | (_, Gap::Unrepresentable) => Gap::Unrepresentable,
                _ => Gap::Unknown,
            };
            match gap {
                Gap::Known(gap) if gap.num == 0 => self.record_issue(Issue::Variable),
                Gap::Known(gap) => {
                    if self.period.replace(gap).is_some_and(|old| old != gap) {
                        self.record_issue(Issue::Variable);
                    }
                }
                Gap::Unknown => self.unknown = true,
                Gap::Variable => self.record_issue(Issue::Variable),
                Gap::Unrepresentable => self.record_issue(Issue::Unrepresentable),
            }
        }
    }

    fn record_issue(&mut self, issue: Issue) {
        self.issue = Some(self.issue.map_or(issue, |old| old.max(issue)));
    }

    pub fn status(&self, fallback: Option<Rational>) -> Status {
        if let Some(issue) = self.issue {
            return match issue {
                Issue::Contradictory => Status::Contradictory,
                Issue::Variable => Status::Variable,
                Issue::Unrepresentable => Status::Unrepresentable,
            };
        }
        let known = if self.count == 1 {
            self.previous_selected
                .and_then(|t| t.av2)
                .and_then(Av2Time::interval)
        } else {
            self.period
        };
        let mut frame_rate = None;
        if let Some(interval) = known {
            let Some(rate) = interval.frame_rate() else {
                return Status::Unrepresentable;
            };
            frame_rate = Some(rate);
            if !self.unknown {
                return Status::Constant { frame_rate: rate };
            }
        }
        if self.variable_evidence {
            return Status::Variable;
        }
        if let Some(fallback) = fallback {
            let normalized = Fraction::new(u128::from(fallback.den), u128::from(fallback.num))
                .and_then(Fraction::frame_rate);
            if normalized.is_some_and(|rate| frame_rate.is_some_and(|known| known != rate)) {
                return Status::Variable;
            }
            return Status::Assumed {
                frame_rate: fallback,
            };
        }
        Status::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: Rational = Rational { num: 24, den: 1 };
    const TICK: Rational = Rational { num: 1, den: 24 };
    const UNTYPED: Evidence = Evidence {
        ivf: None,
        av2: None,
        frame_presentation_time: None,
    };

    fn ivf(pts: u64) -> Evidence {
        Evidence {
            ivf: Some(IvfTime {
                pts,
                time_base: TICK,
            }),
            ..UNTYPED
        }
    }

    #[test]
    fn a_single_ivf_timestamp_does_not_declare_a_rate() {
        let mut timing = Timing::default();
        timing.observe(10, ivf(0), true);
        assert_eq!(timing.status(None), Status::Unknown);
        assert_eq!(
            timing.status(Some(RATE)),
            Status::Assumed { frame_rate: RATE }
        );
        timing.observe(20, ivf(1), true);
        assert_eq!(timing.status(None), Status::Constant { frame_rate: RATE });
        timing.observe(30, ivf(3), true);
        assert_eq!(timing.status(Some(RATE)), Status::Variable);
    }

    #[test]
    fn a_same_unit_output_gap_cannot_use_an_assumed_rate() {
        let mut all = Timing::default();
        let mut one_layer = Timing::default();
        for (offset, selected) in [(10, true), (10, false), (20, true), (20, false)] {
            all.observe(offset, UNTYPED, true);
            one_layer.observe(offset, UNTYPED, selected);
        }
        assert_eq!(all.status(Some(RATE)), Status::Variable);
        assert_eq!(
            one_layer.status(Some(RATE)),
            Status::Assumed { frame_rate: RATE }
        );
    }

    #[test]
    fn ivf_and_av2_cadence_must_agree() {
        let av2 = Some(Av2Time {
            num_units_in_display_tick: 1,
            time_scale: 24,
            equal_picture_interval: true,
            num_ticks_per_picture_minus_1: Some(0),
        });
        let mut timing = Timing::default();
        timing.observe(10, Evidence { av2, ..ivf(0) }, true);
        timing.observe(20, Evidence { av2, ..ivf(2) }, true);
        assert_eq!(timing.status(Some(RATE)), Status::Contradictory);
    }

    #[test]
    fn temporal_point_evidence_applies_only_to_the_selected_span() {
        let variable = Evidence {
            frame_presentation_time: Some(2),
            ..UNTYPED
        };
        let mut timing = Timing::default();
        timing.observe(10, UNTYPED, true);
        timing.observe(20, variable, false);
        assert_eq!(
            timing.status(Some(RATE)),
            Status::Assumed { frame_rate: RATE }
        );
        timing.observe(30, UNTYPED, true);
        assert_eq!(timing.status(Some(RATE)), Status::Variable);

        let mut timed = Timing::default();
        timed.observe(10, ivf(0), true);
        timed.observe(20, variable, false);
        timed.observe(30, ivf(1), true);
        assert_eq!(
            timed.status(Some(RATE)),
            Status::Constant { frame_rate: RATE }
        );
    }
}
