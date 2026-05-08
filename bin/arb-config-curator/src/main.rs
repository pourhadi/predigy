//! `arb-config-curator` — keeps `implication-arb-config.json` and
//! `internal-arb-config.json` non-stale.
//!
//! ## Why this exists
//!
//! `implication-arb` and `internal-arb` strategies hot-reload
//! their config files via mtime poll (30 s by default). The
//! READ-side automation works. But until this binary, the
//! WRITE-side was operator-by-hand: configs accumulated dead
//! tickers from already-settled events (KXPAYROLLS-26APR after
//! April settled, KXMLBGAME for last week's game) and the live
//! strategies effectively had no fireable universe.
//!
//! This curator runs on cron, validates every ticker referenced
//! in the live configs against Kalshi REST `status=open`, drops
//! settled/closed entries, and seeds new entries from currently
//! active monotonic event series (KXPAYROLLS, KXTORNADO,
//! KXECONSTATU3, KXEMPLOYRATE) and 2-leg mutually-exclusive
//! families (KXMLBGAME, KXNBASERIES). Atomic-rename writes; the
//! strategies' existing mtime-watch picks up the new state.
//!
//! ## What this binary deliberately does NOT do
//!
//! - Does not auto-discover NEW implication patterns from
//!   observation data alone. The pattern (parent⊃child) must be
//!   logically true, not statistically inferred. The seed list
//!   below is hand-curated for monotonic threshold ladders that
//!   are unambiguously monotonic by construction.
//! - Does not auto-discover internal-arb families that aren't
//!   provably exhaustive. Two-leg sports outcomes are safe; an
//!   N-way race could leave a non-exhaustive family that
//!   silently leaks edge.
//! - Does not modify strategy code. Output is always a config
//!   file the existing reload path consumes.
//!
//! ## CLI
//!
//! ```text
//! arb-config-curator \
//!     --implication-config ~/.config/predigy/implication-arb-config.json \
//!     --internal-config    ~/.config/predigy/internal-arb-config.json \
//!     --write
//! ```
//!
//! Without `--write`, the proposed configs are printed to stdout
//! (dry-run). With `--write`, they're atomically renamed into
//! place.

use anyhow::{Context as _, Result, anyhow};
use clap::Parser;
use predigy_kalshi_rest::types::MarketSummary;
use predigy_kalshi_rest::{Client as RestClient, Signer};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "arb-config-curator",
    about = "Keep implication-arb / internal-arb config files non-stale."
)]
struct Args {
    #[arg(long, env = "KALSHI_KEY_ID")]
    kalshi_key_id: Option<String>,
    #[arg(long, env = "KALSHI_PEM")]
    kalshi_pem: Option<PathBuf>,
    #[arg(long)]
    kalshi_rest_endpoint: Option<String>,

    #[arg(
        long,
        env = "PREDIGY_IMPLICATION_ARB_CONFIG",
        default_value = "~/.config/predigy/implication-arb-config.json"
    )]
    implication_config: PathBuf,

    #[arg(
        long,
        env = "PREDIGY_INTERNAL_ARB_CONFIG",
        default_value = "~/.config/predigy/internal-arb-config.json"
    )]
    internal_config: PathBuf,

    /// Atomic-rename writes both configs. Without this, prints
    /// the proposed JSON to stdout for review.
    #[arg(long, default_value_t = false)]
    write: bool,
}

/// Series prefixes whose markets are organised as monotonic
/// threshold ladders (`-T<value>` suffix), where the higher
/// threshold YES implies the lower threshold YES.
///
/// Convention enforced by this curator:
/// - `KXPAYROLLS-{YYYYMMM}-T{N}` — payrolls *above* N → lower
///   thresholds also above by construction.
/// - `KXTORNADO-{YYYYMMM}-{N}` — count *above* N (no T prefix
///   on the threshold).
/// - `KXECONSTATU3-{YYYYMMM}-T{X.Y}` — unemployment rate
///   *above* X.Y%.
/// - `KXEMPLOYRATE-{YYYYMMM}-T{X.Y}` — same shape.
///
/// Add a new series here only after verifying that "higher
/// strike YES implies lower strike YES" holds for that series.
const MONOTONIC_LADDER_SERIES: &[&str] =
    &["KXPAYROLLS", "KXTORNADO", "KXECONSTATU3", "KXEMPLOYRATE"];

