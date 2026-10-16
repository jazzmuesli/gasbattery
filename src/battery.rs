//! BatteryMM — the 1 MWh liquidity provider / arbitrageur.
//!
//! Real continuous power markets have no official market maker, but liquidity
//! comes from quoting algos and, increasingly, grid-scale batteries. This is the
//! physical version of that role:
//!
//!   * it keeps a **fair mid** `M` (an EWMA of the public trade tape),
//!   * quotes a **two-sided** book `bid = M - s/2 - skew`, `ask = M + s/2 - skew`,
//!   * the **inventory skew** pushes it to *buy when empty* and *sell when full*,
//!   * it refuses to sell below its round-trip opportunity cost and to buy above
//!     its resale value,
//!   * and it is bounded by real physics: `soc ∈ [0, capacity]` and `|power| ≤ P`.
//!
//! Being a two-sided quoter, it is both a backstop **seller** (discharge) and a
//! backstop **buyer/sink** (charge) — which is why negative prices bottom out at
//! a sensible floor instead of running away.

use crate::merit::round_to_tick;
use crate::wire::{mw_to_qty, qty_to_mw, PRICE_SCALE};

#[derive(Debug, Clone, Copy)]
pub struct BatteryConfig {
    pub capacity_mwh: f64,
    pub power_mw: f64,
    /// Round-trip efficiency in (0, 1].
    pub round_trip_eff: f64,
    /// Half the quoted bid/ask spread, EUR/MWh.
    pub half_spread_eur: f64,
    /// Maximum inventory skew at 0% and 100% SOC, EUR/MWh.
    pub skew_eur: f64,
    /// Fair mid used before the first trade is seen, EUR/MWh.
    pub initial_fair_eur: f64,
    /// EWMA weight on each new trade print (0, 1].
    pub ewma_alpha: f64,
    /// The delivery slot the quote is sized for, in hours.
    pub slot_hours: f64,
}

impl Default for BatteryConfig {
    fn default() -> Self {
        BatteryConfig {
            capacity_mwh: 1.0,
            power_mw: 1.0,
            round_trip_eff: 0.90,
            half_spread_eur: 3.0,
            skew_eur: 20.0,
            initial_fair_eur: 80.0,
            ewma_alpha: 0.25,
            slot_hours: 1.0 / 60.0, // 1-minute slots
        }
    }
}

/// A quote in exchange units (scaled price, scaled quantity). A zero quantity
/// means "do not quote that side" (e.g. an empty battery will not sell).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quote {
    pub bid_price: i64,
    pub ask_price: i64,
    pub bid_quantity: i64,
    pub ask_quantity: i64,
}

pub struct Battery {
    pub cfg: BatteryConfig,
    pub soc_mwh: f64,
    pub fair_eur: f64,
    pub cash_eur: f64,
    /// Energy-weighted average purchase price, EUR/MWh (0 until it first buys).
    pub avg_buy_eur: f64,
    bought_mwh: f64,
    pub trades: u64,
}

impl Battery {
    pub fn new(cfg: BatteryConfig) -> Self {
        let soc = cfg.capacity_mwh * 0.5;
        Battery {
            cfg,
            soc_mwh: soc,
            fair_eur: cfg.initial_fair_eur,
            cash_eur: 0.0,
            avg_buy_eur: 0.0,
            bought_mwh: 0.0,
            trades: 0,
        }
    }

    fn one_way_eff(&self) -> f64 {
        self.cfg.round_trip_eff.sqrt().clamp(0.5, 1.0)
    }

    /// Update the fair mid from a public trade print.
    pub fn observe_trade(&mut self, price_eur: f64) {
        let a = self.cfg.ewma_alpha.clamp(0.001, 1.0);
        self.fair_eur = (1.0 - a) * self.fair_eur + a * price_eur;
    }

