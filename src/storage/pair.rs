//! Lining the two sensors up slot by slot.
//!
//! A battery can only be simulated over time when *both* sides of the meter were
//! recorded: an hour with import readings but no export readings says nothing about the
//! surplus a battery would have stored. Slots that fail that test are dropped and
//! counted, never quietly treated as zero.

use crate::model::Sample;
use crate::storage::series::Slots;

/// One slot both sensors covered.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PairedStep {
    pub start_ts: i64,
    /// Time both sensors were observed for, in seconds.
    pub seconds: f64,
    pub import_kwh: f64,
    pub export_kwh: f64,
}

impl PairedStep {
    pub fn hours(&self) -> f64 {
        self.seconds / 3_600.0
    }

    /// The step as a sample, so [`crate::coverage`] can describe the paired timeline.
    ///
    /// The watt value is the average power across the step, which is what the energy was
    /// integrated from in the first place.
    pub fn as_sample(&self) -> Sample {
        let seconds = self.seconds.max(1.0);
        let watts = (self.import_kwh + self.export_kwh) * 3_600_000.0 / seconds;
        Sample::new(self.start_ts, self.start_ts + seconds as i64, watts)
    }
}

/// The timeline the simulation runs on, plus what had to be left out of it.
#[derive(Debug, Clone, PartialEq)]
pub struct Paired {
    pub steps: Vec<PairedStep>,
    pub slot_seconds: i64,
    /// Slots both sensors touched, but one of them too briefly to trust.
    pub dropped_partial: usize,
    /// Slots only one sensor recorded at all.
    pub dropped_unpaired: usize,
    pub import_kwh: f64,
    pub export_kwh: f64,
    /// Time the paired steps cover, in seconds - the basis for annualising savings.
    pub observed_seconds: f64,
}

impl Paired {
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    pub fn samples(&self) -> Vec<Sample> {
        self.steps.iter().map(PairedStep::as_sample).collect()
    }

    /// Slots that were dropped for either reason.
    pub fn dropped(&self) -> usize {
        self.dropped_partial + self.dropped_unpaired
    }
}

/// Pair two accumulated series, keeping the slots both sensors covered for at least
/// `min_coverage` of their length.
pub fn pair(import: &Slots, export: &Slots, min_coverage: f64) -> Paired {
    let grid = import.grid();
    debug_assert_eq!(grid, export.grid());
    let needed = grid.slot_seconds() as f64 * min_coverage.clamp(0.0, 1.0);

    let mut paired = Paired {
        steps: Vec::new(),
        slot_seconds: grid.slot_seconds(),
        dropped_partial: 0,
        dropped_unpaired: 0,
        import_kwh: 0.0,
        export_kwh: 0.0,
        observed_seconds: 0.0,
    };

    for index in 0..grid.len() {
        let (import_seconds, export_seconds) = (import.seconds(index), export.seconds(index));
        if import_seconds <= 0.0 && export_seconds <= 0.0 {
            // Neither sensor was recording: the slot simply is not part of the history.
            continue;
        }
        if import_seconds < needed || export_seconds < needed {
            if import_seconds > 0.0 && export_seconds > 0.0 {
                paired.dropped_partial += 1;
            } else {
                paired.dropped_unpaired += 1;
            }
            continue;
        }
        let step = PairedStep {
            start_ts: grid.start_of(index),
            seconds: import_seconds.min(export_seconds),
            import_kwh: import.kwh(index),
            export_kwh: export.kwh(index),
        };
        paired.import_kwh += step.import_kwh;
        paired.export_kwh += step.export_kwh;
        paired.observed_seconds += step.seconds;
        paired.steps.push(step);
    }
    paired
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::series::{SlotGrid, accumulate};

    const HOUR: i64 = 3_600;

    fn slots(samples: &[Sample], grid: SlotGrid) -> Slots {
        accumulate(samples, grid)
    }

    fn grid(hours: i64) -> SlotGrid {
        SlotGrid::covering(0, hours * HOUR, HOUR)
    }

    #[test]
    fn only_slots_both_sensors_recorded_are_simulated() {
        let grid = grid(3);
        // Import covers all three hours, export only the middle one.
        let import = slots(&[Sample::new(0, 3 * HOUR, 500.0)], grid);
        let export = slots(&[Sample::new(HOUR, 2 * HOUR, 2_000.0)], grid);

        let paired = pair(&import, &export, 0.9);
        assert_eq!(paired.steps.len(), 1);
        assert_eq!(paired.steps[0].start_ts, HOUR);
        assert!((paired.steps[0].import_kwh - 0.5).abs() < 1e-12);
        assert!((paired.steps[0].export_kwh - 2.0).abs() < 1e-12);
        assert_eq!(paired.dropped_unpaired, 2);
        assert_eq!(paired.dropped_partial, 0);
        assert_eq!(paired.observed_seconds, 3_600.0);
    }

    #[test]
    fn a_slot_neither_sensor_saw_is_not_counted_as_dropped() {
        let grid = grid(3);
        let import = slots(&[Sample::new(0, HOUR, 500.0)], grid);
        let export = slots(&[Sample::new(0, HOUR, 500.0)], grid);
        let paired = pair(&import, &export, 0.9);
        assert_eq!(paired.steps.len(), 1);
        assert_eq!(paired.dropped(), 0, "hours 1 and 2 are simply not history");
    }

    #[test]
    fn a_thinly_covered_slot_is_dropped_as_partial() {
        let grid = grid(1);
        let import = slots(&[Sample::new(0, HOUR, 500.0)], grid);
        // The export sensor only reported for ten minutes of the hour.
        let export = slots(&[Sample::new(0, 600, 500.0)], grid);

        let paired = pair(&import, &export, 0.9);
        assert!(paired.is_empty());
        assert_eq!(paired.dropped_partial, 1);
        assert_eq!(paired.dropped_unpaired, 0);

        // A lower bar accepts it, and the step only claims the time both were watched.
        let paired = pair(&import, &export, 0.1);
        assert_eq!(paired.steps.len(), 1);
        assert_eq!(paired.steps[0].seconds, 600.0);
    }

    #[test]
    fn totals_match_the_slots_that_survived() {
        let grid = grid(2);
        let import = slots(&[Sample::new(0, 2 * HOUR, 1_000.0)], grid);
        let export = slots(&[Sample::new(0, 2 * HOUR, 3_000.0)], grid);
        let paired = pair(&import, &export, 0.9);

        assert_eq!(paired.steps.len(), 2);
        assert!((paired.import_kwh - 2.0).abs() < 1e-12);
        assert!((paired.export_kwh - 6.0).abs() < 1e-12);
        assert_eq!(paired.observed_seconds, 7_200.0);
        assert_eq!(paired.slot_seconds, HOUR);
    }

    #[test]
    fn steps_describe_themselves_as_samples_for_the_coverage_report() {
        let step = PairedStep {
            start_ts: 0,
            seconds: 3_600.0,
            import_kwh: 1.0,
            export_kwh: 0.5,
        };
        let sample = step.as_sample();
        assert_eq!(sample.duration(), 3_600);
        assert!((sample.watts - 1_500.0).abs() < 1e-9, "{}", sample.watts);
        assert_eq!(step.hours(), 1.0);
    }
}
