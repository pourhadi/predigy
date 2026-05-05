# Session Handoff Notes

> **Read this first.** A short, durable orientation for any new Claude
> Code session picking up this codebase. The other docs (`PLAN.md`,
> `STATUS.md`) describe the design and phase status; this doc says
> *what is currently true* operationally — what's deployed, what's
> running, where the money is, what to touch carefully.
>
> Keep this doc current. End-of-session, update the `What's running`
> and `Open work` sections.

## What is the user trying to do

Build a small automated trading system that profits on Kalshi
prediction markets, starting with a $50 funded account and growing
through compounding edge. The user has:

- Deep coding skills, "just keep going / full speed ahead" workflow.
- Limited tolerance for over-asking — make decisions and move forward.
- "Money first, optimize later" preference — deployable strategies
  beat unbuilt theory.
- A laptop, no VPS yet (planned for the latency push).

## What is running RIGHT NOW (laptop, macOS)

Three launchd jobs under the user's account (UID 501):

| Label | Purpose | State |
|---|---|---|
| `com.predigy.latency-trader` | NWS-driven weather strategy. **LIVE** (real submission, $5 account cap). | running |
| `com.predigy.wx-curate` | Daily 06:30 cron — re-curates rules via Claude API. | scheduled |
| `com.predigy.dashboard` | HTTP server :8080 — mobile-friendly read-only dashboard. | running |

Verify with `launchctl print gui/$(id -u)/com.predigy.<label>`.
Logs live in `~/Library/Logs/predigy/*.log`.

### Where money lives

- **Kalshi production account**: ~$49.85 cash as of last check.
- **Account cap in the daemon**: `PREDIGY_MAX_ACCOUNT_NOTIONAL=500` cents
  ($5). Override in `~/.zprofile` if needed.
- **Daily-loss breaker**: `PREDIGY_MAX_DAILY_LOSS=200` cents ($2).
- **Per-side notional cap**: `PREDIGY_MAX_NOTIONAL_PER_SIDE=200` cents.

### Where credentials live

- `~/.config/predigy/kalshi.pem` — RSA private key, mode 0600.
- `~/.zprofile` — `KALSHI_KEY_ID`, `ANTHROPIC_API_KEY`, `NWS_USER_AGENT`,
  `PREDIGY_LIVE=1`. The launchd plists run `zsh -lc` so these are
  visible at process start.
- The Kalshi key is `a381c833-6172-4b19-a27e-a0b2345f86c7`.
  **Rotate after the user is done iterating** — it's been pasted
  into Claude conversation history.

### Persistent state on disk

`~/.config/predigy/`:

| File | Purpose |
|---|---|
| `kalshi.pem` | Kalshi RSA private key (operator-managed). |
| `wx-rules.json` | Latency-trader rule set, written by wx-curator. |
| `oms-cids` | Cid sequence + chunk pre-allocation. |
| `oms-state.json` | OMS positions, daily P&L, kill-switch, in-flight orders. |
| `wx-seen.json` | NWS alert ids already processed (prevents re-fire on restart). |

A restart of `latency-trader` resumes cleanly from these. Don't
delete them mid-trading.

## Architecture quick map

**Single tokio task per binary.** All state mutation goes through
mpsc channels into the OMS task; no shared mutable state, no locks.

**Layered crates:**

- `crates/core` — domain types (Price, Qty, Order, Fill, Side, etc.)
- `crates/book` — order book (snapshot + delta + gap detection)
- `crates/risk` — pre-trade risk engine (limits + breakers)
- `crates/oms` — order management state machine, cid allocator,
  state persistence (`StateBacking::Persistent`)
- `crates/kalshi-rest` — Kalshi REST client (auth-optional)
- `crates/kalshi-md` — Kalshi WebSocket client (public + authed channels)
- `crates/kalshi-exec` — `oms::Executor` impl over Kalshi REST + WS fills
- `crates/kalshi-fix` — FIX 4.4 framing + messages (production wiring NOT done)
- `crates/poly-md` — Polymarket WS reference client
- `crates/ext-feeds` — NWS active alerts poller (with seen-set persistence)
- `crates/signals` — Bayes/Elo/Kelly helpers (used by stat-trader)
- `crates/sim` — backtester runtime + replay

**Strategy binaries:**

- `bin/arb-trader` — single-market YES+NO parity arb. Live-shaken,
  confirmed not profitable on efficient markets (NBA series).
  Keep as regression test only.
- `bin/cross-arb-trader` — Kalshi-vs-Polymarket convergence. Built,
  NEVER live-shaken. Needs Kalshi/Polymarket pair list to run.
- `bin/latency-trader` — NWS alerts → Kalshi weather markets. **Currently live.**
- `bin/stat-trader` — operator-supplied model probabilities. Built,
  no rules curated for it yet.
- `bin/md-recorder` — NDJSON market data recorder.
- `bin/sim-runner` — offline backtester driver.

**Operational binaries:**

- `bin/wx-curator` — Claude-powered rule curator for the weather
  strategy. Hits Anthropic Messages API.
- `bin/dashboard` — read-only HTTP dashboard, port 8080, mobile-first.

**Deploy artifacts** (`deploy/`):

- `macos/com.predigy.{latency-trader,wx-curate,dashboard}.plist` — launchd jobs
- `scripts/install-launchd.sh` — preflight + idempotent install
- `scripts/wx-curate.sh` — daily curator wrapper
- `scripts/latency-trader-run.sh` — trader launcher with persistence
- `README.md` — operational doc

