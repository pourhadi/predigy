# State Log — operational changes timeline

> **Single chronological record of every operational change to
> the running fleet.** Read this if you want to know "what changed
> recently and why." Append-only — never delete entries; if a
> change is reverted, add a new entry referencing the original.
>
> The complementary docs:
> - `SESSIONS.md` — current state snapshot (always reflects "now")
> - `AUDIT_2026-05-08.md` — strategy mechanism verdicts + re-enable
>   conditions
> - `plans/2026-05-08-*.md` — forward-looking plans
>
> This file is for *what we did and when*. Future Claude sessions
> reconstruct context from here.

## 2026-05-12 06:16 UTC — REST submitter terminates zero-fill IOC/FOK responses immediately

**Why**: the OMS venue-only cleanup succeeded, but the live watch
caught a separate transient reconciliation WARN at 06:10 UTC:
`status_mismatches: [(..., "acked", "canceled")]`. Root cause:
Kalshi's create-order response for a stale-touch IOC already returned
`fill_count="0.00"` and `remaining_count="0.00"`, but
`venue_rest` still marked every successful create response `acked`.
With no fill event to apply, reconciliation corrected the row to
`cancelled` on the next pass and logged drift.

**Code change** (`bin/predigy-engine/src/venue_rest.rs`):

- For IOC/FOK create responses with parsed `fill_count == 0` and
  `remaining_count == 0`, mark the intent `cancelled` immediately
  and persist the returned `venue_order_id`.
- GTC zero-fill creates still go through the normal `acked` path.
- IOC/FOK responses with any fill count still wait for the existing
  fill path so position accounting stays centralized.

**Verification**:

- `cargo test -p predigy-engine venue_rest`: 12/12 venue_rest tests passed.
- `cargo build --release -p predigy-engine`: passed.
- Engine restarted live at 06:15 UTC.
- The next live stale-touch probe
  `book-maker:KXNHLGAME-26MAY12ANAVGK-ANA:X:sl:43:01c45018`
  logged `venue_rest: immediate order cancelled unfilled` with
  `fill_count=Some("0.00")`, `remaining_count=Some("0.00")`.
- Postgres row for that client id was `status='cancelled'` with the
  venue order id at 06:16:06, without waiting for reconciliation.
- No new `oms: reconciliation found drift` entries appeared through
  the 06:18 live watch window; last such entry remained the
  pre-fix 06:10 probe.

## 2026-05-12 06:00 UTC — OMS cancels venue-only Predigy orders during reconciliation

**Why**: after the fan-out / DB-pool fix, reconciliation still
warned once per minute about 5 `orders_only_at_venue` entries.
Postgres had no active intents and no rows with those
`venue_order_id`s, so the orders were live at Kalshi but unmanaged
by the DB-backed OMS. Leaving them open risks unmanaged fills and
keeps reconciliation noisy.

**Code change** (`bin/predigy-engine/src/oms_db.rs`):

- During `Oms::reconcile`, venue-only resting orders are now
  cancelled automatically **only if** their venue `client_order_id`
  starts with a known Predigy strategy prefix (`book-maker:`,
  `cross-arb:`, `stat:`, etc.).
- Manual / unknown client IDs are left alone and continue to appear
  in `orders_only_at_venue` so the operator can inspect them.
- Added unit coverage that prefix detection is bounded and does
  not match strings like `manual:book-maker:...`.

**Verification**:

- `cargo test -p predigy-engine predigy_client_order_id_detection_is_prefix_bounded`: passed.
- `cargo fmt && cargo build --release -p predigy-engine`: passed.
- Engine restarted live at 05:59 UTC.
- First reconciliation pass cancelled all 5 venue-only Predigy
  orders (`reduced_by=1.00` each):
  `da550ae5-175f-41fa-84f0-c81533187ac2`,
  `c658559c-9c27-4f5d-b5bd-55e888a597ad`,
  `d1ba9c51-faa0-48e4-83dc-cb47612ce22a`,
  `705f3e4e-3eae-43c2-9155-2e800a139972`,
  `facba00e-a6f8-427b-91df-a37cbe42657d`.
- No reconciliation WARNs after 06:01 UTC; active nonterminal DB
  intents remained 0.

## 2026-05-12 05:37 UTC — book-maker stale-touch exit suppression

**Why**: after the 05:18 engine restart, `book-maker` resumed a
known IOC stop-loss loop on `KXNHLGAME-26MAY12ANAVGK-ANA`. The
strategy held a short YES position (-1 @49) and saw a cached
local touch `yes_ask=43`, so it emitted IOC buy-to-close orders
at 43. Kalshi REST showed the venue book had **no bids / no asks**
on that ticker, so every IOC cancelled with 0 fill. Cancelled IOCs
cost no fees, but the loop wasted REST/order-management bandwidth
and polluted logs.

