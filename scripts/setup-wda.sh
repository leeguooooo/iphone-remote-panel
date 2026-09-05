#!/usr/bin/env bash
# scripts/setup-wda.sh — one-command setup for the L2 element-tree layer (WDA).
#
# Builds Appium's WebDriverAgent, installs it on your iPhone, keeps it running,
# starts a localhost relay, and points the iphone-use daemon at it. Encodes
# every pitfall we hit validating this on hardware (see docs/wda-setup.html).
#
# Usage:
#   ./scripts/setup-wda.sh            # full setup (interactive prompts as needed)
#   ./scripts/setup-wda.sh status     # is WDA + relay up?
#   ./scripts/setup-wda.sh stop       # stop WDA runner + relay
#   ./scripts/setup-wda.sh pause      # give the phone back; disable auto-restart
#   ./scripts/setup-wda.sh resume     # re-enable the managed WDA supervisor
#
# Env overrides:
#   WDA_UDID=...        target device UDID (default: first xcodebuild iOS device)
#   WDA_TEAM_ID=...     Apple dev team (default: Xcode's last-selected team)
#   WDA_ASC_KEY_PATH=... absolute .p8 path; with both IDs, use ASC API key signing
#   WDA_ASC_KEY_ID=...  App Store Connect key ID (all three WDA_ASC_* required)
#   WDA_ASC_ISSUER_ID=... App Store Connect issuer ID
#   WDA_BUNDLE_ID=...   runner bundle id (default: derived from validated Team ID)
#   WDA_DIR=...         WDA checkout    (default: ~/.iphone-use/WebDriverAgent)
#   WDA_REF=...         exact upstream commit (default: pinned v9.15.3 commit)
#   WDA_RUNNER_ICON=... runner icon: auto, none, or a local .png/.icns (default: auto)
#   WDA_PORT=...        relay port      (default: 8100)
#   MJPEG_PORT=...      video relay port (default: 9100)
#   WDA_ALLOW_LAN=1     permit unauthenticated WDA over LAN (unsafe; default off)
#
# Requirements: Xcode (an Apple ID in Settings → Accounts, or WDA_ASC_* signing),
# the iPhone paired + Developer Mode on, and `iproxy` for the default USB relay.
# `socat` is accepted only with the explicit WDA_ALLOW_LAN=1 escape hatch.
set -eu
umask 077

# When spawned by the daemon (POST /agent/mode) the environment is a bare
# LaunchAgent PATH — Homebrew tools (socat, iproxy) and even xcrun helpers
# live outside it. Extend deterministically rather than relying on the shell.
export PATH="/opt/homebrew/bin:/usr/local/bin:/usr/sbin:/sbin:$PATH"

STATE_DIR="$HOME/.iphone-use"
COMMAND="${1:-setup}"
WDA_CHECKOUT_MARKER="$STATE_DIR/wda-checkout-owner.v1"
RUN_LOG="$STATE_DIR/wda-runner.log"
RUNNER_PID_FILE="$STATE_DIR/wda-runner.pid"
RELAY_PID_FILE="$STATE_DIR/wda-relay.pid"
WDA_REPO="https://github.com/appium/WebDriverAgent.git"
DEFAULT_WDA_REF="54f9fc702b5ba40249017a4b9bf48c69b757389b"
DEFAULT_WDA_REF_TAG="v9.15.3"
WDA_AGENT_LABEL="com.leeguoo.iphone-use.wda"
WDA_AGENT_PLIST="$HOME/Library/LaunchAgents/$WDA_AGENT_LABEL.plist"
WDA_AGENT_LOG="$STATE_DIR/wda-agent.log"
WDA_RETRY_STATE="$STATE_DIR/wda-retry-state.v1"
WDA_AGENT_ROLLBACK_PLIST="$STATE_DIR/wda-supervisor.rollback.$$.plist"
DAEMON_LABEL="com.leeguoo.iphone-use"
DAEMON_PLIST="$HOME/Library/LaunchAgents/$DAEMON_LABEL.plist"
DAEMON_ROLLBACK_PLIST="$STATE_DIR/daemon.rollback.$$.plist"
UID_NUM="$(id -u)"
GUI_DOMAIN="gui/$UID_NUM"
WDA_CHECKOUT_CREATED_THIS_RUN=0
WDA_MARKER_REFRESH_ALLOWED=0
WDA_CANONICAL_DIR=""
SHASUM_BIN="$(command -v shasum 2>/dev/null || true)"
WDA_ICON_WORK_DIR=""
WDA_ICON_PRODUCTS_DIR=""
WDA_ICON_APP_PATH=""
WDA_ICON_BACKUP_PATH=""
WDA_ICON_MUTATION_ACTIVE=0
WDA_RUNNER_ICON_INJECTED=0
WDA_ICON_BUILD_LOCKED=0
WDA_RUNNER_REPAIR_ATTEMPTED=0
WDA_RUNNER_VALIDATION_ERROR=""
KEEPALIVE_ATTEMPT_ACTIVE=0
KEEPALIVE_FAILURE_KIND="generic"
KEEPALIVE_LOCK_RETRY=0
INTERACTIVE_LOCK_STARTED_AT=0
INTERACTIVE_LOCK_NOTICE_AT=0
INTERACTIVE_LOCK_NOTICE_ATTEMPT=0
STATUS_RUN_ID=""
STATUS_OWNER_PID=""
STATUS_OWNER_START=""
STATUS_HEARTBEAT_PID=""

_existing_wda_env() {
    [ -f "$WDA_AGENT_PLIST" ] || { printf ''; return; }
    /usr/libexec/PlistBuddy -c "Print :EnvironmentVariables:$1" \
        "$WDA_AGENT_PLIST" 2>/dev/null || printf ''
}
_existing_daemon_env() {
    [ -f "$DAEMON_PLIST" ] || { printf ''; return; }
    /usr/libexec/PlistBuddy -c "Print :EnvironmentVariables:$1" \
        "$DAEMON_PLIST" 2>/dev/null || printf ''
}
_port_from_daemon_url() {
    local value
    value="$(_existing_daemon_env "$1")"
    printf '%s' "$value" | sed -n 's#^http://127\.0\.0\.1:\([0-9][0-9]*\).*$#\1#p'
}

# Preserve setup-owned supervisor policy on a plain rerun. Precedence is:
# explicit environment > existing WDA supervisor > daemon endpoint > default.
WDA_DIR="${WDA_DIR:-$(_existing_wda_env WDA_DIR)}"
WDA_DIR="${WDA_DIR:-$HOME/.iphone-use/WebDriverAgent}"
WDA_PORT="${WDA_PORT:-$(_existing_wda_env WDA_PORT)}"
WDA_PORT="${WDA_PORT:-$(_port_from_daemon_url PHONE_REMOTE_WDA_URL)}"
WDA_PORT="${WDA_PORT:-8100}"
MJPEG_PORT="${MJPEG_PORT:-$(_existing_wda_env MJPEG_PORT)}"
MJPEG_PORT="${MJPEG_PORT:-$(_port_from_daemon_url PHONE_REMOTE_WDA_MJPEG_URL)}"
MJPEG_PORT="${MJPEG_PORT:-9100}"
WDA_BUNDLE_ID="${WDA_BUNDLE_ID:-$(_existing_wda_env WDA_BUNDLE_ID)}"
WDA_TEAM_ID="${WDA_TEAM_ID:-$(_existing_wda_env WDA_TEAM_ID)}"
# Restore the saved trio only when no ASC override was supplied. A partial
# explicit override must not silently combine with a different saved key.
if [ "${WDA_ASC_KEY_PATH+x}${WDA_ASC_KEY_ID+x}${WDA_ASC_ISSUER_ID+x}" = "" ]; then
    WDA_ASC_KEY_PATH="$(_existing_wda_env WDA_ASC_KEY_PATH)"
    WDA_ASC_KEY_ID="$(_existing_wda_env WDA_ASC_KEY_ID)"
    WDA_ASC_ISSUER_ID="$(_existing_wda_env WDA_ASC_ISSUER_ID)"
fi
WDA_REF="${WDA_REF:-$(_existing_wda_env WDA_REF)}"
WDA_REF="${WDA_REF:-$DEFAULT_WDA_REF}"
if [ "$WDA_REF" = "$DEFAULT_WDA_REF" ]; then
    WDA_REF_LABEL="$DEFAULT_WDA_REF_TAG"
else
    WDA_REF_LABEL="custom pin"
fi
# Home-screen name of the XCUITest runner app that gets installed on the phone.
# Upstream leaves it as "WebDriverAgentRunner-Runner", which shows up on the
# user's device as an unexplained blank-icon app (issue #64). Xcode derives the
# runner's CFBundleName from the test target's PRODUCT_NAME, so renaming that
# target — and ONLY that target, in the checked-out project — relabels it.
# Set to an empty string to keep upstream's name.
WDA_RUNNER_NAME="${WDA_RUNNER_NAME:-iPhoneUse}"
case "$WDA_RUNNER_NAME" in
    ''|*[!A-Za-z0-9_-]*)
        [ -z "$WDA_RUNNER_NAME" ] || die "WDA_RUNNER_NAME must be ASCII letters, digits, '_' or '-' (got '$WDA_RUNNER_NAME')" ;;
esac
# The XCUITest runner is synthesised by Xcode, so a normal target asset catalog
# lands inside the nested .xctest instead of the home-screen .xctrunner app.
# `auto` injects the already-installed iPhoneUse app icon after Xcode has built
# that outer app; `none` preserves upstream's blank placeholder. A local PNG or
# ICNS path allows callers to supply a different icon without editing WDA.
WDA_RUNNER_ICON="${WDA_RUNNER_ICON:-auto}"
WDA_ALLOW_LAN="${WDA_ALLOW_LAN:-$(_existing_wda_env WDA_ALLOW_LAN)}"
WDA_ALLOW_LAN="${WDA_ALLOW_LAN:-0}"
WDA_UDID="${WDA_UDID:-${PHONE_REMOTE_UDID:-}}"
WDA_UDID="${WDA_UDID:-$(_existing_daemon_env PHONE_REMOTE_UDID)}"
WDA_UDID="${WDA_UDID:-$(_existing_wda_env WDA_UDID)}"
case "$WDA_ALLOW_LAN" in
    0|1) ;;
    *) printf 'WDA_ALLOW_LAN must be 0 or 1\n' >&2; exit 1 ;;
esac

BOLD=$'\033[1m'; RED=$'\033[0;31m'; GRN=$'\033[0;32m'; YLW=$'\033[1;33m'; RST=$'\033[0m'
info() { printf '%s\n' "${BOLD}== $*${RST}"; }
ok()   { printf '%s\n' "${GRN}✓${RST} $*"; }
warn() { printf '%s\n' "${YLW}⚠${RST}  $*"; }
die()  { printf '%s\n' "${RED}✗ $*${RST}" >&2; exit 1; }

# BEGIN ASC signing helpers.
_asc_signing_enabled() {
    [ -n "${WDA_ASC_KEY_PATH:-}" ] && [ -n "${WDA_ASC_KEY_ID:-}" ] \
        && [ -n "${WDA_ASC_ISSUER_ID:-}" ]
}

_prepare_xcodebuild_args() {
    XCODEBUILD_ARGS=("$@")
    _asc_signing_enabled || return 0
    # Validate strings only. Never read/copy the private key or echo its values.
    # Spaces in an absolute key path remain within one argv element.
    if ! printf '%s\n' "$WDA_ASC_KEY_PATH" | LC_ALL=C grep -Eq '^/[^|[:cntrl:]]+\.p8$' \
        || ! printf '%s\n' "$WDA_ASC_KEY_ID" | LC_ALL=C grep -Eq '^[A-Za-z0-9]+$' \
        || ! printf '%s\n' "$WDA_ASC_ISSUER_ID" | LC_ALL=C grep -Eq '^[A-Za-z0-9-]+$' \
        || ! _safe_expected "$WDA_ASC_KEY_PATH$WDA_ASC_KEY_ID$WDA_ASC_ISSUER_ID"; then
        printf '%s\n' 'Invalid WDA_ASC_* configuration: use an absolute .p8 path and valid key/issuer IDs.' >&2
        return 1
    fi
    local argument has_updates=0
    for argument in "$@"; do
        [ "$argument" != "-allowProvisioningUpdates" ] || has_updates=1
    done
    if [ "$has_updates" = "0" ]; then
        XCODEBUILD_ARGS+=(-allowProvisioningUpdates)
    fi
    XCODEBUILD_ARGS+=(-authenticationKeyPath "$WDA_ASC_KEY_PATH"
        -authenticationKeyID "$WDA_ASC_KEY_ID"
        -authenticationKeyIssuerID "$WDA_ASC_ISSUER_ID"
        -allowProvisioningDeviceRegistration)
}

_wda_xcodebuild() {
    _prepare_xcodebuild_args "$@" || return 1
    "$XCODEBUILD_BIN" "${XCODEBUILD_ARGS[@]}"
}

_prepare_runner_args() {
    if [ -n "${WDA_XCTESTRUN:-}" ]; then
        _prepare_xcodebuild_args -destination "platform=iOS,id=$WDA_UDID" \
            test-without-building -xctestrun "$WDA_XCTESTRUN" || return 1
    else
        _prepare_xcodebuild_args -project WebDriverAgent.xcodeproj \
            -scheme WebDriverAgentRunner -destination "platform=iOS,id=$WDA_UDID" \
            -allowProvisioningUpdates DEVELOPMENT_TEAM="$TEAM_ID" \
            PRODUCT_BUNDLE_IDENTIFIER="$WDA_BUNDLE_ID" test || return 1
    fi
    RUNNER_ARGV=("${XCODEBUILD_ARGS[@]}")
    RUNNER_ARGS="${RUNNER_ARGV[*]}"
}

_report_missing_xcode_account() {
    _setstatus signing-fail account "sign in to an Apple account in Xcode, or configure WDA_ASC_KEY_PATH / WDA_ASC_KEY_ID / WDA_ASC_ISSUER_ID for API key signing"
    die "Xcode has no signed-in Apple account. Open Xcode → Settings → Accounts,
   sign in and select the development team, or configure WDA_ASC_KEY_PATH,
   WDA_ASC_KEY_ID and WDA_ASC_ISSUER_ID for App Store Connect API key signing,
   then rerun."
}
# END ASC signing helpers.

_cleanup_wda_icon_work_dir() {
    [ -n "${WDA_ICON_WORK_DIR:-}" ] || return 0
    # Only remove the exact setup-owned mktemp shape below STATE_DIR. This can
    # run from the global EXIT trap, so never trust a broader or partial path.
    case "$WDA_ICON_WORK_DIR" in
        "$STATE_DIR"/wda-runner-icon.*)
            /bin/rm -rf -- "$WDA_ICON_WORK_DIR" 2>/dev/null || true
            ;;
        *)
            warn "refusing to remove unexpected runner-icon work path: $WDA_ICON_WORK_DIR"
            ;;
    esac
    WDA_ICON_WORK_DIR=""
}

