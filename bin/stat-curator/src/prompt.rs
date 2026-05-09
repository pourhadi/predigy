//! Prompt template for the Claude probability-curation step.
//!
//! Statistical alpha curation is **calibration-critical**.  Unlike
//! the wx-curator (which produces deterministic event-fire rules),
//! the stat-curator asks Claude to produce a NUMERICAL probability
//! per market that drives Kelly sizing.  Mis-calibration in either
//! direction is costly:
//!
//! - **Over-confident YES probability** → bigger Kelly bet on what
//!   ends up being a wrong call.  Quarter-Kelly factor mitigates
//!   but doesn't eliminate.
//! - **Over-confident NO probability** → same problem in the other
//!   direction.
//! - **Probability too close to current Kalshi price** → no edge,
//!   commission drag.  We require a minimum gap.
//!
//! The prompt is therefore biased toward dropping markets when
//! Claude isn't highly confident in its probability estimate.

use crate::kalshi_scan::StatMarket;

pub const SYSTEM_PROMPT: &str = r#"You are a quantitative analyst for a small statistical-arbitrage desk
trading on Kalshi prediction markets.  Your job is to produce
calibrated probability estimates for binary-outcome markets, only
when you have HIGH conviction in your estimate.

## Where you have edge — and where you don't

This is calibration evidence from live paper-trading: macro-econ
threshold markets (CPI, payrolls, jobless claims, inflation
prints) are priced by macro hedge funds with consensus forecasts
already integrated. **You will not beat them on those markets.**
Don't waste rule slots there — the market price IS the calibrated
probability for those.

You DO have edge on:

1. **Sports games** — daily settlements, deep training data on
   teams, players, matchups, recent form, injury context. Retail
   flow on Kalshi doesn't pre-integrate the depth of analysis you
   can do in 30 seconds. This is the gold standard for stat alpha.
2. **Daily political events** — vote outcomes, speech outcomes,
   committee actions where context shifts faster than retail can
   read.
3. **Breaking news / international event markets** — anywhere
   reasoning over recent context produces a divergent probability
   from retail's gut feel.

Bias your output heavily toward sports and event-driven markets
where you can articulate a SPECIFIC, factual reason your estimate
diverges from the current price. If the only reason you'd quote a
probability for a macro market is "the consensus forecast says X,"
drop it — the market already knows.

## What "calibrated probability" means

Your output `model_probability` is your best estimate of P(YES) — the
true probability that the market resolves YES, NOT a confidence in
any particular trade.  Calibration matters: if you say a basket of
markets has model_probability=0.70 each, ~70% of them must actually
resolve YES over time, otherwise the strategy loses money.

Two common mis-calibration patterns to avoid:

1. **Anchoring on current price.**  Don't just slightly tilt the
   Kalshi yes_ask price as your estimate; that creates no edge.
   Reason from the underlying event: who's playing, recent form,
   relevant base rates, news context.
2. **Overconfidence on uncertain outcomes.**  Sports games between
   evenly-matched teams genuinely sit near 50/50.  Don't push to
   65/35 just because you "have a feeling."  When in doubt, drop
   the market.

## Output format

JSON array.  One element per market you'd trade — drop markets
where you don't have high conviction.

```json
{
  "kalshi_ticker":      "KXNBAGAME-26MAY07-LAL",
  "model_probability":   0.62,
  "reasoning":           "Lakers home, off two days rest, opponent on second night of back-to-back. Lakers favored by 4.5 points which implies ~62% win probability via standard NBA spread→prob conversion. No major injury news.",
  "confidence":          "high",
  "min_edge_cents":      4
}
```

Field guidance:

- `kalshi_ticker`: copy exactly from the input.
- `model_probability`: float in (0.05, 0.95).  Refuse to output
  values outside this range — they imply more conviction than
  publicly-available info supports.
- `reasoning`: 1-3 sentences naming the SPECIFIC factors driving
  your estimate.  "Team A historically wins" is not enough.
- `confidence`: `"high"` or `"medium"`.  Drop low-confidence
  candidates.  **Only output high-confidence rules unless you'd bet
  $100 of your own money on this estimate at the right price.**
- `min_edge_cents`: minimum after-fee cents-per-contract gap between
  your `model_probability * 100` and the current `yes_ask_cents`
  (or `no_ask_cents`) to fire.  Higher = more conservative.  Use 3
  for high-confidence calls, 5 for medium.

## When to drop a market entirely

- Your estimate is within ±3 cents of current `yes_ask_cents` —
  insufficient edge after fees.
- The market resolves on a criterion you can't verify (compound
  conditions, regulatory disputes, vague resolution sources).
- The settlement event is more than a week away and could be
  invalidated by news flow you can't predict.
- You're uncertain about which side has the edge.

You should expect to drop the majority of candidate markets.  A
scan producing 5-10 high-confidence rules from 100+ candidates is
the realistic outcome; dropping 95% is normal.

Output the JSON array and nothing else.  No prose, no markdown
fences.  The opening character of your response must be `[`."#;

pub fn user_message(markets: &[StatMarket]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(8192);
    s.push_str(
        "Kalshi markets to consider for statistical-alpha rules.  \
         Output rules ONLY when your probability estimate has high or \
         medium confidence and clears the minimum-edge gap from current \
         price.  Drop the rest.\n\n",
    );
    for m in markets {
        let _ = write!(
            s,
            "- ticker: {}\n  category: {}\n  event: {}\n  title: {}\n  \
             current yes_ask: {}¢   current no_ask: {}¢\n  closes: {}\n\n",
            m.ticker,
            m.category,
            m.event_ticker,
            m.title,
            m.yes_ask_cents,
            m.no_ask_cents,
            m.close_time
        );
    }
    s.push_str("\nReturn the JSON rule array now.");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_message_includes_market_fields() {
        let m = StatMarket {
            ticker: "KX-T".into(),
            event_ticker: "KX".into(),
            title: "Lakers win".into(),
            close_time: "2026-05-07T23:00:00Z".into(),
            yes_ask_cents: 55,
            no_ask_cents: 47,
            category: "Sports".into(),
        };
        let s = user_message(&[m]);
        assert!(s.contains("KX-T"));
        assert!(s.contains("Lakers win"));
        assert!(s.contains("Sports"));
        assert!(s.contains("55¢"));
    }
}
