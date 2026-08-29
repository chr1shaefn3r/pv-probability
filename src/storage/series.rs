//! Turning power readings into energy per time slot.
//!
//! Both grid sensors report watts, so the energy that flowed in a slot is the integral of
//! power over it: a reading of `w` watts in effect for `s` seconds contributes
//! `w * s / 3_600_000` kilowatt hours. Working in fixed slots rather than local hours is
//! deliberate - energy accounting is absolute-time work, and a flat tariff has no
//! time-of-use windows to align a local hour to.

use rayon::prelude::*;

use crate::model::Sample;

/// Samples per rayon work unit, matching [`crate::aggregate`]'s reasoning: each unit
/// allocates one partial [`Slots`], so a few tens of thousands keeps both the allocation
/// overhead and the idle time small.
const CHUNK_SIZE: usize = 32_768;

/// Watt seconds per kilowatt hour.
const WATT_SECONDS_PER_KWH: f64 = 3_600_000.0;

/// The common time grid both sensors are accumulated onto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotGrid {
    origin_ts: i64,
    slot_seconds: i64,
    len: usize,
}

impl SlotGrid {
    /// A grid of `slot_seconds` slots covering `[first_ts, last_ts)`, aligned so that the
    /// origin is a whole multiple of the slot length. Both series must use the same grid,
    /// which is what makes their slots comparable at all.
    pub fn covering(first_ts: i64, last_ts: i64, slot_seconds: i64) -> Self {
        let slot_seconds = slot_seconds.max(1);
        let origin_ts = first_ts.div_euclid(slot_seconds) * slot_seconds;
        let span = (last_ts.max(first_ts) - origin_ts).max(0);
        // The last instant is exclusive, so a span landing exactly on a boundary needs no
        // extra slot.
        let len = (span.div_euclid(slot_seconds) + i64::from(span % slot_seconds != 0)).max(1);
        Self {
            origin_ts,
            slot_seconds,
            len: len as usize,
        }
    }

    pub fn origin_ts(&self) -> i64 {
        self.origin_ts
    }

    pub fn slot_seconds(&self) -> i64 {
        self.slot_seconds
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// When the slot at `index` starts.
    pub fn start_of(&self, index: usize) -> i64 {
        self.origin_ts + index as i64 * self.slot_seconds
    }

    /// Which slot an instant falls in, or `None` when it is outside the grid.
    pub fn index_of(&self, ts: i64) -> Option<usize> {
        let offset = ts - self.origin_ts;
        if offset < 0 {
            return None;
        }
        let index = offset.div_euclid(self.slot_seconds) as usize;
        (index < self.len).then_some(index)
    }
}

/// Split `[start, end)` at slot boundaries, calling `f(slot index, seconds)` for each
/// piece that lands on the grid.
pub fn for_each_slot_slice<F>(start: i64, end: i64, grid: SlotGrid, mut f: F)
where
    F: FnMut(usize, f64),
{
    if end <= start {
        return;
    }
    let mut cursor = start.max(grid.origin_ts());
    let last = end.min(grid.start_of(grid.len()));
    while cursor < last {
        let index = grid
            .index_of(cursor)
            .expect("the cursor stays inside the grid");
        let boundary = grid.start_of(index + 1);
        let slice_end = last.min(boundary);
        f(index, (slice_end - cursor) as f64);
        cursor = slice_end;
    }
}

/// Energy and observation time per slot, for one sensor.
///
/// Additive - both vectors sum element-wise - which is what lets rayon fold disjoint
/// chunks of samples independently and merge the results.
#[derive(Debug, Clone, PartialEq)]
pub struct Slots {
    grid: SlotGrid,
    kwh: Vec<f64>,
    seconds: Vec<f64>,
}

impl Slots {
    pub fn new(grid: SlotGrid) -> Self {
        Self {
            grid,
            kwh: vec![0.0; grid.len()],
            seconds: vec![0.0; grid.len()],
        }
    }

    pub fn grid(&self) -> SlotGrid {
        self.grid
    }

    pub fn kwh(&self, index: usize) -> f64 {
        self.kwh.get(index).copied().unwrap_or(0.0)
    }

    pub fn seconds(&self, index: usize) -> f64 {
        self.seconds.get(index).copied().unwrap_or(0.0)
    }

    pub fn total_kwh(&self) -> f64 {
        self.kwh.iter().sum()
    }