_restore_wda_icon_app() {
    [ "${WDA_ICON_MUTATION_ACTIVE:-0}" = "1" ] || return 0
    # Mutation starts only after all three paths have been resolved and the
    # pristine, signed runner has been copied. Revalidate their containment
    # before removing anything, including during signal/EXIT cleanup.
    case "${WDA_ICON_PRODUCTS_DIR:-}" in
        /*/Build/Products/*) ;;
        *) return 1 ;;
    esac
    case "${WDA_ICON_APP_PATH:-}" in
        "$WDA_ICON_PRODUCTS_DIR"/*-Runner.app) ;;
        *) return 1 ;;
    esac
    [ "${WDA_ICON_BACKUP_PATH:-}" = "$WDA_ICON_WORK_DIR/original.app" ] \
        && [ -d "$WDA_ICON_BACKUP_PATH" ] || return 1

    /bin/rm -rf -- "$WDA_ICON_APP_PATH" 2>/dev/null || return 1
    if /usr/bin/ditto "$WDA_ICON_BACKUP_PATH" "$WDA_ICON_APP_PATH" 2>/dev/null \
        && codesign --verify --deep --strict "$WDA_ICON_APP_PATH" 2>/dev/null; then
        WDA_ICON_MUTATION_ACTIVE=0
        return 0
    fi

    # A missing build product is safe: the unchanged `xcodebuild ... test`
    # below will rebuild it. A half-restored, invalidly signed product is not.
    /bin/rm -rf -- "$WDA_ICON_APP_PATH" 2>/dev/null || true
    WDA_ICON_MUTATION_ACTIVE=0
    return 1
}

if [ "$COMMAND" = "setup" ]; then
    mkdir -p "$STATE_DIR"
    chmod 700 "$STATE_DIR"
fi

# Self-install: keep a copy at a fixed path so the daemon's `POST /agent/mode`
# can start/stop WDA without knowing where the repo lives. Only a real setup may
# replace it; status/doctor/stop must be read-only with respect to the runtime
# script. Preserve the prior copy so a failed upgrade can roll back both the
# supervisor plist and the exact script it points to.
SELF_INSTALL="$STATE_DIR/setup-wda.sh"
SELF_INSTALL_ROLLBACK="$STATE_DIR/setup-wda.rollback.$$.sh"
SELF_INSTALL_REPLACED_THIS_RUN=0
SELF_INSTALL_HAD_PREVIOUS=0
if [ "$COMMAND" = "setup" ] \
    && [ "$(cd "$(dirname "$0")" 2>/dev/null && pwd)/$(basename "$0")" != "$SELF_INSTALL" ]; then
    if [ -f "$SELF_INSTALL" ]; then
        cp -p "$SELF_INSTALL" "$SELF_INSTALL_ROLLBACK" \
            || { printf 'could not back up existing setup-wda.sh\n' >&2; exit 1; }
        SELF_INSTALL_HAD_PREVIOUS=1
    fi
    SELF_INSTALL_TEMP="$STATE_DIR/setup-wda.install.$$"
    if cp -p "$0" "$SELF_INSTALL_TEMP" 2>/dev/null \
        && chmod 700 "$SELF_INSTALL_TEMP" 2>/dev/null \
        && mv -f "$SELF_INSTALL_TEMP" "$SELF_INSTALL" 2>/dev/null; then
        SELF_INSTALL_REPLACED_THIS_RUN=1
    else
        rm -f "$SELF_INSTALL_TEMP"
        printf 'could not atomically install setup-wda.sh at %s\n' "$SELF_INSTALL" >&2
        exit 1
    fi
fi

# devicectl can HANG FOREVER on a device whose tunnel is stuck "connecting"
# (hardware-verified 2026-06-12 — it wedged the whole mode-switch). Every call
# goes through this wrapper: run in background, kill after $1 seconds.
_devicectl_t() {
    local secs="$1"; shift
    local out; out="$(mktemp)"
    ( xcrun devicectl "$@" > "$out" 2>/dev/null ) &
    local pid=$!
    ( sleep "$secs"; kill "$pid" 2>/dev/null ) &
    local killer=$!
    wait "$pid" 2>/dev/null
    kill "$killer" 2>/dev/null
    # Reap the watchdog explicitly. Without this wait, bash may print a
    # delayed `Terminated: 15` job notification into the supervisor log even
    # though devicectl completed successfully.
    wait "$killer" 2>/dev/null || true
    cat "$out"; rm -f "$out"
}

_wda_endpoint_lock_state() {
    local body
    body="$(curl -fsS -m 3 "$TARGET_URL/wda/locked" 2>/dev/null || true)"
    if printf '%s' "$body" \
        | grep -Eq '"value"[[:space:]]*:[[:space:]]*true'; then
        printf 'locked\n'
    elif printf '%s' "$body" \
        | grep -Eq '"value"[[:space:]]*:[[:space:]]*false'; then
        printf 'unlocked\n'
    else
        printf 'unknown\n'
    fi
}

_wda_failure_is_lock_related() {
    if grep -Eiq 'Unlock iPhone to Continue|device is locked|deviceprep.*Code=-3|Code=-3.*deviceprep' \
        "$RUN_LOG" 2>/dev/null; then
        return 0
    fi
    [ "${1:-endpoint}" != "log-only" ] || return 1
    [ "$(_wda_endpoint_lock_state)" = "locked" ]
}

_exponential_retry_delay() {
    local base="$1"
    local cap="$2"
    local attempt="$3"
    local delay index
    delay="$base"
    index=1
    while [ "$index" -lt "$attempt" ] && [ "$delay" -lt "$cap" ]; do
        delay=$((delay * 2))
        [ "$delay" -le "$cap" ] || delay="$cap"
        index=$((index + 1))
    done
    printf '%s\n' "$delay"
}

_read_keepalive_retry_state() {
    KEEPALIVE_RETRY_ATTEMPT=0
    KEEPALIVE_RETRY_NEXT_AT=0
    KEEPALIVE_RETRY_KIND=""
    [ -e "$WDA_RETRY_STATE" ] || return 0
    _marker_file_secure "$WDA_RETRY_STATE" || return 1
    [ "$(awk 'END { print NR }' "$WDA_RETRY_STATE" 2>/dev/null)" = "4" ] || return 1
    [ "$(sed -n '1s/^version=//p' "$WDA_RETRY_STATE")" = "1" ] || return 1
    KEEPALIVE_RETRY_KIND="$(sed -n '2s/^kind=//p' "$WDA_RETRY_STATE")"
    KEEPALIVE_RETRY_ATTEMPT="$(sed -n '3s/^attempt=//p' "$WDA_RETRY_STATE")"
    KEEPALIVE_RETRY_NEXT_AT="$(sed -n '4s/^next_at=//p' "$WDA_RETRY_STATE")"
    case "$KEEPALIVE_RETRY_KIND" in
        generic|locked) ;;
        *) return 1 ;;
    esac
    case "$KEEPALIVE_RETRY_ATTEMPT" in
        ''|*[!0-9]*) return 1 ;;
    esac
    [ "$KEEPALIVE_RETRY_ATTEMPT" -le 64 ] 2>/dev/null || return 1
    case "$KEEPALIVE_RETRY_NEXT_AT" in
        ''|*[!0-9]*) return 1 ;;
    esac
    return 0
}

_wait_for_keepalive_retry() {
    local now wait_for
    if ! _read_keepalive_retry_state; then
        warn "ignoring invalid KeepAlive retry state: $WDA_RETRY_STATE"
        return 0
    fi
    if [ "$KEEPALIVE_RETRY_KIND" = "locked" ]; then
        KEEPALIVE_LOCK_RETRY=1
    fi
    now="$(date +%s)"
    if [ "$KEEPALIVE_RETRY_NEXT_AT" -gt "$now" ] 2>/dev/null; then
        wait_for=$((KEEPALIVE_RETRY_NEXT_AT - now))
        if [ "$KEEPALIVE_RETRY_KIND" != "locked" ]; then
            info "KeepAlive retry backoff: waiting ${wait_for}s before the next rebuild"
        fi
        sleep "$wait_for"
    fi
}

_record_keepalive_failure() {
    local attempt delay cap next_at tmp previous_kind
    previous_kind=""
    if _read_keepalive_retry_state; then
        previous_kind="$KEEPALIVE_RETRY_KIND"
    fi
    if [ "$previous_kind" = "$KEEPALIVE_FAILURE_KIND" ]; then
        attempt=$((KEEPALIVE_RETRY_ATTEMPT + 1))
        [ "$attempt" -le 64 ] || attempt=64
    else
        attempt=1
    fi
    case "$KEEPALIVE_FAILURE_KIND" in
        locked) delay=30; cap=900 ;;
        *) delay=5; cap=300; KEEPALIVE_FAILURE_KIND="generic" ;;
    esac
    delay="$(_exponential_retry_delay "$delay" "$cap" "$attempt")"
    next_at=$(( $(date +%s) + delay ))
    tmp="$(mktemp "$STATE_DIR/wda-retry-state.v1.new.XXXXXX")" || return 1
    if printf 'version=1\nkind=%s\nattempt=%s\nnext_at=%s\n' \
        "$KEEPALIVE_FAILURE_KIND" "$attempt" "$next_at" > "$tmp" \
        && chmod 600 "$tmp" \
        && mv -f "$tmp" "$WDA_RETRY_STATE"; then
        if [ "$KEEPALIVE_FAILURE_KIND" = "locked" ]; then
            _setstatus lock-backoff wda "lock screen blocked WDA; next quiet retry in ${delay}s"
            if [ "$previous_kind" != "locked" ]; then
                warn "iPhone lock screen blocked WDA; retries are now quiet and back off from 30s to 15min"
            fi
        else
            warn "KeepAlive rebuild failed; next retry in ${delay}s (failure $attempt)"
        fi
        return 0
    fi
    rm -f "$tmp"
    return 1
}

_reset_keepalive_retry() {
    if [ -L "$WDA_RETRY_STATE" ]; then
        warn "refusing to remove symlinked KeepAlive retry state: $WDA_RETRY_STATE"
        return 1
    fi
    rm -f "$WDA_RETRY_STATE"
}

_prepare_locked_retry() {
    KEEPALIVE_FAILURE_KIND="locked"
    _stop_managed_process "$MJPEG_RELAY_PID_FILE" "$LEGACY_MJPEG_EXPECTED" mjpeg || true
    _stop_managed_process "$RELAY_PID_FILE" "$LEGACY_RELAY_EXPECTED" relay || true
    _stop_managed_process "$RUNNER_PID_FILE" "$LEGACY_RUNNER_EXPECTED" runner || true
}

_interactive_lock_wait_tick() {
    local now elapsed delay
    now="${1:-$(date +%s)}"
    if [ "$INTERACTIVE_LOCK_STARTED_AT" -eq 0 ]; then
        INTERACTIVE_LOCK_STARTED_AT="$now"
        INTERACTIVE_LOCK_NOTICE_ATTEMPT=1
        delay="$(_exponential_retry_delay 30 120 "$INTERACTIVE_LOCK_NOTICE_ATTEMPT")"
        INTERACTIVE_LOCK_NOTICE_AT=$((now + delay))
        warn "phone is locked; waiting up to 5 minutes without repeating this prompt every poll (Ctrl-C to stop)"
    fi
    elapsed=$((now - INTERACTIVE_LOCK_STARTED_AT))
    if [ "$elapsed" -ge 300 ]; then
        _setstatus building-fail wda "phone remained locked for 5 minutes"
        return 1
    fi
    if [ "$now" -ge "$INTERACTIVE_LOCK_NOTICE_AT" ]; then
        warn "phone is still locked after ${elapsed}s; unlock it, or press Ctrl-C and rerun setup later"
        INTERACTIVE_LOCK_NOTICE_ATTEMPT=$((INTERACTIVE_LOCK_NOTICE_ATTEMPT + 1))
        delay="$(_exponential_retry_delay 30 120 "$INTERACTIVE_LOCK_NOTICE_ATTEMPT")"
        INTERACTIVE_LOCK_NOTICE_AT=$((now + delay))
    fi
    _setstatus building wda "phone is locked; interactive setup is waiting up to 5 minutes"
    return 0
}

STATUS_FILE="$STATE_DIR/wda-setup-status.json"
# BEGIN setup status protocol (also exercised without device access by tests).
# One owner per setup attempt. All writers, including EXIT and the heartbeat,
# compare run_id under the same lock before atomically replacing the JSON file.
_status_publish() {
    local mode="$1"; shift
    [ -n "${STATUS_RUN_ID:-}" ] || return 0
    local -a invoke=(python3)
    # The watcher must be a direct child of the setup shell, so it can detect
    # parent death without following/reaping unrelated processes.
    [ "$mode" != "watch" ] || invoke=(exec python3)
    "${invoke[@]}" - "$STATUS_FILE" "$mode" "$STATUS_RUN_ID" \
        "$STATUS_OWNER_PID" "$STATUS_OWNER_START" "$@" <<'PY_STATUS'
import fcntl
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import time

path = Path(sys.argv[1])
mode, run_id, pid_text, owner_start = sys.argv[2:6]
owner_pid = int(pid_text)
arguments = sys.argv[6:]

def owner_alive():
    try:
        output = subprocess.check_output(
            ["ps", "-p", pid_text, "-o", "lstart="], text=True,
            env={**os.environ, "LC_ALL": "C"}, timeout=2,
        )
        return output.strip() == owner_start
    except (OSError, subprocess.SubprocessError):
        return False

def publish(operation):
    flags = os.O_RDWR | os.O_CREAT | getattr(os, "O_NOFOLLOW", 0)
    fd = os.open(str(path) + ".lock", flags, 0o600)
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid():
            raise OSError("unsafe setup status lock")
        deadline = time.monotonic() + 2
        while True:
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    raise OSError("setup status lock timed out")
                time.sleep(0.02)
        if path.is_symlink():
            raise OSError("refusing symlinked setup status")
        try:
            data = json.loads(path.read_text())
            if not isinstance(data, dict):
                data = {}
        except (FileNotFoundError, ValueError):
            data = {}
        now = int(time.time())
        if operation == "begin":
            blocker = data.get("blocked_on", "")
            data = {
                "schema_version": 1, "run_id": run_id,
                "owner_pid": owner_pid, "owner_start": owner_start,
                "phase": "starting", "phase_started_at": now,
                "blocked_on": blocker if blocker in {"warp", "proxy", "usb", "trust", "ddi", "wda"} else "",
                "message": "starting setup", "active": True, "terminal": False,
            }
        elif data.get("run_id") != run_id:
            return False  # a new attempt owns the shared file now
        elif operation == "phase":
            phase, blocked, message = arguments
            if phase != data.get("phase"):
                data["phase_started_at"] = now
            terminal = phase == "ready" or phase.endswith("-fail") or phase == "lock-backoff"
            data.update(phase=phase, blocked_on=blocked, message=message,
                        active=not terminal, terminal=terminal)
        elif operation == "heartbeat":
            if not data.get("active") or data.get("terminal"):
                return False
        elif operation in {"finish", "abandoned"}:
            if operation == "abandoned" and data.get("terminal"):
                return False
            code = int(arguments[0]) if operation == "finish" else 137
            previous = str(data.get("phase", "starting"))
            if operation == "abandoned":
                phase = "interrupted"
            elif code == 130:
                phase = "stopped"
            elif code:
                phase = previous if previous.endswith("-fail") else previous + "-fail"
            else:
                phase = "ready" if previous == "ready" else "completed"
            data.update(phase=phase, last_phase=previous, active=False,
                        terminal=True, exit_code=code, ended_at=now)
            # Keep the last blocker/message as diagnostics, even on failure.
        data.update(ts=now, heartbeat_ts=now)
        temporary_fd, temporary = tempfile.mkstemp(prefix=path.name + ".", dir=path.parent)
        try:
            with os.fdopen(temporary_fd, "w") as output:
                json.dump(data, output, separators=(",", ":"))
                output.write("\n")
                output.flush()
                os.fsync(output.fileno())
            os.replace(temporary, path)
        finally:
            if os.path.exists(temporary):
                os.unlink(temporary)
        return True
    finally:
        os.close(fd)

if mode == "watch":
    # No subprocess is spawned on each one-second parent check. The expensive
    # identity recheck and heartbeat happen only once per 15 seconds.
    next_heartbeat = time.monotonic() + 15
    while True:
        if os.getppid() != owner_pid:
            publish("abandoned")
            break
        if time.monotonic() >= next_heartbeat:
            if not owner_alive():
                publish("abandoned")
                break
            if not publish("heartbeat"):
                break
            next_heartbeat = time.monotonic() + 15
        time.sleep(1)
else:
    publish(mode)
PY_STATUS
}

_status_begin_run() {
    [ "$COMMAND" = "setup" ] || return 0
    STATUS_OWNER_PID="$$"
    STATUS_OWNER_START="$(LC_ALL=C ps -p "$$" -o lstart= | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')"
    [ -n "$STATUS_OWNER_START" ] || return 1
    STATUS_RUN_ID="$$-$(date +%s)-$RANDOM$RANDOM"
    _status_publish begin || return 1
    _status_publish watch >/dev/null 2>&1 &
    STATUS_HEARTBEAT_PID=$!
}

_status_finish_run() {
    local code="$1"
    [ -n "${STATUS_RUN_ID:-}" ] || return 0
    _status_publish finish "$code" || true
    # Only signal an unreaped job belonging to this shell, never a reused PID.
    if [ -n "${STATUS_HEARTBEAT_PID:-}" ] \
        && jobs -pr | grep -Fxq "$STATUS_HEARTBEAT_PID"; then
        kill "$STATUS_HEARTBEAT_PID" 2>/dev/null || true
    fi
    [ -z "${STATUS_HEARTBEAT_PID:-}" ] || wait "$STATUS_HEARTBEAT_PID" 2>/dev/null || true
    STATUS_HEARTBEAT_PID=""
}

# $1=phase  $2=blocked_on(empty=ok)  $3=human message.
_setstatus() {
    [ "$COMMAND" = "setup" ] || return 0
    _status_publish phase "$1" "${2:-}" "${3:-}" 2>/dev/null || true
}
# END setup status protocol.

_xml_escape() {
    printf '%s' "$1" \
        | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g'
}

_valid_team_id() {
    [ "${#1}" -eq 10 ] || return 1
    case "$1" in
        *[!ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789]*) return 1 ;;
        *) return 0 ;;
    esac
}

_valid_bundle_id() {
    [ -n "$1" ] || return 1
    printf '%s\n' "$1" \
        | LC_ALL=C grep -Eq '^[A-Za-z0-9]+([A-Za-z0-9-]*[A-Za-z0-9])?(\.[A-Za-z0-9]+([A-Za-z0-9-]*[A-Za-z0-9])?)+$'
}

_valid_wda_ref() {
    [ "${#1}" -eq 40 ] || return 1
    case "$1" in
        *[!0123456789ABCDEFabcdef]*) return 1 ;;
        *) return 0 ;;
    esac
}

_valid_port() {
    case "$1" in
        ''|*[!0-9]*) return 1 ;;
    esac
    [ "$1" -ge 1 ] 2>/dev/null && [ "$1" -le 65535 ] 2>/dev/null
}

_sha256_text() {
    local value="$1"
    local output
    [ -n "$SHASUM_BIN" ] && [ -x "$SHASUM_BIN" ] || return 1
    output="$(printf '%s' "$value" | "$SHASUM_BIN" -a 256 2>/dev/null)" || return 1
    output="${output%% *}"
    [ "${#output}" -eq 64 ] || return 1
    case "$output" in
        *[!0-9a-f]*) return 1 ;;
    esac
    printf '%s\n' "$output"
}

_git_metadata_digests() {
    local checkout="$1"
    local refs reflog worktrees objects
    refs="$(GIT_OPTIONAL_LOCKS=0 git -C "$checkout" \
        for-each-ref --format='%(refname) %(objectname)' 2>/dev/null)" || return 1
    reflog="$(GIT_OPTIONAL_LOCKS=0 git -C "$checkout" \
        reflog show --all --format='%H %gD %gs' 2>/dev/null)" || return 1
    worktrees="$(GIT_OPTIONAL_LOCKS=0 git -C "$checkout" \
        worktree list --porcelain 2>/dev/null)" || return 1
    objects="$(GIT_OPTIONAL_LOCKS=0 git -C "$checkout" \
        cat-file --batch-all-objects --batch-check='%(objectname)' 2>/dev/null)" \
        || return 1
    objects="$(printf '%s\n' "$objects" | LC_ALL=C sort)" || return 1
    WDA_MARKER_REFS_DIGEST="$(_sha256_text "$refs")" || return 1
    WDA_MARKER_REFLOG_DIGEST="$(_sha256_text "$reflog")" || return 1
    WDA_MARKER_WORKTREES_DIGEST="$(_sha256_text "$worktrees")" || return 1
    WDA_MARKER_OBJECTS_DIGEST="$(_sha256_text "$objects")" || return 1
    return 0
}

_marker_file_secure() {
    local marker="$1"
    local metadata owner mode
    [ ! -L "$marker" ] && [ -f "$marker" ] || return 1
    metadata="$(/usr/bin/stat -f '%u|%Lp' "$marker" 2>/dev/null)" || return 1
    owner="${metadata%%|*}"
    mode="${metadata#*|}"
    [ "$owner" = "$UID_NUM" ] && [ "$mode" = "600" ]
}

_existing_marker_matches_checkout() {
    local checkout="$1"
    local origin="$2"
    local head="$3"
    local marker="$WDA_CHECKOUT_MARKER"
    local count version path marker_origin marker_head
    local refs_digest reflog_digest worktrees_digest objects_digest
    _marker_file_secure "$marker" || return 1
    count="$(awk 'END { print NR }' "$marker" 2>/dev/null)" || return 1
    [ "$count" = "9" ] || return 1
    version="$(sed -n '1s/^version=//p' "$marker")"
    path="$(sed -n '2s/^path=//p' "$marker")"
    marker_origin="$(sed -n '3s/^origin=//p' "$marker")"
    marker_head="$(sed -n '4s/^head=//p' "$marker")"
    # The old UDID may legitimately differ when setup is moving the managed
    # checkout to another explicitly selected phone. It is validated and
    # rebound only after the new device path is fully proven.
    case "$(sed -n '5s/^udid=//p' "$marker")" in
        ''|*[!0-9A-Fa-f-]*) return 1 ;;
    esac
    refs_digest="$(sed -n '6s/^refs_sha256=//p' "$marker")"
    reflog_digest="$(sed -n '7s/^reflog_sha256=//p' "$marker")"
    worktrees_digest="$(sed -n '8s/^worktrees_sha256=//p' "$marker")"
    objects_digest="$(sed -n '9s/^objects_sha256=//p' "$marker")"
    [ "$version" = "1" ] \
        && [ "$path" = "$checkout" ] \
        && [ "$marker_origin" = "$origin" ] \
        && [ "$(printf '%s' "$marker_head" | tr 'A-F' 'a-f')" \
            = "$(printf '%s' "$head" | tr 'A-F' 'a-f')" ] \
        || return 1
    _git_metadata_digests "$checkout" || return 1
    [ "$refs_digest" = "$WDA_MARKER_REFS_DIGEST" ] \
        && [ "$reflog_digest" = "$WDA_MARKER_REFLOG_DIGEST" ] \
        && [ "$worktrees_digest" = "$WDA_MARKER_WORKTREES_DIGEST" ] \
        && [ "$objects_digest" = "$WDA_MARKER_OBJECTS_DIGEST" ]
}

_write_wda_checkout_marker() {
    local canonical origin head tmp
    [ "$WDA_MARKER_REFRESH_ALLOWED" = "1" ] || return 2
    canonical="$(cd -P "$WDA_DIR" 2>/dev/null && pwd)" || return 1
    case "$canonical" in
        *$'\n'*|*$'\r'*) return 1 ;;
    esac
    origin="$(git -C "$canonical" config --get remote.origin.url 2>/dev/null || true)"
    case "$origin" in
        https://github.com/appium/WebDriverAgent|\
        https://github.com/appium/WebDriverAgent.git|\
        git@github.com:appium/WebDriverAgent.git|\
        ssh://git@github.com/appium/WebDriverAgent.git)
            ;;
        *) return 1 ;;
    esac
    head="$(git -C "$canonical" rev-parse HEAD 2>/dev/null || true)"
    [ "$(printf '%s' "$head" | tr 'A-F' 'a-f')" \
        = "$(printf '%s' "$WDA_REF" | tr 'A-F' 'a-f')" ] || return 1
    case "$WDA_UDID" in
        ''|*[!0-9A-Fa-f-]*) return 1 ;;
    esac
    _git_metadata_digests "$canonical" || return 1
    tmp="$(mktemp "$STATE_DIR/wda-checkout-owner.v1.new.XXXXXX")" || return 1
    if printf '%s\n' \
        "version=1" \
        "path=$canonical" \
        "origin=$origin" \
        "head=$head" \
        "udid=$WDA_UDID" \
        "refs_sha256=$WDA_MARKER_REFS_DIGEST" \
        "reflog_sha256=$WDA_MARKER_REFLOG_DIGEST" \
        "worktrees_sha256=$WDA_MARKER_WORKTREES_DIGEST" \
        "objects_sha256=$WDA_MARKER_OBJECTS_DIGEST" > "$tmp" \
        && chmod 600 "$tmp" \
        && mv -f "$tmp" "$WDA_CHECKOUT_MARKER" \
        && _marker_file_secure "$WDA_CHECKOUT_MARKER" \
        && _existing_marker_matches_checkout "$canonical" "$origin" "$head"; then
        WDA_CANONICAL_DIR="$canonical"
        return 0
    fi
    rm -f "$tmp"
    return 1
}

# Resolve one signing identity for doctor, setup, and the persisted supervisor.
# A shared default bundle ID cannot work across Apple Developer teams, so a fresh
# install derives a legal, team-specific suffix only after the Team ID validates.
_resolve_signing_identity() {
    SIGNING_ERROR=""
    BUNDLE_ID_DERIVED=0
    TEAM_ID="${WDA_TEAM_ID:-$(defaults read com.apple.dt.Xcode IDEProvisioningTeamManagerLastSelectedTeamID 2>/dev/null || true)}"
    if [ -z "$TEAM_ID" ]; then
        SIGNING_ERROR="No Apple Team ID. In Xcode open Settings → Accounts, select the team, then rerun; or export WDA_TEAM_ID=<10-character Team ID>."
        return 1
    fi
    if ! _valid_team_id "$TEAM_ID"; then
        SIGNING_ERROR="Invalid WDA_TEAM_ID '$TEAM_ID'. Expected exactly 10 uppercase ASCII letters/digits, e.g. ABCD123456."
        return 1
    fi
    WDA_TEAM_ID="$TEAM_ID"
    if [ -z "$WDA_BUNDLE_ID" ]; then
        WDA_BUNDLE_ID="com.leeguoo.iphone-use.wda.$(printf '%s' "$TEAM_ID" \
            | tr 'ABCDEFGHIJKLMNOPQRSTUVWXYZ' 'abcdefghijklmnopqrstuvwxyz')"
        BUNDLE_ID_DERIVED=1
    fi
    if ! _valid_bundle_id "$WDA_BUNDLE_ID"; then
        SIGNING_ERROR="Invalid WDA_BUNDLE_ID '$WDA_BUNDLE_ID'. Use dot-separated ASCII letters, digits, dots, and hyphens only."
        return 1
    fi
    if ! _valid_wda_ref "$WDA_REF"; then
        SIGNING_ERROR="Invalid WDA_REF '$WDA_REF'. Pin one exact 40-character upstream commit SHA."
        return 1
    fi
    WDA_REF="$(printf '%s' "$WDA_REF" | tr 'ABCDEF' 'abcdef')"
    if [ "$WDA_REF" = "$DEFAULT_WDA_REF" ]; then
        WDA_REF_LABEL="$DEFAULT_WDA_REF_TAG"
    else
        WDA_REF_LABEL="custom pin"
    fi
    return 0
}

_wait_job_gone() {
    local label="$1"
    local _
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        launchctl print "$GUI_DOMAIN/$label" >/dev/null 2>&1 || return 0
        sleep 0.5
    done
    ! launchctl print "$GUI_DOMAIN/$label" >/dev/null 2>&1
}

_job_is_disabled() {
    local state
    state="$(_job_disabled_state "$1")" || return 2
    [ "$state" = "1" ]
}

_job_disabled_state() {
    local label="$1"
    local disabled_output
    disabled_output="$(launchctl print-disabled "$GUI_DOMAIN" 2>/dev/null)" \
        || return 1
    if printf '%s\n' "$disabled_output" \
        | awk -v key="\"$label\"" \
            '$1 == key && $2 == "=>" && ($3 == "true" || $3 == "disabled") { found=1 } END { exit !found }'; then
        printf '1\n'
    else
        printf '0\n'
    fi
}

_restore_job_policy() {
    local label="$1"
    local expected_disabled="$2"
    local restored_disabled
    case "$expected_disabled" in
        0)
            launchctl enable "$GUI_DOMAIN/$label" >/dev/null 2>&1 || return 1
            ;;
        1)
            launchctl disable "$GUI_DOMAIN/$label" >/dev/null 2>&1 || return 1
            ;;
        *)
            return 1
            ;;
    esac
    restored_disabled="$(_job_disabled_state "$label")" || return 1
    [ "$restored_disabled" = "$expected_disabled" ]
}

_plist_set_env() {
    local plist="$1"
    local key="$2"
    local value="$3"
    if ! /usr/libexec/PlistBuddy \
        -c "Set :EnvironmentVariables:$key $value" "$plist" 2>/dev/null; then
        /usr/libexec/PlistBuddy \
            -c "Add :EnvironmentVariables:$key string $value" "$plist"
    fi
}

# Install the same dedicated WDA supervisor used by POST /agent/mode. Keeping
# setup-wda.sh in its own launchd job means daemon restarts cannot reap WDA, and
# KeepAlive can rebuild after sleep/USB/CoreDevice failures.
_install_wda_supervisor() {
    local env_block=""
    local key value escaped
    local setup_xml log_xml

    if [ ! -x "$SELF_INSTALL" ]; then
        warn "fixed setup script is missing or not executable: $SELF_INSTALL"
        return 1
    fi

    for key in \
        WDA_KEEPALIVE PATH WDA_UDID WDA_TEAM_ID WDA_BUNDLE_ID \
        WDA_DIR WDA_REF WDA_RUNNER_ICON WDA_PORT MJPEG_PORT WDA_ALLOW_LAN \
        WDA_ASC_KEY_PATH WDA_ASC_KEY_ID WDA_ASC_ISSUER_ID
    do
        case "$key" in
            WDA_KEEPALIVE) value="1" ;;
            PATH) value="/opt/homebrew/bin:/usr/local/bin:/usr/sbin:/sbin:/usr/bin:/bin" ;;
            WDA_UDID) value="$WDA_UDID" ;;
            WDA_TEAM_ID) value="$TEAM_ID" ;;
            WDA_BUNDLE_ID) value="$WDA_BUNDLE_ID" ;;
            WDA_DIR) value="$WDA_DIR" ;;
            WDA_REF) value="$WDA_REF" ;;
            WDA_RUNNER_ICON) value="$WDA_RUNNER_ICON" ;;
            WDA_PORT) value="$WDA_PORT" ;;
            MJPEG_PORT) value="$MJPEG_PORT" ;;
            WDA_ALLOW_LAN) value="${WDA_ALLOW_LAN:-}" ;;
            WDA_ASC_KEY_PATH|WDA_ASC_KEY_ID|WDA_ASC_ISSUER_ID)
                _asc_signing_enabled || continue
                value="${!key}"
                ;;
        esac
        [ -n "$value" ] || continue
        escaped="$(_xml_escape "$value")"
        env_block="${env_block}        <key>${key}</key><string>${escaped}</string>
"
    done

    mkdir -p "$HOME/Library/LaunchAgents"
    setup_xml="$(_xml_escape "$SELF_INSTALL")"
    log_xml="$(_xml_escape "$WDA_AGENT_LOG")"
    WDA_AGENT_STAGED_PLIST="${WDA_AGENT_PLIST}.install.$$"
    if ! cat > "$WDA_AGENT_STAGED_PLIST" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
    <key>Label</key><string>${WDA_AGENT_LABEL}</string>
    <key>ProgramArguments</key>
    <array><string>/bin/bash</string><string>${setup_xml}</string></array>
    <key>EnvironmentVariables</key>
    <dict>
${env_block}    </dict>
    <key>KeepAlive</key><true/>
    <!-- The script persists a 5s→10s→…300s retry schedule. Keep launchd's
         own floor at 5s so it does not flatten the first retry steps. -->
    <key>ThrottleInterval</key><integer>5</integer>
    <key>RunAtLoad</key><true/>
    <key>StandardOutPath</key><string>${log_xml}</string>
    <key>StandardErrorPath</key><string>${log_xml}</string>
</dict></plist>
PLIST
    then
        rm -f "$WDA_AGENT_STAGED_PLIST"
        WDA_AGENT_STAGED_PLIST=""
        warn "could not stage the WDA supervisor plist"
        return 1
    fi
    if ! chmod 600 "$WDA_AGENT_STAGED_PLIST" \
        || ! plutil -lint "$WDA_AGENT_STAGED_PLIST" >/dev/null 2>&1; then
        rm -f "$WDA_AGENT_STAGED_PLIST"
        WDA_AGENT_STAGED_PLIST=""
        warn "generated WDA supervisor plist is invalid"
        return 1
    fi
    if ! mv -f "$WDA_AGENT_STAGED_PLIST" "$WDA_AGENT_PLIST"; then
        rm -f "$WDA_AGENT_STAGED_PLIST"
        WDA_AGENT_STAGED_PLIST=""
        warn "could not atomically install the WDA supervisor plist"
        return 1
    fi
    WDA_AGENT_STAGED_PLIST=""

    launchctl bootout "$GUI_DOMAIN/$WDA_AGENT_LABEL" 2>/dev/null || true
    if ! _wait_job_gone "$WDA_AGENT_LABEL"; then
        warn "old WDA supervisor did not finish stopping"
        return 1
    fi
    # A prior installer may have persistently disabled the legacy WDA label.
    # Clear that policy before bootstrap; enabling afterward is too late.
    launchctl enable "$GUI_DOMAIN/$WDA_AGENT_LABEL" 2>/dev/null || true
    if ! launchctl bootstrap "$GUI_DOMAIN" "$WDA_AGENT_PLIST" 2>/dev/null; then
        warn "could not bootstrap WDA supervisor: $WDA_AGENT_PLIST"
        return 1
    fi
    if launchctl print "$GUI_DOMAIN/$WDA_AGENT_LABEL" >/dev/null 2>&1; then
        ok "WDA supervisor job loaded: $GUI_DOMAIN/$WDA_AGENT_LABEL"
        return 0
    fi
    warn "launchctl accepted the WDA plist but the job is not visible"
    return 1
}

# UDIDs of iPhones physically on USB (usbmuxd — always present, no libimobiledevice
# needed, can't hang like devicectl).
_usb_udids() {
python3 - 2>/dev/null <<'PY'
import socket,struct,plistlib
try:
    s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM);s.settimeout(3);s.connect("/var/run/usbmuxd")
    p=plistlib.dumps({"MessageType":"ListDevices","ClientVersionString":"x","ProgName":"x"})
    s.sendall(struct.pack("<IIII",len(p)+16,1,8,1)+p)
    h=s.recv(16);ln=struct.unpack("<I",h[:4])[0];d=b""
    while len(d)<ln-16:d+=s.recv(ln-16-len(d))
    print(" ".join(sorted({x["Properties"]["SerialNumber"] for x in plistlib.loads(d).get("DeviceList",[]) if x["Properties"].get("ConnectionType")=="USB"})))
except Exception: pass
PY
}

_target_on_usb() {
    [ -n "${WDA_UDID:-}" ] || return 1
    case " $(_usb_udids) " in
        *" $WDA_UDID "*) return 0 ;;
        *) return 1 ;;
    esac
}

# WARP breaks the CoreDevice tunnel xcodebuild needs to install AND keep WDA
# alive (hardware-verified: the runner dies "connection was invalidated" the
# moment WARP reconnects). This was the #1 cause of the whole "Device is busy /
# Waiting for developer services" nightmare. Detect it up front.
PREVIOUS_SUPERVISOR_LOADED=0
PREVIOUS_SUPERVISOR_PLIST_PRESENT=0
PREVIOUS_SUPERVISOR_DISABLED=0
SUPERVISOR_TRANSACTION_ACTIVE=0
SUPERVISOR_HANDOFF_COMPLETE=0
DAEMON_TRANSACTION_ACTIVE=0
DAEMON_JOB_WAS_LOADED=0
DAEMON_WAS_DISABLED=0
STARTED_RUNNER=0
STARTED_CONTROL_RELAY=0
STARTED_MJPEG_RELAY=0
WDA_AGENT_STAGED_PLIST=""
DAEMON_STAGED_PLIST=""
# -w (whole word): plain "Connected" also matches "Dis-Connected" as a substring,
# which mis-read WARP-off as WARP-on and blocked WDA for no reason (user-reported).
_warp_cli() {
    if [ -n "${IPHONE_USE_INTERNAL_TEST_WARP_CLI:-}" ]; then
        printf '%s\n' "$IPHONE_USE_INTERNAL_TEST_WARP_CLI"
    else
        command -v warp-cli 2>/dev/null
    fi
}
_warp_on() {
    local cli
    cli="$(_warp_cli)" || return 1
    [ -n "$cli" ] \
        && "$cli" status 2>/dev/null | grep -qiw "Connected"
}

_warp_mode() {
    local cli
    cli="$(_warp_cli)" || return 1
    [ -n "$cli" ] || return 1
    "$cli" settings 2>/dev/null | awk '
        match($0, /Mode:[[:space:]]*/) {
            value = substr($0, RSTART + RLENGTH)
            gsub(/^[[:space:]]+|[[:space:]]+$/, "", value)
            print value
            exit
        }
    '
}

