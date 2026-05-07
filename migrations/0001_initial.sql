-- Predigy initial schema.
--
-- See docs/ARCHITECTURE.md "Database" section for full rationale.
-- Conventions:
--   - All times: TIMESTAMPTZ (Postgres stores UTC, displays in
--     session timezone). Application code always passes UTC.
--   - Money in cents: BIGINT. Probabilities: DOUBLE PRECISION
--     in [0, 1].
--   - JSON payloads: JSONB (queryable + indexable, faster than JSON).
--   - Surrogate keys: BIGSERIAL where no natural key fits.
--   - Strategy id: TEXT (small enum: "latency", "stat", "cross-arb",
--     "settlement", "wx-stat", and curator names). NOT a separate
--     table — keeping it inline keeps queries simple at this scale.
--
-- Idempotent CREATE TABLE IF NOT EXISTS so re-running the import
-- tool against a partially-populated DB doesn't error.

-- ─── Markets ─────────────────────────────────────────────────
-- Static-ish metadata about every market we've ever cared about.
-- Updated by curators when a new market is discovered or its
-- settlement window changes. Source of truth for "is this ticker
-- known and what kind of market is it".
CREATE TABLE IF NOT EXISTS markets (
    ticker         TEXT PRIMARY KEY,
    -- "kalshi" | "polymarket"
    venue          TEXT NOT NULL,
    -- "binary" | "scalar" | "categorical"
    market_type    TEXT NOT NULL,
    title          TEXT,
    settlement_ts  TIMESTAMPTZ,        -- when YES/NO is final-determined
    close_time     TIMESTAMPTZ,        -- when the auction stops accepting orders
    -- Market kind tags for strategy filtering. e.g. ["weather",
    -- "daily_high", "DEN"] or ["sports","mlb"].
    tags           TEXT[],
    -- Full venue payload preserved for replay (rules, strikes,
    -- categorical labels, etc.). Strategies should prefer the
    -- typed columns above; payload is for forensic queries.
    payload        JSONB,
    first_seen_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS markets_venue_idx       ON markets(venue);
CREATE INDEX IF NOT EXISTS markets_settlement_idx  ON markets(settlement_ts) WHERE settlement_ts IS NOT NULL;
CREATE INDEX IF NOT EXISTS markets_close_time_idx  ON markets(close_time)    WHERE close_time IS NOT NULL;

-- ─── Intents (audit trail of every order ever submitted) ────
-- Append-only. Each intent corresponds to exactly one logical
-- order across its lifecycle (submit → ack → fill / cancel).
-- Status transitions update this row in place; all status events
-- ALSO go into intent_events for the full audit timeline.
CREATE TABLE IF NOT EXISTS intents (
    -- Operator-namespaced client order id; e.g. "stat:KXFOO-X:00012345".
    client_id        TEXT PRIMARY KEY,
    strategy         TEXT NOT NULL,
    ticker           TEXT NOT NULL REFERENCES markets(ticker),
    -- "yes" | "no"
    side             TEXT NOT NULL,
    -- "buy" | "sell"
    action           TEXT NOT NULL,
    price_cents      INTEGER,         -- NULL for market orders
    qty              INTEGER NOT NULL,
    order_type       TEXT NOT NULL,   -- "limit" | "market"
    tif              TEXT NOT NULL,   -- "ioc" | "gtc" | "fok"
    -- Operator-readable rationale for this order. Used for grep-
    -- the-log forensics.
    reason           TEXT,
    -- Current state (mirrors latest intent_events.status).
    status           TEXT NOT NULL,
    cumulative_qty   INTEGER NOT NULL DEFAULT 0,
    avg_fill_price_cents INTEGER,
    venue_order_id   TEXT,            -- assigned by venue on ack
    submitted_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS intents_strategy_idx    ON intents(strategy);
CREATE INDEX IF NOT EXISTS intents_ticker_idx      ON intents(ticker);
CREATE INDEX IF NOT EXISTS intents_status_idx      ON intents(status);
CREATE INDEX IF NOT EXISTS intents_submitted_idx   ON intents(submitted_at);

-- Per-intent state-transition history. Append-only.
CREATE TABLE IF NOT EXISTS intent_events (
    id           BIGSERIAL PRIMARY KEY,
    client_id    TEXT NOT NULL REFERENCES intents(client_id),
    ts           TIMESTAMPTZ NOT NULL DEFAULT now(),
    status       TEXT NOT NULL,    -- "submitted" | "acked" | "partial_fill" | "filled" | "cancelled" | "rejected" | ...
    -- Whatever the venue returned (FIX ExecutionReport, REST
    -- response body) so we can reconstruct what we knew at each
    -- step.
    venue_payload JSONB
);

CREATE INDEX IF NOT EXISTS intent_events_client_id_idx ON intent_events(client_id);
CREATE INDEX IF NOT EXISTS intent_events_ts_idx        ON intent_events(ts);

-- ─── Fills (what actually executed at the venue) ────────────
-- Append-only. Multiple rows per intent for partial fills.
-- Fill cascade updates positions + materialised PnL views.
CREATE TABLE IF NOT EXISTS fills (
    id              BIGSERIAL PRIMARY KEY,
    client_id       TEXT NOT NULL REFERENCES intents(client_id),
    -- Venue-assigned fill id, if any. Used to dedupe duplicate
    -- fills from FIX + REST cross-checks.
    venue_fill_id   TEXT,
    ticker          TEXT NOT NULL REFERENCES markets(ticker),
    strategy        TEXT NOT NULL,
    -- "yes" | "no"
    side            TEXT NOT NULL,
    -- "buy" | "sell"
    action          TEXT NOT NULL,
    price_cents     INTEGER NOT NULL,
    qty             INTEGER NOT NULL,
    fee_cents       INTEGER NOT NULL DEFAULT 0,
    ts              TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS fills_client_id_idx  ON fills(client_id);
CREATE INDEX IF NOT EXISTS fills_ticker_idx     ON fills(ticker);
CREATE INDEX IF NOT EXISTS fills_strategy_idx   ON fills(strategy);
CREATE INDEX IF NOT EXISTS fills_ts_idx         ON fills(ts);
-- Cross-source dedupe — same venue_fill_id can't be inserted twice.
CREATE UNIQUE INDEX IF NOT EXISTS fills_venue_fill_id_uniq
    ON fills(venue_fill_id) WHERE venue_fill_id IS NOT NULL;

-- ─── Positions (current open exposure per strategy × market) ──
-- One logical position per (strategy, ticker, side). Lifecycle:
-- opened on first fill, accumulates / nets on subsequent fills,
-- closes when current_qty hits zero. Closed positions stay in the
-- table for history; queries that want "currently open" filter on
-- closed_at IS NULL.
CREATE TABLE IF NOT EXISTS positions (
    id              BIGSERIAL PRIMARY KEY,
    strategy        TEXT NOT NULL,
    ticker          TEXT NOT NULL REFERENCES markets(ticker),
    -- "yes" | "no"
    side            TEXT NOT NULL,
    -- Signed contract count: positive = long, negative = short.
    -- Updated by the OMS as fills land.
    current_qty     INTEGER NOT NULL,
    avg_entry_cents INTEGER NOT NULL,
    -- Realised P&L in cents — accumulated as fills close partial
    -- legs. Excludes unrealised mark-to-market.
    realized_pnl_cents BIGINT NOT NULL DEFAULT 0,
    fees_paid_cents    BIGINT NOT NULL DEFAULT 0,
    opened_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    closed_at       TIMESTAMPTZ,
    last_fill_at    TIMESTAMPTZ
);

-- One open position per (strategy, ticker, side) at any moment.
CREATE UNIQUE INDEX IF NOT EXISTS positions_open_uniq
    ON positions(strategy, ticker, side) WHERE closed_at IS NULL;

CREATE INDEX IF NOT EXISTS positions_ticker_idx     ON positions(ticker);
CREATE INDEX IF NOT EXISTS positions_strategy_idx   ON positions(strategy);
CREATE INDEX IF NOT EXISTS positions_open_idx       ON positions(strategy, ticker) WHERE closed_at IS NULL;

-- ─── model_p time series ────────────────────────────────────
-- Every model_p value any strategy ever computed. Used both for
-- live trading (latest row per (strategy, ticker)) and offline
-- calibration (join with fills + market settlements).
--
-- This grows fast — wx-stat alone produces ~50 rows × 8 cycles/day
-- = 400/day = 150K/year. With all strategies + a few years' history
-- we land in the millions. Indexed on (ticker, ts) for efficient
-- "latest model_p for this ticker" queries.
CREATE TABLE IF NOT EXISTS model_p_snapshots (
    id             BIGSERIAL PRIMARY KEY,
    strategy       TEXT NOT NULL,
    ticker         TEXT NOT NULL REFERENCES markets(ticker),
    ts             TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Pre-calibration model probability — the raw output of the
    -- strategy's belief computation. Stored as DOUBLE PRECISION
    -- not NUMERIC because we never need exact decimal arithmetic
    -- on probabilities.
    raw_p          DOUBLE PRECISION NOT NULL,
    -- Post-calibration probability the trader actually uses.
    -- Equals raw_p when no calibration applies.
    model_p        DOUBLE PRECISION NOT NULL,
    -- Free-form provenance — "nbm:cycle=2026-05-06T12Z fcst_h=24"
    -- or "claude:run_id=abc123" — for replay and audit.
    source         TEXT,
    -- Optional payload: the inputs the model used (NBM quantile
    -- vector, Polymarket implied probability, etc.). NULL when
    -- separately stored in model_p_inputs.
    detail         JSONB,
    CONSTRAINT model_p_snapshots_p_range
        CHECK (raw_p   BETWEEN 0 AND 1
           AND model_p BETWEEN 0 AND 1)
);

CREATE INDEX IF NOT EXISTS model_p_snapshots_ticker_ts_idx
    ON model_p_snapshots(ticker, ts DESC);
CREATE INDEX IF NOT EXISTS model_p_snapshots_strategy_ticker_ts_idx
    ON model_p_snapshots(strategy, ticker, ts DESC);

-- ─── Raw probabilistic inputs (for replay) ──────────────────
-- NBM quantile vectors, Polymarket book snapshots, NWS forecast
-- values etc. Keyed by (source, key, ts). Lets us re-run
-- calibration with new logic against historical inputs without
-- re-fetching from the venue.
CREATE TABLE IF NOT EXISTS model_p_inputs (
    id            BIGSERIAL PRIMARY KEY,
    -- "nbm" | "nws_forecast" | "polymarket_book" | ...
    source        TEXT NOT NULL,
    -- Source-specific key. For NBM: airport code + cycle + fcst
    -- hour. For Polymarket: asset_id. The combination
    -- (source, key, ts) is unique.
    key           TEXT NOT NULL,
    ts            TIMESTAMPTZ NOT NULL,
    payload       JSONB NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS model_p_inputs_source_key_ts_uniq
    ON model_p_inputs(source, key, ts);
CREATE INDEX IF NOT EXISTS model_p_inputs_source_ts_idx
    ON model_p_inputs(source, ts DESC);

-- ─── Rules ──────────────────────────────────────────────────
-- Currently-active strategy rules. Curators upsert; strategies
-- query at startup + watch for INSERT/UPDATE notifications.
-- Replaces the per-strategy rule JSON files.
CREATE TABLE IF NOT EXISTS rules (
    id                 BIGSERIAL PRIMARY KEY,
    strategy           TEXT NOT NULL,
    ticker             TEXT NOT NULL REFERENCES markets(ticker),
    -- "yes" | "no"
    side               TEXT NOT NULL,
    -- The probability the strategy uses for this rule. Mirrors the
    -- latest model_p_snapshot at curate time but persisted here so
    -- a stale strategy module can read its rule set without joining.
    model_p            DOUBLE PRECISION NOT NULL,
    min_edge_cents     INTEGER NOT NULL DEFAULT 5,
    -- Rule expires (e.g. settlement passed) → strategy stops firing.
    expires_at         TIMESTAMPTZ,
    -- Free-form provenance.
    source             TEXT NOT NULL,
    fitted_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- For dry-run / paper rules, set this to false.
    enabled            BOOLEAN NOT NULL DEFAULT true,
    CONSTRAINT rules_p_range CHECK (model_p BETWEEN 0 AND 1)
);

CREATE UNIQUE INDEX IF NOT EXISTS rules_strategy_ticker_uniq
    ON rules(strategy, ticker);
CREATE INDEX IF NOT EXISTS rules_strategy_enabled_idx
    ON rules(strategy) WHERE enabled = true;

-- ─── Kill switches ──────────────────────────────────────────
-- Per-strategy and global. When `armed=true` for a row whose
-- scope matches a strategy, that strategy refuses new entries
-- and flushes existing positions. The legacy file-based switch
-- (~/.config/predigy/kill-switch.flag) stays as a defence-in-
-- depth fallback.
CREATE TABLE IF NOT EXISTS kill_switches (
    id            BIGSERIAL PRIMARY KEY,
    -- "global" | "<strategy_name>"
    scope         TEXT NOT NULL UNIQUE,
    armed         BOOLEAN NOT NULL DEFAULT false,
    set_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    set_by        TEXT,
    reason        TEXT
);

-- ─── Calibration (Phase 2E) ─────────────────────────────────
-- Fitted Platt-scaling coefficients per (strategy, scope_key, month).
-- The scope_key is strategy-defined ("DEN" for wx-stat, etc.).
-- Lookup misses fall back to identity (a=0, b=1).
CREATE TABLE IF NOT EXISTS calibration (
    id            BIGSERIAL PRIMARY KEY,
    strategy      TEXT NOT NULL,
    scope_key     TEXT NOT NULL,        -- e.g. "DEN", "OKC"
    month         SMALLINT NOT NULL,    -- 1..12
    a             DOUBLE PRECISION NOT NULL,
    b             DOUBLE PRECISION NOT NULL,
    n_samples     INTEGER NOT NULL,
    fitted_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT calibration_month_range CHECK (month BETWEEN 1 AND 12)
);

CREATE UNIQUE INDEX IF NOT EXISTS calibration_uniq
    ON calibration(strategy, scope_key, month);

-- ─── Settlements ────────────────────────────────────────────
-- One row per market that's resolved. Used by the calibration
-- fitter, the daily P&L roll-up, and any forensic query that
-- needs to know "how did this market end".
CREATE TABLE IF NOT EXISTS settlements (
    ticker         TEXT PRIMARY KEY REFERENCES markets(ticker),
    -- Resolved value: 1.0 = YES wins, 0.0 = NO wins. For scalar
    -- markets, the actual numeric value (e.g. observed temperature).
    resolved_value DOUBLE PRECISION NOT NULL,
    settled_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Where the value came from — "kalshi" | "nws_clim_report" | ...
    source         TEXT NOT NULL,
    payload        JSONB
);

-- ─── Books (latest-only snapshot) ───────────────────────────
-- Most-recent (best_yes_bid, best_yes_ask) seen for each ticker.
-- Updated by the engine's market-data router on every WS book
-- delta. Used for live risk + UI; *not* a full book history (that
-- would explode quickly; we use Kalshi's md-recorder for replay).
CREATE TABLE IF NOT EXISTS book_snapshots (
    ticker             TEXT PRIMARY KEY REFERENCES markets(ticker),
    best_yes_bid_cents INTEGER,
    best_yes_ask_cents INTEGER,
    best_yes_bid_qty   INTEGER,
    best_yes_ask_qty   INTEGER,
    last_trade_cents   INTEGER,
    last_update        TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ─── Schema version pin ─────────────────────────────────────
-- sqlx tracks its own _sqlx_migrations table; this is a stronger
-- application-level pin so the engine can refuse to run against
-- an unexpected schema.
CREATE TABLE IF NOT EXISTS schema_meta (
    key           TEXT PRIMARY KEY,
    value         TEXT NOT NULL,
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

INSERT INTO schema_meta (key, value)
    VALUES ('schema_version', '0001_initial')
    ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, updated_at = now();
