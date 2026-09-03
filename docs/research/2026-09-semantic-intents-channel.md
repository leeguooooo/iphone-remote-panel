# Semantic intents channel — App Intents / Shortcuts as a first-class agent path

*Research & design doc (no implementation). 2026-09. Target: iOS 27 / Xcode 26.6, daemon Direct/WDA backend.*

## 中文摘要

本文设计一条"语义通道"：当目标应用通过 App Intents / 快捷指令暴露了原生动作时，agent 可以直接调用它拿到结构化 JSON 结果，而不是靠视觉驱动 UI。核心发现：WDA 自带一个免会话的 `POST /url` 端点，守护进程可以用它在手机上直接打开 `shortcuts://run-shortcut?...` 深链，把现有实验桥的"剪贴板 + Spotlight 打字"触发方式替换成一次设备内调用，返回路径继续复用已有的 `/agent/inbox`。App Intents 本身无法从手机外部枚举或直接调用，所以语义通道必须以快捷指令为代理、以仓库内 `registry.json` 为唯一能力清单——这是诚实的边界，不是缺陷。该通道是纯增量的可选路径：注册了 verb 且桥健康时走语义调用，否则回落到现有 AX/快照令牌/受控批处理的 UI 通道，后者不做任何改动。结论：现在即可行（有限制条件），建议先做一次 `battery` verb 的 WDA-URL 触发端到端验证，约半天工作量。

---

## 1. The existing Shortcuts RPC bridge — how it actually works today

The repo already contains a working (experimental, mirror-era) RPC bridge. Reconstructed
from `shortcuts/README.md`, `shortcuts/registry.json`, and `crates/server/src/http.rs`:

### 1.1 Trigger path (legacy, Mirroring-only)

```
daemon writes {"verb":"battery","id":"abc","args":{}} to the *Mac* clipboard
  → (iPhone Mirroring shares the clipboard with the phone)
  → open Spotlight on the mirrored phone, type "iU Bridge", press return
  → the "iU Bridge" shortcut runs: reads the clipboard, dispatches on `verb`,
    executes the native action (Get Battery Level, …)
```

The trigger is **not** a URL scheme, not devicectl — it is ordinary UI automation
(Spotlight + typed text) plus the Mirroring shared clipboard as the parameter carrier.
That is why README.md §"Shortcuts bridge (legacy mirror experiment)" pins it to
`PHONE_REMOTE_BACKEND=mirror`: the Direct/WDA backend types text on-device, so the Mac
clipboard never reaches the phone, and the whole carrier disappears.

### 1.2 Return path (still shipped, backend-agnostic)

The return half lives in `crates/server/src/http.rs` and works today under any backend:

- `POST /agent/inbox` — the shortcut's *Get Contents of URL* action POSTs
  `{"id":"abc","verb":"battery","ok":true,"data":{...}}` to the daemon with
  `Authorization: Bearer <token>` + `X-Phone-Control: 1`. Non-JSON bodies are wrapped;
  each item is stored with a `received_at` timestamp in a bounded queue
  (`INBOX_CAP = 64`, oldest dropped).
- `GET /agent/inbox` — non-destructive peek.
- `POST /agent/inbox/drain` — atomic consume; the agent matches results on `id`.

### 1.3 Governance model (worth keeping verbatim)

`shortcuts/registry.json` is the deliberate answer to "how do we know what the phone can
do": **one** bridge shortcut ("iU Bridge") with branches per `verb`; the registry file in
the repo — not the phone — is the source of truth; agents never create shortcuts on the
fly; adding a verb is a reviewed change. One verb (`battery`) validated the round trip;
`health_*`, `location`, `send_message`, `create_reminder` are `planned`.

### 1.4 Limitations of the current design

1. **Dead under Direct.** The trigger needs Mirroring's shared clipboard and Mac-side
   key events. Direct/WDA is now the product path, so the bridge is effectively offline.
2. **Fragile trigger.** Spotlight typing is the least reliable primitive in the repo's
   own hardware history (Search-pill tap flakiness, CN IME hijacking typed characters).
