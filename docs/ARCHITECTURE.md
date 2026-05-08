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
- One Kalshi market-data WS connection plus one authed fill/position WS
  connection; order entry is currently REST-first until FIX access exists.
- One Postgres database replaces the per-strategy JSON state files.
- Strategy logic lives in module crates loaded into the engine.
- Position management is global: cross-strategy Kelly, shared kill-switch,
  shared book view, shared model_p history.

Current caveat: the 2026-05-07 profitability audit found scale blockers
in exit cap handling, reconciliation, tick scheduling, mark-to-market
risk, and stale-frame handling. See
[`PROFITABILITY_AUDIT_PLAN.md`](./PROFITABILITY_AUDIT_PLAN.md) before
raising caps.

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
   persists the `intents` row, and the REST submitter drains live
   `submitted` rows to Kalshi. FIX routing is planned but blocked on
   Kalshi access.
4. **Fill arrives** — the authed WS fill channel triggers a fill row +
   position update. REST reconciliation/catch-up remains a required
   hardening item.
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
- `opportunity_observations` — append-only scanner records. These
  are intentionally non-executing: scanner jobs write current edge /
  skip evidence here and never write `intents`, `fills`, or
  `positions`.
- `calibration_reports` — archived reliability snapshots by
  strategy/window (`n_predictions`, `n_settled`, Brier, log-loss,
  bins, diagnosis). The dashboard's `/calibration` view reads the
  latest row per strategy.

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
| `crates/strategies/wx-stat` | bin/wx-stat-curator | Consumes NBM-curated weather rules directly; curator gates same-day/past temperature markets through airport-local-day ASOS observed extremes before forecast scoring and can emit a JSON coverage/skip report. `predigy-import` must not mirror `wx-stat-rules.json` into `stat`. |
| `crates/strategies/wx-curator` | bin/wx-curator + bin/stat-curator + bin/cross-arb-curator | LLM-based rule producers. `stat-curator --shadow-db` writes disabled `stat` rules plus `model_p_snapshots` for calibration evidence; it does not enable live `stat` trading. |
| `bin/opportunity-scanner` | launchd one-shot | Read-only scanner that evaluates configured arb books via shared pure evaluators and writes only `opportunity_observations`. |
| `bin/predigy-calibration` | launchd one-shot | Public settlement-outcome backfill plus reliability report writer for `calibration_reports`. |

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
      **Disabled 2026-05-08 after live engine cutover** because the
      stale legacy JSON mirror was re-enabling disabled `stat` rules.
      The wrapper now exits unless `PREDIGY_ENABLE_LEGACY_IMPORT=1`
      is set for an explicit one-off migration.

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
- [x] **Phase 3.4**: flip engine to Live mode + retire legacy
      stat-trader. Cutover happened via REST-primary execution, not FIX.

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

#### Phase 4a — REST submitter + WS-push fills — **SHIPPED 2026-05-07**

This is the production venue path the engine ships with. Built
without any Kalshi-side approval; the venue path is live whenever
`PREDIGY_ENGINE_MODE=live`.

- [x] **REST submitter worker** in `bin/predigy-engine/src/venue_rest.rs`.
      Polls `intents WHERE status='submitted'` every
      `PREDIGY_VENUE_REST_POLL_MS` (default 250ms), submits each
      via Kalshi V2 `POST /portfolio/events/orders`, flips to
      `acked` on response (stamping `venue_order_id`) or
      `rejected` on a 4xx with the venue body preserved in
      `intent_events.venue_payload`. 5xx and 429 are retried on
      the next poll; transport errors leave the row queued.
      Idempotent via the `client_order_id` we send.
