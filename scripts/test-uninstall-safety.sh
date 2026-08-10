#!/usr/bin/env bash
# Focused macOS mocks for uninstall PID identity and WDA checkout ownership.
set -euo pipefail
umask 077

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
UNINSTALL="$ROOT/uninstall.sh"
SETUP="$ROOT/scripts/setup-wda.sh"
TMP_ROOT_RAW="$(mktemp -d "${TMPDIR:-/tmp}/iphone-use-uninstall-test.XXXXXX")"
TMP_ROOT="$(cd -P "$TMP_ROOT_RAW" && pwd)"
SLEEP_PIDS=""

cleanup() {
    local pid
    for pid in $SLEEP_PIDS; do
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
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

new_home() {
    local name="$1"
    TEST_ROOT="$TMP_ROOT/$name"
    TEST_HOME="$TEST_ROOT/home"
    TEST_BIN="$TEST_ROOT/bin"
    mkdir -p "$TEST_HOME/Library/LaunchAgents" "$TEST_HOME/.iphone-use" "$TEST_BIN"
    chmod 700 "$TEST_HOME/Library/LaunchAgents" "$TEST_HOME/.iphone-use"
    cat > "$TEST_BIN/launchctl" <<'SH'
#!/bin/sh
case "${1:-}" in
    print) exit 1 ;;
    bootout|disable|enable|bootstrap|kickstart) exit 0 ;;
    *) exit 0 ;;
esac
SH
    chmod 700 "$TEST_BIN/launchctl"
}

run_uninstall() {
    env \
        HOME="$TEST_HOME" \
        IPHONE_USE_LAUNCHCTL="$TEST_BIN/launchctl" \
        /bin/bash "$UNINSTALL" "$@"
}

git_init_checkout() {
    WDA_DIR="$TEST_HOME/.iphone-use/WebDriverAgent"
    mkdir -p "$WDA_DIR"
    git -C "$WDA_DIR" init -q
    git -C "$WDA_DIR" config user.name "Safety Test"
    git -C "$WDA_DIR" config user.email "safety@example.invalid"
    git -C "$WDA_DIR" remote add origin https://github.com/appium/WebDriverAgent.git
    printf 'fixture\n' > "$WDA_DIR/fixture.txt"
    git -C "$WDA_DIR" add fixture.txt
    git -C "$WDA_DIR" commit -qm fixture
    WDA_HEAD="$(git -C "$WDA_DIR" rev-parse HEAD)"
    WDA_UDID="00008110-001234567890001E"
}

digest_text() {
    printf '%s' "$1" | shasum -a 256 | awk '{print $1}'
}

write_marker() {
    local refs reflog worktrees objects marker
    refs="$(GIT_OPTIONAL_LOCKS=0 git -C "$WDA_DIR" \
        for-each-ref --format='%(refname) %(objectname)')"
    reflog="$(GIT_OPTIONAL_LOCKS=0 git -C "$WDA_DIR" \
        reflog show --all --format='%H %gD %gs')"
    worktrees="$(GIT_OPTIONAL_LOCKS=0 git -C "$WDA_DIR" \
        worktree list --porcelain)"
    objects="$(GIT_OPTIONAL_LOCKS=0 git -C "$WDA_DIR" \
        cat-file --batch-all-objects --batch-check='%(objectname)')"
    objects="$(printf '%s\n' "$objects" | LC_ALL=C sort)"
    marker="$TEST_HOME/.iphone-use/wda-checkout-owner.v1"
    printf '%s\n' \
        "version=1" \
        "path=$WDA_DIR" \
        "origin=https://github.com/appium/WebDriverAgent.git" \
        "head=$WDA_HEAD" \
        "udid=$WDA_UDID" \
        "refs_sha256=$(digest_text "$refs")" \
        "reflog_sha256=$(digest_text "$reflog")" \
        "worktrees_sha256=$(digest_text "$worktrees")" \
        "objects_sha256=$(digest_text "$objects")" > "$marker"
    chmod 600 "$marker"
}