3. **Clipboard as carrier** clobbers whatever the user had on the clipboard.
4. **No request/response correlation enforcement** — correlation is by convention
   (`id` matching after a drain); no timeout semantics, no outcome taxonomy
   (`outcome`/`not_sent`/`retry_safe`), unlike every other mutation path in the daemon.
5. **No liveness signal** — nothing tells an agent whether the bridge shortcut is
   installed, what version it is, or which verbs the installed copy actually implements.

---

## 2. Phone-side trigger mechanisms — feasibility from a Mac daemon

Evaluated against: unlocked requirement, confirmation prompts, parameter passing,
return values, headless operation.

### 2.1 `shortcuts://run-shortcut` deep link, opened on-device via WDA `POST /url` ★ recommended

WebDriverAgent (the appium-webdriveragent the daemon already manages) exposes
`POST /url` — verified against the current source (`FBSessionCommands.m`, appium-webdriveragent 9.x):

- Route: `[[FBRoute POST:@"/url"] respondWithTarget:... action:@selector(handleOpenURL:)]`,
  **sessionless** (also usable without an active WDA session).
- Body: `{"url": "...", "bundleId": "<optional handler app>", "idleTimeoutMs": <optional>}`.
- Implementation uses the modern `XCUIDevice`/system `open` path — no Safari detour.

So the daemon can execute, entirely on-device, over the already-relayed `:8100`:

```
POST http://127.0.0.1:8100/url
{"url":"shortcuts://run-shortcut?name=iU%20Bridge&input=text&text=%7B%22verb%22%3A%22battery%22%2C%22id%22%3A%22abc%22%7D"}
```

Apple documents `shortcuts://run-shortcut?name=[name]&input=[input]&text=[text]`:
`input=text` passes the URL-encoded `text` parameter as the shortcut's input —
**this replaces the clipboard carrier**; `input=clipboard` remains a documented
fallback for oversized payloads (with the clobbering caveat).

Properties:

| Question | Answer |
|---|---|
| Phone unlocked? | Yes — same requirement as the whole Direct channel (`drivable:true`). No regression, no new locked-phone capability. |
| Confirmation prompts? | No per-run confirmation for a manually-installed shortcut. One-time prompts: per-privacy-domain on first use of a sensitive action (Health, Location…), and once per destination host for *Get Contents of URL* ("Allow 'iU Bridge' to connect to `<mac>`?"). Same "expensive once, free after" model as the vision→script flow. |
| Parameters? | Yes, URL-encoded JSON via `input=text&text=…`. Practical URL length is finite (keep request payloads ≤ ~2 KB; larger args should be fetched by the shortcut from a daemon endpoint by id). |
| Return values? | Not via the URL itself. `x-callback-url` (`shortcuts://x-callback-url/run-shortcut?...&x-success=…`) only *opens another URL* on completion — it cannot carry structured results to a Mac. The existing `/agent/inbox` POST **is** the return path; keep it. |
| Headless? | Partially. The run itself needs no human, but the Shortcuts app comes to the **foreground** for the duration of the run — it steals the screen from whatever UI flow was in progress. |

### 2.2 `xcrun devicectl device process launch --payload-url` — fallback trigger

`xcrun devicectl device process launch --device <udid> --payload-url "shortcuts://run-shortcut?..." com.apple.shortcuts`
(iOS 17+, Xcode 15+) launches Shortcuts with the deep link from the Mac, over the same
CoreDevice pairing the daemon already depends on for WDA builds.

- Unlocked: required in practice for the app to come foreground and run.
- Prompts / params / returns: identical to 2.1 (it is the same deep link).
- Value: works when WDA is down (e.g. runner crash) but the CoreDevice tunnel is up.
- Caveats: inherits the WARP/VPN CoreDevice-tunnel fragility documented in the skill;
  spawning `xcrun` per call is slow (~seconds); `--payload-url` delivery has known
  platform quirks (documented broken on tvOS; verify on iOS 27 during the spike).
  Treat as a degraded fallback, not the primary path.

