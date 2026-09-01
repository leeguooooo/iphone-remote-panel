---
name: iphone-use
description: Use when a task needs a real iPhone — operating iOS apps that have no API (Apple Health, banking, IM apps), exporting on-phone data, tapping/typing/scrolling on the phone, or taking phone screenshots. Drives the iphone-use daemon's Direct/WDA HTTP agent API.
---

# iphone-use — drive a real iPhone

Control a physical iPhone through the [iphone-use](https://github.com/leeguooooo/iphone-use)
daemon: see the screen (`/agent/screenshot`), act on it (`/agent/input`), repeat.
The default backend is Direct: WebDriverAgent performs input on the phone and
the device-side screen service provides pixels. It does not need iPhone
Mirroring, Screen Recording, Accessibility, or the Mac cursor. The old
Mirroring path is an explicit compatibility backend only.

## Prerequisites

A Mac on your network running the daemon, with WDA and its 8100/9100 relays
configured for the intended iPhone. Direct control needs the phone unlocked,
trusted for development, and awake. Setup blockers are reported by status.

WDA itself has no authentication. Daemon bearer auth protects `/agent/*`, but
does not protect the phone's own `8100/9100` listeners from another routable
host. Use Direct only on a trusted, isolated network; when practical, turn off
iPhone Wi-Fi and keep the supported Mac relays on USB loopback.

```bash
HOST="${PHONE_REMOTE_URL:-http://127.0.0.1:44321}"
AUTH="Authorization: Bearer $PHONE_REMOTE_TOKEN"   # daemon password or PHONE_REMOTE_AGENT_TOKEN
MUTATION="X-Phone-Control: 1"                      # required on every state-changing POST
```

**Always probe first** — if this fails, stop and report (don't retry blindly;
5 consecutive auth failures lock you out for 30s):

```bash
curl -s -H "$AUTH" "$HOST/agent/status"
# {"ok":true,"backend":"direct","wda":true,"wda_actionable":true,
#  "wda_locked":false,"drivable":true,"device_state":"ready",
#  "screen_state":"waiting","released":false,"reconnecting":false,"hint":"",
#  "setup_blocked_on":"", ...}
```

**Gate on `drivable:true`.** `phone_target` is a legacy Mirroring-window field
and is not a Direct readiness signal.

- `device_state:"ready"` + `drivable:true` → safe to act.
- `device_state:"locked"` → ask the operator to unlock the iPhone and keep it awake.
- `device_state:"released"` or `released:true` → restart Direct/WDA once:

  ```bash
  curl -s -H "$AUTH" -H "$MUTATION" -H 'Content-Type: application/json' \
    -X POST "$HOST/agent/mode" -d '{"mode":"agent"}'
  ```

  With the bundled MCP server, call `phone_reconnect` instead. Then poll
  status; do not retry either path in a tight loop. Neither path accepts a
  transient UDID.
- `reconnecting:true` → first inspect `setup_blocked_on`. If it is non-empty,
  follow the concrete `hint`; do not blindly wait or send another reconnect.
  Otherwise report `setup_phase` / `setup_message`, then wait and poll. A first
  build after an Xcode update can take several minutes. Never send input until
  `drivable:true`.
- `device_state:"blocked"` or `"offline"` → read `hint` and
  `setup_blocked_on` (`warp|proxy|usb|trust|ddi|account`) and fix that blocker first.
- Never switch to `mode=mirror` as automatic recovery. Mirror is an explicit
  operator-selected compatibility mode.

## The API

| Call | Purpose |
|---|---|
| `GET /agent/status` | `{ok, backend, device_state, screen_state, wda, wda_actionable, wda_locked, drivable, released, hint, setup_blocked_on, setup_phase, setup_message, …}` — gate on **`drivable`** |
| `GET /agent/elements` | **Direct/WDA UI as text**: `{"snapshot":"…","elements":[{kind,label,identifier?,rect,depth,value?,enabled?,visible?,accessible?,focused?,placeholder?},…]}` — prefer this over screenshots. Indexes and snapshot tokens are valid only for this read. Add `?since=<prior snapshot>` to get `{"snapshot":…,"baseline":…,"delta":{added,changed,removed,unchanged}}` instead of the full tree (much cheaper on multi-step flows; unknown baseline falls back to the full tree). Both shapes carry a read-only `ax_stats` usability block — see **Vision fallback** below. With `PHONE_REMOTE_ELEMENTS_AFFORDANCES=1` on the daemon, rows also carry sparse `actions` (named `perform` affordances), `selected`, and `min`/`max` |
| `GET /agent/screenshot` | Current phone screen as a device-side PNG; no Mirroring session required |
| `POST /agent/input` | One action (JSON body, below); requires `X-Phone-Control: 1` |
| `POST /agent/actions` | One bounded, fail-closed sequence of `action`, `wait_for`, and short `pause` steps; Direct/WDA only; requires `X-Phone-Control: 1` |
| `GET /agent/intents` | Curated **semantic intents** registry (registered Shortcuts verbs). Empty list + hint when none are set up |
| `POST /agent/intent` | Dispatch one registered verb (`{"name":"battery","args":{}}`); requires `X-Phone-Control: 1`. Results arrive on `/agent/inbox`, matched by the returned `id` |

If a stale caller forgets the mutation header, the 403 response names
`required_header:"X-Phone-Control: 1"` and includes a retry hint. Correct the
request once; do not repeat the unactionable POST.

Actions — coordinates are **normalized [0,1]** over the phone screen
(`0,0` top-left, `1,1` bottom-right), so they're resolution-independent:

```bash
curl -s -H "$AUTH" -H "$MUTATION" -X POST "$HOST/agent/input" -d '{"type":"tap","x":0.5,"y":0.3}'
curl -s -H "$AUTH" -H "$MUTATION" -X POST "$HOST/agent/input" -d '{"type":"tap","label":"新备忘录"}'  # exact label must have one match; ambiguity sends nothing
curl -s -H "$AUTH" -H "$MUTATION" -X POST "$HOST/agent/input" -d '{"type":"tap","element":3,"snapshot":"<same elements response>"}'
curl -s -H "$AUTH" -H "$MUTATION" -X POST "$HOST/agent/input" -d '{"type":"scroll","x":0.5,"y":0.5,"dx":0,"dy":60}'
curl -s -H "$AUTH" -H "$MUTATION" -X POST "$HOST/agent/input" -d '{"type":"text","text":"Health"}'
curl -s -H "$AUTH" -H "$MUTATION" -X POST "$HOST/agent/input" -d '{"type":"key","name":"return"}'
curl -s -H "$AUTH" -H "$MUTATION" -X POST "$HOST/agent/input" -d '{"type":"shortcut","name":"home"}'      # home|spotlight
curl -s -H "$AUTH" -H "$MUTATION" -X POST "$HOST/agent/input" -d '{"type":"longpress","x":0.4,"y":0.6,"duration_ms":700}'
curl -s -H "$AUTH" -H "$MUTATION" -X POST "$HOST/agent/input" -d '{"type":"keyboard"}'                     # dismiss the on-screen keyboard
curl -s -H "$AUTH" -H "$MUTATION" -X POST "$HOST/agent/input" -d '{"type":"set_value","element":5,"snapshot":"…","value":"你好"}'  # write a field directly (clear-then-type; "" clears); no focus tap, no keyboard dance
curl -s -H "$AUTH" -H "$MUTATION" -X POST "$HOST/agent/input" -d '{"type":"scroll","element":7,"snapshot":"…","dy":120}'          # scroll INSIDE that element's rect — never strays into a neighboring scroll view
curl -s -H "$AUTH" -H "$MUTATION" -X POST "$HOST/agent/input" -d '{"type":"perform","element":9,"snapshot":"…","action":"increment"}'  # named affordance on that element: increment|decrement (wheel/stepper/slider), adjust (+"value"), toggle, menu (long-press menu), double_tap, two_finger_tap, scroll_to_visible, pinch, rotate, force_press
```

**Halve the round trips**: `POST /agent/input?return=delta` makes an applied
action settle briefly and return the post-action element change in the same
response — `{"ok":true,"snapshot":…,"baseline":…,"delta":{…}}` when the
baseline (the `since` query param, or the action's own `snapshot` field) is
still server-cached, else full `elements`. That replaces the separate
act-then-`GET /agent/elements` verify pair for routine steps; `delta_error`
alongside `ok:true` means the action applied but the read failed — verify with
a normal `GET /agent/elements`.

After typing into a web form the keyboard covers the page's own submit/next
buttons — send `{"type":"keyboard"}` to dismiss it before tapping them.
`shortcut:"switcher"` is unsupported in Direct/WDA: iOS does not expose an
App Switcher action that WDA can synthesize. Do not send it and claim success.

MCP alternative: the repo ships `iphone-use-mcp` (crates/mcp) with the
day-to-day safe subset: status, reconnect, screenshot, elements, coordinate
and snapshot-bound element taps, strict unique-label taps, scroll, text, named
keys, Home/Spotlight, and `phone_run_steps` for one guarded multi-step call.
Inside a sequence, `tap_locator` can act on the same strict
label/identifier/kind/value/focus/enabled/visible locator used by `wait_for`;
zero or multiple matches send no tap.
Maintenance and less common HTTP actions such as drag, app install/uninstall,
and target configuration are not exposed as native MCP tools.
The installed MCP binary also runs reviewed, versioned JSON without a model:
`iphone-use-mcp flow validate <file>` is offline, and
`iphone-use-mcp flow run <file> --input key=value` requires Direct plus
`drivable:true` before submitting the same guarded batch exactly once.
The browser's **流程** panel can record this v1 JSON without hand-writing it:
only acknowledged actions are kept, semantic labels are preferred, coordinate
gestures are marked fragile, and typed text becomes a named runtime parameter
without retaining the literal recorded value. Parameter values live only in
the current browser page or command invocation. When the
post-action element tree exposes a new unique identifier or foreground
application, the recorder adds a reviewed `wait_for` checkpoint instead of
relying on a fixed delay. It never copies arbitrary screen labels or values
into an automatic checkpoint because those may contain private content. A
recording that could not persist an action is an incomplete draft and cannot
run from the browser. A parameterized recording can be downloaded immediately,
but it cannot run until every required value is filled in.

## The loop: see → act → verify

1. **See**: when `drivable:true`, use `GET /agent/elements` first — it's text
   (10× cheaper than vision) and carries exact labels. Fall back to `screenshot`
   when you need pixels (images, maps, unlabeled UI).
2. **Act**: when two or more consecutive actions are already understood, safe,
   and verifiable, send the longest stable segment as one `phone_run_steps`
   call with page transitions guarded by `wait_for`. Use one atomic action only
   while exploring an unknown screen, waiting for human confirmation, or
   isolating a failed checkpoint. Prefer
   `phone_tap_element(element,snapshot)` after choosing by
   identifier/kind/label/state. `phone_tap_label` is safe only for an exact
   unique label; zero or multiple matches send nothing. Use raw coordinates
   only when the control has no semantic target.
3. **Verify**: `elements` (or `screenshot`) again → confirm the expected change
   before the next step. Treat a non-2xx read or an empty tree with `error` as a
   failed checkpoint, even if an immediately preceding status said
   `drivable:true`; current daemons revoke cached actionability on either read
   path failure.

Operational rules. Hardware evidence is called out only where it exists; a
documented or unit-tested action is not automatically a current-device proof:

- **Scroll**: positive `dy` reveals content farther down; negative `dy` reveals
  content above. Positive `dx` reveals content to the right. A scroll is an
  atomic WDA swipe, not a stream of wheel events.
- **Text input** — focus and verify a field first, then `{"type":"text"}`.
  Direct/WDA sends Unicode on-device, so ASCII and CJK land without touching
  the Mac clipboard or keyboard.
- **Named keys** — the Direct implementation supports `return`/`enter`,
  `escape`, `space`, `tab`,
  `delete`/`backspace`, and the four arrows. Unsupported names must be treated
  as errors, not as successful no-ops. Re-verify these on the target iOS/WDA
  combination before relying on them in a destructive workflow.
- **Shortcuts** — Direct supports `home` and `spotlight`. `switcher` is
  unsupported. Use a supported app-launch action instead of inventing a gesture.
- **WDA and iPhone Mirroring are mutually exclusive** (A/B-tested on hardware):
  the on-phone XCUITest runner monopolizes the device's remote session, so
  while Direct is active any Mirroring window may show an interrupted state.
  That is expected. Do not try to repair or open Mirroring. Reconnect Direct
  with `phone_reconnect` or `POST /agent/mode {"mode":"agent"}`; it needs the
  phone unlocked. The target is canonical: change `PHONE_REMOTE_UDID`, rerun
  setup, and restart the daemon to switch devices. Never pass a one-off UDID
  during recovery.
- **`mode=agent` stuck / `wda` stays false → read `status.setup_blocked_on`**
  (`warp|proxy|usb|trust|ddi|account`). The #1 blocker is **`warp`**: Cloudflare WARP (or any
  VPN) wedges the CoreDevice tunnel xcodebuild needs when its effective Split
  Tunnel exclusions omit `fe80::/10` or the device RSD ULA range `fd00::/8`.
  If WARP is only needed for selected destinations, prefer **Traffic only** mode
  with Split Tunnels **Include** limited to those destination IPs/CIDRs. This
  avoids the Local proxy mode request timeout that can break long Git uploads.
  Local proxy mode remains route-safe for short explicit HTTP(S) traffic. If
  full-tunnel WARP is still required, add both IPv6 exclusions to the Zero Trust
  device profile. `warp-cli disconnect` is only the temporary alternative.
  Run `setup-wda.sh doctor` to distinguish the two states.
  KeepAlive retains the last concrete blocker while its next preflight pass is
  checking, so an empty value means the known prerequisite checks passed.
  `proxy` means an enabled macOS HTTP/HTTPS/SOCKS entry is malformed or points
  at a loopback port with no listener; start that proxy app or disable only the
  stale entry. `trust` = a one-time "trust the Apple Development cert" tap on
  the phone.
- **Never retry reconnect blindly.** Send `mode=agent` once, then poll status
  and follow `hint`/`setup_blocked_on`. Repeated bootstrap requests obscure the
  real USB, trust, DDI, or VPN blocker.
- **Do not batch guesses.** During discovery, keep one action between reads.
  Once a segment is understood, default to `phone_run_steps` or
  `POST /agent/actions` and combine up to 24 steps in one call instead of
  paying one model/tool round-trip per tap. Put semantic `wait_for` gates around
  page transitions and prefer `tap_locator` over coordinates or repeated labels.
  The daemon validates the complete batch first, holds one WDA control lock,
  and stops before every later step on the first failure. Fixed
  `pause`/`after_ms` values are capped at 3 seconds and are only animation
  settles, not proof that the right page appeared.
- **Treat `outcome_unknown` as possibly executed.** A 502/504 after dispatch
  can mean the phone acted but its acknowledgement was lost. Read elements or
  a screenshot before deciding whether to send the action again; never blindly
  replay text, scroll, back, payment, send, or delete actions.
- A reliable "reset to known state": `shortcut home`, then `shortcut spotlight`
  + `text <app name>` + `key return` to launch any app.

## Vision fallback: when the AX tree is unusable

AX-first is the invariant; vision is a screen-scoped fallback. Two failure
modes trigger it:

**Mode A — tree too sparse** (games, canvas/WebGL, custom-drawn UI).
`/agent/elements` succeeds; judge its additive `ax_stats` block
(`{n, n_interactive, labeled_frac, coverage, container_only, max_depth}`):

- **unusable → go vision**: `n_interactive == 0 && container_only` (e.g. the
  1-element tree whose only row is the `Application` node — `coverage` ≈ 1.0
  there is meaningless, never read coverage before those two gates).
- **degraded → hybrid**: `n_interactive < 3`, or `labeled_frac < 0.3`, or
  (`coverage < 0.3` and not `container_only`). Use AX for the rows it has,
  vision for the rest.
- Otherwise **usable**: stay AX-only. Legitimately sparse screens exist (video
  player with one "Done" button); low `n` alone is not a trigger — zero usable
  targets for your current intent is.

**Mode B — reading the tree kills the runner** (KakaoTalk, issue #44: any AX
hierarchy snapshot crashes on-phone WDA; recovery is a 1–3 min rebuild). The
signal: `/agent/elements` returns 502 `wda_source_failed` / 504
`wda_source_timeout` twice on the same foreground app while
`GET /agent/screenshot` still succeeds. Cache that verdict per app for the
session — do not re-probe and pay the rebuild again.

**The AX-free loop** (you are the grounding model; no new infra):

1. `GET /agent/screenshot` → reason over the pixels yourself and pick a target.
2. Act with the **existing** coordinate actions only — `tap` / `longpress` /
   `swipe`/`scroll` with normalized `[0,1]` coordinates. They dispatch via W3C
   `/actions` and never resolve the hierarchy. Unsure of the point (< ~0.5
   confidence)? Send nothing (that is `not_sent`, retry-safe): re-screenshot,
   crop the region to look closer, or scroll the target into view — then
   report with a screenshot if still lost.
3. Verify with a **post-action screenshot** (settle ~300–800 ms), never with an
   element read. **HARD RULE for Mode-B apps: no `?return=delta`, no
   element/label taps, no element `wait_for` gates** — each one resolves the
   tree and takes the device down. The loop must be hermetically AX-free while
   such an app is foreground. (In Mode A `return=delta` is merely useless — a
   game's tree doesn't change; screenshot diff is the verifier there too.)

**Degradation ladder** on the existing outcome grammar:

| Situation | Daemon says | Treat as |
|---|---|---|
| You abstained (low confidence) | nothing sent | `not_sent`, retry-safe: re-screenshot → crop → scroll → report |
| Action applied | `outcome:applied` | *dispatched*, not *achieved* — screenshot diff decides |
| 502/504 after dispatch | `outcome_unknown`, `retry_safe:false` | read a screenshot before any replay (same rule as ever) |
| `wda_pre_dispatch_failed` / transition | `not_sent`, `retry_safe:true` | retry after status settles |
| Applied but screen unchanged | `applied` | soft failure: one adjusted retry, then stop and report |

Vision guesses are *weaker* evidence than AX labels: destructive targets
(send / pay / delete / 2FA) keep their explicit-verification rules regardless
of channel. Any successful AX read on a new screen flips you back to AX-first,
and every successful vision sequence should be compiled into a flow per the
next section — vision is how you discover a flow, not how you run it the
tenth time.

## Semantic intents (registered Shortcuts verbs)

When the task maps to a verb in `GET /agent/intents` (check once per session),
prefer one semantic call over driving the UI: `POST /agent/intent` with
`{"name":"battery","args":{}}` opens the bridge shortcut's deep link on-device
and returns an `id`; the structured result lands on `/agent/inbox` (peek with
GET, consume with `POST /agent/inbox/drain`, match on that `id`). The registry
is deliberately small and human-curated — an empty list is the normal answer,
and then you use the UI channel. Caveats: the Shortcuts app **foregrounds
during the run** (never interleave with a mid-flight UI flow; re-orient after),
and the first run of each verb needs a one-time interactive permission blessing
on the phone — if a call dispatches but no inbox reply appears, a pending
permission dialog on the phone screen is the first suspect. At-most-once rules
apply unchanged: `outcome:"not_sent"` is retry-safe, `outcome:"unknown"`
(`intent_timeout`/`intent_dispatch_failed`) means check the inbox and observe
state before ever re-sending a side-effecting verb.

## Self-improvement: vision once → script forever

The first time you do a task, you're vision-guided (screenshot + reasoning at
every step). That's expensive. **Your job is to never pay that cost twice**:

1. **While solving, log intent and evidence, not only the wire payload** —
   record the intended target, the successful accessibility label/role, the
   precondition, the action, and the observed postcondition. A screenshot or
   snapshot element index is evidence from that moment, not a durable locator.
2. **Compile the successful trace into a guarded flow.** Prefer a fresh-resolved
   accessibility identifier, then a unique role + label + state, then a
   container/anchor relationship. Zero matches and multiple matches both fail
   closed. Never persist WDA element IDs, `/agent/elements` indexes, or snapshot
   tokens: they are valid only for the source read that produced them.
3. **Coordinates are the final fallback, not a reliability claim.** A
   normalized point can drift after an app update, A/B test, keyboard change,
   dynamic list reorder, orientation change, or a different phone. If a
   pixel-only action is unavoidable, bind it to a known screen signature and
   an immediate postcondition; refuse to run when those checks do not match.
4. **Wait for states, not fixed sleeps.** Poll a cheap element/status
   postcondition with a bounded timeout. Use a fixed delay only for a transition
   that has no observable state, and keep it short and explicit.
5. **Keep checkpoints, drop repeated reasoning.** The happy path should need no
   model and no screenshot tokens. On a failed checkpoint, stop and collect the
   last action, current elements, status, and one screenshot for repair. Patch
   the broken locator or branch and create a new flow revision; do not silently
   guess a replacement target.
