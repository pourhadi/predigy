# predigy

Automated trading system for Kalshi prediction markets. Single-binary
Rust engine, four strategies running concurrently, Postgres-backed
state, mobile dashboard.

## Status (2026-05-07)

**LIVE.** The consolidated `predigy-engine` binary executed its
production cutover at 07:45 UTC today. Four strategies run inside it:
`stat`, `settlement`, `latency`, `cross-arb`. The four legacy
per-strategy daemons (`stat-trader`, `settlement-trader`,
`latency-trader`, `cross-arb-trader`) have been booted out of launchd.

| Component | Status |
|---|---|
| `predigy-engine` (consolidated trader) | **live** in `EngineMode::Live` |
| Per-strategy curators (`wx-curate`, `stat-curate`, `cross-arb-curate`, `wx-stat-curate`) | live (cron) |
| `predigy-import` (legacy JSON → Postgres mirror) | live (cron) |
| `predigy-dashboard` (port 8080) | live |
| Phase 4b — FIX as primary order path | blocked on Kalshi institutional access (email pending) |

Capital cap: $5/strategy · $15 global · $2/day-loss · ~$50 funded.
Kill-switch flag at `~/.config/predigy/kill-switch.flag` arms both
the engine and dashboard.

Architectural overview: [`docs/ARCHITECTURE.md`](./docs/ARCHITECTURE.md).
For new operators or new Claude Code sessions: start with
[`docs/SESSIONS.md`](./docs/SESSIONS.md).

## Repo layout

```
crates/
  core/                 Price (cents 1..=99), Side, Action, fee formula
  book/                 L2 order book: snapshot/delta apply, gap detection
  kalshi-rest/          RSA-PSS auth + REST client (V2 endpoints, 429 retry)
  kalshi-md/            Kalshi WS: orderbook + authed fill / market_positions
  poly-md/              Polymarket WS reference (read-only)
  ext-feeds/            NWS active alerts feed (used by latency strategy)
  signals/              Kelly fraction + edge math
  engine-core/          Strategy trait + Event/Intent types + cross-strategy bus
                        contracts. Every strategy crate depends on this.
  strategies/
    stat/               Statistical-probability — model_p vs ask, after-fee Kelly sizing
    settlement/         Sports settlement-time tape-reading; discovery-driven
    latency/            NWS-alert lift on weather markets; rule-file driven
    cross-arb/          Polymarket-vs-Kalshi stat-arb on paired tokens
  oms/                  Legacy in-process OMS (used by retired daemons; superseded by engine)
  kalshi-exec/          Legacy REST executor (superseded by engine's venue_rest)
  kalshi-fix/           FIX 4.4 implementation (Phase 4b — blocked on Kalshi access)
  sim/                  Backtester runtime
bin/
  predigy-engine/       Consolidated trader. OMS, risk, market-data router,
                        REST submitter, WS exec-data consumer, discovery service,
                        external-feeds dispatcher, pair-file service,
                        cross-strategy bus, supervisor.
  predigy-import/       Mirrors legacy oms-state-*.json into Postgres.
                        Compat layer for the migration window.
  predigy-dashboard/    HTTP/HTML mobile dashboard. DB-derived state
                        with JSON fallback. Engine positions + recent exits
                        live since Phase 6.
  cross-arb-curator/    Anthropic-driven Kalshi×Polymarket pair finder.
  stat-curator/         Anthropic-driven model_p curator for stat strategy.
  wx-curator/           NWS-state-aware weather rule curator (latency strategy).
  wx-stat-curator/      NBM-quantile probabilistic weather rule fitter (stat strategy).
  md-recorder/          NDJSON recorder (replay archive).
  arb-trader/, sim-runner/  Tooling.

  # Retired / disabled (legacy daemons, kept around as fallback for one cycle):
  latency-trader/, settlement-trader/, stat-trader/, cross-arb-trader/
```

## Build + test

```bash
cargo build --workspace
cargo test  --workspace          # ~340 tests; integration tests need predigy_test DB
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings

cargo build --release -p predigy-engine
```

CI is GitHub Actions: `fmt --check` + `clippy -D warnings` +
`cargo test --workspace --locked` against a Postgres 16 service
container. See [`.github/workflows/ci.yml`](./.github/workflows/ci.yml).

## Documentation

- **[`docs/ARCHITECTURE.md`](./docs/ARCHITECTURE.md)** — engine
  refactor design (Phases 0–7), database schema rationale, crate
  graph, OMS contract, supervisor model.
- **[`docs/SESSIONS.md`](./docs/SESSIONS.md)** — operational
  handoff snapshot. What's deployed, what's running, what's next.
  Read first for any new session.
- **[`docs/RUNBOOK.md`](./docs/RUNBOOK.md)** — operational
  procedures. Health checks, kill-switch, debugging, redeploy.
- **[`docs/STATUS.md`](./docs/STATUS.md)** — living phase-status.
- **[`docs/CUTOVER.md`](./docs/CUTOVER.md)** — playbook for the
  shadow → live cutover; relevant historically + as a rollback
  reference.
- **[`docs/DATABASE.md`](./docs/DATABASE.md)** — Postgres setup,
  schema, peer-auth convention.
- **[`docs/AUDIT.md`](./docs/AUDIT.md)** — system audit (2026-05-07):
  profit-take, scale-up paths, strategy arsenal expansion.
- **[`docs/KALSHI_FIX_REQUEST.md`](./docs/KALSHI_FIX_REQUEST.md)** —
  draft email for FIX gateway access (Phase 4b).
- **[`docs/PLAN.md`](./docs/PLAN.md)** — original architectural
  plan (largely superseded by ARCHITECTURE.md).

## Confirmed Kalshi fee schedule

```
taker = ceil(0.07   * C * P * (1-P))   // dollars; ceil to nearest cent
maker = ceil(0.0175 * C * P * (1-P))   // 75% cheaper
```

In [`crates/core/src/fees.rs`](./crates/core/src/fees.rs). At p=$0.50
the round-trip taker fee is ~7%, maker round-trip ~1.75% — the gating
constraint on every strategy threshold.
