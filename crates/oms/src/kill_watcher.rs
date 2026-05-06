//! Background task that watches a flag file and arms/disarms the
//! kill switch when its content changes.
//!
//! Used by trading daemons to expose a "stop everything" lever
//! that any out-of-process control surface (the dashboard, a
//! kill-switch CLI, a panic script) can flip by writing a single
//! file.
//!
//! ## Flag file shape
//!
//! - Missing or empty file → kill switch should be DISARMED.
//! - File contains the literal token `armed` (whitespace ignored,
//!   case-insensitive) → kill switch should be ARMED.
//! - Any other content is treated as DISARMED (defensive — better
//!   to err on the side of letting the strategy trade than to
//!   silently arm on a typo).
//!
//! The watcher reads the file every `interval` and only sends a
//! command when the desired state differs from the previous tick,
//! so the steady-state cost is one stat + one short read per tick.
//!
//! ## Why poll instead of fs-notify
//!
//! macOS `fs::notify` requires platform-specific bindings; polling
//! a single file every 2s is well within the OMS budget and works
//! on every platform Predigy targets.

use crate::runtime::OmsControl;
use std::path::PathBuf;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// Spawn a watcher task that periodically reads `flag_path` and
/// arms/disarms `control`'s kill switch. Returns the `JoinHandle`
/// in case the caller wants to abort it on shutdown — daemons
/// generally let it run for the lifetime of the process.
pub fn spawn_kill_watcher(
    control: OmsControl,
    flag_path: PathBuf,
    interval: Duration,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut current_armed: Option<bool> = None;
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let desired = read_flag(&flag_path);
            if Some(desired) == current_armed {
                continue;
            }
            let result = if desired {
                control.arm_kill_switch().await
            } else {
                control.disarm_kill_switch().await
            };
            match result {
                Ok(()) => {
                    info!(
                        flag_path = %flag_path.display(),
                        armed = desired,
                        "kill switch state synced from flag file"
                    );
                    current_armed = Some(desired);
                }
                Err(e) => {
                    warn!(error = %e, "kill_watcher: control send failed");
                }
            }
        }
    })
}

/// `armed` iff the flag file exists AND its trimmed lowercase
/// contents start with `armed`.
fn read_flag(path: &std::path::Path) -> bool {
    match std::fs::read_to_string(path) {
        Ok(s) => s.trim().to_ascii_lowercase().starts_with("armed"),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn read_flag_true_on_armed() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("flag");
        std::fs::write(&p, "armed\n").unwrap();
        assert!(read_flag(&p));
    }

    #[test]
    fn read_flag_false_on_missing() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("flag");
        assert!(!read_flag(&p));
    }

    #[test]
    fn read_flag_false_on_other_content() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("flag");
        std::fs::write(&p, "ok").unwrap();
        assert!(!read_flag(&p));
    }

    #[test]
    fn read_flag_case_insensitive() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("flag");
        std::fs::write(&p, "  ARMED  ").unwrap();
        assert!(read_flag(&p));
    }
}
