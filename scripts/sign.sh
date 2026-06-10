#!/usr/bin/env bash
# scripts/sign.sh — codesign iPhoneUse.app with a stable identity
#
# Usage:
#   ./scripts/sign.sh [APP_PATH]
#
# APP_PATH defaults to ./iPhoneUse.app
#
# Identity selection (first match wins):
#   1. $SIGN_IDENTITY env var  — use verbatim (Developer ID Application or cert name)
#   2. Any "Developer ID Application" cert in the login keychain
#   3. Self-signed "iPhoneUse Local Signing" cert (created if absent)
#
# Why a STABLE identity matters:
#   TCC grants (Screen Recording, Accessibility) are keyed on the Designated
#   Requirement (bundle-id + signing identity hash).  Ad-hoc signing changes the
#   cdhash on every build → grants lost.  A reused self-signed cert keeps the DR
#   stable across rebuilds, just like Developer ID does.
#
# Signing order: nested binary FIRST, then the outer .app bundle.
#
set -eu

APP="${1:-"$(pwd)/iPhoneUse.app"}"
CERT_NAME="iPhoneUse Local Signing"
BUNDLE_ID="com.leeguoo.iphone-use"

# ── Helper: openssl fallback for self-signed cert import ─────────────────────
# Must be defined before any code path that calls it.
# Only called when 'security create-keychain-cert' is unavailable.
_create_selfsigned_cert_via_openssl() {
    local name="$1"
    local tmpdir
    tmpdir="$(mktemp -d)"
    local key="$tmpdir/key.pem"
    local cert="$tmpdir/cert.pem"
    local p12="$tmpdir/cert.p12"
    local pass="iphone-use-local-only"

    # Generate key + self-signed cert
    openssl req -x509 -newkey rsa:4096 -sha256 -days 3650 -nodes \
        -keyout "$key" -out "$cert" \
        -subj "/CN=$name/O=iPhoneUse/OU=Code Signing" \
        -extensions v3_ca \
        2>/dev/null

    # Pack into PKCS#12 for Keychain import
    openssl pkcs12 -export \
        -in "$cert" -inkey "$key" \
        -out "$p12" \
        -passout "pass:$pass" \
        -name "$name" \
        2>/dev/null

    # Import into login keychain; mark as trusted for Code Signing
    security import "$p12" \
        -k ~/Library/Keychains/login.keychain-db \
        -P "$pass" \
        -T /usr/bin/codesign \
        -f pkcs12

    # Trust the cert for Code Signing
    local cert_sha256
    cert_sha256="$(openssl x509 -in "$cert" -noout -fingerprint -sha256 \
                   | sed 's/.*=//;s/://g')"
    security set-key-partition-list \
        -S apple-tool:,apple: \
        -k "" \
        ~/Library/Keychains/login.keychain-db 2>/dev/null || true

    rm -rf "$tmpdir"
}

# ── Resolve signing identity ─────────────────────────────────────────────────
if [ -n "${SIGN_IDENTITY:-}" ]; then
    IDENTITY="$SIGN_IDENTITY"
    echo "Using provided SIGN_IDENTITY: $IDENTITY"
else
    # Look for Developer ID first
    DEV_ID="$(security find-identity -v -p codesigning 2>/dev/null \
              | grep 'Developer ID Application' \
              | head -1 \
              | sed 's/.*"\(.*\)".*/\1/' || true)"

    if [ -n "$DEV_ID" ]; then
        IDENTITY="$DEV_ID"
        echo "Found Developer ID: $IDENTITY"
    else
        # Fall back to self-signed cert; create it if missing
        EXISTING="$(security find-certificate -c "$CERT_NAME" \
                    ~/Library/Keychains/login.keychain-db 2>/dev/null || true)"
        if [ -z "$EXISTING" ]; then
            echo "Creating self-signed Code Signing cert: '$CERT_NAME' ..."
            # Use the system Certificate Assistant CLI.
            # Generates a 4096-bit RSA cert valid for 3650 days in the login keychain.
            security create-keychain-cert \
                -k ~/Library/Keychains/login.keychain-db \
                -c "$CERT_NAME" \
                -t "Code Signing" \
                -s "$CERT_NAME" \
                -Z \
                2>/dev/null || {
                # Fallback: use the older Keychain Assistant approach via
                # a temporary self-signed cert config (macOS 12–15 compatible).
                echo "security create-keychain-cert unavailable; using openssl fallback ..."
                _create_selfsigned_cert_via_openssl "$CERT_NAME"
            }
            echo "Cert created: $CERT_NAME"
        else
            echo "Reusing existing self-signed cert: $CERT_NAME"
        fi
        IDENTITY="$CERT_NAME"
    fi
fi

# ── Verify the .app exists ────────────────────────────────────────────────────
if [ ! -d "$APP" ]; then
    echo "ERROR: app bundle not found: $APP" >&2
    echo "       Run scripts/make-app.sh first." >&2
    exit 1
fi

BINARY="$APP/Contents/MacOS/iphone-use"
if [ ! -f "$BINARY" ]; then
    echo "ERROR: binary missing inside bundle: $BINARY" >&2
    exit 1
fi

# ── Sign nested binary first, then the outer bundle ──────────────────────────
echo "Signing binary: $BINARY ..."
codesign \
    --force \
    --options runtime \
    --sign "$IDENTITY" \
    --timestamp=none \
    "$BINARY"

echo "Signing bundle: $APP ..."
codesign \
    --force \
    --options runtime \
    --sign "$IDENTITY" \
    --timestamp=none \
    "$APP"

# ── Verify ────────────────────────────────────────────────────────────────────
echo "Verifying signature ..."
codesign --verify --verbose=2 "$APP"

SIGNED_BUNDLE_ID="$(codesign --display --verbose=4 "$APP" 2>&1 \
                    | grep 'Identifier=' | head -1 \
                    | sed 's/.*Identifier=\(.*\)/\1/' || true)"

if [ "$SIGNED_BUNDLE_ID" != "$BUNDLE_ID" ]; then
    echo "WARNING: signed bundle-id '$SIGNED_BUNDLE_ID' != expected '$BUNDLE_ID'" >&2
    echo "         TCC grants may not persist.  Check deploy/Info.plist." >&2
else
    echo "Bundle id confirmed: $BUNDLE_ID"
fi

echo "Signing complete."
