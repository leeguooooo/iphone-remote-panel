#!/usr/bin/env bash
# Isolated tests for install.sh's release and agent-skill transaction boundaries.
set -euo pipefail
umask 077

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
INSTALLER="$ROOT/install.sh"
TMP_BASE="$(cd -P "${TMPDIR:-/tmp}" && pwd)"
TMP_ROOT_RAW="$(mktemp -d "$TMP_BASE/iphone-use-skill-test.XXXXXX")"
TMP_ROOT="$(cd -P "$TMP_ROOT_RAW" && pwd)"

cleanup() {
    case "$TMP_ROOT" in
        "$TMP_BASE"/iphone-use-skill-test.*)
            rm -rf "$TMP_ROOT"
            ;;
        *)
            printf 'Refusing unsafe test cleanup path: %s\n' "$TMP_ROOT" >&2
            ;;
    esac
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
    mkdir -p \
        "$TEST_HOME/.agents/skills/iphone-use" \
        "$TEST_HOME/.iphone-use" \
        "$TEST_HOME/expected" \
        "$TEST_HOME/xdg/skills"
    TEST_HOME="$(cd -P "$TEST_HOME" && pwd)"
    touch "$TEST_HOME/.iphone-use-installer-test-root"
    cp "$ROOT/skills/iphone-use/SKILL.md" "$TEST_HOME/expected/SKILL.md"
}

write_previous_state() {
    printf '%s\n' 'old skill content' \
        > "$TEST_HOME/.agents/skills/iphone-use/SKILL.md"
    printf '%s\n' 'old release marker' \
        > "$TEST_HOME/.agents/skills/iphone-use/.iphone-use-release"
    printf '%s\n' 'old extra file' \
        > "$TEST_HOME/.agents/skills/iphone-use/legacy.txt"
    printf '%s\n' \
        '{"version":1,"skills":{"iphone-use":{"source":"leeguooooo/iphone-use"},"other-skill":{"source":"example/other"}}}' \
        > "$TEST_HOME/.agents/.skill-lock.json"
    cp "$TEST_HOME/.agents/.skill-lock.json" \
        "$TEST_HOME/xdg/skills/.skill-lock.json"
    mkdir -p "$TEST_HOME/.claude/skills/iphone-use"
    printf '%s\n' 'old Claude-specific skill copy' \
        > "$TEST_HOME/.claude/skills/iphone-use/SKILL.md"
}

run_hook() {
    env \
        HOME="$TEST_HOME" \
        XDG_STATE_HOME="${2:-$TEST_HOME/xdg}" \
        IPHONE_USE_INTERNAL_TEST_SKILL_ONLY=1 \
        IPHONE_USE_INTERNAL_TEST_EXPECTED="$TEST_HOME/expected/SKILL.md" \
        IPHONE_USE_INTERNAL_TEST_FORCE_FAILURE="${1:-0}" \
        IPHONE_USE_INTERNAL_TEST_MUTATE_LOCK_AFTER_INSTALL="${3:-0}" \
        IPHONE_USE_INTERNAL_TEST_MUTATE_CLAUDE_AFTER_INSTALL="${4:-0}" \
        IPHONE_USE_INTERNAL_TEST_MUTATE_CANONICAL_AFTER_INSTALL="${5:-0}" \
        IPHONE_USE_INTERNAL_TEST_MUTATE_LOCK_BEFORE_REMOVE="${6:-0}" \
        IPHONE_USE_INTERNAL_TEST_MUTATE_LOCK_DURING_NOOP="${7:-0}" \
        IPHONE_USE_INTERNAL_TEST_MUTATE_LOCK_AFTER_MOVE="${8:-0}" \
        /bin/bash "$INSTALLER"
}