6. **Respect at-most-once delivery.** Retry automatically only when the daemon
   proves `outcome:not_sent` and the action is still valid. For
   `outcome_unknown`, inspect current state before deciding. Never blindly
   replay text, scroll, back, payment, send, publish, comment, like, follow, or
   delete actions.
7. **Keep secrets and user data out of v1 flow files.** Explicit string inputs
   use `{"kind":"type","input":"query"}` and are resolved only for the current
   browser run or `flow run --input query=value`; the saved JSON never contains
   the value. CLI values may still appear in shell history or process
   inspection. Never use flow parameters for passwords, session tokens,
   one-time codes, private content, payment, send, publish, comment, like,
   follow, or delete actions.

The research and flow contract live in
`docs/scripted-flows-research.html`. `phone_run_steps` is the bounded in-memory
runner for stable segments; the release-matched `iphone-use-mcp` binary can now
validate and run a user-owned version-1 JSON file without a model. It does not
yet provide a managed flow store, branching, or repair bundles. The browser
does provide a first reviewed recorder/exporter, runtime string parameters,
and semantic wait suggestions; it never makes an uncertain batch replay-safe.

### Worked example: Apple Health full export (proven on hardware)

Apple Health has no API. This flow exports everything (weight, steps, sleep…)
as XML to the Mac, end-to-end ~2–4 min:

1. `shortcut home` → `shortcut spotlight` → `text "Health"` → `key return`
2. Tap the avatar (top-right of the Health summary page)
3. Scroll to the bottom of the profile (`dy:80` × a few, verify by screenshot)
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
```

If this skill's instructions ever disagree with the live API (an endpoint 404s
or a field is missing), the skill copy is probably stale. Rerun the installer:
it installs the daemon and skill from the same immutable release tag. Do not
run a floating global skill update that can separate their versions.

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
**Env**: daemon <version from /agent/status>, backend <direct|mirror>,
device_state <state>, <macOS/iOS if known>
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
- A human can preempt the shared device session at any time. If the screen
  changes under you mid-task, read elements/screenshot and re-orient instead
  of continuing the old plan.
- **Check before you type.** `text` lands in whatever field currently has focus —
  if the human is mid-chat, your words go into THEIR message box. Read
  `/agent/elements` (or a screenshot) first and confirm the foreground app is
  the one you intend to drive.
