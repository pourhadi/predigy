# Strategy Evaluation Framework — Spec

Last revised: 2026-05-07.

## Goal

Provide operators with continuous, actionable visibility into per-
strategy profitability and behavior, surface failure modes mechanically
from production data, and propose specific configuration changes when
a strategy is unprofitable.

The framework is **always-on** (continuous metric collection), **drill-
down** (per-strategy / per-trade), and **prescriptive** (concrete
recommendations, not just diagnostics).

## Why this exists

The engine now hosts ten strategies with disparate signal sources,
sizing models, and exit logic. Without dedicated tooling, the operator
must hand-write SQL to answer "is wx-stat actually making money?" and
even harder, "if it isn't, what should I change?". The eval framework
codifies the answers so the operator gets prescriptive guidance from
the same data the engine already persists.

## Non-goals (v1)

- **Real-time tick-level PnL streaming.** PnL refreshes on each closed
  trade; intra-position drift is the dashboard's existing
  unrealized-PnL panel.
- **Cross-strategy correlation analysis.** Treats strategies as
  independent for now. Cross-strategy entanglement (e.g. stat fading a
  market that book-imbalance just opened on) is v2.
- **Full Bayesian parameter optimization.** v1 ships a rule-based
  recommendation engine; v2 will add a backtest-replay-driven grid
  search over the parameter space.
- **Counterfactual analysis.** "What would PnL have been with
  parameter X' instead of X?" requires historical book replay; that's
  v2's backtester.

## Architecture

```
   DB tables                              eval crate
   ─────────                              ──────────
   intents       ┐                        ┌─→ Trade
   fills         ├─→ Trade Ledger ────────┤   ledger.rs
   positions     ┘                        │
   intent_events                          ├─→ StrategyMetrics
                                          │   metrics.rs
                                          │
                                          ├─→ Diagnosis
                                          │   diagnose.rs
                                          │
                                          └─→ Recommendation
                                              recommend.rs

   Consumers
   ─────────
   bin/predigy-eval (CLI)            — operator-triggered
   bin/dashboard /eval/* JSON        — live UI
   launchd: com.predigy.eval-daily   — daily scheduled report
```

## Data model

### Trade

A `Trade` is a single position lifecycle: one row in `positions`
plus the intents and fills associated with it via `(strategy, ticker,
side, opened_at..closed_at)` window.

```rust
pub struct Trade {
    pub strategy: String,
    pub ticker: String,
    pub side: String,                          // "yes" | "no"
    pub qty_open: i32,                         // signed; positive = long
    pub qty_remaining: i32,                    // 0 if fully closed
    pub avg_entry_cents: i32,
    pub avg_exit_cents: Option<i32>,           // None if still open
    pub realized_pnl_cents: i64,
    pub fees_paid_cents: i64,
    pub opened_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    pub hold_seconds: Option<i64>,             // closed_at - opened_at
    pub exit_reason: Option<ExitReason>,       // parsed from close intent reason
    pub leg_group_id: Option<Uuid>,            // I7 multi-leg rollup
    pub n_fills: i32,                          // diagnostic
}

pub enum ExitReason {
    TakeProfit,        // ":tp:"
    StopLoss,          // ":sl:"
    TrailingStop,      // ":ts:"
    BeliefDrift,       // ":bd:"  (stat A1)
    Convergence,       // ":conv:"  (cross-arb A2)
    ThesisInversion,   // ":inv:"  (cross-arb A2)
    LatencyFlat,       // "latency-flat:"
    SettlementFade,    // "settlement-fade:"
    Manual,            // operator close
    Settled,           // venue auto-settle at expiry
    Unknown,           // couldn't parse
}
```

Closing-intent `reason` strings are already structured (added during
the audit round). The parser maps the leading tag to `ExitReason`.

### StrategyMetrics

Aggregated per strategy over a configurable time window:

```rust
pub struct StrategyMetrics {
    pub strategy: String,
    pub window_start: DateTime<Utc>,
    pub window_end: DateTime<Utc>,

    // Core PnL
    pub n_trades_closed: u64,
    pub n_trades_open: u64,
    pub gross_pnl_cents: i64,                  // sum(realized_pnl)
    pub fees_paid_cents: i64,
    pub net_pnl_cents: i64,                    // gross - fees

    // Distribution
    pub n_winners: u64,
    pub n_losers: u64,
    pub n_breakeven: u64,
    pub win_rate: f64,                         // winners / (winners + losers)
    pub avg_win_cents: f64,
    pub avg_loss_cents: f64,                   // negative
    pub max_win_cents: i64,
    pub max_loss_cents: i64,                   // negative
    pub expectancy_cents: f64,                 // mean(pnl_per_trade)
    pub stddev_pnl_cents: f64,
    pub sharpe_ratio: f64,                     // expectancy / stddev

    // Edge & slippage
    pub avg_intended_edge_cents: f64,          // parsed from entry-intent reason
    pub avg_realized_edge_cents: f64,          // realized / qty
    pub avg_slippage_cents: f64,               // intended - realized

    // Hold time
    pub median_hold_secs: i64,
    pub p95_hold_secs: i64,

    // Exit reason mix (counts; sums to n_trades_closed)
    pub exits_by_reason: HashMap<ExitReason, u64>,

    // Activity (from intents table)
    pub n_intents_submitted: u64,
    pub n_intents_filled: u64,
    pub n_intents_rejected: u64,
    pub n_intents_cancelled: u64,
    pub fill_rate: f64,                        // filled / submitted
    pub reject_rate: f64,
    pub cap_reject_rate: f64,                  // cap-related / total
}
```