# Local proxy mode only tunnels HTTP(S) requests explicitly sent to the
# loopback proxy. It does not install catch-all routes, so CoreDevice traffic
# cannot be captured even while the client reports Connected. It is route-safe,
# although Cloudflare's request timeout can make it unsuitable for long uploads.
_warp_local_proxy_mode() {
    case "$(_warp_mode 2>/dev/null | tr '[:upper:]' '[:lower:]')" in
        proxy|localproxy|"local proxy"|warpproxy*) return 0 ;;
        *) return 1 ;;
    esac
}

# CoreDevice creates an RSD tunnel even for a USB-connected iPhone. The
# device-facing interface uses IPv6 link-local plus a dynamic ULA /64 (observed
# as fd2c:.../64); if WARP captures either range, devicectl hangs and a live
# xcodebuild/WDA session is invalidated. Trust the client's effective route
# dump, not only the shorter policy summary shown by `warp-cli settings`.
_warp_coredevice_bypass_ready() {
    local cli excluded
    cli="$(_warp_cli)" || return 1
    [ -n "$cli" ] || return 1
    excluded="$("$cli" tunnel dump 2>/dev/null | awk '
        /^Excluded:[[:space:]]*$/ { inside = 1; next }
        inside && /^[A-Za-z][A-Za-z ]*:[[:space:]]*$/ { exit }
        inside {
            gsub(/^[[:space:]]+|[[:space:]]+$/, "")
            if (length($0)) print
        }
    ')" || return 1
    printf '%s\n' "$excluded" | grep -Fxq "fe80::/10" || return 1
    if printf '%s\n' "$excluded" | grep -Fxq "fd00::/8"; then
        return 0
    fi
    # A broader ULA exclusion is also sufficient.
    printf '%s\n' "$excluded" | grep -Fxq "fc00::/7"
}

WARP_PREFLIGHT_ERROR=""
_warp_preflight() {
    WARP_PREFLIGHT_ERROR=""
    _warp_on || return 0
    _warp_local_proxy_mode && return 0
    if _warp_coredevice_bypass_ready; then
        return 0
    fi
    WARP_PREFLIGHT_ERROR="WARP is connected, but its effective Split Tunnel exclusions do not cover the CoreDevice device tunnel.
   If WARP is only needed for specific destinations, prefer a Traffic only
   device profile with Split Tunnels in Include mode and only those destination
   IPs/CIDRs. (Local proxy mode is also route-safe, but its request timeout can
   be unsuitable for long Git uploads.)
   Otherwise add BOTH routes to the device profile's Exclude list:
     - fe80::/10  (IPv6 link-local)
     - fd00::/8   (CoreDevice RSD ULA)
   Then wait for policy propagation/reconnect and verify with:
     warp-cli tunnel dump
   Temporary alternative: warp-cli disconnect
   The script did not change WARP or organization policy."
    return 1
}

_warp_ready_summary() {
    if _warp_local_proxy_mode; then
        printf '%s\n' "WARP: connected in Local proxy mode; only explicitly proxied traffic is tunneled"
    else
        printf '%s\n' "WARP: connected with CoreDevice Split Tunnel exclusions (fe80::/10 + fd00::/8)"
    fi
}

# Read the top-level fixed proxy entries from the current System Configuration
# snapshot reported by `scutil --proxy`. Nested scoped/interface dictionaries
# are separate configurations and must not overwrite the global active values.
# Output is protocol|host|port, one enabled entry per line.
_system_proxy_entries() {
    local scutil_bin="${IPHONE_USE_INTERNAL_TEST_SCUTIL:-/usr/sbin/scutil}"
    local snapshot
    [ -x "$scutil_bin" ] || return 1
    snapshot="$("$scutil_bin" --proxy 2>/dev/null)" || return 1
    printf '%s\n' "$snapshot" | awk '
        {
            before = depth
            braces = $0
            opens = gsub(/{/, "{", braces)
            closes = gsub(/}/, "}", braces)
            if (before == 0 && opens > 0 && $1 == "<dictionary>") {
                root_seen = 1
            }
            if (before == 1 && $2 == ":" &&
                $1 ~ /^(HTTP|HTTPS|SOCKS)(Enable|Proxy|Port)$/) {
                value[$1] = $3
                if ($1 ~ /Enable$/) {
                    enable_seen = 1
                }
            }
            depth += opens - closes
            if (depth < 0) {
                invalid = 1
            }
        }
        END {
            if (!root_seen || !enable_seen || depth != 0 || invalid) {
                exit 65
            }
            protocols[1] = "HTTP"
            protocols[2] = "HTTPS"
            protocols[3] = "SOCKS"
            for (i = 1; i <= 3; i++) {
                protocol = protocols[i]
                if (value[protocol "Enable"] == "1") {
                    printf "%s|%s|%s\n", protocol,
                        value[protocol "Proxy"], value[protocol "Port"]
                }
            }
        }
    '
}

_valid_proxy_host() {
    [ -n "$1" ] \
        && printf '%s\n' "$1" | LC_ALL=C grep -Eq '^[A-Za-z0-9._:%-]+$'
}

_proxy_is_loopback() {
    local host
    host="$(printf '%s' "$1" | tr 'ABCDEFGHIJKLMNOPQRSTUVWXYZ' 'abcdefghijklmnopqrstuvwxyz')"
    case "$host" in
        localhost|localhost.|::1|0:0:0:0:0:0:0:1) return 0 ;;
    esac
    printf '%s\n' "$host" | awk -F. '
        NF != 4 || $1 != "127" { exit 1 }
        {
            for (i = 2; i <= 4; i++) {
                if ($i !~ /^[0-9]+$/ || $i > 255) {
                    exit 1
                }
            }
        }
    '
}

# A successful TCP connect proves only that the configured local endpoint
# exists. It deliberately does not claim that the listener speaks the selected
# proxy protocol or that CoreDevice supports the proxy.
_proxy_tcp_reachable() {
    local host="$1"
    local port="$2"
    local probe="${IPHONE_USE_INTERNAL_TEST_PROXY_PROBE:-}"
    if [ -n "$probe" ]; then
        [ -x "$probe" ] || return 2
        "$probe" "$host" "$port" >/dev/null 2>&1
        return $?
    fi
    if [ -x /usr/bin/nc ]; then
        /usr/bin/nc -z -w 1 "$host" "$port" >/dev/null 2>&1
        return $?
    fi
    return 2
}

# Detect active macOS HTTP/HTTPS/SOCKS settings without changing them. A
# reachable proxy is only reported as a diagnostic variable. We fail closed
# only for a malformed enabled entry or a loopback endpoint with no listener:
# those are concrete local configuration faults and the latter reproduced the
# CoreDevice/DDI failure that motivated this check.
SYSTEM_PROXY_ERROR=""
_system_proxy_check() {
    local entries
    local protocol host port probe_status
    local active=0
    local invalid=""
    local dead=""
    SYSTEM_PROXY_ERROR=""

    if ! entries="$(_system_proxy_entries)"; then
        SYSTEM_PROXY_ERROR="Could not inspect macOS HTTP/HTTPS/SOCKS proxy state with /usr/sbin/scutil.
   The script did not change any proxy settings. Run '/usr/sbin/scutil --proxy'
   to repair System Configuration access, then rerun setup."
        return 1
    fi
    if [ -z "$entries" ]; then
        ok "System proxies (HTTP/HTTPS/SOCKS): none enabled"
        return 0
    fi

    while IFS='|' read -r protocol host port; do
        [ -n "$protocol" ] || continue
        active=1
        if ! _valid_proxy_host "$host" || ! _valid_port "$port"; then
            invalid="${invalid}${invalid:+, }$protocol"
            continue
        fi
        if _proxy_is_loopback "$host"; then
            if _proxy_tcp_reachable "$host" "$port"; then
                warn "~ $protocol system proxy enabled at $host:$port (TCP listener responds)"
            else
                probe_status=$?
                if [ "$probe_status" = "1" ]; then
                    dead="${dead}${dead:+, }$protocol $host:$port"
                else
                    warn "~ $protocol system proxy enabled at $host:$port (local endpoint could not be probed)"
                fi
            fi
        else
            warn "~ $protocol system proxy enabled at $host:$port (endpoint not probed)"
        fi
    done <<EOF
$entries
EOF

    if [ -n "$invalid" ] || [ -n "$dead" ]; then
        SYSTEM_PROXY_ERROR="macOS has an enabled but unusable system proxy configuration."
        if [ -n "$invalid" ]; then
            SYSTEM_PROXY_ERROR="$SYSTEM_PROXY_ERROR
   Invalid or incomplete entries: $invalid"
        fi
        if [ -n "$dead" ]; then
            SYSTEM_PROXY_ERROR="$SYSTEM_PROXY_ERROR
   No reachable TCP listener at configured loopback endpoints: $dead"
        fi
        SYSTEM_PROXY_ERROR="$SYSTEM_PROXY_ERROR
   A stale system proxy can prevent Xcode/CoreDevice from reaching developer services;
   this check does not claim that the iPhone or DDI is defective.
   Fix ONE of:
     - restart the proxy app so the listed local endpoint is listening
     - disable only the stale protocols in System Settings -> Network -> active service
       -> Details -> Proxies
   The script did not change proxy settings. Verify with '/usr/sbin/scutil --proxy',
   then rerun setup."
        return 1
    fi

    if [ "$active" = "1" ]; then
        warn "~ Active system proxies are not automatically treated as a blocker. If CoreDevice/DDI stalls, retry after bypassing or disabling them."
    fi
    return 0
}

if [ "${IPHONE_USE_INTERNAL_TEST_PROXY_PREFLIGHT_ONLY:-0}" = "1" ]; then
    [ "$COMMAND" = "doctor" ] \
        || die "internal proxy preflight fixture requires the read-only doctor command"
    if _system_proxy_check; then
        exit 0
    fi
    warn "X $SYSTEM_PROXY_ERROR"
    exit 1
fi

_restore_backup_file() {
    local backup="$1"
    local target="$2"
    local mode="$3"
    local restore_tmp="${target}.restore.$$"
    [ -f "$backup" ] || return 1
    if cp -p "$backup" "$restore_tmp" 2>/dev/null \
        && chmod "$mode" "$restore_tmp" 2>/dev/null \
        && mv -f "$restore_tmp" "$target" 2>/dev/null \
        && cmp -s "$backup" "$target"; then
        return 0
    fi
    rm -f "$restore_tmp"
    return 1
}

_cleanup_on_exit() {
    local status=$?
    local cleanup_failed=0
    local self_restore_ok=1
    local supervisor_restore_ok=1
    local daemon_restore_ok=1
    set +e
    if [ "${WDA_ICON_MUTATION_ACTIVE:-0}" = "1" ]; then
        warn "setup stopped during runner-icon injection — restoring the pristine signed app"
        _restore_wda_icon_app || cleanup_failed=1
    fi
    if [ "${WDA_ICON_MUTATION_ACTIVE:-0}" != "1" ]; then
        _cleanup_wda_icon_work_dir
    else
        warn "runner-icon recovery backup retained at: $WDA_ICON_BACKUP_PATH"
    fi
    if [ "$status" -ne 0 ]; then
        if [ "${STARTED_MJPEG_RELAY:-0}" = "1" ]; then
            _stop_managed_process "$MJPEG_RELAY_PID_FILE" "$LEGACY_MJPEG_EXPECTED" mjpeg \
                || cleanup_failed=1
        fi
        if [ "${STARTED_CONTROL_RELAY:-0}" = "1" ]; then
            _stop_managed_process "$RELAY_PID_FILE" "$LEGACY_RELAY_EXPECTED" relay \
                || cleanup_failed=1
        fi
        if [ "${STARTED_RUNNER:-0}" = "1" ]; then
            _stop_managed_process "$RUNNER_PID_FILE" "$LEGACY_RUNNER_EXPECTED" runner \
                || cleanup_failed=1
        fi
    fi
    if [ "$status" -ne 0 ] && [ "$SELF_INSTALL_REPLACED_THIS_RUN" = "1" ]; then
        if [ "$SELF_INSTALL_HAD_PREVIOUS" = "1" ]; then
            if _restore_backup_file "$SELF_INSTALL_ROLLBACK" "$SELF_INSTALL" 700; then
                rm -f "$SELF_INSTALL_ROLLBACK"
            else
                self_restore_ok=0
                cleanup_failed=1
                warn "could not restore the prior setup script; rescue backup retained at:
   $SELF_INSTALL_ROLLBACK"
            fi
        else
            rm -f "$SELF_INSTALL"
            if [ -e "$SELF_INSTALL" ]; then
                self_restore_ok=0
                cleanup_failed=1
                warn "could not remove the newly installed setup script: $SELF_INSTALL"
            fi
        fi
    fi
    if [ "$status" -ne 0 ] \
        && [ "$SUPERVISOR_TRANSACTION_ACTIVE" = "1" ] \
        && [ "$SUPERVISOR_HANDOFF_COMPLETE" != "1" ]; then
        warn "setup failed — restoring the prior WDA supervisor file and loaded state"
        launchctl bootout "$GUI_DOMAIN/$WDA_AGENT_LABEL" >/dev/null 2>&1 || true
        if ! _wait_job_gone "$WDA_AGENT_LABEL"; then
            supervisor_restore_ok=0
            cleanup_failed=1
            warn "new WDA supervisor did not fully stop during rollback"
        fi
        if [ "$PREVIOUS_SUPERVISOR_PLIST_PRESENT" = "1" ]; then
            if ! _restore_backup_file "$WDA_AGENT_ROLLBACK_PLIST" \
                "$WDA_AGENT_PLIST" 600; then
                supervisor_restore_ok=0
                cleanup_failed=1
                warn "could not restore the prior supervisor plist; rescue backup retained at:
   $WDA_AGENT_ROLLBACK_PLIST"
            fi
        else
            rm -f "$WDA_AGENT_PLIST"
            if [ -e "$WDA_AGENT_PLIST" ]; then
                supervisor_restore_ok=0
                cleanup_failed=1
                warn "could not remove the newly created supervisor plist: $WDA_AGENT_PLIST"
            fi
        fi
        if [ "$PREVIOUS_SUPERVISOR_LOADED" = "1" ]; then
            if [ "$supervisor_restore_ok" = "1" ] \
                && [ "$self_restore_ok" = "1" ] \
                && plutil -lint "$WDA_AGENT_PLIST" >/dev/null 2>&1; then
                launchctl enable "$GUI_DOMAIN/$WDA_AGENT_LABEL" >/dev/null 2>&1 || true
                if ! launchctl bootstrap "$GUI_DOMAIN" "$WDA_AGENT_PLIST" >/dev/null 2>&1 \
                    || ! launchctl print "$GUI_DOMAIN/$WDA_AGENT_LABEL" >/dev/null 2>&1; then
                    supervisor_restore_ok=0
                    cleanup_failed=1
                    warn "prior WDA supervisor plist was restored, but its loaded state was not"
                fi
            else
                supervisor_restore_ok=0
                cleanup_failed=1
                warn "prior WDA supervisor was not restarted because its files were not fully restored"
            fi
        fi
        _restore_job_policy "$WDA_AGENT_LABEL" "$PREVIOUS_SUPERVISOR_DISABLED" \
            || supervisor_restore_ok=0
        if [ "$supervisor_restore_ok" = "1" ]; then
            rm -f "$WDA_AGENT_ROLLBACK_PLIST"
        else
            cleanup_failed=1
            [ -f "$WDA_AGENT_ROLLBACK_PLIST" ] \
                && warn "supervisor rescue backup retained at: $WDA_AGENT_ROLLBACK_PLIST"
        fi
    fi
    if [ "$status" -ne 0 ] && [ "$DAEMON_TRANSACTION_ACTIVE" = "1" ]; then
        warn "setup failed — restoring the prior daemon configuration and loaded state"
        launchctl bootout "$GUI_DOMAIN/$DAEMON_LABEL" >/dev/null 2>&1 || true
        if ! _wait_job_gone "$DAEMON_LABEL"; then
            daemon_restore_ok=0
            cleanup_failed=1
            warn "new daemon job did not fully stop during rollback"
        fi
        if ! _restore_backup_file "$DAEMON_ROLLBACK_PLIST" "$DAEMON_PLIST" 600; then
            daemon_restore_ok=0
            cleanup_failed=1
            warn "could not restore the prior daemon plist; rescue backup retained at:
   $DAEMON_ROLLBACK_PLIST"
        fi
        if [ "$DAEMON_JOB_WAS_LOADED" = "1" ]; then
            if [ "$daemon_restore_ok" = "1" ] \
                && plutil -lint "$DAEMON_PLIST" >/dev/null 2>&1; then
                launchctl enable "$GUI_DOMAIN/$DAEMON_LABEL" >/dev/null 2>&1 || true
                if ! launchctl bootstrap "$GUI_DOMAIN" "$DAEMON_PLIST" >/dev/null 2>&1 \
                    || ! launchctl print "$GUI_DOMAIN/$DAEMON_LABEL" >/dev/null 2>&1; then
                    daemon_restore_ok=0
                    cleanup_failed=1
                    warn "prior daemon plist was restored, but its loaded state was not"
                fi
            else
                daemon_restore_ok=0
                cleanup_failed=1
            fi
        fi
        _restore_job_policy "$DAEMON_LABEL" "$DAEMON_WAS_DISABLED" \
            || daemon_restore_ok=0
        if [ "$daemon_restore_ok" = "1" ]; then
            rm -f "$DAEMON_ROLLBACK_PLIST"
        else
            cleanup_failed=1
            [ -f "$DAEMON_ROLLBACK_PLIST" ] \
                && warn "daemon rescue backup retained at: $DAEMON_ROLLBACK_PLIST"
        fi
    fi
    [ -z "${WDA_AGENT_STAGED_PLIST:-}" ] || rm -f "$WDA_AGENT_STAGED_PLIST"
    [ -z "${DAEMON_STAGED_PLIST:-}" ] || rm -f "$DAEMON_STAGED_PLIST"
    if [ "$status" -ne 0 ] && [ "$status" -ne 130 ] \
        && [ "${WDA_KEEPALIVE:-0}" = "1" ] \
        && [ "${KEEPALIVE_ATTEMPT_ACTIVE:-0}" = "1" ]; then
        KEEPALIVE_ATTEMPT_ACTIVE=0
        _record_keepalive_failure || cleanup_failed=1
    fi
    [ "$cleanup_failed" = "0" ] || status=1
    _status_finish_run "$status"
    trap - EXIT
    exit "$status"
}
trap _cleanup_on_exit EXIT
trap 'exit 130' INT TERM