### 2.3 Direct App Intents invocation from outside the phone — **not possible**

There is no public mechanism for a Mac process to enumerate or invoke another app's
App Intents on a phone. App Intents are reachable only through on-device brokers: Siri,
Spotlight, widgets, the Action button, and the Shortcuts app. WWDC 2026 / iOS 27
("App Intents 2.0": streaming responses, multi-turn follow-ups, on-screen awareness;
SiriKit deprecated in favor of App Intents) changes none of this externally — if
anything it confirms Shortcuts/Siri as the only broker. **Conclusion: the semantic
channel must go through a Shortcut**, which can call any app's App Intent as an action
step. This is a hard boundary, and the design below treats it honestly: discovery is a
curated registry, not runtime enumeration.

### 2.4 Custom URL schemes of target apps

`POST /url` can equally open `things:///add?...`, `omnifocus://…`, etc. Fire-and-forget:
no return value, no completion signal, schema undocumented per app, and success is only
observable by reading the resulting UI (i.e. you are back to the AX channel for
verification). Useful as an *optimization inside* a UI flow (deep-link to the right
screen, then AX-verify), not as a semantic call. Out of scope for the RPC surface;
worth a follow-up `{"type":"open_url"}` action proposal with its own allowlist.

### 2.5 Push-based / automation triggers

iOS 27 added notification, screenshot, and keyboard-connection automation triggers, and
personal automations can run without per-run confirmation. In theory a
notification-triggered automation could run a bridge verb without foregrounding
Shortcuts, and some time-triggered automations run even locked. But the daemon has no
APNs path to the phone, delivery would ride on a third-party messaging app, latency and
reliability are uncharacterizable, and iCloud-synced automations are user-editable
state. **Not a foundation** — file under future work if a locked-phone read-only verb
ever becomes a requirement.

### 2.6 Summary matrix

| Mechanism | Unlocked needed | Prompts | Params in | Results out | Headless | Verdict |
|---|---|---|---|---|---|---|
| WDA `POST /url` → `shortcuts://run-shortcut` | yes (= `drivable`) | one-time grants only | URL-encoded JSON | via `/agent/inbox` | no human, but steals foreground | **primary** |
| `devicectl … --payload-url` | yes | same | same | same | same | fallback when WDA down |
| Direct App Intent invocation | — | — | — | — | — | impossible from outside; Shortcuts is the broker |
| Custom URL schemes | yes | varies | in URL | none | fire-and-forget | optimization only, out of scope |
| Automation/push triggers | sometimes not | none per-run | indirect | via inbox | yes | future work, not a foundation |

---

## 3. Proposed action surface (additive; no change to existing endpoints)

Naming note: the action type `"shortcut"` is **taken** — in `/agent/input` grammar it
means UI shortcuts (`home`/`spotlight`). The semantic channel uses `intent` everywhere
to avoid the collision.

### 3.1 `POST /agent/intent` — one semantic call, one bounded round trip

Dedicated endpoint rather than a new `/agent/input` type: a semantic call is
seconds-long (app launch + native action + phone POST), returns a payload rather than
an acknowledgement, and must serialize against UI actions (it steals the foreground) —
so it takes the same WDA control lock as `/agent/input` / `/agent/actions`.

Request (bearer auth + `X-Phone-Control: 1`, like every mutation):

```json
{
  "verb": "battery",
  "args": {},
  "timeout_ms": 15000
}
```

Daemon behavior (fail-closed at every step):

1. Validate `verb` against the shipped registry (unknown → reject, nothing sent).
2. Check verb `side_effect` class; generate a correlation `id` (server-side, never client-supplied).
3. Trigger: WDA `POST /url` with `shortcuts://run-shortcut?name=<bridge>&input=text&text=<urlencoded {"verb","id","args"}>`.
   If WDA is unreachable and devicectl fallback is enabled, use 2.2; otherwise fail `not_sent`.
