# Shortcuts RPC bridge (experimental)

A second control layer on top of the UI automation: where the UI layer *sees and
taps* any screen, this layer reaches the iPhone's **native iOS APIs** (Health,
battery, location, Messages, HomeKit, any App Intents) through **iOS Shortcuts** —
fast, deterministic, structured JSON, no vision.

It does **not** replace UI automation. The agent picks the path: a registered
shortcut `verb` when one exists (fast path), the UI fallback otherwise (universal).

## The loop

```
trigger:  daemon writes {"verb":"battery","id":"abc"} to the Mac clipboard
          → Spotlight → type "iU Bridge" → return   (all validated primitives)
execute:  the shortcut reads the clipboard, dispatches on `verb`, runs the
          native action (Get Battery Level, Find Health Samples, …)
return:   the shortcut's "Get Contents of URL" POSTs the result to
          http://<mac>:44321/agent/inbox
          (Authorization: Bearer <token>, X-Phone-Control: 1)
collect:  agent POSTs /agent/inbox/drain with both headers, matches on `id`
```

## Endpoints (shipped)

| Call | Who | Purpose |
|---|---|---|
| `POST /agent/inbox` | the phone (shortcut) | deliver one JSON result; requires bearer auth and `X-Phone-Control: 1` |
| `GET /agent/inbox` | the agent | inspect pending results without consuming them |
| `POST /agent/inbox/drain` | the agent | atomically retrieve and consume pending results; requires bearer auth and `X-Phone-Control: 1` |

## Governance — one bridge, one registry

To avoid a mess of shortcuts:

- **One** `iU Bridge` shortcut on the phone is the only thing you install. New
  capabilities are **branches inside it**, not new shortcuts.
- [`registry.json`](registry.json) is the **single source of truth** for what
  verbs exist. Agents read the registry, not the phone.
- Agents **never create shortcuts on the fly** — adding a verb is a deliberate,
  reviewed change (edit the bridge + the registry, ship the iCloud link).
- First run of any native action prompts for permission once (Health, Location…);
  grant it once, then it replays forever — same "expensive once, free after" model
  as the UI layer's vision→script flow.

## Status

Experiment: the `battery` verb validates the full round-trip (zero permission,
zero privacy). Bridge shortcut iCloud link + the Health/location verbs land after
the round-trip is confirmed on hardware.