write_wda_plist() {
    local plist="$TEST_HOME/Library/LaunchAgents/com.leeguoo.iphone-use.wda.plist"
    cat > "$plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>com.leeguoo.iphone-use.wda</string>
  <key>ProgramArguments</key>
  <array><string>/bin/bash</string><string>$TEST_HOME/.iphone-use/setup-wda.sh</string></array>
  <key>EnvironmentVariables</key><dict>
    <key>WDA_DIR</key><string>$WDA_DIR</string>
    <key>WDA_REF</key><string>$WDA_HEAD</string>
    <key>WDA_UDID</key><string>$WDA_UDID</string>
    <key>WDA_BUNDLE_ID</key><string>com.example.iphoneuse.wda</string>
  </dict>
</dict></plist>
PLIST
    chmod 600 "$plist"
}

expect_preserved_checkout() {
    if run_uninstall >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
        fail_test "unsafe checkout case unexpectedly returned success"
    fi
    [ -d "$WDA_DIR/.git" ] || fail_test "unsafe checkout was deleted"
}

new_home "empty-idempotent"
run_uninstall >/dev/null
run_uninstall >/dev/null
pass "empty uninstall is idempotent"

new_home "arbitrary-pid"
/bin/sleep 60 &
victim_pid=$!
SLEEP_PIDS="$SLEEP_PIDS $victim_pid"
victim_lstart="$(LC_ALL=C /bin/ps -p "$victim_pid" -o lstart= \
    | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')"
victim_command="$(LC_ALL=C /bin/ps -ww -p "$victim_pid" -o command= \
    | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')"
printf '%s|%s|runner:%s\n' \
    "$victim_pid" "$victim_lstart" "$victim_command" \
    > "$TEST_HOME/.iphone-use/wda-runner.pid"
chmod 600 "$TEST_HOME/.iphone-use/wda-runner.pid"
if run_uninstall --dry-run >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
    fail_test "arbitrary PID record unexpectedly passed dry-run"
fi
kill -0 "$victim_pid" 2>/dev/null || fail_test "arbitrary process was signalled"
grep -q "non-product runner command contract" "$TEST_ROOT/err" \
    || fail_test "arbitrary PID rejection reason missing"
pass "arbitrary same-uid command is never accepted as a WDA runner"

new_home "valid-runner-contract"
WDA_DIR="$TEST_HOME/.iphone-use/WebDriverAgent"
mkdir -p "$WDA_DIR"
runner_command="/usr/bin/xcodebuild -project WebDriverAgent.xcodeproj -scheme WebDriverAgentRunner -destination platform=iOS,id=00008110-001234567890001E -allowProvisioningUpdates DEVELOPMENT_TEAM=ABCD123456 PRODUCT_BUNDLE_IDENTIFIER=com.example.iphoneuse.wda test"
cat > "$TEST_BIN/ps" <<SH
#!/bin/sh
case " \$* " in
  *" -o pid= "*) printf '4242\\n' ;;
  *" -o uid= "*) printf '%s\\n' "$(id -u)" ;;
  *" -o lstart= "*) printf 'Mon Jul 28 00:00:00 2026\\n' ;;
  *" -o command= "*) printf '%s\\n' "$runner_command" ;;
  *) exit 1 ;;
esac
SH
cat > "$TEST_BIN/lsof" <<SH
#!/bin/sh
printf 'p4242\\nn%s\\n' "$WDA_DIR"
SH
chmod 700 "$TEST_BIN/ps" "$TEST_BIN/lsof"
printf '%s\n' "4242|Mon Jul 28 00:00:00 2026|runner:$runner_command" \
    > "$TEST_HOME/.iphone-use/wda-runner.pid"
chmod 600 "$TEST_HOME/.iphone-use/wda-runner.pid"
if env HOME="$TEST_HOME" \
    IPHONE_USE_LAUNCHCTL="$TEST_BIN/launchctl" \
    IPHONE_USE_PS="$TEST_BIN/ps" \
    IPHONE_USE_LSOF="$TEST_BIN/lsof" \
    /bin/bash "$UNINSTALL" --dry-run >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
    fail_test "unmarked checkout should still make the aggregate dry-run fail"
