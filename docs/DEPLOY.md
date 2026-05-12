# DEPLOY.md — how predigy is deployed today

**Production host (as of 2026-05-12):** Raspberry Pi 5 at
`nas.local` (`dan@192.168.1.35`). 8 GB RAM, Debian 13 (trixie),
PostgreSQL 17, `systemd --user` for service management. ~15 ms RTT
to Kalshi from the home ISP.

**Source of truth for the deploy:** [`deploy/linux/`](../deploy/linux/).

> Why the Pi instead of cloud: the Pi is on 24/7 already, has plenty
> of headroom for predigy's workload, and costs ~$5/yr in power.
> GCP at ~$30–50/mo would burn ≥50 % of funded capital per month
> against ~$80 of working capital. Neither host fixes the latency-
> trader strategy — that one is blocked on FIX + colocation, not
> hosting. See the PR description on the merge that landed this
> deploy for the full tradeoff table.

## Layout

```
deploy/
├── linux/
│   ├── systemd/                       # 10 .service + 8 .timer units
│   │   ├── predigy-engine.service
│   │   ├── predigy-dashboard.service
│   │   ├── predigy-stat-curate.{service,timer}
│   │   ├── predigy-cross-arb-curate.{service,timer}
│   │   ├── predigy-arb-config-curate.{service,timer}
│   │   ├── predigy-calibration.{service,timer}
│   │   ├── predigy-paper-trader.{service,timer}
│   │   ├── predigy-opportunity-scanner.{service,timer}
│   │   ├── predigy-eval-daily.{service,timer}
│   │   └── predigy-db-backup.{service,timer}
│   ├── env.example                    # env-file template
│   ├── install-systemd.sh             # bootstrap (preflight + install)
│   ├── cutover.sh                     # laptop → Pi runbook
│   └── rollback.sh                    # Pi → laptop runbook
├── macos/                             # legacy launchd plists (rollback path)
└── scripts/                           # shared wrapper scripts (Linux + macOS)
    ├── engine-run.sh
    ├── dashboard-run.sh
    ├── stat-curate.sh
    ├── cross-arb-curate.sh
    ├── arb-config-curate.sh
    ├── predigy-calibration-run.sh
    ├── predigy-paper-trader-run.sh
    ├── opportunity-scanner-run.sh
    ├── eval-daily.sh
    └── db-backup.sh
```

The wrapper scripts under `deploy/scripts/` are shared between
macOS and Linux. They honor `PREDIGY_LOG_DIR` (set by the Pi's env
file) and `PREDIGY_HOME` (defaults to `~/code/predigy`).
`engine-run.sh` and the curators read everything else from
environment variables.

## File paths on the Pi

| Path | Purpose |
|---|---|
| `~/code/predigy/` | Git clone (this repo) |
| `~/.config/predigy/env` | Loaded by every systemd unit via `EnvironmentFile=`. Holds `KALSHI_KEY_ID`, `ANTHROPIC_API_KEY`, risk caps, `PREDIGY_ENGINE_MODE`, etc. Chmod 600. |
| `~/.config/predigy/kalshi.pem` | Kalshi RSA private key. Chmod 600. |
| `~/.config/predigy/kill-switch.flag` | Non-empty arms; empty disarms. |
| `~/.config/predigy/{stat,wx-stat}-rules.json`, `cross-arb-pairs.txt`, `*-config.json` | Curator outputs, strategy configs |
| `~/.local/state/predigy/logs/` | stdout/stderr per service (`PREDIGY_LOG_DIR`) |
| `~/.local/state/predigy/backups/` | Daily `pg_dump` gzipped dumps, 30-day rotation |
| `~/.config/systemd/user/predigy-*.{service,timer}` | systemd units (copied from `deploy/linux/systemd/`) |
| `/var/lib/postgresql/17/main/` | Postgres data dir (on SD card; fast fsync) |

## First-time install on a fresh Pi

Prereqs: Debian 13, network reachable from the laptop, ssh access
as user `dan`.

```sh
# --- on the Pi ---
sudo apt-get update
sudo apt-get install -y postgresql-17 postgresql-client-17 postgresql-contrib \
    libssl-dev libsqlite3-dev libtiff-dev pkg-config build-essential \
    cmake zsh curl git
sudo timedatectl set-timezone America/Chicago     # match the laptop
sudo -u postgres createuser dan --superuser
sudo -u postgres createdb -O dan predigy
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
    sh -s -- -y --default-toolchain stable --profile minimal

# clone + build
git clone https://github.com/pourhadi/predigy ~/code/predigy
cd ~/code/predigy
for m in migrations/*.sql; do psql -d predigy -f "$m"; done
source ~/.cargo/env
cargo build --release --workspace          # ~15 min on Pi 5

# config + secrets
mkdir -p ~/.config/predigy ~/.local/state/predigy/logs ~/.local/state/predigy/backups
cp deploy/linux/env.example ~/.config/predigy/env
chmod 600 ~/.config/predigy/env
# edit ~/.config/predigy/env — fill in KALSHI_KEY_ID, ANTHROPIC_API_KEY
# scp ~/.config/predigy/kalshi.pem from the laptop, chmod 600

# install systemd units + start in shadow mode
bash deploy/linux/install-systemd.sh
```

`install-systemd.sh` preflights env vars + binaries + Postgres
reachability, copies units, enables linger (so user services
survive logout), then `systemctl --user enable --now`s
everything. It refuses to proceed if anything's missing.

