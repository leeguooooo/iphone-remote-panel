<p align="center">
  <img src="assets/icon-1024.png" alt="iphone-use icon" width="120">
</p>

<h1 align="center">iphone-use</h1>

<p align="center"><em>Computer-use, but for the iPhone — let AI agents (and your browser) see and drive a real phone.</em></p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-blue.svg" alt="License: MIT"></a>
  <img src="https://img.shields.io/badge/platform-macOS%2015%2B-lightgrey" alt="Platform: macOS 15+">
  <img src="https://img.shields.io/badge/built%20with-Rust-orange" alt="Built with Rust">
  <img src="https://img.shields.io/badge/default-WDA%20direct-success" alt="Default backend: direct WDA">
</p>

<p align="center">
  <strong>English</strong> ·
  <a href="README.zh-CN.md">简体中文</a>
</p>

<p align="center">
  <img src="assets/hero.png" alt="Viewing and controlling an iPhone from a browser" width="320">
</p>

A Mac-side daemon runs WebDriverAgent (WDA) on a USB-connected iPhone and exposes the
phone three ways:

| You are… | You use… | Start here |
|---|---|---|
| a person at a browser | `http://<mac>:44321/phone` — live screen, tap/type/scroll, a flow recorder | [Quick start](#quick-start) |
| an agent or script | the bearer-authenticated HTTP API under `/agent/*` | [Agent API](#agent-api) |
| Claude Code / Claude Desktop / any MCP client | the bundled `iphone-use-mcp` server, 21 tools | [MCP server](#mcp-server) |
| anyone repeating a task | a reviewed **flow** from the official registry — one command, no model | [Flows and the registry](#flows-and-the-official-flow-registry) |

Everything happens on the phone. The default `direct` backend does **not** use macOS
iPhone Mirroring, Screen Recording, Accessibility, the Mac cursor, or a frontmost window,
and it fails closed: when WDA is unavailable, control returns an error instead of moving
anything on the Mac. The older Mirroring path survives only as the explicit
`PHONE_REMOTE_BACKEND=mirror` [compatibility backend](#legacy-mirror-backend).

> Status: individual WDA element, text, tap, and screenshot capabilities have been
> exercised on real hardware. The end-to-end browser vertical still has an open
> [hardware acceptance matrix](#hardware-acceptance-boundary); a green build or a healthy
> `/agent/status` is not a substitute for that record.

## How it works

```text
Browser <── GET /agent/mjpeg ── iphone-use daemon ── 127.0.0.1:9100 ──┐
Browser ── POST /control ─────> iphone-use daemon ── 127.0.0.1:8100 ──┤ WDA on iPhone
Agent   ── /agent/* ──────────> iphone-use daemon ── 127.0.0.1:8100 ──┘
```

- `scripts/setup-wda.sh` builds and signs WDA, starts the XCUITest runner on the phone,
  and pins two `iproxy` loopback relays: `8100` for control, `9100` for the MJPEG screen.
  The daemon only ever talks to localhost, so a background process never holds the
  phone's changing IP. USB is the supported path; Wi-Fi/`socat` is a manual experiment.
- The browser gets the live picture from `/agent/mjpeg` (PNG stills as fallback) and
  sends input through `POST /control`, which answers success or failure for every
  command instead of accepting it blindly over a possibly dead channel.
- Agents read the accessibility tree as text (`/agent/elements`), screenshots as PNG,
  and act through `/agent/input` or a guarded multi-step batch (`/agent/actions`).
- The daemon owns the WDA lifecycle: it releases the phone after idle time, rebuilds
  WDA with backoff, and reports every state in `/agent/status`.

Design, lifecycle, failure states, and security boundaries:
**[`docs/direct-device-architecture.html`](docs/direct-device-architecture.html)**.

## Quick start

### Requirements

- macOS 15 or later, with **full Xcode.app** (Command Line Tools alone are not enough).
  Sign in under Xcode → Settings → Accounts and pick a development team; a free
  Personal Team works, but its WDA profile needs periodic renewal.
- An iPhone with **Developer Mode** on, paired with and trusted by the Mac over USB.
- The phone **unlocked and awake** while WDA is built, launched, and used. WDA cannot
  get past Face ID or the passcode.
- `iproxy` from `brew install libimobiledevice`.
- A Rust toolchain only if you build from source.

### Install and connect the first phone

```bash
curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh
```

The installer fetches the latest GitHub Release, registers a per-user LaunchAgent with
`PHONE_REMOTE_BACKEND=direct`, writes the loopback WDA endpoints, installs the matching
agent skill, and drops the setup helper at `~/.iphone-use/setup-wda.sh`. It does not
prove your team, phone, runner, and relays work together — that is the next step, with
the phone connected, trusted, unlocked, and awake:

```bash
~/.iphone-use/setup-wda.sh doctor    # explains any USB / trust / DDI / WARP blocker
~/.iphone-use/setup-wda.sh           # build, sign, install, launch WDA, start relays
~/.iphone-use/setup-wda.sh status
```

Then open **`http://<mac-lan-ip>:44321/setup`**. The built-in guide translates
`/agent/status` into the current blocker (USB, trust, developer service, WDA, external
host) without changing your VPN or running setup for you. Once the phone is drivable,
continue to **`/phone`** and enter the password `install.sh` printed.

More than one iPhone paired? Pin the same classic UDID in both places:

```bash
export PHONE_REMOTE_UDID=00008…
curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh
WDA_UDID="$PHONE_REMOTE_UDID" ~/.iphone-use/setup-wda.sh
```

### Hand the phone back to yourself

WDA occupies the phone while it runs. To use the phone by hand, pause managed WDA and
resume it before the next agent session:

```bash
~/.iphone-use/setup-wda.sh pause     # disables the launchd job, stops only PID-verified processes
~/.iphone-use/setup-wda.sh resume
```

Simpler still is the **交还 (hand off)** button in the web toolbar, or
`POST /agent/mode {"mode":"human"}`: the daemon stops the runner, opens iPhone Mirroring on
the Mac, and reports `human_handoff:true`; while that holds, agent input gets
409 `phone_handed_to_human` instead of restarting the runner under your fingers. Press
**重新连接** (reconnect), or send `{"mode":"agent"}`, to give the phone back to the agent.

**Operating the phone from somewhere else, as a person:** iPhone Mirroring only runs on
this Mac, so remote use means reaching the Mac — put the Mac and your device on Tailscale,
connect with macOS Screen Sharing, press hand off, and drive the Mirroring window. Driving
the phone from a browser without the Mac desktop (WebRTC video of the phone) is not built;
the WDA path is for agents — its latency and its unlocked-phone requirement do not suit a
person.

The daemon can also do this on its own: set `PHONE_REMOTE_IDLE_RELEASE_SECS` (for
example `300`) and it stops the runner after that long without agent activity or a
live viewer. Since v0.6.3 this is off by default — the runner stays up, so the next
request never waits for a rebuild. Either way the next agent request, or
`POST /agent/mode {"mode":"agent"}`, brings WDA back — unlock the phone if asked.

### Upgrade

The daemon checks GitHub daily and reports `version` / `latest` / `update_available` in
`/agent/status`; the web client shows a banner. Upgrading is the same one-liner as
installing. Details of what the installer verifies are under
[Operations → Upgrades](#upgrades).

**Unattended upgrades** are opt-in and gated on the phone being idle:

```bash
~/.iphone-use/auto-update.sh enable     # daily at 04:30; `disable` / `status` / `run --dry-run`
```

Each run resolves the latest release and upgrades only when it is newer *and* nobody
owns the phone (no `X-Phone-Owner` lease), no hold is active, the daemon is not
releasing/reconnecting, and no WDA session is up. Otherwise it logs one line to
`~/Library/Logs/iPhoneUse/auto-update.log` and tries again tomorrow. The upgrade itself is
`install.sh` with its SHA-256 checks and rollback. `run --force` skips the idle gate;
`run --reinstall` reinstalls the current release. (`scripts/auto-update.sh` self-installs on
`enable`; the installer will offer `--auto-update` once the multi-instance work lands.)

## Drive the phone from the browser

`/phone` renders the on-device MJPEG stream and turns your clicks, drags, long-presses,
scrolls, and typing (Unicode included) into acknowledged `POST /control` commands with a
bounded `ttl_ms`. Nothing steals Mac focus. The **Controls** panel shows the accessibility
tree so you can tap by exact label instead of by pixel.

The **流程** (flow) panel records what you do into a replayable flow file:

- Only acknowledged actions are recorded. Exact accessibility labels are preferred;
  coordinate gestures are marked fragile.
- Typed text becomes a named runtime input; the literal value is discarded and never
  written to the downloaded JSON.
- After each action the recorder diffs the element tree and, when a new unique
  identifier or a foreground-app change is provable, inserts a reviewable `wait_for`
  checkpoint; otherwise a short visible pause remains. It never copies arbitrary labels
  or values into a checkpoint, because they may be private.
- Review, reorder, delete, fill inputs, then download valid flow v1 JSON or run it once.
  A recording with unpersisted actions is labelled an incomplete draft and cannot run.
  Running requires every input filled and an explicit "no irreversible actions" check.
- **打开脚本** reopens a saved flow after strict client-side validation (same limits as
  the CLI; literal typed text is rejected in favour of named inputs).

What you record here is what the [registry](#flows-and-the-official-flow-registry)
distributes.

## Agent API

Full reference: **[`docs/agent-api.html`](docs/agent-api.html)**. The bundled skill
([`skills/iphone-use/SKILL.md`](skills/iphone-use/SKILL.md)) teaches an agent the loop.

### Authentication and headers

| Header | When | Meaning |
|---|---|---|
| `Authorization: Bearer <token>` | every `/agent/*` call | `PHONE_REMOTE_AGENT_TOKEN` if set; otherwise the daemon password (legacy fallback). |
| `X-Phone-Control: 1` | every state-changing POST | CSRF/intent guard on top of auth, not a replacement. Required by `/control`, `/agent/input`, `/agent/actions`, `/agent/mode`, `/agent/hold`, `/agent/owner`, and the POST forms of `/agent/inbox`. The web and MCP clients add it. |
| `X-Phone-Owner: <session>` | control requests | Claims the phone for this session (issue #72). While the lease is live (refreshed per request, `PHONE_REMOTE_OWNER_LEASE_SECS` default 300) other sessions — and header-less clients — get `409 phone_owned` with the owner and seconds left. Read-only calls are unaffected. `X-Phone-Owner-Takeover: 1` replaces a live lease and is logged. |

### Endpoints

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/agent/status` | Readiness and lifecycle: `backend`, `device_state`, `drivable`, `wda_actionable`, `recovery_owner`, `setup_blocked_on` / `setup_phase` / `setup_message`, `hint`, viewer counts, `instance`, `udid`, `owner` / `owner_lease_remaining_secs`, `hold_remaining_secs`, `version` / `latest`. |
| `GET` | `/agent/screenshot` | Current screen as PNG, from the phone. |
| `GET` | `/agent/elements` | Flattened accessibility tree with an ephemeral `snapshot` token, an `ax_stats` usability block, and a sparse `alert` block when a system alert is up. `?since=<snapshot>` returns a `delta` instead of the full tree. WDA missing/busy → `503`; failed source → `502`, never a fake empty `200`. |
| `GET` | `/agent/mjpeg` | Authenticated live MJPEG stream. |
| `POST` | `/agent/input` | One action: tap, drag, long-press, scroll, text, key, `home`/`spotlight`, `launch_app`, `set_value`, `perform`, `alert`. `?return=delta` also attempts a post-action tree read and returns the change plus a `settle` block (`settled`, `reason`: `stable` / `budget_exhausted` / `observation_failed`, `waited_ms`, `captures`, `budget_ms`, and `sparse` / `stale` when they apply). Observation is best-effort: a slow or failed read never downgrades an applied action to an unknown outcome. |
| `POST` | `/agent/actions` | Up to 24 `action` / `wait_for` / `pause` steps validated as a whole, run under one WDA lock, stopped at the first failure. Response: `completed`, `applied_actions`, `failed_step`, `outcome`, `retry_safe`. |
| `POST` | `/agent/mode` | `{"mode":"agent"}` restarts the configured Direct target. Never changes backend or UDID. |
| `POST` | `/agent/hold` | `{"secs":N}` (0 clears, max 14400) keeps the phone from idle release around a human pause. `503 device_release_in_progress` if release already started. |
| `POST` | `/agent/owner` | `{"release":true}` hands the owner lease back early. |
| `GET` | `/agent/intents` | The curated semantic-intent registry (see [Semantic intents](#semantic-intents-shortcuts-on-device)). |
| `POST` | `/agent/intent` | Dispatch one registered verb; the result arrives on `/agent/inbox`. |
| `GET` / `POST` | `/agent/inbox`, `/agent/inbox/drain` | Peek / append / atomically drain the Shortcuts result queue. |
| `POST` | `/control` | Cookie-authenticated browser input with a required 1–2500 ms `ttl_ms`. |

### Semantics an agent must respect

- **Gate on `drivable:true`** (and `backend:"direct"`, `wda_actionable:true`). `device_state`
  is one of `ready`, `locked`, `blocked`, `offline`, `releasing`, `released`,
  `reconnecting`. `phone_target`, `mirror_state`, `human_active` are legacy mirror fields.
- **At-most-once delivery.** Expiry before dispatch → `408 not_sent`, `retry_safe:true`.
  Transport failure after dispatch → `502`, post-dispatch deadline → `504`, both
  `outcome_unknown`, `retry_safe:false`: read the screen before doing anything again.
- **Snapshot-bound targets.** An element index is valid only with the `snapshot` from the
  same `/agent/elements` response; a changed tree fails with `409 stale_element_snapshot`.
  Exact-label taps fail closed on zero or multiple matches. Persist labels, identifiers,
  and locators in scripts — never indexes or snapshot tokens.
- **Element-scoped actions.** `set_value` writes a field (clear-then-type), `scroll` with
  `element` keeps the gesture inside that element, `perform` invokes a named affordance
  (`increment`, `decrement`, `adjust`, `toggle`, `menu`, `double_tap`, `two_finger_tap`,
  `scroll_to_visible`, `pinch`, `rotate`, `force_press`). With
  `PHONE_REMOTE_ELEMENTS_AFFORDANCES=1` the tree advertises which actions each row
  supports.
- **System alerts** are a separate surface: taps on their buttons are acknowledged
  without effect. Use `{"type":"alert","button":"…"}` or `{"action":"accept"|"dismiss"}`.
- **`/agent/actions`** never reports a replay as safe once any action applied.
  `tap_locator` uses the same exact label/identifier/kind/value/state fields as
  `wait_for` and requires one unique match.

```bash
HOST=http://<mac-lan-ip>:44321; AUTH="Authorization: Bearer $TOKEN"
MUTATION="X-Phone-Control: 1"; OWNER="X-Phone-Owner: my-script"
curl -s -H "$AUTH" "$HOST/agent/status"
curl -s -H "$AUTH" "$HOST/agent/screenshot" -o screen.png
curl -s -H "$AUTH" -H "$MUTATION" -H "$OWNER" -X POST "$HOST/agent/input" -d '{"type":"tap","x":0.5,"y":0.3}'
curl -s -H "$AUTH" -H "$MUTATION" -H "$OWNER" -X POST "$HOST/agent/input" -d '{"type":"text","text":"你好"}'
curl -s -H "$AUTH" -H "$MUTATION" -H "$OWNER" -X POST "$HOST/agent/actions" \
  -d '{"steps":[{"kind":"action","action":{"type":"shortcut","name":"home"}},{"kind":"wait_for","expect":{"present":[{"label":"搜索"}]},"timeout_ms":3000}]}'
```

### Semantic intents (Shortcuts, on-device)

For things the UI cannot reach efficiently (battery level, Health samples, sending a
message with a native confirmation), a curated set of **verbs** runs through one bridge
shortcut. The daemon opens `shortcuts://run-shortcut` on the phone via WDA — no Spotlight
or clipboard — and the shortcut posts its result back to `/agent/inbox`.

```bash
python3 deploy/make-bridge-shortcut.py --token "$PHONE_REMOTE_AGENT_TOKEN"
open "iU Bridge.shortcut"     # accept the import; iCloud syncs it to the phone
```

Verbs live in `~/.iphone-use/intents-registry.json` (start from
[`deploy/intents-registry.example.json`](deploy/intents-registry.example.json)); the
shortcut's name must equal the registry's `bridge.name`, and the bearer token lives in
the shortcut's own request headers. `--self-test` checks the plist parts that fail
silently. Each verb needs one interactive permission grant on first use, and Shortcuts
foregrounds during a call.

**The return path needs the phone to reach the daemon** (issue #59). The hardened
default `PHONE_REMOTE_HOST=127.0.0.1` can dispatch a verb but never hear the answer, so
intents are off until you choose one of:

| Return path | How | Trade-off |
|---|---|---|
| LAN bind | `PHONE_REMOTE_HOST=0.0.0.0` with the password **and** `PHONE_REMOTE_AGENT_TOKEN` set | Simplest; exposes the daemon's authenticated surface to your whole LAN. |
| USB reverse tunnel | Forward a phone-side port back to the Mac loopback listener | No LAN exposure; more moving parts. |

Fire-and-forget verbs work on plain loopback. Never bind `0.0.0.0` on an untrusted
network: WDA's own `8100`/`9100` have no authentication.

## MCP server

[`iphone-use-mcp`](crates/mcp/README.md) ships inside the installed app
(`~/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp`) and as a checksummed
standalone archive on every release. It speaks MCP over stdio and adds
`X-Phone-Control` and `X-Phone-Owner` (`PHONE_REMOTE_OWNER`, default `mcp-<pid>`) to
its daemon requests automatically.

```json
{
  "mcpServers": {
    "iphone-use": {
      "command": "/Users/YOUR_ACCOUNT/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp",
      "env": {
        "PHONE_REMOTE_URL": "http://127.0.0.1:44321",
        "PHONE_REMOTE_TOKEN": "<your-agent-token>"
      }
    }
  }
}
```

| Group | Tools |
|---|---|
| See | `phone_status`, `phone_capabilities` (what this build supports vs what is possible right now; wakes nothing), `phone_screenshot`, `phone_elements` (carries a `registry` block naming installed flows for the app on screen) |
| Act | `phone_tap`, `phone_tap_element` (snapshot-bound), `phone_tap_label` (unique exact label), `phone_scroll`, `phone_type` (CJK-clean), `phone_key`, `phone_shortcut` (`home`/`spotlight`) — each takes an optional `observe` |
| Batch | `phone_run_steps` — up to 24 steps incl. `tap_locator`, `launch_app`, `picker`, `alert`, long-press/swipe/drag, `wait_for` |
| Lifecycle | `phone_reconnect` (restart the canonical Direct target, never a UDID switch), `phone_hold`, `phone_release_owner` |
| Flows | `phone_flow_list`, `phone_flow_info`, `phone_flow_run`, `phone_flow_update`, `phone_flow_publish`, `phone_flow_report` |

For those seven act tools and `phone_capabilities`, the parsed JSON arrives as MCP
`structuredContent` and the text block is a preview trimmed at 8 KiB — parse the
structured field. `phone_run_steps` carries its complete batch result in BOTH, so either
is safe to parse. Every other tool keeps the return it always had: complete JSON as
text for most (including `phone_flow_run`'s execution result, passed or failed), an
image for `phone_screenshot`, and explanatory text for errors raised before a call
reaches the phone. Read `structuredContent` when it is present; otherwise read
`content` according to the tool. When a result cannot be confirmed,
`outcome: "unknown"` with `retry_safe: false` says so in a form a program can branch on
— and branch on the explicit `retry_safe` boolean, never on `outcome`. Full table:
[`crates/mcp/README.md`](crates/mcp/README.md).

`observe: true` on a single-step act tool asks the daemon to watch the screen settle and
return what changed (`settle`, `snapshot`, `delta`) with the result. It is off by default
because the wait costs latency an action does not otherwise pay. `settle.reason` is
`stable`, `budget_exhausted` (the observation window ran out — the action still happened)
or `observation_failed` (the read itself broke); `stale: true` means the tree returned is
the previous successful read rather than the current screen, and `sparse: true` marks an
empty or container-only tree, which is never called stable.

Keyboard dismissal, uninstall, and target configuration stay HTTP-only. Full schemas:
[`crates/mcp/README.md`](crates/mcp/README.md).

## Flows and the official flow registry

A **flow** is a strict JSON file (`version: 1`) with the same guarded steps as
`phone_run_steps` plus named string inputs. The `iphone-use-mcp` binary validates and
runs one without any model in the loop:

```bash
MCP="$HOME/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp"
"$MCP" flow validate examples/flows/search-spotlight.json          # offline
PHONE_REMOTE_TOKEN=… "$MCP" flow run examples/flows/search-spotlight.json --input 'query=coffee'
```

The **official registry** — [`leeguooooo/iphone-use-flows`](https://github.com/leeguooooo/iphone-use-flows),
the only supported source — turns this into an installable catalogue of reviewed per-app
flows, the way chrome-use ships site packs:

```bash
"$MCP" flow update                        # mirror into ~/.iphone-use/flows: sha256 + strict validation, 0600
"$MCP" flow list --category health        # id · risk · verified · inputs · name
"$MCP" flow info health/export-all-zh-cn  # metadata and step templates
PHONE_REMOTE_TOKEN=… "$MCP" flow run health/export-all-zh-cn
PHONE_REMOTE_TOKEN=… "$MCP" flow run health/export-all-zh-cn --artifacts-dir ./runs   # record the run (0700 dir, 0600 files)
"$MCP" flow add my.json --as myapp/daily  # your own flow; survives update
"$MCP" flow publish my.json --as myapp/daily --alias MyApp --note "iPhone 17 Pro Max, iOS 26"   # opens the PR via gh
"$MCP" flow report health/export-all --result @run.json --note "profile button renamed"           # files a flow-broken issue
```

When a run fails, the result grows a **`diagnosis`** block: the daemon's own
0-based `failed_step`, whether the screen could be read at all (`observable`),
a `reason` (`locator_matches_now`, `locator_no_match`, `locator_ambiguous`,
`still_present`, `no_similar_element`, `no_readable_tree`, `screen_unreadable`,
`diagnosis_timeout`, `step_has_no_locator`), and up to five
`candidates` with the `matched` / `differed` locator fields they were picked
on. It is one bounded read taken after the run: nothing is re-sent, the flow is
never silently edited, and the run's own `outcome` / `applied_actions` /
`retry_safe` are not touched by it.

`--artifacts-dir DIR` writes a machine-readable record of the run — schema,
flow name and sha256, the versions it ran against (`unavailable` when they
could not be read, never guessed), timings, and the projected result. The
directory is created `0700` and checked for writability *before* anything is
sent; files are `0600`. Structure only: typed input and screen text are never
written. If the write fails after the phone has already acted, the result is
still printed in full with an added `artifact_error` — a failure to record
something cannot rewrite what happened.

Registry metadata on a flow is optional: `app` (bundle id), `category`, `risk`
(`read_only` · `navigation` · `side_effect` — the last refuses to run without
`--confirm` / `confirm=true`), `locale` (labels are language-specific), `tags`, and
`verified_on` (hardware runs that proved the exact file). Files are pure JSON, so
installing the registry never executes code; a checksum or validation failure aborts the
whole update and leaves the store untouched.

Rules baked into the format: `--input KEY=VALUE` resolves only for the current run and is
never written back; a flow stops at the first failed step and never retries; command-line
values can appear in shell history, so inputs must not carry credentials, codes, or
private content, and send/publish/pay/delete actions must be declared `side_effect`.

An app update does not silently break the registry: each flow records the app (or iOS)
version it was proved on, the CLI reads what the phone has installed (`flow apps`), and
every listing shows a `compat` verdict — `verified`, `untested-newer`, `incompatible`,
`broken`, `needs-verification`, `draft`, `unknown`. `flow run` refuses broken or
incompatible flows without `--force`. A nightly canary (`scripts/flow-reverify.py`) re-runs
the verified read-only flows on a real phone and reaches one of three verdicts per flow:
**verified** (refresh `verified_on`), **failed** (tag `needs-verification` and file a
`flow-broken` issue), or **skipped** — the phone was locked, owned by someone else, not
drivable, or the daemon could not determine the outcome. A skipped flow is left exactly as
it was: a night the phone was unavailable says nothing about the flow, so it is neither
marked broken nor credited with a fresh date.

Agents are pushed toward the registry rather than asked to remember it: `phone_elements`
lists the installed flows for the app on screen, a 3+-step `phone_run_steps` success
suggests saving the sequence, and a failed `phone_flow_run` keeps the failure so
`phone_flow_report` needs only a note. The research behind the format is in
[`docs/scripted-flows-research.html`](docs/scripted-flows-research.html).

## Operations

### Lifecycle and recovery

`/agent/status` is the source of truth. `recovery_owner` is `daemon` for managed
loopback WDA, `unconfigured` until a first-run target is persisted, `external` for an
unmanaged endpoint. After a lock-screen failure the daemon rebuilds WDA with backoff from
30 s to 15 min rather than nagging for the passcode; other failures back off from 5 s to
5 min; a verified recovery resets both. Interactive setup waits at most 5 min for an
unlock. `POST /agent/mode {"mode":"agent"}` (or MCP `phone_reconnect`) restarts the
configured target once — do not loop it; read `hint` and `setup_blocked_on`
(`warp|proxy|usb|trust|ddi|account|locked`) first.

**Who may end a reconnect.** A bring-up is owned by the task that started it, and only
that owner ends it. Every begin mints a generation, so a late task cannot end the round
that replaced it, and `GET /agent/status` never ends one: reading status refreshes the
health cache but does not move the lifecycle. A wait ends for exactly one reason —
the phone became drivable, it is locked, setup published a prerequisite, the budget ran
out, or another round took over — and each is logged. The whole wait is bounded by that
budget: a probe is cut off by the absolute deadline rather than its own ceiling, evidence
that arrives after the deadline is discarded, and a wait that is cancelled (runtime
shutdown, a dropped future) releases its own round rather than leaving `reconnecting`
set. Evidence cached before a bring-up is never treated as proof that the bring-up
finished — that briefly shipped in v0.6.4 and opened input onto a runner that was still
being replaced.

### Upgrades

```bash
curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh
```

The installer resolves the release tag to one commit for helpers and the skill, fetches
the daemon app from the matching Release asset, checks its SHA-256, installs and
byte-verifies the skill at `~/.agents/skills/iphone-use` plus its Claude Code discovery
link, and only then replaces the daemon. A skill failure aborts the upgrade; a later
daemon failure restores the previous skill. `IPHONE_USE_SKIP_SKILL=1` leaves the skill
untouched (a degraded install with no compatibility claim). Migration is evidence-based:
an old plist with a valid loopback `PHONE_REMOTE_WDA_URL` moves to Direct, a legacy
install with no WDA configuration stays on Mirror, an explicit backend stays explicit.
`PHONE_REMOTE_NO_UPDATE_CHECK=1` disables the daily check.

Signing on install follows the backend: Direct keeps a valid existing signature and
repairs an invalid one with keychain-free ad-hoc signing (no TCC identity needed);
Mirror uses the stable local `iPhoneUse Local Signing` identity so TCC grants survive
upgrades, warning before any ad-hoc fallback.

### Configuration

| Variable | Default | Purpose |
|---|---|---|
| `PHONE_REMOTE_BACKEND` | `direct` | `direct` = WDA input + on-device MJPEG. `mirror` = legacy ScreenCaptureKit + CGEvent path. |
| `PHONE_REMOTE_HOST` / `PHONE_REMOTE_PORT` | `127.0.0.1` / `44321` | Listen address and port (`0.0.0.0` for LAN; a password is then mandatory). |
| `PHONE_REMOTE_PASSWORD` | *(none)* | Browser login; doubles as the agent bearer only when no agent token is set. |
| `PHONE_REMOTE_AGENT_TOKEN` | *(none)* | Dedicated agent bearer. When set, it is the **only** accepted bearer. |
| `PHONE_REMOTE_UDID` | detected and persisted by the installer | Canonical iPhone for managed WDA and destructive commands. Requests cannot switch it; change the deployment and restart. Pass the same value as `WDA_UDID` to setup. |
| `PHONE_REMOTE_WDA_URL` / `PHONE_REMOTE_WDA_MJPEG_URL` | `http://127.0.0.1:8100` / `:9100` | WDA control and MJPEG loopbacks. Direct fails closed when unreachable. |
| `PHONE_REMOTE_WDA_MANAGED` | on for loopback endpoints | Whether this daemon owns the WDA supervisor/relay lifecycle. |
| `PHONE_REMOTE_IDLE_RELEASE_SECS` | `0` | Stop WDA after this many idle seconds (`300` was the pre-v0.6.3 default); `0` keeps the runner up so a reconnect never rebuilds. |
| `PHONE_REMOTE_OWNER_LEASE_SECS` | `300` | How long an `X-Phone-Owner` lease lives without a refreshing request. |
| `WDA_RUNNER_ICON` | `auto` | Home-screen icon for the runner: `auto` reuses the app icon, `none` keeps WDA's placeholder, or a `.png`/`.icns` path. Failures only warn. |
| `PHONE_REMOTE_WDA_SNAPSHOT_MAX_DEPTH` | WDA default 50 | Bound the accessibility snapshot depth (try `20`–`30` for apps with huge trees, issue #44). |
| `PHONE_REMOTE_WDA_SNAPSHOT_TIMEOUT_S` | WDA default 15 | Bound snapshot resolution time so one oversized read fails instead of wedging the runner. |
| `PHONE_REMOTE_ELEMENTS_AFFORDANCES` | off | `1` adds sparse `actions`, `selected`, `min`/`max` to `/agent/elements` rows. |
| `PHONE_REMOTE_ELEMENTS_TRAITS` | off | `1` also emits raw accessibility trait names. |
| `PHONE_REMOTE_NO_UPDATE_CHECK` | off | Skip the daily release check. |
| `PHONE_REMOTE_CF_TURN_*`, `PHONE_REMOTE_TURN_*`, `PHONE_REMOTE_AUTO_RESUME` | — | Legacy mirror/WebRTC only. |

## Security

The daemon exposes live phone control over the network; treat its URL and password as
credentials.

- The password / cookie / bearer protects port `44321` only. **WDA's own `8100` and
  `9100` on the phone have no authentication**, and the USB `iproxy` relay does not add
  any — another host on the phone's Wi-Fi can reach them directly. Use Direct only on a
  trusted, isolated network; turning off iPhone Wi-Fi while on USB removes that exposure.
- A real authenticated device transport is Phase 2 (a companion app or a controlled
  tunnel). Until then, daemon login is not WDA protection.
- For remote access put `44321` behind an HTTPS tunnel you operate; the daemon serves
  plain HTTP, honours `X-Forwarded-Proto`, and sets an `HttpOnly` + `SameSite=Lax`
  session cookie.
- The owner lease (`X-Phone-Owner`) is coordination between cooperating sessions, not a
  security boundary.
- Do not leave payment apps, private chats, or 2FA screens open while exposing access.
  Stop the LaunchAgent when not in use.

### WARP / VPN

WARP and similar VPNs break the CoreDevice tunnel WDA needs. `setup-wda.sh doctor`
detects it and `/agent/status` reports `device_state:"blocked"`,
`setup_blocked_on:"warp"`; neither changes your VPN — that is an operator decision, and
managed Macs need an administrator split-tunnel rule.

WARP also breaks **iPhone Mirroring itself** with none of this running (issue #17,
reproduced on macOS 26 and 27.0 beta): Mirroring rides on Continuity, which the VPN
degrades. Before filing a bug here: stop our LaunchAgents
(`launchctl bootout gui/$(id -u)/com.leeguoo.iphone-use` and the `.wda` job), quit
Mirroring, `warp-cli disconnect`, reopen Mirroring. If it connects, the daemon was never
involved; an *Always On* Zero Trust policy will reconnect WARP by itself, so only an
administrator exclusion lasts.

## Legacy mirror backend

`PHONE_REMOTE_BACKEND=mirror` captures the iPhone Mirroring window with ScreenCaptureKit,
encodes H.264 with VideoToolbox, streams it over WebRTC, and injects input with CGEvent.
It needs Mirroring connected, Screen Recording and Accessibility grants, an Aqua login
session, and a frontmost-capable Mirroring window. The diagrams in `assets/` describe
this backend, not the default.

The **"iU Bridge" Shortcuts experiment** (`shortcuts/`) belongs to this backend: it
opens Spotlight and feeds clipboard/key events from the Mac. Its Direct-native successor
is the [semantic intents channel](#semantic-intents-shortcuts-on-device). App Switcher,
Control Center, and arbitrary Mac keycodes remain unsupported in Direct until they have
a device-native implementation.

## Development

```bash
cargo build --release --bin iphone-use --bin iphone-use-mcp
./scripts/make-app.sh                  # → ./iPhoneUse.app
./install.sh ./iPhoneUse.app           # sign, install, write the LaunchAgent (uses the worktree skill)

# or run the daemon without installing
PHONE_REMOTE_BACKEND=direct PHONE_REMOTE_WDA_URL=http://127.0.0.1:8100 \
PHONE_REMOTE_WDA_MJPEG_URL=http://127.0.0.1:9100 \
PHONE_REMOTE_HOST=0.0.0.0 PHONE_REMOTE_PASSWORD=secret ./target/release/iphone-use serve
```

| Path | What lives there |
|---|---|
| `crates/server` | daemon: WDA control, MJPEG proxy, browser `/control`, agent API, legacy mirror signaling |
| `crates/mcp` | `iphone-use-mcp`: MCP server, flow runner, registry client, `flow publish` / `report` |
| `crates/core` | ScreenCaptureKit, encoding, geometry, CGEvent — legacy mirror only |
| `web/index.html` | browser client (MJPEG + `/control` by default, WebRTC for mirror) |
| `skills/iphone-use` | the agent skill the installer ships |
| `scripts/`, `deploy/`, `install.sh` | WDA setup, packaging, LaunchAgent, bridge-shortcut generator |
| `docs/` | architecture, agent API reference, WDA setup, flows research |

### Roadmap

- [x] Direct/WDA element-tree control, Unicode text, label taps, on-device screenshots (component-validated on iPhone 17 / iOS 27; see [`docs/wda-setup.html`](docs/wda-setup.html)).
- [x] MCP server; release binaries in CI with a one-line installer.
- [x] Deterministic flows, the official flow registry, publish/report loop.
- [ ] Record the direct-browser hardware acceptance matrix below.
- [ ] Make first-device setup, signing renewal, sleep/reconnect recovery, and multi-device selection understandable from the product UI.
- [ ] Revalidate every advertised command against Direct; inherit no Mirroring capability claims by name.
- [ ] Phase 2 authenticated device transport (companion app or controlled tunnel).
- [ ] A short demo of an agent driving the phone.

### Hardware acceptance boundary

The direct browser default is accepted only after all of these are observed on a real
iPhone:

1. From a Mac without Screen Recording/Accessibility grants and without Mirroring, install, run WDA setup, and keep Direct up.
2. `/agent/status` reports `backend:"direct"`, `wda:true`, `wda_actionable:true`, `drivable:true` for the intended UDID.
3. `/phone` from another device shows a continuously updating picture; stopping the 9100 relay makes the UI report degraded/offline, not success.
4. Tap, drag, long-press, scroll, ASCII and CJK text through `/control` are each acknowledged and land exactly once.
5. `/agent/elements`, `/agent/screenshot`, `/agent/input` work through bearer auth, including with a failed WDA endpoint; no command ever moves the Mac cursor.
6. `releasing → released → reconnecting → ready` is observed, plus lock/unlock, USB reconnect, Mac restart, WDA renew/reinstall, and no silent target change on a multi-device Mac.
7. On an isolated network, record whether the phone's IP exposes unauthenticated `8100/9100`.

## Feedback

Rough edge? [Open an issue](https://github.com/leeguooooo/iphone-use/issues). AI agents
are explicitly invited: the bundled skill tells them to file structured issues (with the
user's consent) when the API misleads them, and to send flow problems to the
[registry](https://github.com/leeguooooo/iphone-use-flows/issues).

## License

[MIT](LICENSE)
