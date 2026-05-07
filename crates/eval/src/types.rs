//! Core types — the `Trade` record and the `ExitReason` taxonomy.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One position lifecycle. Closed trades have `closed_at` and
/// `realized_pnl_cents` populated; open trades have neither (their
/// PnL is unrealized — the dashboard's mark-to-market panel handles
/// that surface). All `_cents` fields are integer cents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trade {
    pub strategy: String,
    pub ticker: String,
    /// Kalshi side: `"yes"` or `"no"`.
    pub side: String,
    /// Signed quantity at open. Positive = long, negative = short.
    /// Kalshi only allows long contracts on each side, so this is
    /// effectively always positive in production but the type
    /// preserves sign for synthetic-test scenarios.
    pub qty_open: i32,
    /// `0` if fully closed.
    pub qty_remaining: i32,
    pub avg_entry_cents: i32,
    /// `None` if still open.
    pub avg_exit_cents: Option<i32>,
    pub realized_pnl_cents: i64,
    pub fees_paid_cents: i64,
    pub opened_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    /// `closed_at - opened_at` for closed trades.
    pub hold_seconds: Option<i64>,
    /// Parsed from the closing intent's `reason` field.
    pub exit_reason: Option<ExitReason>,
    /// I7 multi-leg rollup. `None` for single-leg trades.
    pub leg_group_id: Option<uuid::Uuid>,
    /// Diagnostic — high fill counts on a small position can
    /// indicate cooldown thrash.
    pub n_fills: i32,
    /// Edge claimed in the entry intent's `reason` (e.g.
    /// `"stat fire: model_p=... edge=53.4c"`). `None` if the entry
    /// reason couldn't be parsed.
    pub intended_edge_cents: Option<f64>,
}

impl Trade {
    /// Net realized PnL after subtracting fees. For open trades,
    /// returns `None` since gross PnL isn't realized yet.
    #[must_use]
    pub fn net_pnl_cents(&self) -> Option<i64> {
        self.closed_at.map(|_| self.realized_pnl_cents - self.fees_paid_cents)
    }

    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed_at.is_some()
    }

    #[must_use]
    pub fn is_winner(&self) -> bool {
        self.net_pnl_cents().is_some_and(|p| p > 0)
    }

    #[must_use]
    pub fn is_loser(&self) -> bool {
        self.net_pnl_cents().is_some_and(|p| p < 0)
    }

    #[must_use]
    pub fn is_breakeven(&self) -> bool {
        self.net_pnl_cents() == Some(0)
    }
}

/// Parsed exit-trigger taxonomy. The closing-intent `reason` field
/// is structured (see strategies' fire/exit log lines); the parser
/// in [`ExitReason::parse_reason`] maps the leading tag to a
/// variant. Unknown tags fall back to `Unknown` so reports stay
/// non-empty rather than panicking.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum ExitReason {
    /// `:tp:` — take-profit hit (stat / cross-arb / settlement).
    TakeProfit,
    /// `:sl:` — stop-loss hit.
    StopLoss,
    /// `:ts:` — trailing stop (stat / cross-arb A3).
    TrailingStop,
    /// `:bd:` — belief-drift (stat A1).
    BeliefDrift,
    /// `:conv:` — convergence-aware exit (cross-arb A2).
    Convergence,
    /// `:inv:` — thesis-inversion exit (cross-arb A2).
    ThesisInversion,
    /// `latency-flat:` — latency tiered force-flat (A5).
    LatencyFlat,
    /// `settlement-fade:` — settlement S1 sell-YES on overconfident touch.
    SettlementFade,
    /// Operator manual close.
    Manual,
    /// Venue auto-settled at expiry. Inferred when there is no
    /// closing intent but `closed_at` is set.
    Settled,
    /// Tag couldn't be parsed.
    Unknown,
}