if [ -n "${IPHONE_USE_INTERNAL_TEST_KEEPALIVE_RETRY_KIND:-}" ]; then
    [ "$COMMAND" = "doctor" ] \
        || die "internal KeepAlive retry fixture requires the read-only doctor command"
    case "$IPHONE_USE_INTERNAL_TEST_KEEPALIVE_RETRY_KIND" in
        generic|locked)
            KEEPALIVE_FAILURE_KIND="$IPHONE_USE_INTERNAL_TEST_KEEPALIVE_RETRY_KIND"
            _record_keepalive_failure
            ;;
        reset)
            _reset_keepalive_retry
            ;;
        *)
            die "invalid internal KeepAlive retry fixture"
            ;;
    esac
    exit $?
fi

_warp_check() {
    if _warp_preflight; then
        if _warp_on; then
            ok "$(_warp_ready_summary)"
        fi
        return 0
    fi
    _setstatus prereq warp "WARP is connected and breaks CoreDevice"
    die "$WARP_PREFLIGHT_ERROR
   WARP would otherwise invalidate the just-verified WDA session and create a restart loop.
   See docs/wda-setup.html pitfall (WARP)."
}

if [ "${IPHONE_USE_INTERNAL_TEST_WARP_PREFLIGHT_ONLY:-0}" = "1" ]; then
    [ "$COMMAND" = "doctor" ] \
        || die "internal WARP preflight fixture requires the read-only doctor command"
    if _warp_preflight; then
        if _warp_on; then
            ok "$(_warp_ready_summary)"
        else
            ok "WARP: off / not present"
        fi
        exit 0
    fi
    warn "X $WARP_PREFLIGHT_ERROR"
    exit 1
fi

# One-shot preflight: report the FIRST blocker as a checklist instead of a blind
# wait loop.  `setup-wda.sh doctor`
cmd_doctor() {
    info "WDA preflight"
    local fail=0
    local xcode_version
    if [ -d "$STATE_DIR" ]; then
        ok "setup state directory present: $STATE_DIR"
    else
        warn "~ setup state is not initialized; doctor will not create it"
    fi
    xcode_version="$(xcodebuild -version 2>/dev/null | head -1 || true)"
    if [ -n "$xcode_version" ]; then
        ok "Full Xcode: $xcode_version"
    else
        warn "X full Xcode unavailable (install Xcode, then select it with xcode-select)"
        fail=1
    fi
    if _resolve_signing_identity; then
        ok "Dev team: $TEAM_ID"
        if [ "$BUNDLE_ID_DERIVED" = "1" ]; then
            ok "Runner bundle ID: $WDA_BUNDLE_ID (derived for this team)"
        else
            ok "Runner bundle ID: $WDA_BUNDLE_ID (explicit or persisted)"
        fi
        ok "WDA source pin: $WDA_REF_LABEL $WDA_REF"
    else
        warn "X $SIGNING_ERROR"
        fail=1
    fi
    if _valid_port "$WDA_PORT" && _valid_port "$MJPEG_PORT" \
        && [ "$WDA_PORT" != "$MJPEG_PORT" ]; then
        ok "Loopback ports: control $WDA_PORT, video $MJPEG_PORT"
    else
        warn "X WDA_PORT and MJPEG_PORT must be distinct TCP ports from 1 to 65535"
        fail=1
    fi
    if _warp_preflight; then
        if _warp_on; then
            ok "$(_warp_ready_summary)"
        else
            ok "WARP: off / not present"
        fi
    else
        warn "X $WARP_PREFLIGHT_ERROR"
        fail=1
    fi
    if ! _system_proxy_check; then
        warn "X $SYSTEM_PROXY_ERROR"
        fail=1
    fi
    local usb usb_count
    usb="$(_usb_udids)"
    usb_count="$(printf '%s' "$usb" | wc -w | tr -d '[:space:]')"
    if [ "$WDA_ALLOW_LAN" = "0" ] && [ -z "$usb" ]; then
        warn "X default Direct/WDA requires an iPhone connected over USB"
        fail=1
    elif [ "$WDA_ALLOW_LAN" = "0" ] && [ "$usb_count" -gt 1 ] \
        && [ -z "${WDA_UDID:-}" ]; then
        warn "X multiple USB iPhones found ($usb); set WDA_UDID=<one exact UDID>"
        fail=1
    elif [ "$WDA_ALLOW_LAN" = "0" ] && [ -n "${WDA_UDID:-}" ] \
        && ! _target_on_usb; then
        warn "X configured target $WDA_UDID is not connected over USB"
        fail=1
    elif [ -n "$usb" ]; then
        ok "iPhone on USB: $usb"
    else
        warn "~ WDA_ALLOW_LAN=1: no USB iPhone; setup will require one unambiguous paired destination"
    fi
    if command -v lsof >/dev/null 2>&1; then ok "lsof present for listener ownership checks"; else warn "X lsof is required"; fail=1; fi
    if [ "$WDA_ALLOW_LAN" = "0" ]; then
        if command -v iproxy >/dev/null 2>&1; then
            ok "iproxy present for the default USB-only relay"
        else
            warn "X iproxy is required for the default USB relay: brew install libimobiledevice"
            fail=1
        fi
    elif command -v iproxy >/dev/null 2>&1 || command -v socat >/dev/null 2>&1; then
        ok "relay tool present for explicit WDA_ALLOW_LAN=1 mode"
    else
        warn "X WDA_ALLOW_LAN=1 still requires iproxy or socat"
        fail=1
    fi
    if _valid_port "$WDA_PORT"; then
        curl -s -m 4 "http://127.0.0.1:$WDA_PORT/status" >/dev/null 2>&1 \
            && ok "WDA already serving on 127.0.0.1:$WDA_PORT"
    fi
    if [ "$fail" = 0 ]; then
        ok "preflight checks passed; build, signing, device trust, and launch still require setup verification"
    else
        warn "fix the X items above, then re-run"
    fi
    return $fail
}

_refresh_legacy_contracts() {
    local legacy_udid="${WDA_UDID:-__missing_udid__}"
    local legacy_team="${WDA_TEAM_ID:-__missing_team__}"
    local legacy_bundle="${WDA_BUNDLE_ID:-__missing_bundle__}"
    LEGACY_RUNNER_EXPECTED="legacy-runner:$legacy_udid:$legacy_team:$legacy_bundle"
    LEGACY_RELAY_EXPECTED="legacy-relay:$WDA_PORT:8100:$legacy_udid"
    LEGACY_MJPEG_EXPECTED="legacy-mjpeg:$MJPEG_PORT:9100:$legacy_udid"
}
_refresh_legacy_contracts
MJPEG_RELAY_PID_FILE="$STATE_DIR/wda-mjpeg-relay.pid"
VALIDATED_PID=""
PID_RECORD_PID=""
PID_RECORD_LSTART=""
PID_RECORD_EXPECTED=""
PID_RECORD_LEGACY=0

_pid_exists() {
    case "$1" in
        ''|0|1|0*|*[!0-9]*) return 1 ;;
    esac
    ps -p "$1" -o pid= >/dev/null 2>&1
}

_safe_expected() {
    case "$1" in
        ''|*'|'*|*$'\n'*|*$'\r'*) return 1 ;;
        *) return 0 ;;
    esac
}

_store_pid_record() {
    local file="$1"
    local pid="$2"
    local lstart="$3"
    local expected="$4"
    local tmp="${file}.tmp.$$"
    _pid_exists "$pid" || return 1
    [ -n "$lstart" ] || return 1
    _safe_expected "$expected" || return 1
    printf '%s|%s|%s\n' "$pid" "$lstart" "$expected" > "$tmp" || return 1
    chmod 600 "$tmp" || { rm -f "$tmp"; return 1; }
    mv -f "$tmp" "$file"
}

_pid_record_parse() {
    local file="$1"
    local legacy_expected="$2"
    local record rest
    PID_RECORD_PID=""
    PID_RECORD_LSTART=""
    PID_RECORD_EXPECTED=""
    PID_RECORD_LEGACY=0
    [ -f "$file" ] || return 1
    [ "$(awk 'END { print NR }' "$file" 2>/dev/null)" = "1" ] || return 1
    record="$(sed -n '1p' "$file" 2>/dev/null || true)"
    case "$record" in
        *'|'*)
            PID_RECORD_PID="${record%%|*}"
            rest="${record#*|}"
            case "$rest" in
                *'|'*) ;;
                *) return 1 ;;
            esac
            PID_RECORD_LSTART="${rest%%|*}"
            PID_RECORD_EXPECTED="${rest#*|}"
            [ -n "$PID_RECORD_LSTART" ] || return 1
            _safe_expected "$PID_RECORD_EXPECTED" || return 1
            ;;
        *)
            # Legacy files contained only a PID. They remain compatible, but the
            # caller supplies a UDID/port-specific expected command contract.
            PID_RECORD_PID="$record"
            PID_RECORD_EXPECTED="$legacy_expected"
            PID_RECORD_LEGACY=1
            ;;
    esac
    case "$PID_RECORD_PID" in
        ''|0|1|0*|*[!0-9]*) return 1 ;;
    esac
    _safe_expected "$PID_RECORD_EXPECTED" || return 1
    return 0
}

_expected_role_valid() {
    local expected="$1"
    local role="$2"
    case "$role:$expected" in
        runner:runner:?*|runner:legacy-runner:?*) return 0 ;;
        relay:relay:?*|relay:legacy-relay:?*) return 0 ;;
        mjpeg:mjpeg:?*|mjpeg:legacy-mjpeg:?*) return 0 ;;
        *) return 1 ;;
    esac
}

# The optional ASC suffix must be complete and in the order emitted by
# _prepare_xcodebuild_args. Do not replace it with an arbitrary-arguments tail:
# these signatures authorize signalling the recorded process.
_runner_signature_valid() {
    local asc_suffix
    _safe_expected "$1" || return 1
    asc_suffix=' -authenticationKeyPath /[^|[:cntrl:]]+\.p8 -authenticationKeyID [A-Za-z0-9]+ -authenticationKeyIssuerID [A-Za-z0-9-]+ -allowProvisioningDeviceRegistration'
    printf '%s\n' "$1" | LC_ALL=C grep -Eq \
        "^(/[^ ]*/)?xcodebuild (-project WebDriverAgent\\.xcodeproj -scheme WebDriverAgentRunner -destination platform=iOS,id=[0-9A-Fa-f-]+ -allowProvisioningUpdates DEVELOPMENT_TEAM=[A-Z0-9]{10} PRODUCT_BUNDLE_IDENTIFIER=[A-Za-z0-9.-]+ test($asc_suffix)?|-destination platform=iOS,id=[0-9A-Fa-f-]+ test-without-building -xctestrun /[^ ]+/WebDriverAgentRunner_[^ /]+\\.xctestrun( -allowProvisioningUpdates$asc_suffix)?)$"
}

_command_matches_expected() {
    local command="$1"
    local expected="$2"
    local signature rest local_port device_port target_udid team_id bundle_id
    local legacy_argv xctestrun_argv base_command
    case "$expected" in
        runner:*)
            signature="${expected#*:}"
            _runner_signature_valid "$signature" || return 1
            [ "$command" = "$signature" ]
            ;;
        relay:*|mjpeg:*)
            signature="${expected#*:}"
            if printf '%s\n' "$signature" | LC_ALL=C grep -Eq \
                '^(/[^ ]*/)?iproxy -s 127\.0\.0\.1 [0-9]+:[0-9]+ -u [0-9A-Fa-f-]+$'; then
                :
            elif printf '%s\n' "$signature" | LC_ALL=C grep -Eq \
                '^(/[^ ]*/)?socat TCP-LISTEN:[0-9]+,fork,reuseaddr,bind=127\.0\.0\.1 TCP:[A-Za-z0-9.:%_-]+:[0-9]+$'; then
                :
            else
                return 1
            fi
            [ "$command" = "$signature" ]
            ;;
        legacy-runner:*)
            rest="${expected#legacy-runner:}"
            target_udid="${rest%%:*}"
            rest="${rest#*:}"
            team_id="${rest%%:*}"
            bundle_id="${rest#*:}"
            case "$target_udid" in
                ''|__missing_udid__|*[!0-9A-Fa-f-]*) return 1 ;;
            esac
            _valid_team_id "$team_id" || return 1
            _valid_bundle_id "$bundle_id" || return 1
            _runner_signature_valid "$command" || return 1
            # Strip only the complete, validated suffix before checking the
            # legacy UDID/team/bundle contract. New PID records still require
            # exact equality with the full command, including the key path.
            base_command="${command%% -authenticationKeyPath *}"
            base_command="${base_command% -allowProvisioningUpdates}"
            legacy_argv="xcodebuild -project WebDriverAgent.xcodeproj -scheme WebDriverAgentRunner -destination platform=iOS,id=$target_udid -allowProvisioningUpdates DEVELOPMENT_TEAM=$team_id PRODUCT_BUNDLE_IDENTIFIER=$bundle_id"
            # A runner started with an injected icon runs `test-without-building
            # -xctestrun <path>` instead of `test`. Both are ours; anything else
            # is not. Matching only `test` would leave `pause`/`stop` unable to
            # recognise (and therefore unable to stop) an icon-carrying runner.
            # A runner started with an injected icon runs the xctestrun form,
            # which xcodebuild forbids combining with -project/-scheme — so it
            # shares only the destination with the normal form. Both are ours;
            # matching just one would leave `pause`/`stop` unable to recognise
            # (and therefore unable to stop) the other.
            xctestrun_argv="xcodebuild -destination platform=iOS,id=$target_udid test-without-building -xctestrun"
            case "$base_command" in
                "$legacy_argv test"|*/"$legacy_argv test") return 0 ;;
                "$xctestrun_argv /"*/WebDriverAgentRunner_*.xctestrun \
                |*/"$xctestrun_argv /"*/WebDriverAgentRunner_*.xctestrun)
                    # One path argument, no embedded spaces.
                    case "${base_command#*-xctestrun }" in
                        *" "*) return 1 ;;
                    esac
                    return 0
                    ;;
                *) return 1 ;;
            esac
            ;;
        legacy-relay:*|legacy-mjpeg:*)
            rest="${expected#*:}"
            local_port="${rest%%:*}"
            rest="${rest#*:}"
            device_port="${rest%%:*}"
            target_udid="${rest#*:}"
            _valid_port "$local_port" || return 1
            _valid_port "$device_port" || return 1
            case "$target_udid" in
                ''|__missing_udid__|*[!0-9A-Fa-f-]*) return 1 ;;
            esac
            case "$command" in
                "iproxy $local_port $device_port -u $target_udid"|\
                */"iproxy $local_port $device_port -u $target_udid"|\
                "iproxy -s 127.0.0.1 $local_port:$device_port -u $target_udid"|\
                */"iproxy -s 127.0.0.1 $local_port:$device_port -u $target_udid")
                    return 0
                    ;;
                # A legacy socat argv has no UDID, so it cannot prove which
                # phone it belongs to. Refuse to adopt or kill it.
                *) return 1 ;;
            esac
            ;;
        *) return 1 ;;
    esac
}

_validate_legacy_runner_cwd() {
    local pid="$1"
    local cwd_data process_cwd expected_cwd
    command -v lsof >/dev/null 2>&1 || return 1
    [ -d "$WDA_DIR" ] || return 1
    expected_cwd="$(cd "$WDA_DIR" 2>/dev/null && pwd -P)" || return 1
    cwd_data="$(lsof -nP -a -p "$pid" -d cwd -Fn 2>/dev/null)" || return 1
    process_cwd="$(printf '%s\n' "$cwd_data" | sed -n 's/^n//p' | head -1)"
    [ -n "$process_cwd" ] && [ "$process_cwd" = "$expected_cwd" ]
}

_verify_listener_owner_pid() {
    local pid="$1"
    local port="$2"
    local listener_data listener_pids unexpected
    listener_data="$(lsof -nP -a -iTCP:"$port" -sTCP:LISTEN -Fp 2>/dev/null)" \
        || return 1
    listener_pids="$(printf '%s\n' "$listener_data" | sed -n 's/^p//p' | sort -u)"
    [ -n "$listener_pids" ] || return 1
    unexpected="$(printf '%s\n' "$listener_pids" \
        | awk -v p="$pid" '$0 != p { print; exit }')"
    [ -z "$unexpected" ]
}

_legacy_migration_hint() {
    warn "legacy pid-only state was found but could not be proven safe to stop automatically.
   Retry with the exact old values (do not guess):
     WDA_UDID=<old-device-udid> WDA_TEAM_ID=<10-char-team> \\
     WDA_BUNDLE_ID=<old-runner-bundle-id> WDA_DIR=<old-wda-checkout> \\
       $SELF_INSTALL stop
   If the old relay used socat, inspect its numeric PID with both:
     ps -ww -p <pid> -o uid=,lstart=,command=
     lsof -nP -a -p <pid> -iTCP:<8100-or-9100> -sTCP:LISTEN
   This script intentionally will not turn an unproven legacy PID into a global kill."
}

_validate_pid_record() {
    local file="$1"
    local legacy_expected="$2"
    local role="$3"
    local adopt_legacy="${4:-0}"
    local process_uid process_lstart process_command
    VALIDATED_PID=""
    _pid_record_parse "$file" "$legacy_expected" || return 1
    _expected_role_valid "$PID_RECORD_EXPECTED" "$role" || return 1
    _pid_exists "$PID_RECORD_PID" || return 1
    process_uid="$(ps -p "$PID_RECORD_PID" -o uid= 2>/dev/null | tr -d '[:space:]')"
    [ "$process_uid" = "$UID_NUM" ] || return 1
    process_lstart="$(LC_ALL=C ps -p "$PID_RECORD_PID" -o lstart= 2>/dev/null \
        | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')"
    [ -n "$process_lstart" ] || return 1
    if [ -n "$PID_RECORD_LSTART" ] \
        && [ "$process_lstart" != "$PID_RECORD_LSTART" ]; then
        return 1
    fi
    process_command="$(LC_ALL=C ps -ww -p "$PID_RECORD_PID" -o command= 2>/dev/null \
        | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')"
    _command_matches_expected "$process_command" "$PID_RECORD_EXPECTED" || return 1
    if [ "$PID_RECORD_LEGACY" = "1" ]; then
        case "$PID_RECORD_EXPECTED" in
            legacy-runner:*)
                _validate_legacy_runner_cwd "$PID_RECORD_PID" || return 1
                ;;
        esac
        if [ "$adopt_legacy" = "1" ]; then
            # Only a mutating setup/stop path may adopt a target-verified legacy
            # PID. Status validates the same evidence without rewriting state.
            _store_pid_record "$file" "$PID_RECORD_PID" "$process_lstart" \
                "$PID_RECORD_EXPECTED" || return 1
            _validate_pid_record "$file" "$legacy_expected" "$role" 0
            return $?
        fi
    fi
    [ "$(LC_ALL=C ps -p "$PID_RECORD_PID" -o lstart= 2>/dev/null \
        | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')" = "$process_lstart" ] \
        || return 1
    [ "$(LC_ALL=C ps -ww -p "$PID_RECORD_PID" -o command= 2>/dev/null \
        | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')" = "$process_command" ] \
        || return 1
    VALIDATED_PID="$PID_RECORD_PID"
    return 0
}

_write_pid_record() {
    local file="$1"
    local pid="$2"
    local expected="$3"
    local role="$4"
    local process_uid process_lstart process_command tries
    _expected_role_valid "$expected" "$role" || return 1
    _safe_expected "$expected" || return 1
    tries=0
    while [ "$tries" -lt 30 ]; do
        tries=$((tries + 1))
        _pid_exists "$pid" || return 1
        process_uid="$(ps -p "$pid" -o uid= 2>/dev/null | tr -d '[:space:]')"
        process_lstart="$(LC_ALL=C ps -p "$pid" -o lstart= 2>/dev/null \
            | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')"
        process_command="$(LC_ALL=C ps -ww -p "$pid" -o command= 2>/dev/null \
            | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')"
        if [ "$process_uid" = "$UID_NUM" ] \
            && [ -n "$process_lstart" ] \
            && _command_matches_expected "$process_command" "$expected"; then
            break
        fi
        sleep 0.1
    done
    [ "$process_uid" = "$UID_NUM" ] || return 1
    [ -n "$process_lstart" ] || return 1
    _command_matches_expected "$process_command" "$expected" || return 1
    _store_pid_record "$file" "$pid" "$process_lstart" "$expected" || return 1
    _validate_pid_record "$file" "$expected" "$role"
}

_stop_managed_process() {
    local file="$1"
    local legacy_expected="$2"
    local role="$3"
    local candidate tries was_legacy legacy_rest legacy_port
    [ -f "$file" ] || return 0
    _pid_record_parse "$file" "$legacy_expected" || {
        warn "invalid managed PID record; refusing to act: $file"
        return 1
    }
    candidate="$PID_RECORD_PID"
    was_legacy="$PID_RECORD_LEGACY"
    if ! _validate_pid_record "$file" "$legacy_expected" "$role" 1; then
        if _pid_exists "$candidate"; then
            [ "$was_legacy" = "1" ] && _legacy_migration_hint
            warn "PID $candidate does not match the current-user $role identity; refusing to kill it"
            return 1
        fi
        rm -f "$file"
        return 0
    fi
    if [ "$was_legacy" = "1" ]; then
        case "$legacy_expected" in
            legacy-relay:*|legacy-mjpeg:*)
                legacy_rest="${legacy_expected#*:}"
                legacy_port="${legacy_rest%%:*}"
                if ! _verify_listener_owner_pid "$VALIDATED_PID" "$legacy_port"; then
                    warn "legacy $role PID does not exclusively own TCP $legacy_port; refusing to kill it"
                    _legacy_migration_hint
                    return 1
                fi
                _validate_pid_record "$file" "$legacy_expected" "$role" || return 1
                ;;
        esac
    fi
    kill -TERM "$VALIDATED_PID" 2>/dev/null || true
    tries=0
    while _pid_exists "$VALIDATED_PID" && [ "$tries" -lt 20 ]; do
        tries=$((tries + 1))
        sleep 0.25
    done
    if _pid_exists "$VALIDATED_PID"; then
        warn "managed $role pid $VALIDATED_PID did not stop after SIGTERM"
        return 1
    fi
    rm -f "$file"
    return 0
}

_assert_port_free() {
    local port="$1"
    local listeners lsof_error lsof_status
    command -v lsof >/dev/null 2>&1 || {
        warn "lsof is required to prove TCP $port is free"
        return 1
    }
    lsof_error="$STATE_DIR/.lsof-error.$$"
    if listeners="$(lsof -nP -a -iTCP:"$port" -sTCP:LISTEN 2>"$lsof_error")"; then
        lsof_status=0
    else
        lsof_status=$?
    fi
    if [ "$lsof_status" -eq 1 ] && [ ! -s "$lsof_error" ]; then
        rm -f "$lsof_error"
        return 0
    fi
    if [ "$lsof_status" -ne 0 ]; then
        warn "lsof could not prove TCP $port is free:"
        sed 's/^/    /' "$lsof_error" >&2
        rm -f "$lsof_error"
        return 1
    fi
    if [ -s "$lsof_error" ]; then
        warn "lsof returned diagnostics, so TCP $port is not proven free:"
        sed 's/^/    /' "$lsof_error" >&2
        rm -f "$lsof_error"
        return 1
    fi
    rm -f "$lsof_error"
    [ -z "$listeners" ] && return 0
    warn "TCP $port is already owned by a non-managed listener:"
    printf '%s\n' "$listeners" | sed 's/^/    /' >&2
    return 1
}

