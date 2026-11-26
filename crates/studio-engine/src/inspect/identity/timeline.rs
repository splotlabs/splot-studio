// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Byte ownership, reverse links and range summaries for the frame timeline.

use std::io;

use studio_domain::analysis::IvfHeaderFact;
use studio_domain::timeline::{
    ByteDistribution, MAX_TIMELINE_BUCKETS, TimelineBucket, TimelineClock, TimelineEntry,
    TimelineFileBytes, TimelinePage, TimelineQuantizer, TimelineSelection, TimelineTarget,
    TimelineTime, TimelineView,
};

use super::{
    IdentityCompletion, IdentityIndex, Refusal, State, Table, invalid, source_changed, storage,
};

const MAX_TIMELINE_BYTES: u64 = 128 << 20;
const BLOCK_ROWS: u64 = 32;
const IVF_PTS: u64 = 1;
const RANDOM_ACCESS: u64 = 2;
const DISPLAY_COUNTED: u64 = 4;
const CODED_COUNTED: u64 = 8;
const AMBIGUOUS_PTS: u64 = 16;
const CODED_ORDINAL_SHIFT: u32 = 8;

#[derive(Debug)]
pub(super) struct TimelineIndex {
    // Header offset, OBU bytes, packet PTS, flags and coded ordinal + 1.
    obus: Table,
    // Coded header offset, frame OBU bytes, first output ordinal + 1, random-access flag.
    coded: Track<4>,
    // Coded ordinal, display OBU bytes, packet PTS, interval numerator and denominator, PTS flag.
    output: Track<6>,
    obu_bytes: u64,
    assigned_obu_bytes: u64,
    through: u64,
    last_pts: Option<u64>,
    decreasing: bool,
    ambiguous: bool,
}

impl TimelineIndex {
    pub(super) fn new() -> io::Result<Self> {
        Ok(Self {
            obus: Table::new()?,
            coded: Track::new()?,
            output: Track::new()?,
            obu_bytes: 0,
            assigned_obu_bytes: 0,
            through: 0,
            last_pts: None,
            decreasing: false,
            ambiguous: false,
        })
    }

    pub(super) fn start_unit(&mut self, offset: u64) {
        self.through = self.through.max(offset);
    }

    pub(super) fn obu(
        &mut self,
        offset: u64,
        bytes: u64,
        pts: Option<u64>,
        ambiguous: bool,
        random_access: bool,
    ) -> io::Result<()> {
        let flags = (if pts.is_some() { IVF_PTS } else { 0 })
            | (if random_access { RANDOM_ACCESS } else { 0 })
            | (if ambiguous { AMBIGUOUS_PTS } else { 0 });
        self.budget(Table::<4>::BYTES)?;
        self.obus.append([offset, bytes, pts.unwrap_or(0), flags])?;
        self.obu_bytes += bytes;
        self.through = offset + bytes;
        Ok(())
    }

    pub(super) fn frame(&mut self, offset: u64) -> io::Result<()> {
        self.budget(Table::<4>::BYTES)?;
        let ordinal = self.coded.rows.count;
        let flags = CODED_COUNTED | ((ordinal + 1) << CODED_ORDINAL_SHIFT);
        let obu = self.claim(offset, flags)?;
        if obu[3] >> CODED_ORDINAL_SHIFT != ordinal + 1 {
            return Err(io::Error::other("The decoder reported a coded OBU twice."));
        }
        self.coded
            .append([offset, obu[1], 0, obu[3] & RANDOM_ACCESS])
    }

    pub(super) fn continuation(&mut self, coded: u64, offset: u64) -> io::Result<()> {
        let obu = self.claim(offset, CODED_COUNTED)?;
        let ordinal = self.coded_ordinal(coded)?;
        let mut record = self.coded.rows.at(ordinal)?;
        record[1] += obu[1];
        self.coded.update(ordinal, record)
    }

