# System audit — 2026-05-07

> Snapshot taken the day of the engine's live cutover. Identifies
> profit-take, scale-up, and arsenal-expansion opportunities. Each
> item is tagged with an estimated dev-cost (S / M / L) and an
> operator-action requirement (A / —).
>
> **Status update 2026-05-07 (post-shipping):** A1, A2, A3, A4, A5,
> A6, B2, B3, I2, I3, I4, I5, I6, I7, S1, S2, S3, S4, S8, S9 all
> shipped (see commits 57a28fc through this one). B1, B7 are
> operator-action items. B4 (FIX) + B5 (maker rebates) +
> S7 (MM) + I1 (maker exec) gated on Kalshi access or $25K
> capital. S5 (news-classifier) and S6 (third-venue cross-arb)
> remain — both need new external feeds and operator decisions
> about API providers.

---

## A. Profit-take improvements

Current exit logic by strategy:

| Strategy | Exit | Trigger |
|---|---|---|
| stat | mark-aware TP/SL | mark vs entry, 8¢ / 5¢ |
| cross-arb | mark-aware TP/SL | mark vs entry, 5¢ / 4¢ |
| latency | time-based force-flat | held >30min → 1¢ wide IOC |
| settlement | none | venue auto-settles at $1/$0 |

### A1 — Belief-drift exit for stat (cost: S)

The stat entry signal is `model_p > ask + min_edge`. If the curator
updates the rule with a fresh `model_p` and the new probability no
longer beats `current_mark + min_edge`, the original thesis is
invalidated **even if mark hasn't moved**. Today the strategy holds
the position until TP or SL triggers on price.

Add a third exit branch: when `cached_rule.model_p` for the
ticker drops below `current_mark/100 + min_edge_cents/100` after
the next rule refresh, emit a closing IOC at mark.

This is most valuable for stat-trader because rules churn (curator
runs every 10–15 min); other strategies have stable signals.

### A2 — Convergence-aware exit for cross-arb (cost: S)

Current cross-arb TP fires when `mark - entry >= 5¢`. But the
strategy's actual thesis is "poly_mid converges to kalshi_mark".
The exit should track the spread, not the price.

Replace the TP branch with: exit when
`abs(poly_mid_cents - mark_cents) <= 1`. The position has done its
job — the convergence happened. Booking 5¢ at Kalshi while
poly_mid is still 6¢ away leaves alpha on the table.

Symmetric stop: exit when poly_mid moves contrary to entry by
≥3¢ (thesis broken).

### A3 — Trailing stop instead of fixed SL (cost: S)

Fixed 5¢ stop triggers on a single bad tick. A trailing stop —
once mark has moved favorably ≥4¢, the stop floats up to lock
in `+1¢` — lets winners ride longer without giving back gains.

Implementation: `last_exit_at` already tracks per-position; add
`high_water_mark_cents`. On each book update where mark > high
water, raise the stop. Symmetric on short side.

### A4 — Time-decay TP scaling (cost: S)

For stat especially, the longer a position is held, the more
likely the original edge has decayed. Reduce TP threshold by
~1¢/hour held. Caps adverse drift; encourages exits before
settlement risk.

### A5 — Latency tiered force-flat (cost: S) **SHIPPED 2026-05-07**

Tiered exit:

- 0–`tier1_secs` (default 5min): hold
- `tier1`–`tier2_secs` (default 15min): light TP at any positive
  PnL (requires book mark)
- `tier2`–`max_hold_secs` (default 30min): force-flat at last
  book quote regardless of PnL
- ≥`max_hold_secs`: wide IOC at `force_flat_floor_cents` (1¢) —
  hard floor, fires even without a book mark

Implementation: `LatencyConfig` gained `tier1_secs` and
`tier2_secs` (env-overridable via `PREDIGY_LATENCY_TIER1_SECS`/
`TIER2_SECS`). Strategy gained `book_marks: HashMap<ticker,
(yes_bid, no_bid)>` populated from `Event::BookUpdate`. After
firing an entry, the strategy calls
`state.subscribe_to_markets(...)` so the engine fans the held
market's book updates back into the strategy's queue
(see `bin/predigy-engine/src/self_subscribe.rs` —
`SelfSubscribeDispatcher`).

