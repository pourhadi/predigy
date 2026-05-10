# Strategic Roadmap — 2026-05-10

> **Audit-driven plan** for what predigy should be, ordered by
> expected risk-adjusted return. Written 2026-05-10 after a
> ~50% account drawdown caused by wx-stat being structurally
> negative-EV. The audit (this document's first half) is the
> "why" — the second half is the "what to build, in order."
>
> The current state of the system is **6 strategies live, 1
> halted, 4 disabled**. Lifetime P&L tells the story:
>
> | Strategy | Lifetime P&L | Verdict |
> |---|---:|---|
> | internal-arb | **+$2.20** | KEEP — the only winner |
> | implication-arb | -$1.78 closed (+$1+ floor on opens) | KEEP, but settles months out |
> | stat | -$6.32 | DEFER pending paper-trader evidence |
> | wx-stat | -$17.46 | HALTED, structurally bad |
> | cross-arb | $0.00 (1 trade ever) | KILL — frictions exceed edge |
> | settlement | n/a (0 fires) | KEEP — tail-event harvester |
> | book-imbalance / variance-fade / latency / news-trader | — | DISABLED, correctly |

## Part 1 — Audit findings

### 1.1 The single biggest miss: maker mode

**Confirmed via Whelan 2026 (UCD) + Kalshi fee schedule + pm.wiki**:
Kalshi's standard binary markets carry a **0% maker fee** vs.
the `ceil(0.07 × N × P × (1-P))` taker fee. Premium markets
(NFL championship, NBA Finals, presidential elections) charge a
flat 0.25% maker, still ~4× cheaper than taking. Whelan
explicitly: *"the fee structure makes pure-taker strategies
unprofitable; you must be a maker to survive at scale."*

**Predigy's entire OMS is built for taker flow.** All 10
strategies emit `Tif::Ioc` limit orders only. No strategy posts
resting bids/asks. We're paying ~1¢ on every contract round-trip
that a maker would post for free.

This is the largest gap by a wide margin. It changes the
arithmetic of every other strategy: an internal-arb pair locking
3¢ edge as a taker is 3¢ - 2¢ fees = **1¢ net**. As a maker
posting both legs and getting hit, 3¢ - 0¢ fees = **3¢ net**, a
3× yield improvement before we even build new alpha.

### 1.2 The capital base is too small for the goal

The user's stated objective ($1000 by next week from $55) is
**mathematically unreachable**. 3.4 doublings in 7 days = 18%
per day with zero loss days. No clean strategy on Kalshi
delivers this. Even a perfect maker capturing 2¢/round-trip ×
100 round-trips/day = $2/day = ~3.6%/day, which doubles every
20 days, not every 1.

**We need to either**:
1. Add capital
2. Take a single concentrated directional gamble (not strategy)
3. Reset the goal — $250 in 30 days is achievable; $1000 in 7
   days is not.

The strategies and infrastructure built so far are correct for
**scale**, not for **micro-capital growth**. They will compound
when the account is larger; right now the variance dominates.

### 1.3 Wrong direction on longshot bias

The most-cited persistent statistical alpha on prediction
markets is the **longshot bias**: retail systematically overpays
for long shots (buying YES at 5¢ when fair is 1-2¢, lottery
ticket psychology). The natural trade is **selling YES on cheap
longshots** (i.e., buying NO at 95-99¢ on cheap underdog tickets).

**predigy's internal-arb buys YES on every leg of an event
family.** When the strategy legs (cheap leg fills, expensive
leg cancels at venue), we end up holding naked YES on the cheap
leg — which is the *long shot*. Yesterday's COLPHI/MTLBUF/etc.
legging incident was statistically expected to lose money but
got lucky (+$3.36 on coin flips that broke our way).

A symmetric strategy that *sells* YES on the cheap leg via NO
buys would harvest the longshot premium directly.

### 1.4 No use of Vegas implied probabilities

Sportsbook moneylines (after vig removal) are well-calibrated
for game outcomes — the sharp books (Pinnacle, Circa) are the
gold standard. We could pull these for free and use them as
`model_p` for sports markets, replacing Claude (which isn't
calibrated and never will be on game-by-game outcomes).

**This is a 2-3 day build with a known-positive expected
value.** Free model.

### 1.5 cross-arb is dead

Confirmed via research: Kalshi↔Polymarket arb gaps need >3-5%
to survive frictions (cross-funding lag, withdrawal delays, KYC
walls, tax mismatch, gas on Polygon). predigy's
`min_edge_cents=3` is below the friction floor.

The strategy hasn't fired in 7 days for a reason. **Kill it.**
Save the Anthropic budget on the curator.

### 1.6 Strategy infrastructure complexity vs. capital is mismatched

Predigy has:
- 6 active strategies
- A paper-trading apparatus
- Hourly calibration cron with venue-flat reconciler
- Anti-legging gate
- Multi-leg-group atomicity in OMS
- 248 implication-arb pairs across 5 PAYROLLS months + ECONSTATU3
- Auto-refreshing arb-config-curator scanning Kalshi every 30m

…on $55 of capital. **The infrastructure is paying for
hypothetical scale, not actual yield.** Every piece adds future
scalability but right now nothing matters much because the
positions are tiny.

This isn't wrong — building the infrastructure first is the
right order. But it means **we should not be surprised that
yield is tiny right now**, and we should not interpret tiny
yield as strategies being broken.

### 1.7 Where the actual edge lives (and we've under-invested)

Per QuantPedia + Whelan paper + retail trader posts, the real
profitable lanes on Kalshi:

| Lane | Realistic edge | Predigy state |
|---|---:|---|
| **Market making** | 1-3¢/round-trip @ 0% fee | **NOT BUILT** |
| Longshot-bias harvesting | 1-3¢/contract systematic | **WRONG DIRECTION** |
| Settlement event-driven | 1-2¢ on late drift | Built but dormant |
| Cross-venue arb | 0.5-3% per round-trip, 2-7s window | **Wrong threshold (kill it)** |
| Statistical model_p (Claude) | unproven; likely zero | Halted/paper-trading |

**The only persistent retail-accessible alpha sources we
*haven't* implemented are exactly the highest-expected-return
ones.** That's the gap.

## Part 2 — What to build, in priority order

Each item carries an effort estimate and an expected-yield
estimate at current ($55) and target ($500) capital scales.
Yield estimates are *rough* — within 2× either direction is
acceptable for prioritization.

### Priority 1 — `book-maker` strategy (MOST IMPORTANT)

**Effort**: 1-2 weeks for MVP, 3-4 weeks for production-grade.

**Yield estimate at $55**: $0.50-2.00/day. At $500: $5-20/day.
At full scale (100+ markets quoted): $50-200/day per Whelan's
analysis.

**Spec**:
- Two-sided post-only quoting on a curated market set (start with
  1, scale to 5, then 20).
- Each quote tier: post YES bid at `best_yes_bid + 1¢` and YES
  ask at `best_yes_ask - 1¢` (i.e., narrow the spread by 1¢
  on each side; collect 1¢ on each round-trip when both fill).
- Per-market inventory cap: e.g., max 10 contracts long or short
  in any one market.
- **Inventory skew**: when long 5 contracts, raise the bid even
  more aggressively (less likely to add inventory) and drop the
  ask 1¢ more (more likely to flatten). Stoikov-style.
- **Re-quote on book move**: cancel + repost when the touch
  moves >2¢ from our quote. This is the cancel-replace flow
  that requires a new architectural primitive (see infra-1
  below).
- **Cancel-on-news**: when book widens >5¢ in a single tick,
  cancel both quotes (informed flow probably hitting). Re-post
  after 10s if book restabilizes.

**Why first**: Highest expected return per dollar of capital
deployed, AND it directly addresses the structural fee
disadvantage flagged by the audit. It's also the lane where
prediction-market quants reliably make money.

**Risk**: adverse selection. If we're slower to update than
informed traders, our quotes get hit on stale prices. Mitigated
by: post-only flag (no taker risk on miss), small qty per
quote (1-2 contracts), tight cancel-on-move latency.

**Dependencies (infrastructure)**:
- **infra-1** (cancel mechanism): Strategy trait extension —
  `drain_pending_cancels(&mut self) -> Vec<String>` returning
  client_ids the strategy wants cancelled. Supervisor calls
  `Oms::cancel()` on each.
- **infra-2** (post_only flag): add `post_only: bool` to Intent
  (default false). Engine REST submitter passes through to
  Kalshi `CreateOrderRequest.post_only`.
- **infra-3** (resting-order DB schema): the existing intents
  table already supports GTC; just need to query active orders
  by `(strategy, ticker, side)` for the maker's tick logic.

These 3 infra changes are required to even start the maker
strategy. They unblock other strategies too (e.g., a future
"settlement-maker" that posts asks at $1.

### Priority 2 — Pull Vegas closing lines for sports markets

**Effort**: 2-3 days.

**Yield estimate at $55**: $0.20-1.00/day on sports stat. At
$500: $2-10/day.

**Spec**:
- New binary `sports-line-curator`: pulls moneylines from a
  free public source (the-odds-api free tier, or
  pinnacle.com's public feed if it can be scraped legally) for
  MLB, NBA, NHL games matching predigy's traded universe.
- Converts moneylines to implied probabilities via standard
  vig-removal:
  - For two-way moneyline (-150 / +130): convert each to
    decimal_odds → implied prob → divide by total to remove vig
  - Output: `{ticker, model_p, generated_at_utc}` matching the
    stat-curator output schema
- Writes to `~/.config/predigy/sports-lines.json` on a 5-minute
  cron (or on Kalshi book-update if we want tighter coupling).
- Paper-trader records → if positive after-fee EV over ≥30
  trades, promote a sports-lines-trader to live.

**Why second**: Free, calibrated, well-known to be alpha.
Replaces Claude entirely for the sports lane. Doesn't need
maker mode (taker on sports edge can still net positive when
the model is right).

**Risk**: Vegas lines may already be priced into Kalshi by
quants who do this. Paper-trading first will tell us within ~30
trades whether the edge is still there.

### Priority 3 — Pivot internal-arb to harvest longshot bias (or
add a complementary `longshot-fade` strategy)

**Effort**: 3-5 days for the new strategy, less for a
refactor.

**Yield estimate at $55**: $0.50-2.00/day. At $500: $5-20/day.

**Spec**:
- Either add a flag to internal-arb to fire NO instead of YES on
  the cheap leg of 2-leg families, OR build a sibling strategy
  `longshot-fade` that runs alongside.
- Trigger: any binary leg of a 2-leg event family at
  `yes_ask ≤ 5¢` AND `no_ask ≥ 95¢`. Buy NO (sell YES).
- Per-event cap: max 5 contracts per long-shot leg.
- Hold to settlement (most longshot underdogs lose; collect $1
  on NO contracts at settlement).

**Why third**: Real persistent alpha source per literature.
Doesn't require maker infrastructure (can run as taker;
selling NO at 5¢ has decent fee economics due to the fee
structure favoring rails).

**Risk**: tail risk on the 5-10% of longshot wins. Mitigated
by per-event cap. Also Kalshi's deep-OOM markets can have very
thin books — slippage if size grows.

### Priority 4 — Kill cross-arb

**Effort**: 30 minutes.

**Yield delta**: zero current yield (1 trade ever, $0 P&L);
saving ~$0.05/day in Anthropic curator cost.

**Spec**:
- Comment out `PREDIGY_CROSS_ARB_PAIR_FILE` in `~/.zprofile`.
- Disable `com.predigy.cross-arb-curate.plist` via
  `launchctl bootout` + `launchctl disable`.
- Document in STATE_LOG and AUDIT.

**Why**: confirmed structurally unprofitable at retail scale.
Frees curator budget and reduces ops surface.

### Priority 5 — News-trader Phase 1 (scheduled releases)

**Effort**: 1-2 weeks.

**Yield estimate**: highly variable. On a release day (CPI, NFP,
FOMC) potentially $10-50 in 5 minutes if calibration is right.
Off-day: zero. Maybe 4-8 release days per month.

**Spec** (already drafted in
`plans/2026-05-08-news-trader-implementation.md`):
- BLS / Fed / Treasury RSS scraper firing on known release
  schedules.
- Sub-1s parser of release content into Kalshi-relevant
  outcomes (e.g., "NFP +147k" → KXPAYROLLS thresholds).
- Compare to current Kalshi prices, fire IOC where edge clears.

**Why fifth**: Scheduled releases are a known-time, known-source
event. Nobody on Kalshi has a meaningful latency advantage at
this scale (BLS/Fed releases drop publicly at exact times). The
edge is parser quality, not latency.

**Risk**: parser bugs → misinterpret release → fire wrong side.
Mitigate with paper-trading first.

### Priority 6 — Settlement-maker (post asks at $0.99 near
settlement)