run_bootstrap_verify_hook() {
    local pinned="$1"
    local sha256="$2"
    env \
        HOME="$TEST_HOME" \
        XDG_STATE_HOME="$TEST_HOME/xdg" \
        IPHONE_USE_INTERNAL_TEST_BOOTSTRAP_VERIFY_ONLY=1 \
        IPHONE_USE_INSTALLER_PINNED="$pinned" \
        IPHONE_USE_SKIP_SKILL=1 \
        IPHONE_USE_SKILL_VERIFIED_BY_BOOTSTRAP=1 \
        IPHONE_USE_RELEASE_REF=v-test \
        IPHONE_USE_RELEASE_COMMIT=0123456789abcdef0123456789abcdef01234567 \
        IPHONE_USE_SKILL_SHA256="$sha256" \
        /bin/bash "$INSTALLER"
}

run_daemon_commit_verify_hook() {
    local pinned="$1"
    local sha256="$2"
    local mutate_lock_symlink="${3:-0}"
    env \
        HOME="$TEST_HOME" \
        XDG_STATE_HOME="$TEST_HOME/xdg" \
        IPHONE_USE_INTERNAL_TEST_DAEMON_COMMIT_VERIFY_ONLY=1 \
        IPHONE_USE_INTERNAL_TEST_MUTATE_BOOTSTRAP_LOCK_SYMLINK="$mutate_lock_symlink" \
        IPHONE_USE_INSTALLER_PINNED="$pinned" \
        IPHONE_USE_SKIP_SKILL=1 \
        IPHONE_USE_SKILL_VERIFIED_BY_BOOTSTRAP=1 \
        IPHONE_USE_RELEASE_REF=v-test \
        IPHONE_USE_RELEASE_COMMIT=0123456789abcdef0123456789abcdef01234567 \
        IPHONE_USE_SKILL_SHA256="$sha256" \
        /bin/bash "$INSTALLER"
}

run_archive_hook() {
    local archive="$1"
    local destination="$2"
    env \
        HOME="$TEST_HOME" \
        IPHONE_USE_INTERNAL_TEST_ARCHIVE_ONLY=1 \
        IPHONE_USE_INTERNAL_TEST_ARCHIVE="$archive" \
        IPHONE_USE_INTERNAL_TEST_ARCHIVE_DEST="$destination" \
        /bin/bash "$INSTALLER"
}

run_commit_resolution_hook() {
    local requested_commit="$1"
    env \
        HOME="$TEST_HOME" \
        PATH="$TEST_HOME/bin:$PATH" \
        IPHONE_USE_INTERNAL_TEST_RESOLVE_COMMIT_ONLY=1 \
        IPHONE_USE_INTERNAL_TEST_RELEASE_REF=v-test \
        IPHONE_USE_RELEASE_COMMIT="$requested_commit" \
        /bin/bash "$INSTALLER"
}

run_commit_state_hook() {
    env \
        HOME="$TEST_HOME" \
        IPHONE_USE_INTERNAL_TEST_COMMIT_STATE_ONLY="$1" \
        /bin/bash "$INSTALLER"
}

assert_lock_has_only_other_skill() {
    local lock="$1"
    if /usr/bin/plutil -extract 'skills.iphone-use' json -o - "$lock" \
        >/dev/null 2>&1; then
        fail_test "floating iphone-use entry remains in $lock"
    fi
    /usr/bin/plutil -extract 'skills.other-skill' json -o - "$lock" \
        >/dev/null 2>&1 \
        || fail_test "unrelated skill entry was lost from $lock"
}

new_home "success"
write_previous_state
run_hook >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"
cmp -s \
    "$TEST_HOME/expected/SKILL.md" \
    "$TEST_HOME/.agents/skills/iphone-use/SKILL.md" \
    || fail_test "installed skill bytes differ from the expected source"
grep -Fx 'release_ref=v-test' \
    "$TEST_HOME/.agents/skills/iphone-use/.iphone-use-release" >/dev/null \
    || fail_test "release ref marker is missing"
grep -Fx 'release_commit=0123456789abcdef0123456789abcdef01234567' \
    "$TEST_HOME/.agents/skills/iphone-use/.iphone-use-release" >/dev/null \
    || fail_test "release commit marker is missing"
