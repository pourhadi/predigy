-- Migration 0002: leg-group atomic multi-leg submit (Audit I7)
--
-- Adds an optional `leg_group_id` to the `intents` table. Single-
-- leg submits leave it NULL (current behavior, fully backwards
-- compatible). Multi-leg submits go through `Oms::submit_group`,
-- which generates a fresh UUID and tags every member intent with
-- it inside one transaction — either all rows commit or none do.
--
-- Why a column rather than a join table:
-- - Group membership is immutable post-submit (legs cannot move
--   between groups), so the cardinality is 1:N (group → intents)
--   and a single foreign key on the child table captures it.
-- - Querying "all sibling legs of intent X" stays single-table
--   with a self-join, no additional index lookup chain.
-- - The leg-group UUID is also the operator-readable handle in
--   logs and the dashboard's `leg-group:<uuid>` filter.
--
-- Cascade semantics live in the OMS, not the database — when a
-- venue rejection comes in for any leg in a group, the OMS marks
-- sibling legs `cancel_requested`. The DB only stores the
-- relationship.

ALTER TABLE intents
    ADD COLUMN IF NOT EXISTS leg_group_id UUID NULL;

CREATE INDEX IF NOT EXISTS intents_leg_group_idx
    ON intents(leg_group_id)
    WHERE leg_group_id IS NOT NULL;
