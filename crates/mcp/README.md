# iphone-use-mcp

MCP stdio server that wraps the `iphone-use` daemon's agent HTTP API,
giving MCP clients (Claude Desktop, Claude Code, etc.) native tool access
to a real iPhone. The default Direct backend uses WebDriverAgent on the phone;
the old iPhone Mirroring path is an explicit compatibility backend.

## Prerequisites

The `iphone-use` daemon must already be running on the Mac connected to the
configured iPhone. Direct also requires WDA and its USB loopback relays:

```bash
iphone-use serve
```

## Installation

The recommended installer puts a release-matched bridge inside the app:

```text
/Users/YOUR_ACCOUNT/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp
```

Every tagged GitHub Release also includes
`iphone-use-mcp-macos-universal.tar.gz` and its `.sha256` file for standalone
installation. Build from the workspace only for development:

```bash
cargo build --release -p iphone-use-mcp
# Binary at: target/release/iphone-use-mcp
```

## Configuration

| Env var | Default | Description |
|---|---|---|
| `PHONE_REMOTE_URL` | `http://127.0.0.1:44321` | Base URL of the daemon |
| `PHONE_REMOTE_TOKEN` | _(none)_ | Bearer token / password (omit for open-mode daemons) |

## MCP client config

### Claude Desktop (`~/Library/Application Support/Claude/claude_desktop_config.json`)

```json
{
  "mcpServers": {
    "iphone-use": {
      "command": "/Users/YOUR_ACCOUNT/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp",
      "env": {
        "PHONE_REMOTE_URL": "http://127.0.0.1:44321",
        "PHONE_REMOTE_TOKEN": "your-password"
      }
    }
  }
}
```

