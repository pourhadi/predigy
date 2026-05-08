// Vendor / product names appear throughout operator-facing strings.
#![allow(clippy::doc_markdown)]

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};
use predigy_book::Snapshot;
use predigy_kalshi_rest::Client as KalshiClient;
use predigy_strategy_implication_arb::{
    ImplicationArbConfig, ImplicationArbRulesFile, ImplicationTouch,
    evaluate_implication_opportunity,
};
use predigy_strategy_internal_arb::{
    InternalArbConfig, InternalArbDirection, InternalArbLegTouch, InternalArbRulesFile,
    evaluate_internal_yes_basket,
};
use predigy_strategy_settlement::SettlementConfig;
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "opportunity-scanner",
    about = "Read-only opportunity scanner: records observations, never submits orders."
)]
struct Args {
    /// Postgres DSN. Scanner writes only opportunity_observations.
    #[arg(long, env = "DATABASE_URL", default_value = "postgresql:///predigy")]
    database_url: String,

    /// Optional Kalshi REST endpoint override. Public read-only endpoints only.
    #[arg(long, env = "KALSHI_REST_ENDPOINT")]
    kalshi_rest_endpoint: Option<String>,

    #[command(subcommand)]
    cmd: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Record current configured arb universe as scanner observations.
    Arb {
        #[arg(long, env = "PREDIGY_IMPLICATION_ARB_CONFIG")]
        implication_config: Option<PathBuf>,
        #[arg(long, env = "PREDIGY_INTERNAL_ARB_CONFIG")]
        internal_config: Option<PathBuf>,
        /// Actually insert rows. Without this flag, prints the summary only.
        #[arg(long, default_value_t = false)]
        write_observations: bool,
        /// Do not fetch orderbooks; emit config-inventory observations only.
        #[arg(long, default_value_t = false)]
        skip_quotes: bool,
        /// Pace public REST orderbook fetches to avoid spending live rate budget.
        #[arg(long, default_value_t = 250)]
        quote_delay_ms: u64,
        /// Compatibility flag for launchd one-shots; the scanner always exits.
        #[arg(long, default_value_t = false)]
        once: bool,
    },
    /// Ingest the latest wx-stat curator coverage report as an observation.
    WxStat {
        #[arg(long, env = "PREDIGY_WX_STAT_COVERAGE_REPORT")]
        coverage_report: Option<PathBuf>,
        #[arg(long, default_value_t = false)]
        write_observations: bool,
    },
    /// Record the configured settlement discovery series as observations.
    Settlement {
        #[arg(long, default_value_t = false)]
        write_observations: bool,
    },
}

#[derive(Debug, Clone)]
struct Observation {
    strategy: &'static str,
    opportunity_key: String,
    tickers: Vec<String>,
    kind: &'static str,
    raw_edge_cents: Option<f64>,
    net_edge_cents: Option<f64>,
    max_size: Option<i32>,
    would_fire: bool,
    reason: String,
    payload: serde_json::Value,
}

#[derive(Debug, Clone)]
enum QuoteLookup {
    Disabled,
    Snapshot(Snapshot),
    Error(String),
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let Args {
        database_url,
        kalshi_rest_endpoint,
        cmd,
    } = Args::parse();
    match cmd {
        Command::Arb {
            implication_config,
            internal_config,
            write_observations,
            skip_quotes,
            quote_delay_ms,
            once: _,
        } => {
            let observations = collect_arb_observations(
                implication_config,
                internal_config,
                !skip_quotes,
                kalshi_rest_endpoint.as_deref(),
                Duration::from_millis(quote_delay_ms),
            )
            .await?;
            info!(n_observations = observations.len(), "arb scan complete");
            if write_observations {
                let pool = PgPoolOptions::new()
                    .max_connections(2)
                    .connect(&database_url)
                    .await
                    .with_context(|| format!("connect postgres {database_url}"))?;
                insert_observations(&pool, &observations).await?;
                println!("inserted {} opportunity observations", observations.len());
            } else {
                print_summary("dry-run", &observations);
            }
        }
        Command::WxStat {
            coverage_report,
            write_observations,
        } => {
            let observations = collect_wx_stat_observations(coverage_report)?;
            maybe_write(&database_url, write_observations, &observations).await?;
        }
        Command::Settlement { write_observations } => {
            let observations = collect_settlement_observations();
            maybe_write(&database_url, write_observations, &observations).await?;
        }
    }
    Ok(())
}

