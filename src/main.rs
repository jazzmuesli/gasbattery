//! gasbattery — the Stromstadt supply fleet (market makers).
//!
//! "gasbattery" is a portfolio of independently-economical units that compete
//! with each other and with the city's cheap renewables on the lowlat exchange:
//! several batteries and several gas turbines, each offering at its true cost.
//!
//! Roles:
//!   * `fleet`   — the default German-style portfolio (3 batteries + 5 turbines)
//!   * `battery` — a single default battery market maker
//!   * `gas`     — a single default peaker
//!   * `probe`   — an end-to-end check against a live lowlat
//!
//! Every unit runs on its own lowlat session (its own thread). This binary is
//! std-only and builds fully self-contained (musl + rust-lld).

// The full protocol constant set lives in `wire`; not every variant is used by
// every role, so silence the "unused const" noise (they document the protocol).
#![allow(dead_code)]

mod battery;
mod client;
mod fleet;
mod gas;
mod merit;
mod wire;

use std::collections::HashMap;
use std::process;
use std::time::{Duration, Instant};

use battery::{Battery, BatteryConfig};
use client::LowlatClient;
use fleet::Fleet;
use gas::{GasConfig, GasPeaker};
use wire::{
    eur_to_price, mw_to_qty, price_to_eur, qty_to_mw, ServerMsg, ORD_LIMIT, SIDE_BUY, SIDE_SELL,
    TIF_GTC, TIF_IOC,
};

#[derive(Debug, Clone)]
struct Opts {
    role: String,
    host: String,
    port: u16,
    product: i32,
    seconds: u64,
    interval_ms: u64,
    /// Delivery-slot length in seconds; sizes each quote's energy (default 5 s,
    /// matching the Stromstadt city).
    slot_seconds: f64,
}

fn parse_args() -> Opts {
    let mut o = Opts {
        role: "fleet".into(),
        host: "127.0.0.1".into(),
        port: 9001,
        product: 0,
        seconds: 0,
        interval_ms: 1000,
        slot_seconds: 5.0,
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(first) = args.first() {
        if !first.starts_with("--") {
            o.role = first.clone();
        }
    }
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--host" => {
                i += 1;
                o.host = args[i].clone();
            }
            "--port" => {
                i += 1;
                o.port = args[i].parse().unwrap_or(9001);
            }
            "--product" => {
                i += 1;
                o.product = args[i].parse().unwrap_or(0);
            }
            "--seconds" => {
                i += 1;
                o.seconds = args[i].parse().unwrap_or(0);
            }
            "--interval-ms" => {
                i += 1;
                o.interval_ms = args[i].parse().unwrap_or(1000);
            }
            "--slot-seconds" => {
                i += 1;
                o.slot_seconds = args[i].parse().unwrap_or(5.0);
            }
            _ => {}
        }
        i += 1;
    }
    o
}

fn main() {
    let opts = parse_args();
    let result = match opts.role.as_str() {
        "fleet" | "both" => run_fleet(&opts).map(|_| true),
        "battery" => {
            let maker = Battery::new(BatteryConfig::default());
            run_battery_unit(&opts, maker, "battery".into()).map(|_| true)
        }
        "gas" => {
            let maker = GasPeaker::new("peaker", gas::TurbineKind::Ocgt, GasConfig::default());
            run_turbine_unit(&opts, maker, "peaker".into()).map(|_| true)
        }
        "probe" => run_probe(&opts),
        other => {
            eprintln!("unknown role: {other} (expected fleet|battery|gas|probe)");
            process::exit(2);
        }
    };
    match result {
        Ok(true) => {}
        Ok(false) => process::exit(1),
        Err(e) => {
            eprintln!("error: {e}");
            process::exit(1);
        }
    }
}

fn deadline(opts: &Opts) -> Option<Instant> {
    if opts.seconds == 0 {
        None
    } else {
        Some(Instant::now() + Duration::from_secs(opts.seconds))
    }
}

fn expired(d: Option<Instant>) -> bool {
    d.map(|t| Instant::now() >= t).unwrap_or(false)
}

/// Set `GB_DEBUG=1` to trace every server message (diagnostics only).
fn debug() -> bool {
    std::env::var("GB_DEBUG").is_ok()
}

/// State for one session's live orders: client id -> (product, maybe exch id).
#[derive(Default)]
struct LiveOrders {
    orders: HashMap<u64, (i32, Option<u64>)>,
}

impl LiveOrders {
    fn note_posted(&mut self, client_id: u64, product: i32) {
        self.orders.insert(client_id, (product, None));
    }
    fn on_ack(&mut self, client_id: u64, exch_id: u64, accepted: bool) {
        if accepted {
            if let Some(entry) = self.orders.get_mut(&client_id) {
                entry.1 = Some(exch_id);
            }
        } else {
            self.orders.remove(&client_id);
        }
    }
    fn on_terminal(&mut self, client_id: u64) {
        self.orders.remove(&client_id);
    }
}

// ---------------------------------------------------------------------------
// Fleet: one thread per unit
// ---------------------------------------------------------------------------

