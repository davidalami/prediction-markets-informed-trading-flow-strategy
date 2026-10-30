# Prediction Markets Informed Trading Flow Strategy

A native Rust implementation of a late-flow momentum strategy for Polymarket.
It provides a live alert monitor and deterministic historical replay. No Python,
Node.js, account credentials, or private keys are required.

The monitor is read-only: it alerts on eligible $5 entries and never submits orders.

## Build and run

Install a Rust toolchain with Cargo, then run from this repository:

```sh
cargo build --release --locked

# Terminal alerts and JSON heartbeat output:
./target/release/informed-trader-flow monitor

# Add macOS desktop notifications:
./target/release/informed-trader-flow monitor --notify

# Bounded public-API check:
./target/release/informed-trader-flow monitor --count 2
```

The first poll loads rolling trade history. New signal checks begin at the next
UTC minute. Polling targets five-second intervals; API delays can extend that
cadence. This is a polling monitor, not a subsecond WebSocket feed.

Desktop notifications use macOS's built-in `osascript`. Terminal alerts, JSON
output, and replay do not require it. Ctrl-C stops the monitor.

## Frozen strategy

The rules are stored in [config/strategy.json](config/strategy.json):

| Rule | Value |
|---|---|
| Price signal | Follow a move **greater than 5 percentage points** over five minutes |
| Recent activity | At least $100 taker volume, five fills, and two observed wallets |
| Baseline | Preceding five-minute volume at least $100; recent volume at least as large |
| Entry horizon | Final 60 minutes before scheduled close |
| Trade budget | $5 per condition, once |
| Entry timing | First same-token print at least five seconds after the signal, within two minutes |
| Print participation | At most 10% of that print's notional |
| Entry price cap | 75¢ after assumed adverse slippage |
| Cost assumptions | 100 bps slippage, 50 bps fees |
| Exit for replay | Explicit settlement, or a fresh cutoff mark |

A falling first-outcome price selects the actual opposite token. Current quotes
and future trade prints are never used to calculate an earlier signal.

Live alerts additionally require an open market, unchanged deadline/token mapping,
fresh data, sufficient current ask depth, a valid tick-rounded limit, and the
market's minimum share size. A rejected first print is not replaced by a more
favorable later print. See [strategy details](docs/strategy.md) and [operations](docs/operations.md).

## Alerts and persistence

By default, runtime files go under `data/`:

- `flow-alerts.jsonl`: durable eligible-alert ledger.
- `flow-monitor-state.json`: pending and completed condition state.
- Corresponding `.lock` files prevent overlapping monitors.

Stdout contains NDJSON startup and heartbeat records. Each heartbeat includes
coverage, feed lag, pending signals, eligible alerts, skips, and errors. Human
alerts go to stderr and include the outcome, $5 budget, limit price, and market link.

```sh
./target/release/informed-trader-flow monitor --notify >> data/monitor.ndjson
./target/release/informed-trader-flow monitor --help
```

Alert records are synced to disk before state is atomically replaced and desktop
notifications sent. The ledger is replayed on restart to prevent duplicate alerts.
Corrupt or mismatched state stops the process. The rule fingerprint and JSON
formats are compatible with the original monitor for this frozen configuration.

Use separate state and ledger paths for a different rule set:

```sh
./target/release/informed-trader-flow monitor \
  --strategy config/strategy.json \
  --state data/custom-state.json \
  --alerts data/custom-alerts.jsonl
```

The default configuration/data paths resolve to this repository's build location.
Pass explicit paths if you relocate the compiled executable.

## Offline replay

The included historical regression fixture runs without network access:

```sh
./target/release/informed-trader-flow replay \
  --input tests/fixtures/historical/raw.json \
  --output data/fixture-replay.json
```

For another captured run, use a `raw.json` containing `markets`, `trades`,
`coverage`, and `errors`. The command reads the cutoff from sibling
`results.json`, or accepts explicit timezone-aware bounds:

```sh
./target/release/informed-trader-flow replay \
  --input /path/to/raw.json \
  --start 2026-10-29T13:30:39Z \
  --end 2026-10-30T13:30:39Z \
  --output data/replay.json
```

The optional output file contains individual trades, PnL, summary, skip reasons,
and coverage. Stdout prints a compact summary. Incomplete histories are excluded;
missing marks remain unpriced and are excluded from valued ROI.

## Validation

The full 24-hour Rust replay was compared against the reference capture covering
26,169 markets and 1,051,158 trade rows. Every trade field, skip count, and summary
value matched exactly:

| Metric | Result |
|---|---:|
| Trades | 12 |
| Winners / losers | 11 / 1 |
| Cost | $60.00 |
| Simulated net PnL | +$23.49 |
| ROI on trade costs | +39.14% |

The seven trades outside the fitting window earned $10.60 on $35. These are short,
retrospective results using trade-print fill proxies and assumed costs, not evidence
of persistent future returns or actual executable fills.

[Validation evidence](validation/full-24h-parity.json) records the full-capture
hash and comparison. The bundled fixture contains only the twelve executed
markets for regression testing; it is deliberately **not** an unbiased universe
for measuring or optimizing strategy performance. See [validation notes](docs/validation.md).

The [bounded live check](validation/live-smoke.json) completed two successful polls,
with a two-second feed lag on its second poll.

## Development

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

Tests cover chronological calculations, token direction, costs, fresh/incomplete
feeds, decimal tick rounding, live eligibility, retry behavior, process locks,
restart recovery, and exact replay of the twelve reference trades.

```text
config/strategy.json             Frozen strategy and selection provenance
src/strategy.rs                  Signal rules, normalization, and live book checks
src/api.rs                       Public HTTP client and cursor-paginated trade feed
src/monitor.rs                   Live decision and pending-entry state machine
src/storage.rs                   Durable ledger, atomic state, and process locks
src/replay.rs                    Offline execution/valuation simulation
src/main.rs                      CLI, shutdown handling, terminal/macOS alerts
tests/strategy.rs                Native regression tests
tests/fixtures/historical/       Small deterministic historical fixture
validation/full-24h-parity.json  Full-universe reference comparison
docs/                           Strategy, operation, and validation details
data/                           Ignored runtime files
```

Network calls use only public read APIs. A bounded monitor/replay returns 0 on
complete coverage, 2 on partial/stale data, and 1 on errors; Ctrl-C returns 130.
A complete poll without eligible entries is a normal successful result.

Licensed under [MIT](LICENSE).