fi
grep -q "send SIGTERM to PID-verified runner process 4242" "$TEST_ROOT/out" \
    || fail_test "valid runner contract did not reach the dry-run signal plan"
pass "valid WDA runner command plus canonical cwd reaches the signal plan"

new_home "wrong-runner-cwd"
WDA_DIR="$TEST_HOME/.iphone-use/WebDriverAgent"
mkdir -p "$WDA_DIR" "$TEST_ROOT/not-wda"
cat > "$TEST_BIN/ps" <<SH
#!/bin/sh
case " \$* " in
  *" -o pid= "*) printf '4242\\n' ;;
  *" -o uid= "*) printf '%s\\n' "$(id -u)" ;;
  *" -o lstart= "*) printf 'Mon Jul 28 00:00:00 2026\\n' ;;
  *" -o command= "*) printf '%s\\n' "$runner_command" ;;
  *) exit 1 ;;
esac
SH
cat > "$TEST_BIN/lsof" <<SH
#!/bin/sh
printf 'p4242\\nn%s\\n' "$TEST_ROOT/not-wda"
SH
chmod 700 "$TEST_BIN/ps" "$TEST_BIN/lsof"
printf '%s\n' "4242|Mon Jul 28 00:00:00 2026|runner:$runner_command" \
    > "$TEST_HOME/.iphone-use/wda-runner.pid"
chmod 600 "$TEST_HOME/.iphone-use/wda-runner.pid"
if env HOME="$TEST_HOME" \
    IPHONE_USE_LAUNCHCTL="$TEST_BIN/launchctl" \
    IPHONE_USE_PS="$TEST_BIN/ps" \
    IPHONE_USE_LSOF="$TEST_BIN/lsof" \
    /bin/bash "$UNINSTALL" --dry-run >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
    fail_test "wrong runner cwd unexpectedly passed"
fi
grep -q "not a WDA runner rooted in the configured checkout" "$TEST_ROOT/err" \
    || fail_test "wrong runner cwd rejection reason missing"
! grep -q "send SIGTERM" "$TEST_ROOT/out" \
    || fail_test "wrong-cwd runner reached a signal plan"
pass "WDA-shaped xcodebuild outside the configured checkout is rejected"

new_home "relay-listener-owner"
relay_command="/usr/local/bin/iproxy -s 127.0.0.1 8100:8100 -u 00008110-001234567890001E"
cat > "$TEST_BIN/ps" <<SH
#!/bin/sh
case " \$* " in
  *" -o pid= "*) printf '4243\\n' ;;
  *" -o uid= "*) printf '%s\\n' "$(id -u)" ;;
  *" -o lstart= "*) printf 'Mon Jul 28 00:00:01 2026\\n' ;;
  *" -o command= "*) printf '%s\\n' "$relay_command" ;;
  *) exit 1 ;;
esac
SH
cat > "$TEST_BIN/lsof" <<'SH'
#!/bin/sh
printf 'p9999\n'
SH
chmod 700 "$TEST_BIN/ps" "$TEST_BIN/lsof"
printf '%s\n' "4243|Mon Jul 28 00:00:01 2026|relay:$relay_command" \
    > "$TEST_HOME/.iphone-use/wda-relay.pid"
chmod 600 "$TEST_HOME/.iphone-use/wda-relay.pid"
if env HOME="$TEST_HOME" \
    IPHONE_USE_LAUNCHCTL="$TEST_BIN/launchctl" \
    IPHONE_USE_PS="$TEST_BIN/ps" \
    IPHONE_USE_LSOF="$TEST_BIN/lsof" \
    /bin/bash "$UNINSTALL" --dry-run >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
    fail_test "relay with a foreign listener owner unexpectedly passed"
fi
grep -q "does not exclusively own 127.0.0.1:8100" "$TEST_ROOT/err" \
    || fail_test "relay listener ownership rejection reason missing"
