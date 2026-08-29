//! Trying every battery size, in parallel, and pricing the answer.

use rayon::prelude::*;

use crate::storage::pair::Paired;
use crate::storage::simulate::{BatterySpec, Simulation, simulate};

/// Seconds in a mean Gregorian year, used to annualise a partial history.
pub const SECONDS_PER_YEAR: f64 = 365.2425 * 86_400.0;

/// What energy and batteries cost.
#[derive(Debug, Clone, PartialEq)]
pub struct Economics {
    /// Price of a kilowatt hour taken from the grid.
    pub price_per_kwh: f64,
    /// What a kilowatt hour fed back is worth. Zero when the export is gifted.
    pub feed_in_price: f64,
    /// Installed battery cost per kilowatt hour of nameplate capacity.
    pub cost_per_kwh: f64,
    /// The part of the bill that does not scale with capacity: inverter, wiring, labour.
    pub base_cost: f64,
    pub currency: String,
}

impl Economics {
    /// What a battery of this size costs to put in.
    pub fn investment(&self, capacity_kwh: f64) -> f64 {
        self.base_cost + self.cost_per_kwh * capacity_kwh
    }
}

/// How long an investment takes to pay for itself, or `None` when it never does.
pub fn payback_years(investment: f64, annual_savings: f64) -> Option<f64> {
    (annual_savings > 0.0 && investment.is_finite()).then(|| investment / annual_savings)
}

/// One battery size, simulated and priced.
#[derive(Debug, Clone, PartialEq)]
pub struct SizeResult {
    pub capacity_kwh: f64,
    pub simulation: Simulation,
    /// Savings over the observed history, before annualising.
    pub observed_savings: f64,
    pub annual_savings: f64,
    pub investment: f64,
    pub payback_years: Option<f64>,
    /// Share of the imported energy this battery avoided buying, 0..=1.
    pub import_reduction: f64,
    pub cycles_per_year: f64,
    /// What the step up from the previous size, on its own, takes to pay for itself.
    pub marginal_payback_years: Option<f64>,
}

/// The whole sweep, with the arithmetic that turned an observed period into a year.
#[derive(Debug, Clone, PartialEq)]
pub struct Sweep {
    pub results: Vec<SizeResult>,
    /// Multiplier taking the observed period up to a full year.
    pub annualisation: f64,
    pub observed_seconds: f64,
    pub import_kwh: f64,
    pub export_kwh: f64,
    /// Index of the fastest payback, if anything pays back at all.
    pub best: Option<usize>,
    pub economics: Economics,
}

impl Sweep {
    pub fn best_result(&self) -> Option<&SizeResult> {
        self.best.and_then(|index| self.results.get(index))
    }

    /// The result one step larger than the best, which is what says whether the next
    /// kilowatt hour is worth buying.
    pub fn next_after_best(&self) -> Option<&SizeResult> {
        self.results.get(self.best? + 1)
    }

    /// Payback for one size at a different price and cost, without re-simulating: the
    /// energy flows do not depend on what anything costs.
    pub fn payback_at(&self, result: &SizeResult, price: f64, cost_factor: f64) -> Option<f64> {
        let savings = result
            .simulation
            .savings(price, self.economics.feed_in_price)
            * self.annualisation;
        payback_years(
            self.economics.investment(result.capacity_kwh) * cost_factor,
            savings,
        )
    }
}

/// The sizes to try, from a range.
pub fn size_range(min: f64, max: f64, step: f64) -> Vec<f64> {
    if !(min.is_finite() && max.is_finite() && step.is_finite()) || step <= 0.0 || max < min {
        return Vec::new();
    }
    let count = ((max - min) / step).floor() as usize;
    (0..=count)
        // Multiplying rather than accumulating keeps 0.1 steps from drifting.
        .map(|index| min + step * index as f64)
        .filter(|size| *size > 0.0)
        .collect()
}

