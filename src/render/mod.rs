//! Turning an [`crate::aggregate::Analysis`] into a self-contained HTML page.

pub mod color;
pub mod html;
pub mod layout;
pub mod payback;
pub mod svg;

pub use html::{PageOptions, describe_window, page};
pub use payback::{PaybackOptions, page as payback_page};

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

/// Format an amount of energy for a label: `4.2 kWh`, `420 kWh`, `3.7 MWh`.
pub fn format_kwh(kwh: f64) -> String {
    if !kwh.is_finite() {
        return "-".to_string();
    }
    if kwh.abs() < 10.0 {
        return format!("{} kWh", trim_zeros(&format!("{kwh:.2}")));
    }
    if kwh.abs() < 1_000.0 {
        return format!("{} kWh", group_thousands(&format!("{kwh:.0}")));
    }
    format!("{} MWh", trim_zeros(&format!("{:.2}", kwh / 1_000.0)))
}

/// Format money for a label: `0.35 EUR`, `650 EUR`, `6,500 EUR`.
///
/// The symbol follows the number, which is how most of Europe writes it and reads
/// tolerably everywhere else.
pub fn format_money(amount: f64, currency: &str) -> String {
    if !amount.is_finite() {
        return format!("- {currency}");
    }
    let number = if amount.abs() < 10.0 {
        trim_zeros(&format!("{amount:.2}"))
    } else {
        group_thousands(&format!("{amount:.0}"))
    };
    format!("{number} {currency}")
}

/// Format a payback period: `8.4 years`, or what it means when there is not one.
pub fn format_years(years: Option<f64>) -> String {
    match years {
        None => "never".to_string(),
        Some(years) if !years.is_finite() || years >= 100.0 => "over 100 years".to_string(),
        Some(years) if years < 1.0 => {
            format!("{} months", trim_zeros(&format!("{:.1}", years * 12.0)))
        }
        Some(years) => format!("{} years", trim_zeros(&format!("{years:.1}"))),
    }
}

/// Group the integer part of a formatted number in threes: `6500` becomes `6,500`.
fn group_thousands(text: &str) -> String {
    let (sign, digits) = match text.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", text),
    };
    let (integer, rest) = digits.split_once('.').unwrap_or((digits, ""));
    let mut grouped = String::with_capacity(integer.len() + integer.len() / 3);
    for (index, character) in integer.chars().enumerate() {
        if index > 0 && (integer.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(character);
    }
    if rest.is_empty() {
        format!("{sign}{grouped}")
    } else {
        format!("{sign}{grouped}.{rest}")
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
    fn formats_energy_readably() {
        assert_eq!(format_kwh(0.0), "0 kWh");
        assert_eq!(format_kwh(4.25), "4.25 kWh");
        assert_eq!(format_kwh(9.5), "9.5 kWh");
        assert_eq!(format_kwh(420.0), "420 kWh");
        assert_eq!(format_kwh(3_700.0), "3.7 MWh");
        assert_eq!(format_kwh(f64::NAN), "-");
    }

    #[test]
    fn formats_money_with_the_currency_after_it() {
        assert_eq!(format_money(0.35, "EUR"), "0.35 EUR");
        assert_eq!(format_money(650.0, "EUR"), "650 EUR");
        assert_eq!(format_money(6_500.0, "EUR"), "6,500 EUR");
        assert_eq!(format_money(1_234_567.0, "USD"), "1,234,567 USD");
        assert_eq!(format_money(-250.0, "EUR"), "-250 EUR");
        assert_eq!(format_money(f64::INFINITY, "EUR"), "- EUR");
    }

    #[test]
    fn formats_a_payback_period_including_the_absence_of_one() {
        assert_eq!(format_years(Some(8.4)), "8.4 years");
        assert_eq!(format_years(Some(12.0)), "12 years");
        assert_eq!(format_years(Some(0.5)), "6 months");
        assert_eq!(format_years(Some(250.0)), "over 100 years");
        assert_eq!(format_years(Some(f64::INFINITY)), "over 100 years");
        assert_eq!(format_years(None), "never");
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
