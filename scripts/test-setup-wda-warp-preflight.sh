#!/usr/bin/env bash
# Deterministic mocks for setup-wda's read-only WARP/CoreDevice preflight.
set -euo pipefail
umask 077

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SETUP="$ROOT/scripts/setup-wda.sh"
TMP_ROOT_RAW="$(mktemp -d "${TMPDIR:-/tmp}/iphone-use-warp-test.XXXXXX")"
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

TEST_HOME="$TMP_ROOT/home"
TEST_BIN="$TMP_ROOT/bin"
WARP_DUMP="$TMP_ROOT/warp-dump.txt"
mkdir -p "$TEST_HOME" "$TEST_BIN"

cat > "$TEST_BIN/warp-cli" <<'SH'
#!/bin/sh
case "${1:-}" in
    status)
        printf 'Status update: %s\n' "${WARP_TEST_STATUS:-Disconnected}"
        ;;
    settings)
        printf '(network policy)\tMode: %s\n' "${WARP_TEST_MODE:-WarpWithDnsOverHttps}"
        ;;
    tunnel)
        [ "${2:-}" = "dump" ] || exit 64
        [ "${WARP_DUMP_FAIL:-0}" != "1" ] || exit 1
        /bin/cat "$WARP_DUMP"
        ;;
    *)
        exit 64
        ;;
esac
SH
chmod 700 "$TEST_BIN/warp-cli"

run_check() {
    env \
        HOME="$TEST_HOME" \
        WARP_DUMP="$WARP_DUMP" \
        WARP_DUMP_FAIL="${WARP_DUMP_FAIL:-0}" \
        WARP_TEST_MODE="${WARP_TEST_MODE:-WarpWithDnsOverHttps}" \
        WARP_TEST_STATUS="${WARP_TEST_STATUS:-Disconnected}" \
        IPHONE_USE_INTERNAL_TEST_WARP_CLI="$TEST_BIN/warp-cli" \
        IPHONE_USE_INTERNAL_TEST_WARP_PREFLIGHT_ONLY=1 \
        /bin/bash "$SETUP" doctor
}

WARP_TEST_STATUS=Disconnected
export WARP_TEST_STATUS
printf '' > "$WARP_DUMP"
run_check >"$TMP_ROOT/out" 2>"$TMP_ROOT/err" \
    || fail_test "disconnected WARP was rejected"
grep -q 'WARP: off / not present' "$TMP_ROOT/out" \
    || fail_test "disconnected state was not explained"
pass "disconnected WARP passes without a route dump"

WARP_TEST_STATUS=Connected
export WARP_TEST_STATUS
WARP_TEST_MODE="WarpProxy on port 40000"
export WARP_TEST_MODE
WARP_DUMP_FAIL=1
export WARP_DUMP_FAIL
run_check >"$TMP_ROOT/out" 2>"$TMP_ROOT/err" \
    || fail_test "Local proxy mode was rejected"
grep -q 'connected in Local proxy mode' "$TMP_ROOT/out" \
    || fail_test "Local proxy safety was not explained"
pass "connected Local proxy mode passes without inspecting tunnel routes"

WARP_TEST_MODE=WarpWithDnsOverHttps
export WARP_TEST_MODE
WARP_DUMP_FAIL=0
export WARP_DUMP_FAIL
cat > "$WARP_DUMP" <<'EOF'
Excluded:
  fe80::/10
  169.254.0.0/16
Included:
EOF
if run_check >"$TMP_ROOT/out" 2>"$TMP_ROOT/err"; then
    fail_test "connected WARP without the RSD ULA exclusion unexpectedly passed"
fi
grep -q 'fd00::/8' "$TMP_ROOT/out" \
    || fail_test "missing ULA route was not named"
grep -q 'did not change WARP or organization policy' "$TMP_ROOT/out" \
    || fail_test "non-mutation guarantee was omitted"
pass "connected WARP without fd00::/8 fails with an actionable route"

cat > "$WARP_DUMP" <<'EOF'
Excluded:
  fe80::/10
  fd00::/8
  169.254.0.0/16
Included:
EOF
run_check >"$TMP_ROOT/out" 2>"$TMP_ROOT/err" \
    || fail_test "complete CoreDevice exclusions were rejected"
grep -q 'connected with CoreDevice Split Tunnel exclusions' "$TMP_ROOT/out" \
    || fail_test "successful WARP bypass was not explained"
pass "connected WARP passes with fe80::/10 and fd00::/8 excluded"

cat > "$WARP_DUMP" <<'EOF'
Excluded:
  fe80::/10
  fc00::/7
Included:
EOF
run_check >"$TMP_ROOT/out" 2>"$TMP_ROOT/err" \
    || fail_test "broader ULA exclusion was rejected"
pass "fc00::/7 is accepted as a sufficient ULA exclusion"

cat > "$WARP_DUMP" <<'EOF'
Excluded:
  fe80::/10
Included:
  fd00::/8
EOF
if run_check >"$TMP_ROOT/out" 2>"$TMP_ROOT/err"; then
    fail_test "an included ULA route was mistaken for an exclusion"
fi
pass "only routes inside the effective Excluded section are trusted"

WARP_DUMP_FAIL=1
export WARP_DUMP_FAIL
if run_check >"$TMP_ROOT/out" 2>"$TMP_ROOT/err"; then
    fail_test "an unreadable effective route table failed open"
fi
grep -q 'fd00::/8' "$TMP_ROOT/out" \
    || fail_test "route inspection failure lacked recovery guidance"
pass "effective route inspection failure remains fail-closed"

printf '1..%d\n' "$pass_count"
