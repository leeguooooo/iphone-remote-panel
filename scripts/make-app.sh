#!/usr/bin/env bash
# scripts/make-app.sh — wrap target/release/iphone-use into iPhoneUse.app
#
# Usage:
#   ./scripts/make-app.sh [OUTPUT_DIR]
#
# OUTPUT_DIR defaults to the repo root.  The resulting .app is always at:
#   <OUTPUT_DIR>/iPhoneUse.app
#
# Called by CI (release-binaries.yml) and local dev after:
#   cargo build --release --bin iphone-use
#
set -eu

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUTPUT_DIR="${1:-"$REPO_ROOT"}"
APP="$OUTPUT_DIR/iPhoneUse.app"
BINARY="$REPO_ROOT/target/release/iphone-use"
DEPLOY_DIR="$REPO_ROOT/deploy"

# ── Verify the binary was built ───────────────────────────────────────────────
if [ ! -f "$BINARY" ]; then
    echo "ERROR: binary not found: $BINARY" >&2
    echo "       Run: cargo build --release --bin iphone-use" >&2
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
echo "  Binary : $APP/Contents/MacOS/iphone-use"
echo "  Plist  : $APP/Contents/Info.plist"
[ -f "$ICNS" ] && echo "  Icon   : $APP/Contents/Resources/AppIcon.icns"
