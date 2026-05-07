# Engine cutover runbook (Phase 5 → live)

This is the playbook for migrating from the per-strategy legacy
daemons (stat-trader, settlement-trader, latency-trader,
cross-arb-trader) to the consolidated `predigy-engine` binary.

The engine has been built phase by phase (see `ARCHITECTURE.md`).
Phase 5 ports all four hot-path strategies as engine modules.
Phase 5 ends and Phase 6 begins after cutover.

## Why dual-write first

The engine ports are line-for-line preservations of legacy
strategy logic, but ports always have subtle differences. Cutting
straight from "legacy is sole trader" to "engine is sole trader"
risks discovering the port has a bug after we've already turned
the legacy daemon off. Dual-write resolves that:

- Engine runs in `EngineMode::Shadow` (default).
- Engine writes every intent it would have submitted to
  Postgres at `status='shadow'` but does NOT touch Kalshi.
- Legacy daemons keep submitting orders.
- Operator compares the two ledgers in the `intents` table.
- If the engine's shadow rows match the legacy daemon's actual
  fires, the port is verified. Flip to live.

## Prerequisites

1. **Postgres `predigy` DB** is up and `migrations/0001_initial.sql`
   has been applied. Verify:
   ```bash
   psql -d predigy -c '\dt'   # should list intents, fills, positions, ...
   ```
2. **predigy-import** is running on its launchd schedule. This
   mirrors the legacy daemons' JSON state files into Postgres
   so engine-shadow data and legacy data coexist in one place
   for comparison.
3. **predigy-engine** binary is built:
   ```bash
   (cd ~/code/predigy && cargo build --release -p predigy-engine)
   ```

## Step 1 — Deploy engine in shadow mode

```bash
# Add to ~/.zprofile if not already set:
#   export DATABASE_URL=postgresql:///predigy
#   export PREDIGY_ENGINE_MODE=shadow   # explicit, even though shadow is the default
#
# Optional, gates which strategies run inside the engine:
#   export PREDIGY_NWS_USER_AGENT='(predigy, dan@pourhadi.com)'
#   export PREDIGY_NWS_STATES='TX,OK,CO'
#   export PREDIGY_LATENCY_RULE_FILE="$HOME/.config/predigy/latency-rules.json"
#   export PREDIGY_CROSS_ARB_PAIR_FILE="$HOME/.config/predigy/cross-arb-pairs.txt"

cd ~/code/predigy
./deploy/scripts/install-launchd.sh
```

The engine boots and starts persisting shadow rows. Verify:

```bash
launchctl print "gui/$(id -u)/com.predigy.engine" | head -20
tail -f ~/Library/Logs/predigy/engine.stderr.log
```

You should see logs like:

```
predigy-engine: oms ready  mode=Shadow
predigy-engine: strategy supervisor spawned + registered with router
  strategy="stat" n_markets=12 n_discovery_subs=0
  strategy="settlement" n_markets=0 n_discovery_subs=1
  strategy="latency" n_markets=0 n_discovery_subs=0
  strategy="cross-arb" n_markets=0 n_discovery_subs=0
predigy-engine: ready (running); awaiting shutdown signal
```

## Step 2 — Watch dual-write for ≥24 hours

You want at least one full Kalshi trading session of overlap.
Sports settlements in particular cluster in evening hours; if
you only watched a 4-hour midday window you'd miss whether
settlement is firing.

Quick health checks during the window:

```sql
-- All-strategy fire counts, last hour, split by status.
SELECT strategy,
       SUM(CASE WHEN status = 'shadow' THEN 1 ELSE 0 END) AS engine_shadow,
       SUM(CASE WHEN status != 'shadow' THEN 1 ELSE 0 END) AS legacy_total
  FROM intents
 WHERE submitted_at >= now() - interval '1 hour'
 GROUP BY strategy
 ORDER BY strategy;
```

If `engine_shadow` is 0 across all strategies, something's
wrong with the engine's wiring — check the supervisors loaded
their rule sets, the discovery service connected, etc.

## Step 3 — Run the parity diff (per strategy)

**Note on `client_id`:** the legacy daemons allocate
monotonically-increasing sequence cids (`stat:KX-FOO:00000042`)
via the `CidAllocator` in `crates/oms/src/cid.rs`. The engine
ports use deterministic content-hash cids
(`stat:KX-FOO:42:0001:abc12345`) so the same fire condition
collapses idempotently in the OMS. These schemes do NOT match
on equality, so parity here is by **content** (ticker, side,
action, price, qty) within a small time window, not by
`client_id`.

Re-run this for each strategy you're cutting over:

