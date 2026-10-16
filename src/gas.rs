//! Gas turbines — the dispatchable, price-setting tail of the merit order.
//!
//! Each turbine is an independent, **economical** unit: it offers at its
//! short-run marginal cost (SRMC) and nothing below it, so it can never sell at
//! a loss. Because every turbine posts its own SRMC, the exchange's
//! price–time priority sorts them automatically — the cheapest (a modern CCGT)
//! runs first, a dirty OCGT only clears at the top, and a peaking unit is the
//! last resort. That *is* the German merit order.
//!
//! Nothing here is a feed-in tariff: a turbine earns only when the clearing
//! price exceeds its cost.

use crate::merit::{ceil_to_tick, GasSrmc};
use crate::wire::{eur_to_price, mw_to_qty};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurbineKind {
    /// Combined cycle: high efficiency, low SRMC, runs as baseload/mid-merit.
    Ccgt,
    /// Open cycle: lower efficiency, higher SRMC, peaking.
    Ocgt,
}

#[derive(Debug, Clone, Copy)]
pub struct GasConfig {
    /// Short-run marginal cost, EUR/MWh. May be given directly or derived from a
    /// `GasSrmc` decomposition via `GasPeaker::from_srmc`.
    pub srmc_eur_per_mwh: f64,
    /// Commercial margin added on top of SRMC, EUR/MWh.
    pub margin_eur_per_mwh: f64,
    /// Offered capacity, MW.
    pub capacity_mw: f64,
    /// If the best ask already sits above this, add the scarcity adder.
    pub scarcity_threshold_eur: f64,
    /// Adder applied when the book is tight, EUR/MWh.
    pub scarcity_adder_eur: f64,
}

impl Default for GasConfig {
    fn default() -> Self {
        GasConfig {
            srmc_eur_per_mwh: 230.0, // 23 ct/kWh — the expensive backstop
            margin_eur_per_mwh: 0.0,
            capacity_mw: 20.0,
            scarcity_threshold_eur: 500.0,
            scarcity_adder_eur: 0.0,
        }
    }
}

pub struct GasPeaker {
    pub id: String,
    pub kind: TurbineKind,
    pub cfg: GasConfig,
    /// Latest best-ask seen on the public book (for the scarcity premium).
    pub best_ask_eur: Option<f64>,
}

impl GasPeaker {
    pub fn new(id: &str, kind: TurbineKind, cfg: GasConfig) -> Self {
        GasPeaker {
            id: id.to_string(),
            kind,
            cfg,
            best_ask_eur: None,
        }
    }

    /// Build a turbine whose SRMC is *derived* from its physical cost drivers
    /// (fuel / efficiency + CO2 + variable O&M) — the way a real dispatch desk
    /// would compute its offer.
    pub fn from_srmc(id: &str, kind: TurbineKind, capacity_mw: f64, srmc: GasSrmc) -> Self {
        GasPeaker::new(
            id,
            kind,
            GasConfig {
                srmc_eur_per_mwh: srmc.eur_per_mwh(),
                capacity_mw,
                ..GasConfig::default()
            },
        )
    }

    pub fn observe_best_ask(&mut self, ask_eur: f64) {
        if ask_eur > 0.0 {
            self.best_ask_eur = Some(ask_eur);
        }
    }

    /// The price this unit offers at, EUR/MWh. Never below SRMC (rounded *up* to
    /// the tick so the offer is always ≥ cost).
    pub fn ask_price_eur(&self) -> f64 {
        let mut p = self.cfg.srmc_eur_per_mwh + self.cfg.margin_eur_per_mwh;
        if let Some(best) = self.best_ask_eur {
            if best > self.cfg.scarcity_threshold_eur {
                p += self.cfg.scarcity_adder_eur;
            }
        }
        ceil_to_tick(p)
    }

    /// The resting ask as exchange units: (scaled price, scaled quantity).
    pub fn quote(&self) -> (i64, i64) {
        (
            eur_to_price(self.ask_price_eur()),
            mw_to_qty(self.cfg.capacity_mw),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::TICK;

    fn ocgt_default() -> GasPeaker {
        GasPeaker::new("ocgt-peak", TurbineKind::Ocgt, GasConfig::default())
    }

    #[test]
    fn default_ask_is_23_ct_per_kwh() {
        let g = ocgt_default();
        assert_eq!(g.ask_price_eur(), 230.0);
        let (price, qty) = g.quote();
        assert_eq!(price, 23000);
        assert_eq!(price % TICK, 0, "must sit on the tick grid");
        assert_eq!(qty, 200); // 20 MW scaled by 10
    }

    #[test]
    fn never_offers_below_its_srmc() {
        // A price that is not already on the tick grid must be rounded *up*.
        let g = GasPeaker::new(
            "ccgt",
            TurbineKind::Ccgt,
            GasConfig {
                srmc_eur_per_mwh: 99.97,
                ..GasConfig::default()
            },
        );
        assert!(g.ask_price_eur() >= 99.97);
        assert_eq!(g.ask_price_eur(), 100.0);
    }

    #[test]
    fn srmc_is_derived_from_cost_drivers() {
        let g = GasPeaker::from_srmc(
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
        );
        // 35/0.58 = 60.34 + 0.35*90 = 31.5 + 3 = 94.84 → ceil to 94.9
        assert_eq!(g.ask_price_eur(), 94.9);
    }

    #[test]
    fn margin_moves_the_ask_up_but_stays_on_tick() {
        let g = GasPeaker::new(
            "ccgt",
            TurbineKind::Ccgt,
            GasConfig {
                margin_eur_per_mwh: 12.34,
                ..GasConfig::default()
            },
        );
        assert_eq!(g.ask_price_eur(), 242.4);
    }

    #[test]
    fn scarcity_adder_only_triggers_on_a_tight_book() {
        let mut g = GasPeaker::new(
            "ccgt",
            TurbineKind::Ccgt,
            GasConfig {
                scarcity_threshold_eur: 500.0,
                scarcity_adder_eur: 100.0,
                ..GasConfig::default()
            },
        );
        g.observe_best_ask(100.0);
        assert_eq!(g.ask_price_eur(), 230.0, "calm book → no adder");
        g.observe_best_ask(800.0);
        assert_eq!(g.ask_price_eur(), 330.0, "tight book → adder");
    }
}