/// Simulate every size and price the results.
///
/// The state of charge makes a single simulation inherently sequential, so the sizes are
/// the unit of parallelism: every core replays the whole history for a different battery,
/// which is why a finer sweep costs almost nothing until it outnumbers the cores.
pub fn sweep(
    paired: &Paired,
    sizes: &[f64],
    template: &BatterySpec,
    economics: &Economics,
) -> Sweep {
    let annualisation = if paired.observed_seconds > 0.0 {
        SECONDS_PER_YEAR / paired.observed_seconds
    } else {
        0.0
    };

    let mut results: Vec<SizeResult> = sizes
        .par_iter()
        .map(|capacity_kwh| {
            let spec = BatterySpec {
                capacity_kwh: *capacity_kwh,
                ..*template
            };
            let simulation = simulate(&paired.steps, &spec);
            let observed_savings =
                simulation.savings(economics.price_per_kwh, economics.feed_in_price);
            let annual_savings = observed_savings * annualisation;
            let investment = economics.investment(*capacity_kwh);
            SizeResult {
                capacity_kwh: *capacity_kwh,
                observed_savings,
                annual_savings,
                investment,
                payback_years: payback_years(investment, annual_savings),
                import_reduction: if paired.import_kwh > 0.0 {
                    simulation.avoided_import_kwh / paired.import_kwh
                } else {
                    0.0
                },
                cycles_per_year: simulation.full_cycles * annualisation,
                marginal_payback_years: None,
                simulation,
            }
        })
        .collect();
    results.sort_by(|a, b| a.capacity_kwh.total_cmp(&b.capacity_kwh));

    // What each step up costs against what it alone adds.
    for index in 0..results.len() {
        let (previous_investment, previous_savings) = match index.checked_sub(1) {
            Some(previous) => (
                results[previous].investment,
                results[previous].annual_savings,
            ),
            // The first size is measured against having no battery, which costs nothing.
            None => (0.0, 0.0),
        };
        results[index].marginal_payback_years = payback_years(
            results[index].investment - previous_investment,
            results[index].annual_savings - previous_savings,
        );
    }

    let best = results
        .iter()
        .enumerate()
        .filter_map(|(index, result)| result.payback_years.map(|years| (index, years)))
        .min_by(|left, right| left.1.total_cmp(&right.1))
        .map(|(index, _)| index);

    Sweep {
        results,
        annualisation,
        observed_seconds: paired.observed_seconds,
        import_kwh: paired.import_kwh,
        export_kwh: paired.export_kwh,
        best,
        economics: economics.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::pair::PairedStep;

    fn economics() -> Economics {
        Economics {
            price_per_kwh: 0.35,
            feed_in_price: 0.0,
            cost_per_kwh: 500.0,
            base_cost: 1_500.0,
            currency: "EUR".to_string(),
        }
    }

    /// `days` copies of a day that exports 8 kWh at noon and buys 6 kWh in the evening.
    fn history(days: i64) -> Paired {
        let mut steps = Vec::new();
        for day in 0..days {
            let midnight = day * 86_400;
            for hour in 0..24 {
                let (import_kwh, export_kwh) = match hour {
                    11 | 12 => (0.0, 4.0),
                    18 | 19 => (3.0, 0.0),
                    _ => (0.1, 0.0),
                };
                steps.push(PairedStep {
                    start_ts: midnight + hour * 3_600,
                    seconds: 3_600.0,
                    import_kwh,
                    export_kwh,
                });
            }
        }
        Paired {
            slot_seconds: 3_600,
            dropped_partial: 0,
            dropped_unpaired: 0,
            import_kwh: steps.iter().map(|step| step.import_kwh).sum(),
            export_kwh: steps.iter().map(|step| step.export_kwh).sum(),
            observed_seconds: steps.len() as f64 * 3_600.0,
            steps,
        }
    }

    fn template() -> BatterySpec {
        BatterySpec::new(0.0, 0.9, 0.9)
    }

    #[test]
    fn sizes_come_out_of_the_range_in_order() {
        assert_eq!(size_range(1.0, 5.0, 1.0), vec![1.0, 2.0, 3.0, 4.0, 5.0]);
        assert_eq!(size_range(2.0, 3.0, 0.5), vec![2.0, 2.5, 3.0]);
        // A range that does not divide evenly stops below the maximum rather than over it.
        assert_eq!(size_range(1.0, 4.5, 2.0), vec![1.0, 3.0]);
        // Nonsense yields nothing rather than an infinite loop.
        assert!(size_range(1.0, 5.0, 0.0).is_empty());
        assert!(size_range(5.0, 1.0, 1.0).is_empty());
        assert!(size_range(0.0, 0.0, 1.0).is_empty());
    }

    #[test]
    fn a_year_of_history_needs_no_annualising() {
        let sweep = sweep(&history(365), &[5.0], &template(), &economics());
        assert!(
            (sweep.annualisation - 1.0).abs() < 0.01,
            "{}",
            sweep.annualisation
        );
    }

    #[test]
    fn half_a_year_is_scaled_up_to_one() {
        let sweep = sweep(&history(183), &[5.0], &template(), &economics());
        assert!(
            (sweep.annualisation - 2.0).abs() < 0.05,
            "{}",
            sweep.annualisation
        );
        // And the annual figure really is the scaled observed one.
        let result = &sweep.results[0];
        assert!(
            (result.annual_savings - result.observed_savings * sweep.annualisation).abs() < 1e-9
        );
    }

    #[test]
    fn bigger_batteries_save_more_but_pay_back_more_slowly_in_the_end() {
        let sizes = size_range(1.0, 20.0, 1.0);
        let sweep = sweep(&history(365), &sizes, &template(), &economics());

        assert_eq!(sweep.results.len(), sizes.len());
        let savings: Vec<f64> = sweep.results.iter().map(|r| r.annual_savings).collect();
        for pair in savings.windows(2) {
            assert!(pair[1] >= pair[0] - 1e-9, "savings fell: {savings:?}");
        }

        // This history offers about 6 kWh a day of shiftable energy, so the fastest
        // payback is somewhere in the middle rather than at either end.
        let best = sweep.best.expect("something pays back");
        assert!(best > 0, "the smallest battery carries the whole base cost");
        assert!(
            best + 1 < sweep.results.len(),
            "capacity beyond the daily surplus cannot be the sweet spot: {best}"
        );
        let best_result = sweep.best_result().unwrap();
        for result in &sweep.results {
            if let Some(years) = result.payback_years {
                assert!(years >= best_result.payback_years.unwrap() - 1e-9);
            }
        }
        assert!(sweep.next_after_best().is_some());
    }

    #[test]
    fn the_marginal_kilowatt_hour_gets_worse_as_the_battery_grows() {
        let sweep = sweep(
            &history(365),
            &size_range(1.0, 20.0, 1.0),
            &template(),
            &economics(),
        );
        let best = sweep.best.unwrap();
        let marginal = sweep.results[best + 1].marginal_payback_years;
        assert!(
            marginal.is_none_or(|years| years > sweep.results[best].payback_years.unwrap()),
            "the step past the sweet spot must be the worse buy: {marginal:?}"
        );
        // The first size is measured against owning no battery at all.
        assert_eq!(
            sweep.results[0].marginal_payback_years,
            sweep.results[0].payback_years
        );
    }

    #[test]
    fn nothing_to_shift_means_nothing_pays_back() {
        // Import and export never overlap in a useful order: the battery starts empty and
        // the export comes last.
        let steps = vec![
            PairedStep {
                start_ts: 0,
                seconds: 3_600.0,
                import_kwh: 5.0,
                export_kwh: 0.0,
            },
            PairedStep {
                start_ts: 3_600,
                seconds: 3_600.0,
                import_kwh: 0.0,
                export_kwh: 5.0,
            },
        ];
        let paired = Paired {
            slot_seconds: 3_600,
            dropped_partial: 0,
            dropped_unpaired: 0,
            import_kwh: 5.0,
            export_kwh: 5.0,
            observed_seconds: 7_200.0,
            steps,
        };
        let sweep = sweep(&paired, &[5.0, 10.0], &template(), &economics());
        assert!(sweep.best.is_none());
        assert!(sweep.results.iter().all(|r| r.payback_years.is_none()));
        assert!(sweep.results.iter().all(|r| r.annual_savings == 0.0));
    }

    #[test]
    fn an_empty_history_does_not_divide_by_zero() {
        let paired = Paired {
            steps: Vec::new(),
            slot_seconds: 3_600,
            dropped_partial: 0,
            dropped_unpaired: 3,
            import_kwh: 0.0,
            export_kwh: 0.0,
            observed_seconds: 0.0,
        };
        let sweep = sweep(&paired, &[5.0], &template(), &economics());
        assert_eq!(sweep.annualisation, 0.0);
        assert_eq!(sweep.results[0].annual_savings, 0.0);
        assert_eq!(sweep.results[0].import_reduction, 0.0);
        assert!(sweep.best.is_none());
    }

    #[test]
    fn investment_carries_the_fixed_cost_of_installing_anything_at_all() {
        let economics = economics();
        assert_eq!(economics.investment(0.0), 1_500.0);
        assert_eq!(economics.investment(10.0), 6_500.0);
        assert_eq!(payback_years(6_500.0, 650.0), Some(10.0));
        assert_eq!(payback_years(6_500.0, 0.0), None);
        assert_eq!(payback_years(6_500.0, -5.0), None);
    }

    #[test]
    fn sensitivity_reprices_without_re_simulating() {
        let sweep = sweep(&history(365), &[10.0], &template(), &economics());
        let result = &sweep.results[0];
        let base = result.payback_years.unwrap();

        // A dearer kilowatt hour pays the battery off sooner ...
        let dearer = sweep.payback_at(result, 0.35 * 1.25, 1.0).unwrap();
        assert!(dearer < base, "{dearer} should beat {base}");
        // ... and a dearer battery pays off later, in exact proportion.
        let costlier = sweep.payback_at(result, 0.35, 1.25).unwrap();
        assert!((costlier - base * 1.25).abs() < 1e-9);
    }
}
