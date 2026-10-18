//! Replaying one battery over the paired timeline.

use crate::pair::PairedStep;

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
#[derive(Debug, Clone, Copy, PartialEq, Default)]
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

/// Batteries [`simulate_sizes`] replays side by side, one per lane.
///
/// Eight doubles fill two AVX2 registers, four SSE2 or NEON ones: wide enough that the
/// two divisions every step waits on are hidden behind each other's latency.
pub const LANES: usize = 8;

/// [`simulate`] for every capacity in `capacities`, all else as in `template`.
///
/// A replay is one long chain - each step starts from the state of charge the last one
/// left - so a single battery cannot be split up. Different capacities are independent,
/// though, and every step does the same arithmetic for each: the capacities are taken
/// [`LANES`] at a time and replayed together, one per SIMD lane. Each lane does exactly
/// the operations [`simulate`] does, in the same order, so the results are the same to
/// the last bit.
pub fn simulate_sizes(
    steps: &[PairedStep],
    template: &BatterySpec,
    capacities: &[f64],
) -> Vec<Simulation> {
    capacities
        .chunks(LANES)
        .flat_map(|group| simulate_group(steps, template, group))
        .collect()
}

/// Up to [`LANES`] capacities replayed together.
pub fn simulate_group(
    steps: &[PairedStep],
    template: &BatterySpec,
    capacities: &[f64],
) -> Vec<Simulation> {
    assert!(capacities.len() <= LANES, "{} capacities", capacities.len());
    let eta = template.one_way();
    let mut usable = [0.0; LANES];
    for (lane, capacity_kwh) in capacities.iter().enumerate() {
        usable[lane] = BatterySpec {
            capacity_kwh: *capacity_kwh,
            ..*template
        }
        .usable_kwh();
    }

    let lanes = if eta > 0.0 {
        replay(steps, &usable, eta, template)
    } else {
        [Simulation::default(); LANES]
    };
    // A battery with nothing to cycle does nothing, as `simulate` says; its lane ran
    // along with the others, and is simply not used.
    (0..capacities.len())
        .map(|lane| {
            if usable[lane] > 0.0 {
                lanes[lane]
            } else {
                Simulation::default()
            }
        })
        .collect()
}

/// [`replay_lanes`], on the widest vectors the CPU running it has.
fn replay(
    steps: &[PairedStep],
    usable: &[f64; LANES],
    eta: f64,
    spec: &BatterySpec,
) -> [Simulation; LANES] {
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") {
        // SAFETY: the CPU has just been seen to support AVX2.
        return unsafe { avx2::replay(steps, usable, eta, spec) };
    }
    replay_lanes(steps, usable, eta, spec)
}

/// [`replay_lanes`] written out in AVX2 instructions, four lanes to a register.
///
/// The compiler does not vectorise the portable loop by itself - the NaN handling of
/// `f64::min` and `f64::max` puts it off - so here each line is the instruction it
/// should have chosen. Multiplication, division, addition and subtraction are the same
/// IEEE operations in a vector register as in a scalar one; the minimum and maximum
/// are built to give what `f64::min` and `f64::max` give, NaN included. No FMA: fusing
/// a multiply into an add would round once where [`simulate`] rounds twice.
#[cfg(target_arch = "x86_64")]
mod avx2 {
    use std::arch::x86_64::{
        __m256d, _CMP_UNORD_Q, _mm256_add_pd, _mm256_blendv_pd, _mm256_cmp_pd, _mm256_div_pd,
        _mm256_max_pd, _mm256_min_pd, _mm256_mul_pd, _mm256_set1_pd, _mm256_setr_pd,
        _mm256_setzero_pd, _mm256_storeu_pd, _mm256_sub_pd,
    };

    use super::{BatterySpec, LANES, PairedStep, Simulation};

    /// Registers per quantity.
    const REGISTERS: usize = LANES / 4;

