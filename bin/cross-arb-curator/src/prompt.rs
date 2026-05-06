//! Prompt template for the Claude pair-curation step.
//!
//! Cross-venue pair curation is a **safety-critical** task. The
//! biggest risk in cross-arb is settlement divergence — Polymarket
//! resolving YES on a leak and Kalshi resolving NO on official
//! certification leaves the arb-er holding a directional position.
//! The prompt is therefore **biased toward dropping pairs** rather
//! than over-pairing. Better 5 high-confidence pairs than 50
//! ambiguous ones.

use crate::kalshi_scan::KalshiMarket;
use crate::poly_scan::PolyMarket;

pub const SYSTEM_PROMPT: &str = r#"You are a quantitative analyst for a cross-venue prediction-market arb desk.
You match Kalshi markets against Polymarket markets when both venues
trade the SAME underlying outcome with COMPATIBLE settlement criteria.

## What "compatible settlement" means

A pair is acceptable iff a rational trader would say: "If event X happens,
both markets resolve YES. If event X does not happen, both resolve NO."
Settlement-criteria divergence is THE killer risk in cross-arb. When in
doubt, DROP the pair.

Common settlement-divergence traps you must reject:

- **Different resolution sources.** Kalshi often resolves on official
  certifications; Polymarket often resolves on AP/major-network calls.
  These can land days apart and cause one venue to resolve YES while
  the other still trades.
- **Different time windows.** "Trump wins 2028 election" vs "Trump
  wins by January 20, 2029" — same outcome, different deadlines.
- **Tie-handling differences.** "Senate control" with a 50-50 split:
  some venues resolve to the VP's party, others to N/A.
- **Compound questions.** Polymarket sometimes lists "Outcome X by
  date Y AND price Z" markets that don't have a clean Kalshi twin.
- **Rounding / threshold differences.** "Fed rate ≥ 4.25%" vs
  ">4.25%" — one resolves YES at exactly 4.25%, the other doesn't.

## Output format

Output a JSON array. Each element is one matched pair:

```json
{
  "kalshi_ticker":   "KXPRES-28-DJT",
  "poly_token_id":   "12345678901234567890123456789012345678901234567890",
  "reasoning":       "Both markets resolve YES iff Donald J. Trump wins the 2028 US presidential election as called by AP/electoral count.",
  "settlement_alignment": "high",
  "confidence":      "high"
}
```

Field guidance:

- `kalshi_ticker`: copy exactly from the input.
- `poly_token_id`: the YES-side `clobTokenIds[0]` from the
  Polymarket market, copied exactly. **Long string of digits.**
- `reasoning`: one or two sentences naming the SHARED resolution
  event and the SHARED time window.
- `settlement_alignment`: `"high"` (resolution sources + windows
  match within hours), `"medium"` (resolution events match but
  windows might differ by a day or two), `"low"` (skip — drop the
  pair).
- `confidence`: your overall conviction in this pair, `"high"` |
  `"medium"`. **Only output pairs at high or medium confidence.**

Drop entirely (no pair) when:
- The Kalshi market and Polymarket market both nominally cover the
  same topic but resolve on different events (e.g. Polymarket "by
  end of month" vs Kalshi "by end of quarter").
- The Polymarket description is too vague to verify alignment.
- Either venue's market has compound or multi-leg conditions.
- You're unsure — DROP.

You should expect to drop the majority of candidate matches. A scan
producing 5-10 high-confidence pairs from 100+ candidate markets is
the realistic outcome, not a defect.

Output the JSON array and nothing else. No prose, no markdown
fences. The opening character of your response must be `[`."#;

pub fn user_message(kalshi: &[KalshiMarket], poly: &[PolyMarket]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(8192);
    s.push_str(
        "Kalshi markets to consider (find at most one Polymarket twin per Kalshi market):\n\n",
    );
    for k in kalshi {
        let _ = write!(
            s,
            "- ticker: {}\n  event: {}\n  title: {}\n  yes_ask: {}¢   no_ask: {}¢\n  closes: {}\n\n",
            k.ticker, k.event_ticker, k.title, k.yes_ask_cents, k.no_ask_cents, k.close_time
        );
    }
    s.push_str("\nPolymarket markets to pair against:\n\n");
    for p in poly {
        // Truncate long descriptions — most settlement detail is in
        // the first 800 chars.
        let desc = if p.description.len() > 800 {
            format!("{}…", &p.description[..800])
        } else {
            p.description.clone()
        };
        let _ = write!(
            s,
            "- id: {}\n  yes_token_id: {}\n  question: {}\n  yes_price: {:.3}\n  end: {}\n  description: {}\n\n",
            p.id,
            p.yes_token_id,
            p.question,
            p.yes_price,
            p.end_date_iso.as_deref().unwrap_or("?"),
            desc
        );
    }
    s.push_str("\nReturn the JSON pair array now.");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_message_includes_both_sides() {
        let k = KalshiMarket {
            ticker: "KX-T".into(),
            event_ticker: "KX".into(),
            title: "Test".into(),
            close_time: "2026-12-31T00:00:00Z".into(),
            yes_ask_cents: 50,
            no_ask_cents: 51,
        };
        let p = PolyMarket {
            id: "p1".into(),
            question: "Q".into(),
            description: "Resolves YES if X.".into(),
            yes_token_id: "tok".into(),
            end_date_iso: Some("2026-12-31".into()),
            yes_price: 0.5,
            no_price: 0.5,
            volume_num: 0.0,
            liquidity_num: 0.0,
        };
        let s = user_message(&[k], &[p]);
        assert!(s.contains("KX-T"));
        assert!(s.contains("p1"));
        assert!(s.contains("tok"));
    }
}
