# predigy

Automated trading system for Kalshi prediction markets. Greenfield Rust
build aiming at every available edge — cross-venue arb, intra-venue static
arb, statistical / model alpha, news-data latency, and (later) market
making with rebate capture.

## Status

**Phase 2 in progress.** Phase 1 complete in code (live shake-down
on a real Kalshi key still open). Phase 2 has the risk gate
(`predigy-risk`) and the order management state machine
(`predigy-oms` — single-task runtime, Executor trait, deterministic
cids, VWAP + realised-P&L bookkeeping, kill switch, reconcile).
Coming next: `kalshi-exec` (FIX) and `arb-trader` (first live
strategy).

| Phase | Description | Status |
|---|---|---|
| 0 | Workspace + `core` types + Kalshi fee formula | ✅ |
| 1 | Read-only stack (REST + WS + book + recorder) | ✅ (logic) |
| 2 | OMS + risk + FIX exec + first live strategy | 🟡 in progress (`risk` + `oms` done) |
| 3 | Backtester / sim | ⬜ |
| 4 | Market making (deferred until $25k account) | ⬜ |
| 5 | Cross-venue signal arb (primary engine) | ⬜ |
| 6 | News/data latency (free feeds first) | ⬜ |
| 7 | Statistical / model alpha | ⬜ |
| 8 | Hardening & scaling | ⬜ |

Capital: **$5k**. Infra budget: **~$80-150/mo**. Hosting target: single
Chicago VPS (Kalshi's matching engine is in Chicago — AWS us-east-1 is
~25-40ms away and disqualifying).

## Documentation

- **[`docs/PLAN.md`](./docs/PLAN.md)** — full architectural / strategy /
  infrastructure plan. Source of truth for design decisions.
- **[`docs/STATUS.md`](./docs/STATUS.md)** — living snapshot of what's
  implemented, test counts, confirmed API contracts, known limitations,
  next steps.

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
bin/
  md-recorder/  Long-running NDJSON recorder. Subscribes to a configured
                market list, writes one event per line, on Gap fetches a
                fresh REST snapshot via predigy-kalshi-rest and emits a
                synthetic RestResync line so replay reconstructs identical
                book state.
```

Crates that will be added in subsequent phases: `kalshi-exec`,
`ext-feeds`, `strategy`, `signals`, `sim`, `store`, `ops`,
plus binaries `arb-trader`, `mm-trader`, `latency-trader`, `stat-trader`.

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
