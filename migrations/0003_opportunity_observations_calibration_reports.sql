-- Fill-growth observation and calibration reporting tables.
--
-- Additive migration only: no existing live order/intents tables are
-- modified. The opportunity scanner is deliberately observation-only;
-- it writes here and never writes intents/orders/fills/positions.

CREATE TABLE IF NOT EXISTS opportunity_observations (
    id              BIGSERIAL PRIMARY KEY,
    ts              TIMESTAMPTZ NOT NULL DEFAULT now(),
    strategy        TEXT NOT NULL,
    opportunity_key TEXT NOT NULL,
    tickers         TEXT[] NOT NULL,
    kind            TEXT NOT NULL,
    raw_edge_cents  DOUBLE PRECISION,
    net_edge_cents  DOUBLE PRECISION,
    max_size        INTEGER,
    would_fire      BOOLEAN NOT NULL DEFAULT false,
    reason          TEXT,
    payload         JSONB NOT NULL DEFAULT '{}'::jsonb
);

CREATE INDEX IF NOT EXISTS opportunity_observations_strategy_ts_idx
    ON opportunity_observations(strategy, ts DESC);
CREATE INDEX IF NOT EXISTS opportunity_observations_key_ts_idx
    ON opportunity_observations(strategy, opportunity_key, ts DESC);
CREATE INDEX IF NOT EXISTS opportunity_observations_edge_idx
    ON opportunity_observations(strategy, net_edge_cents DESC)
    WHERE net_edge_cents IS NOT NULL;

CREATE TABLE IF NOT EXISTS calibration_reports (
    id              BIGSERIAL PRIMARY KEY,
    strategy        TEXT NOT NULL,
    window_start    TIMESTAMPTZ NOT NULL,
    window_end      TIMESTAMPTZ NOT NULL,
    n_predictions   INTEGER NOT NULL,
    n_settled       INTEGER NOT NULL,
    brier           DOUBLE PRECISION,
    log_loss        DOUBLE PRECISION,
    net_pnl_cents   BIGINT,
    baseline        JSONB,
    bins            JSONB NOT NULL,
    diagnosis       JSONB NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT calibration_reports_window_order
        CHECK (window_end >= window_start),
    CONSTRAINT calibration_reports_counts_nonnegative
        CHECK (n_predictions >= 0 AND n_settled >= 0),
    CONSTRAINT calibration_reports_metrics_nonnegative
        CHECK ((brier IS NULL OR brier >= 0) AND (log_loss IS NULL OR log_loss >= 0))
);

CREATE INDEX IF NOT EXISTS calibration_reports_strategy_window_idx
    ON calibration_reports(strategy, window_end DESC);
CREATE INDEX IF NOT EXISTS calibration_reports_created_idx
    ON calibration_reports(created_at DESC);
