//! Diagnostic engine — rule-based detection of strategy issues.
//!
//! Each rule is a free function. `diagnose()` runs every rule and
//! collects the resulting `Diagnosis` records. Adding a rule = add
//! a function and push it onto the registry.
//!
//! All rules consume `&StrategyMetrics + &[Trade]` and produce
//! `Option<Diagnosis>`. Returning `None` means the rule didn't
//! trigger.
//!
//! Each diagnosis carries one or more recommendations with concrete
//! proposed numeric values (where possible) — see `recommend.rs`.

use crate::metrics::StrategyMetrics;
use crate::recommend::{ActionKind, Confidence, Recommendation};
use crate::types::{ExitReason, Trade};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnosis {
    pub code: DiagnosisCode,
    pub severity: Severity,
    pub strategy: String,
    pub message: String,
    pub evidence: serde_json::Value,
    pub recommendations: Vec<Recommendation>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum DiagnosisCode {
    /// Net PnL < 0 over ≥ 30 trades.
    D1UnprofitableOverSample,
    /// Win rate × avg_win < (1 - win_rate) × |avg_loss|.
    D2NegativeExpectancy,
    /// Fees consume > 50% of gross PnL.
    D3FeesEatingEdge,
    /// Stop-loss > 50% of exits.
    D4StopLossDominant,
    /// Cap-rejected fires > 10% of total intents.
    D5CapBinding,
    /// p95 hold time exceeds an intended-max heuristic.
    D6ExitsNotFiring,
    /// Realized edge < intended edge by more than the slippage
    /// threshold.
    D7SlippageHigh,
    /// Fewer than 5 intents submitted in the window.
    D8FireRateZero,
    /// Sample too small for confident metrics.
    D9SmallSample,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum Severity {
    Info,
    Warn,
    Critical,
}

/// Run every rule against the metrics + trade slice. Returns
/// diagnoses ordered by severity (critical first).
#[must_use]
pub fn diagnose(metrics: &StrategyMetrics, trades: &[Trade]) -> Vec<Diagnosis> {
    let mut out = Vec::new();
    for rule in RULES {
        if let Some(d) = rule(metrics, trades) {
            out.push(d);
        }
    }
    // Critical first, then Warn, then Info — descending severity.
    out.sort_by_key(|d| std::cmp::Reverse(d.severity));
    out
}

type Rule = fn(&StrategyMetrics, &[Trade]) -> Option<Diagnosis>;

const RULES: &[Rule] = &[
    rule_d1_unprofitable_over_sample,
    rule_d2_negative_expectancy,
    rule_d3_fees_eating_edge,
    rule_d4_stop_loss_dominant,
    rule_d5_cap_binding,
    rule_d6_exits_not_firing,
    rule_d7_slippage_high,
    rule_d8_fire_rate_zero,
    rule_d9_small_sample,
];

const MIN_SAMPLE_FOR_CONFIDENCE: u64 = 30;

fn rule_d1_unprofitable_over_sample(m: &StrategyMetrics, _t: &[Trade]) -> Option<Diagnosis> {
    if m.n_trades_closed < MIN_SAMPLE_FOR_CONFIDENCE {
        return None;
    }
    if m.net_pnl_cents >= 0 {
        return None;
    }
    let proposed_min_edge = compute_min_edge_for_breakeven(m);
    Some(Diagnosis {
        code: DiagnosisCode::D1UnprofitableOverSample,
        severity: Severity::Critical,
        strategy: m.strategy.clone(),
        message: format!(
            "Net PnL {}¢ over {} closed trades; strategy is bleeding capital. \
             Either disable until investigated or raise min_edge_cents to a level \
             where expectancy clears 0.",
            m.net_pnl_cents, m.n_trades_closed
        ),
        evidence: serde_json::json!({
            "n_closed": m.n_trades_closed,
            "net_pnl_cents": m.net_pnl_cents,
            "expectancy_cents": m.expectancy_cents,
            "win_rate": m.win_rate,
        }),
        recommendations: vec![
            Recommendation {
                strategy: m.strategy.clone(),
                action: ActionKind::DisableStrategy {
                    reason: "Unprofitable over sufficient sample — pause and investigate".into(),
                },
                current_value: serde_json::json!(null),
                proposed_value: serde_json::json!(null),
                rationale: "Halt-and-investigate is safer than continuing to lose capital".into(),
                confidence: Confidence::High,
            },
            Recommendation {
                strategy: m.strategy.clone(),
                action: ActionKind::RaiseMinEdge {
                    current: 0,
                    proposed: proposed_min_edge,
                },
                current_value: serde_json::json!("(operator: check current min_edge_cents)"),
                proposed_value: serde_json::json!(proposed_min_edge),
                rationale: format!(
                    "Computed from current win rate ({:.1}%), avg win ({:.1}¢), \
                     avg loss ({:.1}¢) — proposed min_edge would push expectancy positive \
                     under the current win/loss distribution.",
                    m.win_rate * 100.0,
                    m.avg_win_cents,
                    m.avg_loss_cents
                ),
                confidence: Confidence::Medium,
            },
        ],
    })
}

fn rule_d2_negative_expectancy(m: &StrategyMetrics, _t: &[Trade]) -> Option<Diagnosis> {
    if m.n_trades_closed < 10 {
        return None;
    }
    if m.expectancy_cents >= 0.0 {
        return None;
    }
    // Avoid double-firing with D1 when the sample is large enough
    // for D1 to be the more authoritative signal.
    if m.n_trades_closed >= MIN_SAMPLE_FOR_CONFIDENCE && m.net_pnl_cents < 0 {
        return None;
    }
    let proposed_min_edge = compute_min_edge_for_breakeven(m);
    Some(Diagnosis {
        code: DiagnosisCode::D2NegativeExpectancy,
        severity: Severity::Warn,
        strategy: m.strategy.clone(),
        message: format!(
            "Per-trade expectancy is {:.1}¢ over {} closed trades. \
             win_rate × avg_win < (1 - win_rate) × |avg_loss|.",
            m.expectancy_cents, m.n_trades_closed
        ),
        evidence: serde_json::json!({
            "expectancy_cents": m.expectancy_cents,
            "win_rate": m.win_rate,
            "avg_win_cents": m.avg_win_cents,
            "avg_loss_cents": m.avg_loss_cents,
        }),
        recommendations: vec![Recommendation {
            strategy: m.strategy.clone(),
            action: ActionKind::RaiseMinEdge {
                current: 0,
                proposed: proposed_min_edge,
            },
            current_value: serde_json::json!("(operator: current min_edge_cents)"),
            proposed_value: serde_json::json!(proposed_min_edge),
            rationale: "Tightening the entry edge filter biases toward higher-expectancy fires."
                .into(),
            confidence: Confidence::Medium,
        }],
    })
}

fn rule_d3_fees_eating_edge(m: &StrategyMetrics, _t: &[Trade]) -> Option<Diagnosis> {
    if m.gross_pnl_cents <= 0 || m.n_trades_closed < 10 {
        return None;
    }
    let ratio = m.fees_paid_cents as f64 / m.gross_pnl_cents as f64;
    if ratio < 0.5 {
        return None;
    }
    let avg_fee = if m.n_trades_closed > 0 {
        m.fees_paid_cents as f64 / m.n_trades_closed as f64
    } else {
        0.0
    };
    let proposed_min_edge = avg_fee.ceil() as i32 + 3;
    Some(Diagnosis {
        code: DiagnosisCode::D3FeesEatingEdge,
        severity: Severity::Warn,
        strategy: m.strategy.clone(),
        message: format!(
            "Fees consume {:.0}% of gross PnL ({}¢ in fees vs {}¢ gross). \
             Strategy is profitable on paper but giving most of it back to the venue.",
            ratio * 100.0,
            m.fees_paid_cents,
            m.gross_pnl_cents
        ),
        evidence: serde_json::json!({
            "fees_pct_of_gross": ratio,
            "fees_paid_cents": m.fees_paid_cents,
            "gross_pnl_cents": m.gross_pnl_cents,
            "avg_fee_per_trade": avg_fee,
        }),
        recommendations: vec![Recommendation {
            strategy: m.strategy.clone(),
            action: ActionKind::RaiseMinEdge {
                current: 0,
                proposed: proposed_min_edge,
            },
            current_value: serde_json::json!("(operator: current min_edge_cents)"),
            proposed_value: serde_json::json!(proposed_min_edge),
            rationale: format!(
                "Avg fee per trade is {:.1}¢; proposed min_edge clears that with a 3¢ cushion.",
                avg_fee
            ),
            confidence: Confidence::Medium,
        }],
    })
}

fn rule_d4_stop_loss_dominant(m: &StrategyMetrics, _t: &[Trade]) -> Option<Diagnosis> {
    if m.n_trades_closed < 10 {
        return None;
    }
    let n_sl = m
        .exits_by_reason
        .get(&ExitReason::StopLoss)
        .copied()
        .unwrap_or(0);
    let ratio = n_sl as f64 / m.n_trades_closed as f64;
    if ratio < 0.5 {
        return None;
    }
    Some(Diagnosis {
        code: DiagnosisCode::D4StopLossDominant,
        severity: Severity::Warn,
        strategy: m.strategy.clone(),
        message: format!(
            "{:.0}% of closes ({} of {}) hit stop-loss. Either entries are too eager \
             (lower fire frequency / raise edge threshold) or stops are too tight \
             relative to typical mark noise.",
            ratio * 100.0,
            n_sl,
            m.n_trades_closed
        ),
        evidence: serde_json::json!({
            "stop_loss_ratio": ratio,
            "n_stop_loss": n_sl,
            "n_total_closed": m.n_trades_closed,
            "exits_by_reason": &m.exits_by_reason,
        }),
        recommendations: vec![Recommendation {
            strategy: m.strategy.clone(),
            action: ActionKind::Investigate {
                what: "Tighten entry edge OR widen stop-loss; pick based on \
                           whether edge or noise is the binding constraint."
                    .into(),
            },
            current_value: serde_json::json!(null),
            proposed_value: serde_json::json!(null),
            rationale: "Direction is data-dependent — operator judgment".into(),
            confidence: Confidence::Low,
        }],
    })
}

fn rule_d5_cap_binding(m: &StrategyMetrics, _t: &[Trade]) -> Option<Diagnosis> {
    if m.n_intents_submitted < 20 {
        return None;
    }
    if m.cap_reject_rate < 0.1 {
        return None;
    }
    Some(Diagnosis {
        code: DiagnosisCode::D5CapBinding,
        severity: Severity::Warn,
        strategy: m.strategy.clone(),
        message: format!(
            "{} of {} intents ({:.0}%) rejected with notional/cap reasons. \
             Risk caps are binding — the strategy wants to fire more than the operator's \
             capital allocation permits.",
            m.n_intents_cap_rejected,
            m.n_intents_submitted,
            m.cap_reject_rate * 100.0
        ),
        evidence: serde_json::json!({
            "cap_reject_rate": m.cap_reject_rate,
            "n_cap_rejected": m.n_intents_cap_rejected,
            "n_total_intents": m.n_intents_submitted,
        }),
        recommendations: vec![Recommendation {
            strategy: m.strategy.clone(),
            action: ActionKind::RaiseRiskCap {
                which: "PREDIGY_MAX_NOTIONAL_CENTS or PREDIGY_MAX_GLOBAL_NOTIONAL_CENTS".into(),
                current: 0,
                proposed: 0,
            },
            current_value: serde_json::json!("(operator: confirm which cap is binding via log greps)"),
            proposed_value: serde_json::json!("Increase the binding cap by ~50%"),
            rationale: "Strategy has more signal than capital. Either raise caps or accept the throughput limit.".into(),
            confidence: Confidence::Medium,
        }],
    })
}

const INTENDED_MAX_HOLD_SECS: i64 = 30 * 60;

fn rule_d6_exits_not_firing(m: &StrategyMetrics, _t: &[Trade]) -> Option<Diagnosis> {
    if m.n_trades_closed < 10 {
        return None;
    }
    if m.p95_hold_secs < 2 * INTENDED_MAX_HOLD_SECS {
        return None;
    }
    Some(Diagnosis {
        code: DiagnosisCode::D6ExitsNotFiring,
        severity: Severity::Warn,
        strategy: m.strategy.clone(),
        message: format!(
            "p95 hold time is {} seconds — exceeds 2× the {}s intended max-hold heuristic. \
             Active-exit triggers (TP/SL/trailing) may not be firing.",
            m.p95_hold_secs, INTENDED_MAX_HOLD_SECS
        ),
        evidence: serde_json::json!({
            "p95_hold_secs": m.p95_hold_secs,
            "median_hold_secs": m.median_hold_secs,
            "intended_max_secs": INTENDED_MAX_HOLD_SECS,
        }),
        recommendations: vec![Recommendation {
            strategy: m.strategy.clone(),
            action: ActionKind::Investigate {
                what: "Audit the exit-trigger config and confirm tick events are reaching \
                       the strategy's evaluate_exit branch."
                    .into(),
            },
            current_value: serde_json::json!(null),
            proposed_value: serde_json::json!(null),
            rationale:
                "Hold-time outliers usually indicate a stuck position the strategy can't close."
                    .into(),
            confidence: Confidence::Medium,
        }],
    })
}

const SLIPPAGE_THRESHOLD_CENTS: f64 = 1.5;

fn rule_d7_slippage_high(m: &StrategyMetrics, _t: &[Trade]) -> Option<Diagnosis> {
    let slip = m.avg_slippage_cents?;
    if slip < SLIPPAGE_THRESHOLD_CENTS {
        return None;
    }
    if m.n_trades_closed < 10 {
        return None;
    }
    let proposed_min_edge_bump = slip.ceil() as i32;
    Some(Diagnosis {
        code: DiagnosisCode::D7SlippageHigh,
        severity: Severity::Warn,
        strategy: m.strategy.clone(),
        message: format!(
            "Avg slippage is {:.1}¢ (intended edge {:.1}¢ vs realized {:.1}¢). \
             Either entries are too aggressive or markets are moving against the order.",
            slip,
            m.avg_intended_edge_cents.unwrap_or(0.0),
            m.avg_realized_edge_cents
        ),
        evidence: serde_json::json!({
            "avg_slippage_cents": slip,
            "avg_intended_edge_cents": m.avg_intended_edge_cents,
            "avg_realized_edge_cents": m.avg_realized_edge_cents,
        }),
        recommendations: vec![Recommendation {
            strategy: m.strategy.clone(),
            action: ActionKind::RaiseMinEdge {
                current: 0,
                proposed: proposed_min_edge_bump,
            },
            current_value: serde_json::json!("(operator: current min_edge_cents)"),
            proposed_value: serde_json::json!(proposed_min_edge_bump),
            rationale: format!(
                "Bumping min_edge by ~{:.0}¢ absorbs the typical slippage and prevents \
                 false-positive fires.",
                slip.ceil()
            ),
            confidence: Confidence::Medium,
        }],
    })
}

fn rule_d8_fire_rate_zero(m: &StrategyMetrics, _t: &[Trade]) -> Option<Diagnosis> {
    let window_hours = (m.window_end - m.window_start).num_hours().max(1);
    if window_hours < 6 {
        return None;
    }
    if m.n_intents_submitted >= 5 {
        return None;
    }
    Some(Diagnosis {
        code: DiagnosisCode::D8FireRateZero,
        severity: Severity::Info,
        strategy: m.strategy.clone(),
        message: format!(
            "Only {} intents submitted over a {}-hour window. Either the strategy is \
             waiting for a rare signal (expected for arb / news strategies) or the \
             entry threshold is too tight to ever clear.",
            m.n_intents_submitted, window_hours
        ),
        evidence: serde_json::json!({
            "n_intents_submitted": m.n_intents_submitted,
            "window_hours": window_hours,
        }),
        recommendations: vec![Recommendation {
            strategy: m.strategy.clone(),
            action: ActionKind::LowerThreshold {
                which: "operator's edge / imbalance / move-threshold env var".into(),
                current: 0.0,
                proposed: 0.0,
            },
            current_value: serde_json::json!("(operator: current threshold)"),
            proposed_value: serde_json::json!("Lower by ~25% if confident in the signal"),
            rationale: "Fire-rate-zero strategies need either lower thresholds or operator confirmation that this is expected.".into(),
            confidence: Confidence::Low,
        }],
    })
}

fn rule_d9_small_sample(m: &StrategyMetrics, _t: &[Trade]) -> Option<Diagnosis> {
    if m.n_trades_closed >= MIN_SAMPLE_FOR_CONFIDENCE {
        return None;
    }
    if m.n_trades_closed == 0 {
        return None;
    }
    Some(Diagnosis {
        code: DiagnosisCode::D9SmallSample,
        severity: Severity::Info,
        strategy: m.strategy.clone(),
        message: format!(
            "Only {} closed trades in the window — metrics are noisy. \
             Aim for ≥{} before drawing strong conclusions.",
            m.n_trades_closed, MIN_SAMPLE_FOR_CONFIDENCE
        ),
        evidence: serde_json::json!({
            "n_closed": m.n_trades_closed,
            "min_for_confidence": MIN_SAMPLE_FOR_CONFIDENCE,
        }),
        recommendations: vec![],
    })
}

/// Compute the min_edge bump that would push expectancy positive
/// under the strategy's current win/loss distribution.
///
/// The heuristic: required_edge ≈ avg loss × loss rate / win rate
/// minus avg win, with a small cushion. Result is positive
/// integer cents.
fn compute_min_edge_for_breakeven(m: &StrategyMetrics) -> i32 {
    if m.win_rate <= 0.0 {
        return 5; // fallback: a sensible non-zero starting point
    }
    let loss_rate = 1.0 - m.win_rate;
    let required = (loss_rate * m.avg_loss_cents.abs()) / m.win_rate - m.avg_win_cents + 2.0;
    required.max(1.0).ceil() as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::collections::HashMap;

    fn empty_metrics(strategy: &str) -> StrategyMetrics {
        let now = Utc::now();
        StrategyMetrics {
            strategy: strategy.into(),
            window_start: now - chrono::Duration::days(1),
            window_end: now,
            n_trades_closed: 0,
            n_trades_open: 0,
            gross_pnl_cents: 0,
            fees_paid_cents: 0,
            net_pnl_cents: 0,
            n_winners: 0,
            n_losers: 0,
            n_breakeven: 0,
            win_rate: 0.0,
            avg_win_cents: 0.0,
            avg_loss_cents: 0.0,
            max_win_cents: 0,
            max_loss_cents: 0,
            expectancy_cents: 0.0,
            stddev_pnl_cents: 0.0,
            sharpe_ratio: 0.0,
            avg_intended_edge_cents: None,
            avg_realized_edge_cents: 0.0,
            avg_slippage_cents: None,
            median_hold_secs: 0,
            p95_hold_secs: 0,
            exits_by_reason: HashMap::new(),
            n_intents_submitted: 0,
            n_intents_filled: 0,
            n_intents_rejected: 0,
            n_intents_cancelled: 0,
            n_intents_cap_rejected: 0,
            fill_rate: 0.0,
            reject_rate: 0.0,
            cap_reject_rate: 0.0,
        }
    }

    #[test]
    fn d1_fires_on_unprofitable_large_sample() {
        let mut m = empty_metrics("stat");
        m.n_trades_closed = 50;
        m.net_pnl_cents = -200;
        m.win_rate = 0.4;
        m.avg_win_cents = 5.0;
        m.avg_loss_cents = -10.0;
        let d = rule_d1_unprofitable_over_sample(&m, &[]);
        assert!(d.is_some());
        assert_eq!(d.unwrap().severity, Severity::Critical);
    }

    #[test]
    fn d1_does_not_fire_on_small_sample() {
        let mut m = empty_metrics("stat");
        m.n_trades_closed = 5;
        m.net_pnl_cents = -100;
        assert!(rule_d1_unprofitable_over_sample(&m, &[]).is_none());
    }

    #[test]
    fn d2_fires_on_negative_expectancy_below_d1_sample() {
        let mut m = empty_metrics("stat");
        m.n_trades_closed = 15;
        m.expectancy_cents = -2.0;
        let d = rule_d2_negative_expectancy(&m, &[]);
        assert!(d.is_some());
        assert_eq!(d.unwrap().severity, Severity::Warn);
    }

    #[test]
    fn d3_fees_eating_edge() {
        let mut m = empty_metrics("stat");
        m.n_trades_closed = 20;
        m.gross_pnl_cents = 100;
        m.fees_paid_cents = 60; // 60% of gross
        let d = rule_d3_fees_eating_edge(&m, &[]);
        assert!(d.is_some());
    }

    #[test]
    fn d4_stop_loss_dominant() {
        let mut m = empty_metrics("stat");
        m.n_trades_closed = 20;
        m.exits_by_reason.insert(ExitReason::StopLoss, 12);
        m.exits_by_reason.insert(ExitReason::TakeProfit, 8);
        let d = rule_d4_stop_loss_dominant(&m, &[]);
        assert!(d.is_some());
    }

    #[test]
    fn d5_cap_binding() {
        let mut m = empty_metrics("stat");
        m.n_intents_submitted = 100;
        m.n_intents_cap_rejected = 15;
        m.cap_reject_rate = 0.15;
        let d = rule_d5_cap_binding(&m, &[]);
        assert!(d.is_some());
    }

    #[test]
    fn d7_slippage_high() {
        let mut m = empty_metrics("stat");
        m.n_trades_closed = 20;
        m.avg_intended_edge_cents = Some(8.0);
        m.avg_realized_edge_cents = 5.0;
        m.avg_slippage_cents = Some(3.0);
        let d = rule_d7_slippage_high(&m, &[]);
        assert!(d.is_some());
    }

    #[test]
    fn d8_fire_rate_zero_in_long_window() {
        let mut m = empty_metrics("stat");
        m.window_start = Utc::now() - chrono::Duration::hours(24);
        m.n_intents_submitted = 1;
        let d = rule_d8_fire_rate_zero(&m, &[]);
        assert!(d.is_some());
    }

    #[test]
    fn d9_small_sample() {
        let mut m = empty_metrics("stat");
        m.n_trades_closed = 5;
        let d = rule_d9_small_sample(&m, &[]);
        assert!(d.is_some());
    }

    #[test]
    fn diagnose_returns_sorted_by_severity() {
        let mut m = empty_metrics("stat");
        m.n_trades_closed = 50;
        m.net_pnl_cents = -200;
        m.win_rate = 0.3;
        m.avg_win_cents = 5.0;
        m.avg_loss_cents = -10.0;
        m.gross_pnl_cents = 100;
        m.fees_paid_cents = 60;
        m.exits_by_reason.insert(ExitReason::StopLoss, 30);
        let ds = diagnose(&m, &[]);
        // First entry should be Critical (D1).
        assert!(!ds.is_empty());
        assert_eq!(ds[0].severity, Severity::Critical);
        // Subsequent entries are Warn or below.
        for d in &ds[1..] {
            assert!(d.severity <= Severity::Warn);
        }
    }
}
