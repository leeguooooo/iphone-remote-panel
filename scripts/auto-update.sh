#!/bin/bash
# auto-update.sh — unattended upgrades for the iphone-use daemon, gated on idleness.
#
#   auto-update.sh enable        install this script to ~/.iphone-use and register a
#                                daily LaunchAgent (04:30) that runs `auto-update.sh run`
#   auto-update.sh disable       unload and remove that LaunchAgent
#   auto-update.sh status        show whether the job is registered and the last log lines
#   auto-update.sh run           upgrade now IF a newer release exists and the phone is idle
#   auto-update.sh run --force   upgrade now regardless of idleness (still needs a newer release
#                                unless --reinstall)
#   auto-update.sh run --dry-run decide and report, change nothing
#
# "Idle" means: nobody owns the phone (X-Phone-Owner lease empty), no hold lease,
# the daemon is not releasing/reconnecting, and no WDA session is up
# (device_state released / offline / blocked). Upgrading restarts the daemon, and
# a daemon restart in the middle of someone's phone task is exactly the kind of
# interruption #72 was filed about — so the gate is strict and silent: when the
# phone is busy we log one line and try again tomorrow.
#
# The upgrade itself is the documented one-liner (`install.sh` from the release's
# pinned commit): SHA-256-checked assets, skill first, daemon second, rollback on
# failure. This script adds only the decision and the schedule.
set -euo pipefail

REPO="${IPHONE_USE_REPO:-leeguooooo/iphone-use}"
STATE_DIR="${PHONE_REMOTE_STATE_DIR:-$HOME/.iphone-use}"
LABEL="com.leeguoo.iphone-use.autoupdate"
DAEMON_PLIST="$HOME/Library/LaunchAgents/com.leeguoo.iphone-use.plist"
JOB_PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
LOG_DIR="$HOME/Library/Logs/iPhoneUse"
LOG="$LOG_DIR/auto-update.log"
INSTALLED_SELF="$STATE_DIR/auto-update.sh"
INSTALLER_URL="${AUTO_UPDATE_INSTALLER_URL:-https://raw.githubusercontent.com/$REPO/main/install.sh}"
HOUR="${AUTO_UPDATE_HOUR:-4}"
MINUTE="${AUTO_UPDATE_MINUTE:-30}"

log() { mkdir -p "$LOG_DIR"; printf '%s %s\n' "$(date '+%Y-%m-%dT%H:%M:%S%z')" "$*" | tee -a "$LOG" >&2; }
die() { log "ERROR: $*"; exit 1; }

# --- daemon access ----------------------------------------------------------
daemon_env() {  # print "port token host" from the daemon LaunchAgent
    [ -f "$DAEMON_PLIST" ] || return 1
    python3 - "$DAEMON_PLIST" <<'PY'
import plistlib, sys
env = plistlib.load(open(sys.argv[1], 'rb')).get('EnvironmentVariables', {})
print(env.get('PHONE_REMOTE_PORT', '44321'), env.get('PHONE_REMOTE_AGENT_TOKEN', ''), env.get('PHONE_REMOTE_HOST', '127.0.0.1'))
PY
}

status_json() {
    if [ -n "${AUTO_UPDATE_STATUS_URL:-}" ]; then
        curl -fsS -m 10 -H "Authorization: Bearer ${AUTO_UPDATE_TOKEN:-}" "$AUTO_UPDATE_STATUS_URL"
        return
    fi
    local port token host
    read -r port token host < <(daemon_env) || return 1
    [ "$host" = "0.0.0.0" ] && host="127.0.0.1"
    curl -fsS -m 10 -H "Authorization: Bearer $token" "http://$host:$port/agent/status"
}

latest_tag() {
    if [ -n "${AUTO_UPDATE_LATEST_TAG:-}" ]; then printf '%s\n' "$AUTO_UPDATE_LATEST_TAG"; return; fi
    # The releases/latest redirect needs no API token and is not rate-limited
    # like api.github.com; the tag is the last path segment of the target URL.
    local url
    url="$(curl -fsSIL -m 15 -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest")" || return 1
    case "$url" in
        */releases/tag/v[0-9]*) printf '%s\n' "${url##*/}" ;;
        *) return 1 ;;
    esac
}

# --- the decision -----------------------------------------------------------
# Prints one of: upgrade | skip:<reason> ; exit 0 either way, 2 on a probe failure.
decide() {
    local force="$1" reinstall="$2" status latest
    status="$(status_json)" || { printf 'skip:daemon_unreachable\n'; return 2; }
    latest="$(latest_tag)" || { printf 'skip:latest_unresolved\n'; return 2; }
    STATUS_JSON="$status" LATEST="$latest" FORCE="$force" REINSTALL="$reinstall" python3 - <<'PY'
import json, os, sys
s = json.loads(os.environ['STATUS_JSON'])
latest = os.environ['LATEST'].lstrip('v')
current = str(s.get('version') or '').lstrip('v')
force = os.environ['FORCE'] == '1'
reinstall = os.environ['REINSTALL'] == '1'
def ver(v):
    try: return tuple(int(x) for x in v.split('-')[0].split('.'))
    except ValueError: return ()
if not s.get('ok'):
    print('skip:daemon_not_ok'); sys.exit(0)
if not reinstall and (not current or ver(latest) <= ver(current)):
    print(f'skip:up_to_date current={current} latest={latest}'); sys.exit(0)
if not force:
    if s.get('owner'):
        print(f"skip:phone_owned owner={s['owner']} lease={s.get('owner_lease_remaining_secs')}s"); sys.exit(0)
    if (s.get('hold_remaining_secs') or 0) > 0:
        print(f"skip:held {s['hold_remaining_secs']}s"); sys.exit(0)
    if s.get('releasing') or s.get('reconnecting'):
        print('skip:transitioning'); sys.exit(0)
    if s.get('device_state') not in ('released', 'offline', 'blocked'):
        print(f"skip:in_use device_state={s.get('device_state')}"); sys.exit(0)
print(f'upgrade current={current} latest={latest}')
PY
}