**Code change** (`crates/strategies/book-maker/src/lib.rs`):

- Added per-ticker `ExitAttemptState` keyed by the open-position
  signature (`side`, `signed_qty`, `avg_entry_cents`).
- `evaluate_exits` now counts consecutive IOC exit emissions where
  the position signature is unchanged.
- After `PREDIGY_BOOK_MAKER_EXIT_FAILURE_THRESHOLD` unchanged
  attempts (default **5**), exit emissions for that ticker are
  suppressed for `PREDIGY_BOOK_MAKER_EXIT_FAILURE_COOLDOWN_SECS`
  (default **600s**).
- Any position change resets the counter; closed positions are
  pruned from the tracker during the DB refresh.

**Verification**:

- `cargo test -p predigy-strategy-book-maker`: 13/13 passed.
- `cargo fmt && cargo build --release -p predigy-engine`: passed.
- Engine restarted at 05:36 UTC.
- Live log showed exactly the desired behavior:
  `book-maker: suppressing exits after repeated unchanged-position attempts`
  for `KXNHLGAME-26MAY12ANAVGK-ANA`, `consecutive_exit_emits=5`,
  `cooldown_secs=600`.
- 70s post-suppression watch: no further KXNHLGAME exit emits,
  no new KXNHLGAME intents, engine healthy.

**Follow-up 2026-05-12 05:54 UTC**: tightened retry behavior.
The first deployed suppression allowed a full 5-attempt burst
again every 10 minutes after cooldown expiry. `ExitAttemptState`
now keeps `consecutive_emits` above threshold when a suppression
window expires, so an unchanged position receives exactly one
liquidity probe before immediate re-suppression. Verified by new
unit test `expired_exit_suppression_allows_only_one_probe`
(`cargo test -p predigy-strategy-book-maker`: 14/14 passed) and
live redeploy. Engine remained healthy; the post-restart in-memory
counter naturally started fresh, hit 5 attempts, then suppressed.

## 2026-05-12 05:18 UTC — engine restart: fan-out leak fix + DB pool raise

