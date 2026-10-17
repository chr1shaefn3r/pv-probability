//! What a sensor measures, and how its recorded unit becomes the unit a tool works in.
//!
//! Home Assistant records a statistic in whatever unit the sensor reports: a power sensor
//! in W or kW, a thermometer in °C or °F depending on the installation's unit system.
//! Each tool works in one unit per quantity - watts, degrees Celsius - so every reading
//! passes through a [`Conversion`] on the way in.

/// The physical quantity a tool reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quantity {
    /// Instantaneous power, worked in watts.
    Power,
    /// Temperature, worked in degrees Celsius.
    Temperature,
}

impl Quantity {
    pub const ALL: [Quantity; 2] = [Quantity::Power, Quantity::Temperature];

    /// The unit readings are converted into.
    pub fn unit(self) -> &'static str {
        match self {
            Quantity::Power => "W",
            Quantity::Temperature => "°C",
        }
    }

    /// How a recorded unit converts into [`Quantity::unit`], if it is a unit of this
    /// quantity at all.
    pub fn conversion(self, unit: &str) -> Option<Conversion> {
        match (self, unit.trim()) {
            (Quantity::Power, "W") => Some(Conversion::IDENTITY),
            (Quantity::Power, "kW") => Some(Conversion::factor(1_000.0)),
            (Quantity::Power, "MW") => Some(Conversion::factor(1_000_000.0)),
            (Quantity::Power, "mW") => Some(Conversion::factor(0.001)),
            (Quantity::Temperature, "°C" | "℃") => Some(Conversion::IDENTITY),
            (Quantity::Temperature, "°F" | "℉") => Some(Conversion {
                scale: 5.0 / 9.0,
                offset: -32.0 * 5.0 / 9.0,
            }),
            (Quantity::Temperature, "K") => Some(Conversion {
                scale: 1.0,
                offset: -273.15,
            }),
            _ => None,
        }
    }

    /// The quantity a recorded unit measures, if it is one these tools know.
    pub fn of_unit(unit: &str) -> Option<Quantity> {
        Quantity::ALL
            .into_iter()
            .find(|quantity| quantity.conversion(unit).is_some())
    }

    /// What one sensor of this quantity is called in a sentence.
    pub fn sensor_noun(self) -> &'static str {
        match self {
            Quantity::Power => "power sensor",
            Quantity::Temperature => "temperature sensor",
        }
    }

    /// The name of the quantity itself, in a sentence.
    pub fn name(self) -> &'static str {
        match self {
            Quantity::Power => "power",
            Quantity::Temperature => "temperature",
        }
    }
}

/// A linear conversion into a quantity's working unit: `value * scale + offset`.
///
/// A scale alone covers kW to W; a temperature in °F needs the offset as well.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Conversion {
    pub scale: f64,
    pub offset: f64,
}

impl Conversion {
    /// Readings already in the working unit.
    pub const IDENTITY: Conversion = Conversion {
        scale: 1.0,
        offset: 0.0,
    };

    /// A plain multiplication, as `--scale` asks for.
    pub fn factor(scale: f64) -> Self {
        Self { scale, offset: 0.0 }
    }

    pub fn apply(self, value: f64) -> f64 {
        value * self.scale + self.offset
    }

    pub fn is_identity(self) -> bool {
        self == Self::IDENTITY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_units_convert_to_watts() {
        let watts = |unit| Quantity::Power.conversion(unit).map(|c| c.apply(3.2));
        assert_eq!(watts("W"), Some(3.2));
        assert_eq!(watts(" kW "), Some(3_200.0));
        assert_eq!(watts("MW"), Some(3_200_000.0));
        assert!((watts("mW").unwrap() - 0.0032).abs() < 1e-12);
        assert_eq!(watts("kWh"), None, "an energy total is not power");
        assert_eq!(watts(""), None);
    }

    #[test]
    fn temperature_units_convert_to_celsius() {
        let celsius = |unit, value| {
            Quantity::Temperature
                .conversion(unit)
                .map(|c| c.apply(value))
        };
        assert_eq!(celsius("°C", 21.5), Some(21.5));
        assert_eq!(celsius("℃", -4.0), Some(-4.0));
        assert!((celsius("°F", 212.0).unwrap() - 100.0).abs() < 1e-9);
        assert!((celsius("°F", 32.0).unwrap()).abs() < 1e-9);
        assert!(
            (celsius("°F", -40.0).unwrap() + 40.0).abs() < 1e-9,
            "where the scales meet"
        );
        assert!((celsius("K", 273.15).unwrap()).abs() < 1e-9);
        assert_eq!(celsius("W", 20.0), None);
    }

    #[test]
    fn a_unit_names_its_quantity() {
        assert_eq!(Quantity::of_unit("kW"), Some(Quantity::Power));
        assert_eq!(Quantity::of_unit("°F"), Some(Quantity::Temperature));
        assert_eq!(Quantity::of_unit("kWh"), None);
        assert_eq!(Quantity::of_unit("%"), None);
    }

    #[test]
    fn a_factor_is_a_conversion_without_an_offset() {
        assert!(Conversion::IDENTITY.is_identity());
        assert!(Conversion::factor(1.0).is_identity());
        assert!(!Conversion::factor(1_000.0).is_identity());
        assert_eq!(Conversion::factor(2.0).apply(3.0), 6.0);
    }
}
