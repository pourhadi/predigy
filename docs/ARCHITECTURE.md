# Predigy architecture

This document is the source of truth for predigy's runtime
architecture, the data model, and the in-flight migration from the
fragmented multi-binary layout to the consolidated `predigy-engine`.

If something here disagrees with code, code is wrong (or this doc
is out-of-date — fix it in the same commit as the code change).

Written 2026-05-07. Migration in progress.

---

## TL;DR

- One process (`predigy-engine`) replaces 5+ separate trader binaries.
- One Kalshi connection (FIX-primary, REST-fallback) instead of N.
- One Postgres database replaces the per-strategy JSON state files.
- Strategy logic lives in module crates loaded into the engine.
- Position management is global: cross-strategy Kelly, shared kill-switch,
  shared book view, shared model_p history.

The migration is phased so the existing daemons keep running and
making fills throughout. The new engine is built alongside, parity-
verified per strategy, then the old daemons retire one at a time.

---

## Why we're refactoring

The current architecture (5 separate trader binaries + 4 curator
crons + dashboard, communicating through JSON files on disk) was
the right shape for "prove each lane can make a fill". It is the
wrong shape for what we want next:

1. **Rate-limit collisions.** Five processes hit Kalshi REST in
   parallel. They share a global rate-limit pool but coordinate via
   exponential backoff after the fact, which means we routinely
   eat 429s. Logs since 2026-05-06 show 50+ 429s/day.

2. **Stale beliefs.** Curators run on slow cron (3-24h). Traders
   bid using cached model_p that's hours old by the time it meets
   minute-fresh prices. wx-stat-curate runs at 02:30 UTC; the
   trader keeps acting on those numbers until the next 05:30
   curate. When 18Z NBM publishes at 23:00, no one notices until
   the curator's next 03:00 tick.

3. **Fragmented state.** Five OMS state files, five rule files,
   no transactional guarantees, no cross-strategy queries. Asking
   "what's our total weather exposure right now?" requires reading
   five files and reconciling them.

4. **No active position management.** Once filled, positions sit
   until settlement. No early exits on adverse model_p drift, no
   profit-take on saturation. We give back edge passively.

5. **Latency cap.** REST-only orders cap us at ~100-500ms per
   submit/cancel. For news-driven strategies (latency-trader on
   NWS alerts) and spread-capture (cross-arb), that latency IS the
   edge.

6. **No history.** Settled positions get wiped from `oms-state.json`
   files. No audit trail, no calibration data, no replayability,
   no analytics.

---

## Target architecture

```
                     ┌──────────────────────────────────────────┐
                     │              predigy-engine              │
                     │                                          │
                     │  ┌──────────┐    ┌──────────────────┐    │
   Kalshi WS ───────►│  │  market  │───►│ shared book view │    │
   Kalshi FIX ──────►│  │   data   │    └──────┬───────────┘    │
   Kalshi REST  ────►│  │  router  │           │                │
   Polymarket WS ──►─│  └──────────┘           ▼                │
   NWS alerts ──────►│                  ┌──────────────────┐    │
   NBM S3 (3h tick) ►│                  │ strategy modules │    │
                     │  ┌──────────┐    │  ┌────────────┐  │    │
                     │  │ shared   │◄───┼──┤ cross-arb  │  │    │
                     │  │   OMS    │    │  ├────────────┤  │    │
                     │  └────┬─────┘    │  │ stat       │  │    │
                     │       │          │  ├────────────┤  │    │
                     │       │          │  │ latency    │  │    │
                     │       ▼          │  ├────────────┤  │    │
                     │  ┌──────────┐    │  │ settlement │  │    │
                     │  │   FIX    │    │  ├────────────┤  │    │
                     │  │ executor │    │  │ wx-stat    │  │    │
                     │  └────┬─────┘    │  └────────────┘  │    │
                     │       │          └─────────┬────────┘    │
                     │       ▼                    │             │
                     │  ┌──────────────┐          │             │
                     │  │ rate-limited │          │             │
                     │  │   REST       │          │             │
                     │  │  (fallback)  │          │             │
                     │  └──────┬───────┘          │             │
                     │         │                  │             │
                     │         └──────┬───────────┘             │
                     │                ▼                         │
                     │      ┌───────────────────┐               │
                     │      │  PostgreSQL       │◄── psql       │
                     │      │  ─ markets        │◄── grafana    │
                     │      │  ─ positions      │◄── notebooks  │
                     │      │  ─ fills          │               │
                     │      │  ─ intents        │               │
                     │      │  ─ model_p_*      │               │
                     │      │  ─ rules          │               │
                     │      │  ─ calibration    │               │
                     │      │  ─ kill_switches  │               │
                     │      └───────────────────┘               │
                     └──────────────────────────────────────────┘
```

