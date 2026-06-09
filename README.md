# iPhone Remote Panel

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
![Platform: macOS 14+](https://img.shields.io/badge/platform-macOS%2014%2B-lightgrey)
![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange)
![Streaming: WebRTC / H.264](https://img.shields.io/badge/streaming-WebRTC%20%2F%20H.264-success)

**Remote-control your iPhone from any web browser** — over macOS **iPhone Mirroring**,
with low-latency WebRTC video and near-native touch. A Rust daemon captures the Mirroring
window with **ScreenCaptureKit**, hardware-encodes it to **H.264** with **VideoToolbox**,
and streams it to iPhone Safari (or any browser) over **WebRTC** — injecting taps, swipes,
scrolls, and text back as continuous system events. AI agents, scripts, and bots can drive
the same phone through a simple **HTTP API**.

Think **Chrome Remote Desktop, but for your iPhone** — running entirely on your own Mac, no
third-party cloud.

## Features

- 📱 **Control an iPhone from a browser** — live screen with tap / swipe / scroll / type, on iPhone Safari or any desktop browser.
- ⚡ **Low latency** — hardware H.264 (VideoToolbox) over WebRTC, not screenshot polling.
- 🤚 **Near-native touch** — real scroll-wheel scrolling, keycode text input, Home / Spotlight / App-Switcher shortcuts.
- 🤖 **Agent-ready** — an HTTP API (`/agent/input`, `/agent/screenshot`) lets AI agents and scripts *see* and *drive* the phone.
- 🌐 **LAN or remote** — same Wi-Fi over your local network, or from anywhere via a Cloudflare tunnel + TURN.
- 🔒 **Self-hosted & authenticated** — password login; runs on your own machine, your screen never leaves your control.

> v2 — a full WebRTC + hardware-codec + continuous-input rebuild of the original
> v1 screenshot-polling server. The input + video vertical (video, tap, scroll,
> text, shortcuts, LAN WebRTC) is validated on real hardware.

## Architecture

![Architecture](assets/architecture.png)

A Rust daemon captures the macOS iPhone Mirroring window with **ScreenCaptureKit**,
hardware-encodes it to **H.264** with **VideoToolbox**, and streams it over **WebRTC**
(`webrtc-rs`, axum for HTTP/WS signaling). The same capture/input core serves two
front-ends: a **human client** (iPhone Safari — live video + continuous touch) and an
**agent client** (an HTTP control API; see [Agent API](#agent-api)). Touch is injected
back as continuous `CGEvent`s through the system HID event tap. STUN handles most NAT;
optional Cloudflare TURN relays the rest.

Key input findings baked into the daemon (all hardware-validated):

- **Scroll is a wheel event.** iPhone Mirroring reads a mouse-drag as a long-press /
  icon-reorder and never scrolls — a finger swipe must map to `CGEvent` scroll-wheel.
- **Text is keycodes, not Unicode.** Mirroring forwards virtual keycodes (and a *real*
  Shift key), not the `CGEvent` Unicode payload. (CJK needs an on-phone IME.)
- **HID taps need the Mirroring window frontmost** — the daemon re-asserts focus only
  when another app steals it.

### Deployment — a GUI-session LaunchAgent

![Deployment](assets/deployment.png)

ScreenCaptureKit (Screen Recording) and input injection (Accessibility) require TCC
grants tied to a signed identity **in the login session** — an SSH-spawned binary is
denied. So the daemon runs as a codesigned **LaunchAgent** in the desktop session,
granted once; SSH shells, agents, and the iPhone Safari controller all **connect to it**.

### Control lease — one cursor, one controller

![Control and input](assets/control-input.png)

HID-tap input drives the host Mac's **one real cursor** with the Mirroring window
frontmost. A mandatory **control lease** grants that single cursor to one controller at a
time (human or agent); the most recent actor holds control. Without the lease, human and
agent would corrupt each other's gestures fighting over the same cursor.

## Requirements

- macOS 14+ with **iPhone Mirroring** set up and signed in.
- Rust toolchain (to build) — `cargo`.
- **`cua-driver`** (external) for key / text / shortcut injection. Pointer input
  (tap / scroll) works without it; it is only needed for the keyboard and Home /
  Spotlight / App-Switcher shortcuts. Point the daemon at it with the `CUA_DRIVER`
  env var (an absolute path — a LaunchAgent runs with a minimal `PATH`).
- *(optional)* a Cloudflare TURN key for cross-network (cellular / remote) access.

## Install

Build, bundle into a signed `.app`, and register the LaunchAgent:

```bash
cargo build --release --bin iphone-remote
./scripts/make-app.sh                 # → ./iPhoneRemote.app
./install.sh ./iPhoneRemote.app       # signs, installs, writes the LaunchAgent
```

`install.sh` binds `0.0.0.0`, generates a password (or uses `$PHONE_REMOTE_PASSWORD`),
opens the Screen Recording + Accessibility panes to grant once, and prints the iPhone
connect URL. On the iPhone (same Wi-Fi) open **`http://<mac-lan-ip>:8787/phone`** and
enter the password.

### Run without installing (dev)

```bash
PHONE_REMOTE_HOST=0.0.0.0 PHONE_REMOTE_PASSWORD=secret \
  ./target/release/iphone-remote serve
```

## Configuration (environment)

| Variable | Default | Purpose |
|---|---|---|
| `PHONE_REMOTE_HOST` | `127.0.0.1` | Listen address (`0.0.0.0` for LAN). |
| `PHONE_REMOTE_PORT` | `8787` | Listen port. |
| `PHONE_REMOTE_PASSWORD` | *(none)* | Shared password (cookie login + agent bearer). |
| `CUA_DRIVER` | *(local path)* | Absolute path to the cua-driver binary (key/text/shortcut). |
| `PHONE_REMOTE_CF_TURN_KEY_ID` / `_API_TOKEN` | — | Cloudflare TURN key → ephemeral relay creds for cross-network. |
| `PHONE_REMOTE_TURN_URLS` / `_USERNAME` / `_CREDENTIAL` | — | Static TURN server (alternative to Cloudflare). |

## Agent API

Agents drive the phone by connecting in to the running daemon (never by spawning their
own input process — macOS makes a spawned child's events untrusted). Bearer auth =
`Authorization: Bearer <PHONE_REMOTE_PASSWORD>`.

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/agent/status` | Auth / health probe. |
| `POST` | `/agent/input` | One control message: tap / scroll / text / key / shortcut (normalized `[0,1]` coords). |
| `GET` | `/agent/screenshot` | Current phone screen as PNG. |

Full reference: **[`docs/agent-api.html`](docs/agent-api.html)**.

```bash
HOST=http://<mac-lan-ip>:8787; AUTH="Authorization: Bearer $PW"
curl -s -H "$AUTH" "$HOST/agent/screenshot" -o screen.png
curl -s -H "$AUTH" -X POST "$HOST/agent/input" -d '{"type":"shortcut","name":"home"}'
curl -s -H "$AUTH" -X POST "$HOST/agent/input" -d '{"type":"tap","x":0.5,"y":0.3}'
```

## Security notes

This tool exposes live phone control over the network. Treat the URL and password like
sensitive credentials.

- A password is mandatory when binding to the LAN (`install.sh` enforces it).
- HTTPS for remote access is terminated by a Cloudflare tunnel (the daemon serves plain
  HTTP and reads `X-Forwarded-Proto`); the session cookie is `HttpOnly` + `SameSite=Lax`.
- Don't leave payment apps, private chats, or 2FA screens open while exposing access.
- Stop / unload the LaunchAgent when not in use.

## Layout

- `crates/core` — capture, encode, coordinate/geometry, input injection, control lease.
- `crates/server` — the `iphone-remote` daemon: HTTP/WS, WebRTC, signaling, agent API, TURN.
- `web/index.html` — the iPhone Safari client (WebRTC viewer + touch).
- `install.sh`, `scripts/make-app.sh`, `deploy/` — packaging + LaunchAgent.
- `docs/` — design spec, runbooks, agent API reference, research notes.

## License

[MIT](LICENSE)