run_installer() {
    if [ -n "${AUTO_UPDATE_INSTALLER_CMD:-}" ]; then
        IPHONE_USE_AUTO_UPDATE=1 bash -c "$AUTO_UPDATE_INSTALLER_CMD"
    else
        curl -fsSL -m 120 "$INSTALLER_URL" | IPHONE_USE_AUTO_UPDATE=1 sh
    fi
}

cmd_run() {
    local force=0 reinstall=0 dry=0 arg
    for arg in "$@"; do
        case "$arg" in
            --force) force=1 ;;
            --reinstall) reinstall=1; force=1 ;;
            --dry-run) dry=1 ;;
            *) die "unknown option for run: $arg" ;;
        esac
    done
    mkdir -p "$STATE_DIR"
    local lock="$STATE_DIR/auto-update.lock"
    if ! mkdir "$lock" 2>/dev/null; then
        log "skip: another auto-update is running (lock $lock)"; return 0
    fi
    # shellcheck disable=SC2064  # expand now: the local is gone by EXIT time
    trap "rmdir '$lock' 2>/dev/null" EXIT
    local decision rc=0
    decision="$(decide "$force" "$reinstall")" || rc=$?
    log "decision: $decision"
    case "$decision" in
        upgrade*) ;;
        *) return "$rc" ;;
    esac
    if [ "$dry" = 1 ]; then log "dry-run: would run the installer now"; return 0; fi
    local before after
    before="$(status_json 2>/dev/null | python3 -c 'import sys,json; print(json.load(sys.stdin).get("version",""))' 2>/dev/null || true)"
    log "upgrading from ${before:-?} via $INSTALLER_URL"
    if run_installer >>"$LOG" 2>&1; then
        sleep 3
        after="$(status_json 2>/dev/null | python3 -c 'import sys,json; print(json.load(sys.stdin).get("version",""))' 2>/dev/null || true)"
        log "upgrade finished: ${before:-?} -> ${after:-?}"
    else
        die "installer failed (see $LOG); install.sh rolls back on its own"
    fi
}

# --- the schedule -----------------------------------------------------------
write_job_plist() {
    mkdir -p "$(dirname "$JOB_PLIST")" "$LOG_DIR"
    cat > "$JOB_PLIST" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
    <key>Label</key><string>$LABEL</string>
    <key>ProgramArguments</key>
    <array><string>/bin/bash</string><string>$INSTALLED_SELF</string><string>run</string></array>
    <key>EnvironmentVariables</key>
    <dict><key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string></dict>
    <key>StartCalendarInterval</key>
    <dict><key>Hour</key><integer>$HOUR</integer><key>Minute</key><integer>$MINUTE</integer></dict>
    <key>RunAtLoad</key><false/>
    <key>StandardOutPath</key><string>$LOG</string>
    <key>StandardErrorPath</key><string>$LOG</string>
</dict></plist>
PLIST
    plutil -lint "$JOB_PLIST" >/dev/null
}

cmd_enable() {
    mkdir -p "$STATE_DIR"
    local self; self="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
    if [ "$self" != "$INSTALLED_SELF" ]; then cp "$self" "$INSTALLED_SELF"; fi
    chmod 0755 "$INSTALLED_SELF"
    write_job_plist
    launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
    launchctl bootstrap "gui/$(id -u)" "$JOB_PLIST"
    log "enabled: $LABEL runs '$INSTALLED_SELF run' daily at $(printf '%02d:%02d' "$HOUR" "$MINUTE")"
}

cmd_disable() {
    launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
    rm -f "$JOB_PLIST"
    log "disabled: $LABEL removed"
}

cmd_status() {
    if [ -f "$JOB_PLIST" ] && launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1; then
        echo "auto-update: enabled ($JOB_PLIST, daily $(printf '%02d:%02d' "$HOUR" "$MINUTE"))"
    else
        echo "auto-update: disabled"
    fi
    echo "decision now: $(decide 0 0 2>/dev/null || true)"
    [ -f "$LOG" ] && { echo "last log lines:"; tail -n 5 "$LOG"; }
}

case "${1:-}" in
    enable) cmd_enable ;;
    disable) cmd_disable ;;
    status) cmd_status ;;
    run) shift; cmd_run "$@" ;;
    *) sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