### Data flow at runtime

1. **Market data in** — Kalshi WS pushes book updates; the engine's
   market-data router decodes once, fans out to every strategy
   module that subscribed to that ticker.
2. **Strategy decision** — module reads its current beliefs and
   open positions from shared in-memory state, decides whether to
   submit an `Intent`.
3. **Intent → execution** — shared OMS dedupes via client-id,
   routes to FIX (preferred) or REST (fallback), persists the
   `intents` row.
4. **Fill arrives** — FIX `ExecutionReport` (or REST `/fills` poll)
   triggers a fill row + position update + portfolio re-mark.
5. **Settlement** — when Kalshi reports settled, position closes,
   realised P&L lands in `pnl_daily`, model_p history joins with
   outcome for the calibration view.

### Cross-module sharing

The shared in-memory state is the engine's value proposition. A
non-exhaustive list of what becomes free:

- **wx-stat updates Denver quantiles → stat-trader's Denver
  positions re-evaluate immediately.** No 3-hour staleness.
- **Cross-arb's Polymarket book view informs stat-trader's
  model_p.** Two sources of truth merged into one belief.
- **Latency-trader sees the same alert → can size against
  current portfolio exposure**, not just its own rule book.
- **Global Kelly accounting**: each strategy's marginal sizing
  knows about other strategies' open positions in correlated
  markets.

---

## Database

### Engine: PostgreSQL 16

Why Postgres over SQLite for this project:

| Need | Postgres | SQLite |
|---|---|---|
| Time-series queries on model_p history (millions of rows) | First-class; TimescaleDB extension available | Adequate up to ~hundreds of millions but slower at planning |
| Concurrent readers without WAL contention | Native | WAL helps but writers still block readers occasionally |
| External tooling (psql, Grafana, sqlx) simultaneously | Yes | File-locked |
| `LISTEN`/`NOTIFY` pub-sub | Yes | No |
| Slow-query log + EXPLAIN ANALYZE | Yes | Limited |
| Future-proof for multi-machine | Yes | No |

The cost is one extra service to keep running (`brew services
start postgresql@16` + launchd dependency on engine startup).
Acceptable.

### Connection / auth

- Local UNIX socket, peer auth as the operating-system user
  (`dan`). No password.
- Database name: `predigy`.
- Application connects via `postgresql:///predigy` (URL with no
  host falls through to UNIX socket).

### Migrations

`sqlx-cli` manages schema migrations under `migrations/*.sql`.
`sqlx migrate run` applies them in order.

Compile-time query checking via `sqlx::query!` macro: queries are
verified against the live schema at `cargo build` time, not
runtime. This catches schema drift before deploy.

### Schema (initial)

See `migrations/0001_initial.sql` for the canonical definition.
Summary:

- `markets` — ticker → metadata (settlement time, source, kind,
  payload JSON)
- `positions` — current open positions (ticker, side, qty,
  avg_entry, opened_at, strategy_id)
- `intents` — every order ever submitted (with status / reason /
  strategy, used as audit trail)
- `fills` — every fill ever (with intent_id FK, fee, raw venue
  data); `INSERT`-only
