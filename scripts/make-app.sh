#!/usr/bin/env bash
# scripts/make-app.sh — wrap target/release/iphone-remote into iPhoneRemote.app
#
# Usage:
#   ./scripts/make-app.sh [OUTPUT_DIR]
#
# OUTPUT_DIR defaults to the repo root.  The resulting .app is always at:
#   <OUTPUT_DIR>/iPhoneRemote.app
#
# Called by CI (release-binaries.yml) and local dev after:
#   cargo build --release --bin iphone-remote
#
set -eu

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUTPUT_DIR="${1:-"$REPO_ROOT"}"
APP="$OUTPUT_DIR/iPhoneRemote.app"
BINARY="$REPO_ROOT/target/release/iphone-remote"
DEPLOY_DIR="$REPO_ROOT/deploy"

# ── Verify the binary was built ───────────────────────────────────────────────
if [ ! -f "$BINARY" ]; then
    echo "ERROR: binary not found: $BINARY" >&2
    echo "       Run: cargo build --release --bin iphone-remote" >&2
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
cp "$BINARY" "$APP/Contents/MacOS/iphone-remote"
chmod 755 "$APP/Contents/MacOS/iphone-remote"

# Write Info.plist, substituting version placeholders from template
sed \
    -e "s|<string>0\.1\.0</string>|<string>${CARGO_VERSION}</string>|" \
    -e "s|<string>1</string>|<string>${BUNDLE_VERSION}</string>|" \
    "$DEPLOY_DIR/Info.plist" \
    > "$APP/Contents/Info.plist"

# PkgInfo (type + creator; conventional for macOS .app bundles)
printf 'APPL????' > "$APP/Contents/PkgInfo"

echo "Created: $APP"
echo "  Binary : $APP/Contents/MacOS/iphone-remote"
echo "  Plist  : $APP/Contents/Info.plist"
