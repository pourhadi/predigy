# predigy

Automated trading system for Kalshi prediction markets. Greenfield Rust
build aiming at every available edge — cross-venue arb, intra-venue static
arb, statistical / model alpha, news-data latency, and (later) market
making with rebate capture.

## Status

**Live in production.** The weather strategy (`bin/latency-trader`)
runs 24/7 under macOS launchd, submitting real orders against
Kalshi capped at $5 account-wide. A daily Claude-powered curator
(`bin/wx-curator`) refreshes its rules at 06:30; a mobile-friendly
dashboard (`bin/dashboard`) on port 8080 surfaces cash, positions,
fires, and daemon health.

| Phase | Description | Status |
|---|---|---|
| 0 | Workspace + `core` types + Kalshi fee formula | ✅ |
| 1 | Read-only stack (REST + WS + book + recorder) | ✅ live-shaken |
| 2 | OMS + risk + REST exec + persistent state | ✅ live-shaken (FIX flavour deferred to MM) |
| 3 | Backtester / sim | ✅ logic; queue-model integration pending |
| 4 | Market making (≥$25k) | ⬜ deferred |
| 5 | Cross-venue signal arb | ✅ logic (`bin/cross-arb-trader`); live shake-down pending |
| 6 | News/data latency | ✅ **live in production** (`latency-trader` + `wx-curator` + Claude rule curation) |
| 7 | Statistical / model alpha | ✅ logic (`bin/stat-trader`); no live rules curated yet |
| 8 | Hardening & scaling | 🟡 macOS launchd deploy + dashboard done; Linux/systemd + VPS pending |

Capital: **$50** funded today (target: $5k). Infra: laptop today,
us-east-1 VPS planned for the latency push.

**For new operators or new Claude Code sessions:** start with
[`docs/SESSIONS.md`](./docs/SESSIONS.md) — orientation on what's
running, where the money is, and what to touch carefully.

## Documentation

- **[`docs/SESSIONS.md`](./docs/SESSIONS.md)** — handoff orientation
  for any new Claude Code session: what's deployed, what's running,
  where the money is, what's next. Read first.
- **[`docs/RUNBOOK.md`](./docs/RUNBOOK.md)** — operational procedures:
  health checks, common interventions, debugging recipes, kill switch.
- **[`docs/STATUS.md`](./docs/STATUS.md)** — living snapshot of what's
  implemented, test counts, confirmed API contracts, file tree.
- **[`docs/PLAN.md`](./docs/PLAN.md)** — full architectural / strategy /
  infrastructure plan. Source of truth for design decisions.
- **[`deploy/README.md`](./deploy/README.md)** — install + ops layout
  for the macOS launchd deployment.

## Layout

```
crates/
  core/         Price (cents 1..=99), Qty, Side, Action, Intent, Order,
                Fill, Position, fees
  book/         L2 order book: snapshot/delta apply, sequence-gap detection
  kalshi-rest/  RSA-PSS auth + REST client (markets, orderbook, positions)
  kalshi-md/    Kalshi WS: orderbook/ticker/trade decode, auto-reconnect
                with sub replay, integration tests against a loopback server
  poly-md/      Polymarket WS reference: book/price_change/last_trade_price/
                tick_size_change decode, no auth, batched-frame handling
  risk/         Pre-trade limits + breakers (per-market position/notional,
                account gross notional, daily-loss breaker, order-rate
                window, kill switch). Synchronous; first breach wins.
  oms/          Order management state machine. Single tokio task owns
                AccountState + per-order ledger; routes Intent through
                risk; allocates deterministic cids; submits via Executor
                trait; consumes ExecutionReports and updates VWAP +
                realised P&L. Kill switch + reconcile.
  kalshi-exec/  REST flavour of the OMS Executor over Kalshi V2.
                Maps Yes/No intents to bid/ask-at-complement, posts
                /portfolio/events/orders, deletes for cancels, polls
                /portfolio/fills into PartiallyFilled/Filled reports.
  sim/          Backtester runtime. BookStore + IOC SimExecutor
                (Executor impl that matches against the touch and
                consumes liquidity via synthetic deltas) + Replay
                (md-recorder NDJSON → BookStore → strategy callback).
                Strategies run unchanged in sim and live.
bin/
  md-recorder/  Long-running NDJSON recorder. Subscribes to a configured
                market list, writes one event per line, on Gap fetches a
                fresh REST snapshot via predigy-kalshi-rest and emits a
                synthetic RestResync line so replay reconstructs identical
                book state.
  arb-trader/   First live strategy: static intra-venue arb. When
                yes_ask + no_ask < $1 minus taker fees, lifts both legs
                via IOC orders. Per-market cooldown + size-cap-at-touch;
                full risk-engine + OMS path; --dry-run for shake-downs.
```

Crates that will be added in subsequent phases: `ext-feeds`,
`strategy`, `signals`, `sim`, `store`, `ops`, plus binaries
`mm-trader`, `latency-trader`, `stat-trader`, and a FIX flavour of
`kalshi-exec`.

## Build

```bash
cargo build --workspace
cargo test  --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Live read-only smoke test (needs a network with public CA roots):

```bash
cargo run -p predigy-kalshi-rest --example smoke
```

## CI

GitHub Actions runs `fmt --check`, `clippy -D warnings`, and
`test --locked` on every push to `main` / `claude/**` and every PR against
`main`. Workflow lives at [`.github/workflows/ci.yml`](./.github/workflows/ci.yml).

## Confirmed Kalshi fee schedule (Feb 2026)

```
taker = ceil(0.07   * C * P * (1-P))   // dollars; ceil to nearest cent
maker = ceil(0.0175 * C * P * (1-P))   // 75% cheaper
```

Implemented in [`crates/core/src/fees.rs`](./crates/core/src/fees.rs).
At p=$0.50 the round-trip taker fee is ~7%, maker round-trip ~1.75% — the
gating constraint on every strategy threshold. See
[`docs/PLAN.md`](./docs/PLAN.md#confirmed-kalshi-fee-schedule-feb-2026)
for the full table and EV implications.