4. Await a matching `id` on the inbox (internally — items consumed by the endpoint are
   not left for manual drains) up to `timeout_ms` (capped, e.g. 30 s).
5. Restore foreground: issue the existing on-device Home action, then report.

Response, success:

```json
{"ok":true,"verb":"battery","outcome":"applied","data":{"level":0.83,"charging":false},"elapsed_ms":2400}
```

Errors — same taxonomy as the rest of the daemon (`outcome` ∈ `applied|not_sent|unknown`,
`retry_safe` honest):

| `error` | `outcome` | `retry_safe` | Meaning |
|---|---|---|---|
| `intent_unknown_verb` | `not_sent` | `true` | verb not in registry; nothing dispatched |
| `intent_bridge_unavailable` | `not_sent` | `true` | WDA (and fallback) unreachable before dispatch |
| `intent_invalid_args` | `not_sent` | `true` | args fail the verb's declared schema |
| `intent_timeout` | `unknown` | `false` for `side_effect:"write"`, `true` for `"none"/"read"` | URL dispatched, no inbox reply in time — the native action may have run |
| `intent_bridge_error` | `applied` (the bridge ran) | per verb | bridge replied `{"ok":false,...}` — native action failed; bridge error passed through in `data` |

`intent_timeout` on a `write` verb is the semantic twin of `outcome_unknown` in the UI
channel: the agent must observe state (or a read verb) before retrying — never blind-replay
`send_message`-class verbs. This mirrors the at-most-once rules already in SKILL.md.

### 3.2 `GET /agent/intents` — discovery, honestly scoped

Returns the registry the daemon shipped with, plus liveness:

```json
{
  "bridge": {"name":"iU Bridge","required_version":3,
             "installed":"unknown|yes|no","installed_version":2,
             "last_ok_at":"2026-09-01T09:12:00Z"},
  "verbs": [
    {"verb":"battery","summary":"…","args_schema":{},"returns_schema":{},
     "side_effect":"none","permission":"none","status":"validated"},
    {"verb":"send_message","side_effect":"write","permission":"Messages",
     "status":"planned","confirm":"operator"}
  ]
}
```

How `installed` is known — three honest tiers, cheapest first:

1. **Mac-side `shortcuts list --show-identifiers`**: the Shortcuts library is
   iCloud-synced, so the Mac CLI can confirm a shortcut named "iU Bridge" exists in the
   account library *without touching the phone*. Sync lag applies; presence in the
   library ≠ present on this phone if sync is off. Report `"unknown"` when the CLI
   is unavailable or sync can't be assumed.
2. **`ping` verb round trip** (new registry verb, zero permissions): returns
   `{bridge_version}`; the daemon caches `installed_version` + `last_ok_at`. This is
   the only *proof*; run it lazily on first `/agent/intent` or on operator demand,
   never on a poll loop.
3. Per-verb `status` stays what the registry review process says (`experiment` /
   `validated` / `planned`) — the daemon never upgrades it at runtime.

**What is deliberately NOT promised:** enumeration of arbitrary third-party App
Intents. It is not technically possible from outside the phone (§2.3), and the
registry-as-reviewed-allowlist is also the security model: agents cannot invoke
native actions that a human didn't curate into the bridge.

### 3.3 Registry schema extension (`shortcuts/registry.json` v3)

Additive fields per verb: `args_schema` / `returns_schema` (JSON Schema subset),
`side_effect` (`none|read|write`), `min_bridge_version`, `confirm`
(`none|operator` — `operator` verbs, e.g. `send_message`, are rejected by
`/agent/intent` unless the request carries an explicit `"operator_confirmed":true`
that MCP only sets after a human confirmation, mirroring the flow-runner's
irreversible-action gate). Bridge gains a mandatory `ping` verb returning its version.

### 3.4 What does not change

`/agent/input`, `/agent/actions`, `/agent/elements`, snapshot tokens, the guarded batch
validator, `/agent/inbox` (kept for manual/legacy use and as the transport for the new
endpoint) — all untouched. The semantic channel is a new endpoint plus one small
`wda.rs` method (`open_url`, wrapping WDA `POST /url`). If later wanted inside
`/agent/actions` batches, an `intent` step kind can be added — but foreground-stealing
inside a UI batch interacts badly with `wait_for`, so that is explicitly future work.