fn run_fleet(opts: &Opts) -> std::io::Result<()> {
    let fleet = Fleet::default_german();
    println!("fleet merit stack (ascending offer price, EUR/MWh):");
    for (id, price) in fleet.merit_stack() {
        println!("  {id:>12}  {price:>8.2}");
    }

    let mut handles = Vec::new();
    for (i, b) in fleet.batteries.into_iter().enumerate() {
        let o = opts.clone();
        let label = format!("battery-{i}");
        handles.push(std::thread::spawn(move || {
            if let Err(e) = run_battery_unit(&o, b, label.clone()) {
                eprintln!("[{label}] {e}");
            }
        }));
    }
    for t in fleet.turbines.into_iter() {
        let o = opts.clone();
        let label = t.id.clone();
        handles.push(std::thread::spawn(move || {
            if let Err(e) = run_turbine_unit(&o, t, label.clone()) {
                eprintln!("[{label}] {e}");
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// One battery unit
// ---------------------------------------------------------------------------

fn run_battery_unit(opts: &Opts, mut maker: Battery, label: String) -> std::io::Result<()> {
    maker.set_slot_seconds(opts.slot_seconds);
    let mut client = LowlatClient::connect(&opts.host, opts.port)?;
    let mut live = LiveOrders::default();
    let mut side_of: HashMap<u64, bool> = HashMap::new();
    let mut nonce = 0u64;
    println!(
        "[{label}] online {}:{} product {} cap={} MWh pwr={} MW eff={}",
        opts.host,
        opts.port,
        opts.product,
        maker.cfg.capacity_mwh,
        maker.cfg.power_mw,
        maker.cfg.round_trip_eff
    );
    let d = deadline(opts);
    while !expired(d) {
        nonce += 1;
        client.heartbeat(nonce)?;
        let msgs = client.poll()?;
        if debug() {
            for m in &msgs {
                eprintln!("[{label}] <- {m:?}");
            }
        }
        for msg in &msgs {
            apply_battery_msg(msg, &mut maker, &mut live, &mut side_of, &label);
        }
        let to_cancel: Vec<(i32, u64)> = live
            .orders
            .values()
            .filter_map(|(p, e)| e.map(|e| (*p, e)))
            .collect();
        live.orders.clear();
        for (p, e) in to_cancel {
            client.cancel(p, e)?;
        }
        let q = maker.quote();
        if q.bid_quantity > 0 {
            let id = client.new_order(
                opts.product,
                SIDE_BUY,
                ORD_LIMIT,
                TIF_GTC,
                q.bid_price,
                q.bid_quantity,
            )?;
            live.note_posted(id, opts.product);
            side_of.insert(id, true);
        }
        if q.ask_quantity > 0 {
            let id = client.new_order(
                opts.product,
                SIDE_SELL,
                ORD_LIMIT,
                TIF_GTC,
                q.ask_price,
                q.ask_quantity,
            )?;
            live.note_posted(id, opts.product);
            side_of.insert(id, false);
        }
        println!(
            "[{label}] bid {} x {} / ask {} x {}  soc={:.3} cash={:.2} fair={:.2}",
            price_to_eur(q.bid_price),
            qty_to_mw(q.bid_quantity),
            price_to_eur(q.ask_price),
            qty_to_mw(q.ask_quantity),
            maker.soc_mwh,
            maker.cash_eur,
            maker.fair_eur,
        );
        std::thread::sleep(Duration::from_millis(opts.interval_ms));
    }
    Ok(())
}

fn apply_battery_msg(
    msg: &ServerMsg,
    maker: &mut Battery,
    live: &mut LiveOrders,
    side_of: &mut HashMap<u64, bool>,
    label: &str,
) {
    match msg {
        ServerMsg::Ack {
            client_order_id,
            exchange_order_id,
            accepted,
            reason,
            ..
        } => {
            if !accepted {
                eprintln!("[{label}] order {client_order_id} rejected: {reason}");
            }
            live.on_ack(*client_order_id, *exchange_order_id, *accepted);
        }
        ServerMsg::Exec {
            client_order_id,
            status,
            fill,
            ..
        } => {
            if let Some(f) = fill {
                let bought = *side_of.get(client_order_id).unwrap_or(&true);
                let mwh = qty_to_mw(f.quantity) * maker.cfg.slot_hours;
                let price = price_to_eur(f.price);
                maker.on_fill(bought, mwh, price);
                println!(
                    "[{label}] {} {:.3} MWh @ {:.2}",
                    if bought { "bought" } else { "sold" },
                    mwh,
                    price
                );
            }
            if wire::is_terminal_status(*status) {
                live.on_terminal(*client_order_id);
                side_of.remove(client_order_id);
            }
        }
        ServerMsg::Trade { price, .. } => maker.observe_trade(price_to_eur(*price)),
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// One gas turbine unit
// ---------------------------------------------------------------------------

fn run_turbine_unit(opts: &Opts, mut maker: GasPeaker, label: String) -> std::io::Result<()> {
    let mut client = LowlatClient::connect(&opts.host, opts.port)?;
    let mut live = LiveOrders::default();
    let mut nonce = 0u64;
    println!(
        "[{label}] online {}:{} product {} SRMC {:.2} EUR/MWh cap {} MW",
        opts.host,
        opts.port,
        opts.product,
        maker.ask_price_eur(),
        maker.cfg.capacity_mw
    );
    let d = deadline(opts);
    while !expired(d) {
        nonce += 1;
        client.heartbeat(nonce)?;
        let msgs = client.poll()?;
        if debug() {
            for m in &msgs {
                eprintln!("[{label}] <- {m:?}");
            }
        }
        for msg in &msgs {
            apply_turbine_msg(msg, &mut maker, &mut live, &label);
        }
        let to_cancel: Vec<(i32, u64)> = live
            .orders
            .values()
            .filter_map(|(p, e)| e.map(|e| (*p, e)))
            .collect();
        live.orders.clear();
        for (p, e) in to_cancel {
            client.cancel(p, e)?;
        }
        let (price, qty) = maker.quote();
        let id = client.new_order(opts.product, SIDE_SELL, ORD_LIMIT, TIF_GTC, price, qty)?;
        live.note_posted(id, opts.product);
        println!(
            "[{label}] ask {} x {} MW",
            price_to_eur(price),
            qty_to_mw(qty)
        );
        std::thread::sleep(Duration::from_millis(opts.interval_ms));
    }
    Ok(())
}

fn apply_turbine_msg(msg: &ServerMsg, maker: &mut GasPeaker, live: &mut LiveOrders, label: &str) {
    match msg {
        ServerMsg::Ack {
            client_order_id,
            exchange_order_id,
            accepted,
            reason,
            ..
        } => {
            if !accepted {
                eprintln!("[{label}] order {client_order_id} rejected: {reason}");
            }
            live.on_ack(*client_order_id, *exchange_order_id, *accepted);
        }
        ServerMsg::Exec {
            client_order_id,
            status,
            fill,
            ..
        } => {
            if let Some(f) = fill {
                println!(
                    "[{label}] sold {} MW @ {}",
                    qty_to_mw(f.quantity),
                    price_to_eur(f.price)
                );
            }
            if wire::is_terminal_status(*status) {
                live.on_terminal(*client_order_id);
            }
        }
        ServerMsg::Book {
            ask_price,
            ask_quantity,
            ..
        } if *ask_quantity > 0 => maker.observe_best_ask(price_to_eur(*ask_price)),
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Probe: prove the wire end-to-end against a live exchange
// ---------------------------------------------------------------------------

/// Rest an ask, cross it with a buy, and assert a fill and a tape print.
fn run_probe(opts: &Opts) -> std::io::Result<bool> {
    let mut maker = LowlatClient::connect(&opts.host, opts.port)?;
    let mut taker = LowlatClient::connect(&opts.host, opts.port)?;
    let product = opts.product;
    let px = eur_to_price(230.0);
    let qty = mw_to_qty(5.0);

    let sell_id = maker.new_order(product, SIDE_SELL, ORD_LIMIT, TIF_GTC, px, qty)?;
    let mut maker_exch = None;
    let start = Instant::now();
    while maker_exch.is_none() && start.elapsed() < Duration::from_secs(2) {
        for m in maker.poll()? {
            if let ServerMsg::Ack {
                client_order_id,
                exchange_order_id,
                accepted,
                reason,
                ..
            } = m
            {
                if client_order_id == sell_id {
                    if !accepted {
                        eprintln!("probe: resting sell rejected: {reason}");
                        return Ok(false);
                    }
                    maker_exch = Some(exchange_order_id);
                }
            }
        }
    }
    if maker_exch.is_none() {
        eprintln!("probe: no ack for the resting sell");
        return Ok(false);
    }
    println!("probe: resting SELL {} MW @ 230.00 accepted", qty_to_mw(qty));

    let buy_id = taker.new_order(product, SIDE_BUY, ORD_LIMIT, TIF_IOC, px, qty)?;
    let mut taker_filled = 0i64;
    let mut saw_trade = false;
    let start = Instant::now();
    while (taker_filled < qty || !saw_trade) && start.elapsed() < Duration::from_secs(3) {
        for m in taker.poll()? {
            match m {
                ServerMsg::Exec {
                    client_order_id,
                    filled_quantity,
                    fill,
                    ..
                } if client_order_id == buy_id => {
                    taker_filled += fill.map(|f| f.quantity).unwrap_or(filled_quantity);
                }
                ServerMsg::Trade { price, quantity, .. } => {
                    saw_trade = true;
                    println!(
                        "probe: TAPE {} EUR/MWh x {} MW",
                        price_to_eur(price),
                        qty_to_mw(quantity)
                    );
                }
                _ => {}
            }
        }
        for _ in maker.poll()? {}
    }

    let ok = saw_trade && taker_filled >= qty;
    println!(
        "probe: {} (filled {} of {} lots, tape={})",
        if ok { "PASS" } else { "FAIL" },
        taker_filled,
        qty,
        saw_trade
    );
    Ok(ok)
}
