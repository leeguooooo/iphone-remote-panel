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
#
# Env overrides:
#   WDA_UDID=...        target device UDID (default: first xcodebuild iOS device)
#   WDA_TEAM_ID=...     Apple dev team (default: Xcode's last-selected team)
#   WDA_BUNDLE_ID=...   runner bundle id (default: derived from validated Team ID)
#   WDA_DIR=...         WDA checkout    (default: ~/.iphone-use/WebDriverAgent)
#   WDA_REF=...         exact upstream commit (default: pinned v9.15.3 commit)
#   WDA_PORT=...        relay port      (default: 8100)
#   MJPEG_PORT=...      video relay port (default: 9100)
#   WDA_ALLOW_LAN=1     permit unauthenticated WDA over LAN (unsafe; default off)
#
# Requirements: Xcode (with an Apple ID signed in: Xcode → Settings → Accounts),
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

STATUS_FILE="$STATE_DIR/wda-setup-status.json"
# Structured progress the daemon surfaces via /agent/status.blocked_on, so a
# caller (or POST /agent/mode) knows WHY a setup is stuck instead of polling blind.
# $1=phase  $2=blocked_on(empty=ok)  $3=human message
_setstatus() {
    [ "$COMMAND" = "setup" ] || return 0
    printf '{"phase":"%s","blocked_on":"%s","message":"%s","ts":%s}\n' \
        "$1" "${2:-}" "$(printf '%s' "${3:-}" | sed 's/\\/\\\\/g; s/"/\\"/g')" "$(date +%s)" \
        > "$STATUS_FILE" 2>/dev/null || true
}

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
            '$1 == key && $2 == "=>" && $3 == "true" { found=1 } END { exit !found }'; then
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
        WDA_DIR WDA_REF WDA_PORT MJPEG_PORT WDA_ALLOW_LAN
    do
        case "$key" in
            WDA_KEEPALIVE) value="1" ;;
            PATH) value="/opt/homebrew/bin:/usr/local/bin:/usr/sbin:/sbin:/usr/bin:/bin" ;;
            WDA_UDID) value="$WDA_UDID" ;;
            WDA_TEAM_ID) value="$TEAM_ID" ;;
            WDA_BUNDLE_ID) value="$WDA_BUNDLE_ID" ;;
            WDA_DIR) value="$WDA_DIR" ;;
            WDA_REF) value="$WDA_REF" ;;
            WDA_PORT) value="$WDA_PORT" ;;
            MJPEG_PORT) value="$MJPEG_PORT" ;;
            WDA_ALLOW_LAN) value="${WDA_ALLOW_LAN:-}" ;;
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
    <key>ThrottleInterval</key><integer>30</integer>
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
    [ "$cleanup_failed" = "0" ] || status=1
    trap - EXIT
    exit "$status"
}
trap _cleanup_on_exit EXIT
trap 'exit 130' INT TERM
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

_command_matches_expected() {
    local command="$1"
    local expected="$2"
    local signature rest local_port device_port target_udid team_id bundle_id
    local legacy_argv
    case "$expected" in
        runner:*)
            signature="${expected#*:}"
            printf '%s\n' "$signature" | LC_ALL=C grep -Eq \
                '^(/[^ ]*/)?xcodebuild -project WebDriverAgent\.xcodeproj -scheme WebDriverAgentRunner -destination platform=iOS,id=[0-9A-Fa-f-]+ -allowProvisioningUpdates DEVELOPMENT_TEAM=[A-Z0-9]{10} PRODUCT_BUNDLE_IDENTIFIER=[A-Za-z0-9.-]+ test$' \
                || return 1
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
            legacy_argv="xcodebuild -project WebDriverAgent.xcodeproj -scheme WebDriverAgentRunner -destination platform=iOS,id=$target_udid -allowProvisioningUpdates DEVELOPMENT_TEAM=$team_id PRODUCT_BUNDLE_IDENTIFIER=$bundle_id test"
            case "$command" in
                "$legacy_argv"|*/"$legacy_argv") return 0 ;;
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
    status) cmd_status; exit $? ;;
    doctor) cmd_doctor; exit $? ;;
    setup)  ;;
    *) die "unknown command: $1 (use: setup|status|stop|doctor)" ;;
esac

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

