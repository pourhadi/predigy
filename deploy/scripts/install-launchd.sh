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
[[ "$fail" -eq 0 ]] || { echo ""; echo "preflight failed; fix the FAILs above"; exit 1; }

echo ""
echo "=== installing plists ==="
for name in com.predigy.latency-trader com.predigy.wx-curate; do
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
for name in com.predigy.latency-trader com.predigy.wx-curate; do
    if launchctl print "gui/$(id -u)/${name}" >/dev/null 2>&1; then
        state=$(launchctl print "gui/$(id -u)/${name}" | grep -E "state\s*=" | head -1 | xargs || true)
        echo "  ${name}: ${state:-loaded}"
    else
        echo "  ${name}: NOT LOADED"
    fi
done

echo ""
echo "=== next steps ==="
echo "  Logs:        tail -f $LOG_DIR/latency-trader.stderr.log"
echo "  Force run:   launchctl kickstart -k gui/$(id -u)/com.predigy.wx-curate"
echo "  Stop one:    launchctl bootout gui/$(id -u)/com.predigy.latency-trader"
echo "  Go LIVE:     export PREDIGY_LIVE=1 in ~/.zprofile + restart latency-trader"
echo ""
echo "  Default state: dry-run (PREDIGY_LIVE != 1). Watch logs for"
echo "  'rule fired' entries before flipping the live switch."
