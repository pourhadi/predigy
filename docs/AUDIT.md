# System audit — 2026-05-07

> Snapshot taken the day of the engine's live cutover. Identifies
> profit-take, scale-up, and arsenal-expansion opportunities. Each
> item is tagged with an estimated dev-cost (S / M / L) and an
> operator-action requirement (A / —).
>
> **Status update 2026-05-07 (post-shipping):** A1, A2, A3, A4, A6,
> B2, B3, I2, I3, I4, I5, I6, S1 all shipped (see commits 57a28fc
> through 22e2578). A5 deferred with rationale below. B1, B7 are
> operator-action items. B4 (FIX) + B5 (maker rebates) + S7 (MM)
> + I1 (maker exec) gated on Kalshi access or $25K capital. I7
> (atomic multi-leg) deferred — its only consumers (S3, S9) are
> also deferred. S2, S4, S5, S6, S8, S9 remain — each needs new
> infrastructure (curator integration, new feed, price-history
> store) or coupled to deferred I7. Pick one when ready.

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

### A5 — Latency tiered force-flat (cost: S)

Latency's force-flat is binary: held <30min → keep, ≥30min →
exit at 1¢. A tiered exit:

- 0–5 min: hold
- 5–15 min: light TP at any positive PnL
- 15–30 min: force-flat at last book quote (requires book
  subscription — see I1)
- ≥30 min: 1¢ wide IOC (current behavior)

The 15–30 min tier is the new value. Without book access we'd
need to estimate the unwind price from REST `/portfolio/positions`'s
`average_fill_price` snapshot, which is staler than book data.

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

### S2 — Pre-settlement weather decay (cost: M)

Weather markets have a known information schedule: NBM ensemble
forecasts publish at fixed cadence. As the forecast horizon
collapses, individual market quantile probabilities approach
deterministic. If NBM at T-2 hours says "98% probability the high
temp exceeds threshold" but Kalshi quotes 92¢, lift.

`wx-stat-curator` already produces this signal. Today it flows
into stat as a rule. A dedicated `wx-stat` strategy module could
fire faster (no rule-table round-trip; consume the curator output
directly via cross-strategy bus or a dedicated event channel).

### S3 — Kalshi-internal sum-to-1 arb (cost: M)

Mutually-exclusive markets (e.g. "Trump wins NV" + "Harris wins
NV" + "Other") should sum to ≤ 1 minus venue fees. Sometimes
they don't. Sell each leg.

No cross-venue dependency. Pure microstructure on a single Kalshi
event family. Need event-family detection (curator) + multi-leg
order coordination (engine — currently single-leg only).

### S4 — Order-book mean reversion (cost: M)

When the touch is one-sided (e.g. yes_bid stack 100×, no_bid
stack 5×), price often mean-reverts toward midpoint over the next
1–5 min. Scalp the imbalance.

Pure microstructure; works at any market hour. Doesn't depend on
external information. Conceptually similar to settlement but
applies to ANY market with sufficient depth, not just sports
near close.

Risk: in trending markets the imbalance can persist or grow.
Need a cooldown + strict stop-loss.

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

### S8 — Volatility / variance trading (cost: L)

Markets with high recent volatility (price moves >10¢/hour with
no news) often revert. Strategy: detect rapid moves on stable-
information markets, fade them.

Adjacent but DIFFERENT from order-book mean-reversion — this is
based on price history, not current book asymmetry.

### S9 — Settlement-time multi-leg arb (cost: M)

For markets with multiple correlated legs settling simultaneously
(e.g. "Yankees beat Red Sox" + "Yankees season > 90 wins"),
the conditional probability constraint can produce arb when leg
prices drift independently.

Requires modeling the correlation structure per market family.

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

### I7 — Multi-leg / atomic submit (cost: M)

For S3 (sum-to-1 arb) and S9 (multi-leg), we need to submit
multiple intents atomically: "all legs fire OR none fire". The
OMS today is single-leg.

Add a `LegGroup` abstraction: a set of intents that share an
all-or-none constraint. The OMS pre-checks risk caps for the
combined notional, then submits all legs; if any fails, cancels
the others.

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