**Effort**: 1 week (depends on Priority 1's maker infrastructure
landing first).

**Yield estimate at $55**: $0.20-1.00/day. At $500: $2-10/day.

**Spec**:
- For markets in the last 30 minutes of trading where current
  ask ≤ 96¢, post a YES ask at 99¢ as a maker.
- If hit, we sell at 99¢ on something that's nearly certain to
  settle YES at $1. Round-trip: 1¢ profit minus 0¢ maker fee =
  1¢ net.
- Asymmetric risk: if the market unexpectedly resolves NO, we
  paid 99¢ for a contract worth $0 → -99¢. Need very high
  conviction filter on which markets to do this.
- Filter: only fire on markets where current bid ≥ 95¢ (market
  consensus is high).

**Why sixth**: Builds on the maker infrastructure. Different
from existing settlement strategy (which is a TAKER buying at
93¢). Maker version is even more profitable when it hits.

### Priority 7 — Reset the $1000-in-a-week goal

**Not a build item. A conversation.**

The user should explicitly accept that $1000 in 7 days is not
achievable from $55. The strategies above can compound to:
- ~$80 in 7 days (paper-trader proven directional + arb yield)
- ~$200 in 30 days (maker mode + arbs + sports model)
- ~$1000 in 90 days (all of above scaling, plus capital reinvestment)

