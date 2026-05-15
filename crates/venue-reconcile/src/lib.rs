// Shared venue-flat settlement reconciliation: when Kalshi settles a
// market, the local `positions` row needs to be closed with the
// realized P&L baked in from the final outcome. This lives in its
// own crate so both `predigy-engine` (called from the OMS reconcile
// loop, ~60s cadence) and `predigy-calibration` (the
// belt-and-suspenders cron, ~2m cadence) can call the same code path.
//
// Idempotency: every position close is gated on `closed_at IS NULL`,
// so concurrent passes from the engine and the calibration timer
// can't double-close. Same for the markets/settlements upsert
// (ON CONFLICT DO UPDATE).

#![allow(clippy::doc_markdown)]

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use predigy_kalshi_rest::types::MarketDetail;
use predigy_kalshi_rest::{Client as KalshiClient, Error as RestError};
use serde::Serialize;
use serde_json::json;
use sqlx::{Postgres, postgres::PgPool};
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OpenPositionRow {
    pub id: i64,
    pub ticker: String,
    pub side: String,
    pub current_qty: i32,
    pub avg_entry_cents: i32,
}

#[derive(Debug)]
pub struct VenueFlatCandidate {
    pub ticker: String,
    pub db_qty: i32,
    pub rows: Vec<OpenPositionRow>,
}

#[derive(Debug, Serialize)]
pub struct VenueFlatReport {
    pub mode: &'static str,
    pub strategy: Option<String>,
    pub db_open_tickers: usize,
    pub venue_open_tickers: usize,
    pub venue_flat_candidates: usize,
    pub checked_candidates: usize,
    pub eligible_settled: usize,
    pub closed_tickers: usize,
    pub closed_position_rows: usize,
    pub realized_pnl_delta_cents: i64,
    pub tickers: Vec<VenueFlatTickerReport>,
}

#[derive(Debug, Serialize)]
pub struct VenueFlatTickerReport {
    pub ticker: String,
    pub db_qty: i32,
    pub venue_qty: i32,
    pub n_position_rows: usize,
    pub status: String,
    pub outcome: Option<f64>,
    pub settled_at: Option<DateTime<Utc>>,
    pub action: &'static str,
    pub realized_pnl_delta_cents: i64,
    pub reason: String,
}

/// Outcome of resolving a single ticker. Returned by the
/// engine-facing entry point so the caller can log/track results
/// without parsing the larger `VenueFlatReport`.
#[derive(Debug)]
pub enum DriftResolution {
    /// Position(s) closed against a finalized settlement outcome.
    Closed {
        closed_rows: usize,
        realized_pnl_delta_cents: i64,
        outcome: f64,
        settled_at: DateTime<Utc>,
    },
    /// Venue says flat but the market is not yet settled — leave the
    /// ghost in place; next pass will retry.
    NotYetSettled,
    /// No DB-open positions found for the ticker; nothing to do.
    NoOpenRows,
    /// Kalshi returned 404 (ticker not in market_detail). Treat as
    /// "skip this pass" — the calibration sync handles the long tail.
    MarketDetailMissing,
}

