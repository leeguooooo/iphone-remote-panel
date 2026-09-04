#!/usr/bin/env bash
# Deterministic icon compilation/signing/rollback checks; no iPhone required.
# shellcheck disable=SC1091,SC2034,SC2329
# The test dynamically extracts production helpers, so ShellCheck cannot see
# their reads or the eval-time call used to prove the `none` branch is inert.
set -euo pipefail
umask 077

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SETUP="$ROOT/scripts/setup-wda.sh"
TMP_ROOT_RAW="$(mktemp -d "${TMPDIR:-/tmp}/iphone-use-icon-test.XXXXXX")"
TMP_ROOT="$(cd -P "$TMP_ROOT_RAW" && pwd)"

cleanup() {
    rm -rf "$TMP_ROOT"
}
trap cleanup EXIT INT TERM

pass_count=0
pass() {
    pass_count=$((pass_count + 1))
    printf 'ok %d - %s\n' "$pass_count" "$1"
}
fail_test() {
    printf 'not ok - %s\n' "$1" >&2
    exit 1
}

TEST_BIN="$TMP_ROOT/bin"
STATE_DIR="$TMP_ROOT/state"
WDA_DIR="$TMP_ROOT/wda"
SIGN_LOG="$TMP_ROOT/codesign.log"
mkdir -p "$TEST_BIN" "$STATE_DIR" "$WDA_DIR"

# Load the exact production helpers without running setup-wda's command body.
awk '
    /^_cleanup_wda_icon_work_dir\(\)/ { copying=1 }
    copying && /^if \[ "\$COMMAND" = "setup" \]; then/ { exit }
    copying { print }
' "$SETUP" > "$TMP_ROOT/icon-transaction-functions.sh"
awk '
    /^_runner_icon_fail\(\)/ { copying=1 }
    copying && /^if \[ -z "\$\{WDA_UDID:-\}" \]; then/ { exit }
    copying { print }
' "$SETUP" > "$TMP_ROOT/icon-injection-functions.sh"
[ -s "$TMP_ROOT/icon-transaction-functions.sh" ] \
    && [ -s "$TMP_ROOT/icon-injection-functions.sh" ] \
    || fail_test "could not isolate the production icon helpers"

info() { printf 'info: %s\n' "$*"; }
ok() { printf 'ok: %s\n' "$*"; }
warn() { printf 'warn: %s\n' "$*"; }
WDA_ICON_WORK_DIR=""
WDA_ICON_PRODUCTS_DIR=""
WDA_ICON_APP_PATH=""
WDA_ICON_BACKUP_PATH=""
WDA_ICON_MUTATION_ACTIVE=0
WDA_RUNNER_ICON_INJECTED=0
WDA_ICON_BUILD_LOCKED=0
WDA_RUNNER_NAME=iPhoneUse
WDA_UDID=00008110-001234567890001E
TEAM_ID=ABCDE12345
WDA_BUNDLE_ID=com.example.wda
. "$TMP_ROOT/icon-transaction-functions.sh"
. "$TMP_ROOT/icon-injection-functions.sh"

cat > "$TEST_BIN/iconutil" <<'SH'
#!/usr/bin/env bash
set -eu
output=""
while [ "$#" -gt 0 ]; do
    if [ "$1" = "-o" ]; then output="$2"; shift 2; else shift; fi
done
[ -n "$output" ]
mkdir -p "$output"
printf 'fake 1024 png\n' > "$output/icon_512x512@2x.png"
SH

cat > "$TEST_BIN/sips" <<'SH'
#!/usr/bin/env bash
set -eu
case " $* " in
    *" pixelWidth "*)
        printf '  pixelWidth: 1024\n  pixelHeight: 1024\n'
        ;;
    *" hasAlpha "*)
        printf '  hasAlpha: no\n'
        ;;
    *) exit 64 ;;
esac
SH

cat > "$TEST_BIN/xcrun" <<'SH'
#!/usr/bin/env bash
set -eu
if [ "${1:-}" = "--find" ] && [ "${2:-}" = "actool" ]; then
    printf '%s/actool\n' "$TEST_BIN"
    exit 0
fi
[ "${1:-}" = "actool" ] || exit 64
shift
compiled=""
partial=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        --compile) compiled="$2"; shift 2 ;;
        --output-partial-info-plist) partial="$2"; shift 2 ;;
        *) shift ;;
    esac
