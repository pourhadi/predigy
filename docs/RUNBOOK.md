# Operational runbook

> Day-to-day commands for running, debugging, and intervening
> with the predigy stack on macOS.

## Health checks

### Is everything running?

```sh
launchctl list | grep predigy
```

Expected (post-2026-05-08 ops cleanup):

```
<pid>  0  com.predigy.engine
<pid>  0  com.predigy.dashboard
<pid>  0  com.predigy.cross-arb-curate     # only when cron-firing
   -   0  com.predigy.{stat,wx,wx-stat}-curate
```

`com.predigy.import` and the legacy trader jobs
`{latency-trader,stat-trader,settlement,cross-arb}` should be absent and
persistently disabled:

```sh
launchctl print-disabled "gui/$(id -u)" | grep 'com.predigy'
```

The `<pid>  0` rows are running; the `-  0` rows are scheduled cron
tasks waiting for next fire. Anything with a non-zero second column
is in error.

### Engine status

```sh
# Liveness:
launchctl print "gui/$(id -u)/com.predigy.engine" | grep -E '^\s*state\s*='

# Mode:
grep "oms ready" ~/Library/Logs/predigy/engine.stderr.log | tail -1

# Recent activity:
tail -n 50 ~/Library/Logs/predigy/engine.stderr.log
```

### Dashboard

```sh
open http://localhost:8080            # local
open http://192.168.1.217:8080        # LAN (your phone)
```

Top-level pill: green = engine fresh, warn = stale, bad = down.

### Postgres

```sh
psql -d predigy -c "SELECT status, COUNT(*) FROM intents GROUP BY status;"
psql -d predigy -c "SELECT strategy, COUNT(*) FROM positions WHERE closed_at IS NULL GROUP BY strategy;"
psql -d predigy -c "SELECT strategy, COUNT(*) FROM rules WHERE enabled = true GROUP BY strategy;"
```

## Kill switch (panic button)

```sh
echo armed > ~/.config/predigy/kill-switch.flag   # ARM
: > ~/.config/predigy/kill-switch.flag            # DISARM (truncate)
```

Engine + dashboard poll every 5s. Within 5s of arming, the engine
logs "kill-switch: ARMED" and refuses new submits. Existing
positions are NOT auto-flattened.

The dashboard's "kill switch" card on the web UI is wired to the
same flag file via `POST /api/kill`.

Security note: if the dashboard is bound to `0.0.0.0`, `/api/kill`
is reachable from the LAN/Tailscale path unless protected externally.
Treat it as a trading-control endpoint, not a read-only dashboard.

## Redeploy

```sh
cd ~/code/predigy
cargo build --release -p predigy-engine

# Pick up the new binary (KeepAlive restarts the process):
launchctl kickstart -k "gui/$(id -u)/com.predigy.engine"

# Tail the new boot:
tail -f ~/Library/Logs/predigy/engine.stderr.log
```

If the plist itself changed (`deploy/macos/com.predigy.engine.plist`):

```sh
cp deploy/macos/com.predigy.engine.plist ~/Library/LaunchAgents/
launchctl bootout "gui/$(id -u)/com.predigy.engine"
launchctl bootstrap "gui/$(id -u)" ~/Library/LaunchAgents/com.predigy.engine.plist
```

## Debugging

### "Engine is up but not firing"

1. Check kill switch: `cat ~/.config/predigy/kill-switch.flag` —
   non-empty = armed.
2. Check rules count. `stat` currently has 0 enabled rules by design
   pending calibration; `wx-stat` consumes its JSON rule file directly.
3. Check WS connection: `grep "router: subscribe submitted"
   ~/Library/Logs/predigy/engine.stderr.log | tail -1`.
4. Check that markets are open and have books. Overnight markets
   are commonly paused; logs will show
   `409 trading_is_paused`.

### "Engine is rejecting every submit at the venue"

Likely a wire-shape drift. Look at the rejection body:

```sh
grep "rejected by venue" ~/Library/Logs/predigy/engine.stderr.log | tail -5
```

If `400 invalid_parameters` and the cid contains `.`, the
period-strip in `engine_core::cid_safe_ticker` regressed. (This was
the 2026-05-07 cutover bug.)

### "Exits are not getting out"

This is a critical live-trading condition. Check whether TP/SL/force-flat
intents are being rejected by risk caps:

```sh
grep -E "emitting exit|force-flat|oms: rejected" ~/Library/Logs/predigy/engine.stderr.log | tail -80
psql -d predigy -c "SELECT strategy, client_id, action, status, reason, submitted_at FROM intents WHERE client_id LIKE '%-exit:%' OR client_id LIKE '%-flat:%' ORDER BY submitted_at DESC LIMIT 30;"
```