! grep -q "send SIGTERM" "$TEST_ROOT/out" \
    || fail_test "foreign-listener relay reached a signal plan"
pass "WDA relay must exclusively own its exact loopback listener"

new_home "pid-mode"
/bin/sleep 60 &
victim_pid=$!
SLEEP_PIDS="$SLEEP_PIDS $victim_pid"
victim_lstart="$(LC_ALL=C /bin/ps -p "$victim_pid" -o lstart= \
    | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')"
victim_command="$(LC_ALL=C /bin/ps -ww -p "$victim_pid" -o command= \
    | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')"
printf '%s|%s|runner:%s\n' \
    "$victim_pid" "$victim_lstart" "$victim_command" \
    > "$TEST_HOME/.iphone-use/wda-runner.pid"
chmod 644 "$TEST_HOME/.iphone-use/wda-runner.pid"
if run_uninstall --dry-run >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
    fail_test "world-readable PID record unexpectedly passed dry-run"
fi
kill -0 "$victim_pid" 2>/dev/null || fail_test "process referenced by insecure PID file was signalled"
grep -q "mode 600" "$TEST_ROOT/err" || fail_test "PID mode rejection reason missing"
pass "PID record must be current-uid mode 600"

new_home "state-mode"
chmod 755 "$TEST_HOME/.iphone-use"
if run_uninstall --dry-run >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
    fail_test "insecure state namespace unexpectedly passed dry-run"
fi
grep -q "expected 700" "$TEST_ROOT/err" || fail_test "state mode rejection reason missing"
pass "state namespace must be current-uid mode 700"

new_home "transient-symlink"
printf 'keep\n' > "$TEST_ROOT/outside"
ln -s "$TEST_ROOT/outside" \
    "$TEST_HOME/.iphone-use/wda-checkout-owner.v1.new.attacker"
if run_uninstall >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
    fail_test "known-name transient symlink unexpectedly returned success"
fi
[ -L "$TEST_HOME/.iphone-use/wda-checkout-owner.v1.new.attacker" ] \
    || fail_test "transient symlink itself was removed"
[ "$(sed -n '1p' "$TEST_ROOT/outside")" = "keep" ] \
    || fail_test "transient symlink target was changed"
grep -q "unrecognized or unsafe entries" "$TEST_ROOT/err" \
    || fail_test "unsafe known-name entry rejection reason missing"
pass "known-name transient symlinks are preserved and reported as incomplete"

new_home "unmarked-checkout"
git_init_checkout
expect_preserved_checkout
grep -q "ownership marker is missing" "$TEST_ROOT/err" \
    || fail_test "unmarked checkout rejection reason missing"
pass "legacy/unmarked checkout is preserved"

new_home "marked-checkout"
git_init_checkout
write_marker
write_wda_plist
run_uninstall --dry-run >"$TEST_ROOT/dry-out" 2>"$TEST_ROOT/dry-err" \
    || fail_test "fully marked pristine checkout failed dry-run"
[ -d "$WDA_DIR/.git" ] || fail_test "dry-run deleted the marked checkout"
[ -f "$TEST_HOME/.iphone-use/wda-checkout-owner.v1" ] \
    || fail_test "dry-run deleted the ownership marker"
run_uninstall >"$TEST_ROOT/out" 2>"$TEST_ROOT/err" \
    || fail_test "fully marked pristine checkout did not uninstall"
[ ! -e "$WDA_DIR" ] || fail_test "marked pristine checkout remains"
[ ! -e "$TEST_HOME/.iphone-use" ] || fail_test "empty state directory remains"
run_uninstall >/dev/null
pass "marked checkout dry-run is non-mutating and actual uninstall is idempotent"