- `model_p_snapshots` — time series of model_p per (strategy,
  ticker, ts). Hypertable candidate via TimescaleDB; without it,
  index on (ticker, ts) is sufficient up to ~10M rows.
- `model_p_inputs` — raw probabilistic inputs (NBM quantile
  vectors, NWS forecasts, polymarket prices) keyed by (source,
  key, ts). Lets us replay calibration without re-fetching.
- `rules` — currently-active strategy rules. One row per
  (strategy, ticker). Strategy module updates these as curators
  refresh.
- `kill_switches` — per-strategy and global; `armed` boolean +
  reason + set_at + set_by.
- `calibration` — fitted Platt coefficients per (strategy,
  airport, month).

Materialised views (refreshed periodically or on demand):

- `pnl_daily` — daily realised + unrealised per strategy
- `position_summary` — current per-strategy exposure roll-up

### Backups

- `pg_dump predigy | gzip > /Users/dan/.config/predigy/backups/predigy-$(date +%F).sql.gz`
- Daily via launchd (`com.predigy.db-backup`)
- Retain 30 days; rotate via launchd cleanup
- For point-in-time recovery: enable WAL archiving when capital >
  $5K (overkill at current scale)

---

## FIX integration

The `predigy-kalshi-fix` crate (already 1,679 lines, complete
session/executor/messages/tags) becomes the engine's primary
order path.

### Wire format

Kalshi FIX 4.4 over TLS, RSA-PSS-SHA256 logon authentication
(same key material as the REST signer).

### Order routing policy

- **All hot-path orders** (cross-arb, latency-trader on NWS
  alerts, stat-trader fires) → FIX
- **Bulk metadata pulls** (market lists, settlement reports,
  initial position snapshot) → REST
- **FIX session disconnect** → orders fall back to REST until
  reconnect
- **REST 429 backoff in flight** → orders queue on FIX; non-
  critical metadata polls back off

### Throughput

- REST sustained: ~50 req/sec per IP (current cap we keep hitting)
- FIX sustained: ~hundreds/sec per session, sub-millisecond ack
- Saves 100-500ms per order on the hot path. For news-data
  latency strategies, that's the edge.

### What we still use REST for

- Market metadata refresh (not on the hot path)
- Settlement and position snapshots (housekeeping)
- Fills polling as a sanity-check against FIX `ExecutionReport`s
  (defence in depth for missed messages)
- Account balance polling for the dashboard

---

## Strategy modules

Each existing trader becomes a module crate under
`crates/strategies/`. All implement the same `Strategy` trait:

```rust
trait Strategy {
    fn name(&self) -> &'static str;
    fn id(&self) -> StrategyId;

    // Subscribe to market-data events.
    fn subscribed_markets(&self, db: &Db) -> Vec<MarketTicker>;

    // Called on every relevant book update.
    fn on_book(&mut self, market: &MarketTicker, book: &OrderBook,
               state: &mut StrategyState) -> Vec<Intent>;

    // Called when one of our orders fills.
    fn on_fill(&mut self, fill: &Fill, state: &mut StrategyState);

    // Called on a timer (per-strategy cadence) for re-evaluation
    // of held positions.
    fn on_tick(&mut self, state: &mut StrategyState) -> Vec<Intent>;

    // Called when external feeds the strategy depends on update
    // (NWS alert, NBM cycle publish, polymarket book change).
    fn on_external(&mut self, ev: &ExternalEvent,
                   state: &mut StrategyState) -> Vec<Intent>;
}
```

Strategies emit `Intent`s; the engine routes them through the
shared OMS to FIX/REST. Strategies don't talk to Kalshi directly.

### Strategy modules planned