If $1000 in 7 days is non-negotiable, the only path is a single
concentrated directional bet ($50 of $55 on one outcome with
high subjective confidence). That's gambling, not strategy.
predigy doesn't help with that.

## Part 3 — Architectural prerequisites for maker mode

Before we can build the `book-maker` strategy, three engine-side
changes must land:

### Infra-1: Strategy → OMS cancel mechanism

**Current state**: strategies emit `Vec<Intent>` from
`on_event()`. They have no way to cancel an existing order.
The `Oms::cancel()` method exists but is only callable from
the supervisor, not from strategy code.

**Change**:
- Add `Strategy::drain_pending_cancels(&mut self) -> Vec<String>`
  returning client_ids the strategy wants cancelled. Default
  implementation returns empty.
- In supervisor, after calling `on_event()` and submitting any
  intents, call `drain_pending_cancels()` and route each id to
  `Oms::cancel(client_id)`.
- The strategy holds the canonical list of "orders I'd like to
  cancel" in its own state.

**Why this shape**: matches the existing `drain_pending_groups()`
pattern for multi-leg arbs. Strategies don't get direct OMS
access (still goes through the supervisor) but they can express
their intent to cancel.

### Infra-2: post_only flag on Intent

**Current state**: `Intent` has no field for post-only / maker-
only orders. Kalshi's `CreateOrderRequest` supports it but the
engine never passes it through.

