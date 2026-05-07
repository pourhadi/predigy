# Operational runbook

> Day-to-day commands for running, debugging, and intervening
> with the predigy stack on macOS.

## Health checks

### Is everything running?

```sh
launchctl list | grep predigy
```

Expected (post-cutover):

```
<pid>  0  com.predigy.engine
<pid>  0  com.predigy.dashboard
<pid>  0  com.predigy.cross-arb-curate     # only when cron-firing
   -   0  com.predigy.{stat,wx,wx-stat}-curate
   -   0  com.predigy.import
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
2. Check rules count: stat needs `rules.enabled = true` rows;
   curators populate them.
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

The engine doesn't manage positions held by the legacy daemons (those
positions live in the legacy JSON state files, not Postgres).
If you need to flatten a legacy position manually:

```sh
# List Kalshi-account-side positions:
psql -d predigy -c "SELECT * FROM positions WHERE closed_at IS NULL;"

# Or via the dashboard at /api/state — "open_positions" field.
```

To flatten a position, submit a manual order via the Kalshi web UI
or write a one-shot script that uses `predigy-kalshi-rest`.

## Rolling back to legacy daemons

The legacy plists are still on disk. To revert (one cycle only):

```sh
# 1. Stop engine
launchctl bootout "gui/$(id -u)/com.predigy.engine"

# 2. Re-bootstrap legacy traders
for n in latency-trader stat-trader settlement cross-arb; do
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
tail -f ~/Library/Logs/predigy/import.stderr.log
```

The engine's log is structured `tracing` output; filter with grep
on field names: `kill-switch:`, `oms:`, `venue_rest:`, `exec_data:`,
`router:`, `discovery:`, `external_feeds:`, `cross-strategy:`.
