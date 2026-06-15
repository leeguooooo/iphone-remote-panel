#!/usr/bin/env bash
# install.sh — install iPhoneUse.app and register the GUI-session LaunchAgent
#
# USAGE (recommended — no auth required; GITHUB_TOKEN not needed for public releases):
#   curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh
#
# Local / dev usage (skip download; supply a pre-built .app):
#   ./install.sh /path/to/iPhoneUse.app
#
# Requirements:
#   • Must run as the LOGGED-IN GUI user (not root, not over a bare SSH session).
#   • Aqua (WindowServer) session must be active.
#   • macOS 14 Sonoma or later.
#
set -eu

# ── Inline sign helper (fallback when scripts/sign.sh is not available) ───────
# Defined early so it is available throughout the script.
# Ad-hoc sign locally. Why ad-hoc (`codesign --sign -`) over a self-signed cert:
# a self-signed cert lives in the login keychain, and codesign then pops a
# keychain-password dialog to use its private key — which blocks any unattended
# install (and is friction even for a present user). Ad-hoc signing needs no
# keychain, no cert, and no prompt: the signature is keyed on the binary's
# cdhash + the Info.plist bundle id, which is a stable identity for a given build
# — so TCC grants (Screen Recording / Accessibility) persist until you UPDATE to
# a new release (then re-grant once). For zero-friction persistent grants across
# updates, ship a Developer-ID-signed + notarized release from CI instead.
_inline_sign() {
    local app="$1"
    codesign --force --sign - "$app/Contents/MacOS/iphone-use"
    codesign --force --sign - "$app"
    ok "Ad-hoc signed (no keychain prompt; re-grant TCC after an update)"
}

# ── Configuration ─────────────────────────────────────────────────────────────
BUNDLE_ID="com.leeguoo.iphone-use"
APP_NAME="iPhoneUse.app"
INSTALL_DIR="$HOME/Applications"
PLIST_LABEL="com.leeguoo.iphone-use"
PLIST_DST="$HOME/Library/LaunchAgents/${PLIST_LABEL}.plist"
LOG_DIR="$HOME/Library/Logs/iPhoneUse"
REPO="leeguooooo/iphone-use"
BINARY_INSIDE_APP="Contents/MacOS/iphone-use"

# ── Colours ───────────────────────────────────────────────────────────────────
RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
BOLD='\033[1m'; RESET='\033[0m'
ok()   { printf "${GREEN}✓${RESET} %s\n" "$*"; }
warn() { printf "${YELLOW}⚠${RESET}  %s\n" "$*"; }
die()  { printf "${RED}✗ ERROR:${RESET} %s\n" "$*" >&2; exit 1; }
info() { printf "  %s\n" "$*"; }

echo ""
printf "${BOLD}=== iphone-use — install.sh ===${RESET}\n"
echo ""

# ── Guard: must be a GUI session ─────────────────────────────────────────────
if [ -z "${HOME:-}" ]; then
    die "HOME is not set.  Run as a normal user, not root."
fi

UID_NUM="$(id -u)"
if [ "$UID_NUM" = "0" ]; then
    die "Do not run as root.  Run as the logged-in desktop user."
fi

if ! launchctl print "gui/$UID_NUM" >/dev/null 2>&1; then
    warn "Could not enumerate gui/$UID_NUM session."
    warn "Make sure you are logged in to a desktop (Aqua) session."
    warn "Running over SSH is fine ONLY if the desktop user is also logged in."
fi

# ── Step 1 — Obtain the .app ──────────────────────────────────────────────────
APP_SRC=""
if [ -n "${1:-}" ]; then
    # Local path supplied (dev / CI / re-install)
    APP_SRC="$1"
    if [ ! -d "$APP_SRC" ]; then
        die "Supplied path is not a directory: $APP_SRC"
    fi
    ok "Using local app: $APP_SRC"
