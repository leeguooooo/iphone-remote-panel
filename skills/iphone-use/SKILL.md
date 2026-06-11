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
curl -s -H "$AUTH" "$HOST/agent/status"   # want {"ok":true,"phone_target":true}
```

`phone_target:false` → the Mirroring window is gone (phone disconnected /
locked after reboot). A tap at the center sometimes wakes the "paused" screen;
a first-unlock-after-boot needs a human.

## The API (3 endpoints)

| Call | Purpose |
|---|---|
| `GET /agent/status` | `{"ok":true,"phone_target":bool,"wda":bool}` — `wda:true` unlocks the element layer below |
| `GET /agent/elements` | **(wda) The UI as text**: `{"elements":[{kind,label,rect,depth},…]}` — prefer this over screenshots |
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
```

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
- **Text routing depends on `wda`.** With `wda:true`, `{"type":"text"}` goes
  through the on-phone element layer and **any Unicode (incl. CJK) lands
  cleanly** — just make sure a text field has focus. Without WDA it falls back
  to US keycodes: a Chinese/Pinyin IME then eats digits as candidate-selectors
  (`a1b2c3` → `啊不c3`) — switch the phone to the English ABC keyboard for
  literal ASCII, and don't attempt CJK at all.
- **WDA and iPhone Mirroring are mutually exclusive** (A/B-tested on hardware):
  the on-phone XCUITest runner monopolizes the device's remote session, so
  while `wda:true` the Mirroring window shows "Connection Interrupted" and the
  human's live video is replaced by ~2.5s stills — that's expected, not broken.
  Don't try to "fix" the mirror while WDA runs. Switch modes via the API:
  `POST /agent/mode {"mode":"mirror"}` (fully automatic: locks the phone,
  stops WDA, reconnects the mirror, ~10s) or `{"mode":"agent"}` (starts WDA;
  needs the phone unlocked once if it's locked). `GET /agent/status` reports
  the current `mode`.
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
