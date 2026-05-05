# Operational runbook

> Day-to-day commands for running, debugging, and intervening with
> the predigy daemon stack on macOS.

## Health checks

### Is everything running?

```sh
for n in com.predigy.latency-trader com.predigy.wx-curate com.predigy.dashboard; do
    state=$(launchctl print "gui/$(id -u)/$n" 2>/dev/null | grep -E '^\s*state\s*=' | head -1 | awk -F= '{print $2}' | xargs)
    echo "$n: ${state:-NOT LOADED}"
done
```

### Dashboard, the easy way

```sh
open http://localhost:8080            # from this laptop
open http://192.168.1.217:8080        # from any device on the same wifi
```

Health pill: green = log <90s old; warn = stale; bad = down.

### Logs

```sh
tail -f ~/Library/Logs/predigy/latency-trader.stderr.log
tail -f ~/Library/Logs/predigy/wx-curate.stderr.log
tail -f ~/Library/Logs/predigy/dashboard.stderr.log
```

### Account state on the venue

```sh
KALSHI_KEY_ID="$KALSHI_KEY_ID" KALSHI_PEM="$HOME/.config/predigy/kalshi.pem" \
  cargo run -p predigy-kalshi-rest --example auth_smoke
```

Prints positions + P&L per market.

## Common interventions

### Force a wx-curate cycle right now

```sh
launchctl kickstart -k gui/$(id -u)/com.predigy.wx-curate
```

### Restart the trader (e.g. to pick up new env vars)

```sh
launchctl kickstart -k gui/$(id -u)/com.predigy.latency-trader
```

### Halt all trading immediately

```sh
launchctl bootout gui/$(id -u)/com.predigy.latency-trader
```

After halt: Kalshi keeps any resting orders alive. To cancel them
manually:

```sh
# Inspect your portfolio in a browser:
open https://kalshi.com/portfolio

# Or close one specific position via REST:
KALSHI_KEY_ID="$KALSHI_KEY_ID" KALSHI_PEM="$HOME/.config/predigy/kalshi.pem" \
  cargo run -p predigy-kalshi-rest --example close_position -- \
  KXMARKET-TICKER 7 1
# args: market price-cents qty
```

### Bring trading back

```sh
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.predigy.latency-trader.plist
```

### Flip dry-run ↔ live

```sh
# Go LIVE
echo 'export PREDIGY_LIVE=1' >> ~/.zprofile
launchctl kickstart -k gui/$(id -u)/com.predigy.latency-trader

# Go back to DRY-RUN
sed -i.bak '/^export PREDIGY_LIVE=/d' ~/.zprofile
launchctl kickstart -k gui/$(id -u)/com.predigy.latency-trader
```

### Adjust risk caps

Edit `~/.zprofile` and restart the trader:

```sh
export PREDIGY_MAX_ACCOUNT_NOTIONAL=1000   # cents — $10 cap
export PREDIGY_MAX_DAILY_LOSS=500          # cents — $5 daily loss breaker
export PREDIGY_MAX_NOTIONAL_PER_SIDE=500   # cents — $5 per market per side
export PREDIGY_MAX_CONTRACTS=5             # contracts per side
launchctl kickstart -k gui/$(id -u)/com.predigy.latency-trader
```

## Debugging recipes

### "I see a fire in the log but no fill"

In `--dry-run` mode this is expected — fires log but don't submit.
Verify with:

```sh
grep -E "PREDIGY_LIVE|dry_run" ~/Library/Logs/predigy/latency-trader.stderr.log | head -5
```

In live mode, check whether the venue accepted the submit:

```sh
grep -E "Submitted|Acked|Rejected|Filled" ~/Library/Logs/predigy/latency-trader.stderr.log | tail -20
```

### "Orphan position — venue says I own contracts but OMS thinks not"

This happened during the live shake-down due to a fill-decoder bug.
Both halves should reconcile now, but if it recurs:

```sh
# Inspect venue's view
cargo run -p predigy-kalshi-rest --example auth_smoke

# Inspect OMS's view (positions block in the JSON)
cat ~/.config/predigy/oms-state.json | python3 -m json.tool | head -30
```

If they diverge, the safer move is to flatten via venue and let
the next OMS run rehydrate from the snapshot.

### "Daemon keeps crash-looping"

`launchctl print gui/$(id -u)/com.predigy.latency-trader` shows
`last exit code` — anything non-zero indicates the wrapper script
or binary failed at startup. Check stderr log for the panic /
context.

Common causes:
- Missing env var → wrapper script fails preflight.
- Stale rule file → `wx-rules.json` doesn't exist → trader bails
  with "rule file is empty". Force a curate cycle.
- Kalshi auth failure → key rotated, PEM doesn't match. Re-pull
  the PEM from your Kalshi account.

`ThrottleInterval=30` in the plist means launchd waits 30 s between
restarts, so a true crash loop takes a minute to surface in logs.

### "Anthropic key error during wx-curate"

```
ANTHROPIC_API_KEY env var required
```

`~/.zprofile` has it but launchd's `zsh -lc` should source it.
If failing, the user might have moved it to `~/.zshrc` (which
isn't sourced by `-lc`). Move back to `~/.zprofile`.

### "Curator wrote 0 rules"

Either Anthropic returned 0 (rare — usually means prompt is too
restrictive) or the validator dropped them all. Inspect:

```sh
~/Library/Logs/predigy/wx-curate.stderr.log
```

Look for `dropped invalid rule` warnings and `rules proposed`
lines per batch. If proposed=0, edit `bin/wx-curator/src/prompt.rs`
and rebuild.

## Mobile access

Default plist binds the dashboard to `0.0.0.0:8080`. Phone access:

- **Same wifi**: visit `http://<laptop-LAN-IP>:8080`. Get the IP
  via `ipconfig getifaddr en0`.
- **Off-network**: install [Tailscale](https://tailscale.com) on
  laptop and phone (free for 3 nodes), then hit the laptop's
  Tailscale IP.

The dashboard is read-only. There is no kill-switch button — use
ssh / launchctl from a shell to halt.

## Daily / weekly maintenance

| Cadence | What | How |
|---|---|---|
| Daily 06:30 (auto) | wx-curate writes fresh rules + restarts trader | (launchd) |
| Weekly | Skim `latency-trader.stderr.log` for `rule fired` patterns | `grep "rule fired" log \| awk` |
| Weekly | Verify Kalshi balance matches OMS realized P&L sum | `auth_smoke` + `oms-state.json` |
| Monthly | Rotate the Kalshi key (good hygiene) | Kalshi dashboard → API → new key |
| Monthly | Update Rust toolchain + re-build release binaries | `rustup update` + `cargo build --release` |

## Adding a new daemon

If you build a second strategy and want to run it under launchd:

1. Build release binary: `cargo build --release -p <bin>`.
2. Create plist at `deploy/macos/com.predigy.<name>.plist` (copy
   `latency-trader.plist` as template).
3. Add to `deploy/scripts/install-launchd.sh` preflight + install
   loop.
4. Run `./deploy/scripts/install-launchd.sh` to load it.

Each daemon needs its own:
- `--strategy-id` (cid prefix; must be unique per process)
- `--cid-store` (separate file)
- `--oms-state` (separate file)
- Risk caps (separate `PREDIGY_*` env vars)