else
    # Download from the latest GitHub Release.
    # Use the redirect-based direct URL, NOT api.github.com: the API rate-limits
    # anonymous clients to 60 req/h per IP and returns 403 when exhausted
    # (hardware-tested failure mode). The /releases/latest/download/ path is
    # served by the web tier with no API quota.
    info "Fetching latest release from github.com/$REPO ..."
    if command -v curl >/dev/null 2>&1; then
        DOWNLOAD_CMD="curl -fsSL"
    else
        die "curl is required.  Install Xcode Command Line Tools: xcode-select --install"
    fi

    DOWNLOAD_URL="https://github.com/$REPO/releases/latest/download/${APP_NAME}.zip"

    TMPDIR_INSTALL="$(mktemp -d)"
    ZIP="$TMPDIR_INSTALL/${APP_NAME}.zip"
    info "Downloading $DOWNLOAD_URL ..."
    $DOWNLOAD_CMD -o "$ZIP" "$DOWNLOAD_URL"

    # Verify sha256 if checksum file is available
    SHA_URL="${DOWNLOAD_URL}.sha256"
    EXPECTED_SHA="$($DOWNLOAD_CMD "$SHA_URL" 2>/dev/null | awk '{print $1}' || true)"
    if [ -n "$EXPECTED_SHA" ]; then
        ACTUAL_SHA="$(shasum -a 256 "$ZIP" | awk '{print $1}')"
        if [ "$ACTUAL_SHA" != "$EXPECTED_SHA" ]; then
            die "SHA-256 mismatch!\n  expected: $EXPECTED_SHA\n  actual:   $ACTUAL_SHA"
        fi
        ok "SHA-256 verified"
    else
        warn "No .sha256 file found; skipping checksum verification."
    fi

    info "Extracting ..."
    unzip -q "$ZIP" -d "$TMPDIR_INSTALL"
    APP_SRC="$TMPDIR_INSTALL/$APP_NAME"
    if [ ! -d "$APP_SRC" ]; then
        die "Extraction did not produce $APP_NAME in $TMPDIR_INSTALL"
    fi
    ok "Downloaded and extracted: $APP_SRC"
fi

# ── Step 2 — Remove quarantine ────────────────────────────────────────────────
info "Removing quarantine attribute ..."
xattr -dr com.apple.quarantine "$APP_SRC" 2>/dev/null || true
ok "Quarantine cleared"

# ── Step 3 — Sign locally if not already signed with a stable identity ────────
SIGNED_ID="$(codesign --display --verbose=4 "$APP_SRC" 2>&1 \
             | grep 'Identifier=' | head -1 \
             | sed 's/.*Identifier=\(.*\)/\1/' || true)"

if [ "$SIGNED_ID" = "$BUNDLE_ID" ]; then
    ok "Already signed with bundle-id: $BUNDLE_ID"
else
    warn "App is unsigned or has a different bundle-id ('$SIGNED_ID'). Signing locally ..."
    SCRIPT_DIR="$(cd "$(dirname "$0")" 2>/dev/null && pwd || echo ".")"
    SIGN_SH="$SCRIPT_DIR/scripts/sign.sh"
    if [ -f "$SIGN_SH" ]; then
        bash "$SIGN_SH" "$APP_SRC"
    else
        warn "scripts/sign.sh not found; attempting inline self-signed codesign ..."
        _inline_sign "$APP_SRC"
    fi
fi

# ── Step 4 — Verify bundle-id in signature ────────────────────────────────────
FINAL_ID="$(codesign --display --verbose=4 "$APP_SRC" 2>&1 \
            | grep 'Identifier=' | head -1 \
            | sed 's/.*Identifier=\(.*\)/\1/' || true)"
if [ "$FINAL_ID" != "$BUNDLE_ID" ]; then
    die "Signed bundle-id '$FINAL_ID' does not match expected '$BUNDLE_ID'.\n     TCC grants will not persist.  Check deploy/Info.plist."
fi
ok "Bundle-id verified: $BUNDLE_ID"

# ── Step 5 — Install .app ─────────────────────────────────────────────────────
mkdir -p "$INSTALL_DIR"
DEST="$INSTALL_DIR/$APP_NAME"

if [ -d "$DEST" ]; then
    info "Removing existing install: $DEST"
    rm -rf "$DEST"
fi

cp -R "$APP_SRC" "$DEST"
ok "Installed: $DEST"

