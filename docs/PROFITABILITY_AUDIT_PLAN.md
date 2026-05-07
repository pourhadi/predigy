# Profitability Audit Plan

> Created 2026-05-07 after a full architecture, implementation,
> operations, and strategy-theory audit. This is the working plan for
> making Predigy safe enough to evaluate and eventually scale.

## Executive Position

Predigy should remain live only at small shake-down caps until core
execution-safety issues are fixed and each strategy proves positive net
expectancy after fees.

Current assessment:

- The architecture direction is right: consolidated engine, shared OMS,
  Postgres audit trail, modular strategy crates, and a single hot-path
  market-data router.
- The implementation is not yet production-complete for scaling.
- Profitability is unproven; current live data has too few closed trades
  to infer edge.
- Several strategies have weak alpha theses and should stay disabled,
  shadowed, or tightly gated until measured.
- The best candidates for continued investment are `wx-stat`,
  `implication-arb`, `internal-arb`, and carefully measured `settlement`.

## Current Profitability Verdict

Predigy is capable of placing live trades, but it has not yet proven it
can trade profitably. At current scale, fees and execution quality matter
more than strategy count.

Live evidence from the audit window:

- Only two closed positions existed in the evaluation sample, both
  `stat`, net `-2c` after fees.
- Most risk is still open exposure, not realized PnL.
- Fee drag is already meaningful: small one-contract taker trades suffer
  whole-cent fee rounding.
- The engine logs showed take-profit / stop-loss intents being rejected
  by notional and contract caps, meaning safety exits can be blocked.

Do not raise caps until the Priority 0 and Priority 1 items below are
complete and live-observed cleanly.

2026-05-07 weather safety addendum: same-day temperature markets must be
gated against observed ASOS daily extremes before any forecast/NBM model can
emit a rule. This was added after `stat`/`wx-stat` bought YES on
`KXHIGHTSFO-26MAY07-T62` even though SFO had already observed 64°F, making
the below-62 YES side impossible.

## Priority 0: Immediate Safety Fixes

These were scale blockers. Items 1-5 were implemented and redeployed live
on 2026-05-07 in the consolidated engine.

### 1. Fix Reduce / Exit Risk Handling

Problem: `DbBackedOms::check_caps()` treats every intent as additive
exposure. Sell-to-close and buy-to-close exits can be rejected as if they
increase risk. This has already appeared live as stat TP/SL exits rejected
by notional or contract caps.

Files:

- `bin/predigy-engine/src/oms_db.rs`
- `bin/predigy-engine/tests/oms_integration.rs`

Required behavior:

- Closing orders reduce projected contracts and notional.
- Exit orders that reduce risk are not blocked by notional caps.
- Naked sells are either explicitly modeled as short exposure or rejected.
- Venue requests should use `reduce_only` when the order is intended to
  close exposure and Kalshi supports it for the order shape.

Required tests:

- Long at cap can sell-to-close.
- Short at cap can buy-to-close.
- Exit intent reduces projected strategy and global notional.
- New entry still respects all caps.
- Naked sell behavior is explicit and tested.

### 2. Add Supervisor Tick Scheduling

Problem: strategies declare `tick_interval()` and `Event::Tick` exists,
but supervisors only block on inbound event channels. Tick-driven exits,
force-flats, and quiet-market refreshes can be inert unless another event
arrives.

Files:

- `bin/predigy-engine/src/supervisor.rs`
- `crates/engine-core/src/events.rs`

Required behavior:

- Each supervisor emits `Event::Tick` at its strategy's configured cadence.
- Tick events pass through boot grace.
- Tick scheduling shuts down cleanly with the supervisor lifecycle.

Required tests:

- Mock strategy receives ticks at configured cadence without book events.
- Latency force-flat can fire in a quiet market.
- Position refresh logic runs on tick even when books are stale/quiet.

### 3. Implement Real Venue Reconciliation

Problem: reconciliation is documented but effectively stubbed. The WS
fill stream can miss events during reconnects, and venue orders can drift
from local DB state.

Files:

- `bin/predigy-engine/src/oms_db.rs`
- `bin/predigy-engine/src/main.rs`
- `crates/kalshi-rest/src/client.rs`

Required behavior:

- Periodically reconcile DB intents against venue order status.
- Pull recent fills since the last seen fill and apply missed fills.
- Compare DB positions against venue positions and alert on mismatch.
- Detect unmanaged venue orders.
- Record reconciliation findings as structured logs and DB events.

Required tests:

- Missed fill is applied from REST catch-up.
- Venue order absent/expired updates local intent.
- Duplicate fill remains idempotent.
- Manual venue position mismatch is surfaced.

### 4. Deduplicate Fills Before Mutating Intent State