**Why**: status check uncovered cross-arb had been generating
~100 channel-closed warnings/second for ~7 hours, ballooning
`engine.stderr.log` to 271 MB. Root cause: at 2026-05-11 19:47
UTC the Postgres connection pool (`max_connections=8`) exhausted
under combined load (6 strategies + venue-rest submitter +
reconciliation + kill-switch pollers + persist-market-row +
dashboard's own pool). cross-arb's supervisor saw 6 crashes in
56s on `pool timed out` and flap-stopped the strategy
permanently (`flap_threshold=5/120s`). But the market-data router
and external-feeds dispatcher kept fanning every Kalshi book
delta and Polymarket update to the dead channel, each `try_send`
returning `Closed` and producing a WARN.

Separate finding: `target/release/` directory had been deleted
while the engine + dashboard processes kept running off
in-memory binaries. All curator launchd jobs (`stat-curate`,
`wx-curate`, `wx-stat-curate`, `arb-config-curate`,
`cross-arb-curate`, `calibration`, `opportunity-scanner`,
`paper-trader`) had been exiting 127 since the deletion.

**Code changes** (`bin/predigy-engine/`):

- `external_feeds.rs` — `fan_out_external` + `nws_dispatcher_task`
  now check `tx.is_closed()` before `try_send` and match on
  `TrySendError::Closed` to skip silently. WARN preserved only
  for `TrySendError::Full` (genuine slow-strategy backpressure,
  not dead-strategy noise).
- `market_data.rs` — same treatment in the book-update `fan_out`.
- `main.rs` — `connect_with_retry` PgPool sized
  `max_connections=8 → 32`, `acquire_timeout=5s → 10s`. Sized
  for the engine's concurrent load with headroom; nowhere near
  Postgres default `max_connections=100`.

**Deploy steps**:
- `cargo build --release` — rebuilt all binaries (target/release
  restored, ~3m25s).
- Rotated `engine.stderr.log` → `engine.stderr.log.preFix.20260512_051810`
  (271 MB preserved for forensics).
- `launchctl kickstart -k gui/$(id -u)/com.predigy.engine` +
  `com.predigy.dashboard`.
- Manually kicked + verified each curator script.

**Live verification**:
- Engine log post-restart: 153 lines / 31 KB, **0 channel-closed**,
  **0 ERROR** (vs 1.58M / 433k / many before).
- cross-arb supervisor: registered, boot grace ended, healthy.
- Reconciliation immediately caught the 2 stale phantom positions
  the old engine was carrying (`KXNBAPTS-26MAY11DETCLE-CLEEMOBLEY4-15`
  -25 → 0, `KXMLBGAME-26MAY111907TBTOR-TB` 1 → 0). open positions
  4 strategies / 23 rows → 3 strategies / 19 rows.
- Dashboard: HTTP 200 sub-ms on `/`, `/api/state`, `/healthz`;
  no DB pool exhaustion since restart.
- launchctl status: 9/10 jobs last-exit 0; wx-stat-curate's status
  reflects a pre-rebuild attempt and will heal on next hourly run.

**Follow-up completed 2026-05-12 05:37 UTC**: the `book-maker`
stale-touch IOC exit loop now has a per-ticker unchanged-position
suppression counter. See the entry above.

## 2026-05-11 01:30 UTC — book-maker reverts to min_spread=2 with tighter SL/TP

**Why**: the 4c min_spread was too restrictive. Only 2 intents
fired in 20 minutes after the change — the maker was starved.

**Reverted in `~/.config/predigy/book-maker-config.json`**:
- `min_spread_cents` 4 → 2 across all 70 tickers (back to
  maximum fill rate)

**Tightened in `~/.zprofile`**:
- `PREDIGY_BOOK_MAKER_STOP_LOSS_CENTS=4` (was 8) — cuts the
  damage per adverse-selection fill in half
- `PREDIGY_BOOK_MAKER_PROFIT_TAKE_CENTS=4` (was 5) —
  symmetric P&L thresholds

**Math** at the new settings:
- 2c spread × ~50% round-trip rate = +1c per fill gross
- 4c SL × ~10% adverse rate = -0.4c per fill
- Net ~0.6c per fill if spreads capture cleanly
- At 100+ fills/session → $0.60+/day vs $0.16/day at the
  wider filter

**Live evidence (within 30s of bounce)**: book-maker
registered n_markets=70, immediately resumed multi-market
quote acks (NYYBAL, MIAMIN, SFLAD all posting both bid+ask).
Higher fill rate restored.

## 2026-05-10 23:45 UTC — book-maker min_spread_cents 2 → 4 (cut adverse selection)

**Why**: post-stampede cleanup analysis showed 12 exit fills
(7 SL, 5 TP) all concentrated on 3 live-game tickers (STLSD,
PITSF, BUFMTL). The stop-losses fired because live-sports book
volatility (5-15¢ moves on game events) exceeds the 2¢ spread
the maker was capturing — adverse selection eats the position
within seconds of a fresh fill. -$2.59 realized on book-maker
today is dominated by these adverse-selection SLs.

**Change**: `min_spread_cents` 2 → 4 across all 70 tickers in
`~/.config/predigy/book-maker-config.json`. Now the strategy
only quotes on books with raw spread ≥6¢ (step-inside-by-1¢
each side leaves 4¢ between our quotes). Fewer fills but
each clean round-trip captures 4¢ vs the previous 2¢ —
which gives the strategy a wider margin against adverse
selection.

**Side effect**: fewer concurrent quotes means lower
in-flight pressure. The 200-cap stampede earlier today
won't recur at this filter level.

**Live evidence** (within 1 minute of the config-reload
mtime poll picking up the new value): 1 acked quote in
flight, down from the cap-pegged 200 earlier. Most books
on the 70-ticker universe are at 3-4¢ raw spread, below the
new threshold.

**Re-evaluate** after a few hours: if fill rate is too low
(no round-trips at all), consider:
- Loosening back to 3 (1¢ inner spread, more fills but more
  adverse selection)
- Adding volatility-aware widening (currently no such logic)
- Trimming to a smaller universe of consistently-wide-spread
  markets

## 2026-05-10 22:12 UTC — book-maker config trimmed to pre-game tickers only

**Why**: first 4 hours of the 94-ticker book-maker run produced
net **-$2.75 on book-maker**, offset by **+$1.71** of
variance-favorable internal-arb settlements (from before the
halt). Net account move: roughly flat. But the book-maker bleed
is structural and needs to stop.

**Diagnosis**: most of the -$2.75 came from a small number of
big single-name losses where the maker posted on both legs of a
2-leg family, ONE leg got hit, and we held it to settlement
where the game outcome dominated:

| Game | Entry | Loss |
|---|---|---:|
| MAY10 LAATOR-LAA | 30¢ | -$1.40 |
| MAY10 MINCLE-MIN | 40¢ | -$1.27 |
| MAY10 ATHBAL-BAL | 52¢ | -$0.48 |
| MAY10 ATHBAL-ATH | 50¢ | -$0.45 |

Spread captures on clean round-trips were *positive* (a dozen
+1¢ to +5¢ wins, ~+14¢ total), but the four game-outcome
losses overwhelmed them. **A maker isn't supposed to hold to
settlement.** That's the failure mode.

**Fix**: filtered all `26MAY10` (today-settles) tickers out of
`~/.config/predigy/book-maker-config.json`. Of the 94 expansion
tickers, 34 settled today and were the source of the losses.
The remaining 60 are May 11-13 games — pre-game books, less
volatile, more likely to round-trip cleanly long before
settlement.

Engine bounced; `book-maker: config loaded n_markets=60`.

**Follow-up** (NOT in this entry): auto-cancel-before-settlement
logic for the strategy. For each leg, cancel quotes at T-30min
before the market's `expected_expiration_time`. That would make
it safe to quote on same-day games too. Until then,
pre-game-only is the operating policy.

**Live evidence (post-trim)**: 60 markets registered, will
re-evaluate after 12h to confirm the bleed stops.

## 2026-05-10 18:34 UTC — internal-arb HALTED, book-maker takes the family alpha

**Why**: PR #40's anti-legging gate prevents internal-arb from
*re-firing* a family that already has exposure, but **the FIRST
fire of each new family still produces a naked single leg.** Live
evidence today: 7/7 first-fires across 7 different MLB games each
produced one naked YES position (cheap leg lifts at venue,
expensive leg IOC-cancels because liquidity at the offered price
isn't there). Average price ~42¢ — these are roughly EV-zero
directional bets minus 2¢ round-trip fees.

The fundamental issue: **IOC two-leg execution at retail latency
can't atomically fill both legs.** The same problem the strategy's
own doc-comment listed as "no partial-fill recovery" back at
shake-down.

**Halted**: commented out `PREDIGY_INTERNAL_ARB_CONFIG` in
`~/.zprofile`. Engine bounced; `internal-arb` no longer
registered. Last intent at 18:15:20 UTC; nothing fires after the
bounce. The 7 naked legs settle today as games end — they're
roughly EV-zero at entry price so the loss is bounded to ~2¢ of
fees per position.

**Replacement path**: book-maker now quotes on **94 tickers** —
every leg of every internal-arb sum-to-1 family, plus the
original 3 explicit test markets. Same alpha source (Kalshi
sum-to-1 family math) but harvested as a maker:

- For each leg: post a YES bid 1¢ inside the touch + a YES ask 1¢
  inside the touch, post_only=true, Tif=Gtc.
- When both legs of a 2-leg family fill long, the arb captures
  at settlement (exactly one leg wins → $1 from the winner; cost
  was bid_A + bid_B; profit = $1 - (bid_A + bid_B)).
- Each leg also independently captures the maker spread on
  unrelated takers.
- 0% Kalshi maker fee vs ~1¢/contract taker fee — fee economics
  are 4× better than the IOC version.

**Bug fix shipped during the rollout**: Kalshi rejects
`reduce_only=true` on non-IOC orders. The engine's venue_rest
SQL was setting `reduce_only` based on existing-position EXISTS
checks; for IOC takers this was meaningful, but for resting
GTC quotes it was both wrong semantically and venue-illegal.
Patched `build_create_request` to only forward `reduce_only`
when `tif == "ioc"`. After the patch: zero `reduce_only can
only be used with IoC orders` errors.

**Live evidence (first 30 minutes after switch)**:

- book-maker: **14 fills, +$0.04 realized, $0.02 fees → +$0.02 net**.
  Most fills got the 0% maker fee (otherwise fees would be ~14¢
  on 14 fills).
- 0 `reduce_only` errors after the patch.
- 8 net open positions (some round-trips closed, others holding
  to settlement).
- Account: $54.59 (down ~$0.40 from earlier snapshot, all
  natural mark-to-market drift).

**Dashboard updated**: active-tests banner reflects internal-arb
HALTED, book-maker LIVE on 94 tickers.

**Re-enable conditions for internal-arb**:
1. Kalshi institutional FIX access (true multi-leg package
   orders).
2. OR: a sub-millisecond IOC racing infrastructure (NY4
   colocation) that meaningfully reduces the cheap-leg-only
   fill rate. Not viable on a laptop.

The IOC version is **dead until then**. The maker variant
captures the same alpha better.

## 2026-05-10 05:55 UTC — book-maker LIVE (3 markets, post-only)

**Why**: per the 2026-05-10 strategy audit, maker mode is the
single biggest missing alpha source. Kalshi pays 0% maker fee on
standard binary markets vs the taker fee of ceil(0.07×N×P×(1-P)).
Whelan UCD 2026: pure-taker strategies are structurally
unprofitable. PR #45 shipped the strategy + supporting infra
(post_only flag, drain_pending_cancels). This entry documents
the live-deploy step.

**Config** at `~/.config/predigy/book-maker-config.json`:

| Ticker | Initial bid/ask | Spread |
|---|---|---|
| KXNHLGAME-26MAY12ANAVGK-ANA | 38/44 | 6¢ |
| KXMLBGAME-26MAY112005AZTEX-AZ | 47/52 | 5¢ |
| KXMLBGAME-26MAY111835NYYBAL-NYY | 55/59 | 4¢ |

Max inventory 2 contracts/market, quote size 1, min_spread 2¢.

**Per-strategy clamps** in `~/.zprofile`:
- `PREDIGY_BOOK_MAKER_MAX_NOTIONAL_CENTS=1000` ($10 cap)
- `PREDIGY_BOOK_MAKER_MAX_OPEN_CONTRACTS_PER_SIDE=10`
- `PREDIGY_BOOK_MAKER_MAX_DAILY_LOSS_CENTS=500`

**Validation evidence (first 10 minutes)**:
- Strategy registered (`n_markets=3` confirmed in supervisor log).
- Multiple post-only quotes posted; Kalshi accepted them
  (e.g., `venue_rest: order acked client_id=book-maker:...
  venue_order_id=...`). Confirms post-only is supported on
  standard MLB/NHL markets.
- Cancel + replace flow worked when book moved (e.g., A:59 →
  A:43 → cancel_requested as touches drifted).
- Strategy correctly stopped quoting when configured markets
  all tightened to ≤3¢ (step-inside spread would be <2¢
  min_spread).

**Bug fix applied during live test**:
The first 10 minutes surfaced a state-machine gap. When the
strategy queued a cancel for an order that had not yet reached
the venue (still in `submitted` / `acked` race), the row got
stuck in `cancel_requested` indefinitely (venue cancel-path
defers waiting for `venue_order_id`; nothing ever fills it
in). The strategy's `refresh_state_from_db` was still seeing
those rows via `Db::active_intents` and treating them as
"live at price X", causing redundant cancel-loops. Fix
landed: book-maker now skips `cancel_requested` rows in its
view, treating them as already-gone. Pre-existing orphans are
flagged by the engine reconciler as `orders_only_in_db`; a
cleanup-reaper for those is a follow-up.

**Open work**:
1. Watch first 24h on these 3 markets — confirm post-only
   stays honored, no unexpected fills, no quote-flapping
   pathologies.
2. Build an orphan-reaper for `cancel_requested` rows that
   never got a `venue_order_id` (currently they sit forever
   in DB as `orders_only_in_db` reconcile drift).
3. Once stable, expand from 3 → 10-20 markets; current 3 are
   conservative. The book-maker scales with breadth, not depth.
4. Per the strategic roadmap (Priority 2), add a Vegas-line
   curator to feed `model_p` for sports markets where Claude
   has no edge.

## 2026-05-09 22:00 UTC — paper-trader apparatus shipped

**Why**: the `stat` strategy (Claude Sonnet → `model_p` per
market) is the most promising path to **same-day-settling** alpha
in production — sports games, daily macro releases, news
markets, political events. Unlike `wx-stat`, which depends on
one fragile NBM model on one task, `stat` is flexible across
categories. But: enabling it without proven calibration is
exactly what blew up `wx-stat` (3W/8L overnight, halted in
companion PR). We need a way to prove model_p is positive-EV
after fees on a per-category basis BEFORE risking real cash.

**What landed**:

- New `paper_trades` table (migration `0004_paper_trades.sql`).
  Each row: ticker, side, entry-price-at-decision, model_p,
  edge-at-entry, fee, settlement_date. After settlement:
  outcome, paper_pnl_cents. Idempotent on
  (strategy, ticker, side, settlement_date).
- New `bin/predigy-paper-trader/` binary with three subcommands:
  - `record --rules-file ...` — read curator output, fetch live
    Kalshi orderbook for each ticker, compute the same edge the
    `stat-trader` strategy would, insert paper_trades when it
    clears `min_edge_cents`. Mirrors `stat-trader::derive_ask`
    (YES ask = 100 - best NO bid; NO ask = 100 - best YES bid)
    and the after-fee edge logic in `build_intent`. Falls back
    to `market_detail.expected_expiration_time` /
    `close_time` when curator omits `settlement_date` (which
    the current curator output does for many rules).
  - `reconcile` — for each unsettled paper_trade past its
    settlement_date, fetch market_detail and (if resolved) fill
    in `settlement_outcome` + `paper_pnl_cents`.
  - `report --days 14` — aggregate metrics by source / category /
    settlement_date: n_trades, win rate, after-fee EV per trade,
    Brier vs side-adjusted prediction.
- `com.predigy.paper-trader.plist` runs every 5 minutes
  (record + reconcile back-to-back). Cron is **observation-only**
  — never submits orders.
- Wired into `deploy/scripts/install-launchd.sh` preflight + load
  loop.

**First live tick** (right after deploy): 17 rules in current
`stat-rules.json`. Of those, 2 had expired settlement dates
(skipped) and 15 were below the after-fee edge threshold. Zero
paper_trades inserted on first tick. **This is itself
informative**: at the current model_p outputs from Claude
Sonnet, the curator's predictions don't have positive
after-fee edge against live touch prices in the econ
categories that dominate the current rules file. The market is
pricing those events more confidently than our model. We
either need:
1. A different prompting strategy for Claude (more
   non-econ markets — sports, politics)
2. Markets with higher implied uncertainty where the model has
   genuine edge
3. To accept that Claude doesn't beat efficient pricing on
   threshold econ markets and focus stat elsewhere

**Re-enable conditions for live `stat` trading** (now that there
is a ticking measurement apparatus):

1. `predigy-paper-trader report --days 30` shows ≥30 paper
   trades settled on a given (category, source) bucket.
2. After-fee EV per trade is positive in that bucket.
3. Brier score in that bucket is better than 0.25 (the
   coin-flip baseline).
4. The category-specific bucket gets a separate live-rules file
   (or a `rules.category` filter in the engine) so we don't
   enable the bad categories along with the good.

This is the apparatus. The model_p quality is now measured
continuously — once a category proves itself, we promote.

## 2026-05-09 17:45 UTC — wx-stat HALTED (structurally negative-EV)

**Why**: overnight settlement of wx-stat positions delivered the
clean evidence we'd been waiting for. **The strategy is
structurally losing.** Halting before more capital bleeds.

**11 cleanly-settled wx-stat trades since the 2026-05-07 force-flatten** (window
2026-05-08 19:00 UTC → 2026-05-09 17:00 UTC):

| | Wins | Losses | Realized |
|---|---:|---:|---:|
| YES side (5 trades) | 0 | 5 | **-$18.09** |
| NO side (6 trades) | 3 | 3 | +$2.11 |
| **Total** | **3** | **8** | **-$15.98** + $1.23 fees = **-$17.21** |

Worst losses concentrated on YES-side overnight low temperatures
that didn't break the threshold:

- KXLOWTLAX-T59 yes @45 × 20 → -$9.00 (LAX low actual was
  below 59°F threshold — predicted YES was wrong)
- KXLOWTOKC-T54 yes @57 × 12 → -$6.84 (OKC low above threshold)
- KXHIGHTHOU-T76 yes @12 × 15 → -$1.80
- KXHIGHTSEA-T65 no @22 × 10 → -$2.20

**Account impact**: total liquid moved $73.87 (last night) →
**$55.48** (now). 18.4% of the drop is wx-stat realized; the rest
is the natural mark-to-market move on positions that closed at
$0 vs. an aspirational mark.

**Action**: commented out `PREDIGY_WX_STAT_RULE_FILE` in
`~/.zprofile`. Engine bounced; 5 strategies registered (no
wx-stat). Existing wx-stat positions are all closed; nothing to
flatten.

**Re-enable conditions** (per `docs/AUDIT_2026-05-08.md` updated
2026-05-09):

1. NBM-quantile model demonstrates positive after-fee EV in a
   **paper-trading run** (shadow only, no real cash) over ≥30
   trades.
2. Brier score better than the naive
   "always predict 50%" baseline.
3. YES-side bias root-caused. The current data shows YES is the
   loser. Either the model systematically over-predicts the long
   side, or the curator's threshold rounding is off, or
   intraday weather drift makes morning predictions stale by
   close. None of those are diagnosed yet.

**What this leaves running**: the math-proven arbs (`internal-arb`,
`implication-arb`) plus settlement, cross-arb, stat (1 residual
rule). Implication-arb has ~$1+ of locked floor profit waiting on
PAYROLLS calendar dates (June–October).

## 2026-05-09 03:30 UTC — calibration cron 1h → 15m

**Why**: the engine's internal `reconcile_positions` runs every
minute and only OBSERVES drift — it doesn't auto-resolve. The
hourly `predigy-calibration reconcile-venue-flat --write` is the
auto-resolver. That meant every settled-but-DB-still-open phantom
generated up to 60 min of "position_mismatches" drift warnings
before getting cleaned up.

The phantom problem self-fixed at 03:21 UTC when the hourly cron
ran and closed all 7 stale rows from tonight's MLB/NHL games
(same families described in the 01:53 UTC anti-legging entry
below):

| Game | Outcome | Realized delta |
|---|---|---:|
| COLPHI-COL YES @41 | COL won | +$4.13 |
| COLPHI-PHI YES @36 | PHI lost | -$2.52 |
| MINCLE-CLE YES @89 | CLE won | +$0.11 |
| DETKC-KC YES @17 | KC won | +$4.98 |
| SEACWS-CWS YES @27 | CWS lost | -$2.97 |
| CHCTEX-TEX YES @19 | TEX lost | -$0.19 |
| MTLBUF-BUF YES @1 | BUF lost | -$0.18 |
| **Net** | | **+$3.36** |

The legging incident was actually neutral-to-positive on
settlement — three underdogs we'd accumulated naked YES on
actually won.

**Changed**: `deploy/macos/com.predigy.calibration.plist`
`StartInterval` 3600 → 900. Now the engine reconciler's drift
warnings clear within ≤15 min of a settlement instead of ≤60 min.
Reloaded via `launchctl bootout` + `bootstrap`; the script
(sync-settlements + reconcile-venue-flat + reports) is light
enough to run 4×/hour without strain.

**Deferred**: a tighter long-term fix would have the engine's own
`reconcile_positions` do the close-against-settled-outcome work
itself, eliminating the duplication of "two reconcilers, one
observation-only and one writing." Cadence tighten is the
pragmatic v1.

## 2026-05-09 01:53 UTC — internal-arb anti-legging gate

**Why**: the post-cap-raise audit at ~01:25 UTC found
internal-arb was systematically legging in production. Across the
prior 30 min, 9 families were re-firing every minute (tighter than
the 60s cooldown should have allowed because the cooldown timer is
per-family but there's no "still working a prior group" gate). The
cheap-leg IOC kept lifting (BUF@2, BAL@8, KC@18 etc.), the
expensive-leg IOC kept getting cancelled by the venue (no
liquidity at the offered price), and we accumulated naked
underdog YES contracts: 9 BUF, 19 BAL, 6 KC, 11 CWS, 18 MTL — none
of these are arbs, all are coin-flip directional EV-zero lottery
tickets that bleed double fees per cycle.

The strategy doc at the top of `crates/strategies/internal-arb`
explicitly listed "no partial-fill recovery" as a known gap. The
cap raise put the binding constraint on opportunity stacking
rather than on firing rate, which is what surfaced it.

**Fix landed**:

- `internal-arb` now maintains an `exposed_tickers: HashSet<String>`
  rebuilt from `Db::open_positions(strategy)` +
  `Db::active_intents(strategy)` on every BookUpdate that lands on
  a configured ticker (matching `implication-arb`'s already-shipped
  inventory-refresh pattern at `crates/strategies/implication-arb/src/lib.rs:731`).
- `evaluate_family` returns `None` if any leg ticker is in the set.
- After enqueueing a leg group in `on_event`, the about-to-submit
  legs are reserved into `exposed_tickers` so a second BookUpdate
  in the same loop tick can't double-fire.
- New test `skips_family_when_any_leg_has_existing_exposure` proves
  the gate. All 11 unit tests pass.

**Operational sequence**:

1. 01:30–01:44 UTC — diagnosed legging via fills query
2. 01:45 UTC — commented out `PREDIGY_INTERNAL_ARB_CONFIG` in
   `~/.zprofile` and bounced engine to halt the bleed
3. ~01:50 UTC — fix coded + tested
4. 01:52:56 UTC — re-enabled env var, bounced engine
5. 01:53:01 UTC — internal-arb registered (122 markets, 61
   families); first exposure refresh logged
   `n_exposed_tickers=11`
6. Zero new internal-arb intents post-bounce; 52 unlegged families
   remain free to fire when fresh edge appears

**Naked positions left to settle naturally**: ~$1.50 entered cost
across 9 sports families (all settle tonight 2026-05-08
21:40–22:15 PDT). EV ≈ 0 at entry price; selling now would lock a
small loss + double fees. Holding. (Confirmed +$3.36 net at
settlement — see the 03:30 UTC entry above.)

**Deliberate non-goals for this PR**:
- The OMS-side cancellation cascade still only fires on
  `Rejected | Expired` (`bin/predigy-engine/src/oms_db.rs:679`).
  Extending it to fire on `Filled` to cancel siblings the
  *moment* the cheap leg lifts would prevent the venue ever
  seeing the unhedged sibling. Strategy-side gate is enough to
  stop the stacking pathology; OMS cascade extension is a
  follow-up.
- The gate is conservative — it blocks **balanced** re-fires too
  (COLPHI 7+7 is a real arb pair we could scale further). Refining
  to "block only when unbalanced" is a follow-up if throughput
  proves constraining.

## 2026-05-09 01:05 UTC — arb scaling raise

**Why**: math-proven arbs (`internal-arb`, `implication-arb`) lock
cash spreads when they fire. They don't need calibration evidence
to scale; they need throughput. After `arb-config-curator` shipped
yesterday and expanded the universe (12 → 248 implication pairs;
2 → 60 internal-arb families), capital caps became the binding
constraint, not opportunity supply.

**Changed in `~/.zprofile`** (single source of env truth, read by
launchd via `zsh -lc`):

| Var | Was | Now |
|---|---:|---:|
| `PREDIGY_MAX_NOTIONAL_CENTS` | 4000 ($40/strat) | 8000 ($80/strat) |
| `PREDIGY_MAX_GLOBAL_NOTIONAL_CENTS` | 12000 ($120) | 20000 ($200) |
| `PREDIGY_MAX_CONTRACTS_PER_SIDE` | 20 | 100 |
| `PREDIGY_MAX_DAILY_LOSS_CENTS` | 1000 ($10) | 2000 ($20) |
| `PREDIGY_MAX_IN_FLIGHT` | 40 | 80 |

**Pinned wx-stat at the prior shake-down sizing** so its
calibration-unproven sizing doesn't auto-scale with the global
raise. New per-strategy clamps:
- `PREDIGY_WX_STAT_MAX_NOTIONAL_CENTS=4000`
- `PREDIGY_WX_STAT_MAX_OPEN_CONTRACTS_PER_SIDE=20`
- `PREDIGY_WX_STAT_MAX_DAILY_LOSS_CENTS=1000`

Lift these once Brier/log-loss reports show wx-stat positive-EV
after fees over ≥30 closed unforced trades.

**Deploy**: engine bootstrapped fresh; 6 strategies registered;
supervisor logs confirm. Account ~$83 cash.

**Companion doc**: see plans/`2026-05-08-news-trader-implementation.md`
for the parallel news-trader effort (independent alpha source,
not yet implemented).

## 2026-05-09 01:00 UTC — opportunity-scanner cadence 15m → 5m

`com.predigy.opportunity-scanner.plist` `StartInterval` 900 → 300.
Faster candidate observation refresh feeds the
`arb-config-curator`'s 30m tick more current data; reduces window
during which a newly-active event family goes unmodelled.

## 2026-05-08 ~23:30 UTC — arb-config-curator shipped (PR #38)

**Why**: implication-arb / internal-arb config files were full of
already-settled tickers (KXPAYROLLS-26APR, KXMLBGAME-26MAY06,
etc.). The strategies' read-side hot-reload worked but no daemon
was writing fresh configs.

**What landed**:
- New `bin/arb-config-curator/` validates each pair/family
  against Kalshi `status=open` snapshot, drops settled, seeds
  new entries from active monotonic ladders (`KXPAYROLLS`,
  `KXTORNADO`, `KXECONSTATU3`, `KXEMPLOYRATE`) and 2-leg event
  families (`KXMLBGAME`, `KXNBASERIES`, `KXNHLGAME`).
- `com.predigy.arb-config-curate.plist` runs every 30m + at load.
- One-shot result: implication-arb 12 → 248 pairs; internal-arb
  2 → 60 families.

**Deliberate non-goal**: no auto-discovery of new implication
*patterns* from observation data alone. Patterns
(parent⊃child) must be logically true; statistical inference
from co-movement risks adding wrong implications. New patterns
added by editing the `MONOTONIC_LADDER_SERIES` /
`TWO_LEG_FAMILY_SERIES` const lists in the binary.

## 2026-05-08 ~04:00 UTC — audit-tightening landed (PR #37)

Acted on the 2026-05-08 mechanism audit (see
`docs/AUDIT_2026-05-08.md`). Three code changes + four
operational changes that moved the surviving strategies onto
positive-EV ground after Kalshi's actual fee structure was
verified (`ceil(0.07 × N × P × (1-P))` cents, 1¢ floor).

- `cross-arb` `min_edge_cents` 1 → 3 (fee floor at typical
  prices is 1-2¢; firing at 1¢ raw was buying a fee-loss)
- `wx-stat` new `min_ask_cents=5` + `max_notional_per_fire_cents=500`
- `stat` same `min_ask_cents=5` gate
- 4 strategies disabled in `~/.zprofile` (`book-imbalance`,
  `variance-fade`, `news-trader`, `latency`)
- 18 `venue-reconcile` phantom rows purged from cap accounting
- 9 stat econ rules disabled pending recalibration

Live fleet narrowed 10 → 6 strategies. Per-strategy verdicts
and re-enable conditions in `docs/AUDIT_2026-05-08.md`.

## 2026-05-07 evening — force-flatten of 52 positions

Operator-initiated. ~52 open positions at flat-everything time;
32 flattened via best-effort IOC, 20 stuck in empty short-dated
weather books (auto-settled May 7-8 naturally).

**Important**: realized P&L data from this period is biased
*downward* — many positions were dumped at 1¢ that would have
settled fair at $1. `wx-stat` and parts of `stat` particularly
affected. Discount the 7-day realized rollup when using it as
evidence.

## 2026-05-07 ~07:45 UTC — engine cutover

4 legacy daemons (`latency-trader`, `settlement-trader`,
`stat-trader`, `cross-arb-trader`) retired. Single
`predigy-engine` binary owns OMS, market data, exec, all
strategies.

Cid-period bug surfaced live during cutover (Kalshi V2 rejects
`client_order_id` containing `.`); fixed in
`engine_core::intent::cid_safe_ticker(...)` (commit `0c05c40`),
rebuild + restart in ~5 minutes.

---

## How to add an entry

1. Append to the top of the timeline (newest first).
2. Date the change in UTC.
3. State *why* in one paragraph — the durable signal.
4. List concrete deltas (env vars, plist values, code commits).
5. If reverting: add a new entry referencing the original; do
   not edit the original.
6. Cross-reference companion docs (`SESSIONS.md`, `AUDIT_*.md`,
   `plans/*.md`).