/// Series whose events are 2-leg mutually-exclusive (one-or-the-
/// other; sum-to-1 by construction). Each event's two markets
/// form an internal-arb family.
const TWO_LEG_FAMILY_SERIES: &[&str] = &["KXMLBGAME", "KXNBASERIES", "KXNHLGAME"];

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ImplicationConfig {
    #[serde(rename = "_comment", default, skip_serializing_if = "Option::is_none")]
    comment: Option<String>,
    pairs: Vec<ImplicationPair>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ImplicationPair {
    pair_id: String,
    parent: String,
    child: String,
    #[serde(rename = "_comment", default, skip_serializing_if = "Option::is_none")]
    comment: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InternalArbConfig {
    #[serde(rename = "_comment", default, skip_serializing_if = "Option::is_none")]
    comment: Option<String>,
    families: Vec<InternalArbFamily>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InternalArbFamily {
    family_id: String,
    tickers: Vec<String>,
    #[serde(rename = "_comment", default, skip_serializing_if = "Option::is_none")]
    comment: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();
    let imp_path = expand_tilde(&args.implication_config);
    let int_path = expand_tilde(&args.internal_config);

    let rest = build_rest_client(&args).await?;

    let imp_existing = load_implication(&imp_path)?;
    let int_existing = load_internal(&int_path)?;
    info!(
        existing_pairs = imp_existing.pairs.len(),
        existing_families = int_existing.families.len(),
        "loaded existing configs"
    );

    let active = collect_active_markets(&rest).await?;
    info!(
        active_tickers = active.len(),
        ladder_series = MONOTONIC_LADDER_SERIES.len(),
        family_series = TWO_LEG_FAMILY_SERIES.len(),
        "kalshi snapshot complete"
    );

    let imp_next = refresh_implication(&imp_existing, &active);
    let int_next = refresh_internal(&int_existing, &active);

    info!(
        kept_pairs = imp_next.pairs.len(),
        prev_pairs = imp_existing.pairs.len(),
        kept_families = int_next.families.len(),
        prev_families = int_existing.families.len(),
        "refresh complete"
    );

    if args.write {
        write_atomic(&imp_path, &imp_next).context("write implication config")?;
        write_atomic(&int_path, &int_next).context("write internal-arb config")?;
        info!(
            implication_path = %imp_path.display(),
            internal_path = %int_path.display(),
            "configs written"
        );
    } else {
        println!("# DRY RUN — implication-arb-config.json proposal\n");
        println!("{}", serde_json::to_string_pretty(&imp_next)?);
        println!("\n# DRY RUN — internal-arb-config.json proposal\n");
        println!("{}", serde_json::to_string_pretty(&int_next)?);
        println!("\n# Pass --write to commit.");
    }

    Ok(())
}

/// Walk every ladder + family series, return the set of tickers
/// currently `status=open` keyed by ticker, with the parsed
/// strike value (numeric where present) for ladder ordering.
async fn collect_active_markets(rest: &RestClient) -> Result<HashMap<String, ActiveMarket>> {
    let mut out: HashMap<String, ActiveMarket> = HashMap::new();
    for s in MONOTONIC_LADDER_SERIES
        .iter()
        .chain(TWO_LEG_FAMILY_SERIES.iter())
    {
        let mut next: Option<String> = None;
        loop {
            let resp = rest
                .list_markets_in_series(s, Some("open"), Some(1000), next.as_deref())
                .await
                .map_err(|e| anyhow!("kalshi {s}: {e}"))?;
            for m in resp.markets {
                out.insert(m.ticker.clone(), ActiveMarket::from_summary(&m, s));
            }
            match resp.cursor.as_deref() {
                Some(c) if !c.is_empty() => next = Some(c.to_string()),
                _ => break,
            }
        }
    }
    Ok(out)
}

#[derive(Debug, Clone)]
struct ActiveMarket {
    ticker: String,
    event_ticker: String,
    series_prefix: &'static str,
    /// Numeric strike parsed from the ticker suffix; `None` if
    /// the ticker doesn't match the strike-suffix pattern.
    strike: Option<f64>,
}

impl ActiveMarket {
    fn from_summary(m: &MarketSummary, series_prefix: &'static str) -> Self {
        Self {
            ticker: m.ticker.clone(),
            event_ticker: m.event_ticker.clone(),
            series_prefix,
            strike: parse_strike_suffix(&m.ticker),
        }
    }
}

/// Extract the numeric strike from a Kalshi ticker like
/// `KXPAYROLLS-26MAY-T125000` → `125000.0`. Returns `None` if
/// the suffix doesn't parse cleanly.
fn parse_strike_suffix(ticker: &str) -> Option<f64> {
    let last = ticker.rsplit('-').next()?;
    let stripped = last.strip_prefix('T').unwrap_or(last);
    stripped.parse::<f64>().ok()
}

fn refresh_implication(
    existing: &ImplicationConfig,
    active: &HashMap<String, ActiveMarket>,
) -> ImplicationConfig {
    let mut pairs: Vec<ImplicationPair> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // 1. keep existing pairs whose both tickers are still active
    for p in &existing.pairs {
        if active.contains_key(&p.parent) && active.contains_key(&p.child) {
            seen.insert(format!("{}/{}", p.parent, p.child));
            pairs.push(p.clone());
        } else {
            warn!(
                parent = %p.parent,
                child = %p.child,
                pair_id = %p.pair_id,
                "dropping settled / closed implication pair"
            );
        }
    }

    // 2. seed adjacent monotonic threshold ladders for active series
    let mut by_event: HashMap<(&str, String), Vec<&ActiveMarket>> = HashMap::new();
    for m in active.values() {
        if !MONOTONIC_LADDER_SERIES.contains(&m.series_prefix) {
            continue;
        }
        if m.strike.is_none() {
            continue;
        }
        by_event
            .entry((m.series_prefix, m.event_ticker.clone()))
            .or_default()
            .push(m);
    }
    for ((prefix, _event), mut markets) in by_event {
        // Sort ascending by strike. Adjacent pair: parent=lower
        // strike, child=higher strike (because `child YES`
        // ⊆ `parent YES` for monotonic-above series).
        markets.sort_by(|a, b| {
            a.strike
                .partial_cmp(&b.strike)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let _ = prefix;
        for window in markets.windows(2) {
            let parent = window[0];
            let child = window[1];
            let key = format!("{}/{}", parent.ticker, child.ticker);
            if seen.contains(&key) {
                continue;
            }
            seen.insert(key);
            pairs.push(ImplicationPair {
                pair_id: short_pair_id(&parent.ticker, &child.ticker),
                parent: parent.ticker.clone(),
                child: child.ticker.clone(),
                comment: None,
            });
        }
    }

    pairs.sort_by(|a, b| a.pair_id.cmp(&b.pair_id));
    ImplicationConfig {
        comment: Some(
            "Auto-curated by arb-config-curator. Threshold-chain markets where higher \
             threshold YES implies lower threshold YES."
                .to_string(),
        ),
        pairs,
    }
}

fn refresh_internal(
    existing: &InternalArbConfig,
    active: &HashMap<String, ActiveMarket>,
) -> InternalArbConfig {
    let mut families: Vec<InternalArbFamily> = Vec::new();
    let mut seen_event: HashSet<String> = HashSet::new();

    // 1. keep existing families whose tickers are all still active
    for f in &existing.families {
        let all_active = f.tickers.iter().all(|t| active.contains_key(t));
        if all_active {
            // Track event_ticker so we don't re-seed on top of it.
            if let Some(et) = f
                .tickers
                .first()
                .and_then(|t| active.get(t).map(|m| m.event_ticker.clone()))
            {
                seen_event.insert(et);
            }
            families.push(f.clone());
        } else {
            warn!(
                family_id = %f.family_id,
                "dropping settled / closed internal-arb family"
            );
        }
    }

    // 2. seed 2-leg families from active mutually-exclusive series
    let mut by_event: HashMap<String, Vec<&ActiveMarket>> = HashMap::new();
    for m in active.values() {
        if !TWO_LEG_FAMILY_SERIES.contains(&m.series_prefix) {
            continue;
        }
        by_event.entry(m.event_ticker.clone()).or_default().push(m);
    }
    for (event, markets) in by_event {
        if seen_event.contains(&event) {
            continue;
        }
        // Only seed events with EXACTLY 2 markets — verifies the
        // 2-leg-family assumption holds for this event.
        if markets.len() != 2 {
            continue;
        }
        let mut tickers: Vec<String> = markets.iter().map(|m| m.ticker.clone()).collect();
        tickers.sort();
        families.push(InternalArbFamily {
            family_id: event.clone(),
            tickers,
            comment: None,
        });
        seen_event.insert(event);
    }

    families.sort_by(|a, b| a.family_id.cmp(&b.family_id));
    InternalArbConfig {
        comment: Some(
            "Auto-curated by arb-config-curator. Mutually-exclusive 2-leg event \
             families."
                .to_string(),
        ),
        families,
    }
}

fn short_pair_id(parent: &str, child: &str) -> String {
    let p = parent.split('-').next_back().unwrap_or(parent);
    let c = child.split('-').next_back().unwrap_or(child);
    let event = parent.rsplit_once('-').map_or(parent, |x| x.0);
    format!("{event}-{p}-vs-{c}")
}

fn load_implication(path: &std::path::Path) -> Result<ImplicationConfig> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(serde_json::from_str(&s).context("parse implication-arb config")?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ImplicationConfig {
            comment: None,
            pairs: vec![],
        }),
        Err(e) => Err(e.into()),
    }
}

fn load_internal(path: &std::path::Path) -> Result<InternalArbConfig> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(serde_json::from_str(&s).context("parse internal-arb config")?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(InternalArbConfig {
            comment: None,
            families: vec![],
        }),
        Err(e) => Err(e.into()),
    }
}

fn write_atomic<T: Serialize>(path: &std::path::Path, data: &T) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    let body = serde_json::to_string_pretty(data).expect("serialize");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

async fn build_rest_client(args: &Args) -> Result<RestClient> {
    if let (Some(key), Some(pem)) = (&args.kalshi_key_id, &args.kalshi_pem) {
        let pem_text = std::fs::read_to_string(expand_tilde(pem))
            .with_context(|| format!("read PEM at {}", pem.display()))?;
        let signer = Signer::from_pem(key, &pem_text).map_err(|e| anyhow!("signer: {e}"))?;
        if let Some(base) = &args.kalshi_rest_endpoint {
            RestClient::with_base(base, Some(signer))
        } else {
            RestClient::authed(signer)
        }
        .map_err(|e| anyhow!("rest: {e}"))
    } else {
        // Read-only public endpoints are sufficient for our calls.
        if let Some(base) = &args.kalshi_rest_endpoint {
            RestClient::with_base(base, None)
        } else {
            RestClient::public()
        }
        .map_err(|e| anyhow!("rest: {e}"))
    }
}

fn expand_tilde(p: &std::path::Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    p.to_path_buf()
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}
