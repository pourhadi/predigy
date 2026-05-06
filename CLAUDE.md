# CLAUDE.md — repo-specific instructions for Claude Code

This file is auto-loaded by Claude Code at the start of every
session. It points at the durable handoff context — read those docs
before doing real work.

## Read first, in order

1. **[`docs/SESSIONS.md`](./docs/SESSIONS.md)** — what's deployed,
   what's running, where the money is, what's next. The most
   important doc; updated end-of-session.
2. **[`docs/RUNBOOK.md`](./docs/RUNBOOK.md)** — health checks,
   common interventions, debugging recipes, kill switch.
3. **[`docs/STATUS.md`](./docs/STATUS.md)** — phase-by-phase build
   status, file tree, test counts, API contracts.
4. **[`docs/PLAN.md`](./docs/PLAN.md)** — full architectural plan
   (long; reference only when designing).

## Current operational reality (snapshot)

- **macOS laptop deployment** via launchd. Three jobs live: trader,
  daily curator (06:30), dashboard (port 8080).
- **Weather strategy is LIVE** with real submission, $5 account cap.
- Kalshi balance ~$49.85 funded. Account is the user's; treat as
  production.
- Persistent state in `~/.config/predigy/`. Logs in
  `~/Library/Logs/predigy/`.

Verify what's actually running with:

```sh
for n in com.predigy.{latency-trader,wx-curate,dashboard}; do
    state=$(launchctl print "gui/$(id -u)/$n" 2>/dev/null \
        | grep -E '^\s*state\s*=' | head -1 | awk -F= '{print $2}' | xargs)
    echo "$n: ${state:-NOT LOADED}"
done
```

## Working norms in this repo

From the user's profile + memory:

- **Single rolling branch per chunk, single PR.** Don't slice work
  into multiple PRs unnecessarily — user is the only reviewer.
- **No fallbacks, no temporary workarounds, no "simplifying" things
  to bypass a problem.** Find the root cause and fix it. (See the
  user's global `~/.claude/CLAUDE.md`.)
- **Always commit at end of each round of code updates** with a
  detailed message explaining what + why.
- **Build for production, not demos.** "Don't ever create dummy
  code; always do the full implementation."
- **Live-test, don't just unit-test.** The shake-down ladder caught
  10 prod-only bugs that unit tests missed. When something fails in
  prod, suspect Kalshi V2 wire-shape drift first.
- **Money + rotating keys**: the Kalshi private key has been pasted
  into conversation history. Remind the user to rotate after major
  iteration cycles.

## When the user asks "what's next"

Default to forward motion on the highest-leverage open work in
`docs/SESSIONS.md` § "Open work / next session priorities". The
top three at this moment:

1. Validate live weather strategy (passive — watch logs).
2. Cross-arb-trader live shake-down (built, not yet live).
3. Settlement-time sports strategy (not yet built).

Then latency push (us-east-1 VPS + FIX exec).

Ask only when the user's goal is genuinely unclear; otherwise pick
and execute.

## Cross-platform context

This project is one of two automated-trading platforms the operator
runs.  The other is `~/code/tradegy` — a Python-based platform for
equity-index options + futures, with a live IBKR paper-trading daemon
running a 0DTE iron condor on MES futures options as of 2026-05-06.
tradegy harvests the variance risk premium (selling vol on equity
indices) — a categorically different return source than predigy's
prediction-market arbitrage / latency / statistical alpha lanes.

The two platforms harvest **uncorrelated** return streams.  Running
both is real diversification of mechanism, not just instrument.

The joint strategic plan — capital projections at each tier, what's
reusable between the platforms, and the priority sequence for extending
each — lives at `~/code/MOONSHOT_PLAN.md`.  Read that doc when planning
cross-platform work or evaluating where to allocate operator time.
