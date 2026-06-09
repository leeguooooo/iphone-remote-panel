# iPhone Remote Panel

A small authenticated web remote for macOS iPhone Mirroring.

## Architecture (v2 — WebRTC rebuild)

![Architecture](assets/architecture.png)

A Rust daemon captures the macOS iPhone Mirroring window with ScreenCaptureKit,
hardware-encodes it to H.264 with VideoToolbox, and streams it over WebRTC. The same
capture/input core serves two front-ends: a human client (iPhone Safari — live video +
continuous touch) and an agent client (Hermes and other agents via MCP). Touch is
injected back as continuous `CGEvent`s via the system session/HID event tap (the dashed
`session-tap` path). Cloudflare carries signaling (over the tunnel) and TURN for NAT
traversal.

### Deployment — a GUI-session LaunchAgent

![Deployment](assets/deployment.png)

ScreenCaptureKit (Screen Recording) and input injection (Accessibility) require TCC
grants tied to a signed identity **in the login session** — an SSH-spawned binary is
denied. So the daemon runs as a codesigned **LaunchAgent** in the desktop session,
granted once; SSH shells, Hermes, and the iPhone Safari controller all **connect to it**.

### Control lease — one cursor, one controller

![Control and input](assets/control-input.png)

Session/HID-tap input drives the host Mac's **one real cursor** with the Mirroring window
frontmost. A mandatory **control lease** grants that single cursor to one controller at a
time; a human touch **pre-empts** an in-flight agent gesture (the agent can still observe).
Without the lease, human and agent would corrupt each other's gestures fighting over the
same cursor.

> Phase 0 (capture + input) is validated on real hardware; see
> `docs/superpowers/specs/2026-06-09-iphone-remote-webrtc-design.md` and `SPIKE-RESULTS.md`.
> The sections below document the current (v1) screenshot-polling implementation, which
> still ships until v2 lands.

It serves a local page at `http://127.0.0.1:8787/phone` with:

- live iPhone Mirroring screenshots
- tap-on-screenshot control
- drag-to-swipe control
- Home / Search / App Switcher shortcuts
- text input forwarding
- optional 12×24 click grid overlay
- temporary password login
- optional Cloudflare quick tunnel for remote access

## Requirements

- macOS with iPhone Mirroring available
- [`cloudflared`](https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/) for public tunnel support
- Existing local helper commands:
  - `/Users/leo/bin/iphone-shot`
  - `/Users/leo/bin/iphone-act`
  - `/Users/leo/.local/bin/cua-driver`

The helper paths can be overridden with environment variables:

```bash
IPHONE_SHOT=/path/to/iphone-shot \
IPHONE_ACT=/path/to/iphone-act \
CUA_DRIVER=/path/to/cua-driver \
./scripts/phone-remote-start
```

## Start

```bash
./scripts/phone-remote-start
```

Example output:

```text
local=http://127.0.0.1:8787/phone
tunnel=https://example.trycloudflare.com/phone
password=<temporary-password>
server_pid=123 tunnel_pid=456
logs=/tmp/hermes-phone-remote/server.log /tmp/hermes-phone-remote/cloudflared.log
```

Open the local or tunnel URL and enter the temporary password.

## Stop

```bash
./scripts/phone-remote-stop
```

## Security notes

This tool exposes live phone control through a browser. Treat the tunnel URL and password like sensitive credentials.

Recommended usage:

- keep the server bound to `127.0.0.1`
- use temporary Cloudflare quick tunnels only when needed
- do not share the tunnel URL or password
- stop the service after use
- do not leave payment apps, private chats, or 2FA screens open while sharing the tunnel

The generated password/secret and logs live under `/tmp/hermes-phone-remote` by default and are not intended for commit.

## Files

- `phone_remote_server.py` — Python HTTP server and web UI
- `scripts/phone-remote-start` — starts the server and Cloudflare tunnel
- `scripts/phone-remote-stop` — stops the processes recorded in state files
