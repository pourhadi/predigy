//! Engine configuration. Loaded once at startup from CLI flags
//! and env vars; passed by reference everywhere afterward.
//!
//! Production defaults are intentionally tight for the $50-cap
//! shake-down phase; raise via env when capital grows.

use crate::oms_db::EngineMode;
use predigy_engine_core::oms::RiskCaps;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Postgres connection string. Defaults to peer-auth UNIX
    /// socket (`postgresql:///predigy`).
    pub database_url: String,

    /// Run pending sqlx migrations on startup. Default true.
    /// Disable only for read-only tools.
    pub auto_migrate: bool,

    /// Engine execution mode. Defaults to Shadow — production
    /// services should explicitly set `PREDIGY_ENGINE_MODE=live`
    /// once parity is verified. Live mode is what activates the
    /// REST submitter; Shadow leaves intents at status='shadow'
    /// where the venue submitter never sees them.
    pub engine_mode: EngineMode,

    /// REST venue-submitter poll interval. The submitter checks
    /// for `submitted` and `cancel_requested` rows on this cadence.
    /// 250ms keeps median submit-to-ack at ~250ms + REST RTT
    /// (~200ms) ≈ ~450ms. Tighten for latency-sensitive lanes.
    pub venue_rest_poll_interval: Duration,

    /// Kalshi credentials. Used by REST + WS + (Phase 4) FIX.
    pub kalshi_key_id: String,
    pub kalshi_pem_path: PathBuf,
    pub kalshi_rest_endpoint: Option<String>,
    pub kalshi_ws_endpoint: Option<url::Url>,

    /// Per-strategy + global risk caps. Applied by the OMS to
    /// every intent. Currently the same RiskCaps is shared by
    /// every strategy; per-strategy overrides land via env in a
    /// future commit.
    pub default_risk_caps: RiskCaps,

    /// Where to put structured logs (stderr) + the file-based
    /// kill-switch fallback flag.
    pub log_dir: PathBuf,
    pub kill_switch_file: PathBuf,

    /// HTTP bind for the metrics + dashboard surface. The legacy
    /// dashboard binary moves under this socket once Phase 2
    /// completes.
    pub http_bind: String,

    /// Reconciliation cadence — diff DB vs venue snapshot.
    pub reconcile_interval: Duration,

    /// Tick interval the supervisor uses when a strategy doesn't
    /// override (`Strategy::tick_interval`).
    pub default_strategy_tick_interval: Duration,

    /// Hard cap on time the engine waits for graceful shutdown
    /// before aborting tasks.
    pub shutdown_grace: Duration,
}

impl EngineConfig {
    /// Build from environment variables with documented fallbacks.
    /// Missing required variables panic (the engine refuses to
    /// start with bad config — that's strictly safer than
    /// guessing).
    pub fn from_env() -> Result<Self, anyhow::Error> {
        use anyhow::Context as _;
        Ok(Self {
            database_url: std::env::var("DATABASE_URL")
                .unwrap_or_else(|_| "postgresql:///predigy".into()),
            auto_migrate: env_bool("PREDIGY_ENGINE_AUTO_MIGRATE", true),
            engine_mode: env_engine_mode("PREDIGY_ENGINE_MODE", EngineMode::Shadow),
            venue_rest_poll_interval: env_duration_ms(
                "PREDIGY_VENUE_REST_POLL_MS",
                Duration::from_millis(250),
            ),
            kalshi_key_id: std::env::var("KALSHI_KEY_ID")
                .context("KALSHI_KEY_ID env var is required")?,
            kalshi_pem_path: std::env::var("KALSHI_PEM")
                .map(PathBuf::from)
                .context("KALSHI_PEM env var is required")?,
            kalshi_rest_endpoint: std::env::var("KALSHI_REST_ENDPOINT").ok(),
            kalshi_ws_endpoint: match std::env::var("KALSHI_WS_ENDPOINT") {
                Ok(s) => Some(url::Url::parse(&s).context("KALSHI_WS_ENDPOINT not a URL")?),
                Err(_) => None,
            },
            default_risk_caps: caps_from_env(),
            log_dir: env_path("PREDIGY_LOG_DIR", dirs_log_default()),
            kill_switch_file: env_path("PREDIGY_KILL_SWITCH_FILE", dirs_kill_switch_default()),
            http_bind: std::env::var("PREDIGY_HTTP_BIND")
                .unwrap_or_else(|_| "127.0.0.1:8080".into()),
            reconcile_interval: env_duration(
                "PREDIGY_RECONCILE_INTERVAL_SECS",
                Duration::from_secs(60),
            ),
            default_strategy_tick_interval: env_duration(
                "PREDIGY_TICK_INTERVAL_SECS",
                Duration::from_secs(15),
            ),
            shutdown_grace: env_duration("PREDIGY_SHUTDOWN_GRACE_SECS", Duration::from_secs(10)),
        })
    }
}

fn env_bool(name: &str, default: bool) -> bool {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<bool>().ok())
        .unwrap_or(default)
}

fn env_path(name: &str, default: PathBuf) -> PathBuf {
    std::env::var(name).map_or(default, PathBuf::from)
}

fn env_duration(name: &str, default: Duration) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(default, Duration::from_secs)
}

fn env_duration_ms(name: &str, default: Duration) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(default, Duration::from_millis)
}

