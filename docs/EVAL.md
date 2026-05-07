# Strategy Evaluation Framework — operator guide

Last revised: 2026-05-07.

> Internal design reference: [`EVAL_SPEC.md`](./EVAL_SPEC.md).

## What it is

A framework for answering three questions about every strategy
running in the engine:

1. **What is it doing?** — trades, fills, intent activity, hold times,
   exit-reason mix.
2. **Is it profitable?** — net PnL, expectancy per trade, Sharpe-like
   risk-adjusted metric, fee burden.
3. **If not, why and what would fix it?** — rule-based diagnoses
   (D1–D9) paired with concrete recommendations
   (RaiseMinEdge, TightenStopLoss, …) computed from the strategy's
   own data.

## Three ways to use it

### CLI: `predigy-eval`

```sh
# 24h summary table.
./target/release/predigy-eval summary
./target/release/predigy-eval summary --since 7d
./target/release/predigy-eval summary --since all

# Per-strategy detail.
./target/release/predigy-eval ledger stat --since 7d --limit 100
./target/release/predigy-eval diagnose stat
./target/release/predigy-eval compare stat wx-stat

# Full markdown report (used by the daily launchd job).
./target/release/predigy-eval report --since 24h --out /tmp/r.md

# Live-refreshing tmux pane.
./target/release/predigy-eval watch --interval 60s
```

Exit codes:
- `0` → ran successfully, no critical diagnoses.
- `1` → ran, ≥1 critical diagnosis. CI / launchd's failure-routing
  picks this up.
- `2` → error.

### Dashboard

Browse to `http://<dashboard-host>:8080/eval` for the live UI:

- Sortable strategy table with health-color rows
  (red = critical, yellow = warn, green = ok).
- Click any strategy → drill-down: diagnoses with proposed actions
  + ledger of the most recent 50 trades.
- Window selector (1h / 24h / 7d / 30d / all) at the top.
- Auto-refreshes every 60s.

JSON endpoints (programmatic access):

```sh
curl http://localhost:8080/eval/summary.json?since=24h
curl http://localhost:8080/eval/ledger/stat.json?since=7d
curl http://localhost:8080/eval/diagnose/stat.json?since=24h
```

### Daily launchd job

`com.predigy.eval-daily` runs at 23:55 local nightly. Output:
- `~/Library/Logs/predigy/eval/YYYY-MM-DD.md` — that day's report
- `~/Library/Logs/predigy/eval/latest.md` — symlink to the most
  recent report

To enable:

```sh
cp deploy/macos/com.predigy.eval-daily.plist ~/Library/LaunchAgents/
launchctl load -w ~/Library/LaunchAgents/com.predigy.eval-daily.plist
```

## Diagnostic rules

| Code | Trigger | Severity |
|---|---|---|
| D1 UnprofitableOverSample | net PnL < 0 over ≥ 30 closed trades | Critical |
| D2 NegativeExpectancy | win × avg_win < (1-win) × \|avg_loss\| | Warn |
| D3 FeesEatingEdge | fees > 50% of gross PnL | Warn |
| D4 StopLossDominant | stop-loss exits > 50% of all closes | Warn |
| D5 CapBinding | cap-related rejections > 10% of intents | Warn |
| D6 ExitsNotFiring | p95 hold time > 2× intended max-hold | Warn |
| D7 SlippageHigh | avg slippage > 1.5¢ | Warn |
| D8 FireRateZero | < 5 intents over a ≥6h window | Info |
| D9 SmallSample | < 30 closed trades in window | Info |

Each diagnosis produces one or more recommendations with **concrete
numeric proposals** (e.g. "raise `min_edge_cents` from 5 → 9") and a
**confidence label** (low / medium / high).

The rule set is in [`crates/eval/src/diagnose.rs`](../crates/eval/src/diagnose.rs).
Adding a new rule = add a function `fn(&StrategyMetrics, &[Trade])
-> Option<Diagnosis>` and push it onto the `RULES` slice.

## What's measured

### Per-trade

Each closed position is one trade with:
- `realized_pnl_cents` (gross), `fees_paid_cents`
- Hold seconds = closed_at − opened_at
- Exit reason parsed from the closing intent's `reason`
- Intended edge parsed from the entry intent's `reason` (e.g.
  `"stat fire: ...edge=53.4c..."` → 53.4)
- Multi-leg `leg_group_id` (Audit I7) for atomic-multi-leg rollup

### Per-strategy aggregates

Computed over the time window:
- **PnL**: gross, fees, net = gross − fees
- **Distribution**: n winners / losers / break-even, win rate, avg
  win, avg loss, max win, max loss, σ
- **Per-trade**: expectancy = mean(net PnL/trade), Sharpe-like =
  expectancy / σ
- **Edge**: avg intended edge, avg realized edge, avg slippage
- **Hold time**: median, p95
- **Exit-reason mix**: count by tag (TP, SL, TS, BD, conv, inv,
  latency-flat, settled, …)
- **Activity**: intents submitted / filled / rejected / cancelled,
  fill rate, reject rate, cap-reject rate

## Recommendations & how to apply them

When `diagnose` proposes a numeric change, the value is **integer
cents** for direct copy-paste into `~/.zprofile`. Example output:

```
[CRITICAL] D1UnprofitableOverSample
    Net PnL -200¢ over 50 closed trades; strategy is bleeding capital.
    -> Disable strategy: Unprofitable over sufficient sample (high)
       Halt-and-investigate is safer than continuing to lose capital.
    -> Raise min_edge_cents -1 -> 12 (medium)
       Computed from current win rate (32.0%), avg win (5.0c), avg
       loss (-10.0c) — proposed min_edge would push expectancy positive
       under the current win/loss distribution.
```

Apply by editing `~/.zprofile` (e.g.
`export PREDIGY_STAT_MIN_EDGE_CENTS=12`), then kickstarting the
engine:

```sh
launchctl kickstart -k "gui/$(id -u)/com.predigy.engine"
```

For `DisableStrategy` recommendations: arm the per-strategy kill
switch via the dashboard, or unset the strategy's config env var
(e.g. `unset PREDIGY_INTERNAL_ARB_CONFIG`) and restart the engine —
that strategy's registration is gated on the env var.

## What v1 doesn't do (yet)

- **Backtest-replay parameter optimization** (`predigy-eval optimize`
  is a v2 stub). The architecture in `crates/eval/src/optimize.rs`
  defines the `ParameterSpace` + `OptimizationObjective` traits so
  the v2 backtester can plug in without further restructuring. v1
  ships rule-based recommendations only.
- **Cross-strategy correlation analysis.** Strategies are evaluated
  independently. Cross-strategy entanglement (e.g. stat + book-
  imbalance fading the same touch from opposite directions) is v2.
- **Real-time tick-level PnL.** PnL refreshes on each closed trade;
  intra-position drift is the dashboard's existing unrealized-PnL
  panel.