fn collect_wx_stat_observations(coverage_report: Option<PathBuf>) -> Result<Vec<Observation>> {
    let Some(path) = coverage_report else {
        return Ok(vec![Observation {
            strategy: "wx-stat",
            opportunity_key: "wx-stat-coverage-scan".to_string(),
            tickers: Vec::new(),
            kind: "coverage_report_missing",
            raw_edge_cents: None,
            net_edge_cents: None,
            max_size: None,
            would_fire: false,
            reason: "no --coverage-report provided".to_string(),
            payload: json!({"non_executing": true}),
        }]);
    };
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("read wx-stat coverage report {}", path.display()))?;
    let report: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("parse wx-stat coverage report {}", path.display()))?;
    let run_ts = report
        .get("run_ts_utc")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let rules_proposed = report
        .get("rules_proposed")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| i32::try_from(n).ok());
    Ok(vec![Observation {
        strategy: "wx-stat",
        opportunity_key: format!("wx-stat-coverage-{run_ts}"),
        tickers: Vec::new(),
        kind: "coverage_report",
        raw_edge_cents: None,
        net_edge_cents: None,
        max_size: rules_proposed,
        would_fire: false,
        reason: "ingested wx-stat curator coverage report".to_string(),
        payload: json!({
            "coverage_report_path": path,
            "report": report,
            "non_executing": true
        }),
    }])
}

fn collect_settlement_observations() -> Vec<Observation> {
    let config = SettlementConfig::from_env();
    vec![Observation {
        strategy: "settlement",
        opportunity_key: "settlement-configured-series".to_string(),
        tickers: Vec::new(),
        kind: "configured_series",
        raw_edge_cents: None,
        net_edge_cents: None,
        max_size: i32::try_from(config.series.len()).ok(),
        would_fire: false,
        reason: "configured settlement discovery series observed; no orders submitted".to_string(),
        payload: json!({
            "series": config.series,
            "discovery_interval_secs": config.discovery_interval.as_secs(),
            "max_secs_to_settle": config.max_secs_to_settle,
            "require_quote": true,
            "non_executing": true
        }),
    }]
}

async fn collect_arb_observations(
    implication_config: Option<PathBuf>,
    internal_config: Option<PathBuf>,
    fetch_quotes: bool,
    kalshi_rest_endpoint: Option<&str>,
    quote_delay: Duration,
) -> Result<Vec<Observation>> {
    let client = if fetch_quotes {
        Some(if let Some(base) = kalshi_rest_endpoint {
            KalshiClient::with_base(base, None).context("build public Kalshi REST client")?
        } else {
            KalshiClient::public().context("build public Kalshi REST client")?
        })
    } else {
        None
    };
    let mut quote_cache = HashMap::new();
    let mut out = Vec::new();

    if let Some(path) = implication_config {
        let config = ImplicationArbConfig::from_env(path.clone());
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read implication config {}", path.display()))?;
        let parsed: ImplicationArbRulesFile = serde_json::from_str(&raw)
            .with_context(|| format!("parse implication config {}", path.display()))?;
        for pair in parsed.pairs {
            if pair.parent == pair.child {
                continue;
            }
            let key = pair
                .pair_id
                .clone()
                .unwrap_or_else(|| format!("{}|{}", pair.parent, pair.child));
            let parent_quote =
                lookup_quote(client.as_ref(), &mut quote_cache, &pair.parent, quote_delay).await;
            let child_quote =
                lookup_quote(client.as_ref(), &mut quote_cache, &pair.child, quote_delay).await;
            out.push(build_implication_observation(
                &pair.parent,
                &pair.child,
                key,
                &parent_quote,
                &child_quote,
                config.size,
                config.min_edge_cents,
            ));
        }
    }

    if let Some(path) = internal_config {
        let config = InternalArbConfig::from_env(path.clone());
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read internal config {}", path.display()))?;
        let parsed: InternalArbRulesFile = serde_json::from_str(&raw)
            .with_context(|| format!("parse internal config {}", path.display()))?;
        for family in parsed.families {
            if family.tickers.len() < 2 {
                continue;
            }
            let mut quotes = Vec::with_capacity(family.tickers.len());
            for ticker in &family.tickers {
                quotes.push(
                    lookup_quote(client.as_ref(), &mut quote_cache, ticker, quote_delay).await,
                );
            }
            out.push(build_internal_observation(
                family.family_id.clone(),
                &family.tickers,
                &quotes,
                family.extra_fee_padding_cents,
                family.exhaustive,
                family.proof.as_deref(),
                &family.directions,
                config.size,
                config.min_edge_cents,
            ));
        }
    }
    Ok(out)
}

