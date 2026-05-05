# Deploying the weather strategy on macOS

Production-ready setup for running the NWS-driven `latency-trader`
strategy 24/7 on macOS using launchd.

## What runs

Two launchd jobs:

1. **`com.predigy.latency-trader`** — long-running daemon. Subscribes
   to NWS, evaluates each alert against the curated rule file,
   submits IOC orders to Kalshi when a rule fires. Persists OMS
   state, cid sequence, and the NWS seen-id set so a restart
   resumes cleanly.
2. **`com.predigy.wx-curate`** — daily 06:30 cron (via
   `StartCalendarInterval`). Re-curates `wx-rules.json` against
   current Kalshi weather markets via Claude, then kickstarts the
   trader so it picks up fresh rules.

## Persistence layout

Everything lives under `~/.config/predigy/`:

| File | Owner | Purpose |
|---|---|---|
| `kalshi.pem` | operator | Kalshi RSA private key |
| `wx-rules.json` | wx-curator | Rule file (atomic-rename per run) |
| `oms-cids` | OMS | Cid sequence counter, chunk pre-allocated |
| `oms-state.json` | OMS | Positions, daily P&L, kill-switch, in-flight orders |
| `wx-seen.json` | NWS poller | Alert ids already processed (prevents re-fire on restart) |

Logs go to `~/Library/Logs/predigy/{latency-trader,wx-curate}.{stdout,stderr}.log`.

## Required env (in `~/.zprofile`)

The launchd agents run via `zsh -lc` so `~/.zprofile` is sourced.

```sh
export KALSHI_KEY_ID="..."             # Kalshi API key id (UUID-ish)
export ANTHROPIC_API_KEY="sk-ant-..."  # for wx-curator
export NWS_USER_AGENT="(predigy, you@example.com)"
# Optional — defaults shown:
# export KALSHI_PEM="$HOME/.config/predigy/kalshi.pem"
# export PREDIGY_HOME="$HOME/code/predigy"
# export PREDIGY_LIVE=1                  # flip from default dry-run to real submission
# export PREDIGY_NWS_STATES="TX FL CA"   # comma-or-space states (defaults to states from wx-rules.json)
# export PREDIGY_MAX_ACCOUNT_NOTIONAL=500  # in cents — $5 cap
# export PREDIGY_MAX_DAILY_LOSS=200        # in cents — $2 daily-loss breaker
```

## Install

```sh
# Build release binaries first
cd ~/code/predigy
cargo build --release -p wx-curator -p latency-trader

# Install + load both launchd jobs
./deploy/scripts/install-launchd.sh
```

The installer does preflight checks: env vars present, PEM in place,
binaries built. Refuses to install if any check fails.

## First-time bootstrap

The trader needs a rule file before it can do anything. Run the
curator once manually so it's not waiting on the 06:30 cron:

```sh
launchctl kickstart -k gui/$(id -u)/com.predigy.wx-curate
```

Watch the rule file get written:

```sh
tail -f ~/Library/Logs/predigy/wx-curate.stderr.log
```

## Going live

The default is dry-run (logs `rule fired` but doesn't submit).
After watching for a session and confirming the strategy fires
sensibly:

```sh
echo 'export PREDIGY_LIVE=1' >> ~/.zprofile
launchctl kickstart -k gui/$(id -u)/com.predigy.latency-trader
```

## Operational commands

```sh
# Tail live logs
tail -f ~/Library/Logs/predigy/latency-trader.stderr.log

# Force a rule re-curate now (without waiting for 06:30)
launchctl kickstart -k gui/$(id -u)/com.predigy.wx-curate

# Stop the trader (e.g., before manual investigation)
launchctl bootout gui/$(id -u)/com.predigy.latency-trader

# Bring it back
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.predigy.latency-trader.plist

# Verify a job is loaded
launchctl print gui/$(id -u)/com.predigy.latency-trader | head -20

# Inspect persisted state
cat ~/.config/predigy/oms-state.json | python3 -m json.tool | head -40
```

## Restart correctness

The setup tolerates restarts at any point:

| State | How it survives |
|---|---|
| Cid sequence | `--cid-store ~/.config/predigy/oms-cids`, chunk-allocated, atomic write per chunk |
| Positions / daily P&L / kill-switch | `--oms-state ~/.config/predigy/oms-state.json`, atomic-rename JSON snapshot per mutation |
| In-flight orders | Same OMS state file |
| NWS seen-id set | `--nws-seen ~/.config/predigy/wx-seen.json`, atomic-rename per poll |
| Strategy `armed` flag (per-rule "fired this session") | Intentionally not persisted — a restart re-arms each rule for a fresh day, but `wx-seen.json` prevents the same alert from re-firing it |

## Stopping the world

If you need to halt all trading immediately:

```sh
launchctl bootout gui/$(id -u)/com.predigy.latency-trader
```

This sends SIGTERM; the OMS persists final state on its way out.
The Kalshi venue keeps any resting orders alive, so manually
inspect / cancel via `kalshi.com/portfolio` if you want them gone.

## Cost expectations

- **Anthropic**: ~$0.40/day for the curator (one full Kalshi-wide
  scan, ~38K input tokens + ~16K output via Sonnet 4.6). Capped by
  `--max-batches 15` in the wrapper script.
- **Kalshi fees**: 1¢ minimum per fill. Strategy default is 1
  contract per fire; expect 0–10 fires/day depending on weather.
- **NWS**: free; we poll at 30 s (well above NWS's 15 s minimum).

## Linux/systemd

Not yet shipped. Same architecture (one daemon + one timer), just
unit files instead of plists. Open as a follow-up when migrating to
a us-east-1 VPS for latency.
