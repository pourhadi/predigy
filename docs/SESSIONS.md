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
  NEVER live-shaken. Pair list now produced by `cross-arb-curator`
  (was previously operator-supplied).
- `bin/latency-trader` — NWS alerts → Kalshi weather markets. **Currently live.**
- `bin/stat-trader` — operator-supplied model probabilities. Built,
  no rules curated for it yet.
- `bin/md-recorder` — NDJSON market data recorder.
- `bin/sim-runner` — offline backtester driver.

**Operational binaries:**

- `bin/wx-curator` — Claude-powered rule curator for the weather
  strategy. Hits Anthropic Messages API.
- `bin/cross-arb-curator` — Claude-powered Kalshi/Polymarket pair
  curator for `cross-arb-trader`. Conservative settlement-alignment
  prompt; drops ambiguous pairs. Hits gamma-api.polymarket.com +
  Kalshi REST + Anthropic Messages API.
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

## Stat-trader lane added 2026-05-06

The stat-curator + stat-trader pair is now built and shipped, mirroring
the wx-curator + latency-trader pattern but for statistical-alpha
betting on sports / politics / elections / world / economics markets.

**What's in:**

- `bin/stat-curator/` — Rust binary that scans Kalshi via REST,
  filters to actionable markets settling within `--max-days-to-settle`
  (default 14), batches them to Claude with a calibrated-probability
  prompt, validates each proposed rule (probability range,
  confidence rating, edge gap, side direction), writes
  `~/.config/predigy/stat-rules.json`.  Live-shaken 2026-05-06:
  scanned 25 markets, Claude proposed 2, validated 1 (TSA passenger
  count, Yes side, model_p=0.28, edge=9¢).
- `bin/stat-trader/` — was already built; consumes the rule file
  the curator now produces.
- `deploy/scripts/stat-curate.sh` + `deploy/scripts/stat-trader-run.sh`
- `deploy/macos/com.predigy.stat-curate.plist` (every 6h:
  02/08/14/20 local) + `deploy/macos/com.predigy.stat-trader.plist`
  (Disabled=true by default)
- Workspace + install-launchd.sh updated to include both new jobs.

**To activate stat-trader live:**

1. Confirm at least one stat-curate run has produced
   `~/.config/predigy/stat-rules.json` with non-empty content.
2. Manually review the rules — each has a `model_p`, `side`, and
   `min_edge_cents`.  Reject anything that looks miscalibrated.
3. Edit `<key>Disabled</key><true/>` → `<false/>` in
   `deploy/macos/com.predigy.stat-trader.plist`.
4. Re-run `deploy/scripts/install-launchd.sh`.
5. Watch `~/Library/Logs/predigy/stat-trader.stderr.log` for fires.

**Risk caps default tight for shake-down:** $5 account-wide gross,
$2 per-side, $2 daily-loss breaker, max 3 contracts per fire,
60s cooldown between fires per market.  Override via
`PREDIGY_STAT_*` env vars in `~/.zprofile` after validation.

**Cost shape:** stat-curate Anthropic call is ~3.4K input + ~900
output tokens per batch = ~$0.02/batch.  Default 4 batches/run, 4
runs/day = ~$0.32/day = ~$10/month.

## wx-stat lane scaffolded 2026-05-06 (Phase 1 — not yet deployed)

Forecast-driven cousin of stat-curator: same `StatRule[]` output, but
`model_p` comes from the NWS hourly point forecast instead of an LLM.
Targets Kalshi daily-temperature markets (`KXHIGH*` / `KXLOW*`).

**What's in (Phase 1):**

- `crates/ext-feeds/src/nws_forecast.rs` — `NwsForecastClient` with
  `lookup_point(lat, lon) → GridPoint` and `fetch_hourly(GridPoint) →
  HourlyForecast`. Handles both scalar and gridded NWS response
  shapes.
