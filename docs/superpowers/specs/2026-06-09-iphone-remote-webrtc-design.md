# iPhone Remote Panel — WebRTC Rebuild (v2) Design

**Date:** 2026-06-09
**Status:** Approved for planning
**Supersedes:** v1 Python screenshot-polling server (`phone_remote_server.py`)

## 1. Problem

The v1 remote drives the macOS **iPhone Mirroring** window from a browser, but its
video link is a 1.2 s screenshot poll: each frame re-captures via an external CLI,
re-encodes a PNG, and ships it over HTTP. Latency and bandwidth are an order of
magnitude worse than native, and touch never feels connected — swipes are a single
"from→to" gesture with no intermediate points.

**Goal:** rebuild the video and input path with the same techniques Chrome Remote
Desktop uses — WebRTC + hardware video codec + continuous pointer injection — to get
near-native touch feel on **iPhone Safari**, while also exposing a first-class
control surface for agents (Hermes and others).

## 2. Decisions (locked)

| Axis | Decision | Rationale |
|---|---|---|
| Video transport | **WebRTC + hardware H.264** | Lowest latency; Safari-compatible; the CRD approach |
| Capture | **ScreenCaptureKit** (single window) | Apple's modern low-overhead window capture; proven in screenpipe |
| Encode | **VideoToolbox** H.264, low-latency profile | Hardware encoder; no B-frames; on-demand keyframes |
| Host stack | **Single Rust binary** | Matches GitHub-Release distribution habit; Rust+SCK proven; one artifact |
| NAT traversal | **P2P + Cloudflare TURN** | 1:1 control wants direct/relayed P2P, not an SFU extra hop |
| Input injection | **Direct CGEvent synthesis in Rust** | Continuous pointer stream → native-feel drag/swipe/inertia |
| Agent entry | **MCP server** | Fits the Hermes + cua-driver MCP ecosystem; high-level phone tools |
| Arbitration | **Mutex + takeover** (human pre-empts agent) | One controller at a time; a human touch wins; agent can still observe |

## 3. Architecture

A single Rust binary `iphone-remote`. A device-agnostic **core** (capture + input +
window) sits under **three front-ends** (human WebRTC, agent MCP, HTTP/signaling).
The core does not know whether a caller is a human or an agent — it only exposes
"produce frames" and "inject input".

```
            ┌──────────────────────── Rust binary: iphone-remote ────────────────────────┐
 real       │  core::window  ── locate iPhone Mirroring window (pid, bounds, scale)       │
 iPhone     │  core::capture ── ScreenCaptureKit stream of that window (CMSampleBuffer)   │
   ↕ mirror │       │                                                                     │
 Mirroring  │       ▼                                                                     │
 window     │  encode (VideoToolbox H.264)                                                │
   ▲ CGEvent│       │                          front::webrtc (per-viewer PeerConnection)  │
   │ inject │       ├── video track (H.264/RTP) ────────────────────────────────────────┐│
   └────────┤  core::input ◄── data channel (pointer/key/text) ──────────────────────────┘│
            │       ▲                                                                      │
            │       └── front::mcp  (observe/tap/swipe/type/screenshot/acquire/release)    │
            │  front::http (axum): web UI · login · WS signaling · TURN creds              │
            └───────────────────────────────────┬──────────────────────────────────────────┘
                              HTTP/WS signaling  │           UDP media (P2P / TURN relay)
                                  Cloudflare tunnel           Cloudflare TURN
                                                 │                     │
                              ┌──────────────────┴─────────┐   ┌───────┴───────────────┐
                              │ iPhone Safari (human)       │   │ Hermes / agents (MCP) │
                              │ <video playsinline> + touch │   │ tool calls            │
                              └─────────────────────────────┘   └───────────────────────┘
```

### 3.1 Module boundaries

| Module | Responsibility | Inputs | Outputs / deps |
|---|---|---|---|
| `core::window` | Find the iPhone Mirroring window; track resize/move/close | app-name + bounds heuristic (`iPhone`/`镜像`/`Mirroring`, 200–900 × 400–1600) | `{pid, window_id, bounds, scale}`; SCK or `cua-driver list_windows` |
| `core::capture` | SCK `SCStream` filtered to that window; deliver frames | window handle | `CMSampleBuffer` stream; restart on window change |
| `encode` | VideoToolbox H.264, realtime/low-latency, on-demand keyframe when a viewer joins | frame stream | Annex-B/AVCC NAL units → RTP packetizer |
| `core::input` | Map abstract events → `CGEventPostToPid` on window pid | `PointerDown/Move/Up`, `Key`, `Text` (normalized coords) | synthesized CGEvents |
| `front::webrtc` | One `PeerConnection` per viewer: H.264 track + data channel | SDP/ICE via signaling | webrtc-rs |
| `front::mcp` | MCP tools over the same core; screenshots pulled from the live pipeline | tool calls | rmcp (or hand-rolled) |
| `front::http` | Serve web UI, login, WS signaling, ephemeral TURN creds | HTTP/WS | axum |

