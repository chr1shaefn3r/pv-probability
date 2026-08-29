//! Replaying one battery over the paired timeline.

use crate::storage::pair::PairedStep;

/// The battery being tried out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BatterySpec {
    /// Nameplate capacity in kWh.
    pub capacity_kwh: f64,
    /// Share of the nameplate a battery is actually cycled over.
    pub usable_fraction: f64,
    /// Round trip efficiency, charged half on the way in and half on the way out.
    pub round_trip: f64,
    /// Charge and discharge power ceilings, `None` for unlimited.
    pub max_charge_kw: Option<f64>,
    pub max_discharge_kw: Option<f64>,
}

impl BatterySpec {
    /// A battery of `capacity_kwh` with the usual assumptions.
    pub fn new(capacity_kwh: f64, usable_fraction: f64, round_trip: f64) -> Self {
        Self {
            capacity_kwh,
            usable_fraction,
            round_trip,
            max_charge_kw: None,
            max_discharge_kw: None,
        }
    }

    /// The energy actually available to cycle, in kWh.
    pub fn usable_kwh(&self) -> f64 {
        (self.capacity_kwh * self.usable_fraction).max(0.0)
    }

    /// One-way efficiency: the round trip is charged half on each side, so that
    /// `charge * discharge == round_trip`.
    fn one_way(&self) -> f64 {
        self.round_trip.clamp(0.0, 1.0).sqrt()
    }
}

/// What the battery did over the whole history.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Simulation {
    /// Energy delivered to the house that would otherwise have been bought.
    pub avoided_import_kwh: f64,
    /// Surplus taken from the export instead of being given away.
    pub lost_export_kwh: f64,
    /// Full equivalent cycles of the usable capacity.
    pub full_cycles: f64,
    pub peak_soc_kwh: f64,
    pub final_soc_kwh: f64,
}

impl Simulation {
    /// Energy lost to the round trip, in kWh.
    pub fn losses_kwh(&self) -> f64 {
        (self.lost_export_kwh - self.avoided_import_kwh).max(0.0)
    }

    /// What the battery is worth over the simulated period: the import it avoided, less
    /// the export it swallowed if that export was being paid for.
    pub fn savings(&self, price_per_kwh: f64, feed_in_price: f64) -> f64 {
        self.avoided_import_kwh * price_per_kwh - self.lost_export_kwh * feed_in_price
    }
}

