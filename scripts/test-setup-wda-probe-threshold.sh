#!/usr/bin/env bash
# A busy WDA is not a broken one.
#
# The KeepAlive supervisor used to rebuild on a SINGLE unanswered `/status`
# probe. WDA answers HTTP on the XCTest runner's own runloop, so one `/source`
# over a large tree (6.1s measured for 444 KB) blocks the 4s probe — and the
# supervisor tore down a healthy runner and both relays during exactly the
# heavy reads the agent surface exists to perform.
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

# 5. The loop must still exit IMMEDIATELY when a process or listener is gone:
#    those are unambiguous, and only the HTTP probe got a threshold.
loop="$(awk '
    /KeepAlive mode: holding/ { copying=1 }
    copying { print }
    copying && /^    done$/ { exit }
' "$SETUP")"
printf '%s\n' "$loop" | grep -q '_validate_pid_record .* runner || break' \
    || fail_test "a dead runner no longer exits immediately"
printf '%s\n' "$loop" | grep -q '_verify_loopback_listener .* relay "\$WDA_PORT" \\' \
    || fail_test "a vanished relay listener no longer exits immediately"
printf '%s\n' "$loop" | grep -q '_keepalive_probe_verdict || break' \
    || fail_test "the HTTP probe is not going through the threshold"
printf '%s\n' "$loop" | grep -q 'curl -fsS -m 4' \
    && fail_test "the loop still probes inline, bypassing the threshold"
pass "process death and a missing listener still exit at once"

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
