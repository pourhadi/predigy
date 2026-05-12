#!/usr/bin/env bash
# Bootstrap installer for the Linux systemd deployment of predigy.
# Counterpart of deploy/scripts/install-launchd.sh on macOS.
#
# - Confirms the user env file exists and has required vars set.
# - Confirms required release binaries are built.
# - Creates ~/.local/state/predigy/logs/ + ~/.config/predigy/.
# - Installs user systemd units to ~/.config/systemd/user/.
# - Enables linger (so user units survive logout).
# - Reloads + enables/starts services and timers.
#
# Idempotent: re-runs daemon-reload + enable to pick up unit changes.
# Run as the unprivileged user (NOT root); needs sudo only for
# loginctl enable-linger (the rest is per-user systemd).

set -euo pipefail

PREDIGY_HOME="${PREDIGY_HOME:-$HOME/code/predigy}"
UNIT_SRC="${PREDIGY_HOME}/deploy/linux/systemd"
UNIT_DST="${HOME}/.config/systemd/user"
LOG_DIR="${HOME}/.local/state/predigy/logs"
CONFIG_DIR="${HOME}/.config/predigy"
ENV_FILE="${CONFIG_DIR}/env"

LONG_RUNNING=(predigy-engine predigy-dashboard)
TIMERS=(
    predigy-stat-curate
    predigy-cross-arb-curate
    predigy-arb-config-curate
    predigy-calibration
    predigy-paper-trader
    predigy-opportunity-scanner
    predigy-eval-daily
    predigy-db-backup
)

mkdir -p "$UNIT_DST" "$LOG_DIR" "$CONFIG_DIR"

# --- preflight ---
fail=0
ok () { echo "OK:   $1"; }
bad () { echo "FAIL: $1"; fail=1; }

echo "=== preflight ==="

if [[ -f "$ENV_FILE" ]]; then
    ok "$ENV_FILE exists"
    # Source it to check the required vars are populated.
    # shellcheck disable=SC1090
    set -a; source "$ENV_FILE"; set +a
    for var in KALSHI_KEY_ID ANTHROPIC_API_KEY PREDIGY_NWS_USER_AGENT; do
        if [[ -z "${!var:-}" || "${!var}" == REPLACE_ME* ]]; then
            bad "$var unset or still REPLACE_ME in $ENV_FILE"
        else
            ok "$var present"
        fi
    done
else
    bad "$ENV_FILE missing — copy deploy/linux/env.example and edit"
fi

require_file () {
    if [[ -f "$1" ]]; then ok "$1 exists"; else bad "$1 not found — $2"; fi
}

require_file "${CONFIG_DIR}/kalshi.pem" "scp from laptop ~/.config/predigy/kalshi.pem"
require_file "${PREDIGY_HOME}/target/release/predigy-engine" \
    "build: (cd $PREDIGY_HOME && cargo build --release -p predigy-engine)"
require_file "${PREDIGY_HOME}/target/release/predigy-dashboard" \
    "build: (cd $PREDIGY_HOME && cargo build --release -p predigy-dashboard)"
require_file "${PREDIGY_HOME}/target/release/stat-curator" \
    "build: (cd $PREDIGY_HOME && cargo build --release -p stat-curator)"
require_file "${PREDIGY_HOME}/target/release/cross-arb-curator" \
    "build: (cd $PREDIGY_HOME && cargo build --release -p cross-arb-curator)"
require_file "${PREDIGY_HOME}/target/release/arb-config-curator" \
    "build: (cd $PREDIGY_HOME && cargo build --release -p arb-config-curator)"
require_file "${PREDIGY_HOME}/target/release/predigy-calibration" \
    "build: (cd $PREDIGY_HOME && cargo build --release -p predigy-calibration)"
require_file "${PREDIGY_HOME}/target/release/predigy-paper-trader" \
    "build: (cd $PREDIGY_HOME && cargo build --release -p predigy-paper-trader)"
require_file "${PREDIGY_HOME}/target/release/opportunity-scanner" \
    "build: (cd $PREDIGY_HOME && cargo build --release -p opportunity-scanner)"
require_file "${PREDIGY_HOME}/target/release/predigy-eval" \
    "build: (cd $PREDIGY_HOME && cargo build --release -p predigy-eval)"

# Confirm Postgres is reachable on the expected socket.
if command -v psql >/dev/null 2>&1; then
    if psql -d "${PREDIGY_DB_NAME:-predigy}" -c 'SELECT 1' >/dev/null 2>&1; then
        ok "Postgres reachable: ${PREDIGY_DB_NAME:-predigy}"
    else
        bad "psql -d ${PREDIGY_DB_NAME:-predigy} failed — set up the DB first"
    fi
else
    bad "psql not installed — apt install postgresql-client-16"
fi

[[ "$fail" -eq 0 ]] || { echo; echo "preflight failed; fix the FAILs above"; exit 1; }

# --- enable linger so user units survive logout ---
if loginctl show-user "$USER" --property=Linger 2>/dev/null | grep -q 'Linger=yes'; then
    ok "linger already enabled for $USER"
else
    echo "Enabling linger for $USER (needs sudo)..."
    sudo loginctl enable-linger "$USER"
    ok "linger enabled"
fi

# --- install units ---
echo
echo "=== installing units ==="
for unit in "${LONG_RUNNING[@]}"; do
    cp "${UNIT_SRC}/${unit}.service" "${UNIT_DST}/${unit}.service"
    echo "  copied ${unit}.service"
done
for t in "${TIMERS[@]}"; do
    cp "${UNIT_SRC}/${t}.service" "${UNIT_DST}/${t}.service"
    cp "${UNIT_SRC}/${t}.timer"   "${UNIT_DST}/${t}.timer"
    echo "  copied ${t}.service + ${t}.timer"
done

systemctl --user daemon-reload
ok "systemctl --user daemon-reload"

# --- enable + start long-running first, then timers ---
echo
echo "=== enabling long-running services ==="
for unit in "${LONG_RUNNING[@]}"; do
    systemctl --user enable --now "${unit}.service"
    echo "  enabled+started ${unit}.service"
done

echo
echo "=== enabling timers ==="
for t in "${TIMERS[@]}"; do
    systemctl --user enable --now "${t}.timer"
    echo "  enabled+started ${t}.timer"
done

# --- status summary ---
echo
echo "=== status ==="
for unit in "${LONG_RUNNING[@]}"; do
    state=$(systemctl --user is-active "${unit}.service" || true)
    echo "  ${unit}.service: ${state}"
done
for t in "${TIMERS[@]}"; do
    state=$(systemctl --user is-active "${t}.timer" || true)
    next=$(systemctl --user show "${t}.timer" --property=NextElapseUSecRealtime --value 2>/dev/null || true)
    echo "  ${t}.timer: ${state} (next: ${next:-?})"
done

echo
echo "=== next steps ==="
echo "  Logs (journald):    journalctl --user -u predigy-engine -f"
echo "  Logs (file):        tail -f ${LOG_DIR}/engine.stderr.log"
echo "  Force a tick:       systemctl --user start predigy-cross-arb-curate.service"
echo "  Arm kill switch:    echo armed > ${CONFIG_DIR}/kill-switch.flag"
echo "  Disarm kill switch: : > ${CONFIG_DIR}/kill-switch.flag"
echo "  Dashboard:          http://$(hostname -I | awk '{print $1}'):8080"
echo "  Go LIVE:            set PREDIGY_ENGINE_MODE=live in $ENV_FILE,"
echo "                      then: systemctl --user restart predigy-engine"