_verify_loopback_listener() {
    local file="$1"
    local legacy_expected="$2"
    local role="$3"
    local port="$4"
    local pid listener_data listener_pids listener_name_data listener_names unexpected
    command -v lsof >/dev/null 2>&1 || return 1
    _validate_pid_record "$file" "$legacy_expected" "$role" || return 1
    pid="$VALIDATED_PID"
    listener_data="$(lsof -nP -a -iTCP:"$port" -sTCP:LISTEN -Fp 2>/dev/null)" \
        || return 1
    listener_pids="$(printf '%s\n' "$listener_data" | sed -n 's/^p//p' | sort -u)"
    [ -n "$listener_pids" ] || return 1
    unexpected="$(printf '%s\n' "$listener_pids" | awk -v p="$pid" '$0 != p { print; exit }')"
    [ -z "$unexpected" ] || return 1
    listener_name_data="$(lsof -nP -a -p "$pid" -iTCP:"$port" -sTCP:LISTEN -Fn 2>/dev/null)" \
        || return 1
    listener_names="$(printf '%s\n' "$listener_name_data" | sed -n 's/^n//p')"
    [ -n "$listener_names" ] || return 1
    unexpected="$(printf '%s\n' "$listener_names" \
        | awk -v n="127.0.0.1:$port" '$0 != n { print; exit }')"
    [ -z "$unexpected" ] || return 1
    _validate_pid_record "$file" "$legacy_expected" "$role" || return 1
    [ "$VALIDATED_PID" = "$pid" ] || return 1
    return 0
}

cmd_stop() {
    local failed=0
    if [ ! -d "$STATE_DIR" ]; then
        warn "setup state is not initialized at $STATE_DIR; there are no PID-owned processes to stop safely"
        return 1
    fi
    info "Stopping the dedicated WDA supervisor and its managed processes"
    launchctl bootout "$GUI_DOMAIN/$WDA_AGENT_LABEL" 2>/dev/null || true
    _wait_job_gone "$WDA_AGENT_LABEL" || failed=1
    _stop_managed_process "$RUNNER_PID_FILE" "$LEGACY_RUNNER_EXPECTED" runner || failed=1
    _stop_managed_process "$RELAY_PID_FILE" "$LEGACY_RELAY_EXPECTED" relay || failed=1
    _stop_managed_process "$MJPEG_RELAY_PID_FILE" "$LEGACY_MJPEG_EXPECTED" mjpeg || failed=1
    if launchctl print "$GUI_DOMAIN/$WDA_AGENT_LABEL" >/dev/null 2>&1; then
        failed=1
    fi
    if [ "$failed" != "0" ]; then
        warn "stop was not fully verified; no unowned process was killed"
        return 1
    fi
    ok "WDA supervisor and all PID-verified managed processes stopped"
}

cmd_pause() {
    local failed=0
    if [ ! -d "$STATE_DIR" ]; then
        warn "setup state is not initialized at $STATE_DIR; there is no managed WDA stack to pause"
        return 1
    fi
    info "Pausing managed WDA and giving the phone back to the user"
    # Disable the exact launchd label before bootout so KeepAlive cannot race
    # the PID-verified shutdown. Never use pkill: another xcodebuild or relay
    # may belong to the user rather than this setup.
    launchctl disable "$GUI_DOMAIN/$WDA_AGENT_LABEL" >/dev/null 2>&1 || failed=1
    launchctl bootout "$GUI_DOMAIN/$WDA_AGENT_LABEL" 2>/dev/null || true
    _wait_job_gone "$WDA_AGENT_LABEL" || failed=1
    _stop_managed_process "$RUNNER_PID_FILE" "$LEGACY_RUNNER_EXPECTED" runner || failed=1
    _stop_managed_process "$RELAY_PID_FILE" "$LEGACY_RELAY_EXPECTED" relay || failed=1
    _stop_managed_process "$MJPEG_RELAY_PID_FILE" "$LEGACY_MJPEG_EXPECTED" mjpeg || failed=1
    _reset_keepalive_retry || failed=1
    [ "$(_job_disabled_state "$WDA_AGENT_LABEL" 2>/dev/null || true)" = "1" ] \
        || failed=1
    if launchctl print "$GUI_DOMAIN/$WDA_AGENT_LABEL" >/dev/null 2>&1; then
        failed=1
    fi
    if [ "$failed" != "0" ]; then
        warn "pause was not fully verified; no process without a matching PID/argv record was killed"
        return 1
    fi
    ok "WDA paused: supervisor disabled and all PID-verified runner/relay processes stopped"
    printf '  Resume: %s resume\n' "$SELF_INSTALL"
}

cmd_resume() {
    local label program interpreter
    if [ ! -d "$STATE_DIR" ]; then
        warn "setup state is not initialized at $STATE_DIR; run setup before resume"
        return 1
    fi
    if ! _marker_file_secure "$WDA_AGENT_PLIST" \
        || ! plutil -lint "$WDA_AGENT_PLIST" >/dev/null 2>&1; then
        warn "managed WDA supervisor plist is missing or unsafe: $WDA_AGENT_PLIST; run setup again"
        return 1
    fi
    label="$(/usr/libexec/PlistBuddy -c 'Print :Label' "$WDA_AGENT_PLIST" 2>/dev/null || true)"
    interpreter="$(/usr/libexec/PlistBuddy -c 'Print :ProgramArguments:0' "$WDA_AGENT_PLIST" 2>/dev/null || true)"
    program="$(/usr/libexec/PlistBuddy -c 'Print :ProgramArguments:1' "$WDA_AGENT_PLIST" 2>/dev/null || true)"
    if [ "$label" != "$WDA_AGENT_LABEL" ] \
        || [ "$interpreter" != "/bin/bash" ] \
        || [ "$program" != "$SELF_INSTALL" ] \
        || [ ! -x "$SELF_INSTALL" ]; then
        warn "managed WDA supervisor identity is not the expected setup helper; run setup again"
        return 1
    fi

    info "Resuming the managed WDA supervisor"
    _reset_keepalive_retry || return 1
    if ! launchctl enable "$GUI_DOMAIN/$WDA_AGENT_LABEL" >/dev/null 2>&1; then
        warn "could not enable the WDA supervisor"
        return 1
    fi
    if ! launchctl print "$GUI_DOMAIN/$WDA_AGENT_LABEL" >/dev/null 2>&1 \
        && ! launchctl bootstrap "$GUI_DOMAIN" "$WDA_AGENT_PLIST" >/dev/null 2>&1; then
        launchctl disable "$GUI_DOMAIN/$WDA_AGENT_LABEL" >/dev/null 2>&1 || true
        warn "could not bootstrap the WDA supervisor; it remains paused"
        return 1
    fi
    if [ "$(_job_disabled_state "$WDA_AGENT_LABEL" 2>/dev/null || true)" != "0" ] \
        || ! launchctl print "$GUI_DOMAIN/$WDA_AGENT_LABEL" >/dev/null 2>&1; then
        launchctl disable "$GUI_DOMAIN/$WDA_AGENT_LABEL" >/dev/null 2>&1 || true
        warn "resume was not verified; the WDA supervisor remains paused"
        return 1
    fi
    ok "WDA resume requested; lock-screen failures will retry with quiet backoff"
    printf '  Status: %s status\n' "$SELF_INSTALL"
    printf '  Log   : %s\n' "$WDA_AGENT_LOG"
}

cmd_status() {
    local failed=0
    if [ ! -d "$STATE_DIR" ]; then
        warn "setup state is not initialized at $STATE_DIR; run setup before requesting runtime status"
        return 1
    fi
    if ! _valid_port "$WDA_PORT" || ! _valid_port "$MJPEG_PORT" \
        || [ "$WDA_PORT" = "$MJPEG_PORT" ]; then
        warn "WDA_PORT and MJPEG_PORT must be distinct decimal TCP ports from 1 to 65535"
        return 1
    fi
    if [ "$(_job_disabled_state "$WDA_AGENT_LABEL" 2>/dev/null || true)" = "1" ]; then
        warn "WDA is paused; run $SELF_INSTALL resume before the next agent session"
        return 1
    fi
    if ! command -v lsof >/dev/null 2>&1; then
        warn "lsof is required to verify relay PID ownership and loopback-only binds"
        return 1
    fi
    if launchctl print "$GUI_DOMAIN/$WDA_AGENT_LABEL" >/dev/null 2>&1; then
        ok "WDA supervisor loaded: $GUI_DOMAIN/$WDA_AGENT_LABEL"
    else
        warn "WDA supervisor not loaded"
        failed=1
    fi
    if _validate_pid_record "$RUNNER_PID_FILE" "$LEGACY_RUNNER_EXPECTED" runner; then
        ok "PID-verified WDA runner alive: $VALIDATED_PID"
    else
        warn "WDA runner PID record is absent, stale, or does not match its process"
        failed=1
    fi
    if _verify_loopback_listener "$RELAY_PID_FILE" "$LEGACY_RELAY_EXPECTED" relay "$WDA_PORT"; then
        ok "control relay PID owns only 127.0.0.1:$WDA_PORT"
    else
        warn "control relay ownership/bind could not be verified"
        failed=1
    fi
    if _verify_loopback_listener "$MJPEG_RELAY_PID_FILE" "$LEGACY_MJPEG_EXPECTED" mjpeg "$MJPEG_PORT"; then
        ok "video relay PID owns only 127.0.0.1:$MJPEG_PORT"
    else
        warn "video relay ownership/bind could not be verified"
        failed=1
    fi
    if curl -fsS -m 4 "http://127.0.0.1:$WDA_PORT/status" >/dev/null 2>&1; then
        ok "WDA /status reachable through the loopback relay"
    else
        warn "WDA /status is not reachable"
        failed=1
    fi
    return "$failed"
}

case "$COMMAND" in
    stop)   cmd_stop;   exit $? ;;
    pause)  cmd_pause;  exit $? ;;
    resume) cmd_resume; exit $? ;;
    status) cmd_status; exit $? ;;
    doctor) cmd_doctor; exit $? ;;
    setup)  ;;
    *) die "unknown command: $1 (use: setup|status|stop|pause|resume|doctor)" ;;
esac

if [ "${WDA_KEEPALIVE:-0}" = "1" ]; then
    _wait_for_keepalive_retry
    KEEPALIVE_ATTEMPT_ACTIVE=1
fi
_status_begin_run || die "could not initialize the setup status owner"

# A manual setup temporarily owns the lifecycle so an already-running KeepAlive
# job cannot race its build or relays. A successful run installs and bootstraps
# the same supervisor again after WDA has been proven reachable.
if [ "${WDA_KEEPALIVE:-0}" != "1" ]; then
    SUPERVISOR_TRANSACTION_ACTIVE=1
    PREVIOUS_SUPERVISOR_DISABLED="$(_job_disabled_state "$WDA_AGENT_LABEL")" \
        || die "could not snapshot the WDA supervisor's launchd disabled policy"
    if [ -f "$WDA_AGENT_PLIST" ]; then
        PREVIOUS_SUPERVISOR_PLIST_PRESENT=1
        cp -p "$WDA_AGENT_PLIST" "$WDA_AGENT_ROLLBACK_PLIST" \
            || die "could not save the existing WDA supervisor plist for rollback"
    fi
    if launchctl print "$GUI_DOMAIN/$WDA_AGENT_LABEL" >/dev/null 2>&1; then
        [ "$PREVIOUS_SUPERVISOR_PLIST_PRESENT" = "1" ] \
            || die "WDA supervisor is loaded but its plist is missing; refusing an unrecoverable handoff"
        info "Pausing existing WDA supervisor for interactive setup"
        PREVIOUS_SUPERVISOR_LOADED=1
        launchctl bootout "$GUI_DOMAIN/$WDA_AGENT_LABEL" 2>/dev/null || true
        _wait_job_gone "$WDA_AGENT_LABEL" \
            || die "existing WDA supervisor did not stop; refusing to race it"
    fi
fi

# ── 0. Prereqs ────────────────────────────────────────────────────────────────
# Keep the last known blocker visible while KeepAlive starts its next pass.
# Clearing it before the corresponding check completed made `/agent/status`
# flicker between the real cause and an empty blocker every few seconds.
_PREVIOUS_BLOCKER="$(sed -n 's/.*"blocked_on":"\([^"]*\)".*/\1/p' "$STATUS_FILE" 2>/dev/null | head -1)"
case "$_PREVIOUS_BLOCKER" in
    warp|proxy|usb|trust|ddi|wda) ;;
    *) _PREVIOUS_BLOCKER="" ;;
esac
# A USB blocker from an earlier default-mode attempt is incompatible with an
# explicit LAN run. Keeping it visible while the LAN setup builds made status
# falsely tell callers to attach a cable even though socat recovery was active.
if [ "$WDA_ALLOW_LAN" = "1" ] && [ "$_PREVIOUS_BLOCKER" = "usb" ]; then
    _PREVIOUS_BLOCKER=""
fi
# Trust is checked only once xcodebuild reaches the on-device runner. Keep that
# blocker visible across KeepAlive's next prerequisite/build pass; clearing it
# at `building` made status oscillate back to an empty blocker while the phone
# still required the same manual approval. `serving` below is the first
# authoritative evidence that trust was restored, and clears it there.
case "$_PREVIOUS_BLOCKER" in
    trust) _BUILD_BLOCKER="$_PREVIOUS_BLOCKER" ;;
    *) _BUILD_BLOCKER="" ;;
esac
_setstatus prereq "$_PREVIOUS_BLOCKER" "checking prerequisites"
info "Checking prerequisites"
_warp_check   # permits route-safe WARP or explicit-only Local proxy mode
if ! _system_proxy_check; then
    _setstatus prereq proxy "macOS system proxy is enabled but unusable"
    die "$SYSTEM_PROXY_ERROR"
fi
command -v git >/dev/null 2>&1 || die "git is required to fetch the pinned WebDriverAgent source"
command -v lsof >/dev/null 2>&1 || die "lsof is required to verify exclusive loopback relay ownership"
XCODEBUILD_BIN="$(command -v xcodebuild || true)"
[ -n "$XCODEBUILD_BIN" ] \
    || die "xcodebuild is unavailable (install full Xcode, then select it with xcode-select)"
_valid_port "$WDA_PORT" \
    || die "WDA_PORT must be a decimal TCP port from 1 to 65535 (got '$WDA_PORT')"
_valid_port "$MJPEG_PORT" \
    || die "MJPEG_PORT must be a decimal TCP port from 1 to 65535 (got '$MJPEG_PORT')"
[ "$WDA_PORT" != "$MJPEG_PORT" ] \
    || die "WDA_PORT and MJPEG_PORT must be different (both are '$WDA_PORT')"
XCODE_VERSION="$("$XCODEBUILD_BIN" -version 2>/dev/null | head -1 || true)"
[ -n "$XCODE_VERSION" ] \
    || die "full Xcode is unavailable (install Xcode, then select it with xcode-select)"
ok "Xcode: $XCODE_VERSION"

# Resolve and validate one identity before touching the managed checkout.
_resolve_signing_identity || die "$SIGNING_ERROR"
ok "Team: $TEAM_ID"
if [ "$BUNDLE_ID_DERIVED" = "1" ]; then
    ok "Runner bundle ID: $WDA_BUNDLE_ID (derived for this team)"
else
    ok "Runner bundle ID: $WDA_BUNDLE_ID (explicit or persisted)"
fi
ok "WDA source pin: $WDA_REF_LABEL $WDA_REF"

# ── 1. Resolve device ─────────────────────────────────────────────────────────
info "Resolving target device"
# One target contract across installer, daemon, supervisor, and manual reruns:
# explicit WDA_UDID > explicit PHONE_REMOTE_UDID > daemon plist > existing WDA
# supervisor > safe USB auto-detection.
if [ -z "${WDA_UDID:-}" ] && [ -n "${PHONE_REMOTE_UDID:-}" ]; then
    WDA_UDID="$PHONE_REMOTE_UDID"
fi
if [ -z "${WDA_UDID:-}" ] && [ -f "$DAEMON_PLIST" ]; then
    WDA_UDID="$(/usr/libexec/PlistBuddy \
        -c "Print :EnvironmentVariables:PHONE_REMOTE_UDID" "$DAEMON_PLIST" \
        2>/dev/null || true)"
fi
if [ -z "${WDA_UDID:-}" ] && [ -f "$WDA_AGENT_PLIST" ]; then
    WDA_UDID="$(/usr/libexec/PlistBuddy \
        -c "Print :EnvironmentVariables:WDA_UDID" "$WDA_AGENT_PLIST" \
        2>/dev/null || true)"
fi
if [ -n "${WDA_UDID:-}" ] \
    && ! printf '%s' "$WDA_UDID" | LC_ALL=C grep -Eq '^[0-9A-Fa-f-]+$'; then
    die "target UDID contains invalid characters (expected hex and dashes)"
fi
# Prefer the iPhone physically on USB — with several paired phones, auto-detect
# otherwise grabs the first -showdestinations hit, which is often a dead one.
if [ -z "${WDA_UDID:-}" ]; then
    _USB="$(_usb_udids)"
    if [ "$(printf '%s' "$_USB" | wc -w)" = 1 ]; then
        WDA_UDID="$_USB"; ok "using USB-connected iPhone: $WDA_UDID"
    elif [ -n "$_USB" ]; then
        die "multiple iPhones are connected over USB ($_USB). Set WDA_UDID=<one>; refusing to guess."
    fi
fi
if [ "$WDA_ALLOW_LAN" = "0" ]; then
    if [ -z "${WDA_UDID:-}" ]; then
        _setstatus prereq usb "no USB iPhone is connected"
        die "Direct/WDA defaults to USB, but no USB iPhone was found.
   Plug in and unlock one iPhone, or set WDA_UDID=<USB UDID>; no source was fetched or build started."
    fi
    if ! _target_on_usb; then
        _setstatus prereq usb "the configured iPhone is not connected over USB"
        die "target $WDA_UDID is not currently connected over USB.
   Plug in that iPhone, or set WDA_UDID to the exact USB-connected device; refusing a slow Wi-Fi fallback."
    fi
elif [ -z "${WDA_UDID:-}" ]; then
    warn "WDA_ALLOW_LAN=1: no USB target; paired destinations will be enumerated after the pinned checkout"
fi
_setstatus prereq "" "prerequisites passed"

# ── 2. Clone / update WDA ─────────────────────────────────────────────────────
info "WebDriverAgent checkout"
if [ -e "$WDA_DIR" ] && [ ! -d "$WDA_DIR/.git" ]; then
    die "WDA_DIR exists but is not a Git checkout: $WDA_DIR
   Move it aside or set WDA_DIR to an empty managed path; no files were overwritten."
fi
if [ ! -d "$WDA_DIR/.git" ]; then
    # A no-checkout clone has an index at the remote HEAD but an empty worktree,
    # so `git status` reports every tracked file as deleted. That made a clean
    # first setup fail our dirty-check before it could select the pinned commit.
    # Check out the clone normally; we still fetch and detach at the exact pin
    # below before any project source is built or executed.
    git clone --filter=blob:none "$WDA_REPO" "$WDA_DIR" \
        || die "could not clone the official WebDriverAgent repository"
    WDA_CHECKOUT_CREATED_THIS_RUN=1
fi
WDA_ORIGIN="$(git -C "$WDA_DIR" remote get-url origin 2>/dev/null || true)"
case "$WDA_ORIGIN" in
    https://github.com/appium/WebDriverAgent|https://github.com/appium/WebDriverAgent.git|\
    git@github.com:appium/WebDriverAgent.git|ssh://git@github.com/appium/WebDriverAgent.git)
        ;;
    *)
        die "refusing to fetch the WDA pin from unexpected origin '$WDA_ORIGIN'.
   Expected the official appium/WebDriverAgent repository; use a separate WDA_DIR for custom forks."
        ;;
esac
WDA_CANONICAL_DIR="$(cd -P "$WDA_DIR" 2>/dev/null && pwd)" \
    || die "could not resolve the WDA checkout's canonical path"
case "$WDA_CANONICAL_DIR" in
    *$'\n'*|*$'\r'*) die "WDA_DIR contains unsafe newline characters" ;;
esac
WDA_PREUPDATE_HEAD="$(git -C "$WDA_DIR" rev-parse HEAD 2>/dev/null || true)"
if [ "$WDA_CHECKOUT_CREATED_THIS_RUN" = "1" ]; then
    WDA_MARKER_REFRESH_ALLOWED=1
elif [ -n "$WDA_PREUPDATE_HEAD" ] \
    && _existing_marker_matches_checkout \
        "$WDA_CANONICAL_DIR" "$WDA_ORIGIN" "$WDA_PREUPDATE_HEAD"; then
    WDA_MARKER_REFRESH_ALLOWED=1
else
    warn "This existing checkout has no matching setup ownership marker.
   Setup will use it, but uninstall will preserve it rather than guessing ownership:
   $WDA_CANONICAL_DIR"
fi
# The runner rename applied after the checkout (below) edits the tracked
# project.pbxproj, which the guard immediately after this would refuse to
# overwrite on every later run. Restore that one file when the ONLY thing
# changed in it is our own PRODUCT_NAME swap, so re-runs work while any other
# local edit is still reported and refused.
_restore_runner_rename() {
    _rrn_pbx='WebDriverAgent.xcodeproj/project.pbxproj'
    [ -n "$WDA_RUNNER_NAME" ] || return 0
    [ -n "$(git -C "$WDA_DIR" status --porcelain --untracked-files=no -- "$_rrn_pbx" 2>/dev/null)" ] || return 0
    # Bail out (leave it dirty for the guard to report) if any changed line is
    # something other than the exact swap we make.
    if git -C "$WDA_DIR" diff -U0 -- "$_rrn_pbx" 2>/dev/null \
        | grep -E '^[+-][^+-]' \
        | grep -qvE '^\+[[:space:]]*PRODUCT_NAME = '"$WDA_RUNNER_NAME"';$|^-[[:space:]]*PRODUCT_NAME = "\$\(TARGET_NAME\)";$'; then
        return 0
    fi
    git -C "$WDA_DIR" checkout -- "$_rrn_pbx" 2>/dev/null || true
}
_restore_runner_rename
if ! WDA_TRACKED_CHANGES="$(git -C "$WDA_DIR" status --porcelain --untracked-files=no 2>/dev/null)"; then
    die "could not inspect tracked changes in $WDA_DIR"
fi
if [ -n "$WDA_TRACKED_CHANGES" ]; then
    printf '%s\n' "$WDA_TRACKED_CHANGES" | sed 's/^/    /' >&2
    die "tracked changes exist in $WDA_DIR; refusing to overwrite them.
   Commit, stash, or choose a separate WDA_DIR, then rerun."
fi
git -C "$WDA_DIR" fetch --depth 1 origin "$WDA_REF" \
    || die "could not fetch pinned WDA commit $WDA_REF ($WDA_REF_LABEL)"
FETCHED_WDA_COMMIT="$(git -C "$WDA_DIR" rev-parse --verify "$WDA_REF^{commit}" 2>/dev/null || true)"
[ "$FETCHED_WDA_COMMIT" = "$WDA_REF" ] \
    || die "fetched WDA object did not resolve to the required commit $WDA_REF"
git -C "$WDA_DIR" checkout --detach --quiet "$WDA_REF" \
    || die "could not detach the WDA checkout at $WDA_REF; inspect untracked path conflicts"
WDA_COMMIT="$(git -C "$WDA_DIR" rev-parse HEAD 2>/dev/null || true)"
[ "$WDA_COMMIT" = "$WDA_REF" ] \
    || die "WDA checkout verification failed: expected $WDA_REF, found ${WDA_COMMIT:-nothing}"
WDA_COMMIT_SHORT="$(printf '%.12s' "$WDA_COMMIT")"
ok "Pinned WDA source: $WDA_REF_LABEL $WDA_COMMIT_SHORT at $WDA_DIR"