impl ExitReason {
    /// Parse from a closing intent's `reason` field. The format
    /// tags shipped during the audit round are recognized; anything
    /// else maps to `Unknown`.
    #[must_use]
    pub fn parse_reason(reason: &str) -> Self {
        // The structured tags appear inside the reason string,
        // separated by colons. Match in priority order — more-
        // specific tags first.
        if reason.contains("latency-flat:") {
            return Self::LatencyFlat;
        }
        if reason.contains("settlement-fade") {
            return Self::SettlementFade;
        }
        // Compact `:tag:` markers. Order matters when multiple
        // could match; the first hit wins.
        let tags = [
            (":tp:", Self::TakeProfit),
            (":sl:", Self::StopLoss),
            (":ts:", Self::TrailingStop),
            (":bd:", Self::BeliefDrift),
            (":conv:", Self::Convergence),
            (":inv:", Self::ThesisInversion),
        ];
        for (tag, kind) in tags {
            if reason.contains(tag) {
                return kind;
            }
        }
        Self::Unknown
    }

    /// Display label for reports. `kebab-case` so it groups well in
    /// table sort orders.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::TakeProfit => "take-profit",
            Self::StopLoss => "stop-loss",
            Self::TrailingStop => "trailing-stop",
            Self::BeliefDrift => "belief-drift",
            Self::Convergence => "convergence",
            Self::ThesisInversion => "thesis-inversion",
            Self::LatencyFlat => "latency-flat",
            Self::SettlementFade => "settlement-fade",
            Self::Manual => "manual",
            Self::Settled => "settled",
            Self::Unknown => "unknown",
        }
    }
}

/// Parse the `edge=NNc` substring from an entry intent's reason
/// (e.g. `"stat fire: model_p=0.564 ask=3c edge=53.4c size=3"`).
/// Returns `None` if no `edge=` token is present or it doesn't
/// parse as a float. Used for slippage analysis in metrics.
#[must_use]
pub fn parse_intended_edge(reason: &str) -> Option<f64> {
    let i = reason.find("edge=")?;
    let tail = &reason[i + 5..];
    // Pull out the numeric prefix (digits, decimal point, sign).
    let end = tail
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-')
        .unwrap_or(tail.len());
    tail[..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_take_profit() {
        assert_eq!(ExitReason::parse_reason("stat-exit:KX:Y:tp:01abc"), ExitReason::TakeProfit);
    }

    #[test]
    fn parses_stop_loss() {
        assert_eq!(
            ExitReason::parse_reason("stat-exit:KX:Y:sl:01abc"),
            ExitReason::StopLoss
        );
    }

    #[test]
    fn parses_trailing_stop() {
        assert_eq!(
            ExitReason::parse_reason("cross-arb-exit:KX:Y:ts:01:0001"),
            ExitReason::TrailingStop
        );
    }

    #[test]
    fn parses_belief_drift() {
        assert_eq!(
            ExitReason::parse_reason("stat-exit:KX:Y:bd:01abc"),
            ExitReason::BeliefDrift
        );
    }

    #[test]
    fn parses_convergence_inversion() {
        assert_eq!(
            ExitReason::parse_reason("cross-arb-exit:KX:Y:conv:01:0001"),
            ExitReason::Convergence
        );
        assert_eq!(
            ExitReason::parse_reason("cross-arb-exit:KX:Y:inv:01:0001"),
            ExitReason::ThesisInversion
        );
    }

    #[test]
    fn parses_latency_flat() {
        assert_eq!(
            ExitReason::parse_reason("latency-flat:t1 held_360s entry=50c limit=60c pnl=10c"),
            ExitReason::LatencyFlat
        );
    }

    #[test]
    fn parses_settlement_fade() {
        assert_eq!(
            ExitReason::parse_reason("settlement-fade: ticker=KX size=1 ask=99"),
            ExitReason::SettlementFade
        );
    }

    #[test]
    fn unknown_when_no_tag() {
        assert_eq!(
            ExitReason::parse_reason("some unstructured reason"),
            ExitReason::Unknown
        );
    }

    #[test]
    fn parse_intended_edge_basic() {
        assert_eq!(
            parse_intended_edge("stat fire: model_p=0.564 ask=3c edge=53.4c size=3"),
            Some(53.4)
        );
    }

    #[test]
    fn parse_intended_edge_integer() {
        assert_eq!(
            parse_intended_edge("internal-arb FAM: total_ask=90c fee=2c edge=8c"),
            Some(8.0)
        );
    }

    #[test]
    fn parse_intended_edge_missing() {
        assert_eq!(parse_intended_edge("no edge token"), None);
    }
}
