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

**Status checks do not take control.** For initialization, status/health checks,
or when no unfinished user-requested task needs phone access, report the state
and stop. Do not reconnect, take a hold, or poll screenshots/elements to keep
the phone ready. Idle release is intentional.

**Gate phone operations and screen/UI reads on `drivable:true`.** The current
user-requested task must need that access. `phone_target` is a legacy
Mirroring-window field and is not a Direct readiness signal.

- `device_state:"ready"` + `drivable:true` → proceed with the requested task.
- `device_state:"locked"` → ask the operator to unlock the iPhone and keep it
  awake only if the current task needs phone access.
- `device_state:"released"` or `released:true` → expected idle state; leave it
  released unless the current task needs phone access. If that task cannot
  proceed, check `recovery_owner:"daemon"` and `hint` / `setup_blocked_on`.
  Once blockers are resolved and no release/reconnect is in progress, restart
  Direct/WDA once. This starts on-device automation, occupies the phone, and
  may require the operator to unlock it:

  ```bash
  curl -s -H "$AUTH" -H "$MUTATION" -H 'Content-Type: application/json' \
    -X POST "$HOST/agent/mode" -d '{"mode":"agent"}'
  ```

  With the bundled MCP server, call `phone_reconnect` instead under the same
  conditions. Then poll status until `drivable:true`; do not retry either
  path in a tight loop. Neither path accepts a transient UDID.
- `device_state:"releasing"` or `releasing:true` → do not reconnect or take a
  hold during release. Wait for it to finish only if the current task still
  needs phone access, then reassess status and the recovery conditions above.
- `reconnecting:true` → first inspect `setup_blocked_on`. If it is non-empty,
  follow the concrete `hint`; do not blindly wait or send another reconnect.
  Otherwise report `setup_phase` / `setup_message`, then wait and poll. A first
  build after an Xcode update can take several minutes. Never send input until
  `drivable:true`.
- `device_state:"blocked"` or `"offline"` → read `hint` and
  `setup_blocked_on` (`warp|proxy|usb|trust|ddi|account`). For a current task
  that needs phone access, resolve the blocker first; offline recovery follows
  the same task, ownership, and lifecycle conditions as released recovery.
- `device_state:"degraded"` → **not** a blocker; do not go looking for
  `setup_blocked_on`, it is empty by definition here. WDA answers, but the last
  read or action did not complete — usually a `/source` read that timed out on
  a heavy page, or a stalled app. Wait the `retry_after_secs` from the failing
  response (3s) and read again. The next health probe decides whether this
  clears back to `ready` or drops to `offline`; do not restart the service for
  it.
- Never switch to `mode=mirror` as automatic recovery. Mirror is an explicit
  operator-selected compatibility mode.

## The API

| Call | Purpose |
|---|---|
| `GET /agent/status` | `{ok, backend, device_state, screen_state, wda, wda_actionable, wda_locked, drivable, released, hint, setup_blocked_on, setup_phase, setup_message, …}` — gate on **`drivable`** |
| `GET /agent/elements` | **Direct/WDA UI as text**: `{"snapshot":"…","elements":[{kind,label,identifier?,rect,depth,value?,enabled?,visible?,accessible?,focused?,placeholder?},…]}` — prefer this over screenshots. Indexes and snapshot tokens are valid only for this read. Add `?since=<prior snapshot>` to get `{"snapshot":…,"baseline":…,"delta":{added,changed,removed,unchanged}}` instead of the full tree (much cheaper on multi-step flows; unknown baseline falls back to the full tree), plus `app_changed:{from,to}` if the foreground app moved since that baseline. Both shapes carry a read-only `ax_stats` usability block — see **Vision fallback** below — and a sparse `alert:{text,buttons}` block whenever a system alert (UIAlertController) is on screen. With `PHONE_REMOTE_ELEMENTS_AFFORDANCES=1` on the daemon, rows also carry sparse `actions` (named `perform` affordances), `selected`, and `min`/`max` |
| `GET /agent/screenshot` | Current phone screen as a device-side PNG; no Mirroring session required |
| `POST /agent/input` | One action (JSON body, below); requires `X-Phone-Control: 1` |
| `POST /agent/actions` | One bounded, fail-closed sequence of `action`, `wait_for`, and short `pause` steps; Direct/WDA only; requires `X-Phone-Control: 1` |
| `GET /agent/intents` | Curated **semantic intents** registry (registered Shortcuts verbs). Empty list + hint when none are set up |
| `POST /agent/intent` | Dispatch one registered verb (`{"name":"battery","args":{}}`); requires `X-Phone-Control: 1`. Results arrive on `/agent/inbox`, matched by the returned `id` |