# Relabel the runner app that lands on the user's home screen (issue #64).
# Xcode synthesises the runner as "<PRODUCT_NAME>-Runner" from its own template
# and ignores the test target's Info.plist, so there is no plist lever —
# hardware-verified: INFOPLIST_KEY_CFBundleDisplayName is NOT injected into the
# generated .xctrunner. Patch ONLY the Runner target's build configurations,
# identified by their INFOPLIST_FILE; passing PRODUCT_NAME= on the xcodebuild
# command line instead would apply to every target and rename WebDriverAgentLib
# — the build breakage warned about at the build step below. The bundle id and
# the xcodebuild argv are untouched, so the runner identity checks still hold.
if [ -n "$WDA_RUNNER_NAME" ]; then
    if WDA_RUNNER_NAME="$WDA_RUNNER_NAME" python3 - "$WDA_DIR/WebDriverAgent.xcodeproj/project.pbxproj" <<'PY'
import os, re, sys

path = sys.argv[1]
name = os.environ["WDA_RUNNER_NAME"]
source = open(path).read()
blocks = re.split(r'(?=\t\t[0-9A-F]{24} /\* (?:Debug|Release) \*/ = \{)', source)
patched = 0
for index, block in enumerate(blocks):
    if ('INFOPLIST_FILE = WebDriverAgentRunner/Info.plist;' in block
            and 'PRODUCT_NAME = "$(TARGET_NAME)";' in block):
        blocks[index] = block.replace(
            'PRODUCT_NAME = "$(TARGET_NAME)";', 'PRODUCT_NAME = %s;' % name)
        patched += 1
if patched:
    open(path, 'w').write(''.join(blocks))
PY
    then
        ok "Runner app installs as ${WDA_RUNNER_NAME}-Runner (set WDA_RUNNER_NAME= to keep upstream's name)"
    else
        die "could not relabel the WDA runner target in project.pbxproj"
    fi
fi

_runner_icon_fail() {
    local reason="$1"
    local recovery=""
    if [ "${WDA_ICON_MUTATION_ACTIVE:-0}" = "1" ]; then
        if _restore_wda_icon_app; then
            recovery="; restored the pristine signed runner"
        elif [ "${WDA_ICON_MUTATION_ACTIVE:-0}" = "1" ]; then
            recovery="; automatic restore failed and the recovery backup was retained at $WDA_ICON_BACKUP_PATH"
        else
            recovery="; removed the invalid build product so xcodebuild can rebuild it"
        fi
    fi
    if [ "${WDA_ICON_MUTATION_ACTIVE:-0}" != "1" ]; then
        _cleanup_wda_icon_work_dir
    fi
    warn "Runner icon skipped: ${reason}${recovery}. WDA setup will continue without a custom icon."
    return 1
}

# `xcodebuild test` regenerates the runner's Info.plist from the XCTRunner
# template on every run that considers the product stale — it wipes the
# injected icon keys AND does not re-sign, so installd rejects the bundle with
# 0xe8008001 (hardware-verified: Info.plist was 180s newer than the signature).
# `test-without-building` installs the already-built product as-is, so the
# injection survives. Only used when an injection actually happened; without
# one the original `test` argv is kept untouched.
_resolve_xctestrun() {
    local products_dir="$1" parent match count
    [ -n "$products_dir" ] || return 1
    parent="$(dirname "$products_dir")"
    [ -d "$parent" ] || return 1
    count="$(find "$parent" -maxdepth 1 -name '*.xctestrun' 2>/dev/null | wc -l | tr -d ' ')"
    [ "$count" = "1" ] || return 1
    match="$(find "$parent" -maxdepth 1 -name '*.xctestrun' 2>/dev/null | head -1)"
    # The path rides in the PID-identity signature, which is space-delimited.
    case "$match" in
        *[[:space:]]*) return 1 ;;
    esac
    printf '%s\n' "$match"
}

# BEGIN runner product validation.
_validate_runner_bundle() {
    local app="$1" detail
    if ! detail="$(python3 - "$app" <<'PY_RUNNER'
import os
from pathlib import Path
import plistlib
import sys

app = Path(sys.argv[1])
def fail(message):
    print(message)
    raise SystemExit(1)

if app.is_symlink() or not app.is_dir():
    fail("runner is missing or symlinked")
for directory, dirs, files in os.walk(app, followlinks=False):
    for name in dirs + files:
        if name.endswith('.cstemp'):
            fail("signing temporary file found (possible interrupted signing): "
                 + str((Path(directory) / name).relative_to(app)))
tests = list((app / 'PlugIns').glob('*.xctest'))
if not tests:
    fail("runner contains no PlugIns/*.xctest bundle")
bundles = [app]
for directory, dirs, _files in os.walk(app, followlinks=False):
    for name in dirs:
        if name.endswith(('.framework', '.xctest')):
            bundles.append(Path(directory) / name)
for bundle in bundles:
    relative = str(bundle.relative_to(app))
    info_path = bundle / 'Info.plist'
    if not info_path.is_file():
        fail(relative + '/Info.plist is missing')
    try:
        with info_path.open('rb') as stream:
            info = plistlib.load(stream)
    except (OSError, ValueError, plistlib.InvalidFileException):
        fail(relative + '/Info.plist is invalid')
    executable = info.get('CFBundleExecutable') if isinstance(info, dict) else None
    if (not isinstance(executable, str) or not executable or '/' in executable
            or executable in {'.', '..'}):
        fail(relative + ': invalid CFBundleExecutable')
    binary = bundle / executable
    if not binary.is_file() or not os.access(binary, os.X_OK):
        fail(relative + '/' + executable + ' is missing or not executable')
PY_RUNNER
    )"; then
        WDA_RUNNER_VALIDATION_ERROR="$detail"
        return 1
    fi
    if ! detail="$(codesign --verify --deep --strict "$app" 2>&1)"; then
        WDA_RUNNER_VALIDATION_ERROR="runner signature verification failed: $detail"
        return 1
    fi
    WDA_RUNNER_VALIDATION_ERROR=""
}

_run_runner_prebuild() {
    local build_log="$1"
    _setstatus building "${_BUILD_BLOCKER:-}" "building WDA runner product"
    : > "$build_log"
    if ! (
        cd "$WDA_DIR" || exit 1
        _wda_xcodebuild -project WebDriverAgent.xcodeproj \
            -scheme WebDriverAgentRunner \
            -destination "platform=iOS,id=$WDA_UDID" \
            -allowProvisioningUpdates \
            DEVELOPMENT_TEAM="$TEAM_ID" PRODUCT_BUNDLE_IDENTIFIER="$WDA_BUNDLE_ID" \
            build-for-testing
    ) >>"$build_log" 2>&1; then
        if grep -Eiq 'Unlock iPhone to Continue|device is locked|deviceprep.*Code=-3|Code=-3.*deviceprep' "$build_log"; then
            WDA_ICON_BUILD_LOCKED=1
        fi
        WDA_RUNNER_VALIDATION_ERROR="build-for-testing failed (log: $build_log)"
        return 1
    fi
}

_repair_runner_if_invalid() {
    local products="$1" app="$2" build_log="$3" reason
    if _validate_runner_bundle "$app"; then
        return 0
    fi
    reason="$WDA_RUNNER_VALIDATION_ERROR"
    warn "Runner product invalid: $reason"
    _setstatus building "${_BUILD_BLOCKER:-}" "runner product invalid: $reason; rebuilding once"
    if [ "${WDA_RUNNER_REPAIR_ATTEMPTED:-0}" = "1" ]; then
        _setstatus building-fail wda "runner still invalid after one repair: $reason"
        return 1
    fi
    # Only the current target's app, at the build-settings-derived products
    # path, may be discarded. Never clean all DerivedData or follow an app link.
    case "$products" in /*/Build/Products/*) ;; *) return 1 ;; esac
    if [ "$app" != "$products/${WDA_RUNNER_NAME:-WebDriverAgentRunner}-Runner.app" ] \
        || [ -L "$app" ]; then
        WDA_RUNNER_VALIDATION_ERROR="refusing to remove an unowned runner product"
        return 1
    fi
    if ! python3 - "$products" "$app" <<'PY_PRODUCT_PATH'
from pathlib import Path
import sys
products, app = map(Path, sys.argv[1:])
if (not products.is_absolute() or '/Build/Products/' not in str(products.resolve())
        or app.is_symlink() or app.parent.resolve() != products.resolve()):
    raise SystemExit(1)
PY_PRODUCT_PATH
    then
        WDA_RUNNER_VALIDATION_ERROR="refusing to remove a runner outside canonical build products"
        return 1
    fi
    WDA_RUNNER_REPAIR_ATTEMPTED=1
    rm -rf -- "$app" || return 1
    # Deleting the exact poisoned app also discards its .cstemp leftovers;
    # merely unlinking those files would leave missing framework contents.
    if ! _run_runner_prebuild "$build_log" || ! _validate_runner_bundle "$app"; then
        _setstatus building-fail wda "runner repair failed: $WDA_RUNNER_VALIDATION_ERROR"
        return 1
    fi
    WDA_RUNNER_ICON_INJECTED=0
    WDA_XCTESTRUN=""
    _setstatus building "${_BUILD_BLOCKER:-}" "runner product rebuilt and verified"
}

_ensure_launchable_runner() {
    local products="${WDA_ICON_PRODUCTS_DIR:-}" settings app build_log
    build_log="$STATE_DIR/wda-runner-product-build.log"
    if [ -z "$products" ]; then
        _setstatus building "${_BUILD_BLOCKER:-}" "resolving runner product before launch"
        settings="$(
            cd "$WDA_DIR" || exit 1
            _wda_xcodebuild -project WebDriverAgent.xcodeproj \
                -scheme WebDriverAgentRunner \
                -destination "platform=iOS,id=$WDA_UDID" \
                -allowProvisioningUpdates \
                DEVELOPMENT_TEAM="$TEAM_ID" PRODUCT_BUNDLE_IDENTIFIER="$WDA_BUNDLE_ID" \
                -showBuildSettings -json 2>>"$build_log"
        )" || { WDA_RUNNER_VALIDATION_ERROR="could not resolve runner build settings"; return 1; }
        products="$(printf '%s' "$settings" | python3 -c '
import json,sys
records=json.load(sys.stdin)
paths={r.get("buildSettings",{}).get("BUILT_PRODUCTS_DIR") for r in records if r.get("target")=="WebDriverAgentRunner"}
paths.discard(None)
if len(paths)!=1: raise SystemExit(1)
print(paths.pop())
')" || { WDA_RUNNER_VALIDATION_ERROR="ambiguous runner products directory"; return 1; }
    fi
    case "$products" in
        /*/Build/Products/*) ;;
        *) WDA_RUNNER_VALIDATION_ERROR="unexpected runner products path"; return 1 ;;
    esac
    app="$products/${WDA_RUNNER_NAME:-WebDriverAgentRunner}-Runner.app"
    if [ ! -e "$app" ] && [ ! -L "$app" ]; then
        _run_runner_prebuild "$build_log" || return 1
    fi
    _repair_runner_if_invalid "$products" "$app" "$build_log"
}
# END runner product validation.

_build_and_inject_runner_icon() {
    local source="$1"
    local extension icon_png iconset assets_catalog compiled partial_plist
    local icon_dimensions has_alpha build_log build_settings products_dir
    local runner_product signing_identity entitlements nested xctest_count

    extension="$(printf '%s' "${source##*.}" | tr '[:upper:]' '[:lower:]')"
    case "$extension" in
        icns)
            command -v iconutil >/dev/null 2>&1 \
                || { _runner_icon_fail "iconutil is unavailable" || true; return 1; }
            ;;
        png) ;;
        *)
            _runner_icon_fail "WDA_RUNNER_ICON must name a .png or .icns file" || true
            return 1
            ;;
    esac
    command -v sips >/dev/null 2>&1 \
        || { _runner_icon_fail "sips is unavailable" || true; return 1; }
    command -v codesign >/dev/null 2>&1 \
        || { _runner_icon_fail "codesign is unavailable" || true; return 1; }
    [ -x /usr/bin/ditto ] \
        || { _runner_icon_fail "/usr/bin/ditto is unavailable" || true; return 1; }
    xcrun --find actool >/dev/null 2>&1 \
        || { _runner_icon_fail "Xcode actool is unavailable" || true; return 1; }

    WDA_ICON_WORK_DIR="$(mktemp -d "$STATE_DIR/wda-runner-icon.XXXXXX")" \
        || { _runner_icon_fail "could not create a private icon work directory" || true; return 1; }
    icon_png="$WDA_ICON_WORK_DIR/icon-1024.png"
    assets_catalog="$WDA_ICON_WORK_DIR/Assets.xcassets"
    compiled="$WDA_ICON_WORK_DIR/compiled"
    partial_plist="$WDA_ICON_WORK_DIR/partial.plist"
    mkdir -p "$assets_catalog/AppIcon.appiconset" "$compiled" \
        || { _runner_icon_fail "could not prepare the asset catalog" || true; return 1; }

    if [ "$extension" = "icns" ]; then
        iconset="$WDA_ICON_WORK_DIR/source.iconset"
        if ! iconutil -c iconset "$source" -o "$iconset" \
            >"$WDA_ICON_WORK_DIR/iconutil.log" 2>&1 \
            || [ ! -f "$iconset/icon_512x512@2x.png" ]; then
            _runner_icon_fail "the ICNS has no usable 1024px representation" || true
            return 1
        fi
        cp "$iconset/icon_512x512@2x.png" "$icon_png" \
            || { _runner_icon_fail "could not stage the ICNS image" || true; return 1; }
    elif ! sips -s format png -z 1024 1024 "$source" --out "$icon_png" \
        >"$WDA_ICON_WORK_DIR/sips.log" 2>&1; then
        _runner_icon_fail "the PNG could not be converted to 1024x1024" || true
        return 1
    fi

    icon_dimensions="$(sips -g pixelWidth -g pixelHeight "$icon_png" 2>/dev/null \
        | awk '/pixelWidth:/ { width=$2 } /pixelHeight:/ { height=$2 } END { printf "%sx%s", width, height }')"
    if [ "$icon_dimensions" != "1024x1024" ]; then
        _runner_icon_fail "the staged icon is $icon_dimensions instead of 1024x1024" || true
        return 1
    fi
    has_alpha="$(sips -g hasAlpha "$icon_png" 2>/dev/null \
        | awk '/hasAlpha:/ { print tolower($2); exit }')"
    case "$has_alpha" in
        yes|true)
            # iOS rejects primary app icons with alpha. `actool` may still
            # compile them, so flatten first instead of discovering this only
            # after installation.
            if ! sips --setProperty hasAlpha false "$icon_png" \
                >>"$WDA_ICON_WORK_DIR/sips.log" 2>&1; then
                _runner_icon_fail "the icon alpha channel could not be flattened" || true
                return 1
            fi
            ;;
    esac

    cat > "$assets_catalog/AppIcon.appiconset/Contents.json" <<'JSON'
{"images":[{"filename":"icon-1024.png","idiom":"universal","platform":"ios","size":"1024x1024"}],"info":{"author":"xcode","version":1}}
JSON
    cp "$icon_png" "$assets_catalog/AppIcon.appiconset/icon-1024.png" \
        || { _runner_icon_fail "could not populate the asset catalog" || true; return 1; }
    if ! xcrun actool --compile "$compiled" --app-icon AppIcon \
        --minimum-deployment-target 13.0 --platform iphoneos \
        --target-device iphone --output-partial-info-plist "$partial_plist" \
        "$assets_catalog" >"$WDA_ICON_WORK_DIR/actool.log" 2>&1; then
        _runner_icon_fail "actool could not compile the runner icon" || true
        return 1
    fi
    for nested in Assets.car AppIcon60x60@2x.png AppIcon76x76@2x~ipad.png; do
        if [ ! -f "$compiled/$nested" ]; then
            _runner_icon_fail "actool did not produce $nested" || true
            return 1
        fi
    done
    if [ ! -s "$partial_plist" ]; then
        _runner_icon_fail "actool did not produce its partial Info.plist" || true
        return 1
    fi

    # `xcodebuild ... test` builds, installs, and launches in one action, so
    # there is otherwise no safe point to edit the synthesised .xctrunner app.
    # A hardware-verified build-for-testing pass creates it first; the runner is
    # then launched with `test-without-building -xctestrun`, which installs the
    # already-built product as-is. The plain `test` action must NOT be used
    # here: it re-emplaces the runner's Info.plist from the XCTRunner template
    # without re-signing, which both drops the icon keys and breaks the seal
    # (hardware-verified: installd rejects it with 0xe8008001).
    build_log="$STATE_DIR/wda-runner-icon-build.log"
    info "Prebuilding WDA so the runner icon can be injected before installation"
    _setstatus building "${_BUILD_BLOCKER:-}" "prebuilding WDA for runner icon injection"
    if ! (
        cd "$WDA_DIR" || exit 1
        _wda_xcodebuild -project WebDriverAgent.xcodeproj \
            -scheme WebDriverAgentRunner \
            -destination "platform=iOS,id=$WDA_UDID" \
            -allowProvisioningUpdates \
            DEVELOPMENT_TEAM="$TEAM_ID" \
            PRODUCT_BUNDLE_IDENTIFIER="$WDA_BUNDLE_ID" \
            build-for-testing
    ) >"$build_log" 2>&1; then
        if grep -Eiq 'Unlock iPhone to Continue|device is locked|deviceprep.*Code=-3|Code=-3.*deviceprep' \
            "$build_log" 2>/dev/null; then
            # Do not immediately run the guarded `test` action and prompt a
            # second time in the same cycle. The caller records lock backoff.
            WDA_ICON_BUILD_LOCKED=1
            _cleanup_wda_icon_work_dir
            return 1
        fi
        _runner_icon_fail "build-for-testing failed (log: $build_log)" || true
        return 1
    fi

    build_settings="$WDA_ICON_WORK_DIR/build-settings.json"
    _setstatus building "${_BUILD_BLOCKER:-}" "resolving WDA build settings"
    if ! (
        cd "$WDA_DIR" || exit 1
        _wda_xcodebuild -project WebDriverAgent.xcodeproj \
            -scheme WebDriverAgentRunner \
            -destination "platform=iOS,id=$WDA_UDID" \
            -allowProvisioningUpdates \
            DEVELOPMENT_TEAM="$TEAM_ID" \
            PRODUCT_BUNDLE_IDENTIFIER="$WDA_BUNDLE_ID" \
            -showBuildSettings -json
    ) >"$build_settings" 2>>"$build_log"; then
        _runner_icon_fail "could not resolve the built-products directory (log: $build_log)" || true
        return 1
    fi
    products_dir="$(python3 - "$build_settings" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    records = json.load(handle)
paths = {
    record.get("buildSettings", {}).get("BUILT_PRODUCTS_DIR")
    for record in records
    if record.get("target") == "WebDriverAgentRunner"
}
paths.discard(None)
if len(paths) != 1:
    raise SystemExit(1)
print(paths.pop())
PY
    )" || {
        _runner_icon_fail "xcodebuild returned an ambiguous built-products directory" || true
        return 1
    }
    case "$products_dir" in
        /*/Build/Products/*) ;;
        *)
            _runner_icon_fail "xcodebuild returned an unexpected products path" || true
            return 1
            ;;
    esac
    runner_product="${WDA_RUNNER_NAME:-WebDriverAgentRunner}-Runner.app"
    WDA_ICON_PRODUCTS_DIR="$products_dir"
    WDA_ICON_APP_PATH="$products_dir/$runner_product"
    if [ ! -f "$WDA_ICON_APP_PATH/Info.plist" ] \
        || [ ! -d "$WDA_ICON_APP_PATH/PlugIns" ]; then
        _runner_icon_fail "the expected built runner is missing: $runner_product" || true
        return 1
    fi
    if ! _repair_runner_if_invalid "$products_dir" "$WDA_ICON_APP_PATH" "$build_log"; then
        _runner_icon_fail "$WDA_RUNNER_VALIDATION_ERROR" || true
        return 1
    fi
    signing_identity="$(codesign -dvv "$WDA_ICON_APP_PATH" 2>&1 \
        | sed -n 's/^Authority=//p' | head -1 || true)"
    if [ -z "$signing_identity" ]; then
        _runner_icon_fail "the runner signing identity could not be read" || true
        return 1
    fi
    entitlements="$WDA_ICON_WORK_DIR/runner-entitlements.plist"
    if ! codesign -d --entitlements - --xml "$WDA_ICON_APP_PATH" \
        >"$entitlements" 2>>"$build_log" \
        || [ ! -s "$entitlements" ] \
        || ! plutil -lint "$entitlements" >/dev/null 2>&1; then
        _runner_icon_fail "the runner entitlements could not be preserved" || true
        return 1
    fi

    WDA_ICON_BACKUP_PATH="$WDA_ICON_WORK_DIR/original.app"
    if ! /usr/bin/ditto "$WDA_ICON_APP_PATH" "$WDA_ICON_BACKUP_PATH" \
        || ! codesign --verify --deep --strict "$WDA_ICON_BACKUP_PATH" 2>>"$build_log"; then
        _runner_icon_fail "the pristine runner could not be backed up safely" || true
        return 1
    fi
    WDA_ICON_MUTATION_ACTIVE=1

    if ! cp "$compiled/Assets.car" "$compiled/AppIcon60x60@2x.png" \
        "$compiled/AppIcon76x76@2x~ipad.png" "$WDA_ICON_APP_PATH/"; then
        _runner_icon_fail "compiled icon assets could not be copied into the runner" || true
        return 1
    fi
    if ! python3 - "$partial_plist" "$WDA_ICON_APP_PATH/Info.plist" <<'PY'
import os
import plistlib
import sys
import tempfile

partial_path, target_path = sys.argv[1:]
with open(partial_path, "rb") as handle:
    generated = plistlib.load(handle)
with open(target_path, "rb") as handle:
    original_data = handle.read()
info = plistlib.loads(original_data)
info.update(generated)
name = (info.get("CFBundleIcons", {})
        .get("CFBundlePrimaryIcon", {})
        .get("CFBundleIconName"))
if name != "AppIcon":
    raise SystemExit("actool plist is missing the primary CFBundleIconName=AppIcon")
fmt = plistlib.FMT_BINARY if original_data.startswith(b"bplist00") else plistlib.FMT_XML
mode = os.stat(target_path).st_mode
with tempfile.NamedTemporaryFile(
        dir=os.path.dirname(target_path), prefix=".Info.plist.icon.", delete=False) as handle:
    temp_path = handle.name
    plistlib.dump(info, handle, fmt=fmt, sort_keys=False)
os.chmod(temp_path, mode)
os.replace(temp_path, target_path)
PY
    then
        _runner_icon_fail "actool's icon metadata could not be merged into Info.plist" || true
        return 1
    fi

    # Re-sign strictly from the inside out. Signing the app first and then a
    # nested framework/test bundle invalidates the outer resource seal and
    # makes installation fail with 0xe8008001.
    for nested in "$WDA_ICON_APP_PATH"/Frameworks/*.dylib \
        "$WDA_ICON_APP_PATH"/Frameworks/*.framework; do
        [ -e "$nested" ] || continue
        if ! codesign -f -s "$signing_identity" "$nested" >>"$build_log" 2>&1; then
            _runner_icon_fail "a nested runner framework could not be re-signed" || true
            return 1
        fi
    done
    xctest_count=0
    for nested in "$WDA_ICON_APP_PATH"/PlugIns/*.xctest; do
        [ -e "$nested" ] || continue
        xctest_count=$((xctest_count + 1))
        if ! codesign -f -s "$signing_identity" "$nested" >>"$build_log" 2>&1; then
            _runner_icon_fail "the runner xctest bundle could not be re-signed" || true
            return 1
        fi
    done
    if [ "$xctest_count" -eq 0 ]; then
        _runner_icon_fail "the runner contains no xctest bundle to re-sign" || true
        return 1
    fi
    if ! codesign -f -s "$signing_identity" --entitlements "$entitlements" \
        "$WDA_ICON_APP_PATH" >>"$build_log" 2>&1 \
        || ! codesign --verify --deep --strict "$WDA_ICON_APP_PATH" \
            >>"$build_log" 2>&1; then
        _runner_icon_fail "the final runner signature did not verify" || true
        return 1
    fi

    WDA_ICON_MUTATION_ACTIVE=0
    WDA_RUNNER_ICON_INJECTED=1
    _cleanup_wda_icon_work_dir
    ok "Runner icon injected and signature verified (source: $source)"
    return 0
}

if [ -z "${WDA_UDID:-}" ]; then
    # xcodebuild exposes the classic UDID WDA needs. Never use `head -1`: with
    # multiple paired phones, guessing can build/sign/drive the wrong device.
    WDA_DESTINATION_UDIDS="$(cd "$WDA_DIR" \
        && _wda_xcodebuild -project WebDriverAgent.xcodeproj \
            -scheme WebDriverAgentRunner -showdestinations 2>/dev/null \
        | sed -n 's/.*platform:iOS, arch:arm64.*id:\([0-9A-F-]*\),.*/\1/p' \
        | sort -u || true)"
    WDA_DESTINATION_COUNT="$(printf '%s\n' "$WDA_DESTINATION_UDIDS" \
        | awk 'NF { count++ } END { print count + 0 }')"
    case "$WDA_DESTINATION_COUNT" in
        0) die "no iOS device found — pair the iPhone, enable Developer Mode, and rerun" ;;
        1) WDA_UDID="$WDA_DESTINATION_UDIDS" ;;
        *)
            printf '%s\n' "$WDA_DESTINATION_UDIDS" | sed 's/^/    /' >&2
            die "multiple paired iOS destinations are available; set WDA_UDID=<one exact UDID>"
            ;;
    esac