[ "$(find "$TEST_HOME/.agents/skills/iphone-use" -mindepth 1 -maxdepth 1 \
    -print | wc -l | tr -d '[:space:]')" = "2" ] \
    || fail_test "committed skill directory has unexpected files"
assert_lock_has_only_other_skill "$TEST_HOME/.agents/.skill-lock.json"
assert_lock_has_only_other_skill "$TEST_HOME/xdg/skills/.skill-lock.json"
[ -L "$TEST_HOME/.claude/skills/iphone-use" ] \
    || fail_test "Claude Code discovery link was not installed"
[ "$(cd -P "$TEST_HOME/.claude/skills/iphone-use" && pwd -P)" \
    = "$TEST_HOME/.agents/skills/iphone-use" ] \
    || fail_test "Claude Code discovery link resolves to the wrong skill"
[ -z "$(find "$TEST_HOME/.iphone-use" -mindepth 1 -maxdepth 1 \
    -name 'skill-backup.*' -print -quit)" ] \
    || fail_test "successful transaction retained rollback data"
pass "verified skill and both lock files commit atomically"

installed_sha="$(/usr/bin/shasum -a 256 \
    "$TEST_HOME/.agents/skills/iphone-use/SKILL.md" | awk '{print $1}')"
run_bootstrap_verify_hook 1 "$installed_sha" \
    >"$TEST_ROOT/verify-out" 2>"$TEST_ROOT/verify-err" \
    || fail_test "pinned inner installer rejected the verified outer skill"
pass "pinned inner installer re-verifies marker, hash, locks, and discovery"

if run_daemon_commit_verify_hook 1 "$installed_sha" 1 \
    >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
    fail_test "inner daemon commit accepted a symlinked lock after initial verification"
fi
grep -q 'Pinned bootstrap skill changed before daemon commit' "$TEST_ROOT/err" \
    || fail_test "daemon-commit rejection did not identify the unsafe bootstrap state"
[ -L "$TEST_HOME/.agents/.skill-lock.json" ] \
    || fail_test "unsafe bootstrap-lock fixture was not installed"
pass "inner daemon commit rejects an unsafe lock after install work"

new_home "commit-state-before"
if run_commit_state_hook pending-before \
    >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
    fail_test "pending signal before daemon commit unexpectedly returned success"
fi
pass "pending signal before commit leaves every daemon flag rollback-eligible"

new_home "commit-state-after"
run_commit_state_hook failure-after \
    >"$TEST_ROOT/out" 2>"$TEST_ROOT/err" \
    || fail_test "post-commit failure leaked a nonzero status to the pinned outer installer"
pass "post-commit signal or failure is normalized for the pinned outer transaction"

new_home "rollback-existing"
write_previous_state
cp -R "$TEST_HOME/.agents/skills/iphone-use" "$TEST_ROOT/original-skill"
cp -R "$TEST_HOME/.claude/skills/iphone-use" "$TEST_ROOT/original-claude-skill"
cp "$TEST_HOME/.agents/.skill-lock.json" "$TEST_ROOT/original-default-lock"
cp "$TEST_HOME/xdg/skills/.skill-lock.json" "$TEST_ROOT/original-xdg-lock"
if run_hook 1 >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
    fail_test "forced post-install failure unexpectedly succeeded"
fi
/usr/bin/diff -ru "$TEST_ROOT/original-skill" \
    "$TEST_HOME/.agents/skills/iphone-use" >/dev/null \
    || fail_test "previous skill directory was not restored exactly"
/usr/bin/diff -ru "$TEST_ROOT/original-claude-skill" \
    "$TEST_HOME/.claude/skills/iphone-use" >/dev/null \
    || fail_test "previous Claude-specific skill was not restored exactly"
cmp -s "$TEST_ROOT/original-default-lock" \
    "$TEST_HOME/.agents/.skill-lock.json" \
    || fail_test "default skills lock was not restored byte-for-byte"