fn build_implication_observation(
    parent: &str,
    child: &str,
    opportunity_key: String,
    parent_quote: &QuoteLookup,
    child_quote: &QuoteLookup,
    size: u32,
    min_edge_cents: i32,
) -> Observation {
    let parent_touch = implication_touch(parent_quote);
    let child_touch = implication_touch(child_quote);
    let diag = parent_touch.zip(child_touch).and_then(|(p, c)| {
        // Use a floor min edge to get diagnostic edge values even when
        // the current live gate would reject the package.
        evaluate_implication_opportunity(p, c, size, i32::MIN)
    });
    let would_fire = diag
        .as_ref()
        .is_some_and(|o| o.net_edge_cents >= min_edge_cents && o.max_touch_qty > 0);
    let reason = implication_reason(parent_quote, child_quote, diag.as_ref(), min_edge_cents);

    Observation {
        strategy: "implication-arb",
        opportunity_key,
        tickers: vec![parent.to_string(), child.to_string()],
        kind: if matches!(parent_quote, QuoteLookup::Disabled) {
            "implication_pair_configured"
        } else {
            "implication_pair"
        },
        raw_edge_cents: diag.as_ref().map(|o| f64::from(o.raw_edge_cents)),
        net_edge_cents: diag.as_ref().map(|o| f64::from(o.net_edge_cents)),
        max_size: diag
            .as_ref()
            .map(|o| i32::try_from(o.max_touch_qty).unwrap_or(i32::MAX)),
        would_fire,
        reason,
        payload: json!({
            "parent": parent,
            "child": child,
            "size": size,
            "min_edge_cents": min_edge_cents,
            "parent_touch": parent_touch.map(|t| json!({
                "yes_bid_cents": t.yes_bid_cents,
                "yes_ask_cents": t.yes_ask_cents,
                "yes_bid_qty": t.yes_bid_qty,
                "yes_ask_qty": t.yes_ask_qty,
            })),
            "child_touch": child_touch.map(|t| json!({
                "yes_bid_cents": t.yes_bid_cents,
                "yes_ask_cents": t.yes_ask_cents,
                "yes_bid_qty": t.yes_bid_qty,
                "yes_ask_qty": t.yes_ask_qty,
            })),
            "non_executing": true
        }),
    }
}

fn build_internal_observation(
    family_id: String,
    tickers: &[String],
    quotes: &[QuoteLookup],
    extra_fee_padding_cents: u32,
    exhaustive: bool,
    proof: Option<&str>,
    directions: &[InternalArbDirection],
    size: u32,
    min_edge_cents: i32,
) -> Observation {
    let proof_present = proof.is_some_and(|p| !p.trim().is_empty());
    let yes_direction = directions.contains(&InternalArbDirection::YesBasket);
    let touches: Option<Vec<InternalArbLegTouch>> = quotes.iter().map(internal_touch).collect();
    let diag = touches.as_ref().and_then(|legs| {
        evaluate_internal_yes_basket(legs, size, extra_fee_padding_cents, i32::MIN)
    });
    let edge_clears = diag
        .as_ref()
        .is_some_and(|o| o.edge_cents >= min_edge_cents && o.max_touch_qty > 0);
    let would_fire = exhaustive && proof_present && yes_direction && edge_clears;
    let reason = internal_reason(
        quotes,
        diag.as_ref().map(|o| o.edge_cents),
        exhaustive,
        proof_present,
        yes_direction,
        min_edge_cents,
    );

    Observation {
        strategy: "internal-arb",
        opportunity_key: family_id.clone(),
        tickers: tickers.to_vec(),
        kind: if quotes.iter().any(|q| matches!(q, QuoteLookup::Disabled)) {
            "internal_family_configured"
        } else {
            "internal_yes_basket"
        },
        raw_edge_cents: diag.as_ref().map(|o| f64::from(100 - o.total_ask_cents)),
        net_edge_cents: diag.as_ref().map(|o| f64::from(o.edge_cents)),
        max_size: diag
            .as_ref()
            .map(|o| i32::try_from(o.max_touch_qty).unwrap_or(i32::MAX)),
        would_fire,
        reason,
        payload: json!({
            "family_id": family_id,
            "exhaustive": exhaustive,
            "proof": proof,
            "proof_present": proof_present,
            "directions": directions,
            "size": size,
            "min_edge_cents": min_edge_cents,
            "extra_fee_padding_cents": extra_fee_padding_cents,
            "touches": touches.map(|legs| legs.into_iter().map(|t| json!({
                "yes_ask_cents": t.yes_ask_cents,
                "yes_ask_qty": t.yes_ask_qty,
            })).collect::<Vec<_>>()),
            "non_executing": true
        }),
    }
}

