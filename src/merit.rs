//! Merit-order building blocks.
//!
//! The exchange is a pure continuous double auction; **merit order is not coded
//! into lowlat, it emerges from the limit prices agents post.** This module
//! holds the economics that decide those limit prices: the short-run marginal
//! cost (SRMC) of a thermal unit, and the price-grid helpers every maker needs.
//!
//! Supply stack, ascending SRMC (who sets the price at the margin):
//!   renewables (~0)  <  battery (dynamic)  <  gas peaker (~230 EUR/MWh).
//!
//! `230 EUR/MWh = 23 ct/kWh` is the deliberate price ceiling: when the cheaper
//! stack is exhausted, the residual is served by the peaker and the clearing
//! price walks up to roughly this level.

use crate::wire::TICK;

/// A gas unit's short-run marginal cost, decomposed the way a real dispatch
/// desk would: fuel / efficiency + CO2 + variable O&M.
#[derive(Debug, Clone, Copy)]
pub struct GasSrmc {
    /// Fuel price, EUR per MWh of *thermal* energy.
    pub fuel_eur_per_mwh_th: f64,
    /// Electrical efficiency in (0, 1].
    pub efficiency: f64,
    /// CO2 emitted per MWh of *electric* output, tonnes.
    pub co2_t_per_mwh_e: f64,
    /// CO2 allowance price, EUR per tonne.
    pub co2_eur_per_t: f64,
    /// Variable O&M, EUR per MWh of electric output.
    pub variable_om_eur_per_mwh: f64,
}

impl GasSrmc {
    /// SRMC in EUR/MWh of electricity.
    pub fn eur_per_mwh(&self) -> f64 {
        self.fuel_eur_per_mwh_th / self.efficiency
            + self.co2_t_per_mwh_e * self.co2_eur_per_t
            + self.variable_om_eur_per_mwh
    }
}

/// Round a EUR/MWh price to the exchange tick grid (0.10 EUR/MWh, nearest).
pub fn round_to_tick(price_eur: f64) -> f64 {
    let scaled = price_eur * crate::wire::PRICE_SCALE as f64;
    let tick = TICK as f64;
    (scaled / tick).round() * tick / crate::wire::PRICE_SCALE as f64
}

/// Round *down* to the tick grid (used for the sell side so an ask never dips
/// below the intended floor).
pub fn floor_to_tick(price_eur: f64) -> f64 {
    let scaled = price_eur * crate::wire::PRICE_SCALE as f64;
    let tick = TICK as f64;
    (scaled / tick).floor() * tick / crate::wire::PRICE_SCALE as f64
}

/// Round *up* to the tick grid (used for the sell side so an ask never dips
/// below the intended floor).
pub fn ceil_to_tick(price_eur: f64) -> f64 {
    let scaled = price_eur * crate::wire::PRICE_SCALE as f64;
    let tick = TICK as f64;
    (scaled / tick).ceil() * tick / crate::wire::PRICE_SCALE as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srmc_is_fuel_over_efficiency_plus_co2_plus_om() {
        let p = GasSrmc {
            fuel_eur_per_mwh_th: 40.0,
            efficiency: 0.5,
            co2_t_per_mwh_e: 0.4,
            co2_eur_per_t: 90.0,
            variable_om_eur_per_mwh: 6.0,
        };
        // 40/0.5 = 80 fuel + 0.4*90 = 36 CO2 + 6 O&M = 122
        assert!((p.eur_per_mwh() - 122.0).abs() < 1e-9);
    }

    #[test]
    fn a_plausible_ocgt_recipe_lands_in_the_peaker_band() {
        // A tight-market OCGT: expensive gas, low efficiency, CO2-heavy. This is
        // the recipe that justifies a ~23 ct/kWh backstop rather than a CCGT.
        let p = GasSrmc {
            fuel_eur_per_mwh_th: 70.0,
            efficiency: 0.38,
            co2_t_per_mwh_e: 0.55,
            co2_eur_per_t: 95.0,
            variable_om_eur_per_mwh: 8.0,
        };
        let s = p.eur_per_mwh();
        assert!(s > 150.0 && s < 320.0, "peaker SRMC out of band: {s}");
    }

    #[test]
    fn tick_rounding_lands_on_the_grid() {
        assert_eq!(round_to_tick(230.04), 230.0);
        assert_eq!(round_to_tick(230.06), 230.1);
        assert_eq!(floor_to_tick(230.09), 230.0);
        // the scaled values must be whole multiples of the tick
        let scaled = (round_to_tick(123.47) * 100.0).round() as i64;
        assert_eq!(scaled % TICK, 0);
    }

    #[test]
    fn negative_prices_round_to_the_grid_too() {
        assert_eq!(round_to_tick(-49.96), -50.0);
        assert_eq!((round_to_tick(-49.96) * 100.0).round() as i64 % TICK, 0);
    }
}
