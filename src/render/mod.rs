//! Turning an [`crate::aggregate::Analysis`] into a self-contained HTML page.

pub mod color;
pub mod html;
pub mod svg;

pub use html::{PageOptions, describe_window, page};

/// Escape text for inclusion in HTML or SVG markup.
pub fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// Format a power reading for a label: `0 W`, `950 W`, `1.5 kW`.
pub fn format_watts(watts: f64) -> String {
    if !watts.is_finite() {
        return "-".to_string();
    }
    if watts.abs() < 1_000.0 {
        return format!("{} W", trim_zeros(&format!("{watts:.0}")));
    }
    format!("{} kW", trim_zeros(&format!("{:.2}", watts / 1_000.0)))
}

/// Format a probability for a label: `100%`, `62%`, `6.2%`, `0.45%`.
pub fn format_percent(probability: f64) -> String {
    if !probability.is_finite() {
        return "-".to_string();
    }
    let percent = probability * 100.0;
    if percent >= 10.0 || percent == 0.0 {
        format!("{percent:.0}%")
    } else if percent >= 1.0 {
        format!("{}%", trim_zeros(&format!("{percent:.1}")))
    } else {
        format!("{}%", trim_zeros(&format!("{percent:.2}")))
    }
}

/// Format a duration in seconds as `3 d 4 h`, for the report header.
pub fn format_duration(seconds: f64) -> String {
    if !seconds.is_finite() || seconds <= 0.0 {
        return "0 h".to_string();
    }
    let hours = seconds / 3_600.0;
    if hours < 48.0 {
        return format!("{} h", trim_zeros(&format!("{hours:.1}")));
    }
    let days = (hours / 24.0).floor();
    let rest = hours - days * 24.0;
    if rest < 0.05 {
        format!("{days:.0} d")
    } else {
        format!("{days:.0} d {} h", trim_zeros(&format!("{rest:.1}")))
    }
}

fn trim_zeros(text: &str) -> String {
    if !text.contains('.') {
        return text.to_string();
    }
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_markup_sensitive_characters() {
        assert_eq!(escape("sensor.pv"), "sensor.pv");
        assert_eq!(escape("<script>&\"'"), "&lt;script&gt;&amp;&quot;&#39;");
        assert_eq!(escape("Grün & groß"), "Grün &amp; groß");
    }

    #[test]
    fn formats_power_readably() {
        assert_eq!(format_watts(0.0), "0 W");
        assert_eq!(format_watts(50.0), "50 W");
        assert_eq!(format_watts(950.0), "950 W");
        assert_eq!(format_watts(1_000.0), "1 kW");
        assert_eq!(format_watts(1_050.0), "1.05 kW");
        assert_eq!(format_watts(1_500.0), "1.5 kW");
        assert_eq!(format_watts(8_000.0), "8 kW");
        assert_eq!(format_watts(f64::NAN), "-");
    }

    #[test]
    fn formats_probabilities_with_useful_precision() {
        assert_eq!(format_percent(1.0), "100%");
        assert_eq!(format_percent(0.62), "62%");
        assert_eq!(format_percent(0.062), "6.2%");
        assert_eq!(format_percent(0.0045), "0.45%");
        assert_eq!(format_percent(0.0), "0%");
        assert_eq!(format_percent(f64::INFINITY), "-");
    }

    #[test]
    fn formats_observation_time() {
        assert_eq!(format_duration(0.0), "0 h");
        assert_eq!(format_duration(3_600.0), "1 h");
        assert_eq!(format_duration(5_400.0), "1.5 h");
        assert_eq!(format_duration(86_400.0 * 3.0), "3 d");
        assert_eq!(format_duration(86_400.0 * 3.0 + 5_400.0), "3 d 1.5 h");
    }
}
