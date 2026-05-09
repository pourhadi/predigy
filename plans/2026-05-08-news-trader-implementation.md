# News-Trader Classifier — Implementation Plan

**Date**: 2026-05-08
**Status**: research-validated, scope re-framed; ready to implement
**Author**: Claude session continuing from `docs/AUDIT_2026-05-08.md`
**Companion docs**: research transcript at task `a854cc16227197f60`;
  existing strategy code at `crates/strategies/news-trader/src/lib.rs`

## TL;DR

The original thesis (LLM-classified breaking news → fast IOC submit
→ beat the market) **is structurally infeasible** at our scope.
Research-grounded reasons:

1. Institutional prediction-market desks (DRW, SIG, Jump) are
   already at NY4 co-lo with sub-200ms news-to-submit pipelines.
2. We don't have Reuters/AP wire — those are enterprise-tier.
3. Our LLM call alone is **1-2s on Haiku 4.5**, which is the
   fastest model available; that's most of our latency budget.
4. Polymarket-arb-window data shows the average breaking-news
   opportunity collapsed from 12.3s (2024) to 2.7s (early 2026),
   with 73% of profits captured by sub-100ms bots.

**However**, the same research surfaces three viable narrower
scopes where speed disadvantage doesn't matter:

| Scope | Why we can compete | Per-event edge |
|---|---|---|
| **Scheduled releases** (CPI, FOMC, NFP, Treasury) | Timing is published. We pre-stage and only need to diff vs. expectation. ~12 FOMC + ~12 CPI + ~12 NFP per year = ~36 events. | $5-30 |
| **Slow news** | Debates, gradually-developing stories take minutes to fully reprice. Biden-Trump 2024 debate took ~30 min for full move. LLM 2s is fine. | $1-10 |
| **Long-tail markets** | Institutional desks don't watch every market. Niche sports, regional weather, smaller political contracts. | $0.50-3 |

## What we're building

Scope: **a classifier service** that produces JSONL items the
existing `crates/strategies/news-trader/` strategy consumes.
The strategy code is already complete — it mtime-polls the
JSONL and submits IOC orders. Our work is upstream of it.

Output schema (already defined in the strategy):
```json
{"item_id": "bls-cpi-2026-05-13", "ticker": "KX-CPI-26MAY-T0.3",
 "side": "yes", "action": "buy", "max_price_cents": 60,
 "size": 5, "source": "bls.gov", "headline": "CPI 0.4 vs cons 0.3",
 "classified_at": "2026-05-13T12:30:01.234Z",
 "confidence": 0.85}
```

## Phased delivery

### Phase 1 — Scheduled releases (BLS / Fed / Treasury)

**Why first**: highest edge per event, lowest cost, lowest false-
positive risk. Free public RSS feeds. Sub-second polling around
known release windows is legal (post-2020 BLS no longer has media
lockup; data is public at the announced time).

**Components**:

- New binary: `bin/news-classifier-scheduled/`
  - Tracks the release calendar in code: BLS CPI schedule, BEA GDP
    schedule, FOMC dates, NFP schedule, Treasury auctions.
  - Around each known release window (T-60s to T+60s):
    polls the BLS/Fed/Treasury page at 250ms cadence.
  - On detected new content: extracts the headline numbers, fires
    a Claude Haiku 4.5 call with a *cached system prompt* listing
    the active Kalshi markets affected by that release.
  - Claude returns a structured JSON of `(market_ticker, side,
    confidence, max_price)` tuples. We append to the JSONL.
- New table: `release_calendar` (date, release_type, source_url,
  expected_release_time_utc, last_polled_at, last_seen_hash).
  Pre-populated for the next 12 months from each agency's
  published schedule.
- Source list (Phase 1 only):
  - BLS RSS: https://www.bls.gov/feed/news_release.rss
  - Fed RSS: https://www.federalreserve.gov/feeds/press_all.xml
  - Treasury auctions: https://www.treasurydirect.gov/TA_WS/...
  - Whitehouse.gov RSS for executive actions
- All free.

**Latency budget for Phase 1** (target):