**Change**:
- Add `post_only: bool` to Intent (default false).
- In the engine REST submitter, pass `post_only` through to
  `CreateOrderRequest.post_only`.
- Existing strategies emit intents with `post_only: false`
  (default) — no behavior change.
- The maker strategy sets `post_only: true` so its quotes
  cannot accidentally take liquidity.

**Why this shape**: surgical addition. Doesn't disturb existing
code paths. Provides exactly the safety the maker needs (a
maker that accidentally takes pays the full taker fee, which
defeats the entire economic case for maker mode).

### Infra-3: Active-order query helper for maker tick logic

**Current state**: `Db::active_intents(strategy)` exists and
returns non-terminal intents. This is sufficient.

**Change**: none required, but document the query pattern in
the maker strategy's docstring so future strategies follow it:
- On each tick:
  1. Fetch active intents for this strategy
  2. Compute desired quote price for each (market, side)
  3. For each existing intent: if `(market, side, price)` ≠
     desired, add `client_id` to pending_cancels
  4. For each (market, side) without an active intent at the
     desired price, emit a new Intent

## Part 4 — The book-maker MVP, in detail

This section is the spec the implementation should follow.

### 4.1 Crate

`crates/strategies/book-maker/` — sibling to the other
strategies. Public surface:

- `BookMakerStrategy: Strategy` — main implementation
- `BookMakerConfig` — runtime parameters
- `MakerMarket` — per-market config (ticker + size + caps)

### 4.2 Config file

`~/.config/predigy/book-maker-config.json`:

```json
{
  "markets": [
    {
      "ticker": "KXMLBGAME-26MAY101335ATHBAL-ATH",
      "max_inventory_contracts": 5,
      "quote_size": 1,
      "min_spread_cents": 2
    }
  ],
  "global": {
    "max_total_inventory_cents": 2000,
    "max_quote_age_seconds": 30,
    "cancel_on_book_widening_cents": 5,
    "inventory_skew_cents_per_contract": 1
  }
}
```