new_home "local-commit"
git_init_checkout
write_marker
write_wda_plist
git -C "$WDA_DIR" switch -qc local-safety
printf 'local commit\n' >> "$WDA_DIR/fixture.txt"
git -C "$WDA_DIR" add fixture.txt
git -C "$WDA_DIR" commit -qm local-only
git -C "$WDA_DIR" checkout -q --detach "$WDA_HEAD"
expect_preserved_checkout
grep -q "Git refs changed" "$TEST_ROOT/err" || fail_test "local-ref rejection reason missing"
pass "local unpushed commits/branches after marker are preserved"

new_home "stash"
git_init_checkout
write_marker
write_wda_plist
printf 'stashed change\n' >> "$WDA_DIR/fixture.txt"
git -C "$WDA_DIR" stash push -qm local-safety
expect_preserved_checkout
grep -q "Git refs changed" "$TEST_ROOT/err" || fail_test "stash rejection reason missing"
pass "Git stashes after marker are preserved"

new_home "reflog"
git_init_checkout
write_marker
write_wda_plist
git -C "$WDA_DIR" update-ref -m local-safety HEAD "$WDA_HEAD" "$WDA_HEAD"
expect_preserved_checkout
grep -q "Git reflog changed" "$TEST_ROOT/err" || fail_test "reflog rejection reason missing"
pass "reflog-only history after marker is preserved"

new_home "unreferenced-object"
git_init_checkout
write_marker
write_wda_plist
printf 'unpublished object\n' | git -C "$WDA_DIR" hash-object -w --stdin >/dev/null
expect_preserved_checkout
grep -q "object database changed" "$TEST_ROOT/err" \
    || fail_test "unreferenced-object rejection reason missing"
pass "unreferenced unpublished Git objects after marker are preserved"

new_home "ignored-data"
git_init_checkout
printf 'ignored.dat\n' > "$WDA_DIR/.gitignore"
git -C "$WDA_DIR" add .gitignore
git -C "$WDA_DIR" commit -qm ignore-rule
WDA_HEAD="$(git -C "$WDA_DIR" rev-parse HEAD)"
write_marker
write_wda_plist
printf 'private fixture\n' > "$WDA_DIR/ignored.dat"
expect_preserved_checkout
grep -q "ignored" "$TEST_ROOT/err" || fail_test "ignored-data rejection reason missing"
pass "ignored checkout data is preserved"

new_home "linked-worktree"
git_init_checkout
write_marker
write_wda_plist
git -C "$WDA_DIR" worktree add -q -b linked-safety "$TEST_ROOT/linked" "$WDA_HEAD"
expect_preserved_checkout
grep -Eq "Git refs changed|worktree" "$TEST_ROOT/err" \
    || fail_test "linked-worktree rejection reason missing"
pass "linked worktrees and their Git administration data are preserved"

new_home "setup-marker-helper"
git_init_checkout
marker_functions="$(sed -n \
    '/^_sha256_text() {/,/^# Resolve one signing identity/{ /^# Resolve one signing identity/d; p; }' \
    "$SETUP")"
(
    # The extracted production functions consume these globals dynamically.
    # shellcheck disable=SC2034
    UID_NUM="$(id -u)"
    # shellcheck disable=SC2034
    STATE_DIR="$TEST_HOME/.iphone-use"
    # shellcheck disable=SC2034
    WDA_CHECKOUT_MARKER="$STATE_DIR/wda-checkout-owner.v1"
    # shellcheck disable=SC2034
    WDA_REF="$WDA_HEAD"
    # shellcheck disable=SC2034
    WDA_MARKER_REFRESH_ALLOWED=1
    # shellcheck disable=SC2034
    SHASUM_BIN="$(command -v shasum)"
    eval "$marker_functions"
    _write_wda_checkout_marker
    _existing_marker_matches_checkout \
        "$WDA_DIR" "https://github.com/appium/WebDriverAgent.git" "$WDA_HEAD"
)
[ "$(/usr/bin/stat -f '%Lp' "$TEST_HOME/.iphone-use/wda-checkout-owner.v1")" = "600" ] \
    || fail_test "setup marker mode is not 600"
pass "setup marker helper writes and revalidates atomically"

printf '1..%d\n' "$pass_count"