---

## 4. Decision logic — when should an agent take which channel

Prefer the **semantic channel** when *all* hold:

- The task maps to a registered verb (check `GET /agent/intents` once per session).
- The desired output is data (battery, Health samples, location) or a well-defined
  native state change — not "look at a screen and decide".
- No UI flow is mid-progress that must keep the foreground (the intent call will steal
  and then Home-reset it).

Prefer the **UI channel** when any hold:

- No registered verb (the common case — the registry is deliberately small).
- The task is exploratory, interactive, or needs visual/AX verification of an
  app-specific screen.
- The verb is `write`-class and the operator confirmation for it is easier to review as
  a visible UI flow.

How a skill/MCP client learns availability: `GET /agent/intents` becomes part of the
session preamble next to `/agent/status`; MCP grows `phone_intents` (read) and
`phone_intent` (call) tools. The skill doc teaches the same waterfall it already
teaches for elements-vs-screenshot: *registry verb → semantic call; otherwise
see→act→verify.* A failed semantic call with `outcome:"not_sent"` may fall back to the
UI path automatically; `outcome:"unknown"` must first observe state.

---

## 5. Risks and open questions

1. **Foreground theft & mutual exclusion.** The run flashes the Shortcuts app on the
   physical phone (a human co-user sees it) and interrupts any UI-driving mid-flow.
   Mitigation: WDA control lock shared with `/agent/input`, Home-restore after, and
   skill guidance not to interleave.
2. **One-time permission dialogs.** First use of each privacy domain and the
   per-host network grant happen on the phone screen; on a headless rack phone these
   block until someone taps. Mitigation: an operator "bless the bridge" checklist in
   install docs (run each verb once interactively).
3. **User-mutable state.** The shortcut lives in the user's iCloud-synced library:
   renameable, editable, deletable, and sync can lag or resurrect old versions on a
   restore. Mitigation: `ping`/version handshake before trusting a verb;
   `min_bridge_version` per verb; treat mismatch as `intent_bridge_unavailable`.
4. **Secrets.** The request URL (`text=` payload) transits WDA logs and Shortcuts run
   history — it must never contain the bearer token or private args. The token lives
   only inside the shortcut's stored POST headers (as today); rotation requires
   re-editing the shortcut on-device (document this in the rotation runbook).
5. **No return values / unknown schemas for third-party intents.** A verb wrapping a
   third-party App Intent is only as good as that intent's output; many return nothing
   or an opaque entity. The registry review must record `returns_schema` honestly, and
   `returns_schema: {}` (nothing) is a valid, allowed answer — the agent then verifies
   via the UI channel.
6. **iOS version drift.** iOS 27 rebuilt Shortcuts around Apple Intelligence; the
   `run-shortcut` URL scheme is still documented, but foreground behavior, prompt
   wording, and the new automation triggers need hardware re-verification per major iOS
   (the spike below is that verification). App Intents 2.0's streaming responses do not
   reach the URL-scheme path.
7. **URL length limits** for `input=text` are undocumented; the spike should probe the
   practical bound and the design keeps args small by contract (≤ 2 KB, larger blobs
   fetched by the shortcut from the daemon by `id`).
8. **Inbox contention.** `/agent/intent` consuming matched items must not eat unrelated
   manual-bridge results; matching strictly on server-generated `id` prefix scoping
   (e.g. `intent-<uuid>`) keeps the legacy drain flow intact.

---

## 6. Feasibility verdict

**Feasible now, with caveats.** Every link in the chain is verified available: WDA's
sessionless `POST /url` (in the appium-webdriveragent the daemon already manages) can
open the documented `shortcuts://run-shortcut?name=…&input=text&text=…` deep link
on-device, parameters ride in the URL instead of the clipboard, and the shipped
`/agent/inbox` return path already works under Direct. The caveats are structural, not
blockers: the phone must be unlocked (`drivable:true`, same as the UI channel), the
Shortcuts app steals the foreground during a call, first-run permission grants need one
interactive blessing session, and discovery is a curated registry because App Intents
are not externally enumerable — Shortcuts is the only broker Apple provides.