Current behavior: exits/reductions project signed exposure and should not be
blocked by entry caps. If logs show `notional cap` or `contract cap`
rejections for exit intents, treat it as a regression.

### "Positions diverged from venue"

The engine now runs periodic REST reconciliation. It applies missed fills and
terminal order states, detects unmanaged venue orders, and logs position
mismatches as `oms: reconciliation found drift`. It does not auto-flatten or
invent DB positions for manual venue state.

Immediate checks:

```sh
psql -d predigy -c "SELECT strategy, ticker, side, current_qty, avg_entry_cents, fees_paid_cents FROM positions WHERE closed_at IS NULL ORDER BY strategy, ticker;"
psql -d predigy -c "SELECT strategy, status, COUNT(*) FROM intents GROUP BY strategy, status ORDER BY strategy, status;"
tail -n 120 ~/Library/Logs/predigy/engine.stderr.log
```

If venue state disagrees with Postgres, keep the kill switch armed until you
understand whether the drift is legacy/manual exposure or a current OMS bug.
For a quick aggregate venue-vs-DB check, compare dashboard `/api/state`
non-zero `open_positions[].contracts` with:

```sh
psql -d predigy -c "SELECT ticker, SUM(CASE WHEN side='yes' THEN current_qty ELSE -current_qty END) AS db_qty FROM positions WHERE closed_at IS NULL GROUP BY ticker ORDER BY ticker;"
```

The 2026-05-08 cleanup manually aligned stale force-flatten DB rows to
Kalshi `/portfolio/positions`; repeat that only after a fresh DB backup.

### "Dashboard or engine is producing many Kalshi 429s"

The 2026-05-07 audit found two sources of REST pressure: false per-market
orderbook resnapshots in the engine and dashboard mark refresh bursts.
Recent code changes target both sources, but keep watching for regression:

```sh
grep "resnapshot via REST" ~/Library/Logs/predigy/engine.stderr.log | tail -20
grep "kalshi 429" ~/Library/Logs/predigy/dashboard.stderr.log | tail -20
```

Any recurrence means the engine/dashboard is spending rate budget that
should be reserved for order entry and reconciliation.

### "Cross-arb isn't firing"

1. `PREDIGY_CROSS_ARB_PAIR_FILE` set in `~/.zprofile`?
   Engine logs "PREDIGY_CROSS_ARB_PAIR_FILE not set..." at boot
   if missing.
2. Pair file present? `cat ~/.config/predigy/cross-arb-pairs.txt`.
3. Polymarket WS connected?
   `grep "Polymarket dispatcher started" ~/Library/Logs/predigy/engine.stderr.log`.
4. Curator running? `launchctl list | grep cross-arb-curate`.

### "Latency isn't firing"

1. `PREDIGY_LATENCY_RULE_FILE` set? Engine logs "rules loaded" or
   "rule file unreadable" at boot.
2. `PREDIGY_NWS_USER_AGENT` set? Without it, engine logs
   "PREDIGY_NWS_USER_AGENT not set — NWS-dependent strategies
   won't fire this run".

### "Wx-stat isn't firing"

1. `PREDIGY_WX_STAT_RULE_FILE` set in `~/.zprofile`? If unset,
   the engine skips registering the strategy entirely (no log
   noise but no fires either). Confirm with
   `launchctl getenv PREDIGY_WX_STAT_RULE_FILE`.
2. Curator output present? `ls -la $PREDIGY_WX_STAT_RULE_FILE`
   and `jq length $PREDIGY_WX_STAT_RULE_FILE`.
3. Curator running? `launchctl list | grep wx-stat-curate`.
4. Engine reload happening? `grep "wx-stat: rules reloaded"
   ~/Library/Logs/predigy/engine.stderr.log` should fire on each
   curator output update.
5. Any wx-stat positions opening? `psql -d predigy -c "SELECT
   ticker, side, current_qty, avg_entry_cents FROM positions
   WHERE strategy = 'wx-stat' AND current_qty != 0;"`

### "Wx-stat/stat traded a stale same-day weather threshold"

Treat this as a safety incident and keep `stat`/`wx-stat` kill switches
armed until the rule source is clean.

Checks:

```sh
grep "observed:" ~/Library/Logs/predigy/wx-stat-curate.stderr.log | tail -40
jq '.[] | select(.kalshi_market | contains("KXHIGH") or contains("KXLOW"))' ~/.config/predigy/wx-stat-rules.json | head
psql -d predigy -c "SELECT ticker, side, model_p, source, fitted_at FROM rules WHERE enabled = true AND strategy = 'stat' AND ticker LIKE 'KXHIGH%' ORDER BY fitted_at DESC LIMIT 20;"
```

Required behavior: same-day/past daily-temperature markets require ASOS
observed extremes over the airport-local Kalshi settlement day. If
observations are unavailable, the curator skips the market instead of falling
back to forecast/NBM scoring. Current local-day observation pulls must bypass
the ASOS cache; past local days can use cache. Once the observed daily high/low
crosses a threshold, the emitted rule must be forced to the
observed-deterministic side.

The consolidated engine consumes `wx-stat-rules.json` directly under the
`wx-stat` strategy. `predigy-import` is currently disabled; if it is ever
re-enabled, it must not import that file as `stat` rules. Verify after any
import refresh:

```sh
psql -d predigy -c "SELECT COUNT(*) FROM rules WHERE strategy = 'stat' AND source = 'import:/Users/dan/.config/predigy/wx-stat-rules.json' AND enabled = true;"
```

Also verify NBM aggregation direction. Daily-high `greater` and daily-low
`less` are any-hour events. Daily-high `less` and daily-low `greater` are
all-hours events and must use the constraining hour. A regression here
caused PHX below-98 for 2026-05-08 to buy YES using a cool evening hour
even though the high forecast was ~101°F.

## Engine modes (Live ↔ Shadow)

```sh
# Switch to Shadow (engine writes intents at status='shadow', no venue):
sed -i.bak 's/^export PREDIGY_ENGINE_MODE=.*/export PREDIGY_ENGINE_MODE=shadow/' ~/.zprofile
launchctl kickstart -k "gui/$(id -u)/com.predigy.engine"

# Back to Live:
sed -i.bak 's/^export PREDIGY_ENGINE_MODE=.*/export PREDIGY_ENGINE_MODE=live/' ~/.zprofile
launchctl kickstart -k "gui/$(id -u)/com.predigy.engine"
```

Shadow mode is the default if the env var is absent. Use Shadow if
you suspect an engine bug; the strategy keeps recording intents to
Postgres for forensic analysis without spending real money.

## Manual position management

Legacy daemons are disabled. The engine's Postgres `positions` table should
match aggregate Kalshi venue exposure. If you need to flatten a residual
venue position manually:

```sh
# List Kalshi-account-side positions:
psql -d predigy -c "SELECT * FROM positions WHERE closed_at IS NULL;"

# Or via the dashboard at /api/state — "open_positions" field.
```

To flatten a position, submit a manual order via the Kalshi web UI
or write a one-shot script that uses `predigy-kalshi-rest`.

## Rolling back to legacy daemons

The legacy plists are still on disk but are disabled with launchctl. To
revert (one cycle only):

```sh
# 1. Stop engine
launchctl bootout "gui/$(id -u)/com.predigy.engine"

# 2. Re-enable + bootstrap legacy traders
for n in latency-trader stat-trader settlement cross-arb; do
    launchctl enable "gui/$(id -u)/com.predigy.$n"
    launchctl bootstrap "gui/$(id -u)" \
        ~/Library/LaunchAgents/com.predigy.$n.plist
done
```

The legacy daemons still maintain their JSON state files in
`~/.config/predigy/oms-state-*.json`; they pick up where they left
off.

## Database

See [`DATABASE.md`](./DATABASE.md) for setup. To run integration
tests:

```sh
createdb predigy_test 2>/dev/null || true
psql -d predigy_test -f migrations/0001_initial.sql
cargo test -p predigy-engine
```

## Logs

```sh
tail -f ~/Library/Logs/predigy/engine.stderr.log         # the trader
tail -f ~/Library/Logs/predigy/dashboard.stderr.log
tail -f ~/Library/Logs/predigy/cross-arb-curate.stderr.log
tail -f ~/Library/Logs/predigy/wx-curate.stderr.log
tail -f ~/Library/Logs/predigy/stat-curate.stderr.log
tail -f ~/Library/Logs/predigy/wx-stat-curate.stderr.log
tail -f ~/Library/Logs/predigy/import.stderr.log          # currently disabled
```

The engine's log is structured `tracing` output; filter with grep
on field names: `kill-switch:`, `oms:`, `venue_rest:`, `exec_data:`,
`router:`, `discovery:`, `external_feeds:`, `cross-strategy:`.