done
[ -n "$compiled" ] && [ -n "$partial" ]
mkdir -p "$compiled"
printf 'asset catalog\n' > "$compiled/Assets.car"
printf 'phone icon\n' > "$compiled/AppIcon60x60@2x.png"
printf 'ipad icon\n' > "$compiled/AppIcon76x76@2x~ipad.png"
cat > "$partial" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIcons</key><dict>
    <key>CFBundlePrimaryIcon</key><dict>
      <key>CFBundleIconFiles</key><array><string>AppIcon60x60</string></array>
      <key>CFBundleIconName</key><string>AppIcon</string>
    </dict>
  </dict>
  <key>CFBundleIcons~ipad</key><dict>
    <key>CFBundlePrimaryIcon</key><dict>
      <key>CFBundleIconFiles</key><array><string>AppIcon60x60</string><string>AppIcon76x76</string></array>
    </dict>
  </dict>
</dict></plist>
PLIST
SH

cat > "$TEST_BIN/xcodebuild-stub" <<'SH'
#!/usr/bin/env bash
set -eu
case " $* " in
    *" -showBuildSettings "*)
        printf '[{"target":"WebDriverAgentRunner","buildSettings":{"BUILT_PRODUCTS_DIR":"%s"}}]\n' "$PRODUCTS_DIR"
        exit 0
        ;;
    *" build-for-testing ") ;;
    *) exit 64 ;;
esac
if [ "${XCODE_LOCK_FAIL:-0}" = "1" ]; then
    printf 'Error Domain=com.apple.dt.deviceprep Code=-3: Unlock iPhone to Continue\n'
    exit 1
fi
app="$PRODUCTS_DIR/iPhoneUse-Runner.app"
rm -rf "$app"
mkdir -p "$app/Frameworks/Fake.framework" "$app/PlugIns/RunnerTests.xctest"
printf 'framework\n' > "$app/Frameworks/Fake.framework/Fake"
printf 'dylib\n' > "$app/Frameworks/libFake.dylib"
printf 'tests\n' > "$app/PlugIns/RunnerTests.xctest/RunnerTests"
printf 'pristine\n' > "$app/original-marker.txt"
cat > "$app/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>CFBundleName</key><string>iPhoneUse-Runner</string></dict></plist>
PLIST
SH

cat > "$TEST_BIN/codesign" <<'SH'
#!/usr/bin/env bash
set -eu
if [ "${1:-}" = "-dvv" ]; then
    printf 'Authority=Apple Development: Fixture (ABCDE12345)\n' >&2
    exit 0
fi
if [ "${1:-}" = "-d" ] && [ "${2:-}" = "--entitlements" ]; then
    cat <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>application-identifier</key><string>ABCDE12345.com.example.wda</string></dict></plist>
PLIST
    exit 0
fi
target=""
for argument in "$@"; do target="$argument"; done
case " $* " in
    *" --verify "*)
        printf 'verify:%s\n' "$target" >> "$SIGN_LOG"
        exit 0
        ;;
    *" -f "*)
        printf 'sign:%s\n' "$target" >> "$SIGN_LOG"
        if [ "${CODE_SIGN_FAIL_ON_OUTER:-0}" = "1" ]; then
            case "$target" in *-Runner.app) exit 1 ;; esac
        fi
        exit 0
        ;;
    *) exit 64 ;;