# ── Step 6 — Create log directory ─────────────────────────────────────────────
mkdir -p "$LOG_DIR"
ok "Log directory: $LOG_DIR"

# ── Step 6b — Resolve listen host + password ──────────────────────────────────
# The iPhone reaches the daemon over the LAN, so it must bind 0.0.0.0 (the
# daemon's own default is 127.0.0.1, which the phone can't reach). Binding to
# the LAN without a password would let anyone on the network drive the phone, so
# a password is mandatory here — generated if the user didn't supply one.
HOST="${PHONE_REMOTE_HOST:-0.0.0.0}"
PORT="${PHONE_REMOTE_PORT:-44321}"

# Read a single EnvironmentVariables key out of the existing plist (if any), so a
# re-install preserves values the user set on a prior run instead of silently
# dropping them. Empty string when the plist or key is absent.
plist_env_get() {
    [ -f "$PLIST_DST" ] || { printf ''; return; }
    /usr/libexec/PlistBuddy -c "Print :EnvironmentVariables:$1" "$PLIST_DST" 2>/dev/null || printf ''
}

# Password precedence: explicit env > existing plist > freshly generated.
# Re-running install must NOT rotate a working password (it would break every
# saved client/bookmark) — only mint one when there genuinely isn't one yet.
PASSWORD="${PHONE_REMOTE_PASSWORD:-}"
PW_SOURCE="env"
if [ -z "$PASSWORD" ]; then
    PASSWORD="$(plist_env_get PHONE_REMOTE_PASSWORD)"
    PW_SOURCE="existing"
fi
if [ -z "$PASSWORD" ]; then
    PASSWORD="$(LC_ALL=C tr -dc 'A-Za-z0-9' </dev/urandom | head -c 16 || true)"
    [ -n "$PASSWORD" ] || PASSWORD="$(date +%s | shasum | head -c 16)"
    PW_SOURCE="generated"
fi
case "$PW_SOURCE" in
    env)      ok "Using password from \$PHONE_REMOTE_PASSWORD" ;;
    existing) ok "Reusing the password from the existing install" ;;
    generated) ok "Generated a random access password (shown at the end)" ;;
esac

# Best-effort LAN IP for the final connect instructions.
LAN_IP="$(ipconfig getifaddr en0 2>/dev/null || ipconfig getifaddr en1 2>/dev/null || true)"
[ -n "$LAN_IP" ] || LAN_IP="<this-mac-LAN-ip>"

# Optional L2 element layer (WebDriverAgent on the phone). Precedence mirrors the
# password: explicit env > existing plist > absent. Without this the daemon runs
# L3-only (CGEvent against the Mirroring window), which silently drops input when
# the phone is in hand / Mirroring isn't frontmost — so a re-install must carry a
# previously-set URL forward, not wipe it.
WDA_URL="${PHONE_REMOTE_WDA_URL:-}"
[ -n "$WDA_URL" ] || WDA_URL="$(plist_env_get PHONE_REMOTE_WDA_URL)"
WDA_PLIST_BLOCK=""
if [ -n "$WDA_URL" ]; then
    WDA_PLIST_BLOCK="        <key>PHONE_REMOTE_WDA_URL</key>
        <string>${WDA_URL}</string>
"
    ok "L2 element layer (WDA) wired: $WDA_URL"
else
    info "L2 element layer not set (L3 input only). For on-device taps + element"
    info "  tree, run scripts/setup-wda.sh, then export PHONE_REMOTE_WDA_URL and re-run."
fi

# Optional Cloudflare TURN (cross-network access). If both are exported, embed
# them so the daemon mints ephemeral relay credentials. Absent → STUN-only
# (LAN/same-network works; cellular/remote needs these).
CF_TURN_KEY_ID="${PHONE_REMOTE_CF_TURN_KEY_ID:-}"
CF_TURN_API_TOKEN="${PHONE_REMOTE_CF_TURN_API_TOKEN:-}"
CF_PLIST_BLOCK=""
if [ -n "$CF_TURN_KEY_ID" ] && [ -n "$CF_TURN_API_TOKEN" ]; then
    CF_PLIST_BLOCK="        <key>PHONE_REMOTE_CF_TURN_KEY_ID</key>
        <string>${CF_TURN_KEY_ID}</string>
        <key>PHONE_REMOTE_CF_TURN_API_TOKEN</key>
        <string>${CF_TURN_API_TOKEN}</string>
