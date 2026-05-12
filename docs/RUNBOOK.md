# Operational runbook

> Day-to-day commands for running, debugging, and intervening
> with the predigy stack.
>
> **Production host as of 2026-05-12 is the Pi at `nas.local`**
> (`dan@192.168.1.35`) — see [`DEPLOY.md`](./DEPLOY.md) for the
> deploy layout and cutover/rollback runbooks. The Linux commands
> below are the canonical ones; the macOS commands underneath each
> section are the rollback path only.

## Health checks

### Is everything running?

**Pi (production):**

```sh
ssh dan@nas.local 'systemctl --user list-units "predigy-*"'
ssh dan@nas.local 'systemctl --user list-timers "predigy-*"'
```

Expected: `predigy-engine.service` + `predigy-dashboard.service`
`active (running)`; eight timers (`*-curate`, `calibration`,
`paper-trader`, `opportunity-scanner`, `eval-daily`, `db-backup`)
`active (waiting)`. Any `failed` state is an error — inspect with
`systemctl --user status predigy-<unit>` or `journalctl --user -u
predigy-<unit>`.

**Laptop (rollback path — services should currently be inactive):**

```sh
launchctl list | grep predigy
```

If a row shows a non-zero second column, that's an error. Legacy
trader jobs `{latency-trader,stat-trader,settlement,cross-arb}`
plus `import` should be absent and persistently disabled:

```sh
launchctl print-disabled "gui/$(id -u)" | grep 'com.predigy'
```

### Engine status

**Pi:**

```sh
# Liveness:
ssh dan@nas.local 'systemctl --user is-active predigy-engine.service'

# Mode (Live vs Shadow):
ssh dan@nas.local 'grep "oms ready" ~/.local/state/predigy/logs/engine.stderr.log | tail -1'

# Recent activity:
ssh dan@nas.local 'tail -n 50 ~/.local/state/predigy/logs/engine.stderr.log'
# or via journald:
ssh dan@nas.local 'journalctl --user -u predigy-engine -n 50 --no-pager'
```

**Laptop (rollback only):**

```sh
launchctl print "gui/$(id -u)/com.predigy.engine" | grep -E '^\s*state\s*='
grep "oms ready" ~/Library/Logs/predigy/engine.stderr.log | tail -1
tail -n 50 ~/Library/Logs/predigy/engine.stderr.log
```

### Dashboard

```sh
open http://nas.local:8080            # production
open http://localhost:8080            # only if rolled back to laptop
```

Top-level pill: green = engine fresh, warn = stale, bad = down.

Calibration view:

```sh
open http://localhost:8080/calibration
curl -s http://localhost:8080/calibration/summary.json | jq .
```

### Postgres

```sh
psql -d predigy -c "SELECT status, COUNT(*) FROM intents GROUP BY status;"
psql -d predigy -c "SELECT strategy, COUNT(*) FROM positions WHERE closed_at IS NULL GROUP BY strategy;"
psql -d predigy -c "SELECT strategy, COUNT(*) FROM rules WHERE enabled = true GROUP BY strategy;"
psql -d predigy -c "SELECT strategy, COUNT(*), COUNT(*) FILTER (WHERE would_fire) FROM opportunity_observations WHERE ts > now() - interval '6 hours' GROUP BY strategy;"
psql -d predigy -c "SELECT strategy, window_end, n_predictions, n_settled, brier, log_loss FROM calibration_reports ORDER BY window_end DESC LIMIT 10;"
```

## Fill-growth scanner + calibration jobs

Observation-only scanner:

```sh
# Dry-run; fetches public orderbooks for configured arb pairs/families.
./target/release/opportunity-scanner arb

# Production tick: writes opportunity_observations only.
deploy/scripts/opportunity-scanner-run.sh
```

Calibration evidence:

```sh
# Backfill public settled outcomes for predicted tickers.
./target/release/predigy-calibration sync-settlements --window-days 90 --limit 200

# Dry-run settled venue-flat DB reconciliation. Requires Kalshi auth but
# never submits/cancels orders. It only writes with explicit --write.
./target/release/predigy-calibration \
  --kalshi-key-id "$KALSHI_KEY_ID" --kalshi-pem "$KALSHI_PEM" \
  reconcile-venue-flat --limit 100

# Compute/store reliability report. wx-stat reports exclude legacy
# prediction rows whose logged settlement date disagrees with the
# ticker date suffix; those rows came from the pre-fix UTC/local-day
# bug and should not be treated as current-model calibration evidence.
./target/release/predigy-calibration report --strategy stat --window-days 90
./target/release/predigy-calibration report --strategy wx-stat --window-days 90

# Fit wx-stat calibration when enough clean settled samples exist.
# Defaults are conservative: latest clean record per ticker,
# date-mismatch legacy rows excluded, min 30 samples for exact/global
# buckets, regularized monotone Platt scaling. Dry-run first.
./target/release/wx-stat-fit-calibration \
  --predictions-dir data/wx_stat_predictions \
  --asos-cache data/asos_cache \
  --user-agent "$NWS_USER_AGENT" \
  --calibration-out data/wx_stat_calibration.json \
  --dry-run

# One-time/idempotent wx-stat prediction sidecar backfill.
./target/release/wx-stat-curator \
  --kalshi-key-id "$KALSHI_KEY_ID" --kalshi-pem "$KALSHI_PEM" \
  --user-agent "$NWS_USER_AGENT" --shadow-db \
  --nbm-predictions-dir data/wx_stat_predictions \
  --backfill-predictions-only
```

`stat-curate.sh` runs `stat-curator --shadow-db`; it upserts disabled
`stat` rules and appends `model_p_snapshots`. `wx-stat-curate.sh` also runs
`wx-stat-curator --shadow-db`; live `wx-stat` still consumes the JSON rule
file directly, while Postgres gets disabled shadow rules plus
`model_p_snapshots` for calibration reporting. Neither curator should enable
DB rules. Verify after any curator/deploy change:

```sh
psql -d predigy -c "SELECT strategy, enabled, COUNT(*) FROM rules WHERE strategy IN ('stat','wx-stat') GROUP BY strategy, enabled ORDER BY strategy, enabled;"
psql -d predigy -c "SELECT strategy, COUNT(*) FROM model_p_snapshots WHERE ts > now() - interval '24 hours' GROUP BY strategy ORDER BY strategy;"
```

## Kill switch (panic button)

**Pi (production):**

```sh
ssh dan@nas.local 'echo armed > ~/.config/predigy/kill-switch.flag'   # ARM
ssh dan@nas.local ': > ~/.config/predigy/kill-switch.flag'            # DISARM (truncate)
```

**Laptop (rollback only):**

```sh
echo armed > ~/.config/predigy/kill-switch.flag   # ARM
: > ~/.config/predigy/kill-switch.flag            # DISARM
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

**Pi (production):**

```sh
ssh dan@nas.local 'cd ~/code/predigy && git pull && source ~/.cargo/env && cargo build --release -p predigy-engine'
ssh dan@nas.local 'systemctl --user restart predigy-engine.service'
ssh dan@nas.local 'tail -f ~/.local/state/predigy/logs/engine.stderr.log'
```

If a systemd unit file under `deploy/linux/systemd/` changed:

```sh
ssh dan@nas.local 'cd ~/code/predigy && git pull && bash deploy/linux/install-systemd.sh'
```

`install-systemd.sh` is idempotent — it re-copies units, runs
`daemon-reload`, and `enable --now`s the services. It does not
restart already-running services on its own; do that explicitly
if you want the new unit definition to take effect on a running
process.

**Laptop (rollback only):**

```sh
cd ~/code/predigy
cargo build --release -p predigy-engine
launchctl kickstart -k "gui/$(id -u)/com.predigy.engine"
tail -f ~/Library/Logs/predigy/engine.stderr.log
```

Scanner/calibration jobs are non-order-entry. The calibration job
runs `reconcile-venue-flat --write` after settlement sync; it can
close DB-only stale positions only when authenticated venue
exposure is flat and Kalshi market detail has a final binary
outcome. On the Pi those are `predigy-calibration.timer` (15 m)
and `predigy-opportunity-scanner.timer` (5 m) — managed by the
same `install-systemd.sh` re-run shown above.

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

For settled venue-flat stale DB rows, use the calibrated reconciliation
command instead of hand-editing positions:

```sh
# Dry-run first; prints JSON with candidate tickers and expected P&L deltas.
./target/release/predigy-calibration \
  --kalshi-key-id "$KALSHI_KEY_ID" --kalshi-pem "$KALSHI_PEM" \
  reconcile-venue-flat --limit 100

# Write only after the report shows final settled outcomes.
./target/release/predigy-calibration \
  --kalshi-key-id "$KALSHI_KEY_ID" --kalshi-pem "$KALSHI_PEM" \
  reconcile-venue-flat --limit 100 --write
```

This command never submits/cancels orders. It upserts the market/settlement
record and closes matching Postgres `positions` rows at the settled side value.

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
