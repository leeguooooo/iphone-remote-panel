#!/usr/bin/env bash
#
# Regression test for the ad-hoc signing order in install.sh and sign.sh.
#
# The bug this pins down: `iphone-use` is the app bundle's MAIN executable, so
# `codesign --sign - .../MacOS/iphone-use` is treated as signing the *bundle*
# and validates nested code on the way. With `iphone-use-mcp` still only
# linker-signed at that moment, codesign fails:
#
#   .../MacOS/iphone-use: code object is not signed at all
#   In subcomponent: .../MacOS/iphone-use-mcp
#
# install.sh runs under `set -e`, so that aborted the whole install and rolled
# it back — every published release was uninstallable on any machine that took
# the ad-hoc path, which is all of them, because CI ships a bundle whose outer
# signature is absent and whose binaries carry only a linker-signed ad-hoc one.
#
# Unit tests cannot catch this: it only reproduces against a real two-binary
# .app on a real macOS codesign. So this test builds one and runs the actual
# signer functions from the actual scripts.
#
# macOS only; skips cleanly elsewhere.

set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PASS=0
FAIL=0

N=0
plan() { echo "1..$1"; }
ok() {
    N=$((N + 1))
    PASS=$((PASS + 1))
    echo "ok $N - $1"
}
notok() {
    N=$((N + 1))
    FAIL=$((FAIL + 1))
    echo "not ok $N - $1"
}

if [ "$(uname -s)" != "Darwin" ] || ! command -v codesign >/dev/null 2>&1; then
    echo "1..0 # SKIP requires macOS codesign"
    exit 0
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# --- Build a bundle shaped exactly like the shipped one ----------------------
# Two Mach-O binaries in Contents/MacOS, the main one named in Info.plist, each
# carrying only the linker-signed ad-hoc signature the toolchain emits — which
# is what CI publishes.
#
# The binaries MUST be universal. That is the actual trigger, and it is not
# incidental: with thin arm64 binaries codesign signs the main executable
# happily and the bug does not reproduce at all. Release binaries are
# x86_64+arm64, a fat linker-signed helper is what nested validation rejects,
# and a thin fixture would make this test pass against the broken order.
make_app() {
    local app="$1"
    mkdir -p "$app/Contents/MacOS"
    cat >"$WORK/main.c" <<'EOF'
int main(void) { return 0; }
EOF
    cc -arch x86_64 -arch arm64 -o "$app/Contents/MacOS/iphone-use" \
        "$WORK/main.c" 2>/dev/null || return 1
    cc -arch x86_64 -arch arm64 -o "$app/Contents/MacOS/iphone-use-mcp" \
        "$WORK/main.c" 2>/dev/null || return 1
    cat >"$app/Contents/Info.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleExecutable</key><string>iphone-use</string>
  <key>CFBundleIdentifier</key><string>com.leeguoo.iphone-use.signtest</string>
  <key>CFBundleName</key><string>iPhoneUse</string>
  <key>CFBundlePackageType</key><string>APPL</string>
</dict></plist>
EOF
}

plan 4

APP="$WORK/iPhoneUse.app"
if ! make_app "$APP"; then
    echo "1..0 # SKIP no compiler able to emit universal binaries"
    exit 0
fi

# 1. The fixture must actually reproduce the shipped precondition, or this test
#    proves nothing. Assert BOTH halves of that precondition: unsigned as a
#    bundle, and universal — a thin fixture silently stops reproducing the bug.
FIXTURE_OK=1
codesign --verify --deep --strict "$APP" 2>/dev/null && FIXTURE_OK=0
lipo -info "$APP/Contents/MacOS/iphone-use-mcp" 2>/dev/null \
    | grep -q "x86_64 arm64" || FIXTURE_OK=0
if [ "$FIXTURE_OK" -eq 1 ]; then
    ok "fixture starts unsigned and universal, like a published release"
else
    notok "fixture does not match a published release — the rest proves nothing"
fi

# 2. The real _inline_sign from install.sh must succeed on it.
#    Sourced by extracting just the function, so the test exercises the shipped
#    code rather than a copy that can drift away from it.
SIGNER="$(sed -n '/^_inline_sign() {$/,/^}$/p' "$REPO/install.sh")"
if [ -z "$SIGNER" ]; then
    notok "could not extract _inline_sign from install.sh"
else
    eval "$SIGNER"
    # `set -e` is what makes this a real reproduction. install.sh runs under
    # `set -eu`, so the first failing codesign aborts the install. Without -e
    # here the function would keep going, the two later codesigns would
    # succeed, and it would return 0 — the test would pass on the very bug it
    # exists to catch. (Confirmed: it did, before this line.)
    #
    # _inline_sign also ends by calling install.sh's own `ok` progress helper,
    # which would land on this harness's `ok` and corrupt the TAP numbering.
    # Both concerns are handled in one subshell; the signing lands on disk
    # either way.
    # NOT inside `if (...)`: POSIX suppresses errexit for any command used as a
    # condition, subshell included, so `set -e` there is silently inert. Run it
    # as a plain command and read $? afterwards.
    (
        set -e
        ok() { :; }
        _inline_sign "$APP"
    ) >/dev/null 2>&1
    SIGN_RC=$?
    if [ "$SIGN_RC" -eq 0 ]; then
        ok "install.sh _inline_sign signs a two-binary bundle"
    else
        notok "install.sh _inline_sign FAILED (rc=$SIGN_RC) — nested helper signed too late"
    fi
fi

# 3. install.sh gates the install on this exact check afterwards.
if codesign --verify --deep --strict "$APP" 2>/dev/null; then
    ok "signed bundle passes codesign --verify --deep --strict"
else
    notok "signed bundle fails the same gate install.sh applies"
fi

# 4. sign.sh must order its loop the same way. Checked structurally, and
#    specifically against the loop that *signs*: sign.sh also has an earlier
#    `for binary in` that only checks the files exist, where order is
#    irrelevant. Targeting the first match would test the wrong loop.
LOOP="$(awk '/^for binary in/ { line = $0; body = "" ; next }
             line != "" { body = body $0 }
             /^done$/ && line != "" { if (body ~ /codesign/) { print line; exit } ; line = "" }' \
        "$REPO/scripts/sign.sh")"
case "$LOOP" in
    "") notok "could not find sign.sh's signing loop" ;;
    *MCP_BINARY*DAEMON_BINARY*) ok "sign.sh signs the nested helper before the main executable" ;;
    *) notok "sign.sh signs the main executable first — same bug: $LOOP" ;;
esac

echo "# passed $PASS, failed $FAIL"
[ "$FAIL" -eq 0 ]
