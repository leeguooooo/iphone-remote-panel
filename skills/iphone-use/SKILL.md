---
name: iphone-use
description: Use when a task needs a real iPhone — operating iOS apps that have no API (Apple Health, banking, IM apps), exporting on-phone data, tapping/typing/scrolling on the phone, or taking phone screenshots. Drives the iphone-use daemon's HTTP agent API over macOS iPhone Mirroring.
---

# iphone-use — drive a real iPhone

Control a physical iPhone through the [iphone-use](https://github.com/leeguooooo/iphone-use)
daemon: see the screen (`/agent/screenshot`), act on it (`/agent/input`), repeat.
Built on macOS iPhone Mirroring — works on whatever app is on the phone, no
per-app API needed.

## Prerequisites

A Mac on your network running the daemon with iPhone Mirroring connected
(setup: see the repo README; `install.sh` registers it as a LaunchAgent).

```bash
HOST="${PHONE_REMOTE_URL:-http://127.0.0.1:44321}"
AUTH="Authorization: Bearer $PHONE_REMOTE_TOKEN"   # daemon password or PHONE_REMOTE_AGENT_TOKEN
```

**Always probe first** — if this fails, stop and report (don't retry blindly;
5 consecutive auth failures lock you out for 30s):

```bash
curl -s -H "$AUTH" "$HOST/agent/status"
# {"ok":true,"phone_target":true,"wda":false,"drivable":true,"mirror_state":"active",
#  "hint":"","mode":"mirror","viewer_count":0, ...}
```

**Check `drivable`, not `phone_target`.** `phone_target` only means the Mirroring
*window* exists — it stays `true` on the "Connection Paused" and "iPhone in Use"
interstitials, where taps land in the void. `drivable:true` is the real "can I
act now" signal (WDA is up, or the mirror is showing live content).

- `drivable:false` + `mirror_state` tells you why and what to do — read `hint`:
  - `mirror_state:"paused"` → tap the Resume button (a tap at `x=0.5, y=0.64` hits it).
  - `mirror_state:"in_use"` → a human is on the phone; **lock the phone** to reconnect
    (the on-screen Connect button does NOT reconnect while it's in use). Don't fight them.
  - `mirror_state:"offline"` → no Mirroring window; open iPhone Mirroring on the Mac.
- `human_active:true` → **a human is using the Mac right now** (Mirroring isn't frontmost).
  In mirror mode an L3 tap first yanks Mirroring frontmost, stealing the person's focus —
  so **back off**: pause and re-poll until `human_active:false`, or switch to agent mode
  (`POST /agent/mode {"mode":"agent"}`) where input is on-device and never touches the Mac
  cursor. (Always `false` in agent/WDA mode — no contention there.)
- `viewer_count` = connected `/ws` viewers (one streams; others queue, issue #8).

## The API (3 endpoints)

| Call | Purpose |
|---|---|
| `GET /agent/status` | `{ok, phone_target, wda, drivable, mirror_state, hint, mode, viewer_count, …}` — gate on **`drivable`**; `wda:true` unlocks the element layer below |
| `GET /agent/elements` | **(wda) The UI as text**: `{"elements":[{kind,label,rect,depth,value?},…]}` — prefer this over screenshots. `value` carries a Switch's `"0"/"1"`, a Slider fraction, a PickerWheel's current option (issue #20) so you can drive toggles/pickers without vision |
| `GET /agent/screenshot` | Current phone screen as PNG (falls back to on-device capture when Mirroring is gone) |
| `POST /agent/input` | One action (JSON body, below) |

Actions — coordinates are **normalized [0,1]** over the phone screen
(`0,0` top-left, `1,1` bottom-right), so they're resolution-independent:

```bash
curl -s -H "$AUTH" -X POST "$HOST/agent/input" -d '{"type":"tap","x":0.5,"y":0.3}'
curl -s -H "$AUTH" -X POST "$HOST/agent/input" -d '{"type":"tap","label":"新备忘录"}'  # (wda) tap BY ELEMENT — no coords
curl -s -H "$AUTH" -X POST "$HOST/agent/input" -d '{"type":"scroll","x":0.5,"y":0.5,"dx":0,"dy":-60}'
curl -s -H "$AUTH" -X POST "$HOST/agent/input" -d '{"type":"text","text":"Health"}'
curl -s -H "$AUTH" -X POST "$HOST/agent/input" -d '{"type":"key","name":"return"}'
curl -s -H "$AUTH" -X POST "$HOST/agent/input" -d '{"type":"shortcut","name":"home"}'      # home|spotlight|switcher
curl -s -H "$AUTH" -X POST "$HOST/agent/input" -d '{"type":"longpress","x":0.4,"y":0.6}'   # release with {"type":"up",...}
curl -s -H "$AUTH" -X POST "$HOST/agent/input" -d '{"type":"keyboard"}'                     # (wda) dismiss the on-screen keyboard
```

After typing into a web form the keyboard covers the page's own submit/next
buttons — send `{"type":"keyboard"}` to dismiss it before tapping them.

MCP alternative: the repo ships `iphone-use-mcp` (crates/mcp) exposing the
same actions as native MCP tools (`phone_tap`, `phone_screenshot`, …).

## The loop: see → act → verify

1. **See**: if `status` says `wda:true`, `GET /agent/elements` first — it's text
   (10× cheaper than vision), carries exact labels, and works even while a human
   is holding the phone. Fall back to `screenshot` when you need pixels (images,
   maps, unlabeled UI).
2. **Act**: ONE action. Prefer `{"type":"tap","label":"…"}` (no coordinate
   drift) over coordinate taps when the element has a label.
3. **Verify**: `elements` (or `screenshot`) again → confirm the expected change
   before the next step.

Hard-won facts (hardware-validated — trust these):

- **Scroll**: `dy < 0` scrolls content up (reveals what's below). A swipe is a
  scroll, NOT a drag — drags are for sliders/reorder via `longpress`+`up`.
- **Text input** — focus a field first, then `{"type":"text"}`. With `wda:true`
  it goes through the on-phone element layer (cleanest). Without WDA the daemon
  now pastes via the clipboard by default (issue #15), so **plain ASCII (IPs,
  ports, search terms) and CJK both land reliably** through the mirror too — the
  old "Mirroring drops keycodes / Pinyin IME eats digits" failure is gone, and
  the Mac clipboard is saved+restored so yours isn't clobbered. (Set
  `PHONE_REMOTE_TEXT_KEYCODE=1` to force the legacy char-by-char keycode path.)
- **WDA and iPhone Mirroring are mutually exclusive** (A/B-tested on hardware):
  the on-phone XCUITest runner monopolizes the device's remote session, so
  while `wda:true` the Mirroring window shows "Connection Interrupted" and the
  human's live video is replaced by ~2.5s stills — that's expected, not broken.
  Don't try to "fix" the mirror while WDA runs. Switch modes via the API:
  `POST /agent/mode {"mode":"mirror"}` (fully automatic: locks the phone,
  stops WDA, reconnects the mirror, ~10s) or `{"mode":"agent"}` (starts WDA;
  needs the phone unlocked once if it's locked). `GET /agent/status` reports
  the current `mode`. To drive a SECOND, non-Mirroring phone, pass its UDID:
  `{"mode":"agent","udid":"00008…"}`.
- **`mode=agent` stuck / `wda` stays false → read `status.setup_blocked_on`**
  (`warp|usb|trust|ddi`). The #1 blocker is **`warp`**: Cloudflare WARP (or any
  VPN) wedges the CoreDevice tunnel xcodebuild needs, so WDA never installs and
  the runner dies the instant WARP reconnects. Tell the operator to
  `warp-cli disconnect` (or run `setup-wda.sh doctor` for the full checklist).
  `trust` = a one-time "trust the Apple Development cert" tap on the phone.
- **Never blind-tap retry loops on the Mirroring interstitials.** A reconnect
  handshake takes 10–30s and a tap landing mid-handshake CANCELS it — a loop
  that taps Try Again every 20–30s turns "connects fine" into "connects then
  always drops" (hardware-verified the hard way). Tap the recovery button at
  most ONCE per state change, then give it 45s+. Never tap while `in_use`
  (the button is inert; lock the phone instead).
- **One action at a time.** The phone animates; give transitions ~1s before
  the verify screenshot. App launches / share sheets can take 2–4s.
- A reliable "reset to known state": `shortcut home`, then `shortcut spotlight`
  + `text <app name>` + `key return` to launch any app.

## Self-improvement: vision once → script forever

The first time you do a task, you're vision-guided (screenshot + reasoning at
every step). That's expensive. **Your job is to never pay that cost twice**:

1. **While solving, log every successful action** — the exact `input` payloads,
   the waits, and what you verified in each screenshot ("Health profile page:
   avatar top-right visible").
2. **When the task succeeds, freeze the log into a script** (bash or python:
   the curl sequence + sleeps). Normalized coordinates are stable for a given
   app screen + phone model, so replays are reliable.
3. **Keep checkpoints, drop reasoning.** At 2–3 key steps the script should
   grab a screenshot and do a cheap sanity check (or just save it for a human).
   Full vision re-engages ONLY when a checkpoint looks wrong — e.g. an iOS
   update moved a button. Then fix that one step and re-freeze.
4. **Name and keep scripts** somewhere durable (e.g. `~/phone-scripts/`), one
   task per file, with the date + app version it was validated against.

### Worked example: Apple Health full export (proven on hardware)

Apple Health has no API. This flow exports everything (weight, steps, sleep…)
as XML to the Mac, end-to-end ~2–4 min:

1. `shortcut home` → `shortcut spotlight` → `text "Health"` → `key return`
2. Tap the avatar (top-right of the Health summary page)
3. Scroll to the bottom of the profile (`dy:-80` × a few, verify by screenshot)
4. Tap "Export All Health Data" → tap the confirm "Export"
5. Wait ~60s (the phone packs the zip; poll screenshots for the share sheet)
6. In the share sheet: "Save to Files" → iCloud Drive → Save
7. On the Mac, wait for the zip to sync
   (`~/Library/Mobile Documents/com~apple~CloudDocs/导出.zip` or `Export.zip`;
   `brctl download <path>` forces the download), then parse
   `apple_health_export/export.xml` (stream-parse: it can be hundreds of MB).

First run: vision at every step. Second run onward: a one-command script that
only screenshots at steps 2, 5 and 6 as checkpoints.

## Stay current

`GET /agent/status` reports `version`, `latest` and `update_available` (the
daemon checks GitHub releases daily). When `update_available` is true, tell
the user once per session — don't upgrade anything yourself (the daemon
restart would kill your own session):

```
iphone-use 有新版本(latest,当前 version)。升级:
  daemon: curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh
  skill : npx skills update -g
```

If this skill's instructions ever disagree with the live API (an endpoint 404s
or a field is missing), the skill copy is probably stale — suggest
`npx skills update -g` before working around it.

## Found a rough edge? File an issue

You are this product's heaviest user — your friction reports are how it
improves. When something about **iphone-use itself** is broken, confusing, or
needlessly slow (NOT a task-level failure like a mistyped label):

1. Tell the user what you hit and that you'd like to file an issue.
2. With their OK, file it (the `gh` CLI is usually available):

```bash
gh issue create -R leeguooooo/iphone-use \
  -t "agent feedback: <one-line symptom>" \
  -b "$(cat <<'EOF'
**What I was doing**: <task context, 1-2 lines>
**What happened**: <actual behavior, exact error/output>
**Expected**: <what would have been better>
**Env**: daemon <version from /agent/status>, mode <mirror|agent>, <macOS/iOS if known>
**Repro**: <the exact curl/API calls, if reproducible>

*filed by an AI agent via the iphone-use skill, with user consent*
EOF
)"
```

Good candidates: misleading error messages, missing API capabilities you had
to work around, docs that lied, flaky behaviors with repro steps. Complaints
welcome — concrete beats polite.

## Safety

- The phone is REAL: taps have consequences. Verify the screen before tapping
  anything destructive (send / pay / delete). Never operate payment or 2FA
  screens unattended.
- A human can preempt you at any time (single shared cursor, last actor wins) —
  if the screen changes under you mid-task, screenshot and re-orient instead of
  continuing the old plan.
- **Check before you type.** `text` lands in whatever field currently has focus —
  if the human is mid-chat, your words go into THEIR message box. Read
  `/agent/elements` (or a screenshot) first and confirm the foreground app is
  the one you intend to drive.