    /// `value.min(other)`, lane by lane: `other` where `value` is NaN, else the smaller,
    /// `value` on a tie - the select LLVM lowers `f64::min` to on x86.
    #[inline]
    #[target_feature(enable = "avx2")]
    pub(super) fn min(value: __m256d, other: __m256d) -> __m256d {
        let nan = _mm256_cmp_pd::<_CMP_UNORD_Q>(value, value);
        // `minpd(a, b)` is `a < b ? a : b`.
        _mm256_blendv_pd(_mm256_min_pd(other, value), other, nan)
    }

    /// `value.max(other)`, lane by lane, built the same way as [`min`].
    #[inline]
    #[target_feature(enable = "avx2")]
    pub(super) fn max(value: __m256d, other: __m256d) -> __m256d {
        let nan = _mm256_cmp_pd::<_CMP_UNORD_Q>(value, value);
        // `maxpd(a, b)` is `a > b ? a : b`.
        _mm256_blendv_pd(_mm256_max_pd(other, value), other, nan)
    }

    /// The four doubles of a register.
    #[inline]
    #[target_feature(enable = "avx2")]
    pub(super) fn lanes(register: __m256d) -> [f64; 4] {
        let mut out = [0.0; 4];
        // SAFETY: `out` has room for the four doubles stored.
        unsafe { _mm256_storeu_pd(out.as_mut_ptr(), register) };
        out
    }

    #[target_feature(enable = "avx2")]
    pub(super) fn replay(
        steps: &[PairedStep],
        usable: &[f64; LANES],
        eta: f64,
        spec: &BatterySpec,
    ) -> [Simulation; LANES] {
        let usable: [__m256d; REGISTERS] = std::array::from_fn(|register| {
            let lane = register * 4;
            _mm256_setr_pd(
                usable[lane],
                usable[lane + 1],
                usable[lane + 2],
                usable[lane + 3],
            )
        });
        let eta = _mm256_set1_pd(eta);
        let zero = _mm256_setzero_pd();
        let mut soc = [zero; REGISTERS];
        let mut avoided = [zero; REGISTERS];
        let mut lost = [zero; REGISTERS];
        let mut peak = [zero; REGISTERS];

        for step in steps {
            let hours = step.hours();
            let discharge_ceiling = _mm256_set1_pd(
                spec.max_discharge_kw
                    .map_or(f64::INFINITY, |kw| (kw * hours).max(0.0)),
            );
            let charge_ceiling = _mm256_set1_pd(
                spec.max_charge_kw
                    .map_or(f64::INFINITY, |kw| (kw * hours).max(0.0)),
            );
            let import = _mm256_set1_pd(step.import_kwh);
            let export = _mm256_set1_pd(step.export_kwh);

            for register in 0..REGISTERS {
                let deliver = max(
                    min(
                        min(import, _mm256_mul_pd(soc[register], eta)),
                        discharge_ceiling,
                    ),
                    zero,
                );
                soc[register] = _mm256_sub_pd(soc[register], _mm256_div_pd(deliver, eta));
                avoided[register] = _mm256_add_pd(avoided[register], deliver);

                let room = _mm256_div_pd(_mm256_sub_pd(usable[register], soc[register]), eta);
                let absorb = max(min(min(export, room), charge_ceiling), zero);
                soc[register] = _mm256_add_pd(soc[register], _mm256_mul_pd(absorb, eta));
                lost[register] = _mm256_add_pd(lost[register], absorb);

                peak[register] = max(peak[register], soc[register]);
            }
        }

        let unpack = |registers: [__m256d; REGISTERS]| -> [f64; LANES] {
            let mut out = [0.0; LANES];
            for (register, value) in registers.into_iter().enumerate() {
                out[register * 4..register * 4 + 4].copy_from_slice(&lanes(value));
            }
            out
        };
        let (usable, soc) = (unpack(usable), unpack(soc));
        let (avoided, lost, peak) = (unpack(avoided), unpack(lost), unpack(peak));
        std::array::from_fn(|lane| Simulation {
            avoided_import_kwh: avoided[lane],
            lost_export_kwh: lost[lane],
            full_cycles: avoided[lane] / usable[lane],
            peak_soc_kwh: peak[lane],
            final_soc_kwh: soc[lane],
        })
    }
}