    pub fn observed_seconds(&self) -> f64 {
        self.seconds.iter().sum()
    }

    /// Add `seconds` of observation at `watts` to one slot.
    pub fn add(&mut self, index: usize, watts: f64, seconds: f64) {
        if !seconds.is_finite() || seconds <= 0.0 || !watts.is_finite() {
            return;
        }
        let Some(kwh) = self.kwh.get_mut(index) else {
            return;
        };
        *kwh += watts * seconds / WATT_SECONDS_PER_KWH;
        self.seconds[index] += seconds;
    }

    /// Element-wise sum, used to merge the partial results of rayon workers.
    pub fn merge(mut self, other: Slots) -> Slots {
        debug_assert_eq!(self.grid, other.grid);
        for (target, value) in self.kwh.iter_mut().zip(other.kwh.iter()) {
            *target += value;
        }
        for (target, value) in self.seconds.iter_mut().zip(other.seconds.iter()) {
            *target += value;
        }
        self
    }
}

/// Integrate power samples into per-slot energy, in parallel.
pub fn accumulate(samples: &[Sample], grid: SlotGrid) -> Slots {
    samples
        .par_chunks(CHUNK_SIZE)
        .map(|chunk| {
            let mut slots = Slots::new(grid);
            for sample in chunk {
                accumulate_one(&mut slots, sample, grid);
            }
            slots
        })
        .reduce(|| Slots::new(grid), Slots::merge)
}

/// Sequential equivalent of [`accumulate`], kept for tests and tiny inputs.
pub fn accumulate_sequential(samples: &[Sample], grid: SlotGrid) -> Slots {
    let mut slots = Slots::new(grid);
    for sample in samples {
        accumulate_one(&mut slots, sample, grid);
    }
    slots
}

fn accumulate_one(slots: &mut Slots, sample: &Sample, grid: SlotGrid) {
    if sample.duration() <= 0 || !sample.watts.is_finite() {
        return;
    }
    let watts = sample.watts;
    for_each_slot_slice(sample.start_ts, sample.end_ts, grid, |index, seconds| {
        slots.add(index, watts, seconds);
    });
}

/// The grid covering every sample of both sensors.
pub fn grid_for(left: &[Sample], right: &[Sample], slot_seconds: i64) -> Option<SlotGrid> {
    let span = |samples: &[Sample]| {
        let first = samples
            .iter()
            .filter(|sample| sample.duration() > 0)
            .map(|sample| sample.start_ts)
            .min()?;
        let last = samples
            .iter()
            .filter(|sample| sample.duration() > 0)
            .map(|sample| sample.end_ts)
            .max()?;
        Some((first, last))
    };
    let spans = [span(left), span(right)];
    let first = spans.iter().flatten().map(|(first, _)| *first).min()?;
    let last = spans.iter().flatten().map(|(_, last)| *last).max()?;
    Some(SlotGrid::covering(first, last, slot_seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 3_600;

    fn grid(len: usize) -> SlotGrid {
        SlotGrid::covering(0, len as i64 * HOUR, HOUR)
    }

    #[test]
    fn a_grid_is_aligned_to_whole_slots() {
        // 09:30 in a database of hourly slots starts the grid at 09:00.
        let grid = SlotGrid::covering(9 * HOUR + 1_800, 11 * HOUR, HOUR);
        assert_eq!(grid.origin_ts(), 9 * HOUR);
        assert_eq!(grid.len(), 2);
        assert_eq!(grid.start_of(1), 10 * HOUR);
        assert_eq!(grid.index_of(9 * HOUR), Some(0));
        assert_eq!(grid.index_of(10 * HOUR - 1), Some(0));
        assert_eq!(grid.index_of(10 * HOUR), Some(1));
        assert_eq!(grid.index_of(11 * HOUR), None, "the end is exclusive");
        assert_eq!(grid.index_of(0), None, "before the grid");
    }

    #[test]
    fn an_hour_at_a_kilowatt_is_a_kilowatt_hour() {
        let slots = accumulate(&[Sample::new(0, HOUR, 1_000.0)], grid(1));
        assert!((slots.kwh(0) - 1.0).abs() < 1e-12, "{}", slots.kwh(0));
        assert_eq!(slots.seconds(0), 3_600.0);
    }

    #[test]
    fn a_sample_spanning_slots_is_split_by_seconds() {
        // Half an hour either side of the boundary at 10:00.
        let slots = accumulate(
            &[Sample::new(9 * HOUR + 1_800, 10 * HOUR + 1_800, 2_000.0)],
            SlotGrid::covering(9 * HOUR, 11 * HOUR, HOUR),
        );
        assert!((slots.kwh(0) - 1.0).abs() < 1e-12);
        assert!((slots.kwh(1) - 1.0).abs() < 1e-12);
        assert_eq!(slots.seconds(0), 1_800.0);
        assert_eq!(slots.seconds(1), 1_800.0);
    }

    #[test]
    fn a_chatty_sensor_integrates_to_the_same_energy() {
        // One hourly mean of 1 kW ...
        let hourly = accumulate(&[Sample::new(0, HOUR, 1_000.0)], grid(1));
        // ... against 60 one-minute readings averaging the same.
        let chatty: Vec<Sample> = (0..60)
            .map(|minute| {
                let start = minute * 60;
                let watts = if minute % 2 == 0 { 500.0 } else { 1_500.0 };
                Sample::new(start, start + 60, watts)
            })
            .collect();
        let fine = accumulate(&chatty, grid(1));
        assert!((hourly.kwh(0) - fine.kwh(0)).abs() < 1e-12);
        assert_eq!(fine.seconds(0), 3_600.0);
    }

    #[test]
    fn samples_outside_the_grid_are_ignored() {
        let grid = SlotGrid::covering(10 * HOUR, 11 * HOUR, HOUR);
        let slots = accumulate(
            &[
                Sample::new(0, HOUR, 5_000.0),                    // before
                Sample::new(20 * HOUR, 21 * HOUR, 5_000.0),       // after
                Sample::new(10 * HOUR, 10 * HOUR + 900, 4_000.0), // inside
            ],
            grid,
        );
        assert_eq!(slots.observed_seconds(), 900.0);
        assert!((slots.total_kwh() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn degenerate_samples_contribute_nothing() {
        let slots = accumulate(
            &[
                Sample::new(0, 0, 1_000.0),
                Sample::new(HOUR, 0, 1_000.0),
                Sample::new(0, HOUR, f64::NAN),
            ],
            grid(2),
        );
        assert_eq!(slots.total_kwh(), 0.0);
        assert_eq!(slots.observed_seconds(), 0.0);
    }

    #[test]
    fn the_parallel_and_sequential_folds_agree() {
        let samples: Vec<Sample> = (0..50_000)
            .map(|index| {
                let start = index * 300;
                Sample::new(start, start + 300, (index % 4_000) as f64)
            })
            .collect();
        let grid = grid_for(&samples, &[], HOUR).expect("a grid");
        let parallel = accumulate(&samples, grid);
        let sequential = accumulate_sequential(&samples, grid);

        // Floating point addition is not associative, so a chunked fold may differ in the
        // last bits; anything larger than that would be a real disagreement.
        for index in 0..grid.len() {
            assert!(
                (parallel.kwh(index) - sequential.kwh(index)).abs() < 1e-9,
                "slot {index}: {} vs {}",
                parallel.kwh(index),
                sequential.kwh(index)
            );
            assert_eq!(
                parallel.seconds(index),
                sequential.seconds(index),
                "slot {index} saw different observation time"
            );
        }
        assert!((parallel.total_kwh() - sequential.total_kwh()).abs() < 1e-6);
    }

    #[test]
    fn the_grid_covers_both_sensors() {
        let left = [Sample::new(5 * HOUR, 6 * HOUR, 100.0)];
        let right = [Sample::new(9 * HOUR, 10 * HOUR, 100.0)];
        let grid = grid_for(&left, &right, HOUR).unwrap();
        assert_eq!(grid.origin_ts(), 5 * HOUR);
        assert_eq!(grid.len(), 5);
        assert_eq!(grid_for(&[], &[], HOUR), None);
    }

    #[test]
    fn five_minute_slots_work_the_same_way() {
        let grid = SlotGrid::covering(0, HOUR, 300);
        assert_eq!(grid.len(), 12);
        let slots = accumulate(&[Sample::new(0, HOUR, 600.0)], grid);
        assert_eq!(slots.seconds(11), 300.0);
        assert!((slots.total_kwh() - 0.6).abs() < 1e-12);
    }
}
