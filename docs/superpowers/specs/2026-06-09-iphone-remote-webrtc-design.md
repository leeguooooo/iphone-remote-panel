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
| Host stack | **Rust daemon, run as a GUI-session LaunchAgent** | Capture/input need TCC grants tied to a signed identity in the login session — an SSH-spawned binary is denied (§5, validated) |
| NAT traversal | **P2P + Cloudflare TURN** | 1:1 control wants direct/relayed P2P, not an SFU extra hop |
| Input injection | **CGEvent via the session/HID event tap, window frontmost** (validated S1b); `cua-driver` fallback | `post_to_pid` does NOT drive Mirroring; the session/HID tap does — but it **commandeers the real Mac cursor** (§3.1/§3.4) |
| Agent entry | **MCP server** | Fits the Hermes + cua-driver MCP ecosystem; high-level phone tools |
| Arbitration | **Control lease + generation/cancellation token — MANDATORY** (human pre-empts agent) | The real Mac cursor is a single shared resource; without the lease, human and agent fight over one cursor. Not an optional optimization (§3.4) |
| Packaging | **Signed daemon + LaunchAgent + external prerequisites** (`cloudflared`; `cua-driver` for the input fallback) | Daemon runs in the login session with Screen Recording + Accessibility granted once; clients (SSH, Hermes, iPhone Safari) connect remotely (§5) |

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
| `core::window` | Find the iPhone Mirroring window; track resize/move/close | shareable-content list | `{pid, window_id, bounds, scale}`. **Selection (validated):** match name/bundle (`iPhone`/`镜像`/`Mirroring`/`ScreenContinuity`), then prefer the **phone-shape `size_ok` (200–900 × 400–1600) largest** window — SCK `is_on_screen` proved unreliable — and **skip setup/welcome windows** (`welcome`/`欢迎`) and the 1800×N menubar extras |
| `core::capture` | SCK `SCStream` filtered to that window; deliver frames | window handle | `CMSampleBuffer` stream. **Host context:** must call `NSApplicationLoad()` before any SCK call (else `CGS_REQUIRE_INIT` abort — validated). **Invariants/targets:** 30 fps default (cap 60, `SCStreamConfiguration.minimumFrameInterval`), bounded queue depth ~2–3 frames, drop late frames, **silently drop idle samples** (`get_pixel_buffer → CouldNotGetDataBuffer` is normal SCK behavior, not an error), **pass `CMSampleBuffer` PTS through** for pacing, restart `SCStream` on window-id/display change |
| `encode` | VideoToolbox H.264, realtime/low-latency | frame stream | NAL units → RTP. **Contract (S0b-1 validated):** Constrained Baseline, `packetization-mode=1`, **no B-frames**, force IDR on viewer join AND **on RTCP PLI** (wire the `rtp_sender` RTCP read → force-IDR; S0b-2 found artifacts when PLI→IDR was unreliable), **Annex-B with SPS/PPS in-band before every IDR**. **Static-screen keepalive (required):** SCK emits no frames when static, so on a ~500 ms idle timer **emit a fresh KEYFRAME (forced IDR), not a repeated P-frame** — repeating a non-IDR access unit drifts the decoder into color-block artifacts (S0b-2). Use **trickle ICE** for sub-second first frame |
| `core::input` | Map abstract events → injected events | `PointerDown/Move/Up`, `Key`, `Text` (normalized coords) | **`CGEvent::post(CGEventTapLocation::HID)` at global screen coords, Mirroring window frontmost** (validated S1b — `post_to_pid` does NOT work); `cua-driver` fallback. **Commandeers the real Mac cursor.** Tap-vs-long-press **timing fidelity** matters (the probe's ~100 ms press read as a long-press → jiggle mode) — map iPhone touch durations deliberately |
| `front::webrtc` | One `PeerConnection` per viewer (S0b-2 validated): H.264 `TrackLocalStaticSample` + two data channels | SDP/ICE via signaling | webrtc-rs 0.17. **Daemon is the offerer** and creates both channels; **fmtp `profile-level-id=42e01f`** (Safari-native, NOT 42001f); **relay-only fallback ICE** (Cloudflare TURN) when LAN/mDNS fails; ICE-restart daemon-initiated |
| `front::mcp` | MCP tools over the same core; screenshots pulled from the live pipeline | tool calls | **Must be a CONNECT-IN surface served by the running daemon (HTTP/SSE/socket), NOT a per-call stdio-spawn MCP** — a spawned child loses the TCC grant (§5 responsible-process chain). A stdio shim, if any, only forwards to the daemon |
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

### 3.4 Arbitration (control lease + cancellation token) — MANDATORY

**This is mandatory, not an optimization.** Validation (S1b) confirmed input flows through
the **global session/HID event tap and commandeers the one real Mac cursor** — a single
shared physical resource. Without the lease, a human touch and an agent action drive the
same cursor simultaneously and corrupt each other's gestures. The lease is what makes
human↔agent coexistence safe at all.

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

**`cua-driver` dependency (resolved by S1b):** in-process CGEvent input is proven — but
via `CGEvent::post(CGEventTapLocation::HID)` at **global screen coords with the Mirroring
window frontmost**, NOT `post_to_pid` (which Mirroring ignores). The daemon does its own
SCK capture + input in the granted login session, so it is self-contained for the core
paths. `cua-driver` is kept as a **runtime fallback** for input (insurance / discrete
actions) and as the proven reference; the installer lists it as an optional prerequisite.
Because input drives the **real cursor with the window frontmost**, `core::input` brings
the Mirroring window to front before a gesture and the control lease (§3.4) is mandatory.

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

## 5. Deployment — GUI-session LaunchAgent (validated constraint)

**The daemon must run in the login (Aqua) session as a launchd-direct child, not as an
SSH-spawned process.** Two validated facts force this (details: `docs/superpowers/notes/2026-06-09-macos-deployment-tcc-launchagent.md`):

1. An SSH-launched binary is **TCC-denied** for Screen Recording (`SCShareableContent` →
   error -3801) and would be denied Accessibility too.
2. **Responsible-process chain (macOS 15):** `AXIsProcessTrusted`/`CGEventPost` evaluate
   the *caller chain*, not just the daemon's own entry. A binary `spawn`ed by another
   process (e.g. an agent) is **untrusted even with a valid grant**; only a
   **launchd-direct LaunchAgent** is evaluated against its own entry.

→ The daemon is a **codesigned `.app` LaunchAgent** (stable identity so grants persist
across upgrades). **No client may spawn its own copy** — SSH shells, Hermes/agents, and
the iPhone Safari controller all **connect to the running daemon** (this is why `front::mcp`
is connect-in, not stdio-spawn). Mirrors `cua-driver`/screenpipe. Headless boxes need
**auto-login** (a LaunchAgent only runs once a GUI user logs in); macOS 15 re-prompts
Screen Recording ~monthly (interactive, unsuppressible without MDM).

- Ship the daemon `iphone-remote` via **GitHub Releases** + `install.sh` (`curl … | sh`),
  per the project distribution convention — no npm registry. It must be **codesigned with
  a stable identity** so TCC grants persist across upgrades.
- `install.sh` installs a **LaunchAgent** (`~/Library/LaunchAgents/…plist`,
  `launchctl bootstrap gui/$UID`) so the daemon starts in the login session on login, and
  walks the user through granting **Screen Recording + Accessibility once**.
- Subcommands: `serve` (foreground, for dev/the LaunchAgent target) runs the §4 TCC
  preflight, bootstraps the AppKit app context (`NSApplicationLoad`), starts capture +
  input + WebRTC/MCP/HTTP, brings up `cloudflared` for signaling, prints local/tunnel URL
  + password; `stop`/`status` manage the LaunchAgent. **External prerequisites:**
  `cloudflared` (signaling tunnel) and optional `cua-driver` (input fallback) — `serve`
  health-checks them.
- **Operating implication:** while a controller holds the lease, the daemon brings the
  Mirroring window frontmost and **the host Mac's real cursor is driven** — the relay Mac
  is "busy" during control. Acceptable for a dedicated relay; documented for the user.
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

## 9. Validation spikes

Status as of 2026-06-09 (see `SPIKE-RESULTS.md` for the hardware log).

- **S1 — input injection (BLOCKER) — ✅ PASS (as S1b).** `CGEventPostToPid` does NOT
  drive Mirroring; `CGEvent::post(HID/Session)` at global coords with the window
  **frontmost** DOES (entered jiggle/edit mode). Cursor is commandeered, as expected.
  Confirmed frontmost/focused; backgrounded/covered/across-display **not yet tested**
  (likely require frontmost — fold into Phase 1). Resolution → §2, §3.1, §3.4, §3.5.
- **S0 — capture the REAL Mirroring window (BLOCKER) — ✅ PASS.** `CGS_REQUIRE_INIT`
  abort was a missing app context, fixed by `NSApplicationLoad()`; 22 non-black PNGs of
  the live iPhone home screen, changing with interaction. Idle `CouldNotGetDataBuffer`
  samples are normal and dropped. Window-picker fixed (size-shape over `is_on_screen`,
  skip welcome/menubar). **Still pending:** the encode→WebRTC half (`videotoolbox`
  H.264 → `webrtc-rs` → browser decode) — call this **S0b**, the next spike.
- **TCC/deployment finding (validated).** An SSH-spawned binary is denied Screen
  Recording (error -3801); the daemon must run in the GUI login session as a granted,
  signed LaunchAgent (§5). This is now a locked architectural constraint, not a spike.
- **S0b-1 — capture → VideoToolbox H.264 → file — ✅ PASS.** `s0b.h264` decodes
  (ffprobe: Constrained Baseline, 312×694, no B-frames) and visually shows the live Home
  Screen. **Finding:** SCK delivers no frames on a static screen → encoder timed out at 33
  frames; the WebRTC side needs **repeat-last-frame keepalive** (folded into `encode`, §3.1).
- **S0b-2 — capture → VideoToolbox → `webrtc-rs` → desktop browser decode — ✅ PASS.**
  Browser showed the live Home Screen (readyState 4, 312×694, track live); static keepalive
  advanced `currentTime`. **Two quality findings for the production component (not blockers):**
  (a) **first-frame ~3 s** on connect — non-trickle ICE gathering + waiting for an IDR; prod
  must use **trickle ICE** + reliable **force-IDR-on-connect** for sub-second first frame.
  (b) **color-block / codec artifacts** — loss corruption that doesn't recover (PLI→IDR
  unreliable) and/or keepalive repeating a P-frame. **Prod keepalive must emit a fresh
  KEYFRAME on a static screen (not repeat an arbitrary access unit), and wire PLI→force-IDR.**
- **S2 — Safari receive path.** The S0b stream rendered in **iOS Safari**
  `<video playsinline>` with the explicit `play()` gesture; verify decode, latency, and
  data-channel round-trip (separate reliable + unordered channels).
- **S3 — Cloudflare TURN.** A 1:1 P2P session relayed through Cloudflare Realtime TURN
  with server-issued ephemeral creds, credential refresh, and an ICE restart.

S1 and S0 were both blockers and independent — **both now PASS** against the real
Mirroring window. The remaining spikes (S0b, S2, S3) are about the WebRTC/Safari/TURN
half of the pipeline, not the macOS capture/input assumptions, which are settled.