/// Run one battery over the paired steps.
///
/// Each step serves the import first and charges from the surplus afterwards: a battery
/// filled yesterday covers this morning, and today's sun refills it. Import and export
/// inside one step are deliberately not netted off against each other - had they been
/// simultaneous the house would never have imported at all, so they happened at different
/// minutes of the step and bridging them is precisely the battery's job.
pub fn simulate(steps: &[PairedStep], spec: &BatterySpec) -> Simulation {
    let usable = spec.usable_kwh();
    let eta = spec.one_way();
    let mut soc = 0.0f64;
    let mut result = Simulation {
        avoided_import_kwh: 0.0,
        lost_export_kwh: 0.0,
        full_cycles: 0.0,
        peak_soc_kwh: 0.0,
        final_soc_kwh: 0.0,
    };
    if usable <= 0.0 || eta <= 0.0 {
        return result;
    }

    for step in steps {
        let hours = step.hours();

        // Discharge into whatever was being bought this step.
        let ceiling = spec
            .max_discharge_kw
            .map_or(f64::INFINITY, |kw| (kw * hours).max(0.0));
        let deliver = step.import_kwh.min(soc * eta).min(ceiling).max(0.0);
        soc -= deliver / eta;
        result.avoided_import_kwh += deliver;

        // Then store what is left of the surplus.
        let ceiling = spec
            .max_charge_kw
            .map_or(f64::INFINITY, |kw| (kw * hours).max(0.0));
        let absorb = step
            .export_kwh
            .min((usable - soc) / eta)
            .min(ceiling)
            .max(0.0);
        soc += absorb * eta;
        result.lost_export_kwh += absorb;

        result.peak_soc_kwh = result.peak_soc_kwh.max(soc);
    }

    result.final_soc_kwh = soc;
    result.full_cycles = result.avoided_import_kwh / usable;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A day of steps: `(import_kwh, export_kwh)` per hour.
    fn steps(hours: &[(f64, f64)]) -> Vec<PairedStep> {
        hours
            .iter()
            .enumerate()
            .map(|(hour, (import, export))| PairedStep {
                start_ts: hour as i64 * 3_600,
                seconds: 3_600.0,
                import_kwh: *import,
                export_kwh: *export,
            })
            .collect()
    }

    /// A lossless battery, so the arithmetic in a test is the arithmetic being tested.
    fn perfect(capacity_kwh: f64) -> BatterySpec {
        BatterySpec::new(capacity_kwh, 1.0, 1.0)
    }

    #[test]
    fn a_battery_moves_surplus_into_the_next_deficit() {
        // 4 kWh exported at midday, 3 kWh bought in the evening.
        let day = steps(&[(0.0, 4.0), (3.0, 0.0)]);
        let result = simulate(&day, &perfect(10.0));

        assert!((result.lost_export_kwh - 4.0).abs() < 1e-12);
        assert!((result.avoided_import_kwh - 3.0).abs() < 1e-12);
        assert!((result.final_soc_kwh - 1.0).abs() < 1e-12);
        assert!((result.peak_soc_kwh - 4.0).abs() < 1e-12);
    }

    #[test]
    fn nothing_happens_without_a_battery() {
        let day = steps(&[(0.0, 4.0), (3.0, 0.0)]);
        for capacity in [0.0, -5.0] {
            let result = simulate(&day, &perfect(capacity));
            assert_eq!(result.avoided_import_kwh, 0.0);
            assert_eq!(result.lost_export_kwh, 0.0);
            assert_eq!(result.full_cycles, 0.0);
        }
    }

    #[test]
    fn an_enormous_battery_is_bounded_by_the_smaller_side_of_the_meter() {
        // Far more export than import: only the import can ever be avoided.
        let day = steps(&[(0.0, 50.0), (2.0, 0.0), (1.0, 0.0)]);
        let result = simulate(&day, &perfect(1_000.0));
        assert!((result.avoided_import_kwh - 3.0).abs() < 1e-12);

        // And the other way round: only what was exported can ever be stored.
        let day = steps(&[(0.0, 2.0), (50.0, 0.0)]);
        let result = simulate(&day, &perfect(1_000.0));
        assert!((result.avoided_import_kwh - 2.0).abs() < 1e-12);
    }

    #[test]
    fn only_energy_already_stored_can_be_used() {
        // The import comes before the sun, so the battery starts empty and can do nothing.
        let day = steps(&[(3.0, 0.0), (0.0, 4.0)]);
        let result = simulate(&day, &perfect(10.0));
        assert_eq!(result.avoided_import_kwh, 0.0);
        assert!((result.lost_export_kwh - 4.0).abs() < 1e-12);
    }

    #[test]
    fn round_trip_losses_come_out_of_what_is_returned() {
        let day = steps(&[(0.0, 10.0), (10.0, 0.0)]);
        let spec = BatterySpec::new(10.0, 1.0, 0.81); // one way 0.9
        let result = simulate(&day, &spec);

        assert!(
            (result.lost_export_kwh - 10.0).abs() < 1e-12,
            "charges fully"
        );
        assert!(
            (result.avoided_import_kwh - 8.1).abs() < 1e-12,
            "returns the round trip share: {}",
            result.avoided_import_kwh
        );
        assert!((result.losses_kwh() - 1.9).abs() < 1e-12);
    }

    #[test]
    fn only_the_usable_share_is_cycled() {
        let day = steps(&[(0.0, 10.0), (10.0, 0.0)]);
        let result = simulate(&day, &BatterySpec::new(10.0, 0.8, 1.0));
        assert!((result.lost_export_kwh - 8.0).abs() < 1e-12);
        assert!((result.avoided_import_kwh - 8.0).abs() < 1e-12);
        assert!(
            (result.full_cycles - 1.0).abs() < 1e-12,
            "one full cycle of 8 kWh"
        );
    }

    #[test]
    fn power_limits_cap_what_one_step_can_move() {
        let day = steps(&[(0.0, 9.0), (9.0, 0.0)]);
        let mut spec = perfect(20.0);
        spec.max_charge_kw = Some(3.0);
        spec.max_discharge_kw = Some(2.0);
        let result = simulate(&day, &spec);

        assert!(
            (result.lost_export_kwh - 3.0).abs() < 1e-12,
            "3 kW for an hour"
        );
        assert!(
            (result.avoided_import_kwh - 2.0).abs() < 1e-12,
            "2 kW for an hour"
        );
    }

    #[test]
    fn the_state_of_charge_stays_inside_the_usable_window() {
        let day = steps(&[(0.0, 8.0), (1.0, 6.0), (20.0, 0.0), (0.0, 5.0)]);
        let spec = BatterySpec::new(5.0, 0.9, 0.9);
        let result = simulate(&day, &spec);
        assert!(result.peak_soc_kwh <= spec.usable_kwh() + 1e-12);
        assert!(result.final_soc_kwh >= -1e-12);
        assert!(result.final_soc_kwh <= spec.usable_kwh() + 1e-12);
    }

    #[test]
    fn a_bigger_battery_never_saves_less() {
        // A fortnight of a lopsided day, so capacity keeps paying off for a while.
        let mut day = Vec::new();
        for _ in 0..14 {
            day.extend(steps(&[(0.0, 6.0), (0.0, 6.0), (4.0, 0.0), (4.0, 0.0)]));
        }
        let mut previous = 0.0;
        for capacity in [0.0, 1.0, 2.0, 4.0, 8.0, 16.0, 32.0] {
            let saved = simulate(&day, &BatterySpec::new(capacity, 0.9, 0.9)).avoided_import_kwh;
            assert!(
                saved >= previous - 1e-9,
                "{capacity} kWh saved {saved}, less than the smaller battery's {previous}"
            );
            previous = saved;
        }
    }

    #[test]
    fn savings_price_the_import_avoided_and_the_export_given_up() {
        let result = Simulation {
            avoided_import_kwh: 100.0,
            lost_export_kwh: 120.0,
            full_cycles: 10.0,
            peak_soc_kwh: 5.0,
            final_soc_kwh: 1.0,
        };
        // Export is gifted: the whole avoided import is money saved.
        assert!((result.savings(0.35, 0.0) - 35.0).abs() < 1e-12);
        // With a feed-in tariff the surplus was worth something, so less is gained.
        assert!((result.savings(0.35, 0.08) - 25.4).abs() < 1e-12);
    }

    #[test]
    fn half_hour_steps_respect_power_limits_in_kilowatts() {
        let day = vec![
            PairedStep {
                start_ts: 0,
                seconds: 1_800.0,
                import_kwh: 0.0,
                export_kwh: 5.0,
            },
            PairedStep {
                start_ts: 1_800,
                seconds: 1_800.0,
                import_kwh: 5.0,
                export_kwh: 0.0,
            },
        ];
        let mut spec = perfect(20.0);
        spec.max_charge_kw = Some(4.0);
        spec.max_discharge_kw = Some(4.0);
        let result = simulate(&day, &spec);
        assert!(
            (result.lost_export_kwh - 2.0).abs() < 1e-12,
            "4 kW for half an hour"
        );
        assert!((result.avoided_import_kwh - 2.0).abs() < 1e-12);
    }
}
