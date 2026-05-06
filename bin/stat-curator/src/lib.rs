//! Library half of the `stat-curator` binary so the agent + scan
//! halves can be unit-tested without the CLI.

pub mod agent;
pub mod kalshi_scan;
pub mod prompt;

pub use agent::{CuratedStatRule, CuratorError, propose_rules};
pub use kalshi_scan::{DEFAULT_CATEGORIES, ScanError, StatMarket, scan_stat_markets};
pub use prompt::{SYSTEM_PROMPT, user_message};
