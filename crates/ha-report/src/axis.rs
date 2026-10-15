//! Where an axis puts its gridlines.

/// Gridline positions for an axis running from 0 to `max`.
///
/// Steps are the familiar 1 / 2 / 2.5 / 5 × 10^n sequence, so labels land on round
/// numbers like 1 kW rather than 1 137 W.
pub fn nice_ticks(max: f64, target: usize) -> Vec<f64> {
    if !max.is_finite() || max <= 0.0 || target == 0 {
        return vec![0.0];
    }
    let rough = max / target as f64;
    let magnitude = 10f64.powf(rough.log10().floor());
    let normalised = rough / magnitude;
    let step = if normalised <= 1.0 {
        1.0
    } else if normalised <= 2.0 {
        2.0
    } else if normalised <= 2.5 {
        2.5
    } else if normalised <= 5.0 {
        5.0
    } else {
        10.0
    } * magnitude;

    let mut ticks = Vec::new();
    let mut value = 0.0;
    while value <= max + step * 1e-9 {
        ticks.push(value);
        value += step;
    }
    ticks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_land_on_round_numbers() {
        assert_eq!(
            nice_ticks(1_000.0, 5),
            vec![0.0, 200.0, 400.0, 600.0, 800.0, 1_000.0]
        );
        assert_eq!(
            nice_ticks(8_000.0, 5),
            vec![0.0, 2_000.0, 4_000.0, 6_000.0, 8_000.0]
        );
        assert_eq!(nice_ticks(450.0, 5), vec![0.0, 100.0, 200.0, 300.0, 400.0]);
        assert_eq!(nice_ticks(0.0, 5), vec![0.0]);
        assert_eq!(nice_ticks(f64::NAN, 5), vec![0.0]);
        assert_eq!(nice_ticks(100.0, 0), vec![0.0]);
    }

    #[test]
    fn ticks_stay_inside_the_axis() {
        for max in [50.0, 137.0, 999.0, 4_050.0, 12_345.0] {
            let ticks = nice_ticks(max, 5);
            assert!(
                ticks.iter().all(|tick| *tick <= max + 1e-9),
                "{ticks:?} for {max}"
            );
            assert!(ticks.len() >= 2, "{ticks:?} for {max}");
        }
    }
}