    /// The two-sided quote for the current inventory and fair mid.
    ///
    /// Economical by construction: it never bids at or above its fair value and
    /// never asks at or below it, so every round trip captures at least the
    /// spread, and it never discharges below its own (loss-adjusted) cost basis.
    pub fn quote(&self) -> Quote {
        let inv = if self.cfg.capacity_mwh > 0.0 {
            (self.soc_mwh / self.cfg.capacity_mwh).clamp(0.0, 1.0)
        } else {
            0.5
        };
        // -skew when empty (keen to buy), +skew when full (keen to sell). The
        // skew only nudges the centre; the clamps below keep the quotes honest.
        let skew = self.cfg.skew_eur * (2.0 * inv - 1.0);
        let tick_eur = crate::wire::TICK as f64 / PRICE_SCALE as f64;

        // Never buy at/above fair, never sell at/below fair.
        let mut bid = (self.fair_eur - self.cfg.half_spread_eur - skew).min(self.fair_eur - tick_eur);
        let mut ask = (self.fair_eur + self.cfg.half_spread_eur - skew).max(self.fair_eur + tick_eur);

        // Do not sell below the round-trip opportunity cost of what it holds:
        // it must cover purchase price / round-trip efficiency.
        if self.bought_mwh > 0.0 {
            let floor = self.avg_buy_eur / self.cfg.round_trip_eff;
            if ask < floor {
                ask = floor;
            }
        }
        // Keep bid strictly below ask.
        if bid >= ask {
            bid = ask - tick_eur;
        }
        bid = round_to_tick(bid);
        ask = round_to_tick(ask);

        // Size: at most the power cap, and never beyond what the inventory can
        // absorb / deliver.
        let buy_mw = if self.cfg.slot_hours > 0.0 {
            self.cfg
                .power_mw
                .min((self.cfg.capacity_mwh - self.soc_mwh).max(0.0) / self.cfg.slot_hours)
        } else {
            0.0
        };
        let sell_mw = if self.cfg.slot_hours > 0.0 {
            self.cfg
                .power_mw
                .min(self.soc_mwh.max(0.0) / self.cfg.slot_hours)
        } else {
            0.0
        };
        let min_mw = 1.0 / (crate::wire::QTY_SCALE as f64); // one lot = 0.1 MW
        Quote {
            bid_price: (bid * PRICE_SCALE as f64).round() as i64,
            ask_price: (ask * PRICE_SCALE as f64).round() as i64,
            bid_quantity: if buy_mw >= min_mw { mw_to_qty(buy_mw) } else { 0 },
            ask_quantity: if sell_mw >= min_mw { mw_to_qty(sell_mw) } else { 0 },
        }
    }

    /// Apply a fill. `bought` is from the battery's perspective.
    pub fn on_fill(&mut self, bought: bool, quantity_mwh: f64, price_eur: f64) {
        let eff = self.one_way_eff();
        if bought {
            let stored = quantity_mwh * eff;
            self.soc_mwh = (self.soc_mwh + stored).min(self.cfg.capacity_mwh);
            self.cash_eur -= quantity_mwh * price_eur;
            let total = self.bought_mwh + quantity_mwh;
            if total > 0.0 {
                self.avg_buy_eur =
                    (self.avg_buy_eur * self.bought_mwh + price_eur * quantity_mwh) / total;
            }
            self.bought_mwh = total;
        } else {
            let withdrawn = (quantity_mwh / eff).min(self.soc_mwh);
            self.soc_mwh -= withdrawn;
            self.cash_eur += quantity_mwh * price_eur;
        }
        self.trades += 1;
    }