The arb-config-curator pattern (PR #38) can be extended later to
populate this file from heuristics on which markets are
"makeable" — wide bid-ask, decent volume, no recent extreme
moves.

### 4.3 Quote computation

For each (ticker, side) where we don't have a position above
`max_inventory_contracts`:

1. Read current touch from the book cache:
   - `yes_bid_cents`, `yes_ask_cents`
2. Compute fair quote:
   - YES bid: `yes_bid_cents + 1` (i.e., 1¢ inside the current
     best bid)
   - YES ask: `yes_ask_cents - 1`
3. Apply inventory skew:
   - If we're long N contracts: bid -= N × `skew_per_contract`,
     ask -= N × `skew_per_contract` (both shift down — we want
     to shed inventory)
   - If short: shift both up
4. Clamp to legal range: bid ≥ 1, ask ≤ 99, ask > bid + 1
5. If clamped quote violates `min_spread_cents`, don't quote.

### 4.4 Re-quote logic

On every BookUpdate event for a configured ticker:

1. Read active intents for this strategy on this market.
2. Compute desired quote (per 4.3).
3. For each existing intent:
   - If price matches desired exactly: keep.
   - If price differs: add `client_id` to pending_cancels.
4. For each side (yes/no) without a desired-price intent: emit
   a new Intent with `tif=Gtc, post_only=true`.

### 4.5 Inventory tracking

On each BookUpdate (or each tick):

1. `state.db.open_positions(Some("book-maker"))` to get current
   inventory.
2. Compute net qty per ticker.
3. Apply skew formula above.

### 4.6 Cancel-on-widening

If the bid-ask spread on a configured ticker widens by more
than `cancel_on_book_widening_cents` from the moving baseline,
cancel both quotes immediately and don't re-post for 10s.

This is the simplest news/informed-flow defense. More
sophisticated would be: detect trade prints in the unfavorable
direction, mid-velocity exceeding a threshold, etc. — but for
MVP a spread-widening trigger is enough.

### 4.7 Risk caps

Per global engine caps (already in place):
- `PREDIGY_MAX_NOTIONAL_CENTS=8000` per strategy ($80)
- `PREDIGY_MAX_CONTRACTS_PER_SIDE=100`
- `PREDIGY_MAX_DAILY_LOSS_CENTS=2000`

For book-maker specifically (per-strategy clamp env vars,
matching the wx-stat pattern):
- `PREDIGY_BOOK_MAKER_MAX_NOTIONAL_CENTS=2000` ($20) — start
  small while we validate
- `PREDIGY_BOOK_MAKER_MAX_OPEN_CONTRACTS_PER_SIDE=20`
- `PREDIGY_BOOK_MAKER_MAX_DAILY_LOSS_CENTS=500`

### 4.8 Observability

- Metrics: per-ticker quote-fill rate, average inventory age,
  cancel-and-replace rate (a high rate means our quotes are
  getting stale faster than we can update — bad sign).
- Logs at INFO: every quote post, every fill, every cancel.
- Logs at WARN: every inventory-cap rejection.

### 4.9 Test plan

1. **Unit tests**: pure-function tests for quote computation
   (skew application, clamping, min-spread guard).
2. **Integration test**: spin up an in-memory OMS, feed it a
   sequence of BookUpdates, assert the strategy emits/cancels
   the expected intents.
3. **Live shadow mode**: deploy with a small qty (1 contract)
   and `PREDIGY_BOOK_MAKER_SHADOW_ONLY=true` env that converts
   all emitted intents to no-ops, just logging what *would*
   have been posted. Run for 24h to confirm the strategy
   logic matches reality.
4. **Live production mode**: turn off shadow, set 1 market and
   1-contract size. Watch fills and inventory for 48h.
5. **Scale**: if 24h shows positive expected value with no
   pathologies, expand to 5 markets, then 20.

## Part 5 — Tracking progress

Each priority above is a separate task in the task system.
This file is the durable plan; the tasks are the operational
checklist.

When a priority completes, move it to a "completed" section
at the bottom of this doc with a one-line outcome note.

When new priorities surface (e.g., "settlement-maker" once
maker infrastructure is proven), append to part 2.

## Open questions / unknowns

- **Does Kalshi accept post_only on all binary markets**, or
  only on premium? Test with a single small order before
  building the strategy on the assumption.
- **What's the realistic adverse-selection cost** for a maker
  on Kalshi sports markets? Need to instrument and measure
  in the first 24h of live operation.
- **Are makers eligible for any rebate** (negative fee) on
  high-volume markets, like some equity venues? Research
  said no, but worth confirming with Kalshi support.
- **What happens to a resting GTC order at market settlement** —
  is it auto-cancelled, or does it stay alive into the next
  market? The OMS code path for settlement-time cleanup
  needs review.

## Reference docs

- `docs/AUDIT_2026-05-08.md` — original strategy mechanism audit
- `docs/STATE_LOG.md` — append-only timeline of operational changes
- `docs/SESSIONS.md` — current state snapshot
- `plans/2026-05-08-news-trader-implementation.md` — Priority 5 detail
