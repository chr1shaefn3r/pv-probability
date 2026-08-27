//! The likelihood colour scale.
//!
//! Probabilities are quantised into a small number of levels and drawn from a
//! semantic heat ramp: unlikely readings are yellow and recede towards the page, likely
//! ones are a saturated red. Both ramps are monotone in OKLCH lightness (light mode runs
//! L 0.95 -> 0.43, dark mode L 0.34 -> 0.65), so the scale still reads correctly in
//! greyscale and under colour vision deficiencies; the multi-hue journey is the
//! "semantic heat" case, which is why the page always ships a scale legend and a table
//! view of the same numbers.

/// Number of colour levels used unless the caller asks for another.
pub const DEFAULT_LEVELS: usize = 10;
/// Levels below this would blur into each other; above this the legend gets unreadable.
pub const MIN_LEVELS: usize = 3;
pub const MAX_LEVELS: usize = 16;

/// Heat ramp for a light surface (`#fcfcfb`), least to most likely.
pub const HEAT_LIGHT: [&str; DEFAULT_LEVELS] = [
    "#fef0b4", "#fde68f", "#fbd464", "#f9bd45", "#f6a132", "#ef8329", "#e26522", "#d0481f",
    "#b62f1d", "#96161a",
];

/// The same journey stepped for a dark surface (`#1a1a19`), least to most likely.
pub const HEAT_DARK: [&str; DEFAULT_LEVELS] = [
    "#433700", "#4e4000", "#5c4700", "#6d4d00", "#834f00", "#9c4f00", "#b54c10", "#c84d2b",
    "#db503c", "#ed534e",
];

/// Steps on the coverage strip's scale, above "nothing recorded".
pub const COVERAGE_LEVELS: usize = 5;

/// How much of a facet's calendar an hour was observed on, for a light surface.
///
/// Deliberately neutral grey: coverage is a second, secondary encoding sitting under the
/// same plot as the likelihood heat, and a hue there would read as another probability.
pub const COVERAGE_LIGHT: [&str; COVERAGE_LEVELS] =
    ["#e8e7e0", "#d3d2c8", "#b9b8ac", "#9d9b8f", "#7e7c72"];

/// The same scale stepped for a dark surface.
pub const COVERAGE_DARK: [&str; COVERAGE_LEVELS] =
    ["#2c2c2a", "#3d3d3a", "#52514e", "#6a6963", "#898781"];

/// Which coverage step a share of the calendar lands on, or `None` for nothing observed.
pub fn coverage_level(observed: u32, possible: u32) -> Option<usize> {
    if observed == 0 {
        return None;
    }
    if possible == 0 {
        // Nothing to compare against; show it as fully covered rather than inventing a
        // fraction, because the day count in the tooltip is the real answer.
        return Some(COVERAGE_LEVELS - 1);
    }
    let share = (f64::from(observed) / f64::from(possible)).clamp(0.0, 1.0);
    let index = (share * COVERAGE_LEVELS as f64).ceil() as usize;
    Some(index.clamp(1, COVERAGE_LEVELS) - 1)
}

/// Interpolate the reference ramp to `levels` steps.
///
/// The ramps above are authored at ten steps; asking for more or fewer resamples them
/// rather than inventing new hues.
pub fn ramp(levels: usize, dark: bool) -> Vec<String> {
    let reference = if dark { &HEAT_DARK } else { &HEAT_LIGHT };
    let levels = levels.clamp(MIN_LEVELS, MAX_LEVELS);
    (0..levels)
        .map(|index| {
            let position = if levels == 1 {
                0.0
            } else {
                index as f64 / (levels - 1) as f64
            } * (reference.len() - 1) as f64;
            let lower = position.floor() as usize;
            let upper = (lower + 1).min(reference.len() - 1);
            mix(reference[lower], reference[upper], position - lower as f64)
        })
        .collect()
}

/// Blend two `#rrggbb` colours in sRGB.
fn mix(from: &str, to: &str, t: f64) -> String {
    let (from, to) = (parse_hex(from), parse_hex(to));
    let t = t.clamp(0.0, 1.0);
    let channel = |a: u8, b: u8| (f64::from(a) + (f64::from(b) - f64::from(a)) * t).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}",
        channel(from.0, to.0),
        channel(from.1, to.1),
        channel(from.2, to.2)
    )
}