async fn lookup_quote(
    client: Option<&KalshiClient>,
    cache: &mut HashMap<String, QuoteLookup>,
    ticker: &str,
    quote_delay: Duration,
) -> QuoteLookup {
    let Some(client) = client else {
        return QuoteLookup::Disabled;
    };
    if let Some(q) = cache.get(ticker) {
        return q.clone();
    }
    let q = match client.orderbook_snapshot(ticker).await {
        Ok(snapshot) => QuoteLookup::Snapshot(snapshot),
        Err(e) => QuoteLookup::Error(e.to_string()),
    };
    cache.insert(ticker.to_string(), q.clone());
    if quote_delay != Duration::ZERO {
        tokio::time::sleep(quote_delay).await;
    }
    q
}

fn implication_touch(quote: &QuoteLookup) -> Option<ImplicationTouch> {
    let QuoteLookup::Snapshot(snapshot) = quote else {
        return None;
    };
    let (yes_bid, yes_bid_qty) = best_level(&snapshot.yes_bids)?;
    let (no_bid, yes_ask_qty) = best_level(&snapshot.no_bids)?;
    let yes_ask = 100_u8.checked_sub(no_bid)?;
    if yes_ask == 0 || yes_bid_qty == 0 || yes_ask_qty == 0 {
        return None;
    }
    Some(ImplicationTouch {
        yes_bid_cents: yes_bid,
        yes_ask_cents: yes_ask,
        yes_bid_qty,
        yes_ask_qty,
    })
}

fn internal_touch(quote: &QuoteLookup) -> Option<InternalArbLegTouch> {
    let QuoteLookup::Snapshot(snapshot) = quote else {
        return None;
    };
    let (no_bid, yes_ask_qty) = best_level(&snapshot.no_bids)?;
    let yes_ask = 100_u8.checked_sub(no_bid)?;
    if yes_ask == 0 || yes_ask_qty == 0 {
        return None;
    }
    Some(InternalArbLegTouch {
        yes_ask_cents: yes_ask,
        yes_ask_qty,
    })
}

fn best_level(levels: &[(predigy_core::price::Price, u32)]) -> Option<(u8, u32)> {
    levels
        .iter()
        .max_by_key(|(price, _)| price.cents())
        .map(|(price, qty)| (price.cents(), *qty))
}

fn implication_reason(
    parent_quote: &QuoteLookup,
    child_quote: &QuoteLookup,
    diag: Option<&predigy_strategy_implication_arb::ImplicationOpportunity>,
    min_edge_cents: i32,
) -> String {
    if matches!(parent_quote, QuoteLookup::Disabled) {
        return "configured pair observed; quote scan disabled".to_string();
    }
    let mut errors = Vec::new();
    for (label, q) in [("parent", parent_quote), ("child", child_quote)] {
        if let QuoteLookup::Error(e) = q {
            errors.push(format!("{label}={e}"));
        }
    }
    if !errors.is_empty() {
        return format!("quote_error {}", errors.join("; "));
    }
    let Some(opp) = diag else {
        return "missing_two_sided_quote".to_string();
    };
    if opp.max_touch_qty == 0 {
        return "zero_touch_size".to_string();
    }
    if opp.net_edge_cents >= min_edge_cents {
        "edge_clears_current_gate".to_string()
    } else {
        "net_edge_below_min".to_string()
    }
}

