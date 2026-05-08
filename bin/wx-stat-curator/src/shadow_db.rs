//! Postgres shadow writes for wx-stat calibration evidence.
//!
//! These writes are deliberately non-order-entry: they upsert market
//! metadata, append `model_p_snapshots`, and keep DB `rules` disabled.
//! The live `wx-stat` strategy continues to consume the JSON rule file.

use anyhow::{Context as _, Result};
use predigy_core::side::Side;
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use wx_stat_curator::predictions::{PredictionMeasurement, PredictionRecord};
use wx_stat_curator::ticker_parse::settlement_date_from_ticker;

#[derive(Debug, Clone)]
pub struct ShadowRuleRecord {
    pub ticker: String,
    pub title: String,
    pub event_ticker: String,
    pub series_ticker: String,
    pub close_time: String,
    pub side: Side,
    pub raw_p: f64,
    pub model_p: f64,
    pub min_edge_cents: u32,
    pub settlement_date: Option<String>,
    pub generated_at_utc: String,
    pub source: String,
    pub detail: Value,
}

pub async fn write_shadow_records(
    database_url: &str,
    records: &[ShadowRuleRecord],
) -> Result<usize> {
    if records.is_empty() {
        return Ok(0);
    }
    let pool = PgPoolOptions::new()
        .max_connections(3)
        .connect(database_url)
        .await
        .with_context(|| format!("connect postgres {database_url}"))?;

    let mut inserted_snapshots = 0_usize;
    for record in records {
        upsert_market(&pool, record).await?;
        upsert_disabled_rule(&pool, record).await?;
        inserted_snapshots += insert_snapshot_if_absent(
            &pool,
            &record.ticker,
            &record.generated_at_utc,
            record.raw_p,
            record.model_p,
            &record.source,
            &record.detail,
        )
        .await?;
    }
    Ok(inserted_snapshots)
}

pub async fn backfill_prediction_records(
    database_url: &str,
    records: &[PredictionRecord],
) -> Result<usize> {
    if records.is_empty() {
        return Ok(0);
    }
    let pool = PgPoolOptions::new()
        .max_connections(3)
        .connect(database_url)
        .await
        .with_context(|| format!("connect postgres {database_url}"))?;

    let mut inserted = 0_usize;
    for record in records {
        upsert_prediction_market(&pool, record).await?;
        let detail = prediction_detail(record, "wx-stat-curator-prediction-backfill");
        inserted += insert_snapshot_if_absent(
            &pool,
            &record.ticker,
            &record.run_ts_utc,
            record.raw_p,
            record.model_p,
            "wx-stat-curator-prediction-backfill",
            &detail,
        )
        .await?;
    }
    Ok(inserted)
}

async fn upsert_market(pool: &sqlx::PgPool, record: &ShadowRuleRecord) -> Result<()> {
    let payload = json!({
        "source": record.source,
        "event_ticker": record.event_ticker,
        "series_ticker": record.series_ticker,
        "shadow_only": true,
        "detail": record.detail,
    });
    sqlx::query(
        r"
        INSERT INTO markets (ticker, venue, market_type, title, close_time, tags, payload)
        VALUES ($1, 'kalshi', 'binary', $2, NULLIF($3, '')::TIMESTAMPTZ, $4, $5)
        ON CONFLICT (ticker) DO UPDATE
        SET title = COALESCE(EXCLUDED.title, markets.title),
            close_time = COALESCE(EXCLUDED.close_time, markets.close_time),
            tags = COALESCE(markets.tags, EXCLUDED.tags),
            payload = EXCLUDED.payload,
            last_updated_at = now()
        ",
    )
    .bind(&record.ticker)
    .bind(&record.title)
    .bind(&record.close_time)
    .bind(vec![
        "weather".to_string(),
        "wx-stat".to_string(),
        "shadow".to_string(),
    ])
    .bind(payload)
    .execute(pool)
    .await?;
    Ok(())
}

async fn upsert_prediction_market(pool: &sqlx::PgPool, record: &PredictionRecord) -> Result<()> {
    let payload = prediction_detail(record, "wx-stat-curator-prediction-backfill");
    sqlx::query(
        r"
        INSERT INTO markets (ticker, venue, market_type, title, tags, payload)
        VALUES ($1, 'kalshi', 'binary', $2, $3, $4)
        ON CONFLICT (ticker) DO UPDATE
        SET title = COALESCE(markets.title, EXCLUDED.title),
            tags = COALESCE(markets.tags, EXCLUDED.tags),
            payload = COALESCE(markets.payload, EXCLUDED.payload),
            last_updated_at = now()
        ",
    )
    .bind(&record.ticker)
    .bind(format!("wx-stat shadow {}", record.ticker))
    .bind(vec![
        "weather".to_string(),
        "wx-stat".to_string(),
        "shadow-backfill".to_string(),
    ])
    .bind(payload)
    .execute(pool)
    .await?;
    Ok(())
}

