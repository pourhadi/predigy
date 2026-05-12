#!/usr/bin/env bash
# Cutover from the laptop (macOS / launchd) to the Pi (Linux / systemd).
#
# RUN ON THE LAPTOP. Drives the Pi via ssh.
#
# Idempotent in the sense that re-running it after a failure will
# fall through the steps that already completed (kill switch already
# armed, services already stopped, etc.). The DB dump+restore is
# the destructive step — re-running overwrites the Pi DB with the
# laptop's current state, which is fine if the Pi was still in
# shadow mode but DANGEROUS if the Pi was live (it would lose
# fills/intents recorded on the Pi).
#
# Operator: read each phase prompt; the script pauses before any
# step that touches live trading state. Ctrl-C at any prompt is
# safe — earlier steps are non-destructive.

set -euo pipefail

PI_HOST="${PI_HOST:-dan@192.168.1.35}"
PI_PORT="${PI_PORT:-22}"
LAPTOP_CONFIG="${HOME}/.config/predigy"
LAPTOP_LOG="${HOME}/Library/Logs/predigy"
DUMP_PATH="/tmp/predigy-cutover-$(date +%Y%m%d-%H%M%S).dump"

pi () { ssh -p "$PI_PORT" "$PI_HOST" "$@"; }
pause () {
    echo
    read -r -p "[next] $1 [enter to continue, ctrl-c to abort] " _ || exit 130
}

# --- preflight ---
echo "=== preflight ==="
if ! pi 'echo OK' >/dev/null 2>&1; then
    echo "FAIL: ssh $PI_HOST not reachable"; exit 1
fi
echo "  ssh: OK"
if ! pi 'systemctl --user is-active predigy-engine.service' >/dev/null 2>&1; then
    echo "FAIL: predigy-engine.service not active on Pi — install + shadow-run first"; exit 1
fi
echo "  pi engine: active"
pi_mode=$(pi 'grep ^PREDIGY_ENGINE_MODE= /home/dan/.config/predigy/env | cut -d= -f2')
echo "  pi engine mode: $pi_mode (expected: shadow for safe cutover)"
[[ "$pi_mode" == "shadow" ]] || { echo "FAIL: Pi must start in shadow"; exit 1; }

pause "arm laptop kill switch (engine stops submitting new orders)"
echo "armed-for-cutover" > "$LAPTOP_CONFIG/kill-switch.flag"
echo "  laptop kill switch: armed"
sleep 5
open_submitted=$(psql -d predigy -At -c "SELECT COUNT(*) FROM intents WHERE status='submitted'" 2>/dev/null || echo "?")
echo "  laptop submitted intents: $open_submitted (should drop to 0 shortly)"
sleep 10
open_submitted=$(psql -d predigy -At -c "SELECT COUNT(*) FROM intents WHERE status='submitted'" 2>/dev/null || echo "?")
echo "  laptop submitted intents (re-check): $open_submitted"

pause "stop laptop launchd services (engine, dashboard, all curators)"
for plist in "$HOME"/Library/LaunchAgents/com.predigy.*.plist; do
    label=$(basename "$plist" .plist)
    launchctl bootout "gui/$(id -u)/$label" 2>/dev/null && echo "  stopped $label" || echo "  (already stopped: $label)"
done

pause "pg_dump laptop predigy DB to $DUMP_PATH"
pg_dump -Fc predigy > "$DUMP_PATH"
echo "  dump size: $(du -h "$DUMP_PATH" | cut -f1)"

pause "transfer dump to Pi and restore"
scp -P "$PI_PORT" "$DUMP_PATH" "$PI_HOST:/tmp/predigy-cutover.dump"
pi 'pg_restore -d predigy --clean --if-exists /tmp/predigy-cutover.dump 2>&1 | tail -10'
echo "  restore done"
pi 'psql -d predigy -c "SELECT COUNT(*) FROM fills"'

pause "verify Pi can submit (engine still in shadow — this only proves connectivity)"
pi 'systemctl --user restart predigy-engine.service'
sleep 5
pi 'systemctl --user is-active predigy-engine.service'
pi 'tail -20 /home/dan/.local/state/predigy/logs/engine.stderr.log 2>/dev/null | tail -15'

pause "flip Pi engine to LIVE mode and restart"
pi 'sed -i "s/^PREDIGY_ENGINE_MODE=.*/PREDIGY_ENGINE_MODE=live/" /home/dan/.config/predigy/env && grep ^PREDIGY_ENGINE_MODE /home/dan/.config/predigy/env'
pi 'systemctl --user restart predigy-engine.service'
sleep 5
pi 'systemctl --user is-active predigy-engine.service'

pause "disarm Pi kill switch (Pi engine starts submitting)"
pi ': > /home/dan/.config/predigy/kill-switch.flag'
echo "  pi kill switch: disarmed"
sleep 30
pi 'tail -30 /home/dan/.local/state/predigy/logs/engine.stderr.log'

echo
echo "=== cutover complete ==="
echo "  dashboard:    http://nas.local:8080"
echo "  live logs:    ssh $PI_HOST 'journalctl --user -u predigy-engine -f'"
echo "  rollback:     deploy/linux/rollback.sh"
echo
echo "  LEAVE laptop launchd plists installed (bootouted but not disabled)"
echo "  for ~7 days. After 7 days of clean trading on the Pi:"
echo "    launchctl disable gui/\$(id -u)/com.predigy.\\*"
