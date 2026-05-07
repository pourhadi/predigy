//! Per-strategy metric aggregation.
//!
//! Pure function: given a slice of [`Trade`] (typically the output
//! of [`crate::ledger::load_trades`]) and intent-activity counts,
//! produce one [`StrategyMetrics`] per strategy.

use crate::ledger::IntentActivity;
use crate::types::{ExitReason, Trade};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyMetrics {
    pub strategy: String,
    pub window_start: DateTime<Utc>,
    pub window_end: DateTime<Utc>,

    // ─── PnL ────────────────────────────────────────────────
    pub n_trades_closed: u64,
    pub n_trades_open: u64,
    pub gross_pnl_cents: i64,
    pub fees_paid_cents: i64,
    pub net_pnl_cents: i64,

    // ─── Distribution ──────────────────────────────────────
    pub n_winners: u64,
    pub n_losers: u64,
    pub n_breakeven: u64,
    pub win_rate: f64,
    pub avg_win_cents: f64,
    pub avg_loss_cents: f64,
    pub max_win_cents: i64,
    pub max_loss_cents: i64,
    pub expectancy_cents: f64,
    pub stddev_pnl_cents: f64,
    pub sharpe_ratio: f64,

    // ─── Edge & slippage ───────────────────────────────────
    pub avg_intended_edge_cents: Option<f64>,
    pub avg_realized_edge_cents: f64,
    pub avg_slippage_cents: Option<f64>,

    // ─── Hold time ─────────────────────────────────────────
    pub median_hold_secs: i64,
    pub p95_hold_secs: i64,

    // ─── Exit-reason mix ───────────────────────────────────
    pub exits_by_reason: HashMap<ExitReason, u64>,

    // ─── Activity ──────────────────────────────────────────
    pub n_intents_submitted: u64,
    pub n_intents_filled: u64,
    pub n_intents_rejected: u64,
    pub n_intents_cancelled: u64,
    pub n_intents_cap_rejected: u64,
    pub fill_rate: f64,
    pub reject_rate: f64,
    pub cap_reject_rate: f64,
}

/// Aggregate trades + intent activity into per-strategy metrics.
///
/// `window_start` / `window_end` are stored on the result for
/// downstream report rendering — they're not used in the math here.
#[must_use]
pub fn compute_metrics(
    trades: &[Trade],
    activity: &HashMap<String, IntentActivity>,
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
) -> HashMap<String, StrategyMetrics> {
    // Group trades by strategy.
    let mut by_strategy: HashMap<String, Vec<&Trade>> = HashMap::new();
    for t in trades {
        by_strategy.entry(t.strategy.clone()).or_default().push(t);
    }
    // Also create entries for strategies that have intent activity
    // but no trades yet (e.g. firing-but-rejected case).
    for k in activity.keys() {
        by_strategy.entry(k.clone()).or_default();
    }

    let mut out = HashMap::with_capacity(by_strategy.len());
    for (strategy, ts) in by_strategy {
        let m = compute_one(
            &strategy,
            &ts,
            activity.get(&strategy),
            window_start,
            window_end,
        );
        out.insert(strategy, m);
    }
    out
}

