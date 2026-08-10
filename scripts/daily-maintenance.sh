#!/usr/bin/env bash
#
# Daily unattended maintenance for iphone-use.
#
# Runs at 02:00 local time via ~/Library/LaunchAgents/
# com.leeguoo.iphone-use.daily-maintenance.plist.
#
#   1. sync main with origin (bail if the working tree is dirty)
#   2. pull open issues + PRs from GitHub
#   3. hand them to a headless Claude Code session to triage and fix
#   4. gate on `cargo test --workspace` — a red suite rolls the session back
#   5. if the session produced commits, bump the patch version, tag, and push
#      (the tag push is what makes .github/workflows/release-binaries.yml
#      build, sign, and publish the GitHub Release)
#
# The agent fixes code and commits. This script owns the release transaction,
# so a confused session cannot invent a version number or push a tag on its own.
#
# Run it by hand any time:  scripts/daily-maintenance.sh
# Skip the release step:    RELEASE=0 scripts/daily-maintenance.sh
# See what it would do:     DRY_RUN=1 scripts/daily-maintenance.sh

set -uo pipefail

REPO="${REPO:-/Users/leo/github.com/iphone-remote-panel}"
LOG="${LOG:-$HOME/Library/Logs/iphone-use-daily-maintenance.log}"
LOCK="${LOCK:-/tmp/iphone-use-daily-maintenance.lock}"
RELEASE="${RELEASE:-1}"
DRY_RUN="${DRY_RUN:-0}"
MODEL="${MODEL:-opus}"

PATH="/Users/leo/.local/bin:/Users/leo/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin"
export PATH

mkdir -p "$(dirname "$LOG")"
exec >>"$LOG" 2>&1

log() { echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*"; }
die() { log "ABORT: $*"; exit 1; }

# --- single instance ---------------------------------------------------------
if ! mkdir "$LOCK" 2>/dev/null; then
  log "another run holds $LOCK; exiting"
  exit 0
fi
trap 'rmdir "$LOCK" 2>/dev/null' EXIT

log "=== daily maintenance start (release=$RELEASE dry_run=$DRY_RUN) ==="
cd "$REPO" || die "repo not found at $REPO"

# --- 1. sync -----------------------------------------------------------------
if [ -n "$(git status --porcelain)" ]; then
  die "working tree is dirty — refusing to touch it. Commit or stash first."
fi

git rev-parse --abbrev-ref HEAD | grep -qx main || die "not on main"
git fetch --quiet --tags origin || die "git fetch failed"
git pull --quiet --ff-only origin main || die "git pull --ff-only failed (diverged?)"

START_SHA="$(git rev-parse HEAD)"
log "synced main at $START_SHA"

# --- 2. gather work ----------------------------------------------------------
ISSUES="$(gh issue list --state open --limit 30 \
  --json number,title,labels,updatedAt 2>/dev/null)" || die "gh issue list failed"
PRS="$(gh pr list --state open --limit 30 \
  --json number,title,isDraft,mergeable,updatedAt 2>/dev/null)" || die "gh pr list failed"

N_ISSUES="$(echo "$ISSUES" | jq 'length')"
N_PRS="$(echo "$PRS" | jq 'length')"
log "open issues: $N_ISSUES, open PRs: $N_PRS"

if [ "$N_ISSUES" -eq 0 ] && [ "$N_PRS" -eq 0 ]; then
  log "nothing open — done"
  exit 0
fi

# --- 3. headless triage + fix ------------------------------------------------
PROMPT="$(cat <<EOF
You are running unattended as the daily maintainer of the iphone-use repo
($REPO). No human is watching; nobody can answer a question. Finish or stop.

Open issues (JSON):
$ISSUES

Open PRs (JSON):
$PRS

Do this:

1. Read each open issue with \`gh issue view <n> --comments\`. Decide whether it
   is fixable from source alone. SKIP anything that needs physical hardware to
   reproduce or verify (a real iPhone, iPhone Mirroring, a live WDA session, a
   signing keychain) — you have none of that. Skip anything that needs a
   product decision. Skipping is a fine outcome; a wrong guess shipped to
   users is not.
2. For each issue you do take: fix it properly at the root cause, and add or
   extend a test in crates/*/tests or crates/*/src that fails without your fix.
3. Review each open PR with \`gh pr view <n>\` and \`gh pr diff <n>\`. Leave a
   review comment if something is wrong. Do NOT merge any PR.
4. Commit each fix separately on main. Match the repo's commit style — run
   \`git log --oneline -15\` and follow it (English, Conventional Commits,
   e.g. \`fix(wda): ...\`). Reference the issue as "(#N)" in the subject.
   Do NOT tag, do NOT bump any version, do NOT push. A release script handles
   that after you exit.
5. Run \`cargo test --workspace\` and make sure it passes before you finish.
6. On each issue you actually fixed, post a short comment with
   \`gh issue comment <n> --body "..."\` explaining the fix and that it ships
   in the next release. Do not close issues — the release notes and the human
   close them.

If nothing is safely fixable, make no commits and say so. That is a valid,
expected outcome — do not manufacture busywork to look productive.
EOF
)"

if [ "$DRY_RUN" = "1" ]; then
  log "DRY_RUN: would run claude with the prompt below"
  echo "$PROMPT"
  exit 0
fi

log "handing off to claude (model=$MODEL)..."
echo "$PROMPT" | claude -p \
  --model "$MODEL" \
  --dangerously-skip-permissions \
  --add-dir "$REPO"
CLAUDE_RC=$?
log "claude exited rc=$CLAUDE_RC"

# --- 4. verify ---------------------------------------------------------------
END_SHA="$(git rev-parse HEAD)"

if [ "$START_SHA" = "$END_SHA" ]; then
  log "no commits produced — nothing to release"
  # Drop any stray uncommitted edits the session left behind.
  git reset --hard --quiet "$START_SHA" && git clean -fdq
  exit 0
fi

log "new commits:"
git log --oneline "$START_SHA..$END_SHA"

if [ -n "$(git status --porcelain)" ]; then
  log "session left uncommitted changes; discarding them"
  git reset --hard --quiet "$END_SHA" && git clean -fdq
fi

log "running cargo test --workspace ..."
if ! cargo test --workspace >/tmp/iphone-use-daily-test.log 2>&1; then
  log "TESTS FAILED — rolling back to $START_SHA, nothing pushed"
  tail -40 /tmp/iphone-use-daily-test.log
  git reset --hard --quiet "$START_SHA"
  exit 1
fi
log "tests pass"

if [ "$RELEASE" != "1" ]; then
  log "RELEASE=0 — commits left local at $END_SHA, not pushed"
  exit 0
fi

# --- 5. release --------------------------------------------------------------
CUR="$(grep -m1 '^version' crates/server/Cargo.toml | sed 's/.*"\(.*\)".*/\1/')"
MAJ="${CUR%%.*}"; REST="${CUR#*.}"; MIN="${REST%%.*}"; PATCH="${REST#*.}"
NEXT="$MAJ.$MIN.$((PATCH + 1))"
log "version bump: $CUR -> $NEXT"

for f in crates/core/Cargo.toml crates/mcp/Cargo.toml crates/server/Cargo.toml; do
  [ -f "$f" ] || continue
  sed -i '' "1,10s/^version = \"$CUR\"/version = \"$NEXT\"/" "$f"
done
cargo check --workspace --quiet >/dev/null 2>&1   # refresh Cargo.lock

if ! git diff --quiet -- '*Cargo.toml' Cargo.lock; then
  git add -A -- '*Cargo.toml' Cargo.lock
  git commit -q -m "chore(release): v$NEXT"
else
  die "version bump produced no diff — expected $CUR in the Cargo.toml files"
fi

git tag "v$NEXT" || die "tag v$NEXT already exists"

if ! git push origin main; then
  log "push of main failed — removing local tag v$NEXT, nothing released"
  git tag -d "v$NEXT"
  exit 1
fi
git push origin "v$NEXT" || die "tag push failed; main is pushed but no release will build"

log "pushed main + v$NEXT — release-binaries.yml is building"
log "=== daily maintenance done ==="