Problem: `apply_execution()` updates `intents.cumulative_qty` and inserts
an intent event before checking whether `venue_fill_id` was already seen.
A replayed WS fill can corrupt lifecycle state even if the position
cascade is skipped.

File:

- `bin/predigy-engine/src/oms_db.rs`

Required behavior:

- Duplicate fill is a full no-op for intent, event, fill, and position
  state.
- Fill dedupe happens before any state mutation that depends on that fill.

Required tests:

- Replaying the same `venue_fill_id` twice leaves cumulative qty
  unchanged.
- No duplicate lifecycle event is inserted.
- Position quantity and fees are unchanged on duplicate replay.

### 5. Fix Stale / Out-of-Order Market-Data Handling

Problem: recent sid-level sequence handling correctly moved away from
false per-market gaps, but `seq <= last` currently looks like a normal
non-gap path. Duplicate or stale deltas can mutate books after newer
state.

File:

- `bin/predigy-engine/src/market_data.rs`

Required behavior:

- Duplicate or stale deltas are dropped.
- Duplicate or stale snapshots do not replace newer books.
- Forward gaps block affected sid markets until snapshot recovery.
- Snapshot recovery is verified against live Kalshi `get_snapshot` wire
  shape.

Required tests:

- Stale delta does not mutate book.
- Stale snapshot does not replace book.
- True gap requests snapshot and blocks deltas until recovered.
- Unknown-market gap behavior fails closed, not open.

## Priority 1: Risk-Control Hardening

Fix these before any meaningful scale-up. Items 1-5 were implemented and
redeployed live on 2026-05-07. Keep caps small while the new checks are
live-observed.

### 1. Mark-to-Market Daily Loss

Problem: daily loss is enforced using realized PnL only. Open adverse
moves do not stop new entries.

Required behavior:

- Include unrealized PnL from live marks or recent persisted book
  snapshots.
- If no mark is available, either fail closed for new entries or use a
  conservative adverse mark.
- Surface mark availability in dashboard/eval.

### 2. Persist Book Snapshots

Problem: the `book_snapshots` table exists but the router does not write
it. That limits DB-driven risk, dashboard marks, and replayability.

Required behavior:

- Router upserts best yes bid/ask, quantities, last trade when available,
  and update timestamp on WS snapshot/delta.
- Dashboard and risk checks can use recent DB marks.

### 3. Enforce OMS Order-Rate Limits

Problem: `max_orders_per_window` and `rate_window_ms` are configured but
not enforced in consolidated OMS.

Required behavior:

- Check rate window before accepting new intents.
- Strategy runaway loops reject locally before reaching venue.
- Rate-limit rejections appear in eval diagnostics.

### 4. Make Cap Checks Concurrency-Safe

Problem: exposure is read before insert without a transaction-level lock.
Concurrent supervisors can both see under-cap state and jointly exceed
global caps.

Required behavior:

- Use a DB transaction plus advisory lock or equivalent around exposure
  check and insert.
- Include global cap and per-strategy cap in the same critical section.

### 5. Extend Per-Strategy Kill Switches

Problem: per-strategy DB kill-switch sync covers only four legacy IDs.
Newer registered strategies are not covered.

Required behavior:

- Derive known strategy IDs from the registry, not a hardcoded subset.
- Every registered strategy respects DB kill rows.
- Unknown strategy kill rows are logged but do not break sync.

### 6. Clarify Kill-Switch Semantics

Current behavior: halt-only. It blocks new entries and does not flatten
positions.

Decision:

- Keep global kill switch halt-only for now.
- Add a separately named `panic_flatten` operator command later if
  automatic liquidation is wanted.
- Do not document kill switch as flattening until flattening exists.

### 7. Same-Day Weather Observed Gate

Problem: wx-stat/stat weather rules could keep trading same-day threshold
markets from stale forecast/NBM probabilities after the day's observed high
or low had already decided the contract.

Required behavior:

- For daily-high `greater` markets, observed high > threshold forces YES.
- For daily-high `less` markets, observed high >= threshold forces NO.
- For daily-low `less` markets, observed low < threshold forces YES.
- For daily-low `greater` markets, observed low <= threshold forces NO.
- Same-day/past markets without required ASOS observations do not fall back
  to forecast/NBM scoring.
- Observed-deterministic rules are excluded from NBM calibration samples.
- NBM probability aggregation must match contract semantics: daily-high
  `greater` and daily-low `less` are any-hour events, while daily-high
  `less` and daily-low `greater` are all-hours events and use the
  constraining hour, not the easiest hour.

## Priority 2: Strategy Gating

Reduce fee leakage and noisy exposure while the core engine is hardened.

Recommended live posture:

| Strategy | Posture | Reason |
|---|---|---|
| `wx-stat` | Keep, tight caps | Best non-market-derived information thesis if NBM calibration holds |
| `implication-arb` | Keep, tiny size | Real no-arb math, execution and pair-proof risk |
| `internal-arb` | Keep, tiny size | Real no-arb math, multi-leg partial-fill risk |
| `settlement` | Keep measured | Plausible behavioral niche, adverse-selection heavy |
| `cross-arb` | Measure / raise edge | Plausible signal but not hedged true arb |
| `stat` | Measure / raise edge | LLM probability edge unproven after fees |
| `book-imbalance` | Disable or shadow | Displayed depth is weak alpha and likely adverse selection |
| `variance-fade` | Disable or shadow | Fading moves without news filter is fragile |
| `latency` | Disable or shadow | Polling plus REST latency weakens alert edge |
| `news-trader` | Shadow until upstream proven | Strategy is an adapter; alpha depends on classifier/source |

## Priority 3: Profitability Validation

No strategy should scale on belief alone. Use closed-trade data.

Minimum gates before scaling a strategy:

- At least 30 closed trades before drawing weak conclusions.
- Prefer 100 closed trades before materially raising caps.
- Net PnL after fees is positive.
- Fee burden is not consuming the intended edge.
- Exit and reconciliation systems are known-good.
- Strategy-specific failure modes have been measured.

Metrics required per strategy:

- Net PnL after fees.
- Gross PnL.
- Fees as percentage of gross edge.
- Fill rate and reject rate.
- Cap-reject rate.
- Signal-to-fill slippage.
- Hold-time distribution.
- Exit reason distribution.
- Realized edge by intended-edge bucket.

Strategy-specific validation:

| Strategy | Required validation |
|---|---|
| `wx-stat` | Calibration curve, Brier score, PnL by airport/month/lead-time |
| `stat` | `model_p` calibration, PnL by edge bucket and category |
| `settlement` | PnL by entry price band and time-to-close |
| `cross-arb` | Convergence attribution, Poly quote age/liquidity, spread bucket PnL |
| `implication-arb` | Leg fill completeness, settlement proof, actual group cost |
| `internal-arb` | Family proof, leg fill completeness, aggregate cost vs payoff |
| `book-imbalance` | Forward returns after imbalance, continuation vs reversal |
| `variance-fade` | Event study after rapid moves, news/no-news split |
| `latency` | Alert timestamp to fill timestamp, price before/after alert |
| `news-trader` | Source timestamp, classification timestamp, fill timestamp, precision/recall |

## Priority 4: Execution Improvements

Do these after safety is reliable.

### 1. FIX Access

FIX is required for serious latency/cross-arb execution. It remains
blocked on Kalshi institutional onboarding. Do not treat FIX as current
runtime behavior until access is live.

### 2. Maker Execution

Maker execution is the main way to reduce fee drag. It requires order
state tracking, cancel/replace, queue modeling, and higher capital. It is
not a quick patch.

### 3. Reduce One-Lot Fee Drag

One-contract taker trades are structurally disadvantaged by whole-cent fee
rounding. Larger lots reduce fee percentage only after strategy edge and
engine safety are proven.

### 4. Position-Aware Exits Everywhere

Any strategy that holds non-settlement exposure needs mark-aware exits or
a clear reason to hold to resolution.

## Documentation Updates Required

Keep these docs aligned with code after each relevant change:

- `docs/SESSIONS.md`: current production state, blockers, and next action.
- `docs/RUNBOOK.md`: operator checks for cap-blocked exits, reconciliation,
  kill-switch semantics, and dashboard exposure.
- `docs/AUDIT.md`: this safety/theory audit and strategy gating status.
- `docs/ARCHITECTURE.md`: current REST/FIX reality, book snapshot reality,
  reconciliation reality, and kill-switch semantics.

## Recommended Implementation Order

1. Fix exit/reduce cap handling.
2. Fix stale/out-of-order market-data handling.
3. Add supervisor tick scheduler.
4. Deduplicate fills before intent mutation.
5. Implement reconciliation.
6. Persist book snapshots and use mark-to-market risk.
7. Enforce OMS rate limits and concurrency-safe caps.
8. Expand per-strategy kill-switch coverage.
9. Gate or disable weak strategies.
10. Update docs after each behavioral change.
11. Run live for 24-72h at current caps.
12. Begin strategy-by-strategy profitability validation.
13. Consider cap increases only after safety and expectancy are proven.

## Scale-Up Gate

Caps may be raised only when all are true:

- No Priority 0 blockers remain.
- Reconciliation is active and clean.
- Exit orders are confirmed not blocked by risk caps.
- Dashboard and eval show no persistent state/telemetry errors.
- The strategy being scaled has positive net expectancy after fees or is a
  proven no-arb class with verified fill completeness.
- The operator accepts the measured drawdown profile.