## Cutover (laptop → Pi)

Run from the laptop. Each phase pauses for ENTER; Ctrl-C is safe
up to the `pg_restore` step.

```sh
deploy/linux/cutover.sh
```

Phases:
1. Preflight: ssh to Pi works, engine unit is active, env mode is
   `shadow`.
2. **Arm laptop kill switch** — laptop engine stops submitting.
   Wait 30 s for in-flight orders to drain.
3. **Stop laptop launchd services** — `launchctl bootout` every
   `com.predigy.*` plist. Plists stay installed for rollback.
4. **`pg_dump` the laptop DB** to a timestamped `/tmp/predigy-cutover-*.dump`.
5. **Transfer + `pg_restore`** to the Pi. `--clean --if-exists`
   drops + recreates each table. (One PK collision is expected on
   `opportunity_observations` if the Pi's scanner ran during the
   restore window — observation data only, not trading-critical.)
6. **Restart the Pi engine** in shadow mode so it picks up the
   restored positions. Verify no reconciliation drift.
7. **Flip Pi to live**: `sed` `PREDIGY_ENGINE_MODE=live` in
   `~/.config/predigy/env`, `systemctl --user restart
   predigy-engine`. Confirm log line `oms ready mode=Live`.
8. **Disarm Pi kill switch** — engine starts submitting.

## Rollback (Pi → laptop, first ~7 days only)

Mirror of cutover. The laptop launchd plists are still installed
after cutover (just `bootout`'d), so the laptop is ready to take
over again.

```sh
deploy/linux/rollback.sh
```

Phases: arm Pi kill switch → stop Pi services → `pg_dump` Pi →
`pg_restore` to laptop → `launchctl bootstrap` the laptop plists →
disarm laptop kill switch.

After 7 clean days on the Pi: `launchctl disable
gui/$(id -u)/com.predigy.*` on the laptop to take the rollback
path permanently offline. Or leave them disabled-but-installed
forever; they cost nothing dormant.

## Day-to-day operations

```sh
# from the laptop (or on the Pi without the ssh prefix)
ssh dan@nas.local 'systemctl --user list-units "predigy-*"'
ssh dan@nas.local 'systemctl --user list-timers "predigy-*"'
ssh dan@nas.local 'journalctl --user -u predigy-engine -f'
ssh dan@nas.local 'tail -f ~/.local/state/predigy/logs/engine.stderr.log'

# arm / disarm kill switch
ssh dan@nas.local 'echo armed > ~/.config/predigy/kill-switch.flag'
ssh dan@nas.local ': > ~/.config/predigy/kill-switch.flag'

# force a scheduled job to fire now
ssh dan@nas.local 'systemctl --user start predigy-cross-arb-curate.service'

# restart engine after editing the env file
ssh dan@nas.local 'systemctl --user restart predigy-engine.service'

# manual pg_dump
ssh dan@nas.local 'bash ~/code/predigy/deploy/scripts/db-backup.sh'

# dashboard
open http://nas.local:8080
```

## Updating code on the Pi

For non-engine changes (curator scripts, env vars, unit files):

```sh
ssh dan@nas.local 'cd ~/code/predigy && git pull && bash deploy/linux/install-systemd.sh'
```

For engine code changes: rebuild before restart.

```sh
ssh dan@nas.local 'cd ~/code/predigy && git pull && source ~/.cargo/env && cargo build --release --workspace'
ssh dan@nas.local 'systemctl --user restart predigy-engine.service predigy-dashboard.service'
```

Build takes ~10–15 min on Pi 5 for incremental changes. For
crate-level dep changes, expect 15–25 min.

## What is NOT deployed (by design)

- **`wx-curate.{service,timer}`** — feeds the `latency` strategy
  which is DISABLED 2026-05-08 pending FIX + colocation. Adding it
  back later is trivial — write the two unit files and re-run
  `install-systemd.sh`.
- **`wx-stat-curate.{service,timer}`** — feeds `wx-stat` which is
  DISABLED 2026-05-09 (3W/8L after fees). Same story; easy to add
  if/when the strategy re-enables.
- **The 4.5 TB USB HDD at `/media/devmon/NAS`** — exfat with
  `uid=devmon` and no Unix ACLs, so `dan`-as-systemd can't write
  to it without a global remount that would break CasaOS and
  SierraChart usage. Backups go to the SD card instead (52 GB free,
  decades of write endurance at the actual volume). Layer
  rclone-to-cloud on top if you want offsite redundancy.

## Safety checklist before touching production

- [ ] Read this doc.
- [ ] Confirm `systemctl --user is-active predigy-engine.service` is
      `active` before doing anything destructive.
- [ ] Arm the kill switch first if you're about to restart the
      engine while orders may be in-flight.
- [ ] For schema changes: add a new migration under `migrations/`,
      apply it on the Pi via `psql -d predigy -f <file>` BEFORE
      deploying code that uses it. `sqlx` macros check at compile
      time.
- [ ] For env-var changes: edit `~/.config/predigy/env`, then
      restart the affected services. systemd does NOT reload
      `EnvironmentFile=` automatically.
- [ ] Don't `launchctl enable` anything on the laptop while the Pi
      is the live host. Both hosts using the same Kalshi key from
      different IPs causes 429s and can race on the authed WS
      channels.