**Telling resident processes apart** (issue #46): with several agent sessions
open, `ps` shows many identical `iphone-use-mcp` rows. Add an optional
`"args": ["--label", "my-project"]` to the server entry — the tag is ignored
functionally but appears in `ps`/Activity Monitor argv, so each process is
attributable to its session or project.

### Claude Code (project `.claude/settings.json` or `~/.claude/settings.json`)

```json
{
  "mcpServers": {
    "iphone-use": {
      "command": "/Users/YOUR_ACCOUNT/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp",
      "env": {
        "PHONE_REMOTE_URL": "http://127.0.0.1:44321",
        "PHONE_REMOTE_TOKEN": "your-password"
      }
    }
  }
}
```

Remote Mac (daemon on a different machine on the LAN):

```json
{
  "mcpServers": {
    "iphone-use": {
      "command": "/Users/YOUR_ACCOUNT/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp",
      "env": {
        "PHONE_REMOTE_URL": "http://192.168.1.50:44321",
        "PHONE_REMOTE_TOKEN": "your-password"
      }
    }
  }
}
```

## Saved deterministic flows

After one exploratory run, save the stable steps in a versioned JSON file and
replay them without an MCP client or model. The installed MCP binary is also the
flow CLI:

```bash
MCP="$HOME/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp"
"$MCP" flow validate examples/flows/open-spotlight.json
PHONE_REMOTE_TOKEN="$TOKEN" "$MCP" flow run examples/flows/open-spotlight.json
PHONE_REMOTE_TOKEN="$TOKEN" "$MCP" flow run \
  examples/flows/search-spotlight.json --input 'query=咖啡'
```

Flow v1 uses the same step objects as `phone_run_steps`:

```json
{
  "version": 1,
  "name": "Search Spotlight",
  "description": "The query value is supplied only for the current run.",
  "inputs": {
    "query": {
      "type": "string",
      "description": "Spotlight search words for this run.",
      "required": true
    }
  },
  "steps": [
    {"kind":"shortcut","name":"home","after_ms":300},
    {"kind":"shortcut","name":"spotlight"},
    {
      "kind":"wait_for",
      "expect":{"present":[{"kind":"TextField"}]},
      "timeout_ms":3000,
      "poll_ms":100
    },
    {"kind":"type","input":"query","clear":true}
  ]
}
```

`flow validate` is offline and performs the same bounds checks as the MCP
adapter. `flow run` first requires a Direct daemon with `drivable=true`, then
submits one guarded batch. It never retries a failed or unknown result. For
reviewability and tamper resistance, the CLI rejects symlinks, non-regular
files, files owned by another uid, group/world-writable files, files over
64 KiB, unknown fields, and unsupported versions. Flow v1 supports explicit
string placeholders: `flow run --input KEY=VALUE` resolves them only for that
invocation before submitting the guarded batch. Missing, unknown, duplicate,
undefined, and unused inputs fail before any daemon or device action. Values
are never written back to the flow file. Command-line values can still appear
in shell history or process inspection, so never use this mechanism for
passwords, tokens, one-time codes, private message content, or payment actions.

The `/phone` browser UI can create this file without hand-writing JSON: open
**流程**, start recording, operate the phone, then return to review,
reorder, remove, and download the steps. Only acknowledged actions are
recorded. Typed text becomes a named runtime input while the literal text is
discarded; the browser asks for a fresh in-memory value before each one-shot
run and never writes that value to downloaded JSON. Exact accessibility labels
are preferred; coordinate taps, long-presses, swipes, and drags are visibly
marked fragile. Bounded before/after element-tree comparisons add a `wait_for`
checkpoint only when a new unique accessibility identifier or foreground
application can be proved; arbitrary labels and values are never copied into
automatic checkpoints because they may contain private content. Otherwise the
short fallback delay is shown for review. If another action could not be
persisted, the browser labels the download an incomplete draft and disables
one-shot execution. A parameterized flow can be downloaded before values are
filled, but browser execution remains disabled until every required runtime
value is present. Complete browser-side flows additionally require an explicit
no-irreversible-actions review checkbox. On a later visit, **打开脚本** imports the
saved JSON after strict client-side validation, restores the reviewable steps,
and asks for fresh runtime values. The browser importer deliberately rejects
literal `type.text` values and optional inputs: browser-replayable text must use
a required named input so a saved file cannot silently retain private content.

See [`examples/flows/open-spotlight.json`](../../examples/flows/open-spotlight.json).

### The official flow registry

[`leeguooooo/iphone-use-flows`](https://github.com/leeguooooo/iphone-use-flows) is the
single supported source of reviewed per-app flows. The CLI mirrors it into a local
store and runs flows by id:

```bash
"$MCP" flow update                      # fetch index.json + every flow, verify sha256, validate, write 0600
"$MCP" flow list [--category C] [--app A] [--verified] [--json]
"$MCP" flow info <app>/<flow>           # metadata, inputs, step templates, source, sha256
PHONE_REMOTE_TOKEN="$TOKEN" "$MCP" flow run <app>/<flow> [--input K=V]... [--confirm]
"$MCP" flow add <file> --as <app>/<flow> # install your own; kept across updates
"$MCP" flow remove <app>/<flow>
"$MCP" flow sources                     # official URL, override, store path
```

| | |
|---|---|
| Store | `$IPHONE_USE_FLOWS_DIR` or `~/.iphone-use/flows` (0700; files 0600; `.index.json` caches validated metadata) |
| Source override | `IPHONE_USE_FLOWS_SOURCE=<dir or https base>` for development and CI; there is no `sources add` |
| Flow id | `<app>/<flow>`, lowercase slugs; `flow run`/`validate` also accept a file path |
| Metadata | optional `app`, `category`, `risk` (`read_only`\|`navigation`\|`side_effect`), `locale`, `tags`, `verified_on[]` |
| Gate | `risk: side_effect` needs `--confirm` (CLI) or `confirm: true` (MCP); nothing is sent otherwise |

`flow update` aborts on the first checksum or validation failure and leaves the store
untouched; an official flow that disappears from the index is removed locally, flows
added with `flow add` are kept, and a local flow may not shadow an official id.

Contributing back is one command each way (both use the user's `gh` login and are
outward-facing, so the CLI/MCP never run them without being asked):

```bash
"$MCP" flow publish <file|id> --as <app>/<flow> [--alias Health --alias 健康] [--app-name "Apple Health"] [--note "..."] [--draft]
"$MCP" flow report  <app>/<flow> [--result @run.json | --result '<json>'] [--note "..."]
```

`flow publish` forks the registry if needed, adds the file (and `app.json` for a new app,
with `aliases` = the app's foreground label per language), regenerates `index.json`,
pushes a branch, and opens a PR (draft when `verified_on` is empty). `flow report` files an
issue with the failed step template and a redacted daemon result: `text`, `value`, `label`,
`candidates`, and `elements` fields are stripped so screen content and typed input never
reach a public issue.

The MCP also brings the registry to the agent: `phone_elements` responses carry a
`registry` block (installed flows for the app on screen, matched through `app.json`
`aliases`; or a nudge to run `phone_flow_update`), a `phone_run_steps` success of 3+ steps
carries a `registry.hint` to save it as a flow, and a failed `phone_flow_run` names
`phone_flow_report` and keeps the failure so the report needs only a note.

## Tools exposed

| Tool | Arguments | Description |
|---|---|---|
| `phone_status` | — | Query backend, canonical target, `drivable`, WDA readiness, lifecycle, and recovery hints |
| `phone_reconnect` | — | Restart daemon-managed WDA once, then poll `phone_status`; never changes backend or target |
| `phone_screenshot` | — | Capture current screen → PNG image content |
| `phone_elements` | — | **(wda)** The UI as a flattened element list — prefer over screenshots for reasoning |
| `phone_tap` | `x`, `y` (0–1) | Single tap at normalized position |
| `phone_tap_element` | `element`, `snapshot` | **(wda)** Snapshot-bound indexed tap; stale trees are rejected without tapping |
| `phone_tap_label` | `label` | **(wda)** Exact-label tap; requires one unique match and sends nothing on ambiguity |
| `phone_scroll` | `x`, `y`, `dx`, `dy` | Scroll-wheel gesture; negative `dy` scrolls content up |
| `phone_type` | `text` | Type text; with `wda:true` any Unicode (incl. CJK) lands cleanly, else US-ASCII keycodes |
| `phone_key` | `name` | Named key: `return`, `escape`, `space`, `tab`, `delete`, `up`, `down`, `left`, `right` |
| `phone_shortcut` | `name` | Direct/WDA system shortcut: `home` or `spotlight`; App Switcher is unsupported |
| `phone_run_steps` | `steps` | Run up to 24 guarded action/wait steps in one MCP call; includes long-press/swipe/drag, strict `tap_locator`, bundle-id `launch_app`, full preflight, one WDA lock, and first-failure stop |
| `phone_flow_list` | `category?`, `app?`, `verified?` | Installed registry flows with risk/verified/inputs — check this before exploring an app step by step |
| `phone_flow_info` | `id` | One flow's metadata, inputs, and step templates (never runtime values) |
| `phone_flow_run` | `id`, `inputs?`, `confirm?` | Run an installed flow once through Direct/WDA; `side_effect` flows require `confirm=true` |
| `phone_flow_update` | — | Mirror the official registry (checksum + strict validation); network only, phone untouched |
| `phone_flow_publish` | `source`, `id`, `app_name?`, `aliases?`, `note?`, `confirm` | Fork/branch/PR a validated flow into the registry via `gh`; `confirm=true` only after the user agreed |
| `phone_flow_report` | `id`, `note?`, `confirm` | File a registry issue for a failed flow using the captured last failure (redacted); `confirm=true` only after the user agreed |

## Typical session flow

0. `phone_flow_list()` — if a registry flow already does the task, `phone_flow_run(id)` and skip the rest
1. `phone_status()` — confirm `ok=true` and `drivable=true` (Direct does not use the legacy `phone_target` field)
2. `phone_elements()` — inspect semantic controls and keep its snapshot token
3. `phone_tap_element(element, snapshot)` — tap the chosen row; refresh if the snapshot is stale
4. Use `phone_screenshot()` + `phone_tap(x, y)` only for pixel-only controls
5. Once a segment is understood, combine it with `phone_run_steps()` and put a
   semantic `wait_for` between page transitions
6. Repeat from step 2 when a checkpoint fails

If managed Direct is released or offline, call `phone_reconnect()` once and
poll `phone_status()` until `reconnecting=false` and `drivable=true`. Follow
`hint` / `setup_blocked_on` instead of retrying in a loop. While setup is
making progress without a blocker, `setup_phase` and `setup_message` explain
the current stage (for example an Xcode build after an update).

## Implementation notes

- Uses the official [rmcp](https://crates.io/crates/rmcp) crate v1.7.0 with
  `transport-io` (stdio) and `schemars` features.
- All daemon errors surface as MCP tool errors with the daemon's HTTP status
  and body text.
- The client adds the daemon's required mutation guard header automatically.
  `phone_run_steps` is not a blind macro: the daemon validates the whole batch,
  caps waits and step count, excludes uninstall, and reports `retry_safe=false`
  whenever replaying the entire sequence could duplicate an applied action.
  Completed step results and the last failed wait observation are returned for
  repair. A transient stale WDA source session is rebuilt once within the same
  wait deadline.
  A `502`/`504` `outcome_unknown` may already have executed on the phone, so
  inspect the current screen before deciding whether to retry.
- Logs to **stderr** at `INFO` level by default; set `RUST_LOG=debug` for
  verbose output.  Stderr is forwarded to the MCP client's diagnostics view
  and never mixed with the stdio protocol stream.