if [ -z "${WDA_UDID:-}" ]; then
    # xcodebuild exposes the classic UDID WDA needs. Never use `head -1`: with
    # multiple paired phones, guessing can build/sign/drive the wrong device.
    WDA_DESTINATION_UDIDS="$(cd "$WDA_DIR" \
        && "$XCODEBUILD_BIN" -project WebDriverAgent.xcodeproj \
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
if [ "$WDA_ALLOW_LAN" = "1" ]; then
    # Carry the sticky trust blocker through this early phase too: clearing it
    # here made status flip back to a generic "waiting for developer services"
    # between failed attempts while the phone still needed the same manual
    # trust approval (operator-reported: "who knows what we're waiting for").
    _setstatus ddi-wait "$_BUILD_BLOCKER" "waiting for developer services — unlock and keep the iPhone awake"
    info "Waiting for developer services (UNLOCK the iPhone and keep it awake)"
else
    _setstatus ddi-wait usb "waiting for developer services — unlock + USB"
    info "Waiting for developer services (UNLOCK the iPhone, keep it awake, and plug it in via USB)"
fi
TRIES=0
until _devicectl_t 10 device info details --device "$WDA_UDID" | grep -q "ddiServicesAvailable: true"; do
    TRIES=$((TRIES+1))
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
    if [ $((TRIES % 8)) -eq 1 ]; then
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
RUNNER_COMMAND="$XCODEBUILD_BIN -project WebDriverAgent.xcodeproj -scheme WebDriverAgentRunner -destination platform=iOS,id=$WDA_UDID -allowProvisioningUpdates DEVELOPMENT_TEAM=$TEAM_ID PRODUCT_BUNDLE_IDENTIFIER=$WDA_BUNDLE_ID test"
RUNNER_EXPECTED="runner:$RUNNER_COMMAND"
(
    cd "$WDA_DIR" || exit 1
    exec nohup "$XCODEBUILD_BIN" -project WebDriverAgent.xcodeproj -scheme WebDriverAgentRunner \
        -destination "platform=iOS,id=$WDA_UDID" \
        -allowProvisioningUpdates \
        DEVELOPMENT_TEAM="$TEAM_ID" \
        PRODUCT_BUNDLE_IDENTIFIER="$WDA_BUNDLE_ID" \
        test
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
        _setstatus signing-fail account "sign in to an Apple account in Xcode"
        die "Xcode has no signed-in Apple account. Open Xcode → Settings → Accounts,
   sign in and select the development team, then rerun."
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
        _setstatus building-fail wda "WDA runner exited before reporting its server URL"
        die "the PID-verified WDA runner exited before reporting its server URL — check $RUN_LOG"
    fi
    if grep -q "device is locked\|Unlock iPhone" "$RUN_LOG" 2>/dev/null; then
        warn "phone locked — unlock it (build is waiting)"
        _setstatus building "$_BUILD_BLOCKER" "phone is locked — unlock it and keep it awake"
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
    DAEMON_STATUS_TRIES=0
    while [ "$DAEMON_STATUS_TRIES" -lt 30 ]; do
        DAEMON_STATUS_TRIES=$((DAEMON_STATUS_TRIES + 1))
        if [ -n "$DAEMON_AGENT_SECRET" ]; then
            DAEMON_STATUS="$(curl -sS -m 2 -H "Authorization: Bearer $DAEMON_AGENT_SECRET" \
                "http://127.0.0.1:$DAEMON_PORT/agent/status" 2>/dev/null || true)"
        else
            DAEMON_STATUS="$(curl -sS -m 2 \
                "http://127.0.0.1:$DAEMON_PORT/agent/status" 2>/dev/null || true)"
        fi
        if printf '%s' "$DAEMON_STATUS" \
            | grep -Eq '"drivable"[[:space:]]*:[[:space:]]*true'; then
            DAEMON_PRODUCT_READY=1
            break
        fi
        sleep 0.5
    done
    DAEMON_STATUS=""
    DAEMON_AGENT_SECRET=""
    if [ "$DAEMON_PRODUCT_READY" != "1" ]; then
        _setstatus daemon-fail wda "daemon never reported drivable after verified WDA handoff"
        die "WDA, relays, and launchd supervision are verified, but the daemon did not report drivable=true within 15s.
   Inspect: ~/Library/Logs/iPhoneUse/iphone-use.err"
    fi
    ok "daemon product status verified: drivable=true"
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
    warn "WDA runner/relay went down — exiting so launchd KeepAlive rebuilds it"
    _stop_managed_process "$MJPEG_RELAY_PID_FILE" "$LEGACY_MJPEG_EXPECTED" mjpeg || true
    _stop_managed_process "$RELAY_PID_FILE" "$LEGACY_RELAY_EXPECTED" relay || true
    _stop_managed_process "$RUNNER_PID_FILE" "$LEGACY_RUNNER_EXPECTED" runner || true
    exit 1
fi