cmp -s "$TEST_ROOT/original-xdg-lock" \
    "$TEST_HOME/xdg/skills/.skill-lock.json" \
    || fail_test "XDG skills lock was not restored byte-for-byte"
[ -z "$(find "$TEST_HOME/.iphone-use" -mindepth 1 -maxdepth 1 \
    -name 'skill-backup.*' -print -quit)" ] \
    || fail_test "rolled-back transaction retained rollback data"
pass "post-install failure restores previous skill and both locks"

new_home "rollback-empty"
rm -rf "$TEST_HOME/.agents/skills/iphone-use"
rm -f \
    "$TEST_HOME/.agents/.skill-lock.json" \
    "$TEST_HOME/xdg/skills/.skill-lock.json"
if run_hook 1 >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
    fail_test "forced failure on an empty install unexpectedly succeeded"
fi
[ ! -e "$TEST_HOME/.agents/skills/iphone-use" ] \
    || fail_test "failed fresh install left a canonical skill behind"
[ ! -e "$TEST_HOME/.agents/.skill-lock.json" ] \
    || fail_test "failed fresh install created a default skills lock"
[ ! -e "$TEST_HOME/xdg/skills/.skill-lock.json" ] \
    || fail_test "failed fresh install created an XDG skills lock"
[ ! -e "$TEST_HOME/.claude" ] \
    || fail_test "failed fresh install left a Claude discovery namespace"
pass "failed fresh install restores the original absent state"

new_home "invalid-source"
write_previous_state
cp -R "$TEST_HOME/.agents/skills/iphone-use" "$TEST_ROOT/original-skill"
cp "$TEST_HOME/.agents/.skill-lock.json" "$TEST_ROOT/original-default-lock"
printf '%s\n' 'not a valid iphone-use skill' \
    > "$TEST_HOME/expected/SKILL.md"
if run_hook >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
    fail_test "invalid skill source unexpectedly succeeded"
fi
/usr/bin/diff -ru "$TEST_ROOT/original-skill" \
    "$TEST_HOME/.agents/skills/iphone-use" >/dev/null \
    || fail_test "invalid source changed the previous skill"
cmp -s "$TEST_ROOT/original-default-lock" \
    "$TEST_HOME/.agents/.skill-lock.json" \
    || fail_test "invalid source changed the skills lock"
grep -q 'implausible size' "$TEST_ROOT/err" \
    || fail_test "invalid source failure did not identify validation"
pass "skill validation failure is fatal and happens before replacement"

new_home "spoofed-bootstrap"
write_previous_state
old_sha="$(/usr/bin/shasum -a 256 \
    "$TEST_HOME/.agents/skills/iphone-use/SKILL.md" | awk '{print $1}')"
if run_bootstrap_verify_hook 0 "$old_sha" \
    >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
    fail_test "untrusted bootstrap verification environment unexpectedly succeeded"
fi
grep -q 'Pinned bootstrap skill verification failed' "$TEST_ROOT/err" \
    || fail_test "untrusted bootstrap claim rejection reason is missing"
pass "untrusted environment cannot claim that a skipped skill was synchronized"

new_home "nested-xdg"
write_previous_state
cp -R "$TEST_HOME/.agents/skills/iphone-use" "$TEST_ROOT/original-skill"
if run_hook 0 "$TEST_HOME/.agents/skills/iphone-use" \
    >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
    fail_test "nested XDG_STATE_HOME unexpectedly succeeded"
fi
/usr/bin/diff -ru "$TEST_ROOT/original-skill" \
    "$TEST_HOME/.agents/skills/iphone-use" >/dev/null \
    || fail_test "nested XDG rejection changed the previous skill"
grep -q 'overlaps an iphone-use transaction directory' "$TEST_ROOT/err" \
    || fail_test "nested XDG rejection reason is missing"
