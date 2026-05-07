//! Postgres connection handle + read-side helpers shared across
//! every consumer (engine, dashboard, ad-hoc tools).
//!
//! Strategies don't talk to Postgres directly — they call into
//! these helpers through a `&Db` they're handed at startup.
//! That keeps SQL out of strategy code and keeps the SQL we DO
//! write checkable at compile time.

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::time::Duration;

/// Wrapper around the Postgres pool. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Db {
    pool: PgPool,
}

impl Db {
    /// Connect using the canonical predigy connection string. With
    /// peer auth on the local UNIX socket the URL is
    /// `postgresql:///predigy` (no host, no user, no password —
    /// see `docs/DATABASE.md`).
    pub async fn connect(url: &str) -> Result<Self, sqlx::Error> {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(Duration::from_secs(5))
            .connect(url)
            .await?;
        Ok(Self { pool })
    }

    /// Underlying pool — for code paths that need direct sqlx
    /// access (custom queries, transactions). Most callers should
    /// use the typed helpers below.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Snapshot of currently-open positions, optionally filtered
    /// by strategy. Used by the dashboard and the OMS.
    pub async fn open_positions(
        &self,
        strategy: Option<&str>,
    ) -> Result<Vec<OpenPosition>, sqlx::Error> {
        let rows = sqlx::query_as::<_, OpenPosition>(
            "SELECT strategy, ticker, side, current_qty,
                    avg_entry_cents, realized_pnl_cents,
                    fees_paid_cents, opened_at, last_fill_at
               FROM positions
              WHERE closed_at IS NULL
                AND ($1::TEXT IS NULL OR strategy = $1)
              ORDER BY opened_at DESC",
        )
        .bind(strategy)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Per-strategy daily realised P&L for the current UTC day.
    pub async fn daily_realized_pnl(&self) -> Result<Vec<DailyPnl>, sqlx::Error> {
        let rows = sqlx::query_as::<_, DailyPnl>(
            "SELECT strategy,
                    COALESCE(SUM(realized_pnl_cents), 0)::BIGINT AS realized_pnl_cents,
                    COUNT(*)::BIGINT                              AS n_positions
               FROM positions
              WHERE closed_at >= date_trunc('day', now())
              GROUP BY strategy
              ORDER BY realized_pnl_cents DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Latest model_p value seen per (strategy, ticker). Used by
    /// the engine when bootstrapping a strategy at startup so it
    /// doesn't fire entries until a fresh enough number lands.
    pub async fn latest_model_p(
        &self,
        strategy: &str,
        ticker: &str,
    ) -> Result<Option<LatestModelP>, sqlx::Error> {
        let row = sqlx::query_as::<_, LatestModelP>(
            "SELECT ts, raw_p, model_p, source
               FROM model_p_snapshots
              WHERE strategy = $1 AND ticker = $2
              ORDER BY ts DESC
              LIMIT 1",
        )
        .bind(strategy)
        .bind(ticker)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Whether any kill switch is armed for the given scope (or
    /// `"global"`). Strategies poll this; the engine refuses new
    /// intents whose strategy scope is armed.
    pub async fn kill_switch_armed(&self, scope: &str) -> Result<bool, sqlx::Error> {
        let row: Option<(bool,)> = sqlx::query_as(
            "SELECT armed FROM kill_switches WHERE scope IN ($1, 'global') AND armed = true LIMIT 1",
        )
        .bind(scope)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    /// Currently-enabled rules for a strategy. Returns rows in
    /// no particular order; callers index by ticker as needed.
    pub async fn active_rules(&self, strategy: &str) -> Result<Vec<RuleRow>, sqlx::Error> {
        let rows = sqlx::query_as::<_, RuleRow>(
            "SELECT id, strategy, ticker, side, model_p, min_edge_cents,
                    expires_at, source, fitted_at, enabled
               FROM rules
              WHERE strategy = $1
                AND enabled = true
                AND (expires_at IS NULL OR expires_at > now())",
        )
        .bind(strategy)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct OpenPosition {
    pub strategy: String,
    pub ticker: String,
    pub side: String,
    pub current_qty: i32,
    pub avg_entry_cents: i32,
    pub realized_pnl_cents: i64,
    pub fees_paid_cents: i64,
    pub opened_at: chrono::DateTime<chrono::Utc>,
    pub last_fill_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct DailyPnl {
    pub strategy: String,
    pub realized_pnl_cents: i64,
    pub n_positions: i64,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct LatestModelP {
    pub ts: chrono::DateTime<chrono::Utc>,
    pub raw_p: f64,
    pub model_p: f64,
    pub source: Option<String>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RuleRow {
    pub id: i64,
    pub strategy: String,
    pub ticker: String,
    pub side: String,
    pub model_p: f64,
    pub min_edge_cents: i32,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub source: String,
    pub fitted_at: chrono::DateTime<chrono::Utc>,
    pub enabled: bool,
}