    pub(super) fn output(
        &mut self,
        coded: u64,
        showing: u64,
        interval: Option<(u64, u64)>,
    ) -> io::Result<()> {
        self.budget(Table::<6>::BYTES)?;
        let obu = self.claim(showing, DISPLAY_COUNTED)?;
        let ordinal = self.coded_ordinal(coded)?;
        let mut coded = self.coded.rows.at(ordinal)?;
        if coded[2] == 0 {
            coded[2] = self.output.rows.count + 1;
            self.coded.rows.write(ordinal, coded)?;
        }
        let (num, den) = interval.unwrap_or_default();
        self.output
            .append([ordinal, obu[1], obu[2], num, den, obu[3] & IVF_PTS])?;
        if obu[3] & IVF_PTS != 0 {
            self.decreasing |= self.last_pts.is_some_and(|pts| obu[2] < pts);
            self.last_pts = Some(obu[2]);
        }
        self.ambiguous |= obu[3] & AMBIGUOUS_PTS != 0;
        Ok(())
    }

    fn find_obu(&mut self, offset: u64) -> io::Result<(u64, [u64; 4])> {
        let row = self.obus.lower_bound(|obu| obu[0] < offset)?;
        if row == self.obus.count {
            return Err(missing());
        }
        let obu = self.obus.at(row)?;
        if obu[0] != offset {
            return Err(missing());
        }
        Ok((row, obu))
    }

    fn claim(&mut self, offset: u64, flags: u64) -> io::Result<[u64; 4]> {
        let (row, mut obu) = self.find_obu(offset)?;
        if obu[3] & flags & (DISPLAY_COUNTED | CODED_COUNTED) != 0 {
            obu[1] = 0;
        } else {
            let unassigned = obu[3] & (DISPLAY_COUNTED | CODED_COUNTED) == 0;
            obu[3] |= flags;
            self.obus.write(row, obu)?;
            if unassigned {
                self.assigned_obu_bytes += obu[1];
            }
        }
        Ok(obu)
    }

    fn coded_ordinal(&mut self, offset: u64) -> io::Result<u64> {
        let (_, obu) = self.find_obu(offset)?;
        (obu[3] >> CODED_ORDINAL_SHIFT)
            .checked_sub(1)
            .filter(|&row| row < self.coded.rows.count)
            .ok_or_else(missing)
    }

    fn budget(&self, add: usize) -> io::Result<()> {
        let bytes = (self.obus.count + self.coded.rows.count) * Table::<4>::BYTES as u64
            + self.output.rows.count * Table::<6>::BYTES as u64;
        if bytes + add as u64 > MAX_TIMELINE_BYTES {
            return Err(io::Error::other(
                "The frame timeline reached its disk limit.",
            ));
        }
        Ok(())
    }

    fn bytes(
        &mut self,
        view: TimelineView,
        first: u64,
        end: u64,
    ) -> io::Result<Option<ByteDistribution>> {
        match view {
            TimelineView::Coded => self.coded.bytes(first, end),
            TimelineView::Output => self.output.bytes(first, end),
        }
    }

    fn count(&self, view: TimelineView) -> u64 {
        match view {
            TimelineView::Coded => self.coded.rows.count,
            TimelineView::Output => self.output.rows.count,
        }
    }

    fn clock(&self, clock: Option<IvfClock>) -> TimelineClock {
        if self.ambiguous {
            TimelineClock::Conflicting
        } else if self.decreasing {
            TimelineClock::Discontinuous
        } else if clock.is_some() && self.last_pts.is_some() {
            TimelineClock::Ivf
        } else {
            TimelineClock::Unavailable
        }
    }
}

#[derive(Debug)]
struct Track<const WORDS: usize> {
    rows: Table<WORDS>,
    // One summary per group bounds page work independently of its zoom level.
    blocks: Vec<ByteDistribution>,
}

impl<const WORDS: usize> Track<WORDS> {
    fn new() -> io::Result<Self> {
        Ok(Self {
            rows: Table::new()?,
            blocks: Vec::new(),
        })
    }

    fn append(&mut self, row: [u64; WORDS]) -> io::Result<()> {
        let start_block = self.rows.count.is_multiple_of(BLOCK_ROWS);
        self.rows.append(row)?;
        let one = one(row[1]);
        if let Some(block) = self.blocks.last_mut().filter(|_| !start_block) {
            *block = combine(*block, one);
        } else {
            self.blocks.push(one);
        }
        Ok(())
    }