pass "XDG lock paths cannot overlap the skill transaction"

new_home "safe-concurrent-lock"
write_previous_state
run_hook 0 "$TEST_HOME/xdg" 1 >"$TEST_ROOT/out" 2>"$TEST_ROOT/err" \
    || fail_test "safe unrelated lock update incorrectly blocked skill commit"
/usr/bin/plutil -extract 'skills.concurrent-test' json -o - \
    "$TEST_HOME/.agents/.skill-lock.json" >/dev/null 2>&1 \
    || fail_test "safe unrelated lock update was lost at commit"
if /usr/bin/plutil -extract 'skills.iphone-use' json -o - \
    "$TEST_HOME/.agents/.skill-lock.json" >/dev/null 2>&1; then
    fail_test "floating iphone-use entry returned during safe concurrent update"
fi
[ -z "$(find "$TEST_HOME/.iphone-use" -mindepth 1 -maxdepth 1 \
    -name 'skill-backup.*' -print -quit)" ] \
    || fail_test "safe concurrent commit retained rollback data"
pass "safe unrelated lock updates do not create a post-daemon mismatch"

new_home "concurrent-lock"
write_previous_state
cp -R "$TEST_HOME/.agents/skills/iphone-use" "$TEST_ROOT/original-skill"
if run_hook 1 "$TEST_HOME/xdg" 1 >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
    fail_test "concurrent-lock rollback fixture unexpectedly succeeded"
fi
/usr/bin/diff -ru "$TEST_ROOT/original-skill" \
    "$TEST_HOME/.agents/skills/iphone-use" >/dev/null \
    || fail_test "concurrent lock change prevented restoration of the old skill"
/usr/bin/plutil -extract 'skills.concurrent-test' json -o - \
    "$TEST_HOME/.agents/.skill-lock.json" >/dev/null 2>&1 \
    || fail_test "concurrent lock entry was overwritten by rollback"
if /usr/bin/plutil -extract 'skills.iphone-use' json -o - \
    "$TEST_HOME/.agents/.skill-lock.json" >/dev/null 2>&1; then
    fail_test "concurrent lock was replaced by the pre-install snapshot"
fi
grep -q 'Skills lock changed concurrently; preserving it' "$TEST_ROOT/out" \
    || fail_test "concurrent lock preservation warning is missing"
[ -n "$(find "$TEST_HOME/.iphone-use" -mindepth 1 -maxdepth 1 \
    -name 'skill-backup.*' -print -quit)" ] \
    || fail_test "incomplete concurrent rollback did not retain recovery data"
pass "rollback preserves a concurrently changed skills lock"

new_home "pre-remove-lock-race"
write_previous_state
cp -R "$TEST_HOME/.agents/skills/iphone-use" "$TEST_ROOT/original-skill"
if run_hook 0 "$TEST_HOME/xdg" 0 0 0 1 \
    >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
    fail_test "pre-removal lock race fixture unexpectedly succeeded"
fi
/usr/bin/diff -ru "$TEST_ROOT/original-skill" \
    "$TEST_HOME/.agents/skills/iphone-use" >/dev/null \
    || fail_test "pre-removal lock race prevented restoration of the old skill"
/usr/bin/plutil -extract 'skills.concurrent-before-remove' json -o - \
    "$TEST_HOME/.agents/.skill-lock.json" >/dev/null 2>&1 \
    || fail_test "pre-removal concurrent lock entry was overwritten"
grep -q 'Skills lock changed after snapshot' "$TEST_ROOT/err" \
    || fail_test "pre-removal lock race rejection reason is missing"
pass "lock update refuses data changed after its transaction snapshot"

new_home "noop-lock-race"
write_previous_state
/usr/bin/plutil -remove 'skills.iphone-use' \
    "$TEST_HOME/.agents/.skill-lock.json"
cp -R "$TEST_HOME/.agents/skills/iphone-use" "$TEST_ROOT/original-skill"
if run_hook 0 "$TEST_HOME/xdg" 0 0 0 0 1 \
    >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
    fail_test "no-op lock race fixture unexpectedly succeeded"
