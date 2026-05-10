-- Add post_only flag to intents.
--
-- Kalshi's CreateOrderRequest natively supports a post_only flag —
-- the order is rejected if it would cross the book at submit time.
-- This is critical for the book-maker strategy: a maker that
-- accidentally takes pays the full taker fee, which defeats the
-- entire economic case for maker mode.
--
-- All existing strategies emit IOC and don't care about post_only;
-- they pass through with the default `false`. The book-maker
-- strategy sets `post_only=true` on every Intent so its quotes
-- can never cross.
--
-- See `plans/2026-05-10-strategic-roadmap.md` part 3 (infra-2).

ALTER TABLE intents
    ADD COLUMN IF NOT EXISTS post_only BOOLEAN NOT NULL DEFAULT FALSE;

COMMENT ON COLUMN intents.post_only IS
    'When true, venue rejects the order if it would cross the book at submit. Required for maker-mode quoting.';