pub async fn fetch_open_positions(
    pool: &PgPool,
    strategy: Option<&str>,
) -> Result<Vec<OpenPositionRow>> {
    let rows = sqlx::query_as::<_, OpenPositionRow>(
        r"
        SELECT id, ticker, side, current_qty, avg_entry_cents
        FROM positions
        WHERE closed_at IS NULL
          AND ($1::TEXT IS NULL OR strategy = $1)
        ORDER BY ticker, strategy, side, id
        ",
    )
    .bind(strategy)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

async fn fetch_open_positions_for_ticker(
    pool: &PgPool,
    ticker: &str,
) -> Result<Vec<OpenPositionRow>> {
    let rows = sqlx::query_as::<_, OpenPositionRow>(
        r"
        SELECT id, ticker, side, current_qty, avg_entry_cents
        FROM positions
        WHERE closed_at IS NULL AND ticker = $1
        ORDER BY strategy, side, id
        ",
    )
    .bind(ticker)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn fetch_venue_positions(rest: &KalshiClient) -> Result<HashMap<String, i32>> {
    let positions = rest.positions().await.context("fetch Kalshi positions")?;
    let mut by_ticker = HashMap::new();
    for position in positions.market_positions {
        let qty = venue_position_qty(position.position_contracts);
        if qty != 0 {
            *by_ticker.entry(position.ticker).or_insert(0) += qty;
        }
    }
    Ok(by_ticker)
}

pub fn venue_position_qty(position_contracts: Option<f64>) -> i32 {
    let Some(qty) = position_contracts else {
        return 0;
    };
    if qty.is_finite() {
        qty.round() as i32
    } else {
        0
    }
}

pub fn venue_flat_candidates(
    open_rows: Vec<OpenPositionRow>,
    venue_by_ticker: &HashMap<String, i32>,
) -> (usize, Vec<VenueFlatCandidate>) {
    let mut grouped: BTreeMap<String, Vec<OpenPositionRow>> = BTreeMap::new();
    for row in open_rows {
        grouped.entry(row.ticker.clone()).or_default().push(row);
    }
    let db_open_tickers = grouped.len();
    let mut candidates = Vec::new();
    for (ticker, rows) in grouped {
        if venue_by_ticker.get(&ticker).copied().unwrap_or(0) != 0 {
            continue;
        }
        let db_qty = signed_db_qty(&rows);
        candidates.push(VenueFlatCandidate {
            ticker,
            db_qty,
            rows,
        });
    }
    (db_open_tickers, candidates)
}

pub fn signed_db_qty(rows: &[OpenPositionRow]) -> i32 {
    rows.iter()
        .map(|row| {
            if row.side == "yes" {
                row.current_qty
            } else {
                -row.current_qty
            }
        })
        .sum()
}

pub fn final_settlement_outcome(detail: &MarketDetail) -> Option<(f64, DateTime<Utc>)> {
    let outcome = market_outcome_value(detail)?;
    if !(0.0..=1.0).contains(&outcome) || !outcome.is_finite() {
        return None;
    }
    if let Some(settled_at) = parse_rfc3339_utc(detail.settled_time.as_deref()) {
        return Some((outcome, settled_at));
    }
    if market_status_is_final(&detail.status) {
        return Some((outcome, Utc::now()));
    }
    None
}

pub fn market_status_is_final(status: &str) -> bool {
    let normalized: String = status
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    matches!(
        normalized.as_str(),
        "settled" | "finalized" | "resolved" | "closed"
    )
}

pub fn candidate_settlement_pnl(candidate: &VenueFlatCandidate, outcome: f64) -> Result<i64> {
    candidate
        .rows
        .iter()
        .map(|row| position_settlement_pnl(row, outcome))
        .sum()
}

pub fn position_settlement_pnl(row: &OpenPositionRow, yes_outcome: f64) -> Result<i64> {
    let settlement_price_cents = settlement_price_for_side(&row.side, yes_outcome)
        .with_context(|| format!("unknown position side {:?} for {}", row.side, row.ticker))?;
    Ok(i64::from(settlement_price_cents - row.avg_entry_cents)
        * i64::from(row.current_qty.signum())
        * i64::from(row.current_qty.abs()))
}

pub fn settlement_price_for_side(side: &str, yes_outcome: f64) -> Option<i32> {
    match side {
        "yes" => Some((yes_outcome * 100.0).round() as i32),
        "no" => Some(((1.0 - yes_outcome) * 100.0).round() as i32),
        _ => None,
    }
}

pub async fn close_settled_venue_flat_ticker(
    pool: &PgPool,
    candidate: &VenueFlatCandidate,
    detail: &MarketDetail,
    outcome: f64,
) -> Result<(usize, i64)> {
    let mut tx = pool.begin().await?;
    let settled_at = upsert_market_and_settlement_tx(&mut tx, detail, outcome).await?;
    let mut closed_rows = 0_usize;
    let mut realized_delta = 0_i64;
    for row in &candidate.rows {
        let pnl_delta = position_settlement_pnl(row, outcome)?;
        let result = sqlx::query(
            r"
            UPDATE positions
            SET current_qty = 0,
                closed_at = $2,
                last_fill_at = $2,
                realized_pnl_cents = realized_pnl_cents + $3
            WHERE id = $1
              AND closed_at IS NULL
            ",
        )
        .bind(row.id)
        .bind(settled_at)
        .bind(pnl_delta)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() > 0 {
            closed_rows += 1;
            realized_delta += pnl_delta;
        }
    }
    tx.commit().await?;
    Ok((closed_rows, realized_delta))
}

pub async fn upsert_market_and_settlement(
    pool: &PgPool,
    detail: &MarketDetail,
    outcome: f64,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    upsert_market_and_settlement_tx(&mut tx, detail, outcome).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn upsert_market_and_settlement_tx(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    detail: &MarketDetail,
    outcome: f64,
) -> Result<DateTime<Utc>> {
    let close_time = parse_rfc3339_utc(Some(detail.close_time.as_str()));
    let settled_at = parse_rfc3339_utc(detail.settled_time.as_deref()).unwrap_or_else(Utc::now);
    let settlement_ts =
        Some(settled_at).or_else(|| parse_rfc3339_utc(detail.expected_expiration_time.as_deref()));
    let payload = json!({
        "source": "predigy-venue-reconcile",
        "event_ticker": detail.event_ticker,
        "status": detail.status,
        "result": detail.result,
        "market_result": detail.market_result,
        "settled_time": detail.settled_time,
        "floor_strike": detail.floor_strike,
        "cap_strike": detail.cap_strike,
        "strike_type": detail.strike_type,
    });
    sqlx::query(
        r"
        INSERT INTO markets (
            ticker, venue, market_type, title, settlement_ts,
            close_time, tags, payload
        ) VALUES ($1, 'kalshi', 'binary', $2, $3, $4, $5, $6)
        ON CONFLICT (ticker) DO UPDATE
        SET title = COALESCE(EXCLUDED.title, markets.title),
            settlement_ts = COALESCE(EXCLUDED.settlement_ts, markets.settlement_ts),
            close_time = COALESCE(EXCLUDED.close_time, markets.close_time),
            payload = EXCLUDED.payload,
            last_updated_at = now()
        ",
    )
    .bind(&detail.ticker)
    .bind(&detail.title)
    .bind(settlement_ts)
    .bind(close_time)
    .bind(vec![
        "kalshi-settled".to_string(),
        "venue-reconcile".to_string(),
    ])
    .bind(&payload)
    .execute(&mut **tx)
    .await?;

    sqlx::query(
        r"
        INSERT INTO settlements (ticker, resolved_value, settled_at, source, payload)
        VALUES ($1, $2, $3, 'kalshi-market-detail', $4)
        ON CONFLICT (ticker) DO UPDATE
        SET resolved_value = EXCLUDED.resolved_value,
            settled_at = EXCLUDED.settled_at,
            source = EXCLUDED.source,
            payload = EXCLUDED.payload
        ",
    )
    .bind(&detail.ticker)
    .bind(outcome)
    .bind(settled_at)
    .bind(payload)
    .execute(&mut **tx)
    .await?;
    Ok(settled_at)
}

/// Full-scan venue-flat reconciliation. Used by `predigy-calibration`
/// as the periodic belt-and-suspenders sweep. Pulls all open DB
/// positions, fetches the venue snapshot, and acts on every flat
/// candidate up to `limit`.
pub async fn reconcile_venue_flat(
    pool: &PgPool,
    rest: &KalshiClient,
    strategy: Option<&str>,
    limit: i64,
    write: bool,
) -> Result<VenueFlatReport> {
    let open_rows = fetch_open_positions(pool, strategy).await?;
    let venue_by_ticker = fetch_venue_positions(rest).await?;
    let (db_open_tickers, candidates) = venue_flat_candidates(open_rows, &venue_by_ticker);
    let venue_flat_candidates = candidates.len();

    let mut report = VenueFlatReport {
        mode: if write { "write" } else { "dry-run" },
        strategy: strategy.map(ToOwned::to_owned),
        db_open_tickers,
        venue_open_tickers: venue_by_ticker.len(),
        venue_flat_candidates,
        checked_candidates: 0,
        eligible_settled: 0,
        closed_tickers: 0,
        closed_position_rows: 0,
        realized_pnl_delta_cents: 0,
        tickers: Vec::new(),
    };

    for candidate in candidates.into_iter().take(limit as usize) {
        report.checked_candidates += 1;
        let detail = rest
            .market_detail(&candidate.ticker)
            .await
            .with_context(|| format!("fetch market detail {}", candidate.ticker))?
            .market;
        let venue_qty = venue_by_ticker.get(&candidate.ticker).copied().unwrap_or(0);
        let status = detail.status.clone();
        let outcome = market_outcome_value(&detail);
        let settled = final_settlement_outcome(&detail);
        let Some((outcome_value, settled_at)) = settled else {
            report.tickers.push(VenueFlatTickerReport {
                ticker: candidate.ticker,
                db_qty: candidate.db_qty,
                venue_qty,
                n_position_rows: candidate.rows.len(),
                status,
                outcome,
                settled_at: None,
                action: "skipped",
                realized_pnl_delta_cents: 0,
                reason: "venue flat but market detail has no final settled binary outcome"
                    .to_string(),
            });
            continue;
        };

        report.eligible_settled += 1;
        let expected_pnl = candidate_settlement_pnl(&candidate, outcome_value)?;
        if write {
            let (closed_rows, realized_delta) =
                close_settled_venue_flat_ticker(pool, &candidate, &detail, outcome_value).await?;
            report.closed_tickers += usize::from(closed_rows > 0);
            report.closed_position_rows += closed_rows;
            report.realized_pnl_delta_cents += realized_delta;
            report.tickers.push(VenueFlatTickerReport {
                ticker: candidate.ticker,
                db_qty: candidate.db_qty,
                venue_qty,
                n_position_rows: candidate.rows.len(),
                status,
                outcome: Some(outcome_value),
                settled_at: Some(settled_at),
                action: "closed",
                realized_pnl_delta_cents: realized_delta,
                reason: format!(
                    "closed {closed_rows} DB-open rows against settled venue-flat market"
                ),
            });
        } else {
            report.tickers.push(VenueFlatTickerReport {
                ticker: candidate.ticker,
                db_qty: candidate.db_qty,
                venue_qty,
                n_position_rows: candidate.rows.len(),
                status,
                outcome: Some(outcome_value),
                settled_at: Some(settled_at),
                action: "would_close",
                realized_pnl_delta_cents: expected_pnl,
                reason: "dry run: would close DB-open rows against settled venue-flat market"
                    .to_string(),
            });
        }
    }
    Ok(report)
}

/// Engine-facing per-ticker entry point. The OMS reconcile loop in
/// `predigy-engine` already detects drift (DB open, venue flat) on
/// each pass. For each such ticker it calls this to attempt the
/// close — much cheaper than re-running the whole `reconcile_venue_flat`
/// scan inside the engine, and the caller already knows venue_qty == 0
/// for the ticker so we skip that round-trip.
pub async fn resolve_settled_drift_ticker(
    pool: &PgPool,
    rest: &KalshiClient,
    ticker: &str,
) -> Result<DriftResolution> {
    let rows = fetch_open_positions_for_ticker(pool, ticker).await?;
    if rows.is_empty() {
        return Ok(DriftResolution::NoOpenRows);
    }
    let db_qty = signed_db_qty(&rows);
    let detail = match rest.market_detail(ticker).await {
        Ok(d) => d.market,
        Err(RestError::Api { status: 404, .. }) => {
            return Ok(DriftResolution::MarketDetailMissing);
        }
        Err(e) => return Err(anyhow!("fetch market detail {ticker}: {e}")),
    };
    let Some((outcome, settled_at)) = final_settlement_outcome(&detail) else {
        return Ok(DriftResolution::NotYetSettled);
    };
    let candidate = VenueFlatCandidate {
        ticker: ticker.to_string(),
        db_qty,
        rows,
    };
    let (closed_rows, realized_delta) =
        close_settled_venue_flat_ticker(pool, &candidate, &detail, outcome).await?;
    Ok(DriftResolution::Closed {
        closed_rows,
        realized_pnl_delta_cents: realized_delta,
        outcome,
        settled_at,
    })
}

fn parse_rfc3339_utc(raw: Option<&str>) -> Option<DateTime<Utc>> {
    raw.and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

pub fn market_outcome_value(detail: &MarketDetail) -> Option<f64> {
    detail
        .market_result
        .as_deref()
        .or(detail.result.as_deref())
        .and_then(binary_outcome_value)
}

pub fn binary_outcome_value(raw: &str) -> Option<f64> {
    let normalized: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    match normalized.as_str() {
        "yes" | "y" | "true" | "1" => Some(1.0),
        "no" | "n" | "false" | "0" => Some(0.0),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn market_detail(
        status: &str,
        result: Option<&str>,
        settled_time: Option<&str>,
    ) -> MarketDetail {
        MarketDetail {
            ticker: "KX".to_string(),
            event_ticker: "EV".to_string(),
            title: "test".to_string(),
            status: status.to_string(),
            close_time: "2026-05-08T12:00:00Z".to_string(),
            expected_expiration_time: None,
            can_close_early: None,
            yes_bid_dollars: None,
            yes_ask_dollars: None,
            liquidity_dollars: None,
            volume: None,
            result: result.map(ToOwned::to_owned),
            market_result: None,
            settled_time: settled_time.map(ToOwned::to_owned),
            floor_strike: None,
            cap_strike: None,
            strike_type: None,
        }
    }

    #[test]
    fn parses_binary_outcomes_from_common_kalshi_shapes() {
        assert_eq!(binary_outcome_value("yes"), Some(1.0));
        assert_eq!(binary_outcome_value("YES"), Some(1.0));
        assert_eq!(binary_outcome_value("no"), Some(0.0));
        assert_eq!(binary_outcome_value("No"), Some(0.0));
        assert_eq!(binary_outcome_value("not settled"), None);
    }

    #[test]
    fn final_outcome_requires_settlement_marker() {
        let mut detail = market_detail("settled", Some("yes"), None);
        assert!(final_settlement_outcome(&detail).is_some());

        detail.status = "active".to_string();
        assert!(final_settlement_outcome(&detail).is_none());

        detail.settled_time = Some("2026-05-08T12:00:00Z".to_string());
        assert!(final_settlement_outcome(&detail).is_some());
    }

    #[test]
    fn settlement_pnl_uses_side_domain_price() {
        let yes_long = OpenPositionRow {
            id: 1,
            ticker: "KX".to_string(),
            side: "yes".to_string(),
            current_qty: 3,
            avg_entry_cents: 60,
        };
        let no_long = OpenPositionRow {
            id: 2,
            ticker: "KX".to_string(),
            side: "no".to_string(),
            current_qty: 3,
            avg_entry_cents: 20,
        };
        let no_short = OpenPositionRow {
            id: 3,
            ticker: "KX".to_string(),
            side: "no".to_string(),
            current_qty: -3,
            avg_entry_cents: 20,
        };

        assert_eq!(position_settlement_pnl(&yes_long, 1.0).unwrap(), 120);
        assert_eq!(position_settlement_pnl(&yes_long, 0.0).unwrap(), -180);
        assert_eq!(position_settlement_pnl(&no_long, 0.0).unwrap(), 240);
        assert_eq!(position_settlement_pnl(&no_short, 0.0).unwrap(), -240);
    }

    #[test]
    fn venue_flat_candidates_include_net_zero_db_gross() {
        let rows = vec![
            OpenPositionRow {
                id: 1,
                ticker: "KX".to_string(),
                side: "yes".to_string(),
                current_qty: 2,
                avg_entry_cents: 50,
            },
            OpenPositionRow {
                id: 2,
                ticker: "KX".to_string(),
                side: "no".to_string(),
                current_qty: 2,
                avg_entry_cents: 50,
            },
        ];
        let venue = HashMap::new();
        let (db_open, candidates) = venue_flat_candidates(rows, &venue);
        assert_eq!(db_open, 1);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].db_qty, 0);
    }
}
