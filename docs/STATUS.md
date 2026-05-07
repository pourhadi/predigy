# Predigy Build Status

> Living snapshot. Update with every phase commit. Architectural
> design in [`ARCHITECTURE.md`](./ARCHITECTURE.md).

## Current configuration (2026-05-07)

| Setting | Value |
|---|---|
| Phase | 6 complete + cutover executed; Phase 7 (retire scaffolding) pending; Phase 4b (FIX) blocked |
| Capital | ~$50 funded |
| Per-strategy notional cap | $5 |
| Global notional cap | $15 |
| Daily-loss breaker | $2 |
| Hosting | macOS laptop (one-process engine) |
| Branch | `main` |
| Rust toolchain | stable, edition 2024 |
| Workspace test count | ~340 (15 OMS-integration tests against Postgres `predigy_test`) |
| Engine binary | `target/release/predigy-engine` |
| Engine mode | `Live` (env `PREDIGY_ENGINE_MODE=live`) |

## Phase status (engine refactor)

| Phase | Description | Status |
|---|---|---|
| 0 | Plumbing (workspace, core types, fees, lints) | ✅ |
| 1 | Read-only stack: kalshi-rest, book, kalshi-md WS, md-recorder, poly-md WS | ✅ live-shaken |
| 2 | OMS skeleton + Postgres schema + dashboard DB-read path | ✅ |
| 3 | Stat-trader as first strategy module | ✅ |
| 4a | REST submitter + WS-push fills (production venue path) | ✅ shipped |
| 4b | FIX as primary order path | 🟦 **BLOCKED** on Kalshi institutional onboarding |
| 5 | Port remaining traders into engine modules | ✅ stat, settlement, latency, cross-arb. Curators stay external by design. |
| 6 | Active position management | ✅ TP/SL exits in stat + cross-arb; time-based force-flat in latency; settlement skipped (venue auto-settles); global notional cap; cross-strategy event bus. |
| 7 | Retire scaffolding (legacy daemons, JSON state, deprecated crates) | ⬜ Wait for ≥1 week stable engine before deleting. |

### What's running in production (as of cutover)

Live launchd jobs:

- `com.predigy.engine` — consolidated trader, all four strategies
- `com.predigy.dashboard` — port 8080 mobile UI
- `com.predigy.{cross-arb,stat,wx,wx-stat}-curate` — Anthropic-driven
  rule + pair curators (cron-scheduled)
- `com.predigy.import` — legacy JSON state mirror (compat layer)

Retired:

- `com.predigy.{latency-trader,settlement,stat-trader,cross-arb}` —
  bootout'd 2026-05-07 07:45 UTC. Plist files retained on disk for
  one cycle as a rollback path.

## File tree (current)

```
crates/
  core/                 ✅ Price, Side, Action, fees
  book/                 ✅ L2 + delta apply
  kalshi-rest/          ✅ V2 REST + RSA-PSS auth + 429 retry
  kalshi-md/            ✅ WS: orderbook + authed fill/positions
  poly-md/              ✅ Polymarket WS reference
  ext-feeds/            ✅ NWS active alerts feed
  signals/              ✅ Kelly fraction
  engine-core/          ✅ Strategy trait, Event/Intent, cross-strategy bus
  strategies/
    stat/               ✅ + Phase 6.1 TP/SL exits
    settlement/         ✅ discovery-driven; no active exits
    latency/            ✅ + Phase 6.2 time-based force-flat
    cross-arb/          ✅ + Phase 6.2 TP/SL exits + bus producer
  oms/                  Legacy (used only by retired daemons)
  kalshi-exec/          Legacy (used only by retired daemons)
  kalshi-fix/           ⏸ Phase 4b — blocked on Kalshi access
  sim/                  ✅ Backtester
bin/
  predigy-engine/       ✅ THE consolidated trader. config, oms_db,
                        market_data router, exec_data WS consumer,
                        venue_rest REST submitter, discovery_service,
                        external_feeds dispatcher, pair_file_service,
                        cross_strategy_bus, supervisor, registry.
  predigy-dashboard/    ✅ Phase 6 surfaces: engine positions, recent exits
  predigy-import/       ✅ Legacy JSON → Postgres mirror
  cross-arb-curator/    ✅ Anthropic-driven, incremental + watch mode
  stat-curator/         ✅ Anthropic-driven model_p curator
  wx-curator/           ✅ NWS rule curator
  wx-stat-curator/      ✅ NBM-quantile rule fitter
  md-recorder/          ✅ NDJSON archive
  arb-trader/           Legacy intra-venue arb (predigy-engine doesn't yet do this)
  sim-runner/           ✅ Replay tooling
  # Retired (kept around as rollback path):
  latency-trader/, settlement-trader/, stat-trader/, cross-arb-trader/
deploy/
  macos/com.predigy.engine.plist        ✅ live
  macos/com.predigy.dashboard.plist     ✅ live
  macos/com.predigy.import.plist        ✅ live
  macos/com.predigy.{wx,stat,cross-arb,wx-stat}-curate.plist  ✅ live
  macos/com.predigy.{latency-trader,settlement,stat-trader,cross-arb}.plist  retired
  scripts/engine-run.sh                 ✅
  scripts/install-launchd.sh            ✅ extends to engine
docs/
  ARCHITECTURE.md       ✅ engine refactor design (load-bearing)
  SESSIONS.md           ✅ operational handoff
  RUNBOOK.md            ✅ ops procedures
  STATUS.md             this file
  CUTOVER.md            ✅ shadow → live playbook (used as reference)
  DATABASE.md           ✅ Postgres setup + schema
  AUDIT.md              ✅ profit-take + scale-up + arsenal analysis (2026-05-07)
  KALSHI_FIX_REQUEST.md email draft for Phase 4b
  PLAN.md               historic; superseded by ARCHITECTURE.md
  WX_STAT_*.md          historic; weather-strategy notes
migrations/
  0001_initial.sql      ✅ Postgres schema
```

