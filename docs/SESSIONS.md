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

## What is running RIGHT NOW (2026-05-07, post-audit hardening)

```
launchctl list | grep predigy
```

| Job | What it does | State |
|---|---|---|
| `com.predigy.engine` | Consolidated trader. Owns OMS, market data, exec, all four strategies. | running, mode=Live |
| `com.predigy.cross-arb-curate` | Anthropic-driven Kalshi×Polymarket pair curator. 10-min cron. | scheduled |
| `com.predigy.stat-curate` | model_p curator for stat strategy. | scheduled |
| `com.predigy.wx-curate` | NWS-state-aware weather rule curator (latency strategy). | scheduled |
| `com.predigy.wx-stat-curate` | NBM-quantile probabilistic weather rules. | scheduled |
| `com.predigy.import` | Legacy JSON-state mirror to Postgres. | scheduled |
| `com.predigy.dashboard` | HTTP/HTML dashboard at port 8080. | running |

Retired (post-cutover): `latency-trader`, `settlement`, `stat-trader`,
`cross-arb`. The plists are still on disk under
`deploy/macos/`; keeping them around for one cycle as a rollback
path before deletion.

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
  - `max_notional_cents` per strategy: $5 ($500¢)
  - `max_global_notional_cents`: $15 ($1500¢) — binds before
    4×$5=$20 of per-strategy caps could.
  - `max_daily_loss_cents`: $2
  - `max_contracts_per_side`: 3
  - `max_in_flight`: 10
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

- 68 active rules in `rules` table (Postgres). Curated by
  `stat-curate` + `wx-stat-curate`.
- Phase 6.1 active exits: take-profit 8¢ / stop-loss 5¢, defaults.
  Closing IOCs use idempotent
  `stat-exit:{ticker}:{side}:{tp|sl}:{minute_bucket}` cids.
- 2026-05-07 safety note: `stat` and `wx-stat` were halted after a
  same-day SFO high-temperature rule bought YES on a below-62 market after
  observed high had already reached 64°F. `wx-stat-curator` now gates
  same-day/past temperature markets through ASOS observed extremes over the
  airport's local Kalshi settlement day before forecast/NBM scoring; current
  local-day pulls bypass the observation cache so intraday extrema cannot go
  stale. A second wx-stat bug on PHX below-98 for May 8 found that NBM used
  max hourly probability for all strike directions; daily-high `less` and
  daily-low `greater` now use the constraining all-hours probability instead.

### `settlement` (sports tape-reading near close)

- Pure discovery-driven; no static ticker list. Engine's discovery
  service polls Kalshi `/markets?series_ticker=...` for the standard
  sports basket every 60s, auto-registers new markets with the
  router, pushes `Event::DiscoveryDelta` to the strategy.
- No active exits — Kalshi auto-settles binary outcomes at $1/$0.

### `latency` (NWS-alert lift on weather markets)

- Rules loaded from `PREDIGY_LATENCY_RULE_FILE` JSON at startup.
  Set in `~/.zprofile` if you want the strategy to fire — without
  the env var the engine logs a warning and the strategy is a no-op.
- Phase 6.2 force-flat: positions held >30 min get a wide-IOC exit
  at 1¢ (any standing bid takes us). Latency has no book
  subscription so mark-aware exits aren't possible.
- Requires `PREDIGY_NWS_USER_AGENT` in env or NWS feed won't spawn.
- 2026-05-07 audit note: DB kill switch is armed for `latency` after a stale
  NWS alert re-fired following restarts. Keep halted until event freshness and
  restart-persistent de-dupe are fixed.

### `cross-arb` (Kalshi vs Polymarket convergence)

- Pair-driven. Pairs come from `PREDIGY_CROSS_ARB_PAIR_FILE`
  (default: `~/.config/predigy/cross-arb-pairs.txt`), curated by
  `cross-arb-curate`. Pair-file service polls mtime; hot reload.
- Phase 6.2 active exits: take-profit 5¢ / stop-loss 4¢ (tighter
  than stat because cross-arb scalps smaller convergences).
- Cross-strategy bus: cross-arb publishes `PolyMidUpdate` for
  paired markets; stat subscribes (currently log-only — augmenting
  belief from poly-mid is a future enhancement).

## Open work / next session priorities

1. **Do not scale yet.** Priority 0 and Priority 1 safety hardening from
   `docs/PROFITABILITY_AUDIT_PLAN.md` is now live, but it needs clean
   observation before caps increase.
2. **Watch reconciliation drift.** The new REST reconciliation loop applies
   missed fills/order terminal states and logs position mismatches. Current
   live drift includes legacy/manual venue positions that are not fully
   represented in the consolidated OMS DB.
3. **Watch mark availability.** `book_snapshots` are now persisted from WS
   books. The OMS fails closed for risk-increasing entries when a strategy
   has open positions without recent marks; exits/reductions still pass.
4. **Gate strategy exposure while proving edge.** Favor `wx-stat`,
   `implication-arb`, `internal-arb`, and measured `settlement`.
   Shadow or tightly gate `book-imbalance`, `variance-fade`, `latency`,
   and `news-trader` until empirical edge exists.
5. **Book-imbalance has scalable inventory controls.** The 2026-05-07 live
   audit found it buying both YES and NO on `KXVOTEHUBTRUMPUPDOWN-26MAY07`, plus
   insufficient-balance rejects. NO-side accounting was fixed and pre-fix ledger
   rows repaired; corrected mark was about `-30c`, not the pre-repair `-700c`.
   The strategy now scales only same-side exposure under dynamic imbalance /
   liquidity caps, blocks while active orders exist, and treats opposite-side
   signals as flatten-first exits instead of opening the other side.
6. **Implication-arb has package-aware scale caps.** Corrected marks turned the
   existing `KXECONSTATU3` package from apparently positive to about `-130c`.
   The strategy now models parent-YES/child-NO packages, blocks unbalanced pair
   inventory, scales balanced packages by edge/liquidity/pair cap/cluster cap,
   and reserves queued groups in-memory so one book event cannot overrun caps.
7. **Weather stat/wx-stat are intentionally resumed, but keep watching.** The
   wx-stat rule file was regenerated through the observed-extreme gate and
   corrected NBM aggregation. `predigy-import` no longer imports
   `wx-stat-rules.json` as `stat`, and the latest DB refresh disabled all
   legacy enabled rows from that source; `KXHIGHTSFO-26MAY07-T62` is disabled.
   `KXHIGHTPHX-26MAY08-T98` was corrected from YES 0.98 to NO 0.066, and the
   bad 14-contract YES exposure was sold at 3c. The OMS no longer blanket-blocks
   all strategy entries because unrelated held positions have stale marks; stale
   marks are valued conservatively for daily-loss checks, while `stat` and
   `wx-stat` self-subscribe held-position tickers so marks can recover after
   rules roll off. Follow-up: leg-group submits now use the same conservative
   stale/missing-mark daily-loss valuation instead of refusing the whole group.
8. **Latency stale-replay guard is fixed.** NWS alerts now require fresh onset
   or effective timestamps, reject expired or missing-timestamp alerts, and use
   a stable full-alert SHA-256 hash in cids instead of a shared short prefix.
9. **Phase 4b (FIX)** remains blocked on Kalshi institutional access.
   Email draft in `docs/KALSHI_FIX_REQUEST.md`. Operator action, but
   do not prioritize FIX above the safety blockers.
10. **Phase 7 — retire legacy daemons** completely (delete
   `bin/{latency-trader,stat-trader,settlement-trader,cross-arb-trader}`,
   their plists, their JSON state files). Wait until ≥1 week of
   stable engine operation.

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