| Module | Replaces | Notes |
|---|---|---|
| `crates/strategies/cross-arb` | bin/cross-arb-trader | Subscribes to Kalshi + Polymarket pairs, fires on cross-venue spread. |
| `crates/strategies/stat` | bin/stat-trader | model_p vs ask, Kelly-sized. Now sees model_p updates from wx-stat continuously. |
| `crates/strategies/latency` | bin/latency-trader | Subscribes to NWS alerts via the engine's external feed; fires faster via FIX. |
| `crates/strategies/settlement` | bin/settlement-trader | Pre-settlement mispricing capture. |
| `crates/strategies/wx-stat` | bin/wx-stat-curator | NBM updater — emits `model_p_snapshots` rows when new cycles publish. |
| `crates/strategies/wx-curator` | bin/wx-curator + bin/stat-curator + bin/cross-arb-curator | LLM-based rule producers. Now in-engine; output goes to `rules` table. |

### Active position management

Once all strategies share state, per-position re-evaluation
becomes trivial:

- On every book update for a held ticker, the strategy module
  recomputes whether it would still enter at current price + current
  model_p
- If `current_model_p < entry_model_p - drift_threshold` AND
  `position_age > grace_period`, flatten
- If `current_model_p > saturation_threshold` AND
  `unrealised_pnl > take_target`, scale out partial
- All of this is one `on_tick` call per minute per held position

---

## Migration phases

The migration is strictly additive through Phase 4. The existing
daemons keep running and making fills. We don't lose volume during
the work.

### Phase 0 — Setup (DONE 2026-05-07)

- [x] Install Postgres 16 via Homebrew
- [x] Create `predigy` database + role with peer auth
- [x] Add `sqlx`, `sqlx-cli` to the workspace
- [x] Verify connection from a test program
- [x] Document the install + connection in this doc

### Phase 1 — Schema + import tool (DONE 2026-05-07)

- [x] `migrations/0001_initial.sql` with all tables + indexes
- [ ] `migrations/0002_views.sql` with materialised views
      *(deferred — not blocking; views can be added when query
      patterns crystallise)*
- [x] `bin/predigy-import` reads existing JSON state files and
      bulk-inserts into the DB. Idempotent.
- [x] Run the import once. First-pass: 183 markets / 62 intents /
      175 rules.
- [x] Add a launchd job that runs `predigy-import` every 30 min so
      the DB stays in sync until Phase 5 flips the write path over.
      `com.predigy.import` plist + `deploy/scripts/predigy-import-run.sh`.

### Phase 2 — Engine skeleton + Postgres read path (DONE 2026-05-07)

- [x] New `crates/engine-core/` and `bin/predigy-engine/`.
      Engine boots, runs migrations, owns the OMS, supervises
      strategy modules. 14 unit + 12 integration tests.
- [x] Dashboard queries DB-derived state (per-strategy daily PnL,
      kill-switch, in-flight count) with JSON fallback for
      degraded-mode operation.
- [x] Engine exposes the `Strategy` trait + module registry.

### Phase 3 — First strategy ported (stat-trader)

- [x] **Phase 3.1**: `crates/strategies/stat/` implements
      `Strategy`. All legacy logic preserved verbatim. 9 unit tests.
- [x] **Phase 3.2**: Engine runs stat module in **shadow mode**
      by default — intents persist with status='shadow' in the DB
      and never reach Kalshi. Live-verified 2026-05-07: 45 shadow
      intents emitted from 61 subscribed markets in <1s; legacy
      stat-trader still trades unaffected.
- [ ] **Phase 3.3 (BLOCKED on FIX access)**: parity-verification
      tool — diff engine shadow intents vs legacy stat-trader fills
      across a 24-48h window. Tool can be built without FIX, but
      the cutover decision waits.
- [ ] **Phase 3.4 (BLOCKED on Phase 4)**: flip engine to Live
      mode + retire legacy stat-trader. Requires the FIX path.

### Phase 4 — Engine venue path: REST-primary + WS-push fills

The order-protocol decision was relitigated 2026-05-07. Earlier
plan said "FIX as primary, REST fallback". Updated plan: **REST
as primary, WS-push fills for sub-second execution feedback,
FIX as a switchover upgrade once Kalshi grants access**.

