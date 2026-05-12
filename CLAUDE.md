# CLAUDE.md — repo-specific instructions for Claude Code

This file is auto-loaded by Claude Code at the start of every
session. It points at the durable handoff context — read those docs
before doing real work.

## Read first, in order

1. **[`docs/SESSIONS.md`](./docs/SESSIONS.md)** — what's deployed,
   what's running, where the money is, what's next.
2. **[`docs/DEPLOY.md`](./docs/DEPLOY.md)** — **how the live system
   is deployed today (Pi at nas.local), cutover + rollback runbook,
   what to do when a service misbehaves.** Read before touching
   anything that affects production.
3. **[`docs/ARCHITECTURE.md`](./docs/ARCHITECTURE.md)** — engine
   refactor design (Phases 0–7); the load-bearing system doc.
4. **[`docs/RUNBOOK.md`](./docs/RUNBOOK.md)** — health checks,
   kill switch, debugging recipes.
5. **[`docs/AUDIT.md`](./docs/AUDIT.md)** — current strategy /
   scale-up / arsenal-expansion analysis. Use when planning.

## Current operational reality (2026-05-12)

**Live in production on the Raspberry Pi 5 at `nas.local`
(`dan@192.168.1.35`).** The macOS laptop is offline, paused, and
held as the rollback path for ~7 days.

- **Host:** Pi 5, 8 GB RAM, Debian 13 (trixie), PostgreSQL 17,
  ~15 ms RTT to Kalshi from the home ISP.
- **Service manager:** `systemd --user` (units at
  `~/.config/systemd/user/predigy-*.{service,timer}`). Source of
  truth: [`deploy/linux/systemd/`](./deploy/linux/systemd/).
- **Bootstrap:** [`deploy/linux/install-systemd.sh`](./deploy/linux/install-systemd.sh)
  installs units, enables linger, starts everything.
- **Live services:** `predigy-engine.service` (consolidated 6-strat
  trader), `predigy-dashboard.service` (HTTP at
  `http://nas.local:8080`).
- **Scheduled timers** (all matching the prior launchd cadences):
  `predigy-stat-curate`, `predigy-cross-arb-curate`,
  `predigy-arb-config-curate`, `predigy-calibration`,
  `predigy-paper-trader`, `predigy-opportunity-scanner`,
  `predigy-eval-daily`, `predigy-db-backup` (new — pg_dump+rotate
  daily at 03:00 to `~/.local/state/predigy/backups/`).
- **Engine mode:** `EngineMode::Live` — submits real orders to
  Kalshi V2 REST.
- **Kalshi WS:** one connection for orderbook deltas, one separate
  for authed `fill` + `market_positions` channels.
- **Postgres `predigy`** on the Pi (UNIX-socket peer auth as
  `dan`) is the source of truth.
- **Kill-switch flag** at `~/.config/predigy/kill-switch.flag` —
  non-empty contents arms; truncated disarms. Both engine and
  dashboard observe it.
- **Capital cap:** $80/strategy, $200 global, $20 daily-loss;
  ~$80 funded.
- **Env file:** `~/.config/predigy/env` (loaded by every systemd
  unit via `EnvironmentFile=`). Template:
  [`deploy/linux/env.example`](./deploy/linux/env.example).
- **Skipped on the Pi** (strategies disabled upstream): `wx-curate`
  (latency strategy DISABLED 2026-05-08) and `wx-stat-curate`
  (`wx-stat` strategy DISABLED 2026-05-09). Add the units back
  if/when those strategies re-enable.

Curators stay external by design: `stat-curate`,
`cross-arb-curate`, `arb-config-curate`, and the observation jobs.
Their wrapper scripts live in [`deploy/scripts/`](./deploy/scripts/);
the systemd timers in [`deploy/linux/systemd/`](./deploy/linux/systemd/)
schedule them.

Verify what's actually running (from the laptop):

```sh
ssh dan@nas.local 'systemctl --user list-units "predigy-*"'
ssh dan@nas.local 'systemctl --user list-timers "predigy-*"'
ssh dan@nas.local 'psql -d predigy -c "SELECT status, COUNT(*) FROM intents GROUP BY status;"'
ssh dan@nas.local 'tail -n 50 ~/.local/state/predigy/logs/engine.stderr.log'
ssh dan@nas.local 'journalctl --user -u predigy-engine -n 50 --no-pager'
```

Or on the Pi itself: drop `ssh dan@nas.local '...'` from each.

**Cutover & rollback runbooks:**
- [`deploy/linux/cutover.sh`](./deploy/linux/cutover.sh) — laptop →
  Pi, paused at each step.
- [`deploy/linux/rollback.sh`](./deploy/linux/rollback.sh) — Pi →
  laptop, mirror in the opposite direction.
- Full walkthrough + safety reasoning: [`docs/DEPLOY.md`](./docs/DEPLOY.md).

## Working norms in this repo

- **No fallbacks, no temporary workarounds, no "simplifying" things
  to bypass a problem.** Find the root cause and fix it. (See
  `~/.claude/CLAUDE.md`.)
- **Single rolling branch per chunk, single PR.** User is the only
  reviewer.
- **Always commit at end of each round of code updates** with a
  detailed message explaining what + why.
- **Live-test, don't just unit-test.** When something fails in
  prod, suspect Kalshi V2 wire-shape drift first. The 2026-05-07
  cutover surfaced one example: Kalshi V2 rejects cids containing
  `.`, fixed via `engine_core::cid_safe_ticker` — the engine ports
  had to learn what the legacy `CidAllocator` already knew.
- **Build for production, not demos.** Don't create dummy code;
  always do the full implementation.

## When the user asks "what's next"

Default to forward motion on:

1. **Watch live trading on the Pi** — host cutover happened
   2026-05-12 07:23 UTC; first 24h is the critical window. Monitor
   `journalctl --user -u predigy-engine -f` on the Pi (or `ssh
   dan@nas.local 'tail -f ~/.local/state/predigy/logs/engine.stderr.log'`).
   The macOS laptop is paused but the launchd plists remain
   installed for ~7 days as the rollback path.
2. **Phase 4b (FIX)** — blocked on Kalshi institutional onboarding;
   the email draft is in `docs/KALSHI_FIX_REQUEST.md`. Operator
   action required.
3. **Audit follow-throughs** — scale-up + arsenal-expansion items
   listed in `docs/AUDIT.md`. Pick the highest-ROI item.

Ask only when the user's goal is genuinely unclear; otherwise pick
and execute.

## Cross-platform context

This project is one of two automated-trading platforms the operator
runs. The other is `~/code/tradegy` — a Python-based platform for
equity-index options + futures, with a live IBKR paper-trading daemon
running a 0DTE iron condor on MES futures options. tradegy harvests
the variance risk premium (selling vol on equity indices) — a
categorically different return source than predigy's prediction-
market arbitrage / latency / statistical alpha lanes.

The two platforms harvest **uncorrelated** return streams. Running
both is real diversification of mechanism, not just instrument.

The joint strategic plan — capital projections at each tier, what's
reusable between the platforms, and the priority sequence for
extending each — lives at `~/code/MOONSHOT_PLAN.md`. Read that doc
when planning cross-platform work or evaluating where to allocate
operator time.