fi
/usr/bin/diff -ru "$TEST_ROOT/original-skill" \
    "$TEST_HOME/.agents/skills/iphone-use" >/dev/null \
    || fail_test "no-op lock race prevented restoration of the old skill"
/usr/bin/plutil -extract 'skills.concurrent-during-noop' json -o - \
    "$TEST_HOME/.agents/.skill-lock.json" >/dev/null 2>&1 \
    || fail_test "no-op concurrent lock entry was overwritten"
grep -q 'Skills lock changed during no-op synchronization' "$TEST_ROOT/err" \
    || fail_test "no-op lock race rejection reason is missing"
pass "no-op lock synchronization cannot absorb a concurrent update"

new_home "post-move-lock-race"
write_previous_state
cp -R "$TEST_HOME/.agents/skills/iphone-use" "$TEST_ROOT/original-skill"
if run_hook 0 "$TEST_HOME/xdg" 0 0 0 0 0 1 \
    >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
    fail_test "post-move lock race fixture unexpectedly succeeded"
fi
/usr/bin/diff -ru "$TEST_ROOT/original-skill" \
    "$TEST_HOME/.agents/skills/iphone-use" >/dev/null \
    || fail_test "post-move lock race prevented restoration of the old skill"
/usr/bin/plutil -extract 'skills.concurrent-after-move' json -o - \
    "$TEST_HOME/.agents/.skill-lock.json" >/dev/null 2>&1 \
    || fail_test "post-move concurrent lock entry was overwritten"
grep -q 'Skills lock changed immediately after commit' "$TEST_ROOT/err" \
    || fail_test "post-move lock race rejection reason is missing"
pass "lock AFTER state cannot absorb a post-move concurrent update"

new_home "concurrent-claude"
write_previous_state
cp -R "$TEST_HOME/.agents/skills/iphone-use" "$TEST_ROOT/original-skill"
if run_hook 1 "$TEST_HOME/xdg" 0 1 >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
    fail_test "concurrent-Claude rollback fixture unexpectedly succeeded"
fi
/usr/bin/diff -ru "$TEST_ROOT/original-skill" \
    "$TEST_HOME/.agents/skills/iphone-use" >/dev/null \
    || fail_test "concurrent Claude change prevented restoration of the old canonical skill"
grep -Fx 'concurrent Claude target' \
    "$TEST_HOME/.claude/skills/iphone-use/SKILL.md" >/dev/null \
    || fail_test "concurrent Claude target was overwritten by rollback"
grep -q 'Claude skill target changed concurrently; preserving it' "$TEST_ROOT/out" \
    || fail_test "concurrent Claude preservation warning is missing"
[ -n "$(find "$TEST_HOME/.iphone-use" -mindepth 1 -maxdepth 1 \
    -name 'skill-backup.*' -print -quit)" ] \
    || fail_test "incomplete Claude rollback did not retain recovery data"
pass "rollback preserves a concurrently changed Claude discovery target"

new_home "concurrent-canonical"
write_previous_state
if run_hook 0 "$TEST_HOME/xdg" 0 0 1 \
    >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
    fail_test "concurrent canonical commit fixture unexpectedly succeeded"
fi
grep -q 'concurrent canonical mutation' \
    "$TEST_HOME/.agents/skills/iphone-use/SKILL.md" \
    || fail_test "concurrent canonical skill was overwritten by rollback"
grep -q 'Agent skill changed before transaction commit' "$TEST_ROOT/err" \
    || fail_test "commit-time canonical revalidation reason is missing"
grep -q 'Canonical iphone-use skill changed concurrently; preserving it' "$TEST_ROOT/out" \
    || fail_test "canonical CAS preservation warning is missing"
