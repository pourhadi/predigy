//! Per-batch keyword-overlap pre-filter.
//!
//! Kalshi has tens of thousands of open markets across politics +
//! economics + world + elections — far too many to send to Claude
//! in a single Anthropic call (we hit the 1M token context cap on
//! the first run). Most Kalshi markets are also semantically
//! unrelated to any given Polymarket batch.
//!
//! This module filters the Kalshi list down per-batch by requiring
//! at least one shared content word with the Polymarket batch
//! (title + first slice of description). It is intentionally cheap
//! and lossy — the agent still has the final say on whether a pair
//! is a real twin. False positives (semantically unrelated markets
//! that happen to share a word) just cost a few tokens; false
//! negatives are the real cost, so the keyword extraction is broad.

use crate::kalshi_scan::KalshiMarket;
use crate::poly_scan::PolyMarket;
use std::collections::HashSet;

/// Hard cap on Kalshi markets sent per Anthropic call. Even at 4
/// chars/word and 300 markets the prompt stays under ~50k tokens
/// after the system prompt + Polymarket batch.
const MAX_KALSHI_PER_BATCH: usize = 300;

/// Tokens that should never count as a semantic match — too common
/// to be informative.
const STOPWORDS: &[&str] = &[
    "a",
    "an",
    "and",
    "are",
    "as",
    "at",
    "be",
    "by",
    "for",
    "from",
    "has",
    "have",
    "in",
    "is",
    "it",
    "its",
    "of",
    "on",
    "or",
    "than",
    "that",
    "the",
    "this",
    "to",
    "was",
    "were",
    "will",
    "with",
    "before",
    "after",
    "above",
    "below",
    "between",
    "next",
    "last",
    "first",
    "year",
    "month",
    "week",
    "day",
    "today",
    "tomorrow",
    "many",
    "more",
    "most",
    "any",
    "all",
    "no",
    "yes",
    "win",
    "wins",
    "winner",
    "win-loss",
    "over",
    "under",
    "vs",
    "or",
    // Date-ish noise.
    "2024",
    "2025",
    "2026",
    "2027",
    "2028",
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
    // Question-ish noise.
    "who",
    "what",
    "when",
    "where",
    "which",
    "how",
    "be",
    "do",
    "does",
];

fn tokenize(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split(|c: char| !c.is_ascii_alphanumeric())
        .map(str::to_ascii_lowercase)
        .filter(|w| w.len() >= 4 && !STOPWORDS.contains(&w.as_str()))
}

fn keyword_set(poly: &[PolyMarket]) -> HashSet<String> {
    let mut set = HashSet::new();
    for p in poly {
        for w in tokenize(&p.question) {
            set.insert(w);
        }
        // First chunk of description tends to name the entities +
        // resolution event; later text is boilerplate.
        let head: String = p.description.chars().take(200).collect();
        for w in tokenize(&head) {
            set.insert(w);
        }
    }
    set
}

/// Return only the Kalshi markets whose title shares at least one
/// non-stopword token with the Polymarket batch. Caps at
/// `MAX_KALSHI_PER_BATCH` to keep the prompt under model context.
pub fn filter_for_batch(kalshi: &[KalshiMarket], poly: &[PolyMarket]) -> Vec<KalshiMarket> {
    let kw = keyword_set(poly);
    if kw.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<KalshiMarket> = kalshi
        .iter()
        .filter(|k| {
            tokenize(&k.title).any(|w| kw.contains(&w))
                || tokenize(&k.event_ticker).any(|w| kw.contains(&w))
        })
        .cloned()
        .collect();
    // Stable sort by ticker so we hit the cap deterministically.
    hits.sort_by(|a, b| a.ticker.cmp(&b.ticker));
    hits.truncate(MAX_KALSHI_PER_BATCH);
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(ticker: &str, title: &str) -> KalshiMarket {
        KalshiMarket {
            ticker: ticker.into(),
            event_ticker: ticker.into(),
            title: title.into(),
            close_time: "2026-12-31T00:00:00Z".into(),
            yes_ask_cents: 50,
            no_ask_cents: 50,
        }
    }
    fn p(question: &str, desc: &str) -> PolyMarket {
        PolyMarket {
            id: "x".into(),
            question: question.into(),
            description: desc.into(),
            yes_token_id: "1".repeat(40),
            end_date_iso: None,
            yes_price: 0.5,
            no_price: 0.5,
            volume_num: 0.0,
            liquidity_num: 0.0,
        }
    }

    #[test]
    fn keeps_keyword_overlap() {
        let kalshi = vec![
            k("KX-FED", "Fed rate hike in December"),
            k("KX-NBA", "Lakers win NBA finals"),
        ];
        let poly = vec![p("Federal Reserve rate decision", "FOMC")];
        let out = filter_for_batch(&kalshi, &poly);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].ticker, "KX-FED");
    }

    #[test]
    fn drops_when_no_overlap() {
        let kalshi = vec![k("KX-A", "Hurricane forms")];
        let poly = vec![p("Election outcome", "President wins")];
        let out = filter_for_batch(&kalshi, &poly);
        assert!(out.is_empty());
    }

    #[test]
    fn caps_at_max_per_batch() {
        let kalshi: Vec<_> = (0..(MAX_KALSHI_PER_BATCH + 50))
            .map(|i| k(&format!("KX-{i}"), "election president"))
            .collect();
        let poly = vec![p("president election", "")];
        let out = filter_for_batch(&kalshi, &poly);
        assert_eq!(out.len(), MAX_KALSHI_PER_BATCH);
    }
}
