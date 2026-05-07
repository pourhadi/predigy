// Vendor / product names appear in doc comments.
#![allow(clippy::doc_markdown)]

//! `predigy-eval` — operator-facing CLI for the strategy
//! evaluation framework. See `docs/EVAL_SPEC.md` for the design.
//!
//! Subcommands:
//!   summary [--since 24h]
//!   ledger <strategy> [--since ...]
//!   diagnose <strategy> [--since ...]
//!   report [--out FILE] [--format md|json] [--since ...]
//!   compare <strategy_a> <strategy_b> [--since ...]
//!   watch [--interval 60s]
//!   optimize <strategy>            (v2 stub)

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use predigy_engine_core::Db;
use predigy_eval_lib::{
    TimeWindow, compute_metrics, diagnose, ledger::load_intent_activity, load_trades,
    render_markdown_report,
};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "predigy-eval", about = "Strategy evaluation framework", long_about = None)]
struct Cli {
    /// Postgres DSN. Defaults to the engine's `postgresql:///predigy`.
    #[arg(long, env = "DATABASE_URL", default_value = "postgresql:///predigy")]
    database_url: String,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// One-line-per-strategy summary table.
    Summary(WindowOpts),
    /// Trade-by-trade ledger for one strategy.
    Ledger {
        strategy: String,
        #[command(flatten)]
        window: WindowOpts,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Diagnostics + recommendations for one strategy.
    Diagnose {
        strategy: String,
        #[command(flatten)]
        window: WindowOpts,
    },
    /// Full markdown report covering every strategy.
    Report {
        #[command(flatten)]
        window: WindowOpts,
        /// Output file. Defaults to stdout.
        #[arg(long)]
        out: Option<PathBuf>,
        /// `md` (default) or `json`.
        #[arg(long, value_enum, default_value_t = ReportFormat::Md)]
        format: ReportFormat,
    },
    /// Side-by-side comparison.
    Compare {
        a: String,
        b: String,
        #[command(flatten)]
        window: WindowOpts,
    },
    /// Live-refreshing summary in a TTY.
    Watch {
        #[command(flatten)]
        window: WindowOpts,
        /// Refresh cadence. e.g. `60s`, `5m`.
        #[arg(long, default_value = "60s")]
        interval: String,
    },
    /// (v2) Backtest-replay parameter optimizer.
    Optimize {
        strategy: String,
    },
}

#[derive(Parser, Debug, Clone)]
struct WindowOpts {
    /// `1h | 24h | 7d | 30d | all` or `RFC3339..RFC3339`.
    #[arg(long, default_value = "24h")]
    since: String,
}

impl WindowOpts {
    fn into_window(self) -> Result<TimeWindow> {
        TimeWindow::parse(&self.since).map_err(|e| anyhow::anyhow!(e))
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum ReportFormat {
    Md,
    Json,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    let db = Db::connect(&cli.database_url)
        .await
        .with_context(|| format!("connect to {}", cli.database_url))?;

    match cli.cmd {
        Cmd::Summary(w) => cmd_summary(&db, w).await,
        Cmd::Ledger { strategy, window, limit } => cmd_ledger(&db, &strategy, window, limit).await,
        Cmd::Diagnose { strategy, window } => cmd_diagnose(&db, &strategy, window).await,
        Cmd::Report { window, out, format } => cmd_report(&db, window, out, format).await,
        Cmd::Compare { a, b, window } => cmd_compare(&db, &a, &b, window).await,
        Cmd::Watch { window, interval } => cmd_watch(&db, window, &interval).await,
        Cmd::Optimize { strategy } => cmd_optimize(&strategy),
    }
}

async fn cmd_summary(db: &Db, w: WindowOpts) -> Result<()> {
    let win = w.into_window()?;
    let trades = load_trades(db, win, None).await?;
    let activity = load_intent_activity(db, win, None).await?;
    let metrics = compute_metrics(&trades, &activity, win.start, win.end);

    println!(
        "Strategy        | Closed | Open | Win%  | Net PnL  | Gross    | Fees   | E[t]    | Sharpe | Activity"
    );
    println!(
        "----------------+--------+------+-------+----------+----------+--------+---------+--------+---------"
    );
    let mut keys: Vec<&String> = metrics.keys().collect();
    keys.sort();
    let mut critical_count = 0usize;
    for k in keys {
        let m = &metrics[k];
        let diags = diagnose(m, &trades);
        let crit = diags
            .iter()
            .filter(|d| d.severity == predigy_eval_lib::Severity::Critical)
            .count();
        critical_count += crit;
        let warn = diags
            .iter()
            .filter(|d| d.severity == predigy_eval_lib::Severity::Warn)
            .count();
        println!(
            "{:<15} | {:>6} | {:>4} | {:>5.1}% | {:>+7}c | {:>+7}c | {:>5}c | {:>+5.1}c | {:>5.2} | {} sub / {} fill / {} rej",
            k,
            m.n_trades_closed,
            m.n_trades_open,
            m.win_rate * 100.0,
            m.net_pnl_cents,
            m.gross_pnl_cents,
            m.fees_paid_cents,
            m.expectancy_cents,
            m.sharpe_ratio,
            m.n_intents_submitted,
            m.n_intents_filled,
            m.n_intents_rejected,
        );
        if crit > 0 || warn > 0 {
            for d in &diags {
                let sev = match d.severity {
                    predigy_eval_lib::Severity::Critical => "CRITICAL",
                    predigy_eval_lib::Severity::Warn => "warn    ",
                    predigy_eval_lib::Severity::Info => "info    ",
                };
                println!("                  {sev} {:?}: {}", d.code, short_message(&d.message));
            }
        }
    }
    if critical_count > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn short_message(msg: &str) -> String {
    let one_line = msg.replace('\n', " ").replace("  ", " ");
    if one_line.len() > 100 {
        format!("{}...", &one_line[..100])
    } else {
        one_line
    }
}

async fn cmd_ledger(db: &Db, strategy: &str, w: WindowOpts, limit: usize) -> Result<()> {
    let win = w.into_window()?;
    let trades = load_trades(db, win, Some(strategy)).await?;
    println!(
        "Ticker                          | Side | Qty | Entry | Exit  | Net PnL | Fees | Hold | Exit Reason"
    );
    println!(
        "--------------------------------+------+-----+-------+-------+---------+------+------+-------------"
    );
    for t in trades.iter().take(limit) {
        let exit_str = t
            .avg_exit_cents
            .map(|c| format!("{c}c"))
            .unwrap_or_else(|| "—".into());
        let net = t
            .net_pnl_cents()
            .map(|n| format!("{n:+}c"))
            .unwrap_or_else(|| "open".into());
        let hold = t
            .hold_seconds
            .map(|s| format!("{s}s"))
            .unwrap_or_else(|| "—".into());
        let reason = t
            .exit_reason
            .map(|r| r.label().to_string())
            .unwrap_or_else(|| "—".into());
        println!(
            "{:<31} | {:<4} | {:>3} | {:>4}c | {:>5} | {:>7} | {:>3}c | {:>4} | {}",
            t.ticker, t.side, t.qty_open, t.avg_entry_cents, exit_str, net, t.fees_paid_cents, hold, reason
        );
    }
    Ok(())
}

async fn cmd_diagnose(db: &Db, strategy: &str, w: WindowOpts) -> Result<()> {
    let win = w.into_window()?;
    let trades = load_trades(db, win, Some(strategy)).await?;
    let activity = load_intent_activity(db, win, Some(strategy)).await?;
    let metrics = compute_metrics(&trades, &activity, win.start, win.end);
    let m = match metrics.get(strategy) {
        Some(m) => m,
        None => {
            println!("strategy `{strategy}` has no trades or intents in the window");
            return Ok(());
        }
    };
    let diags = diagnose(m, &trades);
    if diags.is_empty() {
        println!("`{strategy}`: no diagnoses fired ({} closed trades, net {:+}c)", m.n_trades_closed, m.net_pnl_cents);
        return Ok(());
    }
    let mut critical = 0usize;
    for d in &diags {
        let sev = match d.severity {
            predigy_eval_lib::Severity::Critical => {
                critical += 1;
                "CRITICAL"
            }
            predigy_eval_lib::Severity::Warn => "warn",
            predigy_eval_lib::Severity::Info => "info",
        };
        println!("[{}] {:?}", sev, d.code);
        println!("    {}", d.message);
        for r in &d.recommendations {
            println!("    -> {} ({})", format_action_short(&r.action), r.confidence.label());
            println!("       {}", r.rationale);
        }
        println!();
    }
    if critical > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn format_action_short(a: &predigy_eval_lib::ActionKind) -> String {
    use predigy_eval_lib::ActionKind::*;
    match a {
        RaiseMinEdge { current, proposed } => format!("Raise min_edge_cents {current} -> {proposed}"),
        LowerMinEdge { current, proposed } => format!("Lower min_edge_cents {current} -> {proposed}"),
        TightenStopLoss { current, proposed } => format!("Tighten stop_loss_cents {current} -> {proposed}"),
        WidenStopLoss { current, proposed } => format!("Widen stop_loss_cents {current} -> {proposed}"),
        AddTrailingStop { trigger, distance } => format!("Add trailing stop trigger={trigger} dist={distance}"),
        LowerThreshold { which, current, proposed } => format!("Lower {which} {current:.2} -> {proposed:.2}"),
        RaiseThreshold { which, current, proposed } => format!("Raise {which} {current:.2} -> {proposed:.2}"),
        RaiseRiskCap { which, current, proposed } => format!("Raise {which} {current} -> {proposed}"),
        DisableStrategy { reason } => format!("Disable strategy: {reason}"),
        EnableStrategy { reason } => format!("Enable strategy: {reason}"),
        Investigate { what } => format!("Investigate: {what}"),
    }
}

async fn cmd_report(
    db: &Db,
    w: WindowOpts,
    out: Option<PathBuf>,
    format: ReportFormat,
) -> Result<()> {
    let win = w.into_window()?;
    let trades = load_trades(db, win, None).await?;
    let activity = load_intent_activity(db, win, None).await?;
    let metrics = compute_metrics(&trades, &activity, win.start, win.end);
    let mut diagnoses: HashMap<String, Vec<_>> = HashMap::new();
    let mut critical = 0usize;
    for (s, m) in &metrics {
        let ds = diagnose(m, &trades);
        critical += ds
            .iter()
            .filter(|d| d.severity == predigy_eval_lib::Severity::Critical)
            .count();
        diagnoses.insert(s.clone(), ds);
    }
    let body = match format {
        ReportFormat::Md => render_markdown_report(&metrics, &diagnoses),
        ReportFormat::Json => serde_json::to_string_pretty(&serde_json::json!({
            "metrics": metrics,
            "diagnoses": diagnoses,
        }))?,
    };
    if let Some(path) = out {
        std::fs::write(&path, &body).with_context(|| format!("write {path:?}"))?;
        eprintln!("wrote report to {}", path.display());
    } else {
        print!("{body}");
    }
    if critical > 0 {
        std::process::exit(1);
    }
    Ok(())
}

async fn cmd_compare(db: &Db, a: &str, b: &str, w: WindowOpts) -> Result<()> {
    let win = w.into_window()?;
    let trades = load_trades(db, win, None).await?;
    let activity = load_intent_activity(db, win, None).await?;
    let metrics = compute_metrics(&trades, &activity, win.start, win.end);
    let ma = metrics.get(a);
    let mb = metrics.get(b);
    println!("Metric              | {:<20} | {:<20}", a, b);
    println!("--------------------+----------------------+----------------------");
    let lines: Vec<(&str, String, String)> = vec![
        (
            "Closed trades",
            ma.map(|m| m.n_trades_closed.to_string()).unwrap_or_else(|| "—".into()),
            mb.map(|m| m.n_trades_closed.to_string()).unwrap_or_else(|| "—".into()),
        ),
        (
            "Net PnL (c)",
            ma.map(|m| format!("{:+}", m.net_pnl_cents)).unwrap_or_else(|| "—".into()),
            mb.map(|m| format!("{:+}", m.net_pnl_cents)).unwrap_or_else(|| "—".into()),
        ),
        (
            "Win rate",
            ma.map(|m| format!("{:.1}%", m.win_rate * 100.0)).unwrap_or_else(|| "—".into()),
            mb.map(|m| format!("{:.1}%", m.win_rate * 100.0)).unwrap_or_else(|| "—".into()),
        ),
        (
            "Expectancy (c)",
            ma.map(|m| format!("{:+.1}", m.expectancy_cents)).unwrap_or_else(|| "—".into()),
            mb.map(|m| format!("{:+.1}", m.expectancy_cents)).unwrap_or_else(|| "—".into()),
        ),
        (
            "Sharpe",
            ma.map(|m| format!("{:.2}", m.sharpe_ratio)).unwrap_or_else(|| "—".into()),
            mb.map(|m| format!("{:.2}", m.sharpe_ratio)).unwrap_or_else(|| "—".into()),
        ),
        (
            "Median hold",
            ma.map(|m| format!("{}s", m.median_hold_secs)).unwrap_or_else(|| "—".into()),
            mb.map(|m| format!("{}s", m.median_hold_secs)).unwrap_or_else(|| "—".into()),
        ),
        (
            "Intents submitted",
            ma.map(|m| m.n_intents_submitted.to_string()).unwrap_or_else(|| "—".into()),
            mb.map(|m| m.n_intents_submitted.to_string()).unwrap_or_else(|| "—".into()),
        ),
    ];
    for (k, va, vb) in lines {
        println!("{k:<19} | {va:<20} | {vb:<20}");
    }
    Ok(())
}

async fn cmd_watch(db: &Db, w: WindowOpts, interval: &str) -> Result<()> {
    let dur = parse_interval(interval)?;
    loop {
        // Clear screen.
        print!("\x1B[2J\x1B[1;1H");
        cmd_summary(db, w.clone()).await.ok();
        tokio::time::sleep(dur).await;
    }
}

fn parse_interval(s: &str) -> Result<std::time::Duration> {
    let (digits, unit) = s.split_at(s.len() - 1);
    let n: u64 = digits.parse().with_context(|| format!("parse interval {s}"))?;
    let dur = match unit {
        "s" => std::time::Duration::from_secs(n),
        "m" => std::time::Duration::from_secs(n * 60),
        other => anyhow::bail!("unknown interval unit `{other}`; want s/m"),
    };
    Ok(dur)
}

fn cmd_optimize(strategy: &str) -> Result<()> {
    eprintln!(
        "v2 — backtest-replay parameter optimizer not yet implemented for `{strategy}`. \
         For v1, use `predigy-eval diagnose {strategy}` for rule-based recommendations."
    );
    std::process::exit(2);
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}