[ -n "$(find "$TEST_HOME/.iphone-use" -mindepth 1 -maxdepth 1 \
    -name 'skill-backup.*' -print -quit)" ] \
    || fail_test "incomplete canonical rollback did not retain recovery data"
pass "commit revalidation refuses and preserves a concurrently replaced skill"

new_home "release-commit-match"
mkdir -p "$TEST_HOME/bin"
printf '%s\n' \
    '#!/bin/sh' \
    "printf '%s\\n' '{' '  \"sha\": \"1111111111111111111111111111111111111111\"' '}'" \
    > "$TEST_HOME/bin/curl"
chmod 700 "$TEST_HOME/bin/curl"
run_commit_resolution_hook 1111111111111111111111111111111111111111 \
    >"$TEST_ROOT/out" 2>"$TEST_ROOT/err" \
    || fail_test "matching release commit override was rejected"
grep -q '1111111111111111111111111111111111111111' "$TEST_ROOT/out" \
    || fail_test "resolved release commit is missing"
if run_commit_resolution_hook 2222222222222222222222222222222222222222 \
    >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
    fail_test "mismatched release commit override unexpectedly succeeded"
fi
grep -q 'does not match release v-test' "$TEST_ROOT/err" \
    || fail_test "release commit mismatch reason is missing"
pass "release app tag cannot be paired with a different helper commit"

new_home "archive-valid"
mkdir -p "$TEST_HOME/archive-source/iPhoneUse.app/Contents/MacOS" \
    "$TEST_HOME/archive-output"
printf '%s\n' 'fixture binary' \
    > "$TEST_HOME/archive-source/iPhoneUse.app/Contents/MacOS/iphone-use"
(
    cd "$TEST_HOME/archive-source"
    /usr/bin/zip -qry "$TEST_HOME/valid-app.zip" iPhoneUse.app
)
run_archive_hook "$TEST_HOME/valid-app.zip" "$TEST_HOME/archive-output" \
    >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"
[ -f "$TEST_HOME/archive-output/iPhoneUse.app/Contents/MacOS/iphone-use" ] \
    || fail_test "valid single-app archive did not extract"
pass "release archive accepts only the expected app tree"

new_home "archive-collision"
mkdir -p "$TEST_HOME/archive-source/iPhoneUse.app" "$TEST_HOME/archive-output"
printf '%s\n' 'fixture app' > "$TEST_HOME/archive-source/iPhoneUse.app/payload"
printf '%s\n' 'collision' > "$TEST_HOME/archive-source/install.sh"
(
    cd "$TEST_HOME/archive-source"
    /usr/bin/zip -qry "$TEST_HOME/collision.zip" iPhoneUse.app install.sh
)
if run_archive_hook "$TEST_HOME/collision.zip" "$TEST_HOME/archive-output" \
    >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
    fail_test "archive helper-collision fixture unexpectedly succeeded"
fi
grep -q 'unexpected top-level entry: install.sh' "$TEST_ROOT/err" \
    || fail_test "archive collision rejection reason is missing"
[ ! -e "$TEST_HOME/archive-output/install.sh" ] \
    || fail_test "malicious top-level helper was extracted"
pass "release archive cannot overwrite commit-pinned helpers"

new_home "archive-symlink"
mkdir -p "$TEST_HOME/archive-source/iPhoneUse.app" "$TEST_HOME/archive-output"
ln -s ../../helpers "$TEST_HOME/archive-source/iPhoneUse.app/escape"
(
    cd "$TEST_HOME/archive-source"
    /usr/bin/zip -qry -y "$TEST_HOME/symlink.zip" iPhoneUse.app
)
if run_archive_hook "$TEST_HOME/symlink.zip" "$TEST_HOME/archive-output" \
    >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
    fail_test "archive symlink fixture unexpectedly succeeded"
fi
grep -q 'archive contains a symlink' "$TEST_ROOT/err" \
    || fail_test "archive symlink rejection reason is missing"
pass "release archive rejects symlink-based extraction escapes"

printf '1..%d\n' "$pass_count"