```sql
-- Set this to the strategy you're verifying.
\set strategy 'stat'

-- 1. Total fire counts in the last 24h.
SELECT
    SUM(CASE WHEN status = 'shadow' THEN 1 ELSE 0 END) AS engine_fires,
    SUM(CASE WHEN status != 'shadow' THEN 1 ELSE 0 END) AS legacy_fires
  FROM intents
 WHERE strategy = :'strategy'
   AND submitted_at >= now() - interval '24 hours';

-- 2. Engine-only fires: engine signalled an intent that has no
--    matching legacy fire on the same (ticker, side, action,
--    price, qty) within a ±2-minute window.
WITH engine AS (
    SELECT client_id, ticker, side, action, price_cents, qty, submitted_at
      FROM intents
     WHERE strategy = :'strategy'
       AND status = 'shadow'
       AND submitted_at >= now() - interval '24 hours'
), legacy AS (
    SELECT ticker, side, action, price_cents, qty, submitted_at
      FROM intents
     WHERE strategy = :'strategy'
       AND status != 'shadow'
       AND submitted_at >= now() - interval '24 hours'
)
SELECT e.client_id, e.ticker, e.side, e.action, e.price_cents, e.qty,
       e.submitted_at AS engine_ts
  FROM engine e
 WHERE NOT EXISTS (
    SELECT 1 FROM legacy l
     WHERE l.ticker = e.ticker
       AND l.side = e.side
       AND l.action = e.action
       AND l.price_cents IS NOT DISTINCT FROM e.price_cents
       AND l.qty = e.qty
       AND ABS(EXTRACT(EPOCH FROM (l.submitted_at - e.submitted_at))) <= 120
 )
 ORDER BY e.submitted_at DESC
 LIMIT 50;

-- 3. Legacy-only fires: legacy fired but engine didn't.
--    Symmetric query.
WITH engine AS (
    SELECT ticker, side, action, price_cents, qty, submitted_at
      FROM intents
     WHERE strategy = :'strategy'
       AND status = 'shadow'
       AND submitted_at >= now() - interval '24 hours'
), legacy AS (
    SELECT client_id, ticker, side, action, price_cents, qty, submitted_at
      FROM intents
     WHERE strategy = :'strategy'
       AND status != 'shadow'
       AND submitted_at >= now() - interval '24 hours'
)
SELECT l.client_id, l.ticker, l.side, l.action, l.price_cents, l.qty,
       l.submitted_at AS legacy_ts
  FROM legacy l
 WHERE NOT EXISTS (
    SELECT 1 FROM engine e
     WHERE e.ticker = l.ticker
       AND e.side = l.side
       AND e.action = l.action
       AND e.price_cents IS NOT DISTINCT FROM l.price_cents
       AND e.qty = l.qty
       AND ABS(EXTRACT(EPOCH FROM (e.submitted_at - l.submitted_at))) <= 120
 )
 ORDER BY l.submitted_at DESC
 LIMIT 50;

-- 4. Per-(ticker, side) fire counts on each side. Quick sanity:
--    are the totals roughly the same across the same markets?
SELECT ticker, side,
       SUM(CASE WHEN status = 'shadow' THEN 1 ELSE 0 END)  AS engine_fires,
       SUM(CASE WHEN status != 'shadow' THEN 1 ELSE 0 END) AS legacy_fires
  FROM intents
 WHERE strategy = :'strategy'
   AND submitted_at >= now() - interval '24 hours'
 GROUP BY ticker, side
HAVING SUM(CASE WHEN status = 'shadow' THEN 1 ELSE 0 END)
       != SUM(CASE WHEN status != 'shadow' THEN 1 ELSE 0 END)
 ORDER BY ticker, side;
```

**Pass criteria for that strategy:**
- Both engine-only and legacy-only sets are small (<5% of total
  fires) and every entry is explainable by clock skew at the
  ±2-minute boundary or known intentional differences.
- Per-(ticker, side) totals broadly agree (same magnitude;
  exact equality not required given the time window).

## Step 4 — Flip one strategy at a time to live

Cutting all four at once is unnecessary risk. Sequence:
**stat → settlement → latency → cross-arb** (most-validated to
least; stat has the longest production history, cross-arb the
shortest).

For each strategy:

1. **Disable the legacy daemon for that strategy** (NOT the
   engine):
   ```bash
   # Example: stat
   launchctl bootout "gui/$(id -u)/com.predigy.stat-trader"
   ```
2. **Flip the engine to live:**
   ```bash
   # Edit ~/.zprofile:
   #   export PREDIGY_ENGINE_MODE=live
   #
   # Then kick the engine to pick it up:
   launchctl kickstart -k "gui/$(id -u)/com.predigy.engine"
   ```
3. **Watch fills land in Postgres** (engine + venue both write
   to `intents` + `fills` now):
   ```sql
   SELECT * FROM fills
    WHERE strategy = 'stat'
      AND ts >= now() - interval '15 minutes'
    ORDER BY ts DESC;
   ```
4. **Re-arm the kill-switch flag-file path**: the dashboard's
   emergency-stop button writes `~/.config/predigy/kill-switch.flag`
   which the engine ALSO polls. Both legacy daemons (still
   running for the other 3 strategies) and the engine respect
   the same flag. Tested end-to-end: arming the dashboard kill
   switch should pause new entries on every running trader.

After verifying ≥1h of live engine fires for that strategy
match expected behavior, repeat for the next strategy.

## Step 5 — Retire the legacy daemons

Once all four strategies are live in the engine and have been
running for ≥1 week without divergence, retire the legacy
binaries:

```bash
# Remove the launchd plists (engine keeps running):
for name in stat-trader latency-trader settlement com.predigy.cross-arb; do
    launchctl bootout "gui/$(id -u)/com.predigy.${name}" 2>/dev/null || true
    rm -f "$HOME/Library/LaunchAgents/com.predigy.${name}.plist"
done
```

Crates / binaries can stay in the workspace for some time as a
fallback. They're not load-bearing once the engine is the sole
trader.

## Rollback plan

If the engine starts misbehaving in live mode:

1. **Immediate**: arm the dashboard's kill switch — both engine
   and any still-running legacy daemons stop submitting new
   intents within 2-5s.
2. **Stop the engine**: `launchctl bootout "gui/$(id -u)/com.predigy.engine"`.
3. **Re-enable the legacy daemon for the affected strategy**:
   ```bash
   launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.predigy.<name>.plist"
   ```
4. **File a bug**: the engine's behavior is in Postgres — query
   `intents`, `fills`, `intent_events` for the time window to
   reconstruct what happened.

The legacy state files in `~/.config/predigy/oms-state-*.json`
are not deleted by the cutover, so the legacy daemons retain
their idempotency state if you have to bring them back up.