fi
case "$WDA_UDID" in
    ''|*[!0123456789ABCDEFabcdef-]*) die "target UDID contains invalid characters (expected hex and dashes)" ;;
esac
_refresh_legacy_contracts
# Show WHICH phone was picked (auto-detect grabs the first destination; with
# several paired iPhones it can choose an unavailable one — let the user catch it).
PICKED_NAME="$(_devicectl_t 8 device info details --device "$WDA_UDID" \
              | sed -n 's/.*marketingName: *//p' | head -1 || true)"
ok "Device UDID: $WDA_UDID${PICKED_NAME:+  ($PICKED_NAME)}"
IOS_COUNT="$(_devicectl_t 8 list devices | grep -ciE 'iPhone|iPad' || true)"
if [ "${IOS_COUNT:-0}" -gt 1 ]; then
    warn "$IOS_COUNT iOS devices are paired — if the wrong one was picked, re-run with WDA_UDID=<classic-udid> (the 00008…/8-… id)."
fi

# ── 3. Wait for dev services (DDI) ────────────────────────────────────────────
# Pitfall: 'Developer Disk Image is not mounted' usually means the phone is
# LOCKED or just-connected — not an Xcode version problem. Keep it unlocked.
_DDI_BLOCKER="$_BUILD_BLOCKER"
if [ "$WDA_ALLOW_LAN" = "1" ]; then
    # Carry the sticky trust blocker through this early phase too: clearing it
    # here made status flip back to a generic "waiting for developer services"
    # between failed attempts while the phone still needed the same manual
    # trust approval (operator-reported: "who knows what we're waiting for").
    _setstatus ddi-wait "$_BUILD_BLOCKER" "waiting for developer services — unlock and keep the iPhone awake"
    if [ "$KEEPALIVE_LOCK_RETRY" != "1" ]; then
        info "Waiting for developer services (UNLOCK the iPhone and keep it awake)"
    fi
else
    _DDI_BLOCKER="usb"
    _setstatus ddi-wait usb "waiting for developer services — unlock + USB"
    if [ "$KEEPALIVE_LOCK_RETRY" != "1" ]; then
        info "Waiting for developer services (UNLOCK the iPhone, keep it awake, and plug it in via USB)"
    fi
fi
TRIES=0
until _devicectl_t 10 device info details --device "$WDA_UDID" | grep -q "ddiServicesAvailable: true"; do
    TRIES=$((TRIES+1))
    # Keep per-step diagnostics fresh too; the independent 15s heartbeat
    # covers a devicectl call (or another blocking stage) that stalls.
    _setstatus ddi-wait "$_DDI_BLOCKER" "waiting for developer services (attempt $TRIES)"
    if [ $TRIES -gt 45 ]; then
        warn "developer services never became available for $WDA_UDID."
        warn "Most reliable fix: connect this iPhone to the Mac with a USB cable"
        warn "(Wi-Fi-only often sits in 'connecting' and never mounts the disk image),"
        warn "keep it unlocked + awake, then re-run. Devices the Mac currently sees:"
        _devicectl_t 8 list devices | sed 's/^/    /' >&2 || true
        warn "If the wrong phone was picked, re-run with WDA_UDID=<classic-udid>."
        warn "If WARP is connected, verify its effective Excluded routes contain fe80::/10 and fd00::/8."
        warn "Temporarily disconnect WARP only when those Zero Trust Split Tunnel exclusions cannot be added."
        _setstatus ddi-fail ddi "developer services never became available"
        die "developer services not available (docs/wda-setup.html pitfall ①; check WARP/USB)"
    fi
    if [ "${WDA_KEEPALIVE:-0}" = "1" ]; then
        if [ "$TRIES" -eq 1 ] && [ "$KEEPALIVE_LOCK_RETRY" != "1" ]; then
            warn "developer services are not ready; KeepAlive will not repeat this prompt on every poll"
        fi
    elif [ $((TRIES % 8)) -eq 1 ]; then
        if [ "$WDA_ALLOW_LAN" = "1" ]; then
            warn "still waiting — UNLOCK the phone and keep the screen on ..."
        else
            warn "still waiting — UNLOCK the phone, keep the screen on, and plug in USB ..."
        fi
    fi
    sleep 4
done
ok "Developer Disk Image mounted"
_setstatus building "$_BUILD_BLOCKER" "building + launching WDA"

# ── 4. Build + run WDA (stays running; this is the server) ───────────────────
# Pitfall: PRODUCT_NAME must NOT be overridden (it renames WebDriverAgentLib
# and breaks the build) — only PRODUCT_BUNDLE_IDENTIFIER is safe to rebrand.
info "Building + launching WDA on the phone (first build takes a few minutes)"
_stop_managed_process "$RUNNER_PID_FILE" "$LEGACY_RUNNER_EXPECTED" runner \
    || die "the prior runner PID record does not safely identify a process; refusing to kill anything"
: > "$RUN_LOG"
# `/usr/bin/xcodebuild` is a dispatcher. Once it execs the selected developer
# tool, `ps` reports the real path under Xcode.app; recording the dispatcher
# path therefore makes the exact-process identity check reject a legitimate
# runner. Resolve the executable that the process will actually become.
XCODEBUILD_BIN="$(xcrun --find xcodebuild 2>/dev/null || true)"
[ -n "$XCODEBUILD_BIN" ] && [ -x "$XCODEBUILD_BIN" ] \
    || die "could not resolve the selected Xcode's xcodebuild executable"
RUNNER_ICON_SOURCE=""
case "$WDA_RUNNER_ICON" in
    none)
        ok "Runner icon injection disabled (WDA_RUNNER_ICON=none)"
        ;;
    auto)
        RUNNER_ICON_SOURCE="$HOME/Applications/iPhoneUse.app/Contents/Resources/AppIcon.icns"
        if [ ! -f "$RUNNER_ICON_SOURCE" ]; then
            warn "Runner icon source is not installed at $RUNNER_ICON_SOURCE; continuing with WDA's placeholder icon"
            RUNNER_ICON_SOURCE=""
        fi
        ;;
    *)
        RUNNER_ICON_SOURCE="$WDA_RUNNER_ICON"
        if [ ! -f "$RUNNER_ICON_SOURCE" ]; then
            warn "WDA_RUNNER_ICON does not name a readable file: $RUNNER_ICON_SOURCE; continuing with WDA's placeholder icon"
            RUNNER_ICON_SOURCE=""
        elif RUNNER_ICON_DIR="$(cd -P "$(dirname "$RUNNER_ICON_SOURCE")" 2>/dev/null && pwd)"; then
            RUNNER_ICON_SOURCE="$RUNNER_ICON_DIR/$(basename "$RUNNER_ICON_SOURCE")"
            # launchd has no stable working directory. Persist the canonical
            # absolute path so a custom icon survives supervisor restarts.
            WDA_RUNNER_ICON="$RUNNER_ICON_SOURCE"
        else
            warn "WDA_RUNNER_ICON could not be resolved to an absolute path; continuing with WDA's placeholder icon"
            RUNNER_ICON_SOURCE=""
        fi
        ;;
esac
WDA_XCTESTRUN=""
if [ -n "$RUNNER_ICON_SOURCE" ]; then
    if _build_and_inject_runner_icon "$RUNNER_ICON_SOURCE"; then
        WDA_XCTESTRUN="$(_resolve_xctestrun "$WDA_ICON_PRODUCTS_DIR" || true)"
        if [ -z "$WDA_XCTESTRUN" ]; then
            warn "Could not resolve a unique .xctestrun; running the normal test action, which will drop the injected icon"
        fi
    fi
fi
if [ "$WDA_ICON_BUILD_LOCKED" != "1" ] && ! _ensure_launchable_runner; then
    if [ "$WDA_ICON_BUILD_LOCKED" != "1" ]; then
        _setstatus building-fail wda "$WDA_RUNNER_VALIDATION_ERROR"
        die "runner product is not launchable: $WDA_RUNNER_VALIDATION_ERROR"
    fi
fi
if [ "$WDA_ICON_BUILD_LOCKED" = "1" ]; then
    if [ "${WDA_KEEPALIVE:-0}" = "1" ]; then
        _prepare_locked_retry
        exit 1
    fi
    die "the phone is locked and WDA prebuild exited. Unlock it, then rerun setup."
fi
# Keep `RUNNER_COMMAND=` at column 0: the icon regression test isolates the
# selection block by scanning for it, and indenting it swallowed the runner
# launch block into that excerpt.
# `-xctestrun` cannot be combined with `-project`/`-scheme`. Build both launch
# forms and their signing suffix through one argv source, also used by the
# exact PID identity record. No eval or string-based command execution.
_prepare_runner_args || die "could not prepare WDA runner signing arguments"
RUNNER_COMMAND="$XCODEBUILD_BIN $RUNNER_ARGS"
RUNNER_EXPECTED="runner:$RUNNER_COMMAND"
(
    cd "$WDA_DIR" || exit 1
    exec nohup "$XCODEBUILD_BIN" "${RUNNER_ARGV[@]}"
) > "$RUN_LOG" 2>&1 &
RUNNER_PID=$!
if ! _write_pid_record "$RUNNER_PID_FILE" "$RUNNER_PID" "$RUNNER_EXPECTED" runner; then
    die "xcodebuild did not become the exact expected runner process; no unverified PID was signalled.
   Inspect $RUN_LOG and any listener before retrying."
fi
STARTED_RUNNER=1
RUNNER_PID="$VALIDATED_PID"
ok "PID-verified runner $RUNNER_PID (log: $RUN_LOG)"

info "Waiting for ServerURLHere (or a trust error) ..."
PHONE_URL=""
TRIES=0
BUILD_STARTED_AT="$(date +%s)"
while [ -z "$PHONE_URL" ]; do
    TRIES=$((TRIES+1))
    if [ $TRIES -gt 120 ]; then
        _setstatus building-fail wda "WDA did not report its server URL before the startup timeout"
        die "timed out waiting for WDA to start — check $RUN_LOG"
    fi
    # Read actionable xcodebuild failures before checking whether its PID is
    # still alive. Fast failures can exit between polls; validating the process
    # first used to hide the real account/profile error behind a generic
    # "runner exited" message.
    if grep -q "No Accounts:" "$RUN_LOG" 2>/dev/null; then
        _report_missing_xcode_account
    fi
    if grep -q "No profiles for .* were found\|requires a provisioning profile" \
        "$RUN_LOG" 2>/dev/null; then
        _setstatus signing-fail account "Xcode could not create the WDA provisioning profile"
        die "Xcode could not find or create the WDA development provisioning profile.
   In Xcode → Settings → Accounts, refresh the selected team, keep the iPhone
   registered, then rerun. If WARP is connected, its effective Excluded routes
   must contain fe80::/10 and fd00::/8 (otherwise disconnect it temporarily)."
    fi
    if grep -q "not trusted" "$RUN_LOG" 2>/dev/null; then
        _setstatus trust trust "trust the Apple Development cert on the iPhone"
        die "Developer cert not trusted. On the iPhone: 设置 → 通用 → VPN与设备管理 → 信任 'Apple Development: …', then re-run. (pitfall ②)"
    fi
    if ! _validate_pid_record "$RUNNER_PID_FILE" "$LEGACY_RUNNER_EXPECTED" runner; then
        if [ "${WDA_KEEPALIVE:-0}" = "1" ] \
            && _wda_failure_is_lock_related log-only; then
            _prepare_locked_retry
            exit 1
        fi
        if _wda_failure_is_lock_related log-only; then
            _setstatus building-fail wda "phone is locked and the WDA runner exited"
            die "the phone is locked and xcodebuild exited. Unlock it, then rerun setup."
        fi
        _setstatus building-fail wda "WDA runner exited before reporting its server URL"
        die "the PID-verified WDA runner exited before reporting its server URL — check $RUN_LOG"
    fi
    if _wda_failure_is_lock_related log-only \
        && [ -z "$(sed -n 's/.*ServerURLHere->\(http[^<]*\)<-ServerURLHere.*/\1/p' \
            "$RUN_LOG" | head -1)" ]; then
        if [ "${WDA_KEEPALIVE:-0}" = "1" ]; then
            _prepare_locked_retry
            exit 1
        fi
        _interactive_lock_wait_tick \
            || die "the phone remained locked for 5 minutes. Unlock it, then rerun setup."
    elif [ $((TRIES % 10)) -eq 0 ]; then
        BUILD_ELAPSED="$(( $(date +%s) - BUILD_STARTED_AT ))"
        _setstatus building "$_BUILD_BLOCKER" "building + launching WDA (${BUILD_ELAPSED}s elapsed)"
    fi
    PHONE_URL="$(sed -n 's/.*ServerURLHere->\(http[^<]*\)<-ServerURLHere.*/\1/p' "$RUN_LOG" | head -1)"
    [ -z "$PHONE_URL" ] && sleep 3
done
case "$PHONE_URL" in
    http://*) ;;
    *) die "WDA reported an unexpected server URL '$PHONE_URL' (plain http:// expected)" ;;
esac
ok "WDA serving at $PHONE_URL"
if [ "$WDA_RUNNER_ICON_INJECTED" = "1" ]; then
    if [ ! -f "$WDA_ICON_APP_PATH/Assets.car" ] \
        || [ ! -f "$WDA_ICON_APP_PATH/AppIcon60x60@2x.png" ] \
        || [ ! -f "$WDA_ICON_APP_PATH/AppIcon76x76@2x~ipad.png" ] \
        || [ "$(/usr/libexec/PlistBuddy \
            -c 'Print :CFBundleIcons:CFBundlePrimaryIcon:CFBundleIconName' \
            "$WDA_ICON_APP_PATH/Info.plist" 2>/dev/null || true)" != "AppIcon" ]; then
        warn "Xcode rebuilt the runner and replaced its injected icon; WDA is healthy, and the next setup run will inject the icon again"
    fi
fi
_setstatus serving "" "WDA serving — starting relay"

# ── 5. Localhost relay ────────────────────────────────────────────────────────
# Pitfall (macOS 15+/26): the daemon is a background LaunchAgent and macOS
# Local Network privacy silently blocks its LAN egress — so it must reach WDA
# via 127.0.0.1 (exempt). WDA itself has no HTTP authentication, so USB iproxy
# is mandatory by default. A LAN relay is available only behind the explicit,
# security-reducing WDA_ALLOW_LAN=1 escape hatch.
info "Starting localhost relay on 127.0.0.1:$WDA_PORT"
PHONE_HOSTPORT="${PHONE_URL#http://}"; PHONE_HOSTPORT="${PHONE_HOSTPORT%/}"
PHONE_IP="${PHONE_HOSTPORT%%:*}"; PHONE_WDA_PORT="${PHONE_HOSTPORT##*:}"
_valid_port "$PHONE_WDA_PORT" \
    || die "WDA reported an invalid device port in '$PHONE_URL'"
_stop_managed_process "$RELAY_PID_FILE" "$LEGACY_RELAY_EXPECTED" relay \
    || die "the prior control-relay PID record is not safe to stop; refusing to reuse TCP $WDA_PORT"
_assert_port_free "$WDA_PORT" \
    || die "TCP $WDA_PORT must be free before starting the managed control relay"
: > "$STATE_DIR/wda-relay.log"
TARGET_IS_USB=0
_target_on_usb && TARGET_IS_USB=1
if [ "$TARGET_IS_USB" = "1" ] && command -v iproxy >/dev/null 2>&1; then
    IPROXY_BIN="$(command -v iproxy)"
    RELAY_COMMAND="$IPROXY_BIN -s 127.0.0.1 $WDA_PORT:$PHONE_WDA_PORT -u $WDA_UDID"
    RELAY_EXPECTED="relay:$RELAY_COMMAND"
    nohup "$IPROXY_BIN" -s 127.0.0.1 "$WDA_PORT:$PHONE_WDA_PORT" -u "$WDA_UDID" \
        > "$STATE_DIR/wda-relay.log" 2>&1 &
    RELAY_PID=$!
    RELAY_DESC="USB iproxy on 127.0.0.1:$WDA_PORT"
elif [ "$WDA_ALLOW_LAN" = "1" ] && command -v socat >/dev/null; then
    warn "WDA_ALLOW_LAN=1: WDA has no authentication; use only on a trusted, isolated LAN"
    printf '%s\n' "$PHONE_IP" | LC_ALL=C grep -Eq '^[A-Za-z0-9.:%_-]+$' \
        || die "WDA reported a LAN host that is unsafe for socat: '$PHONE_IP'"
    SOCAT_BIN="$(command -v socat)"
    RELAY_COMMAND="$SOCAT_BIN TCP-LISTEN:$WDA_PORT,fork,reuseaddr,bind=127.0.0.1 TCP:$PHONE_IP:$PHONE_WDA_PORT"
    RELAY_EXPECTED="relay:$RELAY_COMMAND"
    nohup "$SOCAT_BIN" "TCP-LISTEN:$WDA_PORT,fork,reuseaddr,bind=127.0.0.1" \
        "TCP:$PHONE_IP:$PHONE_WDA_PORT" > "$STATE_DIR/wda-relay.log" 2>&1 &
    RELAY_PID=$!
    RELAY_DESC="LAN socat on 127.0.0.1:$WDA_PORT to $PHONE_IP:$PHONE_WDA_PORT"
else
    if [ "$WDA_ALLOW_LAN" = "0" ] && [ "$TARGET_IS_USB" != "1" ]; then
        _setstatus serving usb "the configured iPhone disconnected before the control relay started"
    else
        _setstatus serving wda "no permitted control relay tool is available"
    fi
    die "Direct/WDA uses USB iproxy by default. Keep this iPhone connected over USB and install
   libimobiledevice (brew install libimobiledevice), then rerun.
   The on-phone WDA server has no HTTP authentication. A LAN relay is therefore disabled
   unless WDA_ALLOW_LAN=1 is explicitly set for a trusted, isolated network."
fi
if ! _write_pid_record "$RELAY_PID_FILE" "$RELAY_PID" "$RELAY_EXPECTED" relay; then
    die "control relay did not become the exact expected process; no unverified PID was signalled.
   Inspect $STATE_DIR/wda-relay.log and TCP $WDA_PORT before retrying."
fi
STARTED_CONTROL_RELAY=1
RELAY_PID="$VALIDATED_PID"
sleep 1
_verify_loopback_listener "$RELAY_PID_FILE" "$LEGACY_RELAY_EXPECTED" relay "$WDA_PORT" \
    || die "control relay ownership/bind verification failed.
   Expected only PID $RELAY_PID on 127.0.0.1:$WDA_PORT; inspect $STATE_DIR/wda-relay.log"
ok "PID-verified control relay $RELAY_PID: $RELAY_DESC"
curl -fsS -m 5 "http://127.0.0.1:$WDA_PORT/status" >/dev/null \
    || die "relay up but WDA not answering through it — check $STATE_DIR/wda-relay.log"
ok "WDA reachable at http://127.0.0.1:$WDA_PORT"
warn "The Mac relay is loopback-only, but WDA on the iPhone has no HTTP authentication.
   Keep the iPhone on a trusted, isolated network even when the Mac relay uses USB."

# ── 5b. MJPEG relay (live video for agent mode — /agent/mjpeg) ─────────────────
# WDA serves an MJPEG screen stream on the device's :9100, INSIDE the same
# XCUITest session as control — so live video and driving coexist (iPhone
# Mirroring can't run alongside WDA, this can). The product's Direct mode needs
# this stream, so setup does not publish a video URL unless relay ownership and
# an initial stream byte are both verified.
PHONE_MJPEG_PORT=9100
_stop_managed_process "$MJPEG_RELAY_PID_FILE" "$LEGACY_MJPEG_EXPECTED" mjpeg \
    || die "the prior video-relay PID record is not safe to stop; refusing to reuse TCP $MJPEG_PORT"
_assert_port_free "$MJPEG_PORT" \
    || die "TCP $MJPEG_PORT must be free before starting the managed video relay"
: > "$STATE_DIR/wda-mjpeg-relay.log"
if [ "$TARGET_IS_USB" = "1" ] && command -v iproxy >/dev/null 2>&1; then
    MJPEG_RELAY_COMMAND="$IPROXY_BIN -s 127.0.0.1 $MJPEG_PORT:$PHONE_MJPEG_PORT -u $WDA_UDID"
    MJPEG_RELAY_EXPECTED="mjpeg:$MJPEG_RELAY_COMMAND"
    nohup "$IPROXY_BIN" -s 127.0.0.1 "$MJPEG_PORT:$PHONE_MJPEG_PORT" -u "$WDA_UDID" \
        > "$STATE_DIR/wda-mjpeg-relay.log" 2>&1 &
    MJPEG_RELAY_PID=$!
    MJPEG_RELAY_DESC="USB iproxy on 127.0.0.1:$MJPEG_PORT"
elif [ "$WDA_ALLOW_LAN" = "1" ] && command -v socat >/dev/null; then
    MJPEG_RELAY_COMMAND="$SOCAT_BIN TCP-LISTEN:$MJPEG_PORT,fork,reuseaddr,bind=127.0.0.1 TCP:$PHONE_IP:$PHONE_MJPEG_PORT"
    MJPEG_RELAY_EXPECTED="mjpeg:$MJPEG_RELAY_COMMAND"
    nohup "$SOCAT_BIN" "TCP-LISTEN:$MJPEG_PORT,fork,reuseaddr,bind=127.0.0.1" \
        "TCP:$PHONE_IP:$PHONE_MJPEG_PORT" > "$STATE_DIR/wda-mjpeg-relay.log" 2>&1 &
    MJPEG_RELAY_PID=$!
    MJPEG_RELAY_DESC="LAN socat on 127.0.0.1:$MJPEG_PORT to $PHONE_IP:$PHONE_MJPEG_PORT"
