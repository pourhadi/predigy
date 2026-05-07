//! Markdown report renderer — operator-facing summary of the
//! metrics + diagnoses for a window.

use crate::diagnose::{Diagnosis, Severity};
use crate::metrics::StrategyMetrics;
use std::collections::HashMap;
use std::fmt::Write as _;

/// Render a full markdown report covering every strategy in
/// `metrics`. `diagnoses` is keyed by strategy id.
#[must_use]
pub fn render_markdown_report(
    metrics: &HashMap<String, StrategyMetrics>,
    diagnoses: &HashMap<String, Vec<Diagnosis>>,
) -> String {
    let mut out = String::new();
    let any = metrics.values().next();
    if let Some(m) = any {
        let _ = writeln!(
            out,
            "# Strategy evaluation report — {} → {}\n",
            m.window_start.format("%Y-%m-%d %H:%M UTC"),
            m.window_end.format("%Y-%m-%d %H:%M UTC")
        );
    } else {
        let _ = writeln!(out, "# Strategy evaluation report\n");
    }

    let mut strategies: Vec<&String> = metrics.keys().collect();
    strategies.sort();

    // Top-level summary table.
    out.push_str("## Summary\n\n");
    out.push_str(
        "| Strategy | Closed | Win% | Net PnL | Gross | Fees | Expectancy | Sharpe | Diagnoses |\n",
    );
    out.push_str("|---|---:|---:|---:|---:|---:|---:|---:|---|\n");
    for s in &strategies {
        let m = &metrics[*s];
        let critical_count = diagnoses
            .get(*s)
            .map(|ds| {
                ds.iter()
                    .filter(|d| d.severity == Severity::Critical)
                    .count()
            })
            .unwrap_or(0);
        let warn_count = diagnoses
            .get(*s)
            .map(|ds| ds.iter().filter(|d| d.severity == Severity::Warn).count())
            .unwrap_or(0);
        let label = format_health(m, critical_count, warn_count);
        let _ = writeln!(
            out,
            "| {} | {} | {:.1}% | {:+}¢ | {:+}¢ | {}¢ | {:+.1}¢ | {:.2} | {} |",
            s,
            m.n_trades_closed,
            m.win_rate * 100.0,
            m.net_pnl_cents,
            m.gross_pnl_cents,
            m.fees_paid_cents,
            m.expectancy_cents,
            m.sharpe_ratio,
            label
        );
    }
    out.push('\n');

    // Per-strategy details.
    for s in &strategies {
        render_strategy_section(&mut out, &metrics[*s], diagnoses.get(*s));
    }

    out
}

fn format_health(m: &StrategyMetrics, critical: usize, warn: usize) -> String {
    if critical > 0 {
        format!("CRITICAL ({critical})")
    } else if warn > 0 {
        format!("warn ({warn})")
    } else if m.n_trades_closed == 0 {
        "—".into()
    } else if m.net_pnl_cents > 0 {
        "ok".into()
    } else {
        "ok".into()
    }
}

fn render_strategy_section(out: &mut String, m: &StrategyMetrics, ds: Option<&Vec<Diagnosis>>) {
    let _ = writeln!(out, "## `{}`", m.strategy);
    out.push('\n');

    let _ = writeln!(
        out,
        "**PnL** — gross {}¢, fees {}¢, net **{:+}¢**.",
        m.gross_pnl_cents, m.fees_paid_cents, m.net_pnl_cents
    );
    let _ = writeln!(
        out,
        "**Trades** — {} closed, {} open, win rate {:.1}% ({} W / {} L / {} BE).",
        m.n_trades_closed,
        m.n_trades_open,
        m.win_rate * 100.0,
        m.n_winners,
        m.n_losers,
        m.n_breakeven
    );
    let _ = writeln!(
        out,
        "**Per-trade** — expectancy {:+.1}¢, max win {}¢, max loss {}¢, σ {:.1}¢, Sharpe-like {:.2}.",
        m.expectancy_cents, m.max_win_cents, m.max_loss_cents, m.stddev_pnl_cents, m.sharpe_ratio
    );
    if let Some(slip) = m.avg_slippage_cents {
        let _ = writeln!(
            out,
            "**Edge** — intended {:.1}¢, realized {:.1}¢, slippage {:+.1}¢.",
            m.avg_intended_edge_cents.unwrap_or(0.0),
            m.avg_realized_edge_cents,
            slip
        );
    }
    let _ = writeln!(
        out,
        "**Hold** — median {}s, p95 {}s.",
        m.median_hold_secs, m.p95_hold_secs
    );
    let _ = writeln!(
        out,
        "**Activity** — {} intents submitted ({} filled, {} rejected{}, {} cancelled). \
         Fill rate {:.1}%, reject rate {:.1}%, cap-reject rate {:.1}%.",
        m.n_intents_submitted,
        m.n_intents_filled,
        m.n_intents_rejected,
        if m.n_intents_cap_rejected > 0 {
            format!(" of which {} cap-related", m.n_intents_cap_rejected)
        } else {
            String::new()
        },
        m.n_intents_cancelled,
        m.fill_rate * 100.0,
        m.reject_rate * 100.0,
        m.cap_reject_rate * 100.0
    );

    if !m.exits_by_reason.is_empty() {
        out.push_str("\n**Exit-reason mix** — ");
        let mut entries: Vec<(&crate::types::ExitReason, &u64)> =
            m.exits_by_reason.iter().collect();
        entries.sort_by(|a, b| b.1.cmp(a.1));
        let total: u64 = m.exits_by_reason.values().sum();
        let parts: Vec<String> = entries
            .iter()
            .map(|(r, n)| {
                let pct = (**n as f64 / total as f64) * 100.0;
                format!("{} {} ({:.0}%)", n, r.label(), pct)
            })
            .collect();
        let _ = writeln!(out, "{}.", parts.join(", "));
    }

    if let Some(ds) = ds
        && !ds.is_empty()
    {
        out.push_str("\n### Diagnoses\n\n");
        for d in ds {
            let sev = match d.severity {
                Severity::Critical => "🔴 CRITICAL",
                Severity::Warn => "🟡 warn",
                Severity::Info => "🔵 info",
            };
            let _ = writeln!(out, "- **{:?}** ({}): {}", d.code, sev, d.message);
            for r in &d.recommendations {
                let _ = writeln!(
                    out,
                    "  - **Action** ({} confidence): {} — {}",
                    r.confidence.label(),
                    format_action(&r.action),
                    r.rationale
                );
            }
        }
    }
    out.push('\n');
}