fn parse_hex(hex: &str) -> (u8, u8, u8) {
    let digits = hex.trim_start_matches('#');
    let value = u32::from_str_radix(digits, 16).unwrap_or(0);
    (
        ((value >> 16) & 0xff) as u8,
        ((value >> 8) & 0xff) as u8,
        (value & 0xff) as u8,
    )
}

/// Which colour level a probability lands in, or `None` when it is below
/// `min_probability` and should be left blank.
///
/// `gamma` shapes the mapping: below 1 it stretches the low end, so a 5% chance of
/// 4 kW at 15:00 is still visible instead of disappearing into the lightest step.
pub fn level(probability: f64, levels: usize, gamma: f64, min_probability: f64) -> Option<usize> {
    if !probability.is_finite() || probability <= 0.0 || probability < min_probability {
        return None;
    }
    let levels = levels.clamp(MIN_LEVELS, MAX_LEVELS);
    let gamma = if gamma.is_finite() && gamma > 0.0 {
        gamma
    } else {
        1.0
    };
    let shaped = probability.clamp(0.0, 1.0).powf(gamma);
    let index = (shaped * levels as f64).floor() as usize;
    Some(index.min(levels - 1))
}

/// The probability at which each level starts, for the legend.
pub fn level_edges(levels: usize, gamma: f64) -> Vec<f64> {
    let levels = levels.clamp(MIN_LEVELS, MAX_LEVELS);
    let gamma = if gamma.is_finite() && gamma > 0.0 {
        gamma
    } else {
        1.0
    };
    (0..levels)
        .map(|index| (index as f64 / levels as f64).powf(1.0 / gamma))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ramps_have_the_requested_number_of_steps() {
        assert_eq!(ramp(DEFAULT_LEVELS, false).len(), DEFAULT_LEVELS);
        assert_eq!(ramp(5, false).len(), 5);
        assert_eq!(ramp(16, true).len(), 16);
        // Out of range requests are clamped rather than rejected.
        assert_eq!(ramp(1, false).len(), MIN_LEVELS);
        assert_eq!(ramp(999, false).len(), MAX_LEVELS);
    }

    #[test]
    fn ramps_keep_their_endpoints_when_resampled() {
        for dark in [false, true] {
            let reference = if dark { HEAT_DARK } else { HEAT_LIGHT };
            for levels in [4usize, 7, 10, 13] {
                let ramp = ramp(levels, dark);
                assert_eq!(ramp[0], reference[0]);
                assert_eq!(ramp[ramp.len() - 1], reference[reference.len() - 1]);
                assert!(ramp.iter().all(|colour| colour.len() == 7));
                assert!(ramp.iter().all(|colour| colour.starts_with('#')));
            }
        }
    }

    /// Relative luminance, used to assert the ramps stay monotone.
    fn luminance(hex: &str) -> f64 {
        let (r, g, b) = parse_hex(hex);
        let channel = |value: u8| {
            let value = f64::from(value) / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
    }

    #[test]
    fn the_light_ramp_darkens_as_likelihood_rises() {
        let ramp = ramp(DEFAULT_LEVELS, false);
        for pair in ramp.windows(2) {
            assert!(
                luminance(&pair[0]) > luminance(&pair[1]),
                "{} should be lighter than {}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn the_dark_ramp_brightens_as_likelihood_rises() {
        // On a dark surface the scale has to run the other way so that likely cells are
        // the ones standing out from the page.
        let ramp = ramp(DEFAULT_LEVELS, true);
        for pair in ramp.windows(2) {
            assert!(
                luminance(&pair[0]) < luminance(&pair[1]),
                "{} should be darker than {}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn the_coverage_ramps_are_monotone_in_both_themes() {
        for pair in COVERAGE_LIGHT.windows(2) {
            assert!(
                luminance(pair[0]) > luminance(pair[1]),
                "{} should be lighter than {}",
                pair[0],
                pair[1]
            );
        }
        for pair in COVERAGE_DARK.windows(2) {
            assert!(
                luminance(pair[0]) < luminance(pair[1]),
                "{} should be darker than {}",
                pair[0],
                pair[1]
            );
        }
    }

    /// How far apart a colour's channels are: near zero is grey, large is saturated.
    fn channel_spread(hex: &str) -> u8 {
        let (r, g, b) = parse_hex(hex);
        r.max(g).max(b) - r.min(g).min(b)
    }

    #[test]
    fn the_coverage_ramp_stays_far_less_saturated_than_the_heat_ramp() {
        // Coverage sits directly under the heatmap, so it must never read as another
        // likelihood. These are the palette's warm greys: a hint of warmth, no hue.
        let coverage_max = COVERAGE_LIGHT
            .iter()
            .chain(COVERAGE_DARK.iter())
            .map(|hex| channel_spread(hex))
            .max()
            .expect("the ramps are not empty");
        let heat_min = HEAT_LIGHT
            .iter()
            .chain(HEAT_DARK.iter())
            .map(|hex| channel_spread(hex))
            .min()
            .expect("the ramps are not empty");

        assert!(
            coverage_max <= 20,
            "coverage is too colourful ({coverage_max})"
        );
        assert!(
            heat_min > coverage_max * 2,
            "the heat ramp ({heat_min}) must be unmistakably more saturated than the \
             coverage ramp ({coverage_max})"
        );
    }

    #[test]
    fn coverage_levels_follow_the_share_of_the_calendar() {
        assert_eq!(coverage_level(0, 30), None, "nothing observed");
        assert_eq!(coverage_level(1, 30), Some(0), "a sliver");
        assert_eq!(
            coverage_level(30, 30),
            Some(COVERAGE_LEVELS - 1),
            "all of it"
        );
        assert_eq!(coverage_level(15, 30), Some(2), "half way up the scale");
        // More days than the calendar allows (pooled years) still tops out.
        assert_eq!(coverage_level(90, 30), Some(COVERAGE_LEVELS - 1));
        // Without a denominator the day count in the tooltip is the answer.
        assert_eq!(coverage_level(4, 0), Some(COVERAGE_LEVELS - 1));
    }

    #[test]
    fn coverage_levels_never_decrease_with_coverage() {
        let mut previous = 0;
        for observed in 1..=30 {
            let level = coverage_level(observed, 30).unwrap();
            assert!(level >= previous, "level fell at {observed} days");
            previous = level;
        }
    }

    #[test]
    fn levels_span_the_probability_range() {
        assert_eq!(level(1.0, 10, 1.0, 0.0), Some(9));
        assert_eq!(level(0.95, 10, 1.0, 0.0), Some(9));
        assert_eq!(level(0.5, 10, 1.0, 0.0), Some(5));
        assert_eq!(level(0.05, 10, 1.0, 0.0), Some(0));
    }

    #[test]
    fn levels_never_decrease_with_probability() {
        let mut previous = 0;
        for step in 0..=100 {
            let probability = f64::from(step) / 100.0;
            let level = level(probability, 10, 0.6, 0.0).unwrap_or(0);
            assert!(level >= previous, "level fell at p={probability}");
            previous = level;
        }
    }

    #[test]
    fn blank_cells_are_reported_as_none() {
        assert_eq!(level(0.0, 10, 0.6, 0.0), None);
        assert_eq!(level(0.001, 10, 0.6, 0.005), None);
        assert_eq!(level(f64::NAN, 10, 0.6, 0.0), None);
        assert_eq!(level(-0.5, 10, 0.6, 0.0), None);
        assert!(level(0.005, 10, 0.6, 0.005).is_some());
    }

    #[test]
    fn gamma_below_one_lifts_rare_cells_out_of_the_background() {
        let plain = level(0.05, 10, 1.0, 0.0).unwrap();
        let lifted = level(0.05, 10, 0.6, 0.0).unwrap();
        assert!(
            lifted > plain,
            "gamma 0.6 gave {lifted}, gamma 1.0 gave {plain}"
        );
        // A certainty stays at the top of the ramp whatever gamma does.
        assert_eq!(level(1.0, 10, 0.6, 0.0), Some(9));
    }

    #[test]
    fn a_nonsensical_gamma_falls_back_to_linear() {
        assert_eq!(level(0.5, 10, 0.0, 0.0), level(0.5, 10, 1.0, 0.0));
        assert_eq!(level(0.5, 10, f64::NAN, 0.0), level(0.5, 10, 1.0, 0.0));
    }

    #[test]
    fn level_edges_match_the_level_mapping() {
        for gamma in [0.5, 0.6, 1.0, 1.5] {
            let edges = level_edges(10, gamma);
            assert_eq!(edges.len(), 10);
            assert_eq!(edges[0], 0.0);
            for pair in edges.windows(2) {
                assert!(pair[0] < pair[1], "edges not increasing: {edges:?}");
            }
            // A probability just above an edge belongs to that edge's level.
            for (index, edge) in edges.iter().enumerate().skip(1) {
                assert_eq!(
                    level(edge + 1e-9, 10, gamma, 0.0),
                    Some(index),
                    "edge {edge} for gamma {gamma}"
                );
            }
        }
    }
}