`evaluate_force_flats()` selects the highest-applicable tier;
tier-2 defers to tier-3 if no mark is yet cached. Cid format
gained a `:tN:` segment so each tier produces a distinct
idempotency key. 7 new tests cover all branches including
NO-side complement-mark derivation.

Plumbing landed in `crates/engine-core/src/state.rs`
(`SelfSubscribeRequest`, `with_self_subscribe_tx`,
`subscribe_to_markets`) and
`bin/predigy-engine/src/self_subscribe.rs`
(`SelfSubscribeDispatcher` consumes requests, looks up the
strategy's own supervisor `event_tx`, dispatches as
`RouterCommand::AddTickers` so deltas flow back to the
requester).

### A6 — Settlement profit lock (cost: S)

Settlement currently rides to venue settlement at $1/$0. If the
price has moved 7¢ in our favor with ≥3 min still on the clock
(e.g. entry 93¢, mark 99¢), there's settlement-race risk + 7¢
already in the bag. Lock it.

Settlement positions are short-duration (<10 min by definition);
adding TP at +5¢ won't fire often but cuts left-tail risk.

---

## B. Scale-up paths

Capital cap today: `$5/strategy`, `$15 global`, `$2 daily-loss`.
With ~$50 funded, we're using <30% of available capital.

### B1 — Raise notional caps after stable week (cost: 0, action: A)

Once the engine has run ≥1 week without major incident, raise
shake-down caps:

| Cap | Current | Proposed |
|---|---|---|
| `max_notional_cents` | $5 | $20 |
| `max_global_notional_cents` | $15 | $60 |
| `max_daily_loss_cents` | $2 | $5 |
| `max_contracts_per_side` | 3 | 10 |
| `max_in_flight` | 10 | 25 |

Override per-cap via env vars in `~/.zprofile`
(`PREDIGY_MAX_*_CENTS`). Bumping `max_contracts_per_side` is the
biggest behavioral change — it lets Kelly sizing actually express
its bet size when the edge is large. Currently 3-contract cap
binds on every high-edge fire and we're sub-Kelly.

### B2 — Half-Kelly factor on stat (cost: 0, action: A)

`StatConfig::kelly_factor` defaults to 0.25 (quarter-Kelly).
Half-Kelly (0.5) is the textbook risk-averse default. Once edge
is verified live, double the factor.

Set `PREDIGY_STAT_KELLY_FACTOR=0.5` in `~/.zprofile`. (Currently
plumbed for the legacy stat-trader; not yet wired into the engine
module — minor S edit to `engine-core::config::caps_from_env` +
`StatConfig`.)

### B3 — Per-strategy cooldown override (cost: S)

Cooldown is global (60s default). For stat, with rules churning
every ~10 min and book deltas common, 60s is fine. For latency,
NWS alerts are sparse; cooldown rarely binds. For cross-arb,
sub-second response to convergence wins (already at 500ms cooldown).

Add env-var overrides per strategy:
`PREDIGY_{STAT,SETTLEMENT,LATENCY,CROSS_ARB}_COOLDOWN_MS`. Already
half-implemented; finish the plumbing.

### B4 — Phase 4b FIX (cost: M, action: A — blocked on Kalshi)