Why the change:
- Kalshi WS does NOT support order submit / cancel — those are
  REST-only by venue design. Confirmed against
  [docs.kalshi.com/websockets/websocket-connection](https://docs.kalshi.com/websockets/websocket-connection).
- Kalshi WS DOES push real-time fill / position / order-state
  events on authed channels (`fill`, `market_positions`,
  `user_orders`). This closes most of the latency gap REST-poll
  would otherwise leave: ~500ms poll latency drops to ~10ms
  push.
- For our 5 strategy lanes, FIX matters meaningfully only for
  latency-trader (NWS alerts) and cross-arb (cross-venue
  spread). cross-arb's bottleneck is the round-trip "submit
  leg 1 → fill notification → submit leg 2" — WS-push fills
  cuts that ~50% without Kalshi-side approval.
- FIX submit-side latency advantage (sub-ms vs REST 200ms) is
  marginal at our current capital scale ($50→$5K). It becomes
  load-bearing for latency-trader specifically when capital
  scales past ~$5K and per-fire size justifies the protocol
  upgrade.

#### Phase 4a — REST submitter + WS-push fills (NOT BLOCKED)

This is the production venue path the engine ships with. Builds
without any Kalshi-side approval; deliverable in days.

- [ ] **REST submitter worker** in `bin/predigy-engine/src/venue_rest.rs`.
      Polls the `intents` table for `status='submitted'` rows
      (Live mode), submits each via Kalshi REST
      `POST /portfolio/orders`, updates status on response.
      Rate-limited share of the global REST budget. Idempotent
      via the `client_id` PK + Kalshi's `client_order_id`
      header (Kalshi rejects duplicate client_order_ids on
      submit so the second submit collapses cleanly).
- [ ] **WS-push fill subscriber** in `bin/predigy-engine/src/exec_data.rs`.
      Subscribes the existing kalshi-md client to the authed
      channels `fill`, `market_positions`, `user_orders`. Maps
      each MdEvent into the OMS's `ExecutionUpdate` and calls
      `apply_execution`. Replaces what the legacy daemons do
      via REST-poll today.
- [ ] **REST cancel path**: same shape as submitter but for
      `status='cancel_requested'` rows, calls `DELETE
      /portfolio/orders/{order_id}`.
- [ ] Engine integration test: mock REST + WS, submit intent,
      observe fill push, position cascade, all transactional.

#### Phase 4b — FIX switchover (BLOCKED on Kalshi access)

The OMS already exposes `VenueChoice::{Rest, Fix}` so flipping
order routing is a config / per-intent decision, not a rewrite.

Status as of 2026-05-07: Kalshi FIX requires institutional-grade
onboarding. Operator emailing `institutional@kalshi.com`. See
"Kalshi FIX onboarding" section below + the email draft in
`docs/KALSHI_FIX_REQUEST.md`.

Once approved:
- [ ] Existing `predigy-kalshi-fix` crate updated to FIXT.1.1 +
      FIX 5.0 SP2 (currently coded against FIX 4.4 per pre-session
      docs; Kalshi's live spec is FIX 5.0 SP2). Schema diff is
      modest — most of the session-layer code transfers.
- [ ] Engine boots a FIX session at startup (in addition to the
      existing REST submitter). Logon with `ResetSeqNumFlag=true`
      (mandatory per spec for non-RT gateway).
- [ ] Order-routing policy: hot-path strategies (latency-trader,
      cross-arb) → FIX first, REST fallback on session loss.
      Other strategies → REST (FIX latency advantage is
      irrelevant to them and FIX's per-key single-connection
      constraint means we don't waste it on non-hot-path
      orders).
- [ ] Verify on stat-trader's order flow first (low volume,
      easy to inspect) before flipping the latency-sensitive
      lanes.

The REST submitter built in 4a stays as the fallback even after
4b lands — defence in depth for FIX session loss / network
glitches. It's not throwaway work.

### Phase 5 — Port remaining strategies

For each of cross-arb, latency, settlement, wx-stat, wx-curator,
stat-curator, cross-arb-curator:

- [ ] Implement strategy module against the trait
- [ ] Run dual-write for 24-48h
- [ ] Verify parity
- [ ] Flip reads, retire old binary

### Phase 6 — Active position management

- [ ] Per-position re-evaluation on book updates (in each
      strategy module)
- [ ] Adverse-drift exits with configurable threshold
- [ ] Profit-take saturation logic
- [ ] Global Kelly accounting (sizing knows about other
      strategies' open positions in correlated markets)
- [ ] Cross-strategy data sharing (wx-stat's model_p drift
      triggers stat-trader re-evaluation; cross-arb's Polymarket
      view feeds stat-trader's belief)

### Phase 7 — Retire scaffolding

- [ ] Remove the JSON-output compat layer once all strategies
      port (the dashboard reads DB; nothing else needs JSON files)
- [ ] Delete legacy binaries from the workspace
- [ ] Consolidate launchd plists to just two: predigy-engine +
      db-backup

---

## Operational runbook

### Daily ops

- Engine should be running under launchd; verify with
  `launchctl print gui/$(id -u)/com.predigy.engine`
- Check the dashboard at `http://127.0.0.1:8080` for daily P&L,
  open positions, recent fills
- Slow-query log: `tail -F ~/Library/Logs/predigy/postgres.log`
- Engine log: `tail -F ~/Library/Logs/predigy/engine.stderr.log`

### When something looks wrong

1. **Lots of 429s in engine log** → REST poll cadences are
   misaligned; check `pg_stat_activity` for long-running queries
   pinning the rate-limit budget
2. **No fills for hours when there should be** → check FIX session
   state (`SELECT * FROM kill_switches WHERE armed=true`) and
   strategy heartbeats
3. **Positions diverged from broker** → run reconciliation:
   `predigy-engine reconcile` pulls the broker snapshot and
   reports diffs
4. **Database disk full** → rotate old `model_p_snapshots`:
   `DELETE FROM model_p_snapshots WHERE ts < now() - INTERVAL '90 days'`

### Backups

- Automatic daily `pg_dump` to `~/.config/predigy/backups/`
- Manual backup before risky changes:
  `pg_dump predigy | gzip > backup-$(date +%F-%H%M).sql.gz`
- Restore: `gunzip < backup-FILE.sql.gz | psql predigy`

### Kill switch

- File-based fallback (works even if engine is hung):
  `touch ~/.config/predigy/kill-switch.flag`
- DB-based (preferred when engine is responsive):
  `INSERT INTO kill_switches (scope, armed, reason)
   VALUES ('global', true, 'manual: <reason>')`
- Engine flushes positions and refuses new entries when armed
- To clear: `DELETE FROM kill_switches WHERE scope='global'` AND
  `rm ~/.config/predigy/kill-switch.flag`

### Schema migrations

- New migration: `cd crates/predigy-engine && sqlx migrate add <name>`
- Edit the generated `.sql` file
- Apply: `sqlx migrate run` (also runs at engine startup)
- Roll back: not supported by sqlx; write a forward-only "fix"
  migration if needed

---

## Decision log

Decisions made during the architecture design, with rationale, so
future-us doesn't redo them.

### 2026-05-07: Postgres over SQLite

- Chose Postgres for time-series + concurrency + external tooling.
- SQLite would've worked for prototype scale but compounds
  technical debt as data grows.
- Setup cost (1 brew install + launchd integration) is one-time;
  benefits compound.

### 2026-05-07: sqlx over diesel/sea-orm

- Compile-time query verification against live schema (run
  migrations against dev DB, sqlx checks types at `cargo build`).
- Async-native, plays well with our tokio runtime.
- No ORM weight. Raw SQL with parameter binding.
- Migrations via `sqlx-cli`.

### 2026-05-07: peer auth on UNIX socket

- Single-machine, single-OS-user deployment for the foreseeable
  future.
- No password to store / rotate / leak.
- If the trading box is ever compromised, the attacker has DB
  access regardless of password (everything else is breached too).

### 2026-05-07: One umbrella binary, modules not microservices

- Microservices buy isolation we don't need (we control all the
  modules; they all run on one machine; they all share latency
  budget).
- One process means shared in-memory state (book view, positions,
  model_p) without IPC overhead.
- One Kalshi connection means rate-limit collisions go away.
- One FIX session means sub-millisecond order ack on the hot path.

### 2026-05-07: FIX-primary, REST-fallback for orders

- FIX latency edge matters for cross-arb spread capture and
  latency-trader's news-driven fires.
- REST stays as fallback because the hard parts of FIX (network
  failures, session loss, dropped messages) are real and a
  fallback path is defence-in-depth.
- All non-hot-path operations (metadata refresh, settlement
  snapshots) stay REST because there's no latency benefit.

### 2026-05-07: Migration is phased, not big-bang

- Existing daemons keep running through Phase 1-4.
- Each strategy ports independently with parity verification.
- We don't lose fill volume to the refactor.
- If the refactor stalls midway, the system stays operational.

---

## Kalshi FIX onboarding

FIX is gated. Per `docs.kalshi.com/fix/connectivity` + the
institutional onboarding flow at `institutional.kalshi.com`,
the process is:

1. **Institutional account.** Apply via
   `institutional.kalshi.com` (entity application form). Required:
   entity formation docs, W-9 / W-8, source of funds, beneficial-
   owner IDs (10%+). All English or certified translation.
2. **Email `institutional@kalshi.com`** with the FIX-access
   request. Reference the institutional account, summarise
   technical sophistication and current REST trading activity,
   ask for the application path + minimum-activity criteria.
3. **They send credentials**: a UUID-format FIX API Key (used
   as `SenderCompID`) plus the certificate to pin on the
   initiator side.
4. **Connect**:
   - Demo: `fix.demo.kalshi.co` (test-driven onboarding before
     prod credentials)
   - Prod: `mm.fix.elections.kalshi.com`
   - 5 gateways:
     | Port | TargetCompID | Purpose |
     |---|---|---|
     | 8228 | `KalshiNR` | Order Entry, no retransmission |
     | 8230 | `KalshiRT` | Order Entry with retransmission + RFQ (institutional) |
     | 8229 | `KalshiDC` | Drop Copy (historical execution queries) |
     | 8231 | `KalshiPT` | Post Trade (settlement streams, institutional) |
     | 8232 | `KalshiRFQ` | Market Maker quoting |
   - Wire: **FIXT.1.1 + FIX 5.0 SP2**, TLS 1.2+ mandatory,
     AWS Network Load Balancer cipher suites.
   - Logon: `ResetSeqNumFlag=true` for non-retransmission
     gateways (NR / DC / PT). Each API key is single-connection.

For predigy, the relevant gateway is **Order Entry NR (port
8228, KalshiNR)** — non-RT is fine for our latency profile;
ResetSeqNumFlag-on-logon avoids the retransmission state-
machine complexity.

The existing `crates/kalshi-fix` was coded against FIX 4.4 (per
older docs). Verify against the current spec on Kalshi's
sending the credentials — likely needs the FIXT.1.1 session-
layer header but most application-layer message types
(NewOrderSingle / OrderCancel / ExecutionReport) are unchanged
between 4.4 and 5.0 SP2.

## Glossary

- **Engine**: the consolidated `predigy-engine` binary running all strategies.
- **Strategy module**: a Rust crate under `crates/strategies/` implementing the `Strategy` trait.
- **Intent**: a desire to submit an order, before idempotency check + venue routing.
- **Fill**: a confirmed execution at the venue, with price + qty + fee.
- **model_p**: a strategy's calibrated probability that a given binary market resolves YES.
- **Hot path**: order submission and cancellation; latency-sensitive.
- **Curator**: a process (or module) that produces strategy rules; LLM-based or quantitative.
- **Calibration**: post-hoc fit of (raw_p, observed_outcome) pairs to correct systematic bias.