async fn upsert_disabled_rule(pool: &sqlx::PgPool, record: &ShadowRuleRecord) -> Result<()> {
    sqlx::query(
        r"
        INSERT INTO rules (
            strategy, ticker, side, model_p, min_edge_cents,
            expires_at, source, enabled
        ) VALUES (
            'wx-stat', $1, $2, $3, $4,
            NULLIF($6, '')::DATE + INTERVAL '2 days',
            $5, false
        )
        ON CONFLICT (strategy, ticker) DO UPDATE
        SET side = EXCLUDED.side,
            model_p = EXCLUDED.model_p,
            min_edge_cents = EXCLUDED.min_edge_cents,
            expires_at = EXCLUDED.expires_at,
            source = EXCLUDED.source,
            fitted_at = now(),
            enabled = false
        ",
    )
    .bind(&record.ticker)
    .bind(side_str(record.side))
    .bind(record.model_p)
    .bind(i32::try_from(record.min_edge_cents).unwrap_or(i32::MAX))
    .bind(&record.source)
    .bind(record.settlement_date.as_deref().unwrap_or(""))
    .execute(pool)
    .await?;
    Ok(())
}

async fn insert_snapshot_if_absent(
    pool: &sqlx::PgPool,
    ticker: &str,
    ts_utc: &str,
    raw_p: f64,
    model_p: f64,
    source: &str,
    detail: &Value,
) -> Result<usize> {
    let result = sqlx::query(
        r"
        INSERT INTO model_p_snapshots (strategy, ticker, ts, raw_p, model_p, source, detail)
        SELECT 'wx-stat', $1, $2::TIMESTAMPTZ, $3, $4, $5, $6
        WHERE NOT EXISTS (
            SELECT 1
            FROM model_p_snapshots
            WHERE strategy = 'wx-stat'
              AND ticker = $1
              AND ts = $2::TIMESTAMPTZ
              AND source = $5
        )
        ",
    )
    .bind(ticker)
    .bind(ts_utc)
    .bind(raw_p)
    .bind(model_p)
    .bind(source)
    .bind(detail)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() as usize)
}

pub fn prediction_detail(record: &PredictionRecord, source: &str) -> Value {
    json!({
        "source": source,
        "run_ts_utc": record.run_ts_utc,
        "ticker": record.ticker,
        "airport": record.airport,
        "settlement_date": record.settlement_date,
        "canonical_settlement_date": settlement_date_from_ticker(&record.ticker),
        "settlement_date_matches_ticker": settlement_date_from_ticker(&record.ticker)
            .is_none_or(|date| date == record.settlement_date),
        "curation_model_version": record.curation_model_version,
        "threshold_k": record.threshold_k,
        "yes_when_above": record.yes_when_above,
        "measurement": measurement_str(record.measurement),
        "raw_p": record.raw_p,
        "model_p": record.model_p,
        "forecast_50pct_f": record.forecast_50pct_f,
        "calibration_sample_eligible": true,
    })
}

fn side_str(side: Side) -> &'static str {
    match side {
        Side::Yes => "yes",
        Side::No => "no",
    }
}

fn measurement_str(measurement: PredictionMeasurement) -> &'static str {
    match measurement {
        PredictionMeasurement::DailyHigh => "daily_high",
        PredictionMeasurement::DailyLow => "daily_low",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pred() -> PredictionRecord {
        PredictionRecord {
            curation_model_version: Some(wx_stat_curator::NBM_CURATION_MODEL_VERSION.to_string()),
            run_ts_utc: "2026-05-08T12:00:00Z".to_string(),
            ticker: "KXHIGHDEN-26MAY08-T70".to_string(),
            airport: "DEN".to_string(),
            settlement_date: "2026-05-08".to_string(),
            threshold_k: 294.0,
            yes_when_above: true,
            measurement: PredictionMeasurement::DailyHigh,
            raw_p: 0.72,
            model_p: 0.70,
            forecast_50pct_f: 73.0,
        }
    }

    #[test]
    fn prediction_detail_marks_calibration_eligible() {
        let detail = prediction_detail(&pred(), "test-source");
        assert_eq!(detail["source"], "test-source");
        assert_eq!(detail["strategy"], Value::Null);
        assert_eq!(detail["airport"], "DEN");
        assert_eq!(detail["measurement"], "daily_high");
        assert_eq!(detail["calibration_sample_eligible"], true);
    }
}
