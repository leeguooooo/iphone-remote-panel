#!/usr/bin/env bash
# Deterministic mocks for setup-wda's read-only macOS system-proxy preflight.
set -euo pipefail
umask 077

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SETUP="$ROOT/scripts/setup-wda.sh"
TMP_ROOT_RAW="$(mktemp -d "${TMPDIR:-/tmp}/iphone-use-proxy-test.XXXXXX")"
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
SCUTIL_FIXTURE="$TMP_ROOT/scutil.txt"
PROBE_LOG="$TMP_ROOT/probe.log"
mkdir -p "$TEST_HOME" "$TEST_BIN"

cat > "$TEST_BIN/scutil" <<'SH'
#!/bin/sh
[ "${1:-}" = "--proxy" ] || exit 64
[ "${SCUTIL_FAIL:-0}" != "1" ] || exit 1
/bin/cat "$SCUTIL_FIXTURE"
SH
cat > "$TEST_BIN/probe" <<'SH'
#!/bin/sh
printf '%s:%s\n' "$1" "$2" >> "$PROBE_LOG"
exit "${PROBE_RESULT:-1}"
SH
chmod 700 "$TEST_BIN/scutil" "$TEST_BIN/probe"

run_check() {
    : > "$PROBE_LOG"
    env \
        HOME="$TEST_HOME" \
        SCUTIL_FIXTURE="$SCUTIL_FIXTURE" \
        SCUTIL_FAIL="${SCUTIL_FAIL:-0}" \
        PROBE_LOG="$PROBE_LOG" \
        PROBE_RESULT="${PROBE_RESULT:-1}" \
        IPHONE_USE_INTERNAL_TEST_PROXY_PREFLIGHT_ONLY=1 \
        IPHONE_USE_INTERNAL_TEST_SCUTIL="$TEST_BIN/scutil" \
        IPHONE_USE_INTERNAL_TEST_PROXY_PROBE="$TEST_BIN/probe" \
        /bin/bash "$SETUP" doctor
}

cat > "$SCUTIL_FIXTURE" <<'EOF'
<dictionary> {
  ExceptionsList : <array> {
    0 : *.local
  }
  HTTPEnable : 0
  HTTPSEnable : 0
  SOCKSEnable : 0
}
EOF
run_check >"$TMP_ROOT/out" 2>"$TMP_ROOT/err" \
    || fail_test "disabled fixed proxies were rejected"
grep -q 'System proxies (HTTP/HTTPS/SOCKS): none enabled' "$TMP_ROOT/out" \
    || fail_test "disabled-proxy result was not explained"
[ ! -s "$PROBE_LOG" ] || fail_test "disabled proxies triggered a TCP probe"
pass "disabled HTTP/HTTPS/SOCKS entries pass without probing"

cat > "$SCUTIL_FIXTURE" <<'EOF'
<dictionary> {
  HTTPEnable : 1
  HTTPPort : 8899
  HTTPProxy : 127.0.0.1
  HTTPSEnable : 1
  HTTPSPort : 8899
  HTTPSProxy : 127.0.0.1
  SOCKSEnable : 0
}
EOF
PROBE_RESULT=1
export PROBE_RESULT
if run_check >"$TMP_ROOT/out" 2>"$TMP_ROOT/err"; then
    fail_test "dead loopback proxies unexpectedly passed"
fi
grep -q 'No reachable TCP listener at configured loopback endpoints' "$TMP_ROOT/out" \
    || fail_test "dead loopback listener was not identified"
grep -q 'HTTP 127.0.0.1:8899' "$TMP_ROOT/out" \
    || fail_test "dead HTTP endpoint was omitted"
grep -q 'HTTPS 127.0.0.1:8899' "$TMP_ROOT/out" \
    || fail_test "dead HTTPS endpoint was omitted"
grep -q 'did not change proxy settings' "$TMP_ROOT/out" \
    || fail_test "non-mutation guarantee was omitted"
[ "$(wc -l < "$PROBE_LOG" | tr -d '[:space:]')" = "2" ] \
    || fail_test "enabled loopback endpoints were not both probed"
