//! The competing supply fleet.
//!
//! "gasbattery" is not one battery and one turbine: it is a **portfolio** of
//! independently-economical units that compete with each other and with the
//! city's cheap renewables. The default fleet is a deliberately small but
//! realistic German-style stack:
//!
//!   renewables (~0, from the city, price-exposed)
//!     < batteries (opportunity cost, dynamic)
//!       < CCGT-1 (~95)  < CCGT-2 (~104)
//!         < OCGT-1 (~145)  < OCGT-2 (~213)  < peaker (~230)
//!
//! Each unit offers at its own cost, so lowlat's price–time priority produces
//! the merit order for free. Nothing receives a feed-in tariff.

use crate::battery::{Battery, BatteryConfig};
use crate::gas::{GasConfig, GasPeaker, TurbineKind};
use crate::merit::GasSrmc;
use crate::wire::price_to_eur;

pub struct Fleet {
    pub batteries: Vec<Battery>,
    pub turbines: Vec<GasPeaker>,
}

impl Fleet {
    /// A small but representative German supply portfolio: three batteries of
    /// different sizes and five gas turbines spanning CCGT → OCGT → peaker.
    pub fn default_german() -> Self {
        let turbines = vec![
            // Modern CCGTs: efficient, cheap, mid-merit.
            GasPeaker::from_srmc(
                "ccgt-1",
                TurbineKind::Ccgt,
                100.0,
                GasSrmc {
                    fuel_eur_per_mwh_th: 35.0,
                    efficiency: 0.58,
                    co2_t_per_mwh_e: 0.35,
                    co2_eur_per_t: 90.0,
                    variable_om_eur_per_mwh: 3.0,
                },
            ),
            GasPeaker::from_srmc(
                "ccgt-2",
                TurbineKind::Ccgt,
                80.0,
                GasSrmc {
                    fuel_eur_per_mwh_th: 38.0,
                    efficiency: 0.55,
                    co2_t_per_mwh_e: 0.35,
                    co2_eur_per_t: 90.0,
                    variable_om_eur_per_mwh: 3.0,
                },
            ),
            // Open-cycle gas: peaking, dirtier, dearer.
            GasPeaker::from_srmc(
                "ocgt-1",
                TurbineKind::Ocgt,
                60.0,
                GasSrmc {
                    fuel_eur_per_mwh_th: 40.0,
                    efficiency: 0.40,
                    co2_t_per_mwh_e: 0.45,
                    co2_eur_per_t: 90.0,
                    variable_om_eur_per_mwh: 4.0,
                },
            ),
            GasPeaker::from_srmc(
                "ocgt-2",
                TurbineKind::Ocgt,
                40.0,
                GasSrmc {
                    fuel_eur_per_mwh_th: 55.0,
                    efficiency: 0.35,
                    co2_t_per_mwh_e: 0.55,
                    co2_eur_per_t: 90.0,
                    variable_om_eur_per_mwh: 6.0,
                },
            ),
            // The deliberately expensive last-resort peaker (23 ct/kWh) that the
            // user specified: only economics makes it run.
            GasPeaker::new("peaker", TurbineKind::Ocgt, GasConfig::default()),
        ];

        let mk = |cap_mwh: f64, power_mw: f64, rt: f64, half: f64| {
            Battery::new(BatteryConfig {
                capacity_mwh: cap_mwh,
                power_mw,
                round_trip_eff: rt,
                half_spread_eur: half,
                initial_fair_eur: 95.0,
                ..BatteryConfig::default()
            })
        };
        let batteries = vec![
            mk(1.0, 1.0, 0.90, 4.0),
            mk(5.0, 2.5, 0.90, 3.0),
            mk(20.0, 5.0, 0.88, 2.0),
        ];

        Fleet {
            batteries,
            turbines,
        }
    }

    /// The offered prices of every dispatchable unit, ascending — the stack the
    /// exchange's priority walk traverses. Turbines are fixed; batteries use
    /// their current ask.
    pub fn merit_stack(&self) -> Vec<(String, f64)> {
        let mut stack: Vec<(String, f64)> = Vec::new();
        for t in &self.turbines {
            stack.push((t.id.clone(), t.ask_price_eur()));
        }
        for (i, b) in self.batteries.iter().enumerate() {
            let ask = price_to_eur(b.quote().ask_price);
            stack.push((format!("battery-{i}"), ask));
        }
        stack.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        stack
    }

    /// Turbines that would be dispatched at a given clearing price: those whose
    /// SRMC is at or below it (the cheapest run first).
    pub fn dispatched_turbines(&self, clearing_eur: f64) -> Vec<String> {
        let mut running: Vec<(String, f64)> = self
            .turbines
            .iter()
            .filter(|t| t.ask_price_eur() <= clearing_eur)
            .map(|t| (t.id.clone(), t.ask_price_eur()))
            .collect();
        running.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        running.into_iter().map(|(id, _)| id).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_fleet_has_several_batteries_and_turbines() {
        let f = Fleet::default_german();
        assert!(f.batteries.len() >= 2, "multiple batteries");
        assert!(f.turbines.len() >= 2, "multiple turbines");
    }

    #[test]
    fn turbines_are_strictly_ordered_by_cost() {
        let f = Fleet::default_german();
        let asks: Vec<f64> = f.turbines.iter().map(|t| t.ask_price_eur()).collect();
        let mut sorted = asks.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(asks, sorted, "turbine order must be a merit order");
        // the CCGTs are cheaper than the peaker
        let ccgt1 = f.turbines.iter().find(|t| t.id == "ccgt-1").unwrap();
        let peaker = f.turbines.iter().find(|t| t.id == "peaker").unwrap();
        assert!(ccgt1.ask_price_eur() < peaker.ask_price_eur());
        assert_eq!(peaker.ask_price_eur(), 230.0);
    }

    #[test]
    fn dispatch_is_a_prefix_of_the_stack() {
        let f = Fleet::default_german();
        // At 150 EUR/MWh only the two CCGTs and the first OCGT are economical.
        let at150 = f.dispatched_turbines(150.0);
        assert_eq!(at150, vec!["ccgt-1", "ccgt-2", "ocgt-1"]);
        // At 240 everything runs; at 50 nothing does.
        assert_eq!(f.dispatched_turbines(240.0).len(), f.turbines.len());
        assert!(f.dispatched_turbines(50.0).is_empty());
    }

    #[test]
    fn merit_stack_is_ascending() {
        let f = Fleet::default_german();
        let stack = f.merit_stack();
        assert!(!stack.is_empty());
        for w in stack.windows(2) {
            assert!(w[0].1 <= w[1].1, "stack not ascending: {:?}", w);
        }
    }
}