    fn update(&mut self, row: u64, record: [u64; WORDS]) -> io::Result<()> {
        let block = row / BLOCK_ROWS;
        let first = block * BLOCK_ROWS;
        let end = self.rows.count.min(first + BLOCK_ROWS);
        let mut summary = one(record[1]);
        self.rows.read_window(first, end, |at, value| {
            if at != row {
                summary = combine(summary, one(value[1]));
            }
        })?;
        let index = usize::try_from(block).map_err(io::Error::other)?;
        self.rows.write(row, record)?;
        self.blocks[index] = summary;
        Ok(())
    }

    fn scan(&mut self, first: u64, end: u64) -> io::Result<Option<ByteDistribution>> {
        let mut summary = None;
        self.rows
            .read_window(first, end, |_, row| merge(&mut summary, one(row[1])))?;
        Ok(summary)
    }

    fn bytes(&mut self, first: u64, end: u64) -> io::Result<Option<ByteDistribution>> {
        let block_start = first.div_ceil(BLOCK_ROWS);
        let block_end = end / BLOCK_ROWS;
        if block_start >= block_end {
            return self.scan(first, end);
        }
        let mut summary = self.scan(first, block_start * BLOCK_ROWS)?;
        let start = usize::try_from(block_start).map_err(io::Error::other)?;
        let end_block = usize::try_from(block_end).map_err(io::Error::other)?;
        for &block in &self.blocks[start..end_block] {
            merge(&mut summary, block);
        }
        if let Some(tail) = self.scan(block_end * BLOCK_ROWS, end)? {
            merge(&mut summary, tail);
        }
        Ok(summary)
    }
}

fn one(bytes: u64) -> ByteDistribution {
    ByteDistribution {
        min: bytes,
        max: bytes,
        sum: bytes,
    }
}

fn combine(a: ByteDistribution, b: ByteDistribution) -> ByteDistribution {
    ByteDistribution {
        min: a.min.min(b.min),
        max: a.max.max(b.max),
        sum: a.sum + b.sum,
    }
}

fn merge(summary: &mut Option<ByteDistribution>, next: ByteDistribution) {
    *summary = Some(summary.map_or(next, |previous| combine(previous, next)));
}

fn missing() -> io::Error {
    io::Error::other("A decoder identity has no matching source record.")
}

#[derive(Clone, Copy)]
struct IvfClock {
    num: u64,
    den: u64,
}

impl IvfClock {
    fn new(header: Option<IvfHeaderFact>) -> Option<Self> {
        let header = header?;
        let num = u64::from(header.timebase_numerator);
        let den = u64::from(header.timebase_denominator);
        (num != 0 && den != 0).then_some(Self { num, den })
    }

    fn time(self, ticks: u64) -> TimelineTime {
        TimelineTime {
            num: u128::from(ticks) * u128::from(self.num),
            den: self.den,
        }
    }

    // ceil(target / timebase) without converting any clock to floating point.
    fn target_tick(self, target: TimelineTime) -> Option<u64> {
        let den = u128::from(target.den) * u128::from(self.num);
        if den == 0 {
            return None;
        }
        let whole = target.num / den;
        let remainder = target.num % den;
        let scale = u128::from(self.den);
        let ticks = whole
            .checked_mul(scale)?
            .checked_add(remainder.checked_mul(scale)?.div_ceil(den))?;
        u64::try_from(ticks).ok()
    }

    fn rate(self, bytes: u64, ticks: u64) -> Option<TimelineTime> {
        let den = ticks.checked_mul(self.num)?;
        let num = u128::from(bytes) * 8 * u128::from(self.den);
        (den != 0).then_some(TimelineTime { num, den })
    }
}

impl State {
    fn complete(&self) -> bool {
        self.completion == IdentityCompletion::Complete
    }