fn env_engine_mode(name: &str, default: EngineMode) -> EngineMode {
    match std::env::var(name).ok().as_deref() {
        Some("live" | "Live" | "LIVE") => EngineMode::Live,
        Some("shadow" | "Shadow" | "SHADOW") => EngineMode::Shadow,
        Some(other) => {
            eprintln!("{name}={other:?} not recognized; using default {default:?}");
            default
        }
        None => default,
    }
}

fn dirs_log_default() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join("Library/Logs/predigy")
    } else {
        PathBuf::from("/tmp/predigy-logs")
    }
}

fn dirs_kill_switch_default() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".config/predigy/kill-switch.flag")
    } else {
        PathBuf::from("/tmp/predigy-kill-switch.flag")
    }
}

fn caps_from_env() -> RiskCaps {
    let mut caps = RiskCaps::shake_down();
    if let Ok(v) = std::env::var("PREDIGY_MAX_NOTIONAL_CENTS")
        .and_then(|s| s.parse::<i64>().map_err(|_| std::env::VarError::NotPresent))
    {
        caps.max_notional_cents = v;
    }
    if let Ok(v) = std::env::var("PREDIGY_MAX_GLOBAL_NOTIONAL_CENTS")
        .and_then(|s| s.parse::<i64>().map_err(|_| std::env::VarError::NotPresent))
    {
        caps.max_global_notional_cents = v;
    }
    if let Ok(v) = std::env::var("PREDIGY_MAX_DAILY_LOSS_CENTS")
        .and_then(|s| s.parse::<i64>().map_err(|_| std::env::VarError::NotPresent))
    {
        caps.max_daily_loss_cents = v;
    }
    if let Ok(v) = std::env::var("PREDIGY_MAX_CONTRACTS_PER_SIDE")
        .and_then(|s| s.parse::<i32>().map_err(|_| std::env::VarError::NotPresent))
    {
        caps.max_contracts_per_side = v;
    }
    if let Ok(v) = std::env::var("PREDIGY_MAX_IN_FLIGHT")
        .and_then(|s| s.parse::<i32>().map_err(|_| std::env::VarError::NotPresent))
    {
        caps.max_in_flight = v;
    }
    if let Ok(v) = std::env::var("PREDIGY_MAX_ORDERS_PER_WINDOW")
        .and_then(|s| s.parse::<u32>().map_err(|_| std::env::VarError::NotPresent))
    {
        caps.max_orders_per_window = v;
    }
    if let Ok(v) = std::env::var("PREDIGY_RATE_WINDOW_MS")
        .and_then(|s| s.parse::<u64>().map_err(|_| std::env::VarError::NotPresent))
    {
        caps.rate_window_ms = v;
    }
    caps
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;

    // Rust 1.94+ marks env::set_var / remove_var unsafe (process-
    // wide mutation; not thread-safe). Tests guard with unsafe
    // blocks; production code never sets env at runtime. The
    // `#[allow(unsafe_code)]` on the test module overrides the
    // workspace-wide `unsafe_code = "forbid"`.
    #[test]
    fn env_bool_parses() {
        unsafe {
            std::env::set_var("PRED_TEST_BOOL_T", "true");
            std::env::set_var("PRED_TEST_BOOL_F", "false");
        }
        assert!(env_bool("PRED_TEST_BOOL_T", false));
        assert!(!env_bool("PRED_TEST_BOOL_F", true));
        assert!(env_bool("PRED_TEST_BOOL_MISSING", true));
        unsafe {
            std::env::remove_var("PRED_TEST_BOOL_T");
            std::env::remove_var("PRED_TEST_BOOL_F");
        }
    }

    #[test]
    fn env_duration_parses_seconds() {
        unsafe {
            std::env::set_var("PRED_TEST_DUR", "30");
        }
        assert_eq!(
            env_duration("PRED_TEST_DUR", Duration::from_secs(0)),
            Duration::from_secs(30)
        );
        assert_eq!(
            env_duration("PRED_TEST_DUR_MISSING", Duration::from_secs(7)),
            Duration::from_secs(7)
        );
        unsafe {
            std::env::remove_var("PRED_TEST_DUR");
        }
    }

    #[test]
    fn caps_from_env_overrides_individual_fields() {
        let old_contract_cap = std::env::var("PREDIGY_MAX_CONTRACTS_PER_SIDE").ok();
        unsafe {
            std::env::set_var("PREDIGY_MAX_NOTIONAL_CENTS", "10000");
            std::env::set_var("PREDIGY_MAX_DAILY_LOSS_CENTS", "5000");
            std::env::remove_var("PREDIGY_MAX_CONTRACTS_PER_SIDE");
        }
        let caps = caps_from_env();
        assert_eq!(caps.max_notional_cents, 10_000);
        assert_eq!(caps.max_daily_loss_cents, 5_000);
        // Untouched fields retain shake-down defaults.
        assert_eq!(caps.max_contracts_per_side, 3);
        unsafe {
            std::env::remove_var("PREDIGY_MAX_NOTIONAL_CENTS");
            std::env::remove_var("PREDIGY_MAX_DAILY_LOSS_CENTS");
            if let Some(v) = old_contract_cap {
                std::env::set_var("PREDIGY_MAX_CONTRACTS_PER_SIDE", v);
            }
        }
    }
}