If a stale caller forgets the mutation header, the 403 response names
`required_header:"X-Phone-Control: 1"` and includes a retry hint. Correct the
request once; do not repeat the unactionable POST.

**Give your own client at least 40s on `/agent/elements`.** The daemon keeps
rebuilding a failed source read until its own 35s deadline before answering
`wda_source_failed` — so a shorter client timeout (`curl -m 25`) cuts the
connection first and hands you an **empty body**, not JSON. That empty body is
your timeout, not a daemon fault and not an empty screen: re-read with a longer
one before concluding anything about the phone.

Actions — coordinates are **normalized [0,1]** over the phone screen
(`0,0` top-left, `1,1` bottom-right), so they're resolution-independent:

```bash
curl -s -H "$AUTH" -H "$MUTATION" -X POST "$HOST/agent/input" -d '{"type":"tap","x":0.5,"y":0.3}'
curl -s -H "$AUTH" -H "$MUTATION" -X POST "$HOST/agent/input" -d '{"type":"tap","label":"新备忘录"}'  # exact label must have one match; ambiguity sends nothing
curl -s -H "$AUTH" -H "$MUTATION" -X POST "$HOST/agent/input" -d '{"type":"tap","element":3,"snapshot":"<same elements response>"}'
curl -s -H "$AUTH" -H "$MUTATION" -X POST "$HOST/agent/input" -d '{"type":"scroll","x":0.5,"y":0.5,"dx":0,"dy":60}'
curl -s -H "$AUTH" -H "$MUTATION" -X POST "$HOST/agent/input" -d '{"type":"text","text":"Health"}'
curl -s -H "$AUTH" -H "$MUTATION" -X POST "$HOST/agent/input" -d '{"type":"key","name":"return"}'  # return|enter|send|go|search all fire the Return key — use this to submit a chat/search; coordinate-tapping a 3rd-party keyboard's 发送/前往 key ACKs but does NOT send
curl -s -H "$AUTH" -H "$MUTATION" -X POST "$HOST/agent/input" -d '{"type":"shortcut","name":"home"}'      # home|spotlight
curl -s -H "$AUTH" -H "$MUTATION" -X POST "$HOST/agent/input" -d '{"type":"longpress","x":0.4,"y":0.6,"duration_ms":700}'
curl -s -H "$AUTH" -H "$MUTATION" -X POST "$HOST/agent/input" -d '{"type":"keyboard"}'                     # dismiss the on-screen keyboard
curl -s -H "$AUTH" -H "$MUTATION" -X POST "$HOST/agent/input" -d '{"type":"set_value","element":5,"snapshot":"…","value":"你好"}'  # write a field directly (clear-then-type; "" clears); no focus tap, no keyboard dance
curl -s -H "$AUTH" -H "$MUTATION" -X POST "$HOST/agent/input" -d '{"type":"scroll","element":7,"snapshot":"…","dy":120}'          # scroll INSIDE that element's rect — never strays into a neighboring scroll view
curl -s -H "$AUTH" -H "$MUTATION" -X POST "$HOST/agent/input" -d '{"type":"alert","button":"不是 li guo?"}'  # press a system-alert button by exact name (UIAlertController; use this, NOT an element tap — alert buttons ACK a coordinate/element tap without acting)
curl -s -H "$AUTH" -H "$MUTATION" -X POST "$HOST/agent/input" -d '{"type":"alert","action":"dismiss"}'      # or the default accept/dismiss button
curl -s -H "$AUTH" -H "$MUTATION" -X POST "$HOST/agent/input" -d '{"type":"perform","element":9,"snapshot":"…","action":"increment"}'  # named affordance on that element: increment|decrement (wheel/stepper/slider), adjust (+"value"), toggle, menu (long-press menu), double_tap, two_finger_tap, scroll_to_visible, pinch, rotate, force_press
```