### 3.2 Data flow

- **Control/signaling:** Safari ⟷ WS (behind Cloudflare tunnel) ⟷ Rust exchange SDP
  offer/answer + ICE candidates; ICE servers include Cloudflare TURN. Media is **not**
  proxied through the tunnel.
- **Video:** SCK frame → VideoToolbox H.264 → RTP → WebRTC video track → Safari
  `<video autoplay playsinline muted>`.
- **Input:** Safari pointer/key events → data channel → `core::input` → CGEvent →
  Mirroring window.

### 3.3 Coordinate mapping (human touch)

Client sends **normalized [0,1]** coordinates relative to the rendered video, never
device pixels. Host maps: `normalized → × window content size → + window origin →
screen point → CGEvent`. Normalization keeps the client resolution-agnostic and
survives window resize. Touch phases map: `touchstart → mouseDown`,
`touchmove → mouseDragged` (every intermediate point), `touchend → mouseUp`.

### 3.4 Arbitration (mutex + takeover)

`core` holds one `control_lock { holder: Human | Agent(id), last_active }`.
- Agent `tap/swipe/type` implicitly `acquire`s; a long agent task may `acquire_control`
  explicitly for exclusivity.
- A human pointer event in Safari **pre-empts**: it takes the lock, the agent receives
  a `preempted` signal, and the agent's subsequent injections are rejected — but the
  agent can still `observe` (read frames/screenshots).
- Idle timeout auto-releases the lock (default 30 s; configurable).

### 3.5 Agent MCP surface

Tools (high-level, phone-semantic — agents never compute raw screen coordinates):
`observe` (latest frame + window metadata), `screenshot` (JPEG from the live pipeline),
`tap(x,y)` / `swipe(direction,distance)` / `type(text)` / `key(name)` /
`shortcut(home|spotlight|switcher)`, `acquire_control` / `release_control`.
`swipe(direction, distance)` is **not** a coarse one-shot gesture: like the human
path, the host synthesizes intermediate `mouseDragged` points through `core::input`
so the same code path serves both front-ends.

**`cua-driver` dependency:** the default is to reimplement keyboard/text/shortcuts
in-process via CGEvent so the shipped binary has **no runtime dependency** on
`cua-driver` (window enumeration in `core::window` can fall back to its own SCK/AX
query). `cua-driver` is used during development for parity checks; whether any path
keeps shelling out to it at runtime is a plan-time decision, but the spec's intent is
a self-contained binary.

## 4. Security

- Reuse v1's **password + HMAC-signed session token** (`exp:nonce:sig`, default 8 h TTL,
  `hmac.compare_digest`). The token gates both WS signaling and MCP.
- **TURN credentials** are short-lived, issued via the Cloudflare API on demand — never
  baked into the binary or page.
- Cloudflare tunnel carries **signaling only** (HTTP/WS); UDP media stays P2P/TURN.
- Keep v1's headers: `Cache-Control: no-store`, `X-Frame-Options: DENY`,
  `Referrer-Policy: no-referrer`. Add `Secure` to the session cookie (tunnel is HTTPS).
- The binary requires **Screen Recording + Accessibility** TCC grants (same as cua-driver).

## 5. Deployment

- Ship a **single binary** `iphone-remote` via **GitHub Releases** + `install.sh`
  (`curl … | sh`), per the project distribution convention — no npm registry.
- Subcommands `serve` / `stop` replace the v1 bash scripts. `serve` launches the host,
  brings up the Cloudflare tunnel for signaling, and prints local URL / tunnel URL /
  password.
- State (pid, logs, ephemeral secret) stays under `/tmp/hermes-phone-remote`, uncommitted.

## 6. Latency budget (target)

capture ~16 ms + encode ~5–15 ms + network (P2P/TURN) ~10–50 ms + decode ~16 ms ≈
**50–100 ms** on a good network, excluding the inherent Mac↔phone Mirroring-layer
latency, which is outside our control.

## 7. Out of scope (YAGNI for v1 of v2)

- Cloudflare Calls **SFU** fan-out (multi-viewer) — P2P + TURN is enough for 1:1.
- Audio.
- Multi-device / multi-phone selection UI — single Mirroring window for now.
- HEVC/AV1 — H.264 for Safari compatibility first.

## 8. Open questions

- Exact Rust crates for SCK + VideoToolbox (`cidre` vs `screencapturekit` + `core-video`)
  and WebRTC (`webrtc-rs` vs `str0m`) — resolved during the plan/spike.
- Whether `front::mcp` uses an existing MCP crate (`rmcp`) or a thin hand-rolled
  stdio JSON-RPC loop.
```
