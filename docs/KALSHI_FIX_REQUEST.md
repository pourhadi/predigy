# Kalshi FIX access — onboarding research + email draft

## What we know about the process

Sources: [docs.kalshi.com/fix/connectivity](https://docs.kalshi.com/fix/connectivity),
[institutional.kalshi.com](https://institutional.kalshi.com/),
[help.kalshi.com](https://help.kalshi.com/en/articles/13823854-kalshi-api).

1. **Contact**: `institutional@kalshi.com` for both institutional
   onboarding and FIX-specific access.
2. **Prerequisite**: An institutional / entity account is the
   default path for FIX. Required documents per
   `institutional.kalshi.com`:
   - Entity formation documents
   - Tax documentation (W-9 or W-8)
   - Source of funds
   - Beneficial owner IDs (10%+ ownership)
   - All in English or certified translation.
3. **FIX is gated**: per Kalshi's docs, "available to qualifying
   members who meet minimum activity and technical
   requirements." The specific bar isn't published — has to be
   asked.
4. **Demo first**: `fix.demo.kalshi.co` is offered for test-
   driven onboarding before prod credentials.
5. **Prod gateway**: `mm.fix.elections.kalshi.com`. Five port-
   specific endpoints depending on use-case (order entry NR/RT,
   drop copy, post trade, RFQ).
6. **Wire format**: FIXT.1.1 + FIX 5.0 SP2, TLS 1.2+ mandatory,
   per-key single-connection. Kalshi provides cert for pinning
   on initiator side. SenderCompID = the FIX API Key (UUID).

The technical bar (TLS, RSA signing, sequence-number management,
proper logon semantics, ExecutionReport handling) is a thing they
gate on — partly to keep retail / poorly-implemented bots off the
FIX gateway.

## Predigy's posture for the request

What we have that demonstrates we're real:
- Already trading on Kalshi via REST + WS for ~weeks; live OMS,
  positions, fills.
- Multi-strategy architecture (5 strategy lanes: stat, weather,
  cross-arb, settlement, NBM-derived weather) with shared OMS,
  Postgres-backed state, idempotent client-id design.
- Existing FIX implementation in Rust (`crates/kalshi-fix`,
  ~1,700 lines covering session, executor, messages, tags) —
  written against FIX 4.4; happy to update to 5.0 SP2 with the
  spec.

What we don't have yet:
- Entity / LLC. Currently an individual trader.
- Significant volume history — at $50 cap by deliberate choice
  during the shake-down phase, ramping up.

The honest ask is: what's the minimum bar, what does the
application path look like for a sophisticated individual
trader who's prepared to incorporate / increase volume, and
can we start in the demo environment now so we're production-
ready when we hit whatever activity threshold they require.

## Email draft

```
To: institutional@kalshi.com
Subject: FIX gateway access request — predigy automated trading platform

Hi,

I'm writing to inquire about FIX gateway access for my Kalshi
trading account (key id a381c833-6172-4b19-a27e-a0b2345f86c7,
account email dan@pourhadi.com). Currently using the REST + WS
APIs in production; would like to graduate to FIX for the
latency edge on news-driven and cross-venue strategies.

A bit on what we've built and what we'd use FIX for:

- Predigy is a multi-strategy automated trading platform in
  Rust. Five live strategy lanes (model-based statistical
  bets, NWS-alert-driven weather, cross-Polymarket arbitrage,
  pre-settlement mispricing capture, NBM probabilistic
  weather). Shared OMS with idempotent client-ids, Postgres-
  backed state, transactional fill cascades to positions.

- We've already implemented a Kalshi FIX session in Rust
  (~1,700 lines covering session lifecycle, ExecutionReport
  parsing, RSA-PSS-SHA256 signing on logon, sequence-number
  management). Coded initially against FIX 4.4; happy to bring
  it forward to FIXT.1.1 + FIX 5.0 SP2 to match Kalshi's
  current spec.

- Primary use-case is the Order Entry NR gateway (port 8228,
  KalshiNR). The latency edge over REST matters most for
  news-data races (NWS active-alert fires) and cross-venue
  spread capture against Polymarket.

A few questions I'd appreciate guidance on:

1. What's the minimum-activity / technical-readiness bar for
   FIX access? We're trading at deliberate small size during a
   shake-down phase but ramping up; would like to understand
   what threshold qualifies us.

2. Is the demo environment (fix.demo.kalshi.co) available for
   integration testing ahead of meeting the activity bar? We'd
   like to validate session lifecycle, logon, ExecutionReport
   handling, etc. against your sandbox before we ask for prod
   credentials.

3. We're currently an individual account, not an entity. Is
   FIX access categorically gated on entity status, or is
   there a path for a sophisticated individual? If entity is
   required, we're prepared to incorporate; just want to know
   before starting that process.

4. Anything you'd like to see in advance of the application —
   architecture summary, FIX message flow diagrams, code
   samples — happy to share.

Thanks,
Dan Pourhadi
dan@pourhadi.com
```

## Tweaks before sending

- **Verify the account email** matches what Kalshi has on file
  (the request will get tied to the account holder).
- **The account-key id** in the draft is from `~/.zprofile`
  (`KALSHI_KEY_ID`); confirm before sending.
- **If you've already incorporated**, drop question 3 and say
  "trading via [Entity Name]" up top.
- **Length**: this is at the upper bound of "polite + thorough".
  Could trim to 3 questions. Don't trim below 2 — they need
  enough to assess seriousness.
- **Tone**: technical but not overdoing it. The "1,700 lines of
  FIX code already written" + specific gateway/port/spec
  references will read as serious to whoever triages the inbox.

## What to do once they reply

1. **If they ask for entity status first**: that's the long
   path (LLC formation, banking, KYC docs, weeks of waiting).
   Decision point: how badly do we need FIX vs ramping REST?
2. **If they grant demo access**: drop everything to validate
   our existing kalshi-fix crate against the live demo gateway.
   That'll surface FIX 4.4 → 5.0 SP2 deltas and confirm
   logon / TLS / cert-pinning all work end-to-end. ~1-2 days.
3. **If they grant prod access**: rebuild kalshi-fix to pass
   live, integrate into `predigy-engine` per Phase 4 of the
   architecture doc, run shadow→Live cutover for stat-trader,
   then port other strategies.
4. **If they decline**: REST stays the order path. Cross-arb
   and latency-trader give back ~100-500ms vs FIX, but the
   architecture doesn't fundamentally need it; the engine
   refactor's value is mostly in shared state + position
   management, not the wire protocol.
