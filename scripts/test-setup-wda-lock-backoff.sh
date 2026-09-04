#!/usr/bin/env bash
# Deterministic checks for setup-wda lock backoff and pause/resume policy.
# shellcheck disable=SC1091,SC2034
# Two production helpers are extracted dynamically so their state transitions
# can be tested without running the setup command body.
set -euo pipefail
umask 077

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SETUP="$ROOT/scripts/setup-wda.sh"
TMP_ROOT_RAW="$(mktemp -d "${TMPDIR:-/tmp}/iphone-use-lifecycle-test.XXXXXX")"
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
STATE_DIR="$TEST_HOME/.iphone-use"
TEST_BIN="$TMP_ROOT/bin"
LAUNCH_STATE="$TMP_ROOT/launch-state"
LAUNCH_LOG="$TMP_ROOT/launch.log"
mkdir -p "$STATE_DIR" "$TEST_HOME/Library/LaunchAgents" "$TEST_BIN"

retry_field() {
    sed -n "$1" "$STATE_DIR/wda-retry-state.v1"
}
run_retry() {
    env HOME="$TEST_HOME" \
        IPHONE_USE_INTERNAL_TEST_KEEPALIVE_RETRY_KIND="$1" \
        /bin/bash "$SETUP" doctor
}
assert_delay_near() {
    local expected="$1"
    local next_at now delta
    next_at="$(retry_field '4s/^next_at=//p')"
    now="$(date +%s)"
    delta=$((next_at - now))
    [ "$delta" -ge $((expected - 2)) ] && [ "$delta" -le $((expected + 1)) ] \
        || fail_test "expected a ${expected}s retry delay, found ${delta}s"
}

run_retry generic >"$TMP_ROOT/retry.out"
[ "$(retry_field '2s/^kind=//p')" = "generic" ] \
    || fail_test "first generic failure did not persist its kind"
[ "$(retry_field '3s/^attempt=//p')" = "1" ] \
    || fail_test "first generic failure did not start at attempt 1"
assert_delay_near 5
run_retry generic >"$TMP_ROOT/retry.out"
[ "$(retry_field '3s/^attempt=//p')" = "2" ] \
    || fail_test "generic retry attempt did not increment"
assert_delay_near 10
for _ in 3 4 5 6 7 8; do
    run_retry generic >"$TMP_ROOT/retry.out"
done
assert_delay_near 300
pass "generic KeepAlive failures back off from 5 seconds and cap at 5 minutes"

run_retry locked >"$TMP_ROOT/locked-first.out"
[ "$(retry_field '2s/^kind=//p')" = "locked" ] \
    || fail_test "lock failure did not replace the generic retry kind"
[ "$(retry_field '3s/^attempt=//p')" = "1" ] \
    || fail_test "switching to lock backoff did not reset the attempt"
assert_delay_near 30
grep -q 'lock screen blocked WDA' "$TMP_ROOT/locked-first.out" \
    || fail_test "first lock transition was not reported"
run_retry locked >"$TMP_ROOT/locked-second.out"
[ "$(retry_field '3s/^attempt=//p')" = "2" ] \
    || fail_test "lock retry attempt did not increment"
assert_delay_near 60
if grep -q 'lock screen blocked WDA' "$TMP_ROOT/locked-second.out"; then
    fail_test "unchanged lock state repeated the user prompt"
fi
for _ in 3 4 5 6 7; do
    run_retry locked >"$TMP_ROOT/locked-repeat.out"
done
assert_delay_near 900
for _ in 8 9 10; do
    run_retry locked >"$TMP_ROOT/locked-repeat.out"
done
[ "$(retry_field '3s/^attempt=//p')" = "10" ] \
    || fail_test "lock retry stopped advancing after reaching its delay cap"
assert_delay_near 900
pass "lock failures retry indefinitely and cap at 15 minutes without repeated prompts"

locked_helper="$(awk '
    /^_prepare_locked_retry\(\)/ { copying=1 }
    copying { print }
    copying && /^}/ { exit }
