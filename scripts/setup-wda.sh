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
#   WDA_BUNDLE_ID=...   runner bundle id (default: com.leeguoo.iphone-use.wda)
#   WDA_DIR=...         WDA checkout    (default: ~/.iphone-use/WebDriverAgent)
#   WDA_PORT=...        relay port      (default: 8100)
#
# Requirements: Xcode (with an Apple ID signed in: Xcode → Settings → Accounts),
# the iPhone paired + Developer Mode on, and `socat` or `iproxy` for the relay.
set -eu

WDA_DIR="${WDA_DIR:-$HOME/.iphone-use/WebDriverAgent}"
# When spawned by the daemon (POST /agent/mode) the environment is a bare
# LaunchAgent PATH — Homebrew tools (socat, iproxy) and even xcrun helpers
# live outside it. Extend deterministically rather than relying on the shell.
export PATH="/opt/homebrew/bin:/usr/local/bin:$PATH"

WDA_PORT="${WDA_PORT:-8100}"
WDA_BUNDLE_ID="${WDA_BUNDLE_ID:-com.leeguoo.iphone-use.wda}"
STATE_DIR="$HOME/.iphone-use"
RUN_LOG="$STATE_DIR/wda-runner.log"
RUNNER_PID_FILE="$STATE_DIR/wda-runner.pid"
RELAY_PID_FILE="$STATE_DIR/wda-relay.pid"
WDA_REPO="https://github.com/appium/WebDriverAgent.git"

BOLD=$'\033[1m'; RED=$'\033[0;31m'; GRN=$'\033[0;32m'; YLW=$'\033[1;33m'; RST=$'\033[0m'
info() { printf '%s\n' "${BOLD}== $*${RST}"; }
ok()   { printf '%s\n' "${GRN}✓${RST} $*"; }
warn() { printf '%s\n' "${YLW}⚠${RST}  $*"; }
die()  { printf '%s\n' "${RED}✗ $*${RST}" >&2; exit 1; }

mkdir -p "$STATE_DIR"

# Self-install: keep a copy at a fixed path so the daemon's `POST /agent/mode`
# can start/stop WDA without knowing where the repo lives. Skip when we ARE
# that copy (cp onto itself fails) or when the source isn't readable.
SELF_INSTALL="$STATE_DIR/setup-wda.sh"
if [ "$(cd "$(dirname "$0")" 2>/dev/null && pwd)/$(basename "$0")" != "$SELF_INSTALL" ]; then
    cp -f "$0" "$SELF_INSTALL" 2>/dev/null && chmod +x "$SELF_INSTALL" 2>/dev/null || true
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
    cat "$out"; rm -f "$out"
}

_kill_pidfile() {
    local f="$1"
    if [ -f "$f" ]; then
        local pid; pid="$(cat "$f" 2>/dev/null || true)"
        [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
        rm -f "$f"
    fi
}

cmd_stop() {
    info "Stopping WDA runner + relay"
    _kill_pidfile "$RUNNER_PID_FILE"
    _kill_pidfile "$RELAY_PID_FILE"
    pkill -f "xcodebuild.*WebDriverAgentRunner" 2>/dev/null || true
    ok "stopped"
}

cmd_status() {
    if curl -s -m 4 "http://127.0.0.1:$WDA_PORT/status" >/dev/null 2>&1; then
        ok "WDA reachable at http://127.0.0.1:$WDA_PORT (relay + runner up)"
    else
        warn "nothing answering on 127.0.0.1:$WDA_PORT"
    fi
    pgrep -f "xcodebuild.*WebDriverAgentRunner" >/dev/null \
        && ok "runner process alive" || warn "runner process not running"
}

case "${1:-setup}" in
    stop)   cmd_stop;   exit 0 ;;
    status) cmd_status; exit 0 ;;
    setup)  ;;
    *) die "unknown command: $1 (use: setup|status|stop)" ;;
esac

# ── 0. Prereqs ────────────────────────────────────────────────────────────────
info "Checking prerequisites"
xcode-select -p >/dev/null 2>&1 || die "Xcode not installed (xcode-select -p failed)"
ok "Xcode: $(xcodebuild -version | head -1)"

# Team id: explicit env, else Xcode's last-selected team.
TEAM_ID="${WDA_TEAM_ID:-$(defaults read com.apple.dt.Xcode IDEProvisioningTeamManagerLastSelectedTeamID 2>/dev/null || true)}"
[ -n "$TEAM_ID" ] || die "No dev team found. Sign in: Xcode → Settings → Accounts, or set WDA_TEAM_ID=…"
ok "Team: $TEAM_ID"

