#!/usr/bin/env bash
# One unanswered probe is not proof that WDA is gone.
#
# The KeepAlive supervisor used to rebuild on a SINGLE unanswered `/status`
# probe, with a 4s timeout. A read of a large element tree was measured at 6.1s
# on this hardware, so a probe issued during one may well go unanswered while
# nothing is actually wrong — one 4s timeout cannot tell a brief non-answer
# from a dead WDA, and the supervisor tore down a healthy runner and both
# relays on the strength of it.
#
# These checks drive the production decision function directly (extracted the
# same way the other setup-wda fixtures do), with `curl` stubbed. No phone, no
# WDA, no launchd.
set -euo pipefail
umask 077

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SETUP="$ROOT/scripts/setup-wda.sh"
TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/iphone-use-probe-test.XXXXXX")"
trap 'rm -rf "$TMP_ROOT"' EXIT

fail_test() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf 'ok: %s\n' "$*"; }

awk '
    /^_keepalive_probe_verdict\(\)/ { copying=1 }
    copying { print }
    copying && /^}/ { exit }
' "$SETUP" > "$TMP_ROOT/probe-verdict.sh"
[ -s "$TMP_ROOT/probe-verdict.sh" ] \
    || fail_test "could not extract _keepalive_probe_verdict from setup-wda.sh"

INFO_LOG="$TMP_ROOT/info.log"
: > "$INFO_LOG"
info() { printf '%s\n' "$*" >> "$INFO_LOG"; }
TARGET_URL="http://127.0.0.1:0"
# The production value, read from the script rather than repeated here.
KEEPALIVE_PROBE_MAX_FAILURES="$(sed -n 's/^    KEEPALIVE_PROBE_MAX_FAILURES=\([0-9]*\)$/\1/p' "$SETUP")"
[ "$KEEPALIVE_PROBE_MAX_FAILURES" = 3 ] \
    || fail_test "expected a fixed threshold of 3, found '${KEEPALIVE_PROBE_MAX_FAILURES}'"
KEEPALIVE_PROBE_FAILURES=0
# Stubbed transport: `CURL_RESULT` decides whether the probe "answered".
CURL_RESULT=0
curl() { return "$CURL_RESULT"; }
# shellcheck source=/dev/null
. "$TMP_ROOT/probe-verdict.sh"

# NOTE: deliberately not `verdict="$(probe 1)"` — a command substitution runs
# in a subshell, where the failure counter this is meant to observe would be
# incremented and then thrown away.
PROBE_VERDICT=""
probe() {
    CURL_RESULT="$1"
    if _keepalive_probe_verdict; then PROBE_VERDICT=hold; else PROBE_VERDICT=rebuild; fi
}

# 1. A busy phone: two unanswered probes must NOT rebuild.
probe 1
[ "$PROBE_VERDICT" = hold ] || fail_test "one unanswered probe already asked for a rebuild"
probe 1
[ "$PROBE_VERDICT" = hold ] || fail_test "two unanswered probes already asked for a rebuild"
[ "$KEEPALIVE_PROBE_FAILURES" = 2 ] || fail_test "consecutive failures were not counted"
pass "a single slow read does not tear down a healthy runner"

# 2. Recovery clears the count: the next failure starts from one again.
probe 0
[ "$PROBE_VERDICT" = hold ] || fail_test "a successful probe asked for a rebuild"
[ "$KEEPALIVE_PROBE_FAILURES" = 0 ] || fail_test "a success did not clear the failure count"
probe 1
[ "$PROBE_VERDICT" = hold ] || fail_test "a failure after recovery rebuilt immediately"
[ "$KEEPALIVE_PROBE_FAILURES" = 1 ] || fail_test "the count did not restart at one"
pass "one answered probe clears the count, so slow reads never accumulate"

# 3. Genuinely unreachable: the third consecutive failure rebuilds.
probe 1
[ "$PROBE_VERDICT" = hold ] || fail_test "the second consecutive failure rebuilt too early"
probe 1
[ "$PROBE_VERDICT" = rebuild ] || fail_test "three consecutive failures did not ask for a rebuild"
pass "three consecutive non-answers still rebuild"