## Verified live (each cost real money — small)

| Path | Status | Cost |
|---|---|---|
| RSA-PSS auth (REST + WS) | ✅ | $0 |
| WS market data (Kalshi orderbook_delta + ticker) | ✅ | $0 |
| WS authed fills + market_position | ✅ (PR #16) | $0 |
| OMS submit → Acked → Cancelled | ✅ | $0 |
| OMS submit → Acked → Filled → PositionUpdated | ✅ | $0.06 round-trip |
| OMS persistence across restart | ✅ | $0 |
| NWS seen-set persistence across restart | ✅ | $0 |
| Live weather strategy (dry-run) | ✅ | $0 |
| Live weather strategy (live submit) | ⚠ just flipped, validating | TBD |

## Bugs found during shakedown (all fixed)

1. `*_dollars` REST fields are quoted decimal strings, not f64.
2. Orderbook wrapper is `orderbook_fp` with `yes_dollars`/`no_dollars`
   `[String; 2]` levels.
3. Recorder REST-resync infinite loop (REST has no seq).
4. `MarketPosition.position` → `position_fp` (decimal string).
5. Kalshi V2 fill records have `action: ""` (empty); use the
   originating order's tracked `(Side, Action)` instead.
6. NWS area-param needed comma-separated form, not repeated `?area=`.
7. NWS dedup state was in-memory only; restart re-fired every active alert.
8. `area_substring` rule filter was unreliable; switched to
   `required_states` + `geocode.UGC` parsing.
9. `wx-curate.sh` rule-count grep used wrong field name.
10. `latency-trader-run.sh` shell-quoting bug on `--nws-states`.

These all live in PR history (#7-#22). When something fails, look
for similar wire-mismatch issues — Kalshi V2 docs and reality
diverge.

## Open work / next session priorities

In rough order of leverage:

1. **Validate live weather strategy** by watching for a few days.
   Check `~/Library/Logs/predigy/latency-trader.stderr.log` for
   `rule fired` lines and verify those would have been positive-EV
   trades. If a rule consistently fires on bad correlations
   (e.g. CO-mountain Winter Storm → Denver airport temp), edit
   `wx-curator/src/prompt.rs` to discourage it and re-curate.

2. **Cross-arb-trader live shake-down.** Built but never live-tested.
   Needs Kalshi/Polymarket pair list. Practical pairs to start:
   - 2026 election outcomes (Polymarket has many, Kalshi has corresponding)
   - FOMC rate decisions (both venues list these around meetings)
   The pairing is `--pair KALSHI_TICKER=POLYMARKET_ASSET_ID`. Run
   in dry-run for a session, look for divergences > 3¢.

3. **Settlement-time sports strategy.** Not built. Design:
   - Watch sports markets within 5-10 min of `close_time`.
   - When `yes_bid_size_qty >> yes_ask_size_qty` AND `yes_ask in [90, 97]`
     AND time-to-close < 5 min → buy YES IOC at touch.
   - Thesis: liquidity asymmetry near settlement signals the price
     will move up; lift before it does.
   - ~400 LOC in `bin/settlement-trader/`. Same OMS/risk wiring
     as other traders. New strategy_id (e.g. "sett").

4. **Latency push** — us-east-1 VPS + FIX exec.
   - VPS (Lightsail / Linode $5-15/mo): drops Kalshi RTT from
     ~100 ms to ~5-15 ms.
   - Port `deploy/macos/*.plist` → `deploy/linux/*.service` (systemd).
   - Wire `predigy-kalshi-fix` to prod: TLS to Kalshi's FIX endpoint,
     real Logon handshake, heartbeat, sequence-number persistence,
     `FixExecutor: oms::Executor` impl.
   - Need to email `[email protected]` for FIX access first.

5. **Dashboard upgrades** (lower priority, polish):
   - Kill-switch button (currently dashboard is read-only).
   - Daily-P&L chart (last 7 days bar chart).
   - Per-rule fire history.

## Conventions when working in this repo

- **Single rolling branch per chunk, single PR.** User said: don't
  slice work into multiple PRs unnecessarily. They are the only reviewer.
- **Don't simplify when stuck.** Per `CLAUDE.md`: no fallbacks, no
  workarounds, no temporary hacks. Find the root cause.
- **Always commit after each round of code updates.**
- **Prod-API wire-shape changes are common.** When something fails to
  decode, suspect Kalshi schema drift first; their V2 docs lag reality.
- **No "dummy code" or demos.** Operator-grade only.
- **Test live, not just unit.** The live shake-down ladder caught
  10 bugs that unit tests missed.

## Stopping the world (kill switch)

If something looks wrong and you need to halt all trading:

```sh
launchctl bootout gui/$(id -u)/com.predigy.latency-trader
```

This sends SIGTERM; OMS persists final state. **Resting orders on
Kalshi are NOT cancelled** — visit kalshi.com/portfolio or run
`crates/kalshi-rest/examples/close_position.rs` to flatten.

## Doc map

- `README.md` — project overview, build/test commands.
- `docs/PLAN.md` — full architecture + strategy plan (long, dense).
- `docs/STATUS.md` — phase-by-phase build status.
- `docs/RUNBOOK.md` — operational procedures (how to debug, intervene).
- `docs/SESSIONS.md` — **this file**.
- `deploy/README.md` — deployment + ops layout.