# ── 1. Resolve device ─────────────────────────────────────────────────────────
info "Resolving target device"
if [ -z "${WDA_UDID:-}" ]; then
    # Pitfall: xcodebuild needs the classic UDID (00008…), NOT the CoreDevice
    # UUID that `devicectl list devices` shows. -showdestinations prints the
    # right one.
    WDA_UDID="$(cd "$WDA_DIR" 2>/dev/null && xcodebuild -project WebDriverAgent.xcodeproj \
                  -scheme WebDriverAgentRunner -showdestinations 2>/dev/null \
                | sed -n 's/.*platform:iOS, arch:arm64.*id:\([0-9A-F-]*\),.*/\1/p' | head -1 || true)"
fi
if [ -z "${WDA_UDID:-}" ]; then
    # WDA not cloned yet — list devices another way and let the user pick.
    warn "Set WDA_UDID=<udid> if auto-detect picks the wrong phone."
fi

# ── 2. Clone / update WDA ─────────────────────────────────────────────────────
info "WebDriverAgent checkout"
if [ ! -d "$WDA_DIR/.git" ]; then
    git clone --depth 1 "$WDA_REPO" "$WDA_DIR"
fi
WDA_COMMIT="$(git -C "$WDA_DIR" rev-parse --short HEAD)"
ok "WDA at $WDA_DIR ($WDA_COMMIT)"

if [ -z "${WDA_UDID:-}" ]; then
    WDA_UDID="$(cd "$WDA_DIR" && xcodebuild -project WebDriverAgent.xcodeproj \
                  -scheme WebDriverAgentRunner -showdestinations 2>/dev/null \
                | sed -n 's/.*platform:iOS, arch:arm64.*id:\([0-9A-F-]*\),.*/\1/p' | head -1 || true)"
    [ -n "$WDA_UDID" ] || die "no iOS device found — plug in / pair the iPhone, enable Developer Mode"
fi
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
info "Waiting for developer services (UNLOCK the iPhone, keep it awake, and plug it in via USB)"
DEV_UUID="$(_devicectl_t 8 list devices | awk -v u="$WDA_UDID" 'NR>2 {print $(NF-2)}' | head -1 || true)"
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
        die "developer services not available (docs/wda-setup.html pitfall ①)"
    fi
    [ $((TRIES % 8)) -eq 1 ] && warn "still waiting — UNLOCK the phone, keep the screen on, and plug in USB ..."
    sleep 4
done
ok "Developer Disk Image mounted"

# ── 4. Build + run WDA (stays running; this is the server) ───────────────────
# Pitfall: PRODUCT_NAME must NOT be overridden (it renames WebDriverAgentLib
# and breaks the build) — only PRODUCT_BUNDLE_IDENTIFIER is safe to rebrand.
info "Building + launching WDA on the phone (first build takes a few minutes)"
_kill_pidfile "$RUNNER_PID_FILE"
pkill -f "xcodebuild.*WebDriverAgentRunner" 2>/dev/null || true
: > "$RUN_LOG"
(
    cd "$WDA_DIR"
    nohup xcodebuild -project WebDriverAgent.xcodeproj -scheme WebDriverAgentRunner \
        -destination "platform=iOS,id=$WDA_UDID" \
        -allowProvisioningUpdates \
        DEVELOPMENT_TEAM="$TEAM_ID" \
        PRODUCT_BUNDLE_IDENTIFIER="$WDA_BUNDLE_ID" \
        test > "$RUN_LOG" 2>&1 &
    echo $! > "$RUNNER_PID_FILE"
)
ok "runner pid $(cat "$RUNNER_PID_FILE") (log: $RUN_LOG)"

info "Waiting for ServerURLHere (or a trust error) ..."
PHONE_URL=""
TRIES=0
while [ -z "$PHONE_URL" ]; do
    TRIES=$((TRIES+1))
    [ $TRIES -gt 120 ] && die "timed out waiting for WDA to start — check $RUN_LOG"
    if grep -q "not trusted" "$RUN_LOG" 2>/dev/null; then
        die "Developer cert not trusted. On the iPhone: 设置 → 通用 → VPN与设备管理 → 信任 'Apple Development: …', then re-run. (pitfall ②)"
    fi
    if grep -q "device is locked\|Unlock iPhone" "$RUN_LOG" 2>/dev/null; then
        warn "phone locked — unlock it (build is waiting)"
    fi
    PHONE_URL="$(sed -n 's/.*ServerURLHere->\(http[^<]*\)<-ServerURLHere.*/\1/p' "$RUN_LOG" | head -1)"
    [ -z "$PHONE_URL" ] && sleep 3
done
ok "WDA serving at $PHONE_URL"