- [x] **WS-push fill subscriber** in `bin/predigy-engine/src/exec_data.rs`.
      Dedicated `kalshi-md` connection subscribed to the authed
      `fill` and `market_positions` channels (empty
      `market_tickers` covers all the user's markets). Maps each
      `FillBody` to an `ExecutionUpdate` and calls
      `Oms::apply_execution`. Computes `cumulative_qty` by
      reading the originating intent's current cumulative + the
      incremental fill qty, picks `Filled` vs `PartialFill`
      accordingly. Replaces the legacy daemons' REST `/portfolio/fills`
      poller (~10ms median vs ~500ms).
- [x] **Fill dedup hardening** in `oms_db.rs`. Replayed WS fills
      (across reconnects, or arriving alongside a belt-and-
      suspenders REST poll) are caught at the top of
      `apply_execution` via a `SELECT 1 FROM fills WHERE
      venue_fill_id = $1` check before the position cascade. The
      existing `fills.venue_fill_id` UNIQUE index is the second
      line of defence. Covered by
      `duplicate_venue_fill_id_is_idempotent` in
      `tests/oms_integration.rs`.
- [x] **REST cancel path**. Polls `intents WHERE
      status='cancel_requested'`, calls `DELETE
      /portfolio/events/orders/{order_id}`. 404s are treated as
      "already gone, mark cancelled". Cancels racing ahead of the
      original submit ack (no `venue_order_id` yet) skip and retry
      on the next tick.
- [x] **Engine mode wiring** in `config.rs`. `PREDIGY_ENGINE_MODE`
      env var selects `Shadow` (default) or `Live`. Shadow writes
      intents at `status='shadow'` and the REST submitter's poll
      query never sees them — same code, no special-case branch.

Latency picture after this lands: REST submit ~200ms (network-
bound), WS push fills ~10ms — total submit-to-fill ~210ms median.
FIX would shave the submit side to <1ms; everything else stays.

The 28-test suite (`cargo test -p predigy-engine`) covers the
submit/cancel/fill state-machine + REST → V2 wire mapping for the
four (Side × Action) cases.

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

Strategy ports (one crate per strategy under `crates/strategies/`):

- [x] **stat-trader** → `crates/strategies/stat` (Phase 3.2,
      shipped 2026-05-06).
- [x] **settlement-trader** → `crates/strategies/settlement`
      (Phase 5, shipped 2026-05-07). Pure discovery-driven —
      operator no longer seeds tickers at boot. Ships with the
      engine's new discovery service infrastructure (see below).
- [x] **latency-trader** → `crates/strategies/latency`
      (Phase 5, shipped 2026-05-07). NWS-alert driven; rules
      loaded from JSON file at `PREDIGY_LATENCY_RULE_FILE`. Ships
      alongside the new external-feed dispatcher infrastructure
      (see below).
- [x] **cross-arb-trader** → `crates/strategies/cross-arb`
      (Phase 5, shipped 2026-05-07). Pair-file driven; pairs
      come from `PREDIGY_CROSS_ARB_PAIR_FILE` (curator's output)
      and are hot-reloaded via the new pair-file service. Ships
      alongside Polymarket WS support in the external-feed
      dispatcher.
- **Curators stay external by design** (wx-stat, wx-curator,
  stat-curator, cross-arb-curator). These are scheduled (every
  10 minutes to 4 hours) Anthropic-driven processes that write
  rules to Postgres or pair files; the engine just consumes
  their output. Folding them into the engine binary would couple
  hot-path latency to their LLM-call latency for no benefit.
  They keep their own launchd plists.

#### Discovery service (shipped 2026-05-07)

Periodic Kalshi-REST scan that auto-feeds dynamic market sets to
strategies. Architecture:

- `engine_core::DiscoverySubscription` — declarative config a
  strategy emits at registration (series, interval,
  max-secs-to-settle, require_quote).
- `Strategy::discovery_subscriptions()` — default empty;
  strategies override to opt in.
- `Event::DiscoveryDelta { added, removed }` — fired into the
  supervisor on each tick where the universe changed.
- `bin/predigy-engine/src/discovery_service.rs` — spawns one
  worker per (strategy, subscription) pair; paginates the Kalshi
  REST scan, filters by `expected_expiration_time` (preferred for
  per-event games) falling back to `close_time`, diffs against
  the prior tick.
- `MarketDataRouter::AddTickers` command + `command_tx()`
  handle — discovery worker auto-registers new tickers with the
  router so book updates start flowing before the strategy sees
  the discovery delta.
- `kalshi-rest`'s `send_with_retry` (PR #29) handles 429s
  transparently underneath all REST calls — no per-caller retry
  logic needed.

The settlement strategy is the canonical consumer. With the
discovery service running, the engine picks up newly-listed games
within one polling interval (60s default); operator restart is no
longer the bottleneck for sports-market entries.

#### External-feed dispatcher (shipped 2026-05-07)

Single point of contact for non-Kalshi data feeds — today NWS
alerts, future Polymarket book / NBM cycle publish.

- `bin/predigy-engine/src/external_feeds.rs` — spawns each feed
  at most once, fans events out to every supervisor that opted in
  via `Strategy::external_subscriptions() -> ["nws_alerts"]`.
  Translates `predigy_ext_feeds::NwsAlert` → engine-core's
  vendor-agnostic `NwsAlertPayload` shim at the boundary so
  engine-core stays free of the ext-feeds dep.
- Configured via env vars: `PREDIGY_NWS_USER_AGENT` (required —
  NWS refuses connections without identifying contact info),
  `PREDIGY_NWS_STATES`, `PREDIGY_NWS_POLL_MS`,
  `PREDIGY_NWS_SEEN_PATH` (cross-restart dedup).
- Latency strategy is the first consumer. Without
  `PREDIGY_NWS_USER_AGENT` set the engine logs a warning at boot
  and skips spawning the feed; latency-strategy supervisor still
  comes up but never receives an event.
- Cross-arb is the second consumer (Polymarket feed). The
  dispatcher exposes a `PolyCommandTx` handle so the pair-file
  service can dynamically extend the asset-id subscription set
  as new pairs land in the curator's output file.

#### Pair-file service (shipped 2026-05-07)

Watches the cross-arb-curator's output file
(`PREDIGY_CROSS_ARB_PAIR_FILE`) for changes and emits
`Event::PairUpdate` to the cross-arb supervisor. Mtime-poll based
(default 30s); suitable cadence given the curator runs on a
10-minute interval.

On each detected change:
1. **Router subscribe** — added Kalshi tickers via
   `RouterCommand::AddTickers`.
2. **Polymarket subscribe** — added asset_ids via
   `PolyFeedCommand::AddAssets`.
3. **Polymarket prune** — removed asset_ids drop from the saved-
   sub set so reconnects don't re-subscribe to dropped pairs
   (Poly WS has no in-band unsubscribe per the docs).
4. **Strategy delta** — `Event::PairUpdate { added, removed }`
   to the cross-arb supervisor's queue.

File format matches the legacy daemon: `KALSHI_TICKER=POLY_ASSET_ID`
per line, `#` comments + blanks tolerated.

#### Migration plan per strategy

For each remaining strategy:

1. Implement strategy module against the trait.
2. Run dual-write for 24–48h: legacy daemon keeps trading; engine
   runs in `EngineMode::Shadow` so it persists intents at
   `status='shadow'` without sending to the venue. Compare the
   two ledgers offline.
3. Verify parity (same fires, same sizes, same client_ids).
4. Flip the engine to `Live` for that strategy + disable the
   legacy launchd job.

### Phase 6 — Active position management

- [x] **Per-position re-evaluation on book updates** (Phase 6.1
      in stat-trader, shipped 2026-05-07). Strategy maintains an
      in-memory position cache (refreshed on Tick from
      `Db::open_positions(Some(STRATEGY_ID.0))`); on each
      `Event::BookUpdate` the strategy re-evaluates open
      positions for that ticker against current mark.
- [x] **Adverse-drift + profit-take exits**:
      - stat-trader (Phase 6.1, 8¢ / 5¢ defaults).
      - cross-arb-trader (Phase 6.2, 5¢ / 4¢ defaults — tighter
        because cross-arb scalps smaller convergences).
      - Both use the same pattern: in-memory CachedPosition map
        refreshed on the configured cadence (default 60s);
        evaluate_exit() runs alongside entry on each
        Event::BookUpdate; closing IOC at the current mark with
        idempotent-cid `<strategy>-exit:{ticker}:{side}:{tag}:...`.
- [x] **Global notional cap across strategies** (Phase 6.2).
      `RiskCaps::max_global_notional_cents` enforces an
      engine-wide ceiling in `oms_db::check_caps`; rejects with
      `RejectionReason::NotionalExceeded { scope: "global" }`.
      0 disables the global gate (per-strategy caps still apply).
      Default shake-down: $15 global vs 4×$5 per-strategy so it
      actually binds.
- [x] **latency-trader force-flat** (Phase 6.2). Time-based
      exit only — latency has no book subscription so it can't
      do mark-aware TP/SL. `LatencyConfig::max_hold_secs`
      (default 30 min); 0 disables. On each Tick, the strategy
      walks open positions and force-flats any held longer than
      max_hold via a wide IOC at `force_flat_floor_cents` (1¢
      default — any standing bid takes us). Idempotent cid
      `latency-flat:{ticker}:{side}:{day_bucket:08x}`.
- [ ] settlement-trader exits intentionally skipped — Kalshi
      auto-settles binary outcomes at $1/$0, so the strategy
      doesn't need an explicit close.
- [x] **Cross-strategy event bus** (Phase 6.2 final, shipped
      2026-05-07). `CrossStrategyEvent::{PolyMidUpdate,
      ModelProbabilityUpdate}` + topic-based fan-out. Producers
      call `state.publish_cross_strategy(...)` (non-blocking
      try_send); consumers subscribe via
      `Strategy::cross_strategy_subscriptions()` and receive
      `Event::CrossStrategy { source, payload }`. The dispatcher
      task in `bin/predigy-engine/src/cross_strategy_bus.rs`
      routes by topic and self-filters (producer doesn't get its
      own emission). Live wiring: cross-arb publishes poly-mid;
      stat subscribes and currently log-only — augmenting stat's
      belief with poly-mid is a future enhancement.

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
3. **Positions diverged from broker** → full reconciliation is a
   scale-blocking TODO. Compare Postgres positions/intents against
   Kalshi manually and keep the kill switch armed until repaired.
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
- Engine refuses new entries when armed. Existing positions are not
  auto-flattened by the current kill switch.
- To clear: `DELETE FROM kill_switches WHERE scope='global'` AND
  truncate `~/.config/predigy/kill-switch.flag` to empty.

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
- A future FIX session should give sub-millisecond order ack on the hot
  path once Kalshi access is available.

### 2026-05-07: REST-first orders, FIX planned

- FIX latency edge matters for cross-arb spread capture and
  latency-trader's news-driven fires.
- Current live order entry is REST. FIX remains blocked on Kalshi
  institutional onboarding.
- The REST path will remain useful after FIX for fallback and non-hot-path
  order operations, but the engine must first complete reconciliation and
  exit/risk hardening.
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
