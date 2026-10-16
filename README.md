# gasbattery

The **supply fleet** of Stromstadt: a portfolio of independently-economical
generation units that compete with each other and with the city's cheap
renewables on the `lowlat` exchange.

It is *not* one battery and one gas turbine. It is a small but representative
German-style merit order:

```
  renewables (~0, from the city, price-exposed — no feed-in tariff)
    < batteries (opportunity cost, dynamic)
      < CCGT-1 (~95)  < CCGT-2 (~104)
        < OCGT-1 (~145)  < OCGT-2 (~213)  < peaker (~230)
```

Every unit offers at its own short-run marginal cost (or opportunity cost), so
lowlat's price–time priority produces the merit order for free. Nothing receives
a feed-in tariff: a unit is dispatched only when the clearing price covers its
cost, so nothing that runs makes a loss.

## Layout

| file | what |
|---|---|
| `src/wire.rs` | hand-rolled, `FastWire`-compatible protobuf codec + framing + `is_terminal_status` |
| `src/client.rs` | blocking TCP client: heartbeat, new order, cancel, poll |
| `src/merit.rs` | SRMC decomposition + tick rounding (`round`/`floor`/`ceil`) |
| `src/battery.rs` | one battery: fair-value EWMA, two-sided quoting, inventory |
| `src/gas.rs` | one gas turbine: SRMC-derived, tick-safe ask |
| `src/fleet.rs` | `Fleet::default_german()` — 3 batteries + 5 turbines, merit stack |
| `src/main.rs` | roles `fleet` / `battery` / `gas` / `probe`; one thread per unit |

## Build & test

No C compiler is assumed: the crate is std-only and builds self-contained with
musl + `rust-lld` (see `.cargo/config.toml`).

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cd gasbattery
cargo test        # pure unit tests: wire, merit, battery, gas, fleet
cargo build
BIN=target/x86_64-unknown-linux-musl/debug/gasbattery
```

## Run against lowlat

Start the exchange (needs the Agrona module flags, so use its `tools/run.sh`):

```sh
cd ~/hgit/lowlat && ./tools/run.sh engine --port 9001
```

Then, from the fleet binary:

```sh
# end-to-end check: resting ask -> crossing buy -> ack -> fill -> tape
$BIN probe  --host 127.0.0.1 --port 9001

# the whole portfolio (3 batteries + 5 turbines), one session per unit
$BIN fleet  --host 127.0.0.1 --port 9001 --product 0

# a single unit, for debugging
$BIN battery --host 127.0.0.1 --port 9001
$BIN gas     --host 127.0.0.1 --port 9001
```

Options: `--product <id>` (default 0), `--seconds <n>` (0 = run forever),
`--interval-ms <n>` (requote period). Set `GB_DEBUG=1` to trace every server
message.

## Economics, in one paragraph

A gas turbine asks `ceil_tick(fuel/η + co2·price + O&M)` and never below; a
battery keeps a fair mid, bids strictly below it and asks strictly above it, and
refuses to discharge below `buy_price / η_rt`. Because the cheapest ask rests
first, the CCGTs run as mid-merit, the OCGTs only at peak, and the peaker is the
last resort — the marginal unit sets the price, and the batteries sit inside the
thermal stack. Curtailment of price-exposed renewables (in `pvr-stochastic`)
supplies the floor; the peaker supplies the cap.