fn internal_reason(
    quotes: &[QuoteLookup],
    edge_cents: Option<i32>,
    exhaustive: bool,
    proof_present: bool,
    yes_direction: bool,
    min_edge_cents: i32,
) -> String {
    if quotes.iter().any(|q| matches!(q, QuoteLookup::Disabled)) {
        return if exhaustive && proof_present {
            "configured exhaustive family observed; quote scan disabled".to_string()
        } else {
            "configured family observed but exhaustiveness proof is missing".to_string()
        };
    }
    let errors: Vec<String> = quotes
        .iter()
        .enumerate()
        .filter_map(|(i, q)| match q {
            QuoteLookup::Error(e) => Some(format!("leg_{i}={e}")),
            _ => None,
        })
        .collect();
    if !errors.is_empty() {
        return format!("quote_error {}", errors.join("; "));
    }
    if !yes_direction {
        return "yes_basket_direction_disabled".to_string();
    }
    if !exhaustive || !proof_present {
        return "not_exhaustive_proven".to_string();
    }
    let Some(edge) = edge_cents else {
        return "missing_yes_ask_quote".to_string();
    };
    if edge >= min_edge_cents {
        "edge_clears_current_gate".to_string()
    } else {
        "net_edge_below_min".to_string()
    }
}

async fn maybe_write(database_url: &str, write: bool, observations: &[Observation]) -> Result<()> {
    if !write {
        print_summary("dry-run", observations);
        return Ok(());
    }
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(database_url)
        .await
        .with_context(|| format!("connect postgres {database_url}"))?;
    insert_observations(&pool, observations).await?;
    println!("inserted {} opportunity observations", observations.len());
    Ok(())
}

async fn insert_observations(pool: &sqlx::PgPool, observations: &[Observation]) -> Result<()> {
    for obs in observations {
        sqlx::query(
            r"
            INSERT INTO opportunity_observations (
                strategy, opportunity_key, tickers, kind,
                raw_edge_cents, net_edge_cents, max_size,
                would_fire, reason, payload
            ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
            ",
        )
        .bind(obs.strategy)
        .bind(&obs.opportunity_key)
        .bind(&obs.tickers)
        .bind(obs.kind)
        .bind(obs.raw_edge_cents)
        .bind(obs.net_edge_cents)
        .bind(obs.max_size)
        .bind(obs.would_fire)
        .bind(&obs.reason)
        .bind(&obs.payload)
        .execute(pool)
        .await?;
    }
    Ok(())
}

fn print_summary(prefix: &str, observations: &[Observation]) {
    let would_fire = observations.iter().filter(|o| o.would_fire).count();
    println!(
        "{prefix}: {} opportunity observations (would_fire={would_fire})",
        observations.len()
    );
    for obs in observations.iter().filter(|o| o.would_fire).take(10) {
        println!(
            "  {} {} net_edge={:?} max_size={:?} tickers={}",
            obs.strategy,
            obs.opportunity_key,
            obs.net_edge_cents,
            obs.max_size,
            obs.tickers.join(",")
        );
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn arb_observations_never_claim_would_fire_without_quotes() {
        let dir = tempfile::tempdir().unwrap();
        let implication = dir.path().join("implication.json");
        let internal = dir.path().join("internal.json");
        std::fs::write(
            &implication,
            r#"{"pairs":[{"parent":"KX-P","child":"KX-C"}]}"#,
        )
        .unwrap();
        std::fs::write(
            &internal,
            r#"{"families":[{"family_id":"F","tickers":["KX-A","KX-B"]}]}"#,
        )
        .unwrap();

        let obs = collect_arb_observations(
            Some(implication),
            Some(internal),
            false,
            None,
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(obs.len(), 2);
        assert!(obs.iter().all(|o| !o.would_fire));
        assert!(obs.iter().all(|o| o.payload["non_executing"] == true));
    }
}
