-- Paper-trading ledger for the stat strategy (and any future
-- strategy that wants to prove its edge before risking cash).
--
-- Each row is "what would have happened if we'd lifted this rule
-- at the live touch price at decision time, held to settlement,
-- and paid the standard taker fee." Settlement reconciliation
-- fills in `settled_at`, `settlement_outcome`, and
-- `paper_pnl_cents` once the underlying market resolves.
--
-- Idempotency: one paper trade per (strategy, ticker, side,
-- settlement_date). The recorder upserts; replays are a no-op.

CREATE TABLE IF NOT EXISTS paper_trades (
    id BIGSERIAL PRIMARY KEY,
    strategy TEXT NOT NULL,
    ticker TEXT NOT NULL,
    side TEXT NOT NULL CHECK (side IN ('yes', 'no')),
    qty INT NOT NULL DEFAULT 1 CHECK (qty > 0),

    entered_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    entry_price_cents INT NOT NULL CHECK (entry_price_cents BETWEEN 1 AND 99),

    model_p DOUBLE PRECISION NOT NULL CHECK (model_p BETWEEN 0 AND 1),
    raw_p DOUBLE PRECISION NOT NULL CHECK (raw_p BETWEEN 0 AND 1),
    min_edge_cents INT NOT NULL,
    edge_at_entry_cents INT NOT NULL,
    fee_cents INT NOT NULL CHECK (fee_cents >= 0),

    settlement_date DATE NOT NULL,

    settled_at TIMESTAMPTZ,
    settlement_outcome DOUBLE PRECISION CHECK (
        settlement_outcome IS NULL
        OR (settlement_outcome >= 0 AND settlement_outcome <= 1)
    ),
    paper_pnl_cents INT,

    source TEXT NOT NULL,
    category TEXT,
    detail JSONB
);

CREATE UNIQUE INDEX IF NOT EXISTS paper_trades_unique_idx
    ON paper_trades (strategy, ticker, side, settlement_date);
CREATE INDEX IF NOT EXISTS paper_trades_unsettled_idx
    ON paper_trades (settlement_date) WHERE settled_at IS NULL;
CREATE INDEX IF NOT EXISTS paper_trades_strategy_settlement_idx
    ON paper_trades (strategy, settlement_date);
CREATE INDEX IF NOT EXISTS paper_trades_settled_at_idx
    ON paper_trades (strategy, settled_at) WHERE settled_at IS NOT NULL;