else
    if [ "$WDA_ALLOW_LAN" = "0" ] && [ "$TARGET_IS_USB" != "1" ]; then
        _setstatus serving usb "the configured iPhone disconnected before the video relay started"
    else
        _setstatus serving wda "no permitted video relay tool is available"
    fi
    die "Direct video requires the same permitted relay path as control.
   Keep USB iproxy available, or explicitly use WDA_ALLOW_LAN=1 only on a trusted, isolated LAN."
fi
if ! _write_pid_record "$MJPEG_RELAY_PID_FILE" "$MJPEG_RELAY_PID" \
    "$MJPEG_RELAY_EXPECTED" mjpeg; then
    die "video relay did not become the exact expected process; no unverified PID was signalled.
   Inspect $STATE_DIR/wda-mjpeg-relay.log and TCP $MJPEG_PORT before retrying."
fi
STARTED_MJPEG_RELAY=1
MJPEG_RELAY_PID="$VALIDATED_PID"
sleep 1
_verify_loopback_listener "$MJPEG_RELAY_PID_FILE" "$LEGACY_MJPEG_EXPECTED" mjpeg "$MJPEG_PORT" \
    || die "video relay ownership/bind verification failed.
   Expected only PID $MJPEG_RELAY_PID on 127.0.0.1:$MJPEG_PORT; inspect $STATE_DIR/wda-mjpeg-relay.log"
MJPEG_PROBE_FILE="$STATE_DIR/.wda-mjpeg-probe.$$"
curl -fsS -m 8 "http://127.0.0.1:$MJPEG_PORT" 2>/dev/null \
    | head -c 1 > "$MJPEG_PROBE_FILE" || true
if [ ! -s "$MJPEG_PROBE_FILE" ]; then
    rm -f "$MJPEG_PROBE_FILE"
    die "video relay owns 127.0.0.1:$MJPEG_PORT but no MJPEG data arrived within 8s.
   The daemon configuration was not changed; inspect $STATE_DIR/wda-mjpeg-relay.log."
fi
rm -f "$MJPEG_PROBE_FILE"
ok "PID-verified video relay $MJPEG_RELAY_PID: $MJPEG_RELAY_DESC"

# ── 6. Point the daemon at the verified direct endpoints ───────────────────────
TARGET_URL="http://127.0.0.1:$WDA_PORT"
TARGET_MJPEG_URL="http://127.0.0.1:$MJPEG_PORT"
DAEMON_JOB_LOADED=0
DAEMON_HTTP_READY=0
DAEMON_PORT=44321

if [ -f "$DAEMON_PLIST" ]; then
    info "Configuring the iphone-use daemon for the direct backend"
    DAEMON_WAS_DISABLED="$(_job_disabled_state "$DAEMON_LABEL")" \
        || die "could not snapshot the daemon's launchd disabled policy"
    cp -p "$DAEMON_PLIST" "$DAEMON_ROLLBACK_PLIST" \
        || die "could not back up the daemon plist before changing its backend"
    if launchctl print "$GUI_DOMAIN/$DAEMON_LABEL" >/dev/null 2>&1; then
        DAEMON_JOB_WAS_LOADED=1
    fi
    DAEMON_TRANSACTION_ACTIVE=1
    DAEMON_STAGED_PLIST="${DAEMON_PLIST}.install.$$"
    cp -p "$DAEMON_PLIST" "$DAEMON_STAGED_PLIST" \
        || die "could not stage the daemon plist for an atomic update"
    chmod 600 "$DAEMON_STAGED_PLIST" \
        || die "could not secure the staged daemon plist"
    /usr/libexec/PlistBuddy -c "Print :EnvironmentVariables" "$DAEMON_STAGED_PLIST" >/dev/null 2>&1 \
        || /usr/libexec/PlistBuddy -c "Add :EnvironmentVariables dict" "$DAEMON_STAGED_PLIST"

    CURRENT_BACKEND="$(/usr/libexec/PlistBuddy -c "Print :EnvironmentVariables:PHONE_REMOTE_BACKEND" "$DAEMON_STAGED_PLIST" 2>/dev/null || true)"
    CURRENT_UDID="$(/usr/libexec/PlistBuddy -c "Print :EnvironmentVariables:PHONE_REMOTE_UDID" "$DAEMON_STAGED_PLIST" 2>/dev/null || true)"
    CURRENT_URL="$(/usr/libexec/PlistBuddy -c "Print :EnvironmentVariables:PHONE_REMOTE_WDA_URL" "$DAEMON_STAGED_PLIST" 2>/dev/null || true)"
    CURRENT_MJPEG_URL="$(/usr/libexec/PlistBuddy -c "Print :EnvironmentVariables:PHONE_REMOTE_WDA_MJPEG_URL" "$DAEMON_STAGED_PLIST" 2>/dev/null || true)"
    CURRENT_MANAGED="$(/usr/libexec/PlistBuddy -c "Print :EnvironmentVariables:PHONE_REMOTE_WDA_MANAGED" "$DAEMON_STAGED_PLIST" 2>/dev/null || true)"
    CURRENT_ALLOW_LAN="$(/usr/libexec/PlistBuddy -c "Print :EnvironmentVariables:WDA_ALLOW_LAN" "$DAEMON_STAGED_PLIST" 2>/dev/null || true)"
    CONFIG_CHANGED=0
    if [ "$CURRENT_BACKEND" != "direct" ] \
        || [ "$CURRENT_UDID" != "$WDA_UDID" ] \
        || [ "$CURRENT_URL" != "$TARGET_URL" ] \
        || [ "$CURRENT_MJPEG_URL" != "$TARGET_MJPEG_URL" ] \
        || [ "$CURRENT_MANAGED" != "true" ] \
        || [ "$CURRENT_ALLOW_LAN" != "$WDA_ALLOW_LAN" ]; then
        _plist_set_env "$DAEMON_STAGED_PLIST" PHONE_REMOTE_BACKEND direct
        _plist_set_env "$DAEMON_STAGED_PLIST" PHONE_REMOTE_UDID "$WDA_UDID"
        _plist_set_env "$DAEMON_STAGED_PLIST" PHONE_REMOTE_WDA_URL "$TARGET_URL"
        _plist_set_env "$DAEMON_STAGED_PLIST" PHONE_REMOTE_WDA_MJPEG_URL "$TARGET_MJPEG_URL"
        _plist_set_env "$DAEMON_STAGED_PLIST" PHONE_REMOTE_WDA_MANAGED true
        _plist_set_env "$DAEMON_STAGED_PLIST" WDA_ALLOW_LAN "$WDA_ALLOW_LAN"
        CONFIG_CHANGED=1
        ok "daemon plist set to managed direct + fixed device + WDA control/video endpoints"
    else
        ok "daemon plist already has the managed direct + fixed device + WDA endpoint configuration"
    fi

    plutil -lint "$DAEMON_STAGED_PLIST" >/dev/null 2>&1 \
        || die "staged daemon LaunchAgent plist is invalid after configuration"
    if [ "$CONFIG_CHANGED" = "1" ]; then
        mv -f "$DAEMON_STAGED_PLIST" "$DAEMON_PLIST" \
            || die "could not atomically install the configured daemon plist"
    else
        rm -f "$DAEMON_STAGED_PLIST"
    fi
    DAEMON_STAGED_PLIST=""

    if [ "$CONFIG_CHANGED" = "1" ] \
        || ! launchctl print "$GUI_DOMAIN/$DAEMON_LABEL" >/dev/null 2>&1; then
        launchctl bootout "$GUI_DOMAIN/$DAEMON_LABEL" 2>/dev/null || true
        _wait_job_gone "$DAEMON_LABEL" \
            || die "daemon LaunchAgent did not finish stopping"
        launchctl enable "$GUI_DOMAIN/$DAEMON_LABEL" 2>/dev/null || true
        if ! launchctl bootstrap "$GUI_DOMAIN" "$DAEMON_PLIST" 2>/dev/null; then
            die "WDA is reachable, but the daemon LaunchAgent could not be bootstrapped"
        fi
    fi
    if launchctl print "$GUI_DOMAIN/$DAEMON_LABEL" >/dev/null 2>&1; then
        DAEMON_JOB_LOADED=1
        ok "daemon LaunchAgent job loaded"
    else
        warn "daemon LaunchAgent loaded state could not be verified"
    fi

    DAEMON_PORT="$(/usr/libexec/PlistBuddy -c "Print :EnvironmentVariables:PHONE_REMOTE_PORT" "$DAEMON_PLIST" 2>/dev/null || true)"
    [ -n "$DAEMON_PORT" ] || DAEMON_PORT=44321
    if [ "$DAEMON_JOB_LOADED" = "1" ]; then
        for _ in 1 2 3 4 5 6 7 8 9 10; do
            if curl -sS -m 2 -o /dev/null "http://127.0.0.1:$DAEMON_PORT/" 2>/dev/null; then
                DAEMON_HTTP_READY=1
                break
            fi
            sleep 0.5
        done
        if [ "$DAEMON_HTTP_READY" = "1" ]; then
            ok "daemon HTTP endpoint verified on 127.0.0.1:$DAEMON_PORT"
        else
            warn "daemon job is loaded, but its HTTP endpoint is not verified; check its error log"
        fi
    fi
else
    warn "daemon LaunchAgent not found; WDA can be verified, but the product daemon cannot be started"
    printf '    PHONE_REMOTE_BACKEND=direct PHONE_REMOTE_WDA_URL=%s PHONE_REMOTE_WDA_MJPEG_URL=%s iphone-use serve\n' \
        "$TARGET_URL" "$TARGET_MJPEG_URL"
fi

# ── 7. Hand the proven runner/relays to the dedicated launchd supervisor ───────
SUPERVISOR_VERIFIED=0
MJPEG_READY=0
if [ "${WDA_KEEPALIVE:-0}" = "1" ]; then
    if launchctl print "$GUI_DOMAIN/$WDA_AGENT_LABEL" >/dev/null 2>&1 \
        && _validate_pid_record "$RUNNER_PID_FILE" "$LEGACY_RUNNER_EXPECTED" runner; then
        RPID="$VALIDATED_PID"
    else
        RPID=""
    fi
    if [ -n "$RPID" ] \
        && _verify_loopback_listener "$RELAY_PID_FILE" "$LEGACY_RELAY_EXPECTED" relay "$WDA_PORT" \
        && _verify_loopback_listener "$MJPEG_RELAY_PID_FILE" "$LEGACY_MJPEG_EXPECTED" mjpeg "$MJPEG_PORT" \
        && curl -fsS -m 4 "$TARGET_URL/status" >/dev/null 2>&1; then
        SUPERVISOR_VERIFIED=1
        MJPEG_READY=1
    fi
else
    _validate_pid_record "$RUNNER_PID_FILE" "$LEGACY_RUNNER_EXPECTED" runner \
        || die "interactive runner identity was lost before launchd handoff"
    HANDOFF_OLD_ID="$PID_RECORD_PID|$PID_RECORD_LSTART"
    _setstatus supervisor "" "handing WDA to its launchd supervisor"
    info "Handing the verified WDA setup to its dedicated launchd supervisor"
    _install_wda_supervisor \
        || die "WDA is reachable now, but its launchd supervisor could not be installed"

    # Bootstrap starts a fresh supervisor-owned setup process. Verify that it
    # replaced the interactive runner (not merely that launchctl accepted XML)
    # and that the replacement still answers /status.
    HANDOFF_TRIES=0
    while [ "$HANDOFF_TRIES" -lt 60 ]; do
        HANDOFF_TRIES=$((HANDOFF_TRIES + 1))
        HANDOFF_NEW_ID=""
        if _validate_pid_record "$RUNNER_PID_FILE" "$LEGACY_RUNNER_EXPECTED" runner; then
            HANDOFF_NEW_ID="$PID_RECORD_PID|$PID_RECORD_LSTART"
        fi
        if [ -n "$HANDOFF_NEW_ID" ] \
            && [ "$HANDOFF_NEW_ID" != "$HANDOFF_OLD_ID" ] \
            && [ -n "$PID_RECORD_LSTART" ] \
            && launchctl print "$GUI_DOMAIN/$WDA_AGENT_LABEL" >/dev/null 2>&1 \
            && _verify_loopback_listener "$RELAY_PID_FILE" "$LEGACY_RELAY_EXPECTED" relay "$WDA_PORT" \
            && _verify_loopback_listener "$MJPEG_RELAY_PID_FILE" "$LEGACY_MJPEG_EXPECTED" mjpeg "$MJPEG_PORT" \
            && grep -q '"phase":"ready"' "$STATUS_FILE" 2>/dev/null \
            && curl -fsS -m 4 "$TARGET_URL/status" >/dev/null 2>&1; then
            SUPERVISOR_VERIFIED=1
            MJPEG_READY=1
            break
        fi
        sleep 2
    done
    if [ "$SUPERVISOR_VERIFIED" != "1" ]; then
        _setstatus supervisor-fail wda "launchd handoff not verified"
        die "WDA launchd job loaded, but its replacement runner was not verified within 120s.
   Check: $WDA_AGENT_LOG
   Then:  $SELF_INSTALL status"
    fi
    ok "launchd replacement verified: runner identity, both loopback relays, and WDA /status"
fi

if [ "$SUPERVISOR_VERIFIED" != "1" ] || [ "$MJPEG_READY" != "1" ]; then
    _setstatus supervisor-fail wda "WDA is reachable but launchd supervision is unverified"
    die "WDA endpoint is up, but dedicated launchd supervision could not be verified"
fi

# Post-handoff verdict from one `/agent/status` body. Prints exactly one word:
#   drivable   — the phone can act right now (setup and runtime both good)
#   reachable  — the daemon reaches WDA through the relays; the phone cannot
#                act yet (locked, automation pending, or the probe is running)
#   down       — the daemon does not see WDA at all (real handoff failure)
# `"wda"` is matched with its opening quote so `managed_wda` / `wda_actionable`
# never satisfy it.
_daemon_product_verdict() {
    local status="${1:-}"
    if printf '%s' "$status" | grep -Eq '"drivable"[[:space:]]*:[[:space:]]*true'; then
        printf 'drivable\n'
    elif printf '%s' "$status" | grep -Eq '"wda"[[:space:]]*:[[:space:]]*true'; then
        printf 'reachable\n'
    else
        printf 'down\n'
    fi
}

# Whether the daemon read the device as locked (`wda_locked:true`). `null`
# (unknown) and `false` both return 1.
_daemon_status_reports_locked() {
    printf '%s' "${1:-}" | grep -Eq '"wda_locked"[[:space:]]*:[[:space:]]*true'
}

# Polling budget for the verdict: 0.5s per try. The daemon probes WDA every
# ~2s with a 20s action timeout, so `wda:true` can take a couple of probe
# rounds to surface even when the relay came up instantly.
DAEMON_STATUS_MAX_TRIES="${DAEMON_STATUS_MAX_TRIES:-120}"
# Once WDA is reachable, keep waiting this many tries for drivable before
# accepting the handoff as-is (an unlocked phone usually clears within it).
DAEMON_REACHABLE_GRACE_TRIES="${DAEMON_REACHABLE_GRACE_TRIES:-20}"

# The daemon intentionally refreshes Direct health in the background: the first
# /agent/status after a cold start can return its conservative cached `offline`
# value while also starting the real WDA actionability probe. Do not announce a
# successful product setup during that window. Poll the authenticated product
# status until callers can actually use it, and fail closed if the cache never
# becomes actionable.
DAEMON_PRODUCT_READY=0
if [ "$DAEMON_HTTP_READY" = "1" ]; then
    DAEMON_AGENT_SECRET="$(/usr/libexec/PlistBuddy -c "Print :EnvironmentVariables:PHONE_REMOTE_AGENT_TOKEN" "$DAEMON_PLIST" 2>/dev/null || true)"
    if [ -z "$DAEMON_AGENT_SECRET" ]; then
        DAEMON_AGENT_SECRET="$(/usr/libexec/PlistBuddy -c "Print :EnvironmentVariables:PHONE_REMOTE_PASSWORD" "$DAEMON_PLIST" 2>/dev/null || true)"
    fi
    # Two different questions hide behind that status, and only the first is a
    # setup verdict:
    #   1. did the handoff work — does the daemon reach WDA through the relays
    #      (`wda:true`)?
    #   2. can the phone act right now — unlocked, automation mode granted
    #      (`drivable:true`)?
    # The daemon keeps `reconnecting=true` (hence drivable=false) until its own
    # action-level probe succeeds, which a locked phone defers indefinitely.
    # Gating on drivable therefore failed every daemon-initiated rebuild while
    # the phone sat locked, and the rollback below killed a perfectly healthy
    # WDA each time — 62 KeepAlive failures in one evening (2026-09-05).
    # A reachable-but-locked phone is a runtime hint for the daemon and the web
    # client, never a reason to tear the runner down.
    DAEMON_STATUS_TRIES=0
    DAEMON_PRODUCT_VERDICT=down
    DAEMON_REACHABLE_TRIES=0
    while [ "$DAEMON_STATUS_TRIES" -lt "$DAEMON_STATUS_MAX_TRIES" ]; do
        DAEMON_STATUS_TRIES=$((DAEMON_STATUS_TRIES + 1))
        if [ -n "$DAEMON_AGENT_SECRET" ]; then
            DAEMON_STATUS="$(curl -sS -m 2 -H "Authorization: Bearer $DAEMON_AGENT_SECRET" \
                "http://127.0.0.1:$DAEMON_PORT/agent/status" 2>/dev/null || true)"
        else
            DAEMON_STATUS="$(curl -sS -m 2 \
                "http://127.0.0.1:$DAEMON_PORT/agent/status" 2>/dev/null || true)"
        fi
        DAEMON_PRODUCT_VERDICT="$(_daemon_product_verdict "$DAEMON_STATUS")"
        if [ "$DAEMON_PRODUCT_VERDICT" = "drivable" ]; then
            DAEMON_PRODUCT_READY=1
            break
        fi
        if [ "$DAEMON_PRODUCT_VERDICT" = "reachable" ]; then
            # Give an unlocked phone a moment to finish the action probe, then
            # accept the handoff as-is instead of waiting out the whole budget.
            DAEMON_REACHABLE_TRIES=$((DAEMON_REACHABLE_TRIES + 1))
            if [ "$DAEMON_REACHABLE_TRIES" -ge "$DAEMON_REACHABLE_GRACE_TRIES" ]; then
                DAEMON_PRODUCT_READY=1
                break
            fi
        fi
        sleep 0.5
    done
    DAEMON_LOCKED_HINT=0
    if _daemon_status_reports_locked "$DAEMON_STATUS"; then
        DAEMON_LOCKED_HINT=1
    fi
    DAEMON_STATUS=""
    DAEMON_AGENT_SECRET=""
    if [ "$DAEMON_PRODUCT_READY" != "1" ]; then
        _setstatus daemon-fail wda "daemon never reached WDA after verified WDA handoff"
        die "WDA, relays, and launchd supervision are verified, but the daemon did not report wda=true within $((DAEMON_STATUS_MAX_TRIES / 2))s.
   Inspect: ~/Library/Logs/iPhoneUse/iphone-use.err"
    fi
    if [ "$DAEMON_PRODUCT_VERDICT" = "drivable" ]; then
        ok "daemon product status verified: drivable=true"
    elif [ "$DAEMON_LOCKED_HINT" = "1" ]; then
        ok "daemon product status verified: WDA reachable through the relays"
        warn "the iPhone is locked — unlock it once; the daemon keeps probing and reports drivable=true as soon as WDA can act"
    else
        ok "daemon product status verified: WDA reachable through the relays"
        warn "WDA answers but cannot act yet (drivable=false) — keep the iPhone unlocked and awake; the daemon keeps probing"
    fi
fi

if [ "$WDA_MARKER_REFRESH_ALLOWED" = "1" ]; then
    _write_wda_checkout_marker \
        || die "Direct/WDA is healthy, but the checkout ownership marker could not be written atomically.
   Runtime changes are being rolled back; the checkout will be preserved by uninstall."
    ok "WDA checkout ownership marker: $WDA_CHECKOUT_MARKER"
else
    warn "WDA checkout remains unmarked and will be preserved by uninstall: $WDA_CANONICAL_DIR"
fi

SUPERVISOR_HANDOFF_COMPLETE=1
if [ "${WDA_KEEPALIVE:-0}" = "1" ]; then
    _reset_keepalive_retry \
        || warn "could not clear KeepAlive retry state after a verified recovery"
fi
rm -f "$WDA_AGENT_ROLLBACK_PLIST" "$DAEMON_ROLLBACK_PLIST" "$SELF_INSTALL_ROLLBACK"
SUPERVISOR_TRANSACTION_ACTIVE=0
DAEMON_TRANSACTION_ACTIVE=0
SELF_INSTALL_REPLACED_THIS_RUN=0
_setstatus ready "" "direct WDA backend and launchd supervisor verified"
printf '\n%s\n' "${BOLD}━━━ WDA device layer verified ━━━${RST}"
printf '  WDA       : %s (on-phone), %s (verified relay)\n' "$PHONE_URL" "$TARGET_URL"
printf '  Supervisor: %s (job, runner, and /status verified)\n' "$GUI_DOMAIN/$WDA_AGENT_LABEL"
printf '  Video     : %s (startup stream + relay ownership verified)\n' "$TARGET_MJPEG_URL"
if [ "$DAEMON_HTTP_READY" = "1" ]; then
    printf '  Daemon    : http://127.0.0.1:%s (verified)\n' "$DAEMON_PORT"
else
    printf '  Daemon    : not HTTP-verified; inspect ~/Library/Logs/iPhoneUse/iphone-use.err\n'
fi
printf '  Try       : curl -H "Authorization: Bearer %s" http://127.0.0.1:%s/agent/elements\n' \
    "\$PW" "$DAEMON_PORT"
printf '  Stop      : %s stop\n' "$SELF_INSTALL"
printf '  Pause     : %s pause  (give the phone back without auto-restart)\n' "$SELF_INSTALL"
printf '  Resume    : %s resume\n' "$SELF_INSTALL"
printf '  WDA source: %s %s (exact checkout)\n' "$WDA_REF_LABEL" "$WDA_REF"
printf '  Signing   : free Apple ID profiles may expire after 7 days; re-run setup when needed.\n'

# In supervisor mode, stay alive while the runner and verified relay do. launchd
# sees an exit as a failure and rebuilds incrementally after sleep/USB/tunnel
# failures. Interactive setup returned above only after handing off to this job.
if [ "${WDA_KEEPALIVE:-0}" = "1" ]; then
    info "KeepAlive mode: holding while the PID-verified runner and relays stay healthy"
    while :; do
        _validate_pid_record "$RUNNER_PID_FILE" "$LEGACY_RUNNER_EXPECTED" runner || break
        RPID="$VALIDATED_PID"
        _verify_loopback_listener "$RELAY_PID_FILE" "$LEGACY_RELAY_EXPECTED" relay "$WDA_PORT" \
            || break
        _verify_loopback_listener "$MJPEG_RELAY_PID_FILE" "$LEGACY_MJPEG_EXPECTED" mjpeg "$MJPEG_PORT" \
            || break
        curl -fsS -m 4 "$TARGET_URL/status" >/dev/null 2>&1 || break
        sleep 10
    done
    if _wda_failure_is_lock_related; then
        _prepare_locked_retry
        exit 1
    fi
    warn "WDA runner/relay went down — exiting so launchd KeepAlive rebuilds it"
    _stop_managed_process "$MJPEG_RELAY_PID_FILE" "$LEGACY_MJPEG_EXPECTED" mjpeg || true
    _stop_managed_process "$RELAY_PID_FILE" "$LEGACY_RELAY_EXPECTED" relay || true
    _stop_managed_process "$RUNNER_PID_FILE" "$LEGACY_RUNNER_EXPECTED" runner || true
    exit 1
fi