## Test discipline

CI runs `fmt --check` + `clippy -D warnings` + `cargo test --workspace
--locked` against a Postgres 16 service container. The integration
tests in `bin/predigy-engine/tests/oms_integration.rs` (15 tests)
exercise the OMS against a real DB including idempotency, kill
switch, contract caps, partial fills, position cascade, and the
Phase 6.2 global notional cap.

Runtime breakdown of passing tests:

| Crate | Tests |
|---|---|
| predigy-core | 8 |
| predigy-book | 21 |
| predigy-kalshi-rest | 49 |
| predigy-kalshi-md | 25 |
| predigy-poly-md | 18 |
| predigy-ext-feeds | 11 |
| predigy-signals | 4 |
| predigy-risk | 7 |
| predigy-oms (legacy) | 67 |
| predigy-kalshi-exec (legacy) | 13 |
| predigy-kalshi-fix | 32 |
| predigy-sim | 21 |
| predigy-engine-core | 2 |
| predigy-engine (lib + integration) | 30 + 15 |
| predigy-strategy-stat | 16 |
| predigy-strategy-settlement | 13 |
| predigy-strategy-latency | 17 |
| predigy-strategy-cross-arb | 16 |
| predigy-dashboard | 9 |
| predigy-import | 12 |
| Curators (`*-curator`) | varies |

## Confirmed Kalshi V2 API contracts (Feb–May 2026)

- **Fee schedule** (live-confirmed): `taker = ceil(0.07 × C × P × (1−P))`,
  `maker = ceil(0.0175 × C × P × (1−P))`. Implementation in
  `crates/core/src/fees.rs`.
- **`createorder` shape**: V2 expects `side` (yes/no), separate
  `action` (buy/sell), `price` (single field, decimal-string,
  4-decimal precision; the documented `yes_price`/`no_price`
  variants returned "empty decimal string" errors when probed).
- **`client_order_id`**: rejects values containing `.` (live-
  confirmed during the 2026-05-07 cutover). Both legacy
  `CidAllocator` and engine's `cid_safe_ticker` strip them.
- **WS authed channels** (`fill`, `market_positions`, `order_state`):
  empty `market_tickers` = "all the user's markets".
- **WS sequence-gap recovery**: `OrderbookDelta` events carry
  monotonic `seq`; on gap, the kalshi-md crate's session task
  recomputes via REST `orderbook_snapshot`.
- **MarketSummary fields**: `expected_expiration_time` and
  `can_close_early` exist on per-event sports markets;
  settlement-trader's discovery filters by these (preferred over
  `close_time`).

## Open issues to watch

1. **Initial-snapshot burst.** When the engine first connects to WS,
   the strategy treats the first snapshot of every subscribed market
   as a fresh book delta and tries to fire on all of them. The
   in-flight cap (10) bounds the damage, but a startup-grace period
   would prevent the noise. Tracked as audit item #G1 in `AUDIT.md`.
2. **Cross-strategy bus is consumer-light.** stat subscribes to
   `poly_mid` and `model_probability` topics but only logs them.
   The augmented-belief implementation is a future commit. Audit
   item #A4.
3. **No book mark for engine-positions on dashboard.** Dashboard's
   "engine positions" table doesn't compute unrealized P&L —
   `book_snapshots` only carries YES-side data. Audit item #A2.
4. **Phase 4b FIX blocked.** Operator-side action: send the email
   in `docs/KALSHI_FIX_REQUEST.md`.