"
    ok "Cloudflare TURN configured — cross-network relay enabled"
else
    info "Cloudflare TURN not set (STUN-only; fine on same Wi-Fi). To enable cross-network,"
    info "  export PHONE_REMOTE_CF_TURN_KEY_ID + PHONE_REMOTE_CF_TURN_API_TOKEN and re-run."
fi

# ── Step 7 — Write the LaunchAgent plist ─────────────────────────────────────
mkdir -p "$HOME/Library/LaunchAgents"
BINARY_PATH="$DEST/$BINARY_INSIDE_APP"

cat > "$PLIST_DST" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
    "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>${PLIST_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>${BINARY_PATH}</string>
        <string>serve</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>${LOG_DIR}/iphone-use.log</string>
    <key>StandardErrorPath</key>
    <string>${LOG_DIR}/iphone-use.err</string>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>LimitLoadToSessionType</key>
    <string>Aqua</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>RUST_LOG</key>
        <string>info</string>
        <key>PHONE_REMOTE_HOST</key>
        <string>${HOST}</string>
        <key>PHONE_REMOTE_PORT</key>
        <string>${PORT}</string>
        <key>PHONE_REMOTE_PASSWORD</key>
        <string>${PASSWORD}</string>
${WDA_PLIST_BLOCK}${CF_PLIST_BLOCK}    </dict>
</dict>
</plist>
PLIST

# The plist embeds the password in plaintext — lock it to the user only.
chmod 600 "$PLIST_DST"

ok "LaunchAgent plist written: $PLIST_DST"

# ── Step 8 — Load / restart the LaunchAgent (no sudo; gui/$UID) ──────────────
info "Loading LaunchAgent (gui/$UID_NUM) ..."

# Evict any PRIOR-label daemon first. Before v0.2.0 the label/app/bundle-id were
# work.pwtk.iphone-remote / iPhoneRemote.app. A label change means the OLD
# LaunchAgent is NOT superseded by ours — it keeps respawning and squats the
# port, so the new daemon can't bind and the two race (flaky, served the wrong
# build). Boot it out, disable its plist, and kill the old app.
for OLD_LABEL in work.pwtk.iphone-remote; do
    if launchctl print "gui/$UID_NUM/$OLD_LABEL" >/dev/null 2>&1; then
        warn "Evicting old daemon: $OLD_LABEL (port-squat from a pre-rename install)"
        launchctl bootout "gui/$UID_NUM/$OLD_LABEL" 2>/dev/null || true
    fi
    OLD_PLIST="$HOME/Library/LaunchAgents/$OLD_LABEL.plist"
    [ -f "$OLD_PLIST" ] && mv "$OLD_PLIST" "$OLD_PLIST.disabled" 2>/dev/null || true
done
pkill -f "iPhoneRemote.app/Contents/MacOS" 2>/dev/null || true

# Unload OUR label if already running (idempotent)
launchctl bootout "gui/$UID_NUM/$PLIST_LABEL" 2>/dev/null || true

# Bootstrap from the new plist
launchctl bootstrap "gui/$UID_NUM" "$PLIST_DST"
ok "LaunchAgent bootstrapped"

# Enable (persist across reboots)
launchctl enable "gui/$UID_NUM/$PLIST_LABEL"
ok "LaunchAgent enabled (persists across reboots)"

# Kick it: -k kills a running instance first, then starts fresh
launchctl kickstart -k "gui/$UID_NUM/$PLIST_LABEL"
ok "LaunchAgent started"

# ── Step 9 — Open TCC permission panes ───────────────────────────────────────
echo ""
printf "${BOLD}━━━ Grant permissions (required once) ━━━${RESET}\n"
echo ""
info "Opening System Settings > Privacy & Security > Screen Recording ..."
open "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
sleep 1
info "Opening System Settings > Privacy & Security > Accessibility ..."
open "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"

