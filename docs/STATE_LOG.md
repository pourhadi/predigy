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