    fn entry(
        &mut self,
        view: TimelineView,
        ordinal: u64,
        clock: Option<IvfClock>,
    ) -> io::Result<TimelineEntry> {
        let is_output = view == TimelineView::Output;
        let coded_ordinal = if is_output {
            self.timeline.output.rows.at(ordinal)?[0]
        } else {
            ordinal
        };
        let [coded_offset, coded_bytes, first_output, access] =
            self.timeline.coded.rows.at(coded_ordinal)?;
        let [_, sequence_offset, ..] = self.coded.at(coded_ordinal)?;
        let output_ordinal = if is_output {
            Some(ordinal)
        } else {
            first_output
                .checked_sub(1)
                .filter(|&index| index < self.timeline.output.rows.count)
        };
        let mut entry = TimelineEntry {
            ordinal,
            coded_offset,
            showing_offset: None,
            sequence_offset,
            coded_ordinal,
            output_ordinal,
            bytes: coded_bytes,
            time: None,
            interval: None,
            random_access: access != 0,
            quantizer: TimelineQuantizer::Unavailable,
        };
        if let Some(output) = output_ordinal {
            let [_, showing, ..] = self.output.at(output)?;
            let [_, bytes, pts, num, den, flags] = self.timeline.output.rows.at(output)?;
            entry.showing_offset = Some(showing);
            entry.interval = (den != 0).then_some(TimelineTime {
                num: u128::from(num),
                den,
            });
            entry.time = clock
                .filter(|_| flags & IVF_PTS != 0)
                .map(|clock| clock.time(pts));
            if is_output {
                entry.bytes = bytes;
            }
        }
        Ok(entry)
    }

    fn rate(
        &mut self,
        clock: IvfClock,
        first: u64,
        end: u64,
        bytes: u64,
    ) -> io::Result<Option<TimelineTime>> {
        if end >= self.timeline.output.rows.count {
            return Ok(None);
        }
        let start = self.timeline.output.rows.at(first)?[2];
        let finish = self.timeline.output.rows.at(end)?[2];
        // An equal timestamp means some simultaneous pictures lie outside the window.
        if (first > 0 && self.timeline.output.rows.at(first - 1)?[2] == start)
            || self.timeline.output.rows.at(end - 1)?[2] == finish
        {
            return Ok(None);
        }
        Ok(finish
            .checked_sub(start)
            .and_then(|ticks| clock.rate(bytes, ticks)))
    }
}

impl IdentityIndex {
    pub(in crate::inspect) fn timeline(
        &self,
        header: Option<IvfHeaderFact>,
        view: TimelineView,
        first: u64,
        limit: u64,
        buckets: u32,
    ) -> Result<TimelinePage, Refusal> {
        if limit == 0 || !(1..=MAX_TIMELINE_BUCKETS).contains(&buckets) {
            return Err(invalid(
                "The timeline range or bucket count is out of range.",
            ));
        }
        let mut state = self.state();
        if state.changed {
            return Err(source_changed());
        }
        let total = state.timeline.count(view);
        if first > total {
            return Err(invalid("The first timeline row is past the end."));
        }
        let clock = IvfClock::new(header);
        let clock_state = state.timeline.clock(clock);
        let end = first.saturating_add(limit).min(total);
        let count = end - first;
        let width = count.div_ceil(u64::from(buckets)).max(1);
        let mut groups = Vec::new();
        let mut bytes = None;
        let mut start = first;
        while start < end {
            let stop = start.saturating_add(width).min(end);
            let distribution = state
                .timeline
                .bytes(view, start, stop)
                .map_err(|e| storage(&e))?;
            if let Some(distribution) = distribution {
                merge(&mut bytes, distribution);
            }
            let entry = (stop - start == 1)
                .then(|| state.entry(view, start, clock))
                .transpose()
                .map_err(|e| storage(&e))?;
            groups.push(TimelineBucket {
                first: start,
                count: stop - start,
                bytes: distribution,
                entry,
            });
            start = stop;
        }
        let rate = match (clock, bytes) {
            (Some(clock), Some(bytes))
                if view == TimelineView::Output
                    && clock_state == TimelineClock::Ivf
                    && state.complete() =>
            {
                state
                    .rate(clock, first, end, bytes.sum)
                    .map_err(|e| storage(&e))?
            }
            _ => None,
        };
        let file = state.fingerprint.len;
        let classified = state.timeline.through;
        let obu = state.timeline.obu_bytes;
        Ok(TimelinePage {
            view,
            first,
            total,
            complete: state.complete(),
            verified: state.matches_structure,
            clock: clock_state,
            file_bytes: TimelineFileBytes {
                file,
                obu,
                assigned_obu: state.timeline.assigned_obu_bytes,
                unassigned_obu: obu - state.timeline.assigned_obu_bytes,
                framing: classified - obu,
                unclassified: file - classified,
            },
            bytes,
            rate,
            buckets: groups,
        })
    }