### Diagnosis

```rust
pub struct Diagnosis {
    pub code: DiagnosisCode,                   // D1..D8
    pub severity: Severity,                    // Info / Warn / Critical
    pub strategy: String,
    pub message: String,                       // human-readable
    pub evidence: serde_json::Value,           // e.g. {"win_rate": 0.32, ...}
    pub recommendations: Vec<Recommendation>,
}

pub enum DiagnosisCode {
    D1_UnprofitableOverSample,    // net PnL < 0 over ≥30 trades
    D2_NegativeExpectancy,        // win × avg_win < loss × avg_loss
    D3_FeesEatingEdge,            // fees > 50% of gross PnL
    D4_StopLossDominant,          // SL > 50% of exits
    D5_CapBinding,                // cap-rejected fires > 10%
    D6_ExitsNotFiring,            // hold time p95 >> intended
    D7_SlippageHigh,              // realized edge << intended
    D8_FireRateZero,              // ~0 fires over the window
    D9_SmallSample,               // < 30 closed trades; metrics unreliable
}

pub enum Severity { Info, Warn, Critical }
```

### Recommendation

```rust
pub struct Recommendation {
    pub action: ActionKind,
    pub strategy: String,
    pub current_value: serde_json::Value,
    pub proposed_value: serde_json::Value,
    pub rationale: String,
    pub confidence: Confidence,
}

pub enum ActionKind {
    RaiseMinEdge { current: i32, proposed: i32 },
    LowerMinEdge { current: i32, proposed: i32 },
    TightenStopLoss { current: i32, proposed: i32 },
    WidenStopLoss { current: i32, proposed: i32 },
    AddTrailingStop { trigger: i32, distance: i32 },
    LowerThreshold { which: String, current: f64, proposed: f64 },
    RaiseThreshold { which: String, current: f64, proposed: f64 },
    RaiseRiskCap { which: String, current: i64, proposed: i64 },
    DisableStrategy { reason: String },
    EnableStrategy { reason: String },
    Investigate { what: String },              // human review needed
}

pub enum Confidence { Low, Medium, High }
```

## Library API (`crates/eval/`)

Top-level entry points:

```rust
// Connect-and-go: load all closed trades for the window.
pub async fn load_trades(
    db: &Db,
    window: TimeWindow,
    strategy_filter: Option<&str>,
) -> Result<Vec<Trade>>;

// Aggregate per-strategy metrics from a Trade slice.
pub fn compute_metrics(trades: &[Trade]) -> HashMap<String, StrategyMetrics>;

// Run all diagnostic rules against the metrics.
pub fn diagnose(metrics: &StrategyMetrics, trades: &[Trade])
    -> Vec<Diagnosis>;

// Render a full markdown report.
pub fn render_markdown_report(
    metrics: &HashMap<String, StrategyMetrics>,
    diagnoses: &HashMap<String, Vec<Diagnosis>>,
) -> String;
```

Each diagnostic rule is a free function with signature
`fn(&StrategyMetrics, &[Trade]) -> Option<Diagnosis>` registered in a
`Vec`. Adding a new rule = add a function + push it on. Same shape for
recommendations (`fn(&Diagnosis) -> Vec<Recommendation>`).

## CLI (`bin/predigy-eval/`)

```
predigy-eval summary [--since 24h|7d|30d|all]
predigy-eval ledger <strategy> [--since ...] [--limit N]
predigy-eval diagnose <strategy> [--since ...]
predigy-eval report [--out FILE] [--format md|json] [--since ...]
predigy-eval compare <strategy_a> <strategy_b> [--since ...]
predigy-eval watch [--interval 60s]            # live-refresh summary
```

`--since` parses `1h`, `24h`, `7d`, `30d`, `all`, or RFC3339.
`summary` renders a CLI table; `report` is the canonical full-output.
`watch` is `summary` in a loop with terminal clear, useful as a tmux
pane during deployment.

Exit codes:
- 0 = ran successfully, no critical diagnoses
- 1 = ran successfully, ≥1 critical diagnosis (CI/operator alerting
  hook)
