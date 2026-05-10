# Session Handoff Notes

> **Read this first.** Operational orientation for any new Claude
> Code session picking up this codebase. Captures *what is currently
> true* — not what's planned. Update at end of session.

## What the user is doing

Building an automated trading system on Kalshi prediction markets.
Started with $50, scaling up through demonstrated edge. The user
wants:

- Forward motion. Decide and execute; don't over-ask.
- Money first, optimization later. Deployable strategies beat
  unbuilt theory.
- No fallbacks. Find the root cause; fix it.
- Comprehensive production-ready code. No demos.

## What is running RIGHT NOW (2026-05-09, post arb-scaling raise)

```
launchctl list | grep predigy
```

| Job | What it does | State |
|---|---|---|
| `com.predigy.engine` | Consolidated trader. Owns OMS, market data, exec, **6 active strategies** post-audit. | running, mode=Live |
| `com.predigy.cross-arb-curate` | Anthropic-driven Kalshi×Polymarket pair curator. 2-min cron (post-Phase-A). | scheduled |
| `com.predigy.stat-curate` | model_p curator for stat strategy. | scheduled |
| `com.predigy.wx-curate` | NWS-state-aware weather rule curator. (Was for the now-disabled latency strategy; kept running so rules don't go stale if latency is re-enabled later.) | scheduled |
| `com.predigy.wx-stat-curate` | NBM-quantile probabilistic weather rules. Hourly post-Phase-A. | scheduled |
| `com.predigy.dashboard` | HTTP/HTML dashboard at port 8080; includes `/calibration`. | running |
| `com.predigy.opportunity-scanner` | Observation-only scanner writing `opportunity_observations`; no OMS/orders. | scheduled every **5m** (post-2026-05-09) |
| `com.predigy.calibration` | Settlement sync + reliability report writer. | scheduled hourly |
| `com.predigy.arb-config-curate` | Validates implication-arb / internal-arb configs against Kalshi state, drops settled, seeds new active ladders + 2-leg families. | scheduled every 30m + RunAtLoad (post-2026-05-08) |
| `com.predigy.paper-trader` | Shadow-executes stat-curator rules vs live Kalshi prices into `paper_trades`; reconciles on settlement. **No orders submitted.** Evidence layer gating `stat` re-enable. | scheduled every 5m (post-2026-05-09) |

`com.predigy.import` is intentionally **disabled**. With legacy traders
retired, the JSON mirror was stale and re-enabled disabled `stat` rules from
`stat-rules.json` on every import tick.

**Active engine strategies (5, post-2026-05-09 wx-stat halt):**
`stat`, `settlement`, `cross-arb`, `internal-arb`, `implication-arb`.

**Disabled engine strategies (5):** `wx-stat`, `book-imbalance`,
`variance-fade`, `latency`, `news-trader`. Disabled by unsetting
their config env vars in `~/.zprofile` (engine skips registration
when `PREDIGY_*_CONFIG` / `PREDIGY_*_RULE_FILE` /
`PREDIGY_*_ITEMS_FILE` is unset). See `docs/AUDIT_2026-05-08.md`
for verdicts and re-enable conditions.

`wx-stat` was halted 2026-05-09 17:45 UTC after the first 11
cleanly settled trades came in 3W/8L (realized -$17.21). YES-side
hit 0/5 — strategy is structurally negative-EV in production at
current calibration. Re-enable only after a paper-trading run
shows positive after-fee EV over ≥30 trades.

Retired (post-cutover): `latency-trader`, `settlement-trader`,
`stat-trader`, `cross-arb-trader`. As of the 2026-05-08 ops cleanup they
are booted out and persistently disabled with `launchctl disable`.
Plists still on disk under `deploy/macos/`; Phase 7 will delete them once
≥1 week stable.

## The cutover (2026-05-07 07:45 UTC)

- Stopped the four legacy trader daemons.
- Set `PREDIGY_ENGINE_MODE=live` + `DATABASE_URL=postgresql:///predigy`
  in `~/.zprofile`.
- Bootstrapped `com.predigy.engine.plist`.
- Engine booted in Live mode, subscribed 68 stat-rule markets via
  WS, started firing.
- **Bug surfaced live**: Kalshi V2 rejects `client_order_id`
  containing `.`. Engine ports embedded raw tickers like
  `KXBRAZILINF-26APR-T4.30` → 10 intents got `400 invalid_parameters`.
  Fixed via `engine_core::intent::cid_safe_ticker(...)` (commit
  `0c05c40`); rebuilt + restarted in ~5 minutes; kill-switch
  armed during the patch window.
- Skipped the `docs/CUTOVER.md` shadow-mode dual-write phase per
  operator direction. Live verification is what the next 24h is for.

## Where money lives

- **Kalshi production account**: $100 deposited, ~$83 cash as of
  2026-05-09 01:00 UTC. `KALSHI_KEY_ID` in `~/.zprofile`; PEM at
  `~/.config/predigy/kalshi.pem`.
- **Capital caps (2026-05-09 arb-scaling raise)**:
  - `max_notional_cents` per strategy: **$80** ($8000¢) — was $40
  - `max_global_notional_cents`: **$200** ($20000¢) — was $120
  - `max_daily_loss_cents`: **$20** — was $10
  - `max_contracts_per_side`: **100** — was 20
  - `max_in_flight`: **80** — was 40
- **Per-strategy override clamps** (calibration-unproven strategies
  pinned at the prior shake-down sizing):
  - `wx-stat`: `MAX_NOTIONAL_CENTS=4000`, `MAX_OPEN_CONTRACTS_PER_SIDE=20`,
    `MAX_DAILY_LOSS_CENTS=1000`
  - **Lift these once Brier/log-loss reports show wx-stat
    positive-EV after fees over ≥30 unforced closed trades.**
- See `docs/STATE_LOG.md` for the full timeline of cap changes.

## Kill switch (panic button)

```sh
echo armed > ~/.config/predigy/kill-switch.flag   # ARM (refuse new entries)
: > ~/.config/predigy/kill-switch.flag            # DISARM (truncate)
```

Engine + dashboard both poll the file every 5 seconds. Engine logs
"kill-switch: ARMED" when it sees a non-empty flag.

## Fill-growth / calibration evidence additions (2026-05-08)

Implemented in the repo for the post-cleanup fill-growth plan:

- `opportunity_observations` and `calibration_reports` additive DB
  tables (`migrations/0003_*`).
- `bin/opportunity-scanner`: observation-only. It evaluates the
  configured `implication-arb` and `internal-arb` books with the same
  pure evaluators the live strategies use, ingests `wx-stat` coverage
  reports, records settlement configured-series observations, and writes
  only `opportunity_observations`.
- `bin/predigy-calibration`: syncs public Kalshi market outcomes for
  predicted tickers and writes Brier/log-loss reliability reports.
- Dashboard `/calibration` and `/calibration/summary.json` read the
  latest `calibration_reports` rows.
- `stat-curator --shadow-db` writes disabled `stat` rules plus
  `model_p_snapshots`; this starts collecting evidence without
  re-enabling live `stat` trading.
- `com.predigy.opportunity-scanner` and `com.predigy.calibration` are
  bootstrapped. Scanner cadence **5m** (post-2026-05-09; was 15m) with
  paced public orderbook fetches to avoid spending live REST rate budget.
- `com.predigy.arb-config-curate` (post-2026-05-08, PR #38) refreshes
  `implication-arb-config.json` and `internal-arb-config.json` from
  live Kalshi `status=open` snapshots every 30m, drops settled
  tickers, seeds new monotonic-ladder pairs and 2-leg families.
  Strategies hot-reload via mtime poll. Initial bootstrap expanded
  implication-arb 12 → 248 pairs; internal-arb 2 → 60 families.

## Auto-refresh + scaling timeline

See **`docs/STATE_LOG.md`** for the append-only chronological record
of every operational change to the running fleet (cap raises, plist
edits, daemon adds/removes). That doc is the durable signal for
"what changed and why"; this file (SESSIONS.md) is the "now"
snapshot.

## Strategy-by-strategy state

### `stat` (statistical model probability vs ask)

- 0 enabled rules in `rules` table (Postgres) as of the 2026-05-08 ops
  cleanup. `stat-curate` may still write `stat-rules.json`, but
  `predigy-import` is disabled so stale/unproven econ rules do not get
  mirrored back into DB trading state. `wx-stat-curate` feeds the
  dedicated `wx-stat` strategy directly.
- Phase 6.1 active exits: take-profit 8¢ / stop-loss 5¢, defaults.
  Closing IOCs use idempotent
  `stat-exit:{ticker}:{side}:{tp|sl}:{minute_bucket}` cids.
- 2026-05-07 churn fix: entries are now same-day-only by default,
  exits are evaluated before entries, and the strategy will not enter
  while any same-ticker position is open or during the post-exit
  re-entry cooldown. This specifically stops the observed buy/instant
  stop-loss loop in stale/non-same-day econ rules.
- 2026-05-07 safety note: `stat` and `wx-stat` were halted after a
  same-day SFO high-temperature rule bought YES on a below-62 market after
  observed high had already reached 64°F. `wx-stat-curator` now gates
  same-day/past temperature markets through ASOS observed extremes over the
  airport's local Kalshi settlement day before forecast/NBM scoring; current
  local-day pulls bypass the observation cache so intraday extrema cannot go
  stale. A second wx-stat bug on PHX below-98 for May 8 found that NBM used
  max hourly probability for all strike directions; daily-high `less` and
  daily-low `greater` now use the constraining all-hours probability instead.

### `wx-stat` (NBM probability vs weather ask)

- Consumes `~/.config/predigy/wx-stat-rules.json` directly.
- New entries are fail-closed unless curator rules carry
  `settlement_date` and `generated_at_utc`. By default the strategy only
  trades rules whose settlement date equals today's local date and whose
  curator timestamp is no older than 6h.
- 2026-05-07 same-day fix: temperature-market settlement date now comes
  from the Kalshi event-ticker date suffix (`26MAY07`) rather than naive
  UTC `occurrence_datetime`, which can land on the following UTC date for
  US local-day temperature markets.
- 2026-05-08 calibration error analysis: `wx-stat` shadow ingestion is live,
  but most settled evidence came from the pre-fix UTC/local-day prediction
  bug. Calibration reports now exclude date-mismatch legacy rows and mark the
  lane `insufficient_clean_settled_samples` (latest: 11 clean settled tickers,
  159 excluded settled snapshots). `wx-stat-fit-calibration` now defaults to
  latest clean record per ticker plus regularized monotone Platt fits with
  global fallbacks, but no calibration file should be written until at least
  30 clean settled samples exist.

### `settlement` (sports tape-reading near close)

- Pure discovery-driven; no static ticker list. Engine's discovery
  service polls Kalshi `/markets?series_ticker=...` for the standard
  sports basket every 60s, auto-registers new markets with the
  router, pushes `Event::DiscoveryDelta` to the strategy.
- No active exits — Kalshi auto-settles binary outcomes at $1/$0.

### `latency` (NWS-alert lift on weather markets) — **DISABLED 2026-05-08**

- `PREDIGY_LATENCY_RULE_FILE` commented out in `~/.zprofile`.
- Mechanism audit verdict: structurally infeasible without FIX +
  co-located VPS. Public NWS RSS + REST submit puts us 200-500ms
  behind FIX takers. Tier-3 1¢ force-flat floor turns every
  non-converging long into max-loss.
- **Re-enable condition**: B4 (Kalshi institutional FIX access)
  approved AND us-east-2 VPS deployed. Engine code is fine; it's
  the path that's wrong.

### `cross-arb` (Kalshi vs Polymarket convergence)

- Pair-driven. Pairs come from `PREDIGY_CROSS_ARB_PAIR_FILE`
  (default: `~/.config/predigy/cross-arb-pairs.txt`), curated by
  `cross-arb-curate`. Pair-file service polls mtime; hot reload.
- Phase 6.2 active exits: take-profit 5¢ / stop-loss 4¢ (tighter
  than stat because cross-arb scalps smaller convergences).
- **2026-05-08 audit**: `min_edge_cents` raised 1 → 3. Round-trip
  taker fee at 30-70¢ contracts is 1-2¢; firing at 1¢ raw edge
  is buying a fee-loss. Override via
  `PREDIGY_CROSS_ARB_MIN_EDGE_CENTS`.
- **2026-05-08 cleanup**: 18 stale `venue-reconcile` rows purged
  from `positions` table. They were stranding cross-arb's contract
  cap with phantom 50¢ entries. Later the same ops pass reconciled
  Postgres open positions to Kalshi `/portfolio/positions`; aggregate
  DB-vs-venue position mismatch count was verified at 0.
- Cross-strategy bus: cross-arb publishes `PolyMidUpdate` for
  paired markets; stat subscribes (currently log-only — augmenting
  belief from poly-mid is a future enhancement).

### `book-imbalance` — **DISABLED 2026-05-08**

- `PREDIGY_BOOK_IMBALANCE_CONFIG` commented out in `~/.zprofile`.
- Mechanism audit verdict: displayed-touch quantity has no proven
  alpha on Kalshi (makers cancel constantly); strategy demonstrably
  bought both YES@85 and NO@15 on the same ticker for guaranteed
  fee-loss. The `min_edge_cents=1` is a fee-affordability check,
  not an EV check.
- **Re-enable condition**: signal-quality study showing displayed-
  imbalance has positive EV after fees on a defined market subset,
  AND a pre-trade gate that prevents same-ticker both-side entry.

### `variance-fade` — **DISABLED 2026-05-08**

- `PREDIGY_VARIANCE_FADE_CONFIG` commented out in `~/.zprofile`.
- Mechanism audit verdict: fades 8¢ moves without news suppression
  → fades legitimate info trends. No probabilistic edge model.
- **Re-enable condition**: news-suppression layer that gates fade
  entries when a relevant news event has fired in the last N min.
  Probably depends on news-trader's classifier shipping first.

### `news-trader` — **DISABLED 2026-05-08** (dormant)

- `PREDIGY_NEWS_TRADER_ITEMS_FILE` commented out in `~/.zprofile`.
- Strategy code is fine — it's a clean adapter for an external
  classifier that writes JSONL items. The classifier itself is
  not deployed.
- **Re-enable condition**: classifier service running and
  appending to the JSONL file. Until then the strategy sat idle
  even when registered.

## Open work / next session priorities

The 2026-05-08 mechanism audit (see `docs/AUDIT_2026-05-08.md`)
is the current strategic frame. Live fleet has been narrowed
from 10 → 6 strategies; force-flatten of 32/52 positions freed capital;
legacy launchd traders + `predigy-import` are disabled; engine is
**disarmed and trading** as of the 2026-05-08 ops cleanup. Account value
is roughly mid-$70s ($100 deposited net minus shake-down losses).

1. **Watch the surviving 6 for ≥30 closed trades each before
   scaling caps.** The audit's mechanism verdict is theoretical —
   live realized P&L on unforced exits is what proves it.
   Highest-conviction strategies: `wx-stat` (NBM is genuinely
   skilled), `internal-arb` and `implication-arb` (real arb math).
   On probation: `stat`, `settlement`, `cross-arb`.

2. **Residual short-dated weather positions auto-settle in 24-48h**
   from the 2026-05-07 force-flatten. The ops cleanup aligned DB
   aggregate exposure to Kalshi venue exposure, but realized P&L from
   the force-flatten/settlement mess is still not clean strategy evidence.
   Don't conflate it with post-cleanup unforced strategy P&L.

3. **stat econ rules disabled pending recalibration.** All 9
   active stat rules were econ markets (BRAZILINF, ECONSTATU3,
   PAYROLLS, U3) — the audit said model_p calibration on these
   is unproven. `stat-curator --shadow-db` and `predigy-calibration`
   now provide the evidence path; re-enable a rule only after a saved
   calibration report shows enough settled out-of-sample samples and
   positive after-fee expectancy.

4. **wx-stat is the highest-leverage strategy to watch.** New
   defaults: `min_ask_cents=5` (skip lottery tickets) and
   `max_notional_per_fire_cents=500` ($5/ticker cap). Active
   exits remain settlement-only. If hit-rate × edge × size is
   positive over 30+ closed trades, raise the per-fire cap.

5. **cross-arb on probation.** `min_edge_cents` 1 → 3 fixes the
   fee-floor bug. 18 venue-reconcile phantom rows purged from
   the contract-cap accounting, and the legacy `cross-arb` launchd
   job is disabled so the curator cannot restart the old trader.
   Watch for actual fires from the consolidated engine only.

6. **Disabled-strategy re-enable conditions** (per
   `docs/AUDIT_2026-05-08.md`):
   - `book-imbalance`: signal-quality study + same-ticker
     both-side gate.
   - `variance-fade`: news-suppression layer (depends on
     news-trader classifier).
   - `news-trader`: classifier service deployed and writing JSONL.
   - `latency`: B4 FIX access + us-east-2 VPS.

7. **Phase 4b (FIX)** remains blocked on Kalshi institutional
   access. Email draft in `docs/KALSHI_FIX_REQUEST.md`.

8. **Phase 7 — retire legacy daemons** completely (delete
   `bin/{latency-trader,stat-trader,settlement-trader,cross-arb-trader}`,
   their plists, their JSON state files). Wait until ≥1 week of
   stable engine operation.

9. **Cap raises — math-proven arbs scaled 2026-05-09.** Updated rule:
   the original gate ("≥30 unforced closed trades after fees") still
   holds for **calibration-dependent strategies** (`wx-stat`, `stat`,
   `cross-arb`, `settlement`). It does **not** hold for
   `internal-arb` / `implication-arb` — those lock cash spreads and
   need throughput, not calibration evidence. As of 2026-05-09 01:05
   UTC the global caps were doubled (see top "Where money lives"
   section); `wx-stat` was simultaneously pinned at the prior
   shake-down sizing via `PREDIGY_WX_STAT_*` env clamps so it does
   not auto-scale. Lift the wx-stat clamp once Brier reports show
   positive after-fee EV over ≥30 unforced closed trades. The
   `stat` strategy remains gated (only 1 residual rule active).

10. **News-trader rebuild plan drafted (deferred).** See
    `plans/2026-05-08-news-trader-implementation.md`. Research
    showed the breaking-news speed race is unwinnable from a
    laptop without enterprise wires; plan pivots to scheduled
    macro releases (free BLS/Fed/Treasury RSS, Claude Haiku 4.5
    classifier with cached prompt), slow news, and long-tail
    markets where institutional desks aren't competing. Phase 1 is
    not yet implemented — arb scaling has priority until proven.

## Things to be careful about

- **Kalshi private key** has been pasted into conversation history
  many times. Rotate periodically.
- **`~/.zprofile`** is the single source of env truth. Engine reads
  it via `zsh -lc`. Don't put secrets that you don't want in
  process env there.
- **Postgres `predigy_test`** is wiped on every integration test
  run — don't store anything important there.
- **Dropping the kill-switch flag file** (`rm`) doesn't disarm; the
  engine treats absent and empty as both = disarmed, but the
  dashboard's POST /api/kill writes via tmp+rename so it can race
  with `rm`. Truncate (`: > flag`) is the safe disarm.

## Cross-platform context

The other platform is `~/code/tradegy` (Python, equity-index
options + MES futures options). Different return mechanism (variance
risk premium); deliberately uncorrelated with predigy.
`~/code/MOONSHOT_PLAN.md` is the joint strategic doc.