| Stage | Time |
|---|---|
| Scheduled release wire-public → our poll detects | 100-500ms |
| Local extract (regex / pre-defined parser per source) | <10ms |
| Claude Haiku 4.5 with cached prompt | 800-1500ms |
| JSONL append + news-trader mtime tick (5s default) | up to 5s |
| Strategy submit IOC | 50-150ms |
| Kalshi accept | 10-50ms |
| **End-to-end** | **~1-7s p50** (dominated by news-trader poll) |

The 5s news-trader mtime poll is the biggest controllable lever —
will tighten it to 1s for Phase 1 deploys (`refresh_interval`
config knob already exists).

**Critical risk**: insider-trading rules. Per Kalshi (Feb 2026
Axios report on suspensions, surveillance partnership with
Solidus Labs + Wharton): trading on data **before** public release
is prohibited. Mitigation: use only post-publication public-domain
sources. Never query non-public endpoints. Never poll a server
that *might* leak pre-release data.

### Phase 2 — Slow news via Benzinga Pro

**Why second**: covers the broader news universe (Twitter
firehose-equivalent for retail), comes with structured tickers,
proven by other retail trading bots. $37/mo budget.

**Components**:

- Add Benzinga newsfeed ingest as a TCP/HTTP push consumer
- Reuse the same Claude Haiku classification path from Phase 1,
  but with a different prompt per news type
- Confidence threshold higher (0.85+) because false positives
  are more costly here — the universe is broader
- Specific markets we'd target:
  - Election / political (Truth Social, Bluesky also)
  - Weather (severe storms, flooding)
  - Sports trade/injury (overlap with Phase 3)

**Cost economics**:
- Benzinga: $37/mo
- Claude Haiku at 2-3 classifications/hour active: ~$0.005/call ×
  500/mo ≈ $2.50/mo
- Total: ~$40/mo
- Per-fire edge target: $1-5
- Break-even: 8-10 profitable fires/month
- We expect 5-15 fires/week from Benzinga based on retail bot
  reports

### Phase 3 — Sports edge from real-time game state

**Why third**: complements the existing `settlement` strategy
rather than replacing it. ESPN and MLB Stats API are free with
near-real-time data.

**Components**:

- New `bin/news-classifier-sports/` polls live game state via
  ESPN's hidden API (free, no SLA) + MLB Stats API
- Computes win-probability from a small model (start with
  logistic regression on score-differential × time-remaining,
  upgrade to LightGBM as data accumulates)