- 2 = error

## Dashboard integration

Three new axum routes in `bin/dashboard/src/main.rs`:

- `GET /eval/summary.json` — full metrics map (parameterized by
  `?since=24h`)
- `GET /eval/ledger/:strategy.json` — trade ledger for one strategy
- `GET /eval/diagnose/:strategy.json` — diagnoses + recommendations

Static UI: a new `eval.html` page with a sortable table, drill-down on
strategy click (loads ledger + diagnose), and a per-strategy color
indicator (green=profitable + no critical, yellow=warn, red=critical
or unprofitable).

The existing dashboard's "engine positions" panel stays — `eval` is
its closed-trade complement.

## Scheduled report

`deploy/launchd/com.predigy.eval-daily.plist`: runs the
`predigy-eval report` command nightly at 23:55 local. Emits markdown
to `~/Library/Logs/predigy/eval/YYYY-MM-DD.md`. On any critical
diagnosis, the launchd job exits non-zero, which the operator's
existing log-watcher can route to push notifications.

## Optimizer (v2 hook)

A `crates/eval/optimize.rs` module is included as scaffolding only:

- Defines `ParameterSpace` per strategy (env-var ranges, with
  step sizes).
- Defines an `OptimizationObjective` trait
  (`fn evaluate(metrics) -> f64`) — default is `net_pnl_cents`,
  pluggable to Sharpe, expectancy, etc.
- `predigy-eval optimize <strategy>` exists but currently emits
  `"v2 — backtest-replay optimizer not yet implemented; falling
  back to rule-based recommendations from `diagnose`"`.

This keeps the architecture extensible without committing to a
backtester this round. The v2 implementation will replay historical
fills + book deltas and score parameter candidates against the same
metrics this framework already computes.

## Diagnostic rules (v1 set)

| Code | Trigger | Severity | Recommendation |
|---|---|---|---|
| D1 | net_pnl < 0 AND n_closed ≥ 30 | Critical | DisableStrategy, then Investigate |
| D2 | win_rate × avg_win < (1-win_rate) × |avg_loss| | Warn | RaiseMinEdge to a level where expectancy clears 0 |
| D3 | fees > 50% of gross_pnl | Warn | RaiseMinEdge to clear avg_fee + cushion |
| D4 | exits_by_reason[SL] > 0.5 × n_closed | Warn | LowerThreshold (entry signal too eager) OR WidenStopLoss |
| D5 | cap_reject_rate > 0.1 | Warn | RaiseRiskCap OR investigate which cap is binding |
| D6 | p95_hold_secs > 2× intended_max_hold | Warn | TightenStop or LowerTakeProfit |
| D7 | avg_slippage > 1.5¢ | Warn | RaiseMinEdge by avg_slippage to absorb |
| D8 | n_intents_submitted < 5 over 24h | Info | LowerThreshold (signal too tight) |
| D9 | n_closed < 30 | Info | "Insufficient sample; metrics unreliable, keep running" |

Confidence levels: Critical diagnoses with ≥30 trades = High
confidence; <30 trades = Medium; <10 trades = Low.

Each strategy has a `intended_max_hold_secs` and `intended_edge_cents`
declared in code (looked up from the strategy's config defaults).

## Recommendation specifics

When D2 fires (negative expectancy), compute the proposed `min_edge`
that would have made expectancy positive:

```
required_edge = (loss_rate × |avg_loss|) / win_rate - avg_win + buffer
proposed_min_edge = current_min_edge + required_edge
```

Similar arithmetic for D3 (fee cushion) and D7 (slippage cushion). All
proposed values are integers (cents) for direct copy-paste into
`.zprofile`.

## Testing

- **Unit**: each diagnostic rule has a positive case (rule fires) +
  a negative case (rule doesn't fire) + edge cases (empty, n=1).
- **Integration**: against the test DB, seed N synthetic trades with
  controlled outcomes, verify metrics + diagnoses + recommendations.
- **Live**: at the end of v1 implementation, run `predigy-eval report`
  against the production DB and verify the output is sane against
  observed behavior.

## Documentation

- `docs/EVAL.md` — operator-facing usage docs (this spec is the
  internal reference)
- Inline rustdoc on all public types
- README updates with the new `predigy-eval` binary

## Implementation order

1. ✅ This spec
2. Crate scaffold + types (`crates/eval/`)
3. Trade ledger derivation (DB → `Trade` rows)
4. Metrics computation
5. Diagnostic + recommendation engines (rule set above)
6. Markdown report renderer
7. CLI (`bin/predigy-eval/`)
8. Tests (unit + integration)
9. Dashboard JSON routes + static panel
10. Scheduled launchd job
11. Operator docs (`docs/EVAL.md`)
12. Live verification against prod DB