**Halve the round trips**: `POST /agent/input?return=delta` makes an applied
action settle briefly and return the post-action element change in the same
response — `{"ok":true,"snapshot":…,"baseline":…,"delta":{…}}` when the
baseline (the `since` query param, or the action's own `snapshot` field) is
still server-cached, else full `elements`. That replaces the separate
act-then-`GET /agent/elements` verify pair for routine steps.

**Read the `settle` block before you trust the delta.** The action result and
the observation are separate facts: `ok:true` means the action applied, and
nothing in the observation can take that back — a slow or failed read comes
back in `settle.reason` beside `ok:true`, never as an unknown outcome you have
to escalate.

| `settle.reason` | What it means | What to do |
|---|---|---|
| `stable` | Two consecutive reads matched over a tree with something in it. | The tree is worth reading — now check your postcondition against it. Stable is not "it worked". |
| `budget_exhausted` | Stability was not confirmed within the budget. The screen may still be moving, or `settle_ms` was too short (or `0`, which does not read the tree at all), or every read was container-only. | Do not conclude anything from the delta alone; re-read with `GET /agent/elements`. |
| `observation_failed` | The read path itself broke (plus the legacy `delta_error`). Running out of budget is *not* this — a cancelled read is not a broken one. | The action still applied. Verify with `GET /agent/elements`. |

Two more flags matter: `sparse:true` means the tree held nothing to act on
(empty, or containers only) — two identical bare reads are evidence you cannot
see the screen, not that it settled, so never read `sparse` + unchanged as
"nothing happened". `stale:true` means the tree in the response is the LAST
good read, not a fresh one — either the refreshing read broke, or the budget
cut it off mid-sample. And `settled:true` only ever claims *the tree
stopped changing* — it is never a claim that what you wanted actually
happened; that judgment stays yours.

`settle_ms`, `budget_ms` and `waited_ms` all describe the TREE read only; the
`alert` probe is separate and hard-capped at 1.5s. Tune with `settle_ms`
(default 1200, max 5000): raise it for a screen with a
push transition, drop it to `0` for a step you are going to verify by
screenshot anyway.

**A failed `wait_for` tells you whether anyone could see the screen.** Its
`observation` block distinguishes three things you must not conflate:
`read:false` (no tree was ever obtained — this proves nothing, least of all
that an element is absent), `read:true, stale:true` (the screen was read, then
the read path failed; you are looking at the last valid observation, not the
current screen), and plain `read:true` (the screen was read and the condition
genuinely was not met). `sparse:true` means the tree was empty or
container-only; every `absent` locator it would have "satisfied" is listed in
`absent_unproven` and the step deliberately does not pass. **Never conclude an
element is gone from a tree that had nothing in it** — take a screenshot
instead.

**`app_changed` means your tap landed in the wrong app.** Any delta response
(`?return=delta` or `GET /agent/elements?since=`) grows an
`app_changed:{from,to}` block when the frontmost app is not the one the
baseline was taken in. The usual cause is a banner notification dropping in
from the top and swallowing the tap, which opens *its* app — the delta then
describes a screen you never asked for. **Stop and re-orient when you see it**:
do not keep issuing steps against the old plan, and never treat the new
screen's elements as the ones you were aiming at. Recover by sending
`{"type":"home"}` and re-entering the intended app.
Its absence is not a guarantee — a tree with no `Application` row reports
nothing rather than guessing.

After typing into a web form the keyboard covers the page's own submit/next
buttons — send `{"type":"keyboard"}` to dismiss it before tapping them.
`shortcut:"switcher"` is unsupported in Direct/WDA: iOS does not expose an
App Switcher action that WDA can synthesize. Do not send it and claim success.

`{"type":"back"}` is a left-edge-swipe gesture, not a universal Back button: on
a screen with no in-app back target it can carry past the edge and **switch
apps** (e.g. drop you onto the Home screen or another app). Prefer resolving and
tapping the on-screen back control (`{"type":"tap","label":"…"}` / an element
tap) when one exists; use `back` only when you know the current screen has an
edge-swipe-back. For unattended runs, turn on a **Focus mode** first — a banner
notification dropping from the top will otherwise intercept a tap and open the
notifying app (hardware-seen: a chat banner hijacked a tap and opened WeChat).

MCP alternative: the repo ships `iphone-use-mcp` (crates/mcp) with the
day-to-day safe subset: status, capabilities, reconnect, screenshot, elements,
coordinate and snapshot-bound element taps, strict unique-label taps, scroll,
text, named keys, Home/Spotlight, and `phone_run_steps` for one guarded
multi-step call.
`phone_capabilities` answers two separate questions without touching the
phone: what this build supports for the configured backend, and whether the
device can be driven right now (`blocked_by` names locked / released /
reconnecting / human_handoff / owned_by_other / offline; `ok: null` means no
evidence either way). It wakes nothing and takes no owner lease, so it is safe
before deciding what to attempt. Its scope is UI control and observation only.
For those seven act tools and `phone_capabilities`, the JSON comes back as MCP
`structuredContent` and the text block is only a preview (trimmed at 8 KiB), so
parse the structured field. `phone_run_steps` puts its complete batch result in
both, so either is safe to parse. Every other tool keeps its original shape:
`phone_elements` and the `phone_flow_*` tools return complete JSON as text
(including `phone_flow_run`'s execution result, passed or failed), and
`phone_screenshot` returns an image. Errors raised before a call reaches the
phone are explanatory text instead. The rule is simply: read
`structuredContent` when it is there, otherwise read `content` according to the
tool, and call `phone_status` / `phone_capabilities` when you need a
machine-readable state. When a tool cannot confirm what happened it answers
`outcome: "unknown"` with `retry_safe: false` — the request may have reached the
phone, so check `phone_elements` or `phone_screenshot` rather than resending.
Decide by the explicit `retry_safe` boolean, not by `outcome`: a local refusal
and a daemon-reported `not_sent` are both valid evidence when they carry
`retry_safe: true`, and `not_sent` on one step never means a whole batch can be
replayed.

Every single-step act tool takes an optional `observe`. With `observe: true`
the daemon watches the screen settle and returns `settle`, `snapshot` and
`delta` alongside the result, so you do not need a separate `phone_elements`
round trip to see what an action produced. It is off by default because that
wait is latency the action does not otherwise pay. Read `settle.reason`:
`stable` means the screen stopped changing, `budget_exhausted` means the
observation window ran out and says NOTHING against the action itself, and
`observation_failed` means the read broke. `stale: true` marks a tree from the
previous successful read rather than the current screen; `sparse: true` marks
an empty or container-only tree, which is never reported stable.
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

## The registry contract: check it, feed it, fix it

The **official flow registry** ([`leeguooooo/iphone-use-flows`](https://github.com/leeguooooo/iphone-use-flows))
holds reviewed, deterministic per-app scripts that replay a whole task in one
call with no model and no screenshots. Every phone task you do has three
registry obligations. They are not optional politeness; they are how the next
run of the same task costs one call instead of thirty.

```bash
MCP="$HOME/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp"
"$MCP" flow update                     # once per machine; mirrors the registry
"$MCP" flow list [--category health]   # id · risk · verified · inputs · name
"$MCP" flow info health/export-all     # metadata + step templates
PHONE_REMOTE_TOKEN="$PHONE_REMOTE_TOKEN" "$MCP" flow run system/spotlight-search --input query=Health
"$MCP" flow publish my.json --as health/export-all-zh-cn --alias 健康 --note "iPhone 17 Pro Max, iOS 26"
"$MCP" flow report health/export-all --result @run.json --note "profile button is now '资料'"
```

MCP: `phone_flow_list / info / run / update / publish / report`.

**1. Before acting — check.** Call `phone_flow_list` (or `flow list`) before
driving any app step by step. `phone_elements` already tells you: its
`registry` block lists the installed flows for the app on screen and says when
the store is empty (`phone_flow_update` once). A matching flow beats
exploration every time. Read `verified` and `risk`: `verified: no` means nobody
has proved the file on a real phone yet — run it, then take one checkpoint
screenshot. `risk: side_effect` (send/publish/pay/delete) refuses to run without
`--confirm` / `confirm=true`; confirm only after the user approved the exact
target and inputs.

**2. After succeeding — feed.** When `phone_run_steps` completes 3+ steps, its
`registry.hint` reminds you: that sequence is a flow waiting to be saved. Write
it as v1 JSON (typed text → named `input`, page transitions guarded by
`wait_for`, `app` / `category` / `risk` / `locale` filled in, `verified_on` with
device, iOS, date), `flow validate` it, run it once from the file, then **tell
the user you would like to publish it** and, with their OK, call
`phone_flow_publish(confirm=true)` / `flow publish`. It forks if needed, adds
`app.json` for a new app (`aliases` = the app's foreground label in each
language, e.g. `Health`, `健康`, so `phone_elements` can surface it later),
rebuilds `index.json`, and opens the PR. Unverified files open as draft PRs.
Publishing is the default outcome of a successful multi-step task, not an
extra; skip it only for one-off or private tasks.

**3. On failure — fix.** A flow that stops (`failed_step`, `element_not_found`,
`missing_present`) is a registry bug until proven otherwise. Read
`phone_elements` to see where the phone stopped. If the flow is wrong (label
changed, app updated, locale mismatch), tell the user and, with their OK, call
`phone_flow_report(id, note, confirm=true)` / `flow report`: the last failure of
that id is already captured (failed step, redacted daemon result, daemon
version; screen labels and typed text are stripped). If you can also fix the
locator, publish the corrected file with a bumped `verified_on` and mention the
issue. Do not report a phone that was merely locked, offline, or on the wrong
app. Never replay a failed flow blindly — the first tap may already have acted.

**Compat is computed for you.** Every flow in `flow list` / `phone_flow_list` /
the `registry` block carries `compat`, derived from the flow's `verified_on`
app version (the iOS version for Apple system apps) against what this phone has
installed (`flow apps` shows the inventory):

| compat | what you do |
|---|---|
| `verified` | run it |
| `untested-newer` | the app updated since the last verification: run it, take one checkpoint screenshot, and if it worked publish the new `verified_on` |
| `incompatible` / `broken` | do not run (`flow run` refuses without `--force`): explore by hand, then publish the fixed flow |
| `needs-verification` | nightly re-verification failed and nobody fixed it yet: treat like broken, fixing it is the contribution |
| `draft` | no hardware record: run with a checkpoint, then publish `verified_on` |
| `unknown` | no version data (daemon has no `/agent/apps` and is not on loopback): behave as `untested-newer` |

Flow labels are locale-specific (`locale`): an `en` flow fails closed on a
Chinese phone with zero matches and no tap. Pick the variant that matches the
device language or record one (`health/export-all-zh-cn`). Keep your own flows
next to the official ones with `flow add <file> --as <app>/<name>`; they
survive `flow update`. Only the official source exists — there is no
`sources add`.

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
   only when the control has no semantic target. For stateful controls
   (Switch / Slider / Stepper / PickerWheel), prefer `perform` over any tap —
   see the Switches & sliders rule below; when a row advertises `actions`
   (affordances flag on), use the listed action instead of guessing a gesture.
3. **Verify**: `elements` (or `screenshot`) again → confirm the expected change
   before the next step. Treat a non-2xx read or an empty tree with `error` as a
   failed checkpoint, even if an immediately preceding status said
   `drivable:true`; current daemons revoke cached actionability on either read
   path failure.

Operational rules. Hardware evidence is called out only where it exists; a
documented or unit-tested action is not automatically a current-device proof:

- **System alerts** (permission prompts, "以 X 的身份设置?", sign-in confirmations):
  they show up as the `alert:{text,buttons}` block on `/agent/elements`, and a
  coordinate or element tap on an alert button is **acknowledged but often does
  nothing** (same false-success class as switches). Dismiss/answer them with
  `{"type":"alert","button":"<exact button text>"}` or
  `{"type":"alert","action":"accept"|"dismiss"}`.
- **One session per phone.** Name yourself on every state-changing request
  with `X-Phone-Owner: <session-name>` (the MCP server does this for you, from
  `PHONE_REMOTE_OWNER` or `mcp-<pid>`). Status then shows `owner` and
  `owner_lease_remaining_secs`. If `owner` is someone else, do not drive the
  phone: control calls answer `409 phone_owned` with the owner's name — wait,
  ask that session to `POST /agent/owner {"release":true}` when it is done,
  and never send `X-Phone-Owner-Takeover: 1` unless the user confirms the
  other session is abandoned. Release your own lease when your task ends.
- **Human-in-the-loop pauses** within a current user-requested phone task
  (waiting for the operator to type a password, approve a prompt, or fetch a
  code): use a bounded HTTP `POST /agent/hold {"secs":600}` before the pause
  if the task needs to retain the phone. Clear it with `{"secs":0}` when the
  pause ends or the task completes/is cancelled. Status reports
  `hold_remaining_secs`. A hold prevents idle release; it does not start WDA
  or prove readiness. Do not use or renew it for initialization, health checks,
  or to keep the phone ready without a task. There is no bundled MCP hold tool.
  A `503 device_release_in_progress` (with `Retry-After`) means release already
  started. Only if the current task still needs phone access, wait for release
  to finish and reassess status; reconnect once under the recovery conditions
  above, and take another bounded hold only if the task still needs that pause.
- **Switches & sliders** (hardware-verified, iOS 27): a coordinate tap on a
  Switch is **acknowledged but does not flip it** — a silent false success.
  The only reliable path is `{"type":"perform","element":N,"snapshot":"…",
  "action":"toggle"}`, which works on both the labeled full-row switch and the
  bare paired `UISwitch`. Sliders: `perform` `increment`/`decrement` steps
  ~10%, `adjust` takes a normalized `"value"` in 0..1. Verify the new `value`
  from a fresh `elements` read (or the `?return=delta` response) — never trust
  the ACK alone for state changes.
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
  only when the current task needs it and the recovery conditions above are
  met, using `phone_reconnect` or `POST /agent/mode {"mode":"agent"}`; it needs the
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
- **Reconnect only for a current task that needs phone access.** Status/health
  checks and initialization leave released/offline devices alone. Check
  `recovery_owner:"daemon"`, resolve `hint`/`setup_blocked_on`, and wait out any
  release/reconnect already in progress. Then send `mode=agent` once and poll
  status until `drivable:true`; stop recovery when the task no longer needs
  the phone. Repeated bootstrap requests obscure the USB, trust, DDI, or VPN
  blocker.
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
runner for stable segments; the release-matched `iphone-use-mcp` binary
validates and runs version-1 JSON without a model, and `flow update/list/info/
run <app>/<flow>` mirrors the official registry into `~/.iphone-use/flows`
(see **Registry first** above). A compiled flow that works should end up in the
registry: add `app`, `category`, `risk`, `locale`, and `verified_on`, then open a
PR to `leeguooooo/iphone-use-flows`. There is still no branching or repair
bundle; the runner stops at the first failed step. The browser
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
improves. Two repositories, two kinds of report:

- A **flow** from the registry failed or is missing → `phone_flow_report` /
  `flow report` (see **The registry contract**), or for a flow you wish existed,
  `gh issue create -R leeguooooo/iphone-use-flows -l new-flow -t "flow request: <app> — <task>"`.
- **iphone-use itself** is broken, confusing, or needlessly slow (NOT a
  task-level failure like a mistyped label) → the steps below.

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
