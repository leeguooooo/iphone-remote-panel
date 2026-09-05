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

# Dedicated signing keychain. We own its password, so partition-list setup is
# fully non-interactive — no GUI "Always Allow" prompt, no login password.
SIGN_KEYCHAIN="$HOME/Library/Keychains/iphone-use-signing.keychain-db"
SIGN_KC_PASS="iphone-use-local-only"

# ── Create (once) a self-signed Code Signing identity in a dedicated keychain ─
# A STABLE signing identity keeps the Designated Requirement constant across
# rebuilds, so TCC grants (Screen Recording, Accessibility) survive — unlike
# ad-hoc signing, whose cdhash changes every build and drops the grants.
_ensure_local_signing_cert() {
    # Already present? Use `find-certificate`, NOT `find-identity`. A self-signed
    # cert isn't *trusted*, so BOTH `find-identity -v` and `-p codesigning` hide
    # it (they apply a trust/validity policy) even though codesign signs with it
    # fine. Using find-identity here made the check always fail → a NEW cert (new
    # hash → new DR) every run, defeating the whole point. find-certificate looks
    # it up by name regardless of trust.
    if security find-certificate -c "$CERT_NAME" "$SIGN_KEYCHAIN" >/dev/null 2>&1; then
        return 0
    fi

    echo "Creating self-signed Code Signing identity '$CERT_NAME' (one-time) ..." >&2
    local tmpdir; tmpdir="$(mktemp -d)"
    local key="$tmpdir/key.pem" cert="$tmpdir/cert.pem" p12="$tmpdir/cert.p12"
    local ext="$tmpdir/ext.cnf" p12pass="pw"

    # A code-signing LEAF cert: CA:false + the codeSigning EKU is what codesign
    # requires (the old v3_ca extension produced a CA cert codesign rejects).
    cat > "$ext" <<EOF
[req]
distinguished_name = dn
x509_extensions = v3
prompt = no
[dn]
CN = $CERT_NAME
[v3]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
EOF
    openssl req -x509 -newkey rsa:2048 -sha256 -days 3650 -nodes \
        -keyout "$key" -out "$cert" -config "$ext" 2>/dev/null
    # -legacy: macOS `security import` can't read OpenSSL 3's default PKCS#12 AES.
    openssl pkcs12 -export -legacy -in "$cert" -inkey "$key" -out "$p12" \
        -passout "pass:$p12pass" -name "$CERT_NAME" 2>/dev/null

    # Dedicated keychain whose password we know → no prompts, no login keychain.
    security create-keychain -p "$SIGN_KC_PASS" "$SIGN_KEYCHAIN" 2>/dev/null || true
    security set-keychain-settings "$SIGN_KEYCHAIN"          # no auto-lock timeout
    security unlock-keychain -p "$SIGN_KC_PASS" "$SIGN_KEYCHAIN"
    security import "$p12" -k "$SIGN_KEYCHAIN" -P "$p12pass" \
        -T /usr/bin/codesign -A
    # Authorize codesign to use the private key without a GUI prompt.
    security set-key-partition-list -S apple-tool:,apple: -s \
        -k "$SIGN_KC_PASS" "$SIGN_KEYCHAIN" >/dev/null 2>&1
    rm -rf "$tmpdir"
    echo "Created '$CERT_NAME' in $SIGN_KEYCHAIN" >&2
}

# Make the dedicated keychain visible to codesign's identity search.
_add_signing_keychain_to_search() {
    local cur
    cur="$(security list-keychains -d user | sed 's/[" ]//g' | tr '\n' ' ')"
    case " $cur " in
        *" $SIGN_KEYCHAIN "*) : ;;  # already in the list
        *) security list-keychains -d user -s "$SIGN_KEYCHAIN" $cur ;;
    esac
    security unlock-keychain -p "$SIGN_KC_PASS" "$SIGN_KEYCHAIN" 2>/dev/null || true
}

# ── Resolve signing identity ─────────────────────────────────────────────────
if [ -n "${SIGN_IDENTITY:-}" ]; then
    IDENTITY="$SIGN_IDENTITY"
    echo "Using provided SIGN_IDENTITY: $IDENTITY"
else
    # Prefer a real Developer ID if the user has one.
    DEV_ID="$(security find-identity -v -p codesigning 2>/dev/null \
              | grep 'Developer ID Application' \
              | head -1 \
              | sed 's/.*"\(.*\)".*/\1/' || true)"

    if [ -n "$DEV_ID" ]; then
        IDENTITY="$DEV_ID"
        echo "Found Developer ID: $IDENTITY"
    else
        # Stable self-signed identity in our dedicated keychain.
        _ensure_local_signing_cert
        _add_signing_keychain_to_search
        IDENTITY="$CERT_NAME"
        echo "Using stable self-signed identity: $CERT_NAME"
    fi
fi

# ── Verify the .app exists ────────────────────────────────────────────────────
if [ ! -d "$APP" ]; then
    echo "ERROR: app bundle not found: $APP" >&2
    echo "       Run scripts/make-app.sh first." >&2
    exit 1
fi

DAEMON_BINARY="$APP/Contents/MacOS/iphone-use"
MCP_BINARY="$APP/Contents/MacOS/iphone-use-mcp"
for binary in "$DAEMON_BINARY" "$MCP_BINARY"; do
    if [ ! -f "$binary" ]; then
        echo "ERROR: binary missing inside bundle: $binary" >&2
        exit 1
    fi
done

# ── Signing flags ─────────────────────────────────────────────────────────────
# Notarization rejects a Developer ID signature without the hardened runtime
# and a secure timestamp ("The executable does not have the hardened runtime
# enabled" / "The signature does not include a secure timestamp"). The
# self-signed local identity is never notarized, and a timestamp request
# needs network access it may not have, so it keeps the offline flags.
case "$IDENTITY" in
    *"Developer ID Application"*)
        SIGN_FLAGS=(--options runtime --timestamp)
        echo "Developer ID identity: signing with hardened runtime + secure timestamp"
        ;;
    *)
        SIGN_FLAGS=(--timestamp=none)
        ;;
esac

# ── Sign nested binaries first, then the outer bundle ─────────────────────────
# $MCP_BINARY leads deliberately. $DAEMON_BINARY is the bundle's main
# executable, so codesign folds bundle validation into signing it and rejects
# the still-unsigned nested helper ("code object is not signed at all / In
# subcomponent: .../iphone-use-mcp"). The comment above was always right; the
# order underneath it was not.
for binary in "$MCP_BINARY" "$DAEMON_BINARY"; do
    echo "Signing binary: $binary ..."
    codesign \
        --force \
        --sign "$IDENTITY" \
        "${SIGN_FLAGS[@]}" \
        "$binary"
done

echo "Signing bundle: $APP ..."
codesign \
    --force \
    --sign "$IDENTITY" \
    "${SIGN_FLAGS[@]}" \
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
