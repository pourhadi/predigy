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

## What is running RIGHT NOW (2026-05-08, post-audit-tightening)

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
| `com.predigy.import` | Legacy JSON-state mirror to Postgres. | scheduled |
| `com.predigy.dashboard` | HTTP/HTML dashboard at port 8080. | running |

**Active engine strategies (6, post-2026-05-08 mechanism audit):**
`stat`, `wx-stat`, `settlement`, `cross-arb`, `internal-arb`, `implication-arb`.

**Disabled engine strategies (4):** `book-imbalance`, `variance-fade`,
`latency`, `news-trader`. Disabled by unsetting their config env
vars in `~/.zprofile` (engine skips registration when
`PREDIGY_*_CONFIG` / `PREDIGY_*_RULE_FILE` /
`PREDIGY_*_ITEMS_FILE` is unset). See `docs/AUDIT_2026-05-08.md`
for verdicts and re-enable conditions.

Retired (post-cutover): `latency-trader`, `settlement-trader`,
`stat-trader`, `cross-arb-trader`. Plists still on disk under
`deploy/macos/`; Phase 7 will delete them once ≥1 week stable.

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

- **Kalshi production account**: ~$50 funded. `KALSHI_KEY_ID` in
  `~/.zprofile`; PEM at `~/.config/predigy/kalshi.pem`.
- **Capital caps in the engine** (RiskCaps shake-down defaults):
  - `max_notional_cents` per strategy: $40 ($4000¢)
  - `max_global_notional_cents`: $120 ($12000¢)
  - `max_daily_loss_cents`: $10
  - `max_contracts_per_side`: 20
  - `max_in_flight`: 40
  - Override per-strategy via env vars in `~/.zprofile`
    (`PREDIGY_MAX_NOTIONAL_CENTS`, `PREDIGY_MAX_GLOBAL_NOTIONAL_CENTS`,
    etc.).

## Kill switch (panic button)

```sh
echo armed > ~/.config/predigy/kill-switch.flag   # ARM (refuse new entries)
: > ~/.config/predigy/kill-switch.flag            # DISARM (truncate)
```

Engine + dashboard both poll the file every 5 seconds. Engine logs
"kill-switch: ARMED" when it sees a non-empty flag.

## Strategy-by-strategy state

### `stat` (statistical model probability vs ask)

- 5 enabled rules in `rules` table (Postgres) as of the latest check.
  Curated by `stat-curate`; `wx-stat-curate` feeds the dedicated
  `wx-stat` strategy directly.
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
- Latest live restart loaded zero active `wx-stat` rules because the
  old metadata-less/misdated rule file is skipped. `com.predigy.wx-stat-curate`
  was running a full NBM cache refresh to regenerate metadata-bearing rules.

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
  cap with phantom 50¢ entries.
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
from 10 → 6 strategies; force-flatten of 32/52 positions has
freed capital; engine is **disarmed and trading** as of
2026-05-08 ~03:30 UTC. Account is ~$76.71 ($100 deposited net
−$23 from shake-down period).

1. **Watch the surviving 6 for ≥30 closed trades each before
   scaling caps.** The audit's mechanism verdict is theoretical —
   live realized P&L on unforced exits is what proves it.
   Highest-conviction strategies: `wx-stat` (NBM is genuinely
   skilled), `internal-arb` and `implication-arb` (real arb math).
   On probation: `stat`, `settlement`, `cross-arb`.

2. **20 short-dated weather positions auto-settle in 24-48h**
   from the 2026-05-07 force-flatten. Cost basis ~$20.21, current
   exposure ~$1.06 on the dashboard. Don't conflate their
   settlement P&L with the post-disarm strategy P&L.

3. **stat econ rules disabled pending recalibration.** All 9
   active stat rules were econ markets (BRAZILINF, ECONSTATU3,
   PAYROLLS, U3) — the audit said model_p calibration on these
   is unproven. Re-enable a rule only after at least one print
   cycle's worth of out-of-sample calibration data.

4. **wx-stat is the highest-leverage strategy to watch.** New
   defaults: `min_ask_cents=5` (skip lottery tickets) and
   `max_notional_per_fire_cents=500` ($5/ticker cap). Active
   exits remain settlement-only. If hit-rate × edge × size is
   positive over 30+ closed trades, raise the per-fire cap.

5. **cross-arb on probation.** `min_edge_cents` 1 → 3 fixes the
   fee-floor bug. 18 venue-reconcile phantom rows purged from
   the contract-cap accounting. Watch for actual fires post-
   purge.

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

9. **Cap raises are gated on edge proof.** Don't raise
   `PREDIGY_MAX_NOTIONAL_CENTS` / `PREDIGY_MAX_GLOBAL_NOTIONAL_CENTS`
   in `~/.zprofile` until the surviving strategies show ≥30
   *unforced* closed trades net positive after fees. Yesterday's
   force-flatten makes the realized P&L data unreliable; that
   resets the trade-count clock.

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