/// One double per battery, with the arithmetic [`simulate`] uses done lane by lane.
///
/// Every operation is a loop over a fixed-size array and nothing else, which is the shape
/// the compiler reliably turns into a handful of vector instructions.
#[derive(Debug, Clone, Copy)]
struct Lanes([f64; LANES]);

impl Lanes {
    #[inline(always)]
    fn splat(value: f64) -> Self {
        Self([value; LANES])
    }

    #[inline(always)]
    fn map(self, other: Self, op: impl Fn(f64, f64) -> f64) -> Self {
        Self(std::array::from_fn(|lane| op(self.0[lane], other.0[lane])))
    }

    #[inline(always)]
    fn min(self, other: Self) -> Self {
        self.map(other, f64::min)
    }

    #[inline(always)]
    fn max(self, other: Self) -> Self {
        self.map(other, f64::max)
    }
}

impl std::ops::Add for Lanes {
    type Output = Self;
    #[inline(always)]
    fn add(self, other: Self) -> Self {
        self.map(other, |left, right| left + right)
    }
}

impl std::ops::Sub for Lanes {
    type Output = Self;
    #[inline(always)]
    fn sub(self, other: Self) -> Self {
        self.map(other, |left, right| left - right)
    }
}

impl std::ops::Mul for Lanes {
    type Output = Self;
    #[inline(always)]
    fn mul(self, other: Self) -> Self {
        self.map(other, |left, right| left * right)
    }
}

impl std::ops::Div for Lanes {
    type Output = Self;
    #[inline(always)]
    fn div(self, other: Self) -> Self {
        self.map(other, |left, right| left / right)
    }
}