' "$SETUP")"
printf '%s\n' "$locked_helper" | grep -q 'KEEPALIVE_FAILURE_KIND="locked"' \
    || fail_test "lock failures no longer select the locked retry schedule"
if printf '%s\n' "$locked_helper" | grep -Eq 'while|sleep|lockState'; then
    fail_test "lock failure path contains an unbounded wait or false lock-state probe"
fi
pass "lock failures always exit into a scheduled rebuild instead of waiting forever"

awk '
    /^_exponential_retry_delay\(\)/ { copying=1 }
    copying { print }
    copying && /^}/ { exit }
' "$SETUP" > "$TMP_ROOT/exponential-delay.sh"
awk '
    /^_interactive_lock_wait_tick\(\)/ { copying=1 }
    copying { print }
    copying && /^}/ { exit }
' "$SETUP" > "$TMP_ROOT/interactive-lock-tick.sh"
LOCK_PROMPT_LOG="$TMP_ROOT/interactive-lock-prompts.log"
: > "$LOCK_PROMPT_LOG"
warn() { printf '%s\n' "$*" >> "$LOCK_PROMPT_LOG"; }
_setstatus() { :; }
INTERACTIVE_LOCK_STARTED_AT=0
INTERACTIVE_LOCK_NOTICE_AT=0
INTERACTIVE_LOCK_NOTICE_ATTEMPT=0
. "$TMP_ROOT/exponential-delay.sh"
. "$TMP_ROOT/interactive-lock-tick.sh"
_interactive_lock_wait_tick 1000 \
    || fail_test "interactive lock wait timed out immediately"
for now in 1003 1006 1009 1012 1015 1018 1021 1024 1027; do
    _interactive_lock_wait_tick "$now" \
        || fail_test "interactive lock wait timed out before five minutes"
done
[ "$(wc -l < "$LOCK_PROMPT_LOG" | tr -d '[:space:]')" = "1" ] \
    || fail_test "interactive setup repeated the lock prompt on every 3s poll"
_interactive_lock_wait_tick 1030 \
    || fail_test "interactive lock wait timed out at its first reminder"
_interactive_lock_wait_tick 1090 \
    || fail_test "interactive lock wait timed out at its second reminder"
[ "$(wc -l < "$LOCK_PROMPT_LOG" | tr -d '[:space:]')" = "3" ] \
    || fail_test "interactive lock reminders did not reuse exponential intervals"
grep -q 'Ctrl-C' "$LOCK_PROMPT_LOG" \
    || fail_test "interactive lock wait omitted its explicit exit hint"
if _interactive_lock_wait_tick 1300; then
    fail_test "interactive lock wait had no five-minute timeout"
fi
pass "interactive lock prompts back off and exit after five minutes"

run_retry generic >"$TMP_ROOT/retry.out"
[ "$(retry_field '3s/^attempt=//p')" = "1" ] \
    || fail_test "switching away from lock backoff did not reset the attempt"
assert_delay_near 5
run_retry reset >"$TMP_ROOT/retry.out"
[ ! -e "$STATE_DIR/wda-retry-state.v1" ] \
    || fail_test "verified recovery did not clear retry state"
pass "retry schedules reset on recovery or failure-kind change"