## 7. Next-step spike (smallest end-to-end proof)

**Goal:** one `battery` round trip under the Direct backend, no Spotlight, no clipboard.

1. Edit the existing "iU Bridge" shortcut: take input from *Shortcut Input*
   (`input=text`) instead of the clipboard; keep the clipboard branch as fallback.
2. Add a `ping` branch returning `{"id":…,"verb":"ping","ok":true,"data":{"bridge_version":3}}`.
3. From the Mac (daemon host, WDA relay up), no daemon changes yet — drive it by hand:
   `curl -X POST http://127.0.0.1:8100/url -d '{"url":"shortcuts://run-shortcut?name=iU%20Bridge&input=text&text=%7B%22verb%22%3A%22battery%22%2C%22id%22%3A%22spike-1%22%7D"}'`,
   then `POST /agent/inbox/drain` and match `spike-1`.
4. Measure: trigger→inbox latency, foreground-steal duration, whether Home-restore is
   needed, prompt inventory on a fresh install, practical `text=` size limit
   (binary-search 1/2/4/8 KB), and the same trigger via
   `xcrun devicectl device process launch --payload-url … com.apple.shortcuts` with WDA stopped.
5. Record results in `SPIKE-RESULTS.md` style; only then implement `wda.rs::open_url`,
   `POST /agent/intent`, `GET /agent/intents`, registry v3.

**Effort:** spike ≈ half a day with phone in hand (coordinate with the hardware-test
session; steps 3–4 touch `:8100` and the daemon). Follow-up implementation of the two
endpoints + registry v3 + MCP tools + skill/README docs ≈ 2–3 days including hardware
acceptance.

---

### Sources

- Repo: `shortcuts/README.md`, `shortcuts/registry.json`, `crates/server/src/http.rs`
  (inbox routes ~L588–592, dispatch ~L3544, `/agent/actions` validator ~L4362),
  `crates/server/src/wda.rs` (`launch_app` L663), `README.md` §Shortcuts bridge, `skills/iphone-use/SKILL.md`.
- [Apple: Run a shortcut using a URL scheme](https://support.apple.com/guide/shortcuts/run-a-shortcut-from-a-url-apd624386f42/ios)
- [Apple: Use x-callback-url with Shortcuts](https://support.apple.com/guide/shortcuts/use-x-callback-url-apdcd7f20a6f/ios)
- [Apple: Run shortcuts from the command line (macOS)](https://support.apple.com/guide/shortcuts-mac/run-shortcuts-from-the-command-line-apd455c82f02/mac) · [Sync shortcuts on Mac](https://support.apple.com/guide/shortcuts-mac/sync-shortcuts-apdb3a4240b0/mac)
- [appium-webdriveragent `FBSessionCommands.m` (POST /url, sessionless)](https://cdn.jsdelivr.net/npm/appium-webdriveragent@9.0.6/WebDriverAgentLib/Commands/FBSessionCommands.m)
- [Apple Developer Forums: devicectl `--payload-url`](https://developer.apple.com/forums/thread/744765) · [tvOS payload-url quirk](https://developer.apple.com/forums/thread/775693)
- [WWDC26: What's new in Shortcuts](https://developer.apple.com/videos/play/wwdc2026/310/) · [TechCrunch: AI-built Shortcuts workflows](https://techcrunch.com/2026/06/08/apple-will-let-you-build-workflows-using-ai-in-its-new-shortcuts-app/) · [App Intents vs Shortcuts on macOS 27](https://elephas.app/resources/app-intents-vs-shortcuts) · [iOS 27 App Intents / agents strategy](https://ecorpit.com/ios-27-app-store-ai-agents-app-intents-developer-strategy-2026/)