The biggest scale-up lever. Current REST submit is ~200ms; FIX
sub-ms. For latency-sensitive strategies (cross-arb's
convergence, latency-trader's news lift), winning the touch
matters. FIX would let us be the first taker on most events.

**Blocked on Kalshi institutional onboarding.** Email draft in
`docs/KALSHI_FIX_REQUEST.md`. Operator-action: send the email,
follow up.

The engine's `VenueChoice` enum already exists in OMS — switching
between REST and FIX is per-intent, not a rebuild.

### B5 — Maker rebates (cost: L, action: A — capital threshold)

Kalshi's maker fee is 75% cheaper than taker. Currently every
strategy uses IOC takers. Adding a passive-quoting strategy
(post-only GTC at a price tier away from the touch) converts fee
into edge — but requires queue modeling, order_state tracking,
and meaningful capital ($25K threshold per the original plan).

Phase 4 of the original PLAN.md. Deferred until ≥$25K.

### B6 — Curator-side scale (cost: M)

Stat has 68 active rules; cross-arb has 2–5 pairs typically. Each
strategy is rule/pair-bottlenecked, not capital-bottlenecked.

To scale, the curators need to find more edge candidates:

- **stat-curator**: increase the universe scanned (currently
  filters narrow); raise the model_p quality bar so we accept
  weaker but more numerous rules. Anthropic-cost scales.
- **cross-arb-curator**: expand category coverage (already done in
  PR #32). Bottleneck is now the cross-venue Polymarket overlap
  itself, not scan coverage.

### B7 — Multi-region engine (cost: L, action: A)

Engine runs on a macOS laptop. Network round-trip to Kalshi
(us-east) from the laptop is ~30ms one-way. A us-east-1 VPS
removes that. Combined with FIX (B4), latency-sensitive strategies
become viable in their proper form.

Phase 8 / "Hardening & scaling" in the original PLAN.md. Deferred
until per-strategy capital is large enough to justify the VPS
spend.

---

## C. Strategy arsenal expansion

### S1 — Settlement-time fade (cost: S)

Inverse of current settlement strategy. If yes_ask climbs above
96¢ in the last 10 min with thin bid stack vs heavy ask stack,
the market is **overconfident**. Sell-YES IOC at the touch.
Settlement reverts (or Kalshi pulls quote): profit.

Symmetric to current settlement; same discovery service, same
data inputs. Pure code addition.

### S2 — Pre-settlement weather decay (cost: M) **SHIPPED 2026-05-07**

New `predigy-strategy-wx-stat` crate (`STRATEGY_ID = "wx-stat"`)
consumes `wx-stat-curator`'s `wx-stat-rules.json` directly via
mtime-poll, bypassing the `rules` table round-trip. Same Kelly
sizing + after-fee edge math as stat. Half-Kelly default since
the curator's `model_p` is calibrated against historical NBM
error (`wx-stat-fit-calibration`) — the calibration confidence
warrants the larger sizing fraction.

Self-subscribes to curated markets via the A5 self-subscribe path
(no static `subscribed_markets`). On every Tick the strategy
mtime-polls the rule file; if the file changed, it parses and
diffs and self-subscribes to newly-introduced tickers. The
curator can rewrite the file at any time and the strategy picks
up the change within `rule_refresh_interval` (default 30s).

The schema mirrors `stat_trader::StatRule` exactly, so the
curator's existing JSON output is consumed without modification.

Operational:
- `PREDIGY_WX_STAT_RULE_FILE` (path, required) — gates registration.
- `PREDIGY_WX_STAT_BANKROLL_CENTS`, `_KELLY_FACTOR`, `_MAX_SIZE`,
  `_COOLDOWN_MS`, `_RULE_REFRESH_MS` — tunables.
- Independent `STRATEGY_ID` means positions, kill-switch, and
  risk caps are tracked separately from `stat`. Operators can A/B
  compare wx-stat vs stat alpha cleanly.

7 unit tests cover edge calculation, cooldown, file-change
diffing, and validation rejection (model_p outside [0.01, 0.99]).
No active exits in v1 — weather markets resolve at venue
settlement; the original entry thesis is by definition
settlement-resolved. Layer in TP/SL only if empirical data later
justifies it.

### S3 — Kalshi-internal sum-to-1 arb (cost: M) **SHIPPED 2026-05-07**

New `predigy-strategy-internal-arb` (`STRATEGY_ID="internal-arb"`)
fires when the YES side of a mutually-exclusive event family
sums below 100¢ minus fees minus the operator's edge threshold.

Mechanism:
- JSON-config file lists event families: each is a list of
  Kalshi tickers known to be mutually-exclusive.
- Strategy subscribes to all listed tickers via
  `subscribed_markets()` (loaded from the config file at
  construction time so the engine can wire the router
  correctly), and recomputes per-family arb on every
  `Event::BookUpdate` for any leg.
- Per-leg YES-ask is derived `100 - best_no_bid`. Per-leg taker
  fee from `predigy_core::fees::taker_fee`. Aggregate edge =
  `100 - Σ ask - Σ fee - extra_fee_padding`.
- When `edge ≥ min_edge_cents`, the strategy queues a
  `LegGroup` of YES-buy IOC limits (one per leg) into its
  `pending_groups` buffer. The supervisor drains and forwards
  via `Oms::submit_group` (Audit I7).
- Per-family cooldown (default 60s) prevents re-firing while
  the existing group is still working at the venue.

Foundation lift in this round:
- New `Strategy::drain_pending_groups()` trait method (default
  empty); supervisor drains after every `on_event` and routes
  each group through I7's atomic submit. Existing single-leg
  strategies inherit the default and stay untouched.

What S3 doesn't do (by design):
- No event-family auto-detection. Operator (or future curator)
  authors the JSON. Auto-discovery via Kalshi event taxonomy is
  a follow-up.
- No NO-side mirror arb (`Σ no_ask > (n-1) + edge`). Easy to
  add later if YES-side proves out.
- No partial-fill recovery: I7's cascade cancels siblings on
  full venue rejection, but if leg 1 fully fills and leg 2
  partially fills, the operator is left with asymmetric
  exposure. IOC TIF bounds the worst case to one tick.

Operational:
- `PREDIGY_INTERNAL_ARB_CONFIG` (path) — required.
- `PREDIGY_INTERNAL_ARB_MIN_EDGE_CENTS` / `_SIZE` /
  `_COOLDOWN_MS` / `_REFRESH_MS` — tunables.

Tests (6 unit, all passing): fires when total ask clears
threshold, skips when at-or-above 100¢, skips when a leg has no
book, cooldown blocks repeats, family with <2 tickers rejected
at load, ticker→families reverse index built correctly across
overlapping families.

### S4 — Order-book mean reversion (cost: M) **SHIPPED 2026-05-07**

New `predigy-strategy-book-imbalance`
(`STRATEGY_ID="book-imbalance"`) fades the dominant side of a
heavily-imbalanced touch:

- imbalance = `(yes_bid_qty − no_bid_qty) / (yes_bid_qty + no_bid_qty)`
- When `|imbalance| ≥ threshold` (default 0.7) AND total qty ≥
  `min_total_qty` (default 50), fire a fade IOC limit at the
  weak side's ask.
- yes_bid stack dominant → buy NO at `100 − yes_bid`
- no_bid stack dominant → buy YES at `100 − no_bid`
- Per-market cooldown (default 60s) prevents re-firing while the
  position is still open.
- Take-ask gating (`min_take_ask_cents` 5, `max_take_ask_cents`
  90) keeps the strategy out of the rails.

Operational:
- `PREDIGY_BOOK_IMBALANCE_CONFIG` (path) — required.
- `_THRESHOLD`, `_MIN_TOTAL_QTY`, `_MIN_EDGE_CENTS`,
  `_MAX_TAKE_ASK_CENTS`, `_MIN_TAKE_ASK_CENTS`, `_SIZE`,
  `_COOLDOWN_MS`, `_REFRESH_MS` — tunables.

Per-market threshold override via `imbalance_threshold_override`
in the JSON: tighter on noisy markets, looser on highest-volume
families.

Tests (8 unit, all passing):
- fades dominant YES bid stack → buy NO
- fades dominant NO bid stack → buy YES
- skips balanced book
- skips when total qty below floor
- skips when ticker not in config
- cooldown blocks repeats
- skips when ask outside [min, max] take floor
- per-market threshold override applied at evaluate time

What S4 v1 deliberately doesn't do:
- No active mark-aware exits. The OMS's session-flatten +
  kill-switch handle forced flats. Layer TP/SL when empirical
  performance justifies it.
- No book-depth aggregation (touch-only). Stack-of-stacks would
  improve robustness but doesn't fundamentally change the signal.

### S5 — News-event semantic latency expansion (cost: L)

Current `latency` strategy fires on NWS event-type substrings
(`"Tornado"`, `"Heat Advisory"`). The signal pipeline is
`NwsAlert → string-match → fire`.

Extending to free-text news (Twitter, RSS, Bloomberg) needs a
classifier: text → market-relevance label. Claude is the right
tool. Architecture:

- New ext-feeds module per source (twitter, rss, bloomberg).
- A classifier service (or per-event Claude call) tags each
  incoming item with `(market_ticker, side, expected_prob_shift)`.
- `latency` consumes the classifier output as a new
  `ExternalEvent::ClassifiedNews` variant.

Cost is real (Anthropic per-call); may be feasible only on
high-impact-per-call sources.

### S6 — Cross-arb expansion to other venues (cost: L)

Currently cross-arb is Kalshi vs Polymarket. Adding a third venue
(Manifold, PredictIt-successor, etc.) widens the convergence
surface. Same architecture: WS feed + reference price + paired
markets.

Manifold's API is open; PredictIt-successor is uncertain.

### S7 — Liquidity-provision market making (cost: L, action: A — capital)

Phase 4 of the original plan. Passive quoting with rebate
capture. Big infrastructure investment (queue model, order_state
WS channel wired, dual-side state machine). Deferred until $25K.

### S8 — Volatility / variance trading (cost: L) **SHIPPED 2026-05-07**

New `predigy-strategy-variance-fade`
(`STRATEGY_ID="variance-fade"`) maintains a rolling window of
mid-price observations per ticker and fades excessive moves on
operator-declared "stable-information" markets.

Mechanism:
- Per-ticker `VecDeque<(at, yes_mid_cents)>` window (default
  10 min); samples older than `window` evict on each tick.
- Mid = `(yes_bid + (100 − no_bid)) / 2`. Falls back to the
  single-side price when only one side has a touch.
- On each `Event::BookUpdate`, compute the latest mid AND the
  median of the window. When `|current − median| ≥
  move_threshold_cents` (default 8¢) AND the window has at least
  `min_observations` samples (default 30), the strategy fires a
  fade IOC limit at the cheap-side ask:
  - mid moved UP → YES has gotten expensive → buy NO
  - mid moved DOWN → buy YES
- Per-market move-threshold override + per-market cooldown
  prevent over-firing on either tail.

Distinct from S4 (book-imbalance): S4 uses *current touch
asymmetry*; S8 uses *price history*. The two signals are
independent and can run on the same universe simultaneously.

Operational:
- `PREDIGY_VARIANCE_FADE_CONFIG` (path) — required.
- `_WINDOW_SECS`, `_MOVE_THRESHOLD_CENTS`, `_MIN_OBSERVATIONS`,
  `_MIN_TAKE_ASK_CENTS`, `_MAX_TAKE_ASK_CENTS`, `_SIZE`,
  `_COOLDOWN_MS`, `_REFRESH_MS` — tunables.

Tests (8 unit, all passing):
- fades upward move → buys NO at correct ask
- fades downward move → buys YES at correct ask
- skips small moves (below threshold)
- skips when window is below min_observations
- evicts samples older than the window (memory bounded)
- cooldown blocks repeats within window
- skips markets not in config
- per-market threshold override applied at evaluate time

What S8 v1 deliberately doesn't do:
- **No news-suppression gate.** A real news event drives the
  move and the strategy will fade incorrectly. The operator's
  universe choice (information-stable markets) is the only
  current safeguard. A future integration with the news-
  classifier (S5) will gate on a "no high-impact news" signal.
- **No active mark-aware exits.** The OMS's session-flatten +
  kill-switch handle forced flats. Layered TP/SL is a follow-up.
- **No book-depth weighting.** Touch-only. Adding bid-stack-vs-
  ask-stack to the variance signal would be more robust but
  changes the model materially — defer.

### S9 — Settlement-time multi-leg arb (cost: M) **SHIPPED 2026-05-07**

New `predigy-strategy-implication-arb`
(`STRATEGY_ID="implication-arb"`) handles the strict-implication
case: when child YES ⊂ parent YES, prices must satisfy
`P(child) ≤ P(parent)`. When the touch quotes drift such that
`yes_bid_child − yes_ask_parent ≥ min_edge`, a two-leg trade
locks in profit:

- **Buy YES_parent at `yes_ask_parent`**
- **Sell YES_child at `yes_bid_child`** (= **Buy NO_child at
  `100 − yes_bid_child`**)

Minimum-payoff scenario is "parent YES & child YES":
parent settles to +$1, the child short pays out $1 → settlement
nets $0; the cash leg yields `yes_bid_child − yes_ask_parent`
up front. The child YES & parent NO scenario is impossible by
the implication premise. Other scenarios add further profit.

Mechanism:
- JSON config: `[{ "parent": "KX-A", "child": "KX-B" }, ...]`.
- Strategy subscribes to all referenced tickers; touches are
  cached on each `Event::BookUpdate`.
- On each book update, every pair this ticker is part of is
  re-evaluated. When edge clears, a 2-leg `LegGroup` is queued
  in `pending_groups`; the supervisor drains and routes through
  `Oms::submit_group` for atomic persistence + cancellation
  cascade (Audit I7).
- Per-pair cooldown prevents re-firing while an open group is
  still working at the venue.

Operational:
- `PREDIGY_IMPLICATION_ARB_CONFIG` (path) — required.
- `PREDIGY_IMPLICATION_ARB_MIN_EDGE_CENTS` / `_SIZE` /
  `_COOLDOWN_MS` / `_REFRESH_MS` — tunables.

Tests (5 unit, all passing):
- fires when child bid exceeds parent ask + edge
- skips when child bid ≤ parent ask
- skips when a leg has no book mark
- cooldown blocks repeats
- degenerate self-pair (parent == child) skipped at config load

What S9 deliberately doesn't do:
- **No general correlation modeling.** Markets that are merely
  correlated (not strict-implication) like "Yankees beat Red Sox
  today" + "Yankees > 90 wins" need a Bayesian constraint solver
  — a follow-up, separate strategy.
- **No auto-discovery.** Operator authors the pair list. A
  future curator could detect implication pairs by parsing
  Kalshi's event taxonomy.

---

## D. Infrastructure improvements (cross-cutting)

### I1 — Maker-side execution layer (cost: L)

Adds:
- `OrderType::PostOnly` + GTC TIF
- order_state WS channel wiring (the channel exists, exec_data
  doesn't subscribe)
- Resting-order tracking in OMS
- Cancel-via-REST when book moves through our resting price

Required for B5 (rebates) and S7 (MM).

### I2 — Per-strategy kill switch (cost: S)

Current kill switch is global. A per-strategy variant lets the
operator pause stat without affecting cross-arb during surgical
interventions.

`kill_switches` table already has a `scope` column; the engine's
`KillSwitchView` needs to track per-strategy + global, and OMS
checks both at submit time.

### I3 — Strategy-output cross-augmentation (cost: M)

Cross-strategy bus is wired (Phase 6.2). cross-arb publishes
`PolyMidUpdate`; stat subscribes — but only logs.

Implementation: when stat receives PolyMidUpdate for a paired
ticker, blend it into its belief — `effective_p = stat_model_p ×
α + poly_mid × (1 − α)` for some α. The combined signal should
have lower noise than either alone.

This is the most concrete win from the bus we already built.

### I4 — Book-aware unrealized PnL on engine positions (cost: S)

Dashboard's "engine positions" table doesn't compute unrealized
P&L because `book_snapshots` only has YES-side data. Add NO-side
fields (`best_no_bid_cents`, `best_no_ask_cents`, qtys) — the
WS router already sees both sides; just needs a write path.

### I5 — Fill-latency telemetry (cost: S)

Measure `intent.submitted_at → fill.ts` per fill. Surface
per-strategy / per-venue distributions in the dashboard.
Degradation alarm at p95 > 1s.

Currently observable only via raw log greps.

### I6 — Initial-snapshot grace period (cost: S)

When the engine first connects to WS, the strategy treats every
initial book snapshot as a fresh delta and tries to fire on all
subscribed markets at once. The in-flight cap (10) bounds the
damage today, but it's noise that wastes a venue submit budget.

Add a 5-second grace window after WS connect during which
strategies skip evaluation. Logged as G1 in `STATUS.md` open
issues.

### I7 — Multi-leg / atomic submit (cost: M) **SHIPPED 2026-05-07**

Foundation for S3 (sum-to-1 arb) and S9 (multi-leg arb).

- `LegGroup { group_id: Uuid, intents: Vec<Intent> }` lives in
  `engine_core::intent`. `LegGroup::new()` allocates a fresh UUID;
  `LegGroup::with_id()` reattaches a known UUID for replay.
- `Oms::submit_group(LegGroup) -> SubmitGroupOutcome` — new trait
  method.
- `SubmitGroupOutcome` variants: `Submitted` (all legs persisted
  with shared `group_id`), `Idempotent` (every leg already exists
  under the same group), `Rejected` (returns first failing
  leg + reason — no rows inserted), `PartialCollision` (some
  legs already exist under a different/no group; refuses to graft).
- DbBackedOms impl performs:
  1. Per-leg pre-check (kill switch + shape).
  2. Idempotency probe across all legs simultaneously.
  3. Combined-notional cap check (sum of all leg projected
     notional vs strategy + global caps), plus per-leg
     contract-side cap.
  4. Atomic insert: every leg's `intents` row + `intent_events`
     row inside one Postgres transaction. All-or-none.
- **Cancellation cascade**: when `apply_execution` records a
  `Rejected` or `Expired` status for a leg with a non-NULL
  `leg_group_id`, the OMS marks every still-active sibling leg
  `cancel_requested` in the same transaction and emits a cascade
  event with `cascade_source` + `leg_group_id` provenance. The
  venue router then sends cancels for those siblings just like
  any other operator-triggered cancel.

DB:
- Migration `0002_leg_group.sql` adds `leg_group_id UUID NULL`
  to `intents` plus a partial index on the non-NULL subset.
  Single-leg submits leave the column NULL — fully backwards
  compatible.

Tests (6 new integration, all passing against `predigy_test`):
- 2-leg submit persists both legs with the same group_id; both
  intent_events fire.
- Kill-switch arms → group rejects, no rows inserted.
- Combined notional cap rejects whole group even when each leg
  alone would pass.
- Idempotent replay returns Idempotent + leaves count at 2.
- Same client_id reused across distinct group constructions
  returns PartialCollision (operator must resolve).
- Venue rejection on leg 1 cascade-cancels leg 2; cascade event
  carries the source client_id and leg_group_id for forensics.

What this enables: S3 (Kalshi-internal sum-to-1 arb) and S9
(settlement-time multi-leg arb) can now construct LegGroups and
get DB-side atomicity + venue-side cascade cancellation for free.

---

## E. Priority ranking — top 5 by ROI / effort

| # | Item | Cost | Action | Why |
|---|---|---|---|---|
| 1 | B4 — Phase 4b FIX | M | A | Blocked but highest-leverage. 0 dev work for the operator-action piece. |
| 2 | I3 — Cross-strategy belief augmentation | M | — | Activates the cross-strategy bus we already built. Doubles signal quality on overlapping tickers. |
| 3 | A1 — Stat belief-drift exit | S | — | Prevents holding stale-edge positions. Trivial code change. |
| 4 | A2 — Cross-arb convergence-aware exit | S | — | Aligns exit with thesis. Also trivial. |
| 5 | B1 + B2 — Raise caps + half-Kelly | 0 | A | Operator config change. Doubles capacity to put capital to work. Wait until ≥1 week of stable engine first. |

---

## F. Items deliberately not pursued

- **Real-time queue modeling** — only useful for maker strategies
  (S7), which are gated by capital threshold. Defer.
- **Multi-region engine** — laptop is fine until FIX is granted;
  VPS without FIX is a marginal upgrade.
- **Custom WS exec / direct binary protocol** — Kalshi V2 is
  HTTP/JSON; FIX is the standard upgrade path.
- **Distributed multi-engine** — single-engine is the right model
  for our capital scale. Sharding adds complexity that doesn't
  pay back below ~$100K AUM.
- **Cross-asset (futures, options)** — that's `~/code/tradegy`'s
  job. Predigy stays prediction-market-focused; tradegy stays
  equity-derivative-focused. Diversification is by mechanism, not
  instrument.

---

*This audit was generated post-cutover on 2026-05-07. Update as
items are picked up or as live experience changes priorities.*
