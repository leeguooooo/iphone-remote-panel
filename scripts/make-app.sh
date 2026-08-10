#!/usr/bin/env bash
# scripts/make-app.sh — wrap the daemon and MCP bridge into iPhoneUse.app
#
# Usage:
#   ./scripts/make-app.sh [OUTPUT_DIR]
#
# OUTPUT_DIR defaults to the repo root.  The resulting .app is always at:
#   <OUTPUT_DIR>/iPhoneUse.app
#
# Called by CI (release-binaries.yml) and local dev after:
#   cargo build --release --bin iphone-use --bin iphone-use-mcp
#
set -eu

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUTPUT_DIR="${1:-"$REPO_ROOT"}"
APP="$OUTPUT_DIR/iPhoneUse.app"
BINARY="$REPO_ROOT/target/release/iphone-use"
MCP_BINARY="$REPO_ROOT/target/release/iphone-use-mcp"
DEPLOY_DIR="$REPO_ROOT/deploy"

# ── Verify both release-matched binaries were built ───────────────────────────
if [ ! -f "$BINARY" ]; then
    echo "ERROR: binary not found: $BINARY" >&2
    echo "       Run: cargo build --release --bin iphone-use --bin iphone-use-mcp" >&2
    exit 1
fi
if [ ! -f "$MCP_BINARY" ]; then
    echo "ERROR: MCP binary not found: $MCP_BINARY" >&2
    echo "       Run: cargo build --release --bin iphone-use --bin iphone-use-mcp" >&2
    exit 1
fi

# ── Extract version from Cargo.toml for CFBundleShortVersionString ────────────
CARGO_VERSION=""
SERVER_TOML="$REPO_ROOT/crates/server/Cargo.toml"
if [ -f "$SERVER_TOML" ]; then
    CARGO_VERSION="$(grep '^version' "$SERVER_TOML" | head -1 | sed 's/.*= *"\([^"]*\)".*/\1/')"
fi
CARGO_VERSION="${CARGO_VERSION:-0.1.0}"

# ── Bundle build number (numeric; use git commit count if available) ──────────
BUNDLE_VERSION="$(git -C "$REPO_ROOT" rev-list --count HEAD 2>/dev/null || echo '1')"

# ── (Re)create the bundle skeleton ───────────────────────────────────────────
echo "Building $APP (version $CARGO_VERSION, build $BUNDLE_VERSION) ..."
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
mkdir -p "$APP/Contents/Resources"

# Copy the binary
cp "$BINARY" "$APP/Contents/MacOS/iphone-use"
chmod 755 "$APP/Contents/MacOS/iphone-use"
cp "$MCP_BINARY" "$APP/Contents/MacOS/iphone-use-mcp"
chmod 755 "$APP/Contents/MacOS/iphone-use-mcp"

# Write Info.plist, substituting version placeholders from template.
# Uses context-sensitive sed (n command: advance to next line after the key) so
# the version value is updated regardless of what it was before — bumping the
# version in Cargo.toml is reflected without touching this script.
sed \
    -e "/CFBundleShortVersionString/{n; s|<string>[^<]*</string>|<string>${CARGO_VERSION}</string>|;}" \
    -e "/CFBundleVersion/{n; s|<string>[^<]*</string>|<string>${BUNDLE_VERSION}</string>|;}" \
    "$DEPLOY_DIR/Info.plist" \
    > "$APP/Contents/Info.plist"

# App icon (Info.plist references CFBundleIconFile = AppIcon).
ICNS="$REPO_ROOT/assets/AppIcon.icns"
if [ -f "$ICNS" ]; then
    cp "$ICNS" "$APP/Contents/Resources/AppIcon.icns"
fi

# PkgInfo (type + creator; conventional for macOS .app bundles)
printf 'APPL????' > "$APP/Contents/PkgInfo"

echo "Created: $APP"
echo "  Daemon : $APP/Contents/MacOS/iphone-use"
echo "  MCP    : $APP/Contents/MacOS/iphone-use-mcp"
echo "  Plist  : $APP/Contents/Info.plist"
[ -f "$ICNS" ] && echo "  Icon   : $APP/Contents/Resources/AppIcon.icns"