- `bin/wx-stat-curator/` — full curator. Modules: `airports.rs`
  (30 hand-curated airport→lat/lon), `ticker_parse.rs` (event
  ticker + Kalshi `floor_strike`/`strike_type`/`occurrence_datetime`
  → structured spec), `kalshi_scan.rs` (Climate-and-Weather → temp
  markets only), `forecast_to_p.rs` (forecast aggregate → conviction
  zone gate → 0.97/0.03 model_p).
- `crates/kalshi-rest` extended `MarketSummary` with optional
  `floor_strike` / `cap_strike` / `strike_type` /
  `occurrence_datetime` fields. Non-breaking via `#[serde(default)]`.
- `docs/WX_STAT_PLAN.md` — full Phase 1 / 2 / 3 plan, edge thesis,
  risk register.

**Live shake-down 2026-05-06:** scanned 285 actionable temp markets
across 13 airports, emitted 21 rules, 263 skipped (most inside the
5°F conviction zone). Audit log shows forecast values, hours
considered, model_p, side, and yes_ask side-by-side. One example
candidate: `KXLOWTOKC-26MAY07-T43` (>43F low) — NWS forecast 53F low
→ model_p=0.97, market yes_ask=50¢. Real ~47¢ pre-fee apparent edge.

**Deploy scaffolding shipped (Disabled=true), not yet enabled:**

- `deploy/scripts/wx-stat-curate.sh` — wrapper. Writes to
  `~/.config/predigy/wx-stat-rules.json` (separate from
  stat-rules.json — the wx-stat output is intentionally
  quarantined for review in Phase 1).
- `deploy/macos/com.predigy.wx-stat-curate.plist` — every 3h:
  00/03/06/09/12/15/18/21 local. **Disabled=true by default.**
- `deploy/scripts/install-launchd.sh` updated to add the new job.
- Smoke-tested 2026-05-06: wrapper produces 20 valid rules in
  `~/.config/predigy/wx-stat-rules.json`.

**To enable Phase 1 inspection-only runs:**
1. Build: `cargo build --release -p wx-stat-curator`
2. Edit `deploy/macos/com.predigy.wx-stat-curate.plist` →
   `<key>Disabled</key><false/>`
3. Re-run `deploy/scripts/install-launchd.sh`
4. Watch the rule file refresh every 3h:
   `tail -f ~/Library/Logs/predigy/wx-stat-curate.stderr.log`

**To promote rules to live trading:** wx-stat-rules.json is NOT
yet read by stat-trader. To put weight on these rules, copy the
ones you trust into `~/.config/predigy/stat-rules.json` and let
stat-trader pick them up on its own poll cadence. The deliberate
two-file split keeps the LLM-curated stat rules and the
forecast-derived wx-stat rules from racing.

Phase 2 (NBM probabilistic + per-airport calibration) and Phase 3
(auto-merge into stat-rules.json + bigger-cap deploy) are
described in `docs/WX_STAT_PLAN.md`.

**Phase 1 conviction-zone gate**: rules only emit when forecast
margin to the threshold is ≥ 5°F. This compensates for NWS hourly
being a point forecast rather than a distribution. Phase 2 replaces
this with calibrated probabilities from NBM gridded data.

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

3. **Settlement-time sports strategy.** ✅ Built (PR #24).
   `bin/settlement-trader` watches sports markets near `close_time`,
   fires when `yes_ask in [88,96]` AND
   `bid_stack_qty >= 5 × ask_stack_qty` (book-asymmetry tell).
   12 unit tests covering all gates + cooldown.
   Deployment scaffold ready (`com.predigy.settlement.plist`,
   `Disabled=true`). Activate by:
   1. Author `~/.config/predigy/settlement-markets.txt` —
      one Kalshi ticker per line, sports preferred.
   2. `cargo build --release -p settlement-trader`.
   3. Edit `Disabled` key out of the plist + run install script.
   **Live shake-down pending** — try a calm Saturday with NBA
   playoffs in the final minutes. Dry-run a few sessions before
   flipping to live; settlement-time strategies are loss-tail-heavy
   and need real-data validation.

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