# 4. While holding, the operator is told what is happening — and told it is a
#    count, not a diagnosis.
grep -q "1/3" "$INFO_LOG" || fail_test "the held probe did not report its progress"
grep -q "runner and relays are alive" "$INFO_LOG" \
    || fail_test "the held probe did not report that the processes are alive"
pass "holding is reported with the count, not as a verdict"

# 5. Process death and a missing listener must still exit AT ONCE. Driven by
#    running the production loop under stubs, because grepping its source only
#    shows the code is shaped right, not that it behaves right.
awk '
    /^    while :; do$/ { copying=1 }
    copying { print }
    copying && /^    done$/ { exit }
' "$SETUP" > "$TMP_ROOT/keepalive-loop.sh"
grep -q "_keepalive_probe_verdict || break" "$TMP_ROOT/keepalive-loop.sh" \
    || fail_test "could not extract the KeepAlive loop from setup-wda.sh"

run_loop() {
    # $1 runner alive, $2 relay listening, $3 mjpeg listening
    (
        RUNNER_OK="$1"; RELAY_OK="$2"; MJPEG_OK="$3"
        PROBE_CALLS=0
        VALIDATED_PID=1234
        KEEPALIVE_PROBE_MAX_FAILURES=3
        KEEPALIVE_PROBE_FAILURES=0
        RUNNER_PID_FILE=; LEGACY_RUNNER_EXPECTED=; RELAY_PID_FILE=
        LEGACY_RELAY_EXPECTED=; MJPEG_RELAY_PID_FILE=; LEGACY_MJPEG_EXPECTED=
        WDA_PORT=8100; MJPEG_PORT=9100
        info() { :; }
        _validate_pid_record() { [ "$RUNNER_OK" = 1 ]; }
        _verify_loopback_listener() {
            case "$3" in
                relay) [ "$RELAY_OK" = 1 ] ;;
                mjpeg) [ "$MJPEG_OK" = 1 ] ;;
                *) return 1 ;;
            esac
        }
        _keepalive_probe_verdict() {
            PROBE_CALLS=$((PROBE_CALLS + 1))
            return 1   # would ask for a rebuild, so the loop cannot spin
        }
        sleep() { :; }
        # shellcheck source=/dev/null
        . "$TMP_ROOT/keepalive-loop.sh"
        printf '%s %s\n' "$KEEPALIVE_EXIT_CAUSE" "$PROBE_CALLS"
    )
}

result="$(run_loop 0 1 1)"
[ "$result" = "runner 0" ] \
    || fail_test "a dead runner did not exit at once with cause=runner (got '$result')"
result="$(run_loop 1 0 1)"
[ "$result" = "relay 0" ] \
    || fail_test "a missing relay listener did not exit at once with cause=relay (got '$result')"
result="$(run_loop 1 1 0)"
[ "$result" = "relay 0" ] \
    || fail_test "a missing mjpeg listener did not exit at once (got '$result')"
result="$(run_loop 1 1 1)"
[ "$result" = "unreachable 1" ] \
    || fail_test "a healthy set of processes did not reach the HTTP probe (got '$result')"
pass "process death and a missing listener exit before any HTTP probe is made"

# 6. The rebuild message reports the observation and does not assert a cause.
verdict="$(awk '
    /^        unreachable\)$/ { copying=1 }
    copying { print }
    copying && /^            ;;$/ { exit }
' "$SETUP")"
printf '%s\n' "$verdict" | grep -q 'times in a row' \
    || fail_test "the rebuild message does not report the consecutive count"
printf '%s\n' "$verdict" | grep -q 'most likely changed' \
    && fail_test "the rebuild message still asserts a Wi-Fi address change as the cause"
printf '%s\n' "$verdict" | grep -q 'the USB tunnel dropped;' \
    && fail_test "the rebuild message still asserts a dropped USB tunnel as the cause"
printf '%s\n' "$verdict" | grep -q 'Possible causes' \
    && fail_test "the rebuild message still speculates about causes"
pass "the rebuild message states what was observed, not which cause it was"

printf '\nall probe-threshold checks passed\n'