- Compares to live Kalshi sports market touch
- Fires when |true_p - kalshi_implied_p| > threshold AND
  |kalshi_implied_p in [88, 96]| (settlement strategy's regime)
- Appends classified items to the same JSONL

**Note**: this is a different beast from Phases 1-2 because the
"news" is continuous game state, not a discrete event. Polling
ESPN at 5s cadence during games gives constant stream. Claude
not needed — pure ML.

### Phase 4 — Twitter/X polling (deferred)

**Why deferred**: cost. X firehose is $4,500-50,000/mo. Pay-per-
read at $0.005/read with 2M cap means ~50 keyword searches per
second is the practical limit at $100/mo budget — that's polling,
not real-time. Defer until Phase 1 + 2 prove the JSONL pipeline.

If pursued: focus on @realDonaldTrump, @POTUS, official campaign
accounts, Fed governors, top sports reporters (Schefter et al.).
~10 high-signal accounts.

## Architecture diagram

```
┌──────────────────────────────────────────────────┐
│  Sources                                         │
├──────────────────────────────────────────────────┤
│  BLS RSS    Fed RSS    Treasury    Benzinga TCP │
│      │         │           │            │       │
│      ↓         ↓           ↓            ↓       │
│  ┌────────────────────────────────────────────┐  │
│  │  Source ingestor (per-source parser)       │  │
│  │  - emits NormalizedHeadline                 │  │
│  └─────────────────────┬───────────────────────┘  │
│                        │                          │
│                        ↓                          │
│  ┌────────────────────────────────────────────┐  │
│  │  Claude Haiku 4.5 classifier               │  │
│  │  - cached system prompt (active markets)   │  │
│  │  - structured output: ClassifiedNewsItem    │  │
│  │  - confidence threshold filter              │  │
│  └─────────────────────┬───────────────────────┘  │
│                        │                          │
│                        ↓                          │
│  ┌────────────────────────────────────────────┐  │
│  │  Output: ~/.config/predigy/news-items.jsonl│  │
│  │  - atomic append (lockfile)                │  │
│  └─────────────────────┬───────────────────────┘  │
└────────────────────────│───────────────────────────┘
                         │
                         ↓ (mtime poll, 1s)
┌──────────────────────────────────────────────────┐
│  predigy-engine                                  │
│   └ strategies/news-trader (already shipped)    │
│      - dedup on item_id                          │
│      - submit IOC at max_price_cents             │
└──────────────────────────────────────────────────┘
```

## File-level changes required

### New (Phase 1)

- `bin/news-classifier-scheduled/Cargo.toml`
- `bin/news-classifier-scheduled/src/main.rs` — main loop
- `bin/news-classifier-scheduled/src/calendar.rs` — release schedule
- `bin/news-classifier-scheduled/src/sources/{bls,fed,treasury}.rs`
- `bin/news-classifier-scheduled/src/anthropic.rs` — Haiku call
- `bin/news-classifier-scheduled/src/jsonl_writer.rs` — atomic append
- `migrations/0004_release_calendar.sql`
- `deploy/macos/com.predigy.news-classifier.plist`
- `deploy/scripts/news-classifier-scheduled.sh`

### Modified

- `~/.zprofile`: re-enable `PREDIGY_NEWS_TRADER_ITEMS_FILE`
- `Cargo.toml`: add `bin/news-classifier-scheduled` to workspace
- `crates/strategies/news-trader/src/lib.rs`: lower default
  `refresh_interval` from 5s to 1s for Phase 1
- `deploy/scripts/install-launchd.sh`: wire the new plist

### New tables

```sql
-- migrations/0004_release_calendar.sql
CREATE TABLE release_calendar (
    id BIGSERIAL PRIMARY KEY,
    release_type TEXT NOT NULL,            -- 'cpi' | 'fomc' | 'nfp' | 'gdp' | ...
    source_url TEXT NOT NULL,
    expected_release_at TIMESTAMPTZ NOT NULL,
    last_polled_at TIMESTAMPTZ,
    last_seen_hash TEXT,                   -- SHA-256 of polled body
    notes TEXT
);
CREATE INDEX release_calendar_at_idx ON release_calendar (expected_release_at);
CREATE INDEX release_calendar_type_idx ON release_calendar (release_type);
```

## Anthropic prompt structure

System prompt (cached, ~3-5K tokens):
```
You are a Kalshi prediction-market news classifier. Given a headline
or release excerpt, identify which currently-active Kalshi market
it affects, in what direction, and at what confidence.

Active markets (refreshed every 4h):
  KX-CPI-26MAY-T0.0 — Will CPI MoM be 0.0% or negative?
  KX-CPI-26MAY-T0.1 — Will CPI MoM be 0.1% or higher? Currently
    bid 0.65 / ask 0.68.
  ...
  [200-500 markets, by category]

Respond with a single JSON array of zero or more affected markets:
[{"ticker": "KX-...", "side": "yes"|"no", "action": "buy",
  "confidence": 0.0-1.0, "rationale": "<one sentence>"}]

Empty array if no market is meaningfully affected.
Confidence < 0.7 → still emit; the strategy filters.
```

User prompt:
```
Source: bls.gov (CPI release, May 2026)
Headline: "CPI MoM 0.4% vs Reuters consensus 0.3%, prior 0.2%"
Released at: 2026-05-13T12:30:00Z (now+0.5s)
```

Expected response:
```json
[
  {"ticker": "KX-CPI-26MAY-T0.3", "side": "yes", "action": "buy",
   "confidence": 0.95, "rationale": "0.4% > 0.3% threshold"},
  {"ticker": "KX-FED-HIKE-26JUN", "side": "yes", "action": "buy",
   "confidence": 0.7, "rationale": "Hot CPI raises hike odds modestly"}
]
```

## Risk register

| Risk | Severity | Mitigation |
|---|---|---|
| **Insider-trading violation** (poll source pre-release) | CRITICAL | Calendar table holds expected_release_at; never poll before that timestamp. Audit each source for "is this published-on-the-second or earlier-leaked?" |
| LLM hallucinates a wrong ticker | HIGH | Confidence threshold ≥0.85 for ticker match. Strategy bounds-checks `max_price_cents` ∈ [min_take_ask, max_take_ask] (already implemented). Per-fire size cap ≤ 5 contracts initially. |
| LLM misjudges direction | HIGH | Confidence threshold + per-source historical accuracy tracking (calibration_reports table already exists). |
| Rate-limit on Anthropic | MEDIUM | Haiku is fast; with cached system prompt, we can do 100+ classifications/min. Real risk is API outages — fail-closed (don't submit on missing classification). |
| Network partition between us and Kalshi during release window | MEDIUM | Latency-trader's existing tier-3 force-flat logic applies — held positions auto-flatten after 30 min. |
| Adverse selection: institutional desks already moved the price | MEDIUM | Confidence-vs-edge gating: only fire when confidence×edge > 2× round-trip fee. |
| Cost overrun on Anthropic | LOW | Haiku at $0.0015/call × even 1000/day = $1.50/day. Budget $50/mo cap on the classifier service. |

## Success metrics (Phase 1)

After 4 weeks of live operation:

- ≥80% of FOMC + CPI + NFP releases produced ≥1 classified item
  within 10 seconds of public release.
- ≥60% of fired items were positive-EV after fees (the
  `calibration_reports` infrastructure measures this directly).
- Net P&L ≥ +$50 across the 4-week window. (10 profitable fires
  × ~$5 each minus losers + fees ≈ $50.)
- Zero insider-trading incidents (we run only post-public sources).

If metrics not met, root-cause:
- Low fire rate → polling cadence or active-markets list issue
- Low hit rate → classifier confidence threshold or prompt issue
- Negative net P&L despite fires → fee math or per-fire size cap

## Out of scope (this plan)

- True breaking-news race against institutional desks. Need NY4
  + Reuters wire to compete; not feasible at current budget.
- Twitter/X firehose. Cost-prohibitive at $100/mo budget.
- Voice transcription of Fed pressers. Different design problem;
  separate plan if pursued.
- Polymarket comment / forum text mining. Speculative; defer until
  Phase 1-2 prove the pipeline.
- Cross-platform regime sizing from `~/code/tradegy`. Separate
  pipeline; tracked in `~/code/MOONSHOT_PLAN.md`.

## Estimated total effort

- **Phase 1** (scheduled releases): 1-2 sessions, ~600 LoC.
- **Phase 2** (Benzinga): 1 session + Benzinga subscription. Reuses
  Phase 1 classifier path.
- **Phase 3** (sports state): 2-3 sessions, ~800 LoC; needs ML
  baseline.
- **Phase 4** (Twitter): deferred indefinitely.

Recommendation: **ship Phase 1 first**, observe a full FOMC + CPI
cycle, then decide whether to invest in Phase 2 + 3.

## Decision gates

- **Before Phase 1 deploy**: confirm Kalshi has at least 5 active
  markets per scheduled release type. Without active markets, the
  classifier has nothing to map news to. (Verified empirically;
  KXCPIYOY, KXFEDDECISION, KXEMPLOYRATE, etc. exist.)
- **Before Phase 2 deploy**: Phase 1 hit rate must be ≥60% to
  justify the Benzinga $37/mo subscription.
- **Before Phase 3 deploy**: settlement strategy must be running
  cleanly so the sports classifier can complement it without
  competing for the same fires.

## Open questions for operator

- Comfort level with the post-2020 BLS / Fed direct-poll-public-
  page approach? (No legal issue; just confirming.)
- Phase 2 Benzinga $37/mo committed before Phase 1 proves out, or
  conditional on Phase 1 hit rate?
- Strict sports universe limit, or open-ended? (ESPN's hidden
  API has no rate limit but breaks without notice.)