    /// Convenience for the runner: convert a scaled quantity back to MWh of
    /// grid-side energy for a given fill.
    pub fn fill_mwh(&self, scaled_quantity: i64) -> f64 {
        qty_to_mw(scaled_quantity) * self.cfg.slot_hours
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::TICK;

    fn cfg() -> BatteryConfig {
        BatteryConfig::default()
    }

    #[test]
    fn quotes_a_positive_spread_on_the_tick_grid() {
        let b = Battery::new(cfg());
        let q = b.quote();
        assert!(q.ask_price > q.bid_price, "ask must be above bid");
        assert_eq!((q.ask_price - q.bid_price) % TICK, 0);
        assert_eq!(q.bid_price % TICK, 0);
        assert_eq!(q.ask_price % TICK, 0);
    }

    #[test]
    fn never_buys_at_or_above_fair_and_never_sells_at_or_below_fair() {
        // Across the whole SOC range, the quotes stay economically honest.
        for step in 0..=10 {
            let mut b = Battery::new(cfg());
            b.soc_mwh = b.cfg.capacity_mwh * step as f64 / 10.0;
            b.fair_eur = 120.0;
            let q = b.quote();
            let fair = crate::wire::eur_to_price(120.0);
            assert!(
                q.bid_price < fair || q.bid_quantity == 0,
                "bid {} >= fair {}",
                q.bid_price,
                fair
            );
            assert!(
                q.ask_price > fair || q.ask_quantity == 0,
                "ask {} <= fair {}",
                q.ask_price,
                fair
            );
        }
    }

    #[test]
    fn empty_battery_wants_to_buy_not_sell() {
        let mut b = Battery::new(cfg());
        b.soc_mwh = 0.0;
        let q = b.quote();
        assert!(q.bid_quantity > 0, "empty battery still bids");
        assert_eq!(q.ask_quantity, 0, "empty battery cannot sell");
    }

    #[test]
    fn full_battery_wants_to_sell_not_buy() {
        let mut b = Battery::new(cfg());
        b.soc_mwh = b.cfg.capacity_mwh;
        let q = b.quote();
        assert_eq!(q.bid_quantity, 0, "full battery will not buy");
        assert!(q.ask_quantity > 0, "full battery sells");
    }

    #[test]
    fn inventory_skew_direction_is_right() {
        let mut empty = Battery::new(cfg());
        empty.soc_mwh = 0.0;
        let mut full = Battery::new(cfg());
        full.soc_mwh = full.cfg.capacity_mwh;
        // When empty, both prices are skewed up (keen to buy); when full, down.
        assert!(empty.quote().bid_price > full.quote().bid_price);
        assert!(empty.quote().ask_price > full.quote().ask_price);
    }

    #[test]
    fn never_crosses_itself_and_respects_the_round_trip_floor() {
        let mut b = Battery::new(cfg());
        b.fair_eur = 1.0;
        b.avg_buy_eur = 200.0;
        b.bought_mwh = 1.0;
        let q = b.quote();
        assert!(q.ask_price >= 200, "must not sell below cost basis");
        assert!(q.ask_price > q.bid_price);
    }

    #[test]
    fn soc_stays_within_bounds_under_repeated_fills() {
        let mut b = Battery::new(cfg());
        for _ in 0..1000 {
            b.on_fill(true, 0.02, 50.0);
        }
        assert!(b.soc_mwh <= b.cfg.capacity_mwh + 1e-9);
        for _ in 0..1000 {
            b.on_fill(false, 0.02, 50.0);
        }
        assert!(b.soc_mwh >= -1e-9);
    }

    #[test]
    fn buying_then_selling_at_a_higher_price_makes_money() {
        let mut b = Battery::new(cfg());
        b.soc_mwh = 0.5;
        let cash0 = b.cash_eur;
        b.on_fill(true, 0.5, 20.0); // buy cheap
        b.on_fill(false, 0.4, 60.0); // sell dear
        assert!(b.cash_eur > cash0, "round trip should profit");
    }

    #[test]
    fn fair_mid_tracks_the_tape() {
        let mut b = Battery::new(cfg());
        for _ in 0..50 {
            b.observe_trade(100.0);
        }
        assert!((b.fair_eur - 100.0).abs() < 1.0);
    }
}