# ── 5. Localhost relay ────────────────────────────────────────────────────────
# Pitfall (macOS 15+/26): the daemon is a background LaunchAgent and macOS
# Local Network privacy silently blocks its LAN egress — so it must reach WDA
# via 127.0.0.1 (exempt). USB iproxy when usbmuxd sees the device (most
# stable), else socat to the phone's Wi-Fi IP.
info "Starting localhost relay on 127.0.0.1:$WDA_PORT"
_kill_pidfile "$RELAY_PID_FILE"
PHONE_HOSTPORT="${PHONE_URL#http://}"; PHONE_HOSTPORT="${PHONE_HOSTPORT%/}"
PHONE_IP="${PHONE_HOSTPORT%%:*}"; PHONE_WDA_PORT="${PHONE_HOSTPORT##*:}"
if command -v iproxy >/dev/null && idevice_id -l 2>/dev/null | grep -q "$WDA_UDID"; then
    nohup iproxy "$WDA_PORT" "$PHONE_WDA_PORT" -u "$WDA_UDID" > "$STATE_DIR/wda-relay.log" 2>&1 &
    echo $! > "$RELAY_PID_FILE"
    ok "iproxy (USB) relay pid $(cat "$RELAY_PID_FILE")"
elif command -v socat >/dev/null; then
    nohup socat "TCP-LISTEN:$WDA_PORT,fork,reuseaddr,bind=127.0.0.1" \
        "TCP:$PHONE_IP:$PHONE_WDA_PORT" > "$STATE_DIR/wda-relay.log" 2>&1 &
    echo $! > "$RELAY_PID_FILE"
    ok "socat relay pid $(cat "$RELAY_PID_FILE") → $PHONE_IP:$PHONE_WDA_PORT"
else
    die "need iproxy (brew install libimobiledevice) or socat (brew install socat) for the localhost relay"
fi
sleep 1
curl -s -m 5 "http://127.0.0.1:$WDA_PORT/status" >/dev/null \
    || die "relay up but WDA not answering through it — check $STATE_DIR/wda-relay.log"
ok "WDA reachable at http://127.0.0.1:$WDA_PORT"

# ── 6. Point the daemon at it ─────────────────────────────────────────────────
PLIST="$HOME/Library/LaunchAgents/com.leeguoo.iphone-use.plist"
if [ -f "$PLIST" ]; then
    info "Configuring the iphone-use daemon"
    TARGET_URL="http://127.0.0.1:$WDA_PORT"
    CURRENT_URL="$(/usr/libexec/PlistBuddy -c "Print :EnvironmentVariables:PHONE_REMOTE_WDA_URL" "$PLIST" 2>/dev/null || true)"
    if [ "$CURRENT_URL" = "$TARGET_URL" ]; then
        # Idempotent skip — CRITICAL when the daemon itself spawned this script
        # (POST /agent/mode {"mode":"agent"}): `launchctl bootout` kills the
        # daemon's entire cgroup, which includes THIS script and the runner +
        # relay it just started (hardware-verified failure). No value change →
        # no restart needed.
        ok "daemon already configured (PHONE_REMOTE_WDA_URL=$TARGET_URL) — no restart needed"
    else
        /usr/libexec/PlistBuddy -c "Delete :EnvironmentVariables:PHONE_REMOTE_WDA_URL" "$PLIST" 2>/dev/null || true
        /usr/libexec/PlistBuddy -c "Add :EnvironmentVariables:PHONE_REMOTE_WDA_URL string $TARGET_URL" "$PLIST"
        UID_NUM="$(id -u)"
        launchctl bootout "gui/$UID_NUM/com.leeguoo.iphone-use" 2>/dev/null || true
        launchctl bootstrap "gui/$UID_NUM" "$PLIST" 2>/dev/null || true
        sleep 2
        ok "daemon restarted with PHONE_REMOTE_WDA_URL=$TARGET_URL"
    fi
else
    warn "daemon LaunchAgent not found — run the daemon with:"
    printf '    PHONE_REMOTE_WDA_URL=http://127.0.0.1:%s ... iphone-use serve\n' "$WDA_PORT"
fi

printf '\n%s\n' "${BOLD}━━━ L2 element layer ready ━━━${RST}"
printf '  WDA       : %s (on-phone), http://127.0.0.1:%s (relay)\n' "$PHONE_URL" "$WDA_PORT"
printf '  Try       : curl -H "Authorization: Bearer \$PW" http://127.0.0.1:44321/agent/elements\n'
printf '  Tap       : {"type":"tap","label":"<element label>"}\n'
printf '  CJK text  : {"type":"text","text":"你好"} — lands clean via WDA\n'
printf '  Stop      : ./scripts/setup-wda.sh stop\n'
printf '  Note      : free Apple ID signing expires in 7 days — re-run this script weekly.\n'