pass "dead 127.0.0.1:8899 HTTP/HTTPS settings fail with scoped recovery"

PROBE_RESULT=0
export PROBE_RESULT
run_check >"$TMP_ROOT/out" 2>"$TMP_ROOT/err" \
    || fail_test "reachable loopback proxy was treated as a proven blocker"
grep -q 'TCP listener responds' "$TMP_ROOT/out" \
    || fail_test "reachable local endpoint was not reported"
grep -q 'not automatically treated as a blocker' "$TMP_ROOT/out" \
    || fail_test "reachable proxy caveat was omitted"
pass "reachable loopback proxy is reported without over-diagnosing it"

cat > "$SCUTIL_FIXTURE" <<'EOF'
<dictionary> {
  HTTPEnable : 0
  HTTPSEnable : 0
  SOCKSEnable : 1
  SOCKSPort : 1080
  SOCKSProxy : 127.example.com
}
EOF
PROBE_RESULT=1
export PROBE_RESULT
run_check >"$TMP_ROOT/out" 2>"$TMP_ROOT/err" \
    || fail_test "remote SOCKS proxy was treated as a proven blocker"
grep -q 'SOCKS system proxy enabled at 127.example.com:1080 (endpoint not probed)' \
    "$TMP_ROOT/out" \
    || fail_test "remote SOCKS diagnostic was omitted"
[ ! -s "$PROBE_LOG" ] \
    || fail_test "remote proxy received a potentially misleading local reachability probe"
pass "127-prefixed hostname remains a remote diagnostic variable"

cat > "$SCUTIL_FIXTURE" <<'EOF'
<dictionary> {
  HTTPEnable : 1
  HTTPProxy : 127.0.0.1
  HTTPSEnable : 0
}
EOF
if run_check >"$TMP_ROOT/out" 2>"$TMP_ROOT/err"; then
    fail_test "malformed enabled proxy unexpectedly passed"
fi
grep -q 'Invalid or incomplete entries: HTTP' "$TMP_ROOT/out" \
    || fail_test "malformed proxy was not identified without echoing unsafe data"
pass "enabled proxy with an invalid host/port is a concrete blocker"

cat > "$SCUTIL_FIXTURE" <<'EOF'
<dictionary> {
  HTTPEnable : 0
  HTTPSEnable : 0
  __SCOPED__ : <dictionary> {
    en0 : <dictionary> {
      HTTPEnable : 1
      HTTPPort : 8899
      HTTPProxy : 127.0.0.1
    }
  }
}
EOF
run_check >"$TMP_ROOT/out" 2>"$TMP_ROOT/err" \
    || fail_test "nested scoped proxy metadata overrode the top-level state"
[ ! -s "$PROBE_LOG" ] \
    || fail_test "nested scoped proxy metadata triggered a TCP probe"
grep -q 'none enabled' "$TMP_ROOT/out" \
    || fail_test "top-level active-state result was not reported"
pass "nested scoped proxy metadata does not override global active state"

printf '' > "$SCUTIL_FIXTURE"
if run_check >"$TMP_ROOT/out" 2>"$TMP_ROOT/err"; then
    fail_test "empty successful scutil output unexpectedly passed"
fi
grep -q 'Could not inspect macOS HTTP/HTTPS/SOCKS proxy state' "$TMP_ROOT/out" \
    || fail_test "empty scutil output was misreported as no enabled proxies"
pass "empty or structurally invalid scutil output fails inspection"

SCUTIL_FAIL=1
export SCUTIL_FAIL
if run_check >"$TMP_ROOT/out" 2>"$TMP_ROOT/err"; then
    fail_test "failed System Configuration inspection unexpectedly passed"
fi
grep -q 'Could not inspect macOS HTTP/HTTPS/SOCKS proxy state' "$TMP_ROOT/out" \
    || fail_test "scutil inspection failure was not actionable"
pass "proxy inspection failure is explicit and remains read-only"

printf '1..%d\n' "$pass_count"