printf 'loaded=1\ndisabled=0\n' > "$LAUNCH_STATE"
cat > "$TEST_BIN/launchctl" <<'SH'
#!/usr/bin/env bash
set -eu
. "$LAUNCH_STATE"
printf '%s\n' "$*" >> "$LAUNCH_LOG"
case "${1:-}" in
    print-disabled)
        if [ "$disabled" = "1" ]; then
            printf '"com.leeguoo.iphone-use.wda" => disabled\n'
        else
            printf '"com.leeguoo.iphone-use.wda" => enabled\n'
        fi
        ;;
    disable)
        disabled=1
        printf 'loaded=%s\ndisabled=%s\n' "$loaded" "$disabled" > "$LAUNCH_STATE"
        ;;
    enable)
        disabled=0
        printf 'loaded=%s\ndisabled=%s\n' "$loaded" "$disabled" > "$LAUNCH_STATE"
        ;;
    bootout)
        loaded=0
        printf 'loaded=%s\ndisabled=%s\n' "$loaded" "$disabled" > "$LAUNCH_STATE"
        ;;
    bootstrap)
        [ "$disabled" = "0" ] || exit 1
        loaded=1
        printf 'loaded=%s\ndisabled=%s\n' "$loaded" "$disabled" > "$LAUNCH_STATE"
        ;;
    print)
        [ "$loaded" = "1" ]
        ;;
    *) exit 64 ;;
esac
SH
chmod 700 "$TEST_BIN/launchctl"

SELF_INSTALL="$STATE_DIR/setup-wda.sh"
printf '#!/bin/sh\nexit 0\n' > "$SELF_INSTALL"
chmod 700 "$SELF_INSTALL"
WDA_PLIST="$TEST_HOME/Library/LaunchAgents/com.leeguoo.iphone-use.wda.plist"
cat > "$WDA_PLIST" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>com.leeguoo.iphone-use.wda</string>
  <key>ProgramArguments</key>
  <array><string>/bin/bash</string><string>${SELF_INSTALL}</string></array>
  <key>EnvironmentVariables</key><dict>
    <key>WDA_UDID</key><string>00008110-001234567890001E</string>
    <key>WDA_TEAM_ID</key><string>ABCDE12345</string>
    <key>WDA_BUNDLE_ID</key><string>com.example.wda</string>
  </dict>
  <key>KeepAlive</key><true/>
</dict></plist>
PLIST
chmod 600 "$WDA_PLIST"

run_lifecycle() {
    env HOME="$TEST_HOME" \
        PATH="$TEST_BIN:/usr/bin:/bin:/usr/sbin:/sbin" \
        LAUNCH_STATE="$LAUNCH_STATE" \
        LAUNCH_LOG="$LAUNCH_LOG" \
        /bin/bash "$SETUP" "$1"
}

run_lifecycle pause >"$TMP_ROOT/pause.out" \
    || fail_test "pause failed against a loaded managed supervisor"
loaded="$(sed -n 's/^loaded=//p' "$LAUNCH_STATE")"
disabled="$(sed -n 's/^disabled=//p' "$LAUNCH_STATE")"
[ "$loaded:$disabled" = "0:1" ] \
    || fail_test "pause did not unload and disable the exact supervisor"
grep -q 'WDA paused' "$TMP_ROOT/pause.out" \
    || fail_test "pause did not report the verified result"
pass "pause disables launchd before stopping the managed stack"

if run_lifecycle status >"$TMP_ROOT/status.out" 2>&1; then
    fail_test "status treated an intentional pause as healthy"
fi
grep -q 'WDA is paused' "$TMP_ROOT/status.out" \
    || fail_test "status did not explain the intentional pause"
pass "status distinguishes an intentional pause from an unknown outage"

run_lifecycle resume >"$TMP_ROOT/resume.out" \
    || fail_test "resume failed against a valid managed supervisor plist"
loaded="$(sed -n 's/^loaded=//p' "$LAUNCH_STATE")"
disabled="$(sed -n 's/^disabled=//p' "$LAUNCH_STATE")"
[ "$loaded:$disabled" = "1:0" ] \
    || fail_test "resume did not enable and bootstrap the exact supervisor"
grep -q 'WDA resume requested' "$TMP_ROOT/resume.out" \
    || fail_test "resume did not report the verified launchd handoff"
pass "resume validates, enables, and bootstraps the managed supervisor"

if grep -q 'pkill' "$LAUNCH_LOG"; then
    fail_test "pause/resume used a broad process-name kill"
fi
pass "pause/resume never use broad process-name matching"

printf '1..%d\n' "$pass_count"