fn format_action(a: &crate::recommend::ActionKind) -> String {
    use crate::recommend::ActionKind::*;
    match a {
        RaiseMinEdge { current, proposed } => {
            format!("Raise `min_edge_cents` from {current} → {proposed}")
        }
        LowerMinEdge { current, proposed } => {
            format!("Lower `min_edge_cents` from {current} → {proposed}")
        }
        TightenStopLoss { current, proposed } => {
            format!("Tighten `stop_loss_cents` from {current} → {proposed}")
        }
        WidenStopLoss { current, proposed } => {
            format!("Widen `stop_loss_cents` from {current} → {proposed}")
        }
        AddTrailingStop { trigger, distance } => {
            format!("Enable trailing stop (trigger={trigger}¢, distance={distance}¢)")
        }
        LowerThreshold {
            which,
            current,
            proposed,
        } => {
            format!("Lower {which} from {current:.2} → {proposed:.2}")
        }
        RaiseThreshold {
            which,
            current,
            proposed,
        } => {
            format!("Raise {which} from {current:.2} → {proposed:.2}")
        }
        RaiseRiskCap {
            which,
            current,
            proposed,
        } => {
            format!("Raise {which} from {current} → {proposed}")
        }
        DisableStrategy { reason } => format!("Disable strategy ({reason})"),
        EnableStrategy { reason } => format!("Enable strategy ({reason})"),
        Investigate { what } => format!("Investigate: {what}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnose::DiagnosisCode;
    use crate::recommend::{ActionKind, Confidence, Recommendation};
    use chrono::Utc;
    use std::collections::HashMap;

    fn empty_metrics(strategy: &str) -> StrategyMetrics {
        let now = Utc::now();
        StrategyMetrics {
            strategy: strategy.into(),
            window_start: now - chrono::Duration::days(1),
            window_end: now,
            n_trades_closed: 10,
            n_trades_open: 0,
            gross_pnl_cents: 100,
            fees_paid_cents: 5,
            net_pnl_cents: 95,
            n_winners: 7,
            n_losers: 3,
            n_breakeven: 0,
            win_rate: 0.7,
            avg_win_cents: 20.0,
            avg_loss_cents: -15.0,
            max_win_cents: 30,
            max_loss_cents: -25,
            expectancy_cents: 9.5,
            stddev_pnl_cents: 12.5,
            sharpe_ratio: 0.76,
            avg_intended_edge_cents: Some(10.0),
            avg_realized_edge_cents: 9.0,
            avg_slippage_cents: Some(1.0),
            median_hold_secs: 120,
            p95_hold_secs: 600,
            exits_by_reason: HashMap::new(),
            n_intents_submitted: 50,
            n_intents_filled: 40,
            n_intents_rejected: 5,
            n_intents_cancelled: 5,
            n_intents_cap_rejected: 0,
            fill_rate: 0.8,
            reject_rate: 0.1,
            cap_reject_rate: 0.0,
        }
    }

    #[test]
    fn renders_with_no_diagnoses() {
        let mut metrics = HashMap::new();
        metrics.insert("stat".to_string(), empty_metrics("stat"));
        let diagnoses: HashMap<String, Vec<Diagnosis>> = HashMap::new();
        let report = render_markdown_report(&metrics, &diagnoses);
        assert!(report.contains("# Strategy evaluation report"));
        assert!(report.contains("## `stat`"));
        assert!(report.contains("net **+95¢**"));
    }

    #[test]
    fn renders_with_critical_diagnosis() {
        let mut metrics = HashMap::new();
        metrics.insert("stat".to_string(), empty_metrics("stat"));
        let mut diagnoses = HashMap::new();
        diagnoses.insert(
            "stat".to_string(),
            vec![Diagnosis {
                code: DiagnosisCode::D1UnprofitableOverSample,
                severity: Severity::Critical,
                strategy: "stat".into(),
                message: "test diagnosis".into(),
                evidence: serde_json::json!({}),
                recommendations: vec![Recommendation {
                    strategy: "stat".into(),
                    action: ActionKind::DisableStrategy {
                        reason: "test".into(),
                    },
                    current_value: serde_json::json!(null),
                    proposed_value: serde_json::json!(null),
                    rationale: "test".into(),
                    confidence: Confidence::High,
                }],
            }],
        );
        let report = render_markdown_report(&metrics, &diagnoses);
        assert!(report.contains("CRITICAL"));
        assert!(report.contains("Disable strategy"));
    }
}
