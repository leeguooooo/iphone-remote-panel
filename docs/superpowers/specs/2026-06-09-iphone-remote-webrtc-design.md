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
| Input injection | **CGEvent synthesis in Rust (primary), `cua-driver`/AX fallback** — gated on Spike S1 | Continuous pointer stream → native feel; primary path unproven until S1 (§9) |
| Agent entry | **MCP server** | Fits the Hermes + cua-driver MCP ecosystem; high-level phone tools |
| Arbitration | **Control lease + generation/cancellation token** (human pre-empts agent) | Lock never held across `.await`/FFI/gesture loop, so a human touch interrupts mid-gesture (§3.4) |
| Packaging | **Single host binary + external prerequisites** (`cloudflared`, and `cua-driver` until S1) | One built artifact for host logic; tunnel/input fallback are separate prereqs (§5) |

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
| `core::capture` | SCK `SCStream` filtered to that window; deliver frames | window handle | `CMSampleBuffer` stream. **Invariants/targets:** 30 fps default (cap 60, `SCStreamConfiguration.minimumFrameInterval`), bounded queue depth ~2–3 frames, drop late frames rather than buffer, **pass the `CMSampleBuffer` PTS through to the encoder/RTP** for pacing (don't re-stamp), restart `SCStream` on window-id/display change |
| `encode` | VideoToolbox H.264, realtime/low-latency | frame stream | NAL units → RTP. **Contract:** Constrained Baseline, `packetization-mode=1`, **no B-frames**, force IDR (`kVTEncodeFrameOptionKey_ForceKeyFrame`) on viewer join, **SPS/PPS in-band before every IDR** |
| `core::input` | Map abstract events → injected events on window pid | `PointerDown/Move/Up`, `Key`, `Text` (normalized coords) | injected events — **primary path `CGEventPostToPid`, fallback `cua-driver`/AX** until Spike S1 proves the primary (see §9) |
| `front::webrtc` | One `PeerConnection` per viewer: H.264 track + data channel | SDP/ICE via signaling | webrtc-rs |
| `front::mcp` | MCP tools over the same core; screenshots pulled from the live pipeline | tool calls | rmcp (or hand-rolled) |
| `front::http` | Serve web UI, login, WS signaling, ephemeral TURN creds | HTTP/WS | axum |

### 3.2 Data flow

- **Control/signaling:** Safari ⟷ WS (behind Cloudflare tunnel) ⟷ Rust exchange SDP
  offer/answer + ICE candidates; ICE servers include Cloudflare TURN. Media is **not**
  proxied through the tunnel.
- **Video:** SCK frame → VideoToolbox H.264 → RTP → WebRTC video track → Safari
  `<video autoplay playsinline muted>`. iOS Safari may still stall a one-way stream to a
  black element, so the client keeps an explicit **"connect/play" user gesture** that
  calls `video.play()` after the remote track attaches and treats a rejected `play()`
  promise as a real UI state, not a console error.
- **Input:** Safari pointer/key events → data channel → `core::input` → injected
  event → Mirroring window. Reliability/order are per-`RTCDataChannel`, so this needs
  **two separate channel objects**, not two message types on one channel: a
  **reliable + ordered** channel for `down`/`up`/`key`/`text`, and an
  **unordered + lossy** channel (`maxRetransmits: 0`, `ordered: false`) for high-rate
  `move`, which carries a sequence number so the host **drops stale moves**. A single
  reliable-ordered channel would head-of-line-block `move` and defeat the latency goal.

### 3.3 Coordinate mapping (human touch)

Client sends **normalized [0,1]** coordinates relative to the **displayed video
content rect** — not the `<video>` element box. The client must subtract `object-fit`
letterbox/pillarbox bars and ignore any element padding, so a tap on a black bar maps
to nothing rather than to an edge. Host maps: `normalized → × captured content size →
+ window content origin → screen point → CGEvent`.

To make this unambiguous, the host **publishes session metadata at connect** and on any
change: captured **frame size, content rect, backing scale, and orientation**. The
client recomputes its content-rect math on orientation change / resize. Normalization
keeps the client resolution-agnostic and survives window resize. Touch phases map:
`touchstart → mouseDown`, `touchmove → mouseDragged` (every intermediate point),
`touchend → mouseUp`.

### 3.4 Arbitration (control lease + cancellation token)

Not a long-held mutex — a mutex held across an `.await`, a blocking FFI call, or a
multi-event gesture loop would queue human input *behind* the agent instead of
pre-empting it. Instead:

- `core` holds `control { holder: Human(session_id) | Agent(id), generation: u64,
  last_active, pressed_state }` guarded by a **short** lock that is **never held across
  `.await`, blocking FFI, or an injection loop** — only long enough to read/swap the
  holder, bump `generation`, and snapshot `pressed_state`.
- `holder` carries a **session id even for humans** (one `PeerConnection` per viewer, so
  two browsers are both "Human"). **One active human controller at a time**: a second
  human is an observer until it explicitly preempts; human-to-human preemption uses the
  same generation bump as human-over-agent.
- Each injected event checks the current `generation` **before** it fires; a gesture
  loop (`move` stream, multi-key `text`) re-checks every iteration and aborts the moment
  its generation is stale.
- **Cleanup on preemption (no stuck pointer):** the host tracks the **pressed pointer/key
  state per generation**. If a holder is preempted mid-gesture (a `mouseDown` with no
  `mouseUp`, a held key), the handover path **synthesizes the matching `mouseUp` / key-up
  to release** before the new holder injects — the target window never lands in a
  permanently-pressed state.
- A human pointer event **pre-empts**: it swaps `holder`, bumps `generation`, runs the
  cleanup release, and the displaced agent receives a `preempted` event. A displaced
  party can still `observe`.
- Agent `tap/swipe/type` implicitly acquires; a long agent task may `acquire_control`
  for exclusivity. Idle timeout auto-releases (default 30 s; configurable).

### 3.5 Agent MCP surface

Tools (high-level, phone-semantic — agents never compute raw screen coordinates):
`observe` (latest frame + window metadata), `screenshot` (JPEG from the live pipeline),
`tap(x,y)` / `swipe(direction,distance)` / `type(text)` / `key(name)` /
`shortcut(home|spotlight|switcher)`, `acquire_control` / `release_control`.
`swipe(direction, distance)` is **not** a coarse one-shot gesture: like the human
path, the host synthesizes intermediate `mouseDragged` points through `core::input`
so the same code path serves both front-ends.

**`cua-driver` dependency:** a self-contained binary (in-process CGEvent for all
input, own SCK/AX window enumeration) is the **goal**, but it is **gated on Spike S1**
(§9): raw `CGEventPostToPid` driving the Mirroring window is unproven. Until S1 passes,
`cua-driver` stays as a **runtime fallback** for input, and the installer treats it as a
prerequisite. If S1 shows `CGEventPostToPid` only works with the window frontmost,
`core::input` must explicitly activate/focus the window before injecting (or keep the
`cua-driver` path). This fork is resolved by S1 before the plan commits a deployment
story.

## 4. Security

- Reuse v1's **password + HMAC-signed session token** (`exp:nonce:sig`, default 8 h TTL),
  compared in **constant time** (Rust: `subtle::ConstantTimeEq` or `ring`, not a plain
  `==` — v1's `hmac.compare_digest` is the Python equivalent). The token gates both WS
  signaling and MCP.
- **TURN credentials** are short-lived, issued server-side via the Cloudflare API on
  demand — never baked into the binary or page. TTL must exceed expected session length;
  the client **refreshes via `setConfiguration()` before expiry** and supports **ICE
  restart** so a mid-session credential rotation or path change doesn't silently drop the
  call.
- Cloudflare tunnel carries **signaling only** (HTTP/WS); UDP media stays P2P/TURN.
- Keep v1's headers: `Cache-Control: no-store`, `X-Frame-Options: DENY`,
  `Referrer-Policy: no-referrer`. Add `Secure` to the session cookie (tunnel is HTTPS).
- The binary requires **Screen Recording + Accessibility** TCC grants (same as
  cua-driver). These are **not passive dependencies** for a background daemon: `serve`
  runs a **preflight that fails closed** — `CGPreflightScreenCaptureAccess` /
  `CGRequestScreenCaptureAccess` for capture and `AXIsProcessTrustedWithOptions` for
  input — printing the exact System Settings panes and refusing to start until both are
  granted. The binary must ship with a **fixed codesign identity** so the grant sticks
  across upgrades (TCC keys on the executable's identity).

## 5. Deployment

- Ship a **single binary** `iphone-remote` via **GitHub Releases** + `install.sh`
  (`curl … | sh`), per the project distribution convention — no npm registry. "Single
  binary" refers to the host logic; **`cloudflared` remains an external prerequisite**
  (the `serve` health check verifies it exists before reporting a tunnel URL), and
  `cua-driver` is a prerequisite only until Spike S1 retires the input fallback. The
  installer documents both.
- Subcommands `serve` / `stop` replace the v1 bash scripts. `serve` runs the TCC
  preflight (§4), launches the host, brings up the `cloudflared` tunnel for signaling,
  and prints local URL / tunnel URL / password.
- State (pid, logs, ephemeral secret) stays uncommitted in a **per-user `0700` runtime
  directory** (e.g. `$TMPDIR/hermes-phone-remote-$UID`, not shared `/tmp`). Files are
  created **atomically** (`O_CREAT|O_EXCL`, no-follow), and the secret/pid files have
  their **ownership and `0600` mode validated before being read** — `/tmp` is world-shared
  on macOS, so a plain path invites symlink races and secret leakage.

## 6. Latency budget (target)

capture ~16 ms + encode ~5–15 ms + network (P2P/TURN) ~10–50 ms + decode ~16 ms ≈
**50–100 ms** on a good network, excluding the inherent Mac↔phone Mirroring-layer
latency, which is outside our control.

## 7. Out of scope (YAGNI for v1 of v2)

- Cloudflare Calls **SFU** fan-out (multi-viewer) — P2P + TURN is enough for 1:1.
- Audio.
- Multi-device / multi-phone selection UI — single Mirroring window for now.
- HEVC/AV1 — H.264 for Safari compatibility first.

## 8. Crate selection (default; confirm in Spike S0)

- **Capture/encode:** `screencapturekit` (purpose-built, has single-window examples) +
  `videotoolbox` (exposes realtime H.264 builder paths) over `cidre` (broader but less
  documented).
- **WebRTC:** `webrtc-rs` (PeerConnection-style API, includes an H.264 payloader) over
  `str0m` (Sans-I/O — only worth it if we deliberately want to own the reactor loop and
  TURN sockets).
- **MCP:** `rmcp` if it fits cleanly, else a thin hand-rolled stdio JSON-RPC loop.

## 9. Validation spikes (do FIRST, before the plan commits)

Ordered by risk. Each is a throwaway probe that retires a specific unknown.

- **S1 — input injection (BLOCKER).** Does `CGEventPostToPid(mouseDown/Dragged/Up)` on
  the Mirroring window's pid actually drive the mirrored iPhone? Test window
  **frontmost, backgrounded, covered, and across displays**. Outcome decides whether the
  binary is self-contained, must focus the window first, or keeps the `cua-driver`
  fallback (§3.1, §3.5).
- **S0 — capture + encode the REAL Mirroring window (BLOCKER).** Not a generic window:
  filter `SCStream` to the actual **iPhone Mirroring** window and verify **non-black,
  changing** frames (Mirroring may emit blank/DRM-protected output or composite
  oddly). Test **frontmost, backgrounded, covered, and across displays** with S1-level
  rigor. Then chain it through `videotoolbox` realtime H.264 (Constrained Baseline,
  `packetization-mode=1`, no B-frames, forced IDR + in-band SPS/PPS) → `webrtc-rs`
  H.264 track that a desktop browser decodes. Confirms both the physical capture
  assumption and the §8 stack before building on either.
- **S2 — Safari receive path.** The S0 stream rendered in **iOS Safari**
  `<video playsinline>` with the explicit `play()` gesture; verify decode, latency, and
  data-channel round-trip (separate reliable + unordered channels).
- **S3 — Cloudflare TURN.** A 1:1 P2P session relayed through Cloudflare Realtime TURN
  with server-issued ephemeral creds, credential refresh, and an ICE restart.

S1 and S0 are both blockers and independent — run them in parallel; neither the input
path nor the video path is proven against the real Mirroring window yet.