fn compute_one(
    strategy: &str,
    trades: &[&Trade],
    activity: Option<&IntentActivity>,
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
) -> StrategyMetrics {
    let closed: Vec<&&Trade> = trades.iter().filter(|t| t.is_closed()).collect();
    let open: Vec<&&Trade> = trades.iter().filter(|t| !t.is_closed()).collect();

    let pnl_per_trade: Vec<i64> = closed.iter().filter_map(|t| t.net_pnl_cents()).collect();
    let gross: i64 = closed.iter().map(|t| t.realized_pnl_cents).sum();
    let fees: i64 = closed.iter().map(|t| t.fees_paid_cents).sum();
    let net: i64 = pnl_per_trade.iter().sum();

    let winners: Vec<i64> = pnl_per_trade.iter().copied().filter(|&p| p > 0).collect();
    let losers: Vec<i64> = pnl_per_trade.iter().copied().filter(|&p| p < 0).collect();
    let n_breakeven = pnl_per_trade.iter().filter(|&&p| p == 0).count() as u64;
    let n_winners = winners.len() as u64;
    let n_losers = losers.len() as u64;
    let n_decided = n_winners + n_losers;
    let win_rate = if n_decided > 0 {
        n_winners as f64 / n_decided as f64
    } else {
        0.0
    };
    let avg_win = mean(&winners);
    let avg_loss = mean(&losers); // negative
    let max_win = winners.iter().copied().max().unwrap_or(0);
    let max_loss = losers.iter().copied().min().unwrap_or(0);
    let expectancy = mean(&pnl_per_trade);
    let stddev = stddev(&pnl_per_trade);
    let sharpe = if stddev > 0.0 {
        expectancy / stddev
    } else {
        0.0
    };

    // Edge analysis. We compare per-contract intended edge to
    // per-contract realized PnL on closed trades. Closed-trade
    // realized-per-contract = realized_pnl / qty_open.
    let intended_edges: Vec<f64> = closed
        .iter()
        .filter_map(|t| t.intended_edge_cents)
        .collect();
    let avg_intended_edge = if intended_edges.is_empty() {
        None
    } else {
        Some(intended_edges.iter().sum::<f64>() / intended_edges.len() as f64)
    };
    let realized_per_contract: Vec<f64> = closed
        .iter()
        .filter(|t| t.qty_open != 0)
        .map(|t| {
            // Realize PnL is on the closed portion. For fully-
            // closed trades qty_open is the closed_qty.
            t.realized_pnl_cents as f64 / t.qty_open.abs() as f64
        })
        .collect();
    let avg_realized_edge = if realized_per_contract.is_empty() {
        0.0
    } else {
        realized_per_contract.iter().sum::<f64>() / realized_per_contract.len() as f64
    };
    let avg_slippage = avg_intended_edge.map(|i| i - avg_realized_edge);

    // Hold time percentiles.
    let mut holds: Vec<i64> = closed.iter().filter_map(|t| t.hold_seconds).collect();
    holds.sort_unstable();
    let median_hold_secs = percentile(&holds, 0.5);
    let p95_hold_secs = percentile(&holds, 0.95);

    // Exit-reason mix.
    let mut exits_by_reason: HashMap<ExitReason, u64> = HashMap::new();
    for t in &closed {
        if let Some(r) = t.exit_reason {
            *exits_by_reason.entry(r).or_insert(0) += 1;
        }
    }

    // Activity counters.
    let act = activity.cloned().unwrap_or_default();
    let fill_rate = if act.total > 0 {
        act.filled as f64 / act.total as f64
    } else {
        0.0
    };
    let reject_rate = if act.total > 0 {
        act.rejected as f64 / act.total as f64
    } else {
        0.0
    };
    let cap_reject_rate = if act.total > 0 {
        act.cap_rejected as f64 / act.total as f64
    } else {
        0.0
    };

    StrategyMetrics {
        strategy: strategy.to_string(),
        window_start,
        window_end,
        n_trades_closed: closed.len() as u64,
        n_trades_open: open.len() as u64,
        gross_pnl_cents: gross,
        fees_paid_cents: fees,
        net_pnl_cents: net,
        n_winners,
        n_losers,
        n_breakeven,
        win_rate,
        avg_win_cents: avg_win,
        avg_loss_cents: avg_loss,
        max_win_cents: max_win,
        max_loss_cents: max_loss,
        expectancy_cents: expectancy,
        stddev_pnl_cents: stddev,
        sharpe_ratio: sharpe,
        avg_intended_edge_cents: avg_intended_edge,
        avg_realized_edge_cents: avg_realized_edge,
        avg_slippage_cents: avg_slippage,
        median_hold_secs,
        p95_hold_secs,
        exits_by_reason,
        n_intents_submitted: act.total,
        n_intents_filled: act.filled,
        n_intents_rejected: act.rejected,
        n_intents_cancelled: act.cancelled,
        n_intents_cap_rejected: act.cap_rejected,
        fill_rate,
        reject_rate,
        cap_reject_rate,
    }
}

fn mean(xs: &[i64]) -> f64 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().map(|&x| x as f64).sum::<f64>() / xs.len() as f64
    }
}

fn stddev(xs: &[i64]) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    let m = mean(xs);
    let var = xs.iter().map(|&x| (x as f64 - m).powi(2)).sum::<f64>() / (xs.len() - 1) as f64;
    var.sqrt()
}

fn percentile(sorted: &[i64], p: f64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let n = sorted.len();
    let i = ((n as f64 - 1.0) * p).round() as usize;
    sorted[i.min(n - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn t_closed(strategy: &str, qty: i32, realized: i64, fees: i64, hold_s: i64) -> Trade {
        let opened_at = Utc::now() - Duration::seconds(hold_s);
        Trade {
            strategy: strategy.into(),
            ticker: "KX-T".into(),
            side: "yes".into(),
            qty_open: qty,
            qty_remaining: 0,
            avg_entry_cents: 50,
            avg_exit_cents: Some(60),
            realized_pnl_cents: realized,
            fees_paid_cents: fees,
            opened_at,
            closed_at: Some(Utc::now()),
            hold_seconds: Some(hold_s),
            exit_reason: Some(ExitReason::TakeProfit),
            leg_group_id: None,
            n_fills: 2,
            intended_edge_cents: Some(10.0),
        }
    }

    #[test]
    fn computes_winners_and_losers() {
        let trades = vec![
            t_closed("stat", 1, 10, 1, 60),  // net +9
            t_closed("stat", 1, -5, 1, 120), // net -6
            t_closed("stat", 1, 15, 1, 30),  // net +14
        ];
        let refs: Vec<&Trade> = trades.iter().collect();
        let now = Utc::now();
        let m = compute_one("stat", &refs, None, now, now);
        assert_eq!(m.n_trades_closed, 3);
        assert_eq!(m.n_winners, 2);
        assert_eq!(m.n_losers, 1);
        assert!((m.win_rate - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(m.gross_pnl_cents, 20);
        assert_eq!(m.fees_paid_cents, 3);
        assert_eq!(m.net_pnl_cents, 17);
    }

    #[test]
    fn empty_strategy_has_zero_metrics() {
        let now = Utc::now();
        let m = compute_one("ghost", &[], None, now, now);
        assert_eq!(m.n_trades_closed, 0);
        assert_eq!(m.n_winners, 0);
        assert_eq!(m.win_rate, 0.0);
    }

    #[test]
    fn percentile_handles_edge_cases() {
        assert_eq!(percentile(&[], 0.5), 0);
        assert_eq!(percentile(&[10], 0.5), 10);
        assert_eq!(percentile(&[10, 20, 30, 40, 50], 0.5), 30);
    }

    #[test]
    fn stddev_zero_for_single_sample() {
        assert_eq!(stddev(&[5]), 0.0);
    }
}