esac
SH
chmod 700 "$TEST_BIN"/*

export PATH="$TEST_BIN:/usr/bin:/bin:/usr/sbin:/sbin"
export TEST_BIN SIGN_LOG
XCODEBUILD_BIN="$TEST_BIN/xcodebuild-stub"
ICON_SOURCE="$TMP_ROOT/source.icns"
printf 'fake icns\n' > "$ICON_SOURCE"

PRODUCTS_DIR="$TMP_ROOT/success/Build/Products/Debug-iphoneos"
export PRODUCTS_DIR
mkdir -p "$PRODUCTS_DIR"
: > "$SIGN_LOG"
CODE_SIGN_FAIL_ON_OUTER=0
export CODE_SIGN_FAIL_ON_OUTER
_build_and_inject_runner_icon "$ICON_SOURCE" >"$TMP_ROOT/success.out" \
    || fail_test "icon injection failed with complete fake actool output"
APP="$PRODUCTS_DIR/iPhoneUse-Runner.app"
for artifact in Assets.car AppIcon60x60@2x.png AppIcon76x76@2x~ipad.png; do
    [ -f "$APP/$artifact" ] || fail_test "injected app is missing $artifact"
done
[ "$(/usr/libexec/PlistBuddy \
    -c 'Print :CFBundleIcons:CFBundlePrimaryIcon:CFBundleIconName' \
    "$APP/Info.plist")" = "AppIcon" ] \
    || fail_test "partial plist metadata was not merged"
pass "actool outputs and CFBundleIconName are injected together"

xctest_line="$(sed -n '/^sign:.*\/PlugIns\/.*\.xctest$/=' "$SIGN_LOG" | head -1)"
outer_line="$(sed -n '/^sign:.*\/iPhoneUse-Runner\.app$/=' "$SIGN_LOG" | head -1)"
[ -n "$xctest_line" ] && [ -n "$outer_line" ] && [ "$xctest_line" -lt "$outer_line" ] \
    || fail_test "outer runner was signed before its xctest bundle"
pass "nested xctest signing precedes the outer runner signature"

PRODUCTS_DIR="$TMP_ROOT/rollback/Build/Products/Debug-iphoneos"
export PRODUCTS_DIR
mkdir -p "$PRODUCTS_DIR"
: > "$SIGN_LOG"
CODE_SIGN_FAIL_ON_OUTER=1
export CODE_SIGN_FAIL_ON_OUTER
if _build_and_inject_runner_icon "$ICON_SOURCE" >"$TMP_ROOT/rollback.out"; then
    fail_test "forced outer-signature failure unexpectedly succeeded"
fi
APP="$PRODUCTS_DIR/iPhoneUse-Runner.app"
[ "$(cat "$APP/original-marker.txt")" = "pristine" ] \
    || fail_test "failed injection did not restore the pristine app"
[ ! -e "$APP/Assets.car" ] \
    || fail_test "failed injection left a modified asset catalog behind"
if /usr/libexec/PlistBuddy \
    -c 'Print :CFBundleIcons:CFBundlePrimaryIcon:CFBundleIconName' \
    "$APP/Info.plist" >/dev/null 2>&1; then
    fail_test "failed injection left icon metadata behind"
fi
codesign --verify --deep --strict "$APP" \
    || fail_test "restored runner did not pass signature verification"
[ "$WDA_ICON_MUTATION_ACTIVE" = "0" ] \
    || fail_test "rollback left the icon transaction active"
pass "signature failure restores and verifies the pristine runner"

PRODUCTS_DIR="$TMP_ROOT/locked/Build/Products/Debug-iphoneos"
export PRODUCTS_DIR
mkdir -p "$PRODUCTS_DIR"
WDA_ICON_BUILD_LOCKED=0
WDA_KEEPALIVE=1
XCODE_LOCK_FAIL=1
export WDA_KEEPALIVE XCODE_LOCK_FAIL
if _build_and_inject_runner_icon "$ICON_SOURCE" >"$TMP_ROOT/locked.out"; then
    fail_test "lock-blocked build-for-testing unexpectedly succeeded"
fi
[ "$WDA_ICON_BUILD_LOCKED" = "1" ] \
    || fail_test "lock-blocked prebuild did not hand off to lock backoff"
[ -z "$WDA_ICON_WORK_DIR" ] \
    || fail_test "lock-blocked prebuild retained its temporary assets"
pass "lock-blocked prebuild skips the second xcodebuild attempt in that cycle"
unset WDA_KEEPALIVE XCODE_LOCK_FAIL

SKIP_MARKER="$TMP_ROOT/none-called"
export SKIP_MARKER
selection_block="$(awk '
    /^RUNNER_ICON_SOURCE=""/ { copying=1 }
    copying && /^RUNNER_COMMAND=/ { exit }
    copying { print }
' "$SETUP")"
[ -n "$selection_block" ] || fail_test "could not isolate WDA_RUNNER_ICON selection"
(
    WDA_RUNNER_ICON=none
    WDA_ICON_BUILD_LOCKED=0
    _build_and_inject_runner_icon() { : > "$SKIP_MARKER"; }
    eval "$selection_block"
)
[ ! -e "$SKIP_MARKER" ] \
    || fail_test "WDA_RUNNER_ICON=none still entered build or injection"
pass "WDA_RUNNER_ICON=none skips build and injection completely"

printf '1..%d\n' "$pass_count"