echo ""
printf "${YELLOW}ACTION REQUIRED:${RESET}\n"
printf "  1. In ${BOLD}Screen Recording${RESET}: enable the toggle next to ${BOLD}iPhoneUse${RESET}.\n"
printf "  2. In ${BOLD}Accessibility${RESET}: enable the toggle next to ${BOLD}iPhoneUse${RESET}.\n"
printf "  3. After granting both, restart the daemon:\n"
printf "     ${BOLD}launchctl kickstart -k gui/%s/%s${RESET}\n" "$UID_NUM" "$PLIST_LABEL"
printf "\n"
printf "${YELLOW}NOTE (monthly):${RESET} macOS ~monthly re-prompts for Screen Recording.\n"
printf "  You must re-grant it in the GUI; it cannot be suppressed without MDM/PPPC.\n"
printf "\n"
printf "${YELLOW}NOTE (headless):${RESET} The daemon only starts after a desktop login (Aqua session).\n"
printf "  On unattended machines enable auto-login: System Settings > Users & Groups.\n"

# ── Step 9b — Companion agent skill (best-effort) ────────────────────────────
# Teach skills-capable agents (Claude Code, Codex, Cursor, …) how to drive this
# daemon. `npx skills add -g` both installs a fresh copy and updates an existing
# one, so this doubles as the skill-side upgrade. Best-effort: gated on `npx`
# being present, never fails the install, and opt-out via IPHONE_USE_SKIP_SKILL=1.
echo ""
printf "${BOLD}━━━ Agent skill ━━━${RESET}\n"
if [ "${IPHONE_USE_SKIP_SKILL:-0}" = "1" ]; then
    info "Skipped (IPHONE_USE_SKIP_SKILL=1)."
elif command -v npx >/dev/null 2>&1; then
    info "Installing/updating the iphone-use agent skill via npx skills ..."
    # A harmless 'PromptScript does not support global skill installation' line
    # may appear (PromptScript is project-level only); the other agents install
    # fine and the exit status stays 0.
    if npx -y skills add leeguooooo/iphone-use -g -y >/dev/null 2>&1; then
        ok "Agent skill installed/updated (npx skills add leeguooooo/iphone-use -g)."
    else
        warn "npx skills did not complete — run it yourself:"
        printf "    npx skills add leeguooooo/iphone-use -g -y\n"
    fi
else
    info "npx not found — skipping. To teach agents to drive the phone, run:"
    printf "    npx skills add leeguooooo/iphone-use -g -y\n"
fi

# ── Step 10 — Print current status ───────────────────────────────────────────
echo ""
printf "${BOLD}━━━ Current LaunchAgent status ━━━${RESET}\n"
launchctl print "gui/$UID_NUM/$PLIST_LABEL" 2>/dev/null || info "(not running yet — grant permissions then kickstart)"

echo ""
printf "${BOLD}━━━ Connect from your iPhone ━━━${RESET}\n"
printf "  1. Make sure the iPhone is on the same Wi-Fi as this Mac.\n"
printf "  2. In iPhone Safari open:  ${BOLD}http://%s:%s/phone${RESET}\n" "$LAN_IP" "$PORT"
printf "  3. Password: ${BOLD}%s${RESET}\n" "$PASSWORD"
if [ "$GENERATED_PW" = "1" ]; then
    printf "     ${YELLOW}(generated — save it; it's stored in %s)${RESET}\n" "$PLIST_DST"
fi
printf "     Change it later by editing PHONE_REMOTE_PASSWORD in that plist + kickstart.\n"

echo ""
printf "${BOLD}━━━ Quick reference ━━━${RESET}\n"
printf "  Status  : launchctl print gui/%s/%s\n"       "$UID_NUM" "$PLIST_LABEL"
printf "  Restart : launchctl kickstart -k gui/%s/%s\n" "$UID_NUM" "$PLIST_LABEL"
printf "  Stop    : launchctl bootout gui/%s/%s\n"      "$UID_NUM" "$PLIST_LABEL"
printf "  Logs    : tail -f %s/iphone-use.log\n"    "$LOG_DIR"
printf "  Errors  : tail -f %s/iphone-use.err\n"    "$LOG_DIR"
echo ""
ok "Install complete."