/// The loop of [`simulate`], with a lane per usable capacity.
///
/// Line for line the same arithmetic, in the same order; the ceilings depend only on the
/// step, so they are worked out once for every lane.
#[inline(always)]
fn replay_lanes(
    steps: &[PairedStep],
    usable: &[f64; LANES],
    eta: f64,
    spec: &BatterySpec,
) -> [Simulation; LANES] {
    let usable = Lanes(*usable);
    let eta = Lanes::splat(eta);
    let zero = Lanes::splat(0.0);
    let mut soc = zero;
    let mut avoided = zero;
    let mut lost = zero;
    let mut peak = zero;

    for step in steps {
        let hours = step.hours();
        let discharge_ceiling = Lanes::splat(
            spec.max_discharge_kw
                .map_or(f64::INFINITY, |kw| (kw * hours).max(0.0)),
        );
        let charge_ceiling = Lanes::splat(
            spec.max_charge_kw
                .map_or(f64::INFINITY, |kw| (kw * hours).max(0.0)),
        );

        let deliver = Lanes::splat(step.import_kwh)
            .min(soc * eta)
            .min(discharge_ceiling)
            .max(zero);
        soc = soc - deliver / eta;
        avoided = avoided + deliver;

        let absorb = Lanes::splat(step.export_kwh)
            .min((usable - soc) / eta)
            .min(charge_ceiling)
            .max(zero);
        soc = soc + absorb * eta;
        lost = lost + absorb;

        peak = peak.max(soc);
    }

    std::array::from_fn(|lane| Simulation {
        avoided_import_kwh: avoided.0[lane],
        lost_export_kwh: lost.0[lane],
        full_cycles: avoided.0[lane] / usable.0[lane],
        peak_soc_kwh: peak.0[lane],
        final_soc_kwh: soc.0[lane],
    })
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

    /// A year of uneven hours: sun some days and not others, a deficit most evenings.
    fn a_year() -> Vec<PairedStep> {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f64 / (1u64 << 53) as f64
        };
        (0..8_760)
            .map(|hour| {
                let daylight = (6..18).contains(&(hour % 24));
                PairedStep {
                    start_ts: hour * 3_600,
                    seconds: if hour % 50 == 0 { 2_700.0 } else { 3_600.0 },
                    import_kwh: if daylight { next() * 0.3 } else { next() * 1.7 },
                    export_kwh: if daylight { next() * 4.1 } else { 0.0 },
                }
            })
            .collect()
    }

    #[test]
    fn side_by_side_replays_match_one_at_a_time_to_the_last_bit() {
        let year = a_year();
        // Seventeen sizes: two full groups and a ragged one, with a battery too small to
        // cycle anything among them.
        let capacities: Vec<f64> = (0..17).map(|index| f64::from(index) * 1.37).collect();
        let mut limited = BatterySpec::new(0.0, 0.9, 0.88);
        limited.max_charge_kw = Some(3.3);
        limited.max_discharge_kw = Some(2.1);

        for template in [
            BatterySpec::new(0.0, 0.9, 0.9),
            BatterySpec::new(0.0, 1.0, 1.0),
            limited,
        ] {
            let together = simulate_sizes(&year, &template, &capacities);
            assert_eq!(together.len(), capacities.len());
            for (capacity_kwh, side_by_side) in capacities.iter().zip(&together) {
                let alone = simulate(
                    &year,
                    &BatterySpec {
                        capacity_kwh: *capacity_kwh,
                        ..template
                    },
                );
                // Compared bit for bit: equal is not close enough.
                for (left, right) in [
                    (alone.avoided_import_kwh, side_by_side.avoided_import_kwh),
                    (alone.lost_export_kwh, side_by_side.lost_export_kwh),
                    (alone.full_cycles, side_by_side.full_cycles),
                    (alone.peak_soc_kwh, side_by_side.peak_soc_kwh),
                    (alone.final_soc_kwh, side_by_side.final_soc_kwh),
                ] {
                    assert_eq!(left.to_bits(), right.to_bits(), "{capacity_kwh} kWh");
                }
            }
        }
    }

    #[test]
    fn the_portable_replay_matches_the_one_the_cpu_gets() {
        let year = a_year();
        let usable: [f64; LANES] = std::array::from_fn(|lane| lane as f64 * 2.5 + 0.5);
        let spec = BatterySpec::new(0.0, 0.9, 0.9);
        let portable = replay_lanes(&year, &usable, spec.one_way(), &spec);
        assert_eq!(replay(&year, &usable, spec.one_way(), &spec), portable);
    }

    /// The AVX2 minimum and maximum against `f64::min` and `f64::max`, on every pair of
    /// awkward values - NaN, infinities, the smallest and largest doubles - except the
    /// two zeros, which Rust leaves free to come out either way round.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn the_vector_minimum_and_maximum_are_the_scalar_ones() {
        if !std::arch::is_x86_feature_detected!("avx2") {
            return;
        }
        let values = [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MAX,
            f64::MIN,
            f64::MIN_POSITIVE,
            -f64::MIN_POSITIVE,
            0.0,
            1.0,
            -2.5,
            1e-300,
            3.75,
        ];
        let same = |left: f64, right: f64| {
            left.to_bits() == right.to_bits() || (left.is_nan() && right.is_nan())
        };
        for chunk in values.chunks(4) {
            let mut value = [0.0; 4];
            value[..chunk.len()].copy_from_slice(chunk);
            for other in values {
                // SAFETY: AVX2 was checked for above.
                let (min, max) = unsafe {
                    let value = std::arch::x86_64::_mm256_loadu_pd(value.as_ptr());
                    let other = std::arch::x86_64::_mm256_set1_pd(other);
                    (
                        avx2::lanes(avx2::min(value, other)),
                        avx2::lanes(avx2::max(value, other)),
                    )
                };
                for lane in 0..4 {
                    let value = value[lane];
                    assert!(same(min[lane], value.min(other)), "{value} min {other}");
                    assert!(same(max[lane], value.max(other)), "{value} max {other}");
                }
            }
        }
    }

    #[test]
    fn a_battery_without_efficiency_does_nothing_side_by_side_too() {
        let year = a_year();
        let results = simulate_sizes(&year, &BatterySpec::new(0.0, 0.9, 0.0), &[5.0, 10.0]);
        assert_eq!(results, [Simulation::default(); 2]);
        assert!(simulate_sizes(&year, &BatterySpec::new(0.0, 0.9, 0.9), &[]).is_empty());
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
