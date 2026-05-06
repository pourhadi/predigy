#!/bin/zsh
# One-shot installer for the macOS launchd setup.
#
# - Creates ~/Library/Logs/predigy/.
# - Copies plists to ~/Library/LaunchAgents/.
# - Loads them with launchctl.
# - Verifies env, paths, and the binaries are built.
#
# Idempotent: re-runs unload+load to pick up plist changes.

set -euo pipefail

PREDIGY_HOME="${PREDIGY_HOME:-$HOME/code/predigy}"
LAUNCH_AGENTS="${HOME}/Library/LaunchAgents"
PLIST_SRC="${PREDIGY_HOME}/deploy/macos"
LOG_DIR="${HOME}/Library/Logs/predigy"
CONFIG_DIR="${HOME}/.config/predigy"

mkdir -p "$LAUNCH_AGENTS" "$LOG_DIR" "$CONFIG_DIR"

# --- preflight ---
fail=0
require_env () {
    local var="$1"
    if [[ -z "${(P)var:-}" ]]; then
        echo "FAIL: $var must be set in ~/.zprofile (zsh -lc reads it)"
        fail=1
    else
        echo "OK:   $var present"
    fi
}
require_file () {
    if [[ -f "$1" ]]; then
        echo "OK:   $1 exists"
    else
        echo "FAIL: $1 not found — $2"
        fail=1
    fi
}

echo "=== preflight ==="
# Source ~/.zprofile so env vars set there are visible.
source "$HOME/.zprofile" 2>/dev/null || true
require_env KALSHI_KEY_ID
require_env ANTHROPIC_API_KEY
require_env NWS_USER_AGENT
require_file "${CONFIG_DIR}/kalshi.pem" "Kalshi private key (PEM)"
require_file "${PREDIGY_HOME}/target/release/wx-curator" \
    "build with: (cd $PREDIGY_HOME && cargo build --release -p wx-curator)"
require_file "${PREDIGY_HOME}/target/release/latency-trader" \
    "build with: (cd $PREDIGY_HOME && cargo build --release -p latency-trader)"
require_file "${PREDIGY_HOME}/target/release/predigy-dashboard" \
    "build with: (cd $PREDIGY_HOME && cargo build --release -p predigy-dashboard)"
require_file "${PREDIGY_HOME}/target/release/cross-arb-trader" \
    "build with: (cd $PREDIGY_HOME && cargo build --release -p cross-arb-trader)"
require_file "${CONFIG_DIR}/cross-arb-pairs.txt" \
    "seed with: ./target/release/cross-arb-curator --kalshi-key-id \$KALSHI_KEY_ID --kalshi-pem $CONFIG_DIR/kalshi.pem --output $CONFIG_DIR/cross-arb-pairs.txt --write"
require_file "${PREDIGY_HOME}/target/release/cross-arb-curator" \
    "build with: (cd $PREDIGY_HOME && cargo build --release -p cross-arb-curator)"
require_file "${PREDIGY_HOME}/target/release/settlement-trader" \
    "build with: (cd $PREDIGY_HOME && cargo build --release -p settlement-trader)"
require_file "${PREDIGY_HOME}/target/release/wx-stat-curator" \
    "build with: (cd $PREDIGY_HOME && cargo build --release -p wx-stat-curator)"
[[ "$fail" -eq 0 ]] || { echo ""; echo "preflight failed; fix the FAILs above"; exit 1; }

echo ""
echo "=== installing plists ==="
for name in com.predigy.latency-trader com.predigy.wx-curate com.predigy.dashboard com.predigy.cross-arb com.predigy.cross-arb-curate com.predigy.settlement com.predigy.stat-curate com.predigy.stat-trader com.predigy.wx-stat-curate; do
    src="${PLIST_SRC}/${name}.plist"
    dst="${LAUNCH_AGENTS}/${name}.plist"
    cp "$src" "$dst"
    echo "  copied $src → $dst"
    # Try unload first (no-op if not loaded).
    launchctl bootout "gui/$(id -u)/${name}" 2>/dev/null || true
    launchctl bootstrap "gui/$(id -u)" "$dst"
    echo "  loaded ${name}"
done

echo ""
echo "=== status ==="
for name in com.predigy.latency-trader com.predigy.wx-curate com.predigy.dashboard com.predigy.cross-arb com.predigy.cross-arb-curate com.predigy.settlement com.predigy.stat-curate com.predigy.stat-trader com.predigy.wx-stat-curate; do
    if launchctl print "gui/$(id -u)/${name}" >/dev/null 2>&1; then
        state=$(launchctl print "gui/$(id -u)/${name}" | grep -E "state\s*=" | head -1 | xargs || true)
        echo "  ${name}: ${state:-loaded}"
    else
        echo "  ${name}: NOT LOADED"
    fi
done

echo ""
echo "=== next steps ==="
echo "  Trader logs: tail -f $LOG_DIR/latency-trader.stderr.log"
echo "  Force run:   launchctl kickstart -k gui/$(id -u)/com.predigy.wx-curate"
echo "  Stop one:    launchctl bootout gui/$(id -u)/com.predigy.latency-trader"
echo "  Go LIVE:     export PREDIGY_LIVE=1 in ~/.zprofile + restart latency-trader"
echo ""
echo "  Dashboard:   open http://$(ipconfig getifaddr en0 2>/dev/null || echo 127.0.0.1):8080"
echo "               (LAN IP works on your phone if on the same wifi;"
echo "                or use Tailscale to hit it from anywhere)"
echo ""
echo "  Default trader state: dry-run (PREDIGY_LIVE != 1). Watch logs"
echo "  for 'rule fired' entries before flipping the live switch."