    pub(in crate::inspect) fn timeline_locate(
        &self,
        header: Option<IvfHeaderFact>,
        target: TimelineTarget,
    ) -> Result<TimelineSelection, Refusal> {
        let mut state = self.state();
        if state.changed {
            return Err(source_changed());
        }
        let clock = IvfClock::new(header);
        let (view, index) = match target {
            TimelineTarget::Output { index } => (TimelineView::Output, index),
            TimelineTarget::Coded { index } => (TimelineView::Coded, index),
            TimelineTarget::Time { time } => {
                let clock = clock
                    .filter(|_| {
                        state.complete() && state.timeline.clock(clock) == TimelineClock::Ivf
                    })
                    .ok_or_else(|| {
                        invalid("Time search needs a complete, unambiguous IVF clock.")
                    })?;
                let ticks = clock
                    .target_tick(time)
                    .ok_or_else(|| invalid("The requested time is outside the IVF clock."))?;
                let index = state
                    .timeline
                    .output
                    .rows
                    .lower_bound(|row| row[2] < ticks)
                    .map_err(|e| storage(&e))?;
                (TimelineView::Output, index)
            }
        };
        if index >= state.timeline.count(view) {
            return Err(invalid("The requested timeline row is unavailable."));
        }
        let entry = state.entry(view, index, clock).map_err(|e| storage(&e))?;
        Ok(TimelineSelection {
            coded_ordinal: entry.coded_ordinal,
            coded_offset: entry.coded_offset,
            output_ordinal: entry.output_ordinal,
            showing_offset: entry.showing_offset,
            time: entry.time,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_keep_exact_bucket_edges_and_continuation_sizes() -> io::Result<()> {
        let mut track = Track::<4>::new()?;
        for row in 0..143 {
            track.append([row, row + 1, 0, 0])?;
        }
        track.update(35, [35, 200, 0, 0])?;
        assert_eq!(
            track.bytes(11, 103)?,
            Some(ByteDistribution {
                min: 12,
                max: 200,
                sum: (12..=103).sum::<u64>() + 200 - 36,
            })
        );
        assert_eq!(track.bytes(36, 37)?, Some(one(37)));
        Ok(())
    }

    #[test]
    fn reverse_joins_keep_order_even_when_coded_offsets_do_not_increase() -> io::Result<()> {
        let mut timeline = TimelineIndex::new()?;
        timeline.obu(12, 3, Some(0), false, false)?;
        timeline.obu(24, 6, Some(1), false, false)?;
        timeline.frame(24)?;
        timeline.frame(12)?;
        timeline.output(12, 24, None)?;
        timeline.output(24, 24, None)?;
        assert_eq!(timeline.output.rows.at(0)?[0], 1);
        assert_eq!(timeline.output.rows.at(1)?[0], 0);
        assert_eq!(timeline.output.rows.at(1)?[1], 0);
        assert_eq!(timeline.assigned_obu_bytes, 9);
        Ok(())
    }

    #[test]
    fn rational_clock_handles_overflow_and_nonintegral_input() {
        let clock = IvfClock {
            num: 1001,
            den: 30_000,
        };
        assert_eq!(clock.target_tick(TimelineTime { num: 1, den: 30 }), Some(1));
        assert_eq!(clock.target_tick(TimelineTime { num: 2, den: 30 }), Some(2));
        assert_eq!(
            clock.target_tick(TimelineTime {
                num: u128::MAX,
                den: 1
            }),
            None
        );
        assert_eq!(clock.target_tick(TimelineTime { num: 1, den: 0 }), None);
        assert_eq!(
            clock.rate(10, 2),
            Some(TimelineTime {
                num: 2_400_000,
                den: 2002
            })
        );
        assert_eq!(clock.rate(10, 0), None);
        assert_eq!(clock.time(u64::MAX).num, u128::from(u64::MAX) * 1001);
    }
}
