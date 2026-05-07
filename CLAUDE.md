# CLAUDE.md — repo-specific instructions for Claude Code

This file is auto-loaded by Claude Code at the start of every
session. It points at the durable handoff context — read those docs
before doing real work.

## Read first, in order

1. **[`docs/SESSIONS.md`](./docs/SESSIONS.md)** — what's deployed,
   what's running, where the money is, what's next.
2. **[`docs/ARCHITECTURE.md`](./docs/ARCHITECTURE.md)** — engine
   refactor design (Phases 0–7); the load-bearing system doc.
3. **[`docs/RUNBOOK.md`](./docs/RUNBOOK.md)** — health checks,
   kill switch, debugging recipes.
4. **[`docs/AUDIT.md`](./docs/AUDIT.md)** — current strategy /
   scale-up / arsenal-expansion analysis. Use when planning.

## Current operational reality (2026-05-07)

**Live in production via consolidated `predigy-engine` binary.**

- Engine cutover happened 07:45 UTC today. Five strategy modules
  run in one process: `stat`, `settlement`, `latency`, `cross-arb`,
  `wx-stat` (`wx-stat` registers only when
  `PREDIGY_WX_STAT_RULE_FILE` points to a curator-output JSON).
- Legacy per-strategy daemons (`*-trader`) booted out of launchd.
- Engine in `EngineMode::Live` — submits real orders to Kalshi V2
  REST.
- Kalshi WS: one connection for orderbook deltas, one separate for
  authed `fill` + `market_positions` channels.
- Postgres `predigy` is the source of truth for intents, fills,
  positions, rules, kill switches.
- Kill-switch flag at `~/.config/predigy/kill-switch.flag` —
  non-empty contents arms; truncated disarms. Both engine and
  dashboard observe it.
- Capital cap: $5/strategy, $15 global, $2 daily-loss; ~$50 funded.

Curators stay external by design (cron-driven Anthropic agents):
`wx-curate`, `stat-curate`, `cross-arb-curate`, `wx-stat-curate`,
plus `predigy-import` (legacy state-file mirror).

Verify what's actually running:

```sh
launchctl list 2>/dev/null | grep predigy
psql -d predigy -c "SELECT status, COUNT(*) FROM intents GROUP BY status;"
tail -n 50 ~/Library/Logs/predigy/engine.stderr.log
```

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

1. **Watch live trading** — the engine just cut over today; first
   24h is the critical window.
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
