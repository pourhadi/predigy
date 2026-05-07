#!/bin/zsh
# predigy-engine launcher (shadow mode by default).
#
# The consolidated trading engine. Replaces the per-strategy
# binaries (stat-trader, settlement-trader, latency-trader,
# cross-arb-trader) once dual-write parity is verified — see
# docs/CUTOVER.md.
#
# By default this runs in **Shadow mode**: the engine writes
# intents to Postgres at status='shadow' but does NOT submit to
# Kalshi. The legacy daemons keep trading as the sole live
# trader. Once parity-diff shows engine and legacy fire the same
# intents, flip via PREDIGY_ENGINE_MODE=live and disable the
# corresponding legacy launchd job.
#
# Required env from ~/.zprofile:
#   KALSHI_KEY_ID
#
# Optional env (gates which strategies the engine spawns):
#   PREDIGY_NWS_USER_AGENT  — required for latency strategy
#   PREDIGY_NWS_STATES      — comma-separated state codes
#   PREDIGY_LATENCY_RULE_FILE — JSON file (latency strategy only)
#   PREDIGY_CROSS_ARB_PAIR_FILE — pair file (cross-arb only)
#
# Risk caps default to the same shake-down envelope as the legacy
# daemons; override per-strategy via the env vars in EngineConfig.

set -euo pipefail

PREDIGY_HOME="${PREDIGY_HOME:-$HOME/code/predigy}"
CONFIG_DIR="${HOME}/.config/predigy"
LOG_DIR="${HOME}/Library/Logs/predigy"
mkdir -p "$CONFIG_DIR" "$LOG_DIR"

KALSHI_KEY_ID="${KALSHI_KEY_ID:?KALSHI_KEY_ID env var required}"
KALSHI_PEM="${KALSHI_PEM:-${CONFIG_DIR}/kalshi.pem}"
export KALSHI_KEY_ID KALSHI_PEM

# Default to Shadow. Operator flips to "live" via env override
# after parity-diff has shown ledger agreement with the legacy
# daemon for at least one full trading day.
export PREDIGY_ENGINE_MODE="${PREDIGY_ENGINE_MODE:-shadow}"

# Postgres connection string. Local dev uses peer-auth on the
# UNIX socket; CI / remote will need a TCP DSN.
export DATABASE_URL="${DATABASE_URL:-postgresql:///predigy}"

# Run pending migrations on startup (idempotent).
export PREDIGY_ENGINE_AUTO_MIGRATE="${PREDIGY_ENGINE_AUTO_MIGRATE:-true}"

# Kill-switch flag file — same convention as the legacy daemons
# so the dashboard's emergency-stop button arms ALL traders +
# the engine in one click.
export PREDIGY_KILL_SWITCH_FILE="${PREDIGY_KILL_SWITCH_FILE:-${CONFIG_DIR}/kill-switch.flag}"

# Plumb optional strategy settings if set. The engine binary
# uses these to gate strategy registration; missing env =
# strategy not registered.
[ -n "${PREDIGY_LATENCY_RULE_FILE:-}" ] && export PREDIGY_LATENCY_RULE_FILE
[ -n "${PREDIGY_CROSS_ARB_PAIR_FILE:-}" ] && export PREDIGY_CROSS_ARB_PAIR_FILE
[ -n "${PREDIGY_NWS_USER_AGENT:-}" ] && export PREDIGY_NWS_USER_AGENT
[ -n "${PREDIGY_NWS_STATES:-}" ] && export PREDIGY_NWS_STATES
# Audit S2 / S3 / S4 / S5 / S8 / S9 — six new strategy config
# vars (each gates its own strategy registration).
[ -n "${PREDIGY_WX_STAT_RULE_FILE:-}" ] && export PREDIGY_WX_STAT_RULE_FILE
[ -n "${PREDIGY_INTERNAL_ARB_CONFIG:-}" ] && export PREDIGY_INTERNAL_ARB_CONFIG
[ -n "${PREDIGY_IMPLICATION_ARB_CONFIG:-}" ] && export PREDIGY_IMPLICATION_ARB_CONFIG
[ -n "${PREDIGY_BOOK_IMBALANCE_CONFIG:-}" ] && export PREDIGY_BOOK_IMBALANCE_CONFIG
[ -n "${PREDIGY_VARIANCE_FADE_CONFIG:-}" ] && export PREDIGY_VARIANCE_FADE_CONFIG
[ -n "${PREDIGY_NEWS_TRADER_ITEMS_FILE:-}" ] && export PREDIGY_NEWS_TRADER_ITEMS_FILE

cd "$PREDIGY_HOME"

echo "[$(date -Iseconds)] predigy-engine: starting in mode=${PREDIGY_ENGINE_MODE}"

exec "./target/release/predigy-engine"
