# iPhone Remote Panel — WebRTC Rebuild Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the v1 screenshot-polling remote with a single Rust binary that streams the macOS iPhone Mirroring window over WebRTC (hardware H.264) to iPhone Safari and injects continuous touch, with a shared core that also serves agents over MCP.

**Architecture:** A device-agnostic `core` (window-find · capture · input) sits under three front-ends (human WebRTC+web, agent MCP, HTTP/signaling). ScreenCaptureKit → VideoToolbox H.264 → webrtc-rs video track; pointer/key over two data channels → CGEvent injection. P2P + Cloudflare TURN for NAT. See spec: `docs/superpowers/specs/2026-06-09-iphone-remote-webrtc-design.md`.

**Tech Stack:** Rust; `screencapturekit` + `videotoolbox` (Apple capture/encode), `webrtc-rs` (PeerConnection + H.264 payloader), `axum` (HTTP/WS signaling), `tokio`, `rmcp` (or hand-rolled stdio JSON-RPC) for MCP, `cloudflared` (external, signaling tunnel), Cloudflare Realtime TURN.

---

## Scope & phasing

The two **blocker spikes gate the design**: until they pass we do not know whether the
binary is self-contained (S1) or whether the real Mirroring window is even capturable
(S0). So:

- **Phase 0 — Validation spikes (gate).** Throwaway probes. No TDD; the deliverable is a
  recorded yes/no + measurements that lock later decisions. **Nothing in Phase 1+ starts
  until the Phase 0 gate is recorded.**
- **Phase 1 — Foundation + first working human vertical slice.** Real code, TDD on pure
  logic. Deliverable: from iPhone Safari you see live video and a tap/swipe drives the
  phone. This is shippable software on its own.
- **Phase 2 — Arbitration depth + agent MCP front-end.** Expanded at the gate.
- **Phase 3 — Hardening + packaging** (TCC preflight, runtime-dir safety, TURN refresh,
  `serve`/`stop`, `install.sh`, Release CI).

Phases 2–3 are specified at task granularity here; their bite-sized steps are finalized
right after the Phase 0 gate, because spike outcomes (e.g. "CGEvent only works
frontmost") change them. This is intentional, not under-specification.

TDD note: apply TDD to **pure logic** (token sign/verify, coordinate math, arbitration
generation/cancellation, stale-move dropping, NAL/SPS-PPS framing helpers, protocol
parsing). OS/WebRTC/codec glue is validated by **integration + manual** checks — you
cannot unit-test "does CGEvent drive iPhone Mirroring".

---

## File structure

```
Cargo.toml                      # workspace
crates/
  core/
    src/window.rs               # find iPhone Mirroring window (pid, bounds, scale, orientation)
    src/capture.rs              # SCStream → frame channel (FPS/queue/PTS invariants)
    src/encode.rs               # VideoToolbox H.264 (Constrained Baseline, IDR-on-join, in-band SPS/PPS)
    src/input.rs                # abstract events → CGEvent (primary) / cua-driver (fallback)
    src/coords.rs               # normalized<->content-rect<->screen mapping (pure)
    src/control.rs              # arbitration: lease + generation + preempt cleanup (pure-ish)
    src/lib.rs
  server/
    src/auth.rs                 # password + HMAC token (pure)
    src/http.rs                 # axum: web UI, /login, /logout, TURN creds
    src/signaling.rs            # WebSocket SDP/ICE exchange
    src/webrtc.rs               # PeerConnection per viewer, H.264 track, two data channels
    src/protocol.rs             # input/control message types + parse (pure)
    src/runtime_dir.rs          # 0700 per-user dir, atomic files, perm validation
    src/mcp.rs                  # MCP front-end tools (Phase 2)
    src/main.rs                 # serve / stop subcommands, TCC preflight
  spikes/                       # Phase 0 throwaway probes (kept for reference, not shipped)
web/
  index.html                    # login + player + input capture (single self-contained page)
install.sh
.github/workflows/release-binaries.yml
docs/superpowers/specs/2026-06-09-iphone-remote-webrtc-design.md
docs/superpowers/plans/2026-06-09-iphone-remote-webrtc.md   # this file
SPIKE-RESULTS.md                # Phase 0 gate record (created in Phase 0)
```

---

# Phase 0 — Validation spikes (GATE)

Probes live under `crates/spikes/` as separate `[[bin]]` targets. Record every outcome
in `SPIKE-RESULTS.md`. **S0 and S1 are independent blockers — do them first, in either
order or parallel.**

## Spike S1 — Input injection into iPhone Mirroring (BLOCKER)

**Question:** does `CGEventPostToPid(mouseDown/Dragged/Up)` actually drive the mirrored
iPhone, and under what window state?

**Files:**
- Create: `crates/spikes/src/bin/s1_input.rs`

- [ ] **Step 1: Scaffold the probe**

Create a minimal `[[bin]]` that takes a target pid + a screen point and posts a
mouse-down→drag→up using `core-graphics` (`CGEvent::new_mouse_event`,
`CGEvent::post_to_pid`). Print each event it sends.

```rust
// crates/spikes/src/bin/s1_input.rs
// usage: s1_input <pid> <x> <y>
// posts mouseDown@(x,y) -> 5 dragged steps -> mouseUp, via CGEventPostToPid
```

- [ ] **Step 2: Find the Mirroring window pid**

Run: `/Users/leo/.local/bin/cua-driver call list_windows '{}'` and note the iPhone
Mirroring window `pid` and `bounds`.

- [ ] **Step 3: Probe — window FRONTMOST**

Bring iPhone Mirroring to the front. Run `s1_input <pid> <x> <y>` aimed at a known tap
target (e.g. an app icon). Observe the real iPhone.
Record: did the tap land? did the drag scroll?

- [ ] **Step 4: Probe — window BACKGROUNDED, COVERED, ACROSS DISPLAYS**

Repeat Step 3 with the Mirroring window (a) behind another window, (b) fully covered,
(c) on a secondary display. Record each result.

- [ ] **Step 5: Record the verdict in SPIKE-RESULTS.md**

```markdown
## S1 — input injection
- frontmost: PASS/FAIL (notes)
- backgrounded: PASS/FAIL
- covered: PASS/FAIL
- across-display: PASS/FAIL
- DECISION: [self-contained CGEvent | must-focus-window-first | keep cua-driver fallback]
- coordinate space confirmed: [screen points | backing px | window-local]
```

- [ ] **Step 6: Commit**

```bash
git add crates/spikes/src/bin/s1_input.rs SPIKE-RESULTS.md Cargo.toml
git commit -m "spike: S1 CGEvent injection into iPhone Mirroring"
```

## Spike S0 — Capture the real Mirroring window → H.264 → browser (BLOCKER)

**Question:** can ScreenCaptureKit capture the *actual* iPhone Mirroring window (non-black,
changing), and does the VideoToolbox→webrtc-rs H.264 chain decode in a browser?

**Files:**
- Create: `crates/spikes/src/bin/s0_capture.rs`

- [ ] **Step 1: Capture-only probe**

Filter `SCStream` to the iPhone Mirroring window (match by app/title from
`SCShareableContent`). Dump 30 frames to PNG.

- [ ] **Step 2: Verify non-black, changing frames**

Interact with the phone while capturing. Inspect the PNGs: are they non-black and do
they change? Record. **If black/DRM-protected → STOP, surface to human; this is the
hard blocker.**

- [ ] **Step 3: Window-state matrix**

Repeat Step 1 with the window frontmost / backgrounded / covered / across-display.
Record which states still yield live frames.

- [ ] **Step 4: Chain to H.264 + browser decode**

Extend the probe: frame → `videotoolbox` realtime H.264 (Constrained Baseline,
`packetization-mode=1`, no B-frames, forced IDR at start, in-band SPS/PPS) → `webrtc-rs`
H.264 track → a throwaway local HTML page (`<video>`) in **desktop Chrome/Safari**.
Confirm it decodes and renders.

- [ ] **Step 5: Measure**

Record glass-to-glass latency (rough), fps, and CPU. Note encoder settings that worked.

- [ ] **Step 6: Record verdict + commit**

```markdown
## S0 — real Mirroring-window capture + encode
- non-black/changing: PASS/FAIL
- state matrix: frontmost/backgrounded/covered/across-display = ...
- browser decode: PASS/FAIL
- latency ~Xms, fps Y, cpu Z%
- DECISION: crates locked = screencapturekit + videotoolbox + webrtc-rs (or note swap)
```
```bash
git add crates/spikes/src/bin/s0_capture.rs SPIKE-RESULTS.md
git commit -m "spike: S0 capture real Mirroring window + H.264 browser decode"
```

## Spike S2 — Safari (iOS) receive path

- [ ] **Step 1:** Serve the S0 page over the LAN; open in **iOS Safari**,
  `<video autoplay playsinline muted>` + an explicit play button calling `video.play()`.
- [ ] **Step 2:** Confirm decode + render; measure latency. Test the play-gesture
  fallback (does autoplay alone work, or is the button required?).
- [ ] **Step 3:** Open a reliable + an unordered data channel; round-trip a ping on each;
  confirm both deliver and measure RTT. Record + commit.

## Spike S3 — Cloudflare TURN

- [ ] **Step 1:** Issue ephemeral TURN creds from the Cloudflare Realtime API server-side.
- [ ] **Step 2:** Force a relayed 1:1 path (set `iceTransportPolicy: 'relay'`); confirm
  media flows host↔Safari over TURN.
- [ ] **Step 3:** Exercise credential refresh via `setConfiguration()` and an ICE restart;
  confirm the session survives. Record + commit.

## Phase 0 GATE

- [ ] **Write the gate decision** at the top of `SPIKE-RESULTS.md`: input path
  (self-contained / focus-first / fallback), capture viability + working states, locked
  crates, and any design deltas to fold back into the spec. Then expand Phase 2 and
  Phase 3 to bite-sized steps using the gate outcomes. **Stop-and-re-brainstorm
  conditions:** S0 or S1 hard-fail (no capture / no input — fatal to everything); **S2
  fail** = iOS Safari can't decode the stream (fatal to the human path — investigate
  H.264 profile/level before continuing); **S3 fail** = no relayed path (degrade to
  LAN-only and defer public access, not a full stop).

```bash
git add SPIKE-RESULTS.md && git commit -m "docs: Phase 0 spike gate decision"
```

---

# Phase 1 — Foundation + first human vertical slice

> Reuse the spike code as reference; productionize, don't copy-paste throwaway probes.

## Task 1: Cargo workspace scaffold

**Files:**
- Create: `Cargo.toml`, `crates/core/Cargo.toml`, `crates/core/src/lib.rs`,
  `crates/server/Cargo.toml`, `crates/server/src/main.rs`

- [ ] **Step 1:** Create the workspace `Cargo.toml` with members `core`, `server`,
  `spikes`. Add deps per the locked crate list.
- [ ] **Step 2:** `crates/server/src/main.rs` with a `clap` CLI exposing `serve` and
  `stop` (stubs that print and exit 0 for now).
- [ ] **Step 3:** Run `cargo build` — expect success.
- [ ] **Step 4:** Commit `chore: scaffold rust workspace (core + server)`.

## Task 2: `core::auth` — password + HMAC session token (TDD)

**Files:**
- Create: `crates/core/src/auth.rs`, tests inline `#[cfg(test)]`

- [ ] **Step 1: Failing tests** — port v1's token scheme (`exp:nonce:sig`,
  HMAC-SHA256, constant-time compare via `subtle`/`ring`):

```rust
#[test] fn make_then_check_roundtrips() { /* make_token() -> check_token() == true */ }
#[test] fn expired_token_rejected() { /* exp in the past -> false */ }
#[test] fn tampered_sig_rejected() { /* flip a byte -> false */ }
#[test] fn wrong_secret_rejected() { /* different SECRET -> false */ }
```

- [ ] **Step 2:** Run `cargo test -p core auth::` → FAIL (not implemented).
- [ ] **Step 3:** Implement `make_token`, `check_token` (constant-time), `verify_password`.
- [ ] **Step 4:** Run tests → PASS.
- [ ] **Step 5:** Commit `feat(core): HMAC session token + password verify`.

## Task 3: `core::coords` — coordinate mapping (TDD)

**Files:**
- Create: `crates/core/src/coords.rs`

- [ ] **Step 1: Failing tests** for normalized↔content-rect↔screen, including letterbox
  subtraction and orientation:

```rust
#[test] fn normalized_center_maps_to_content_center() {}
#[test] fn letterbox_bar_maps_to_none() {}        // tap on black bar -> None
#[test] fn rotated_window_maps_correctly() {}      // orientation applied
#[test] fn survives_resize() {}                    // same normalized -> scaled screen pt
```

- [ ] **Step 2:** `cargo test -p core coords::` → FAIL.
- [ ] **Step 3:** Implement `SessionGeometry { frame_size, content_rect, scale, orientation }`
  and `fn to_screen(norm: (f64,f64), geo: &SessionGeometry) -> Option<ScreenPoint>`.
- [ ] **Step 4:** Tests → PASS.
- [ ] **Step 5:** Commit `feat(core): coordinate mapping with letterbox + orientation`.

## Task 4: `core::window` — find the Mirroring window

**Files:**
- Create: `crates/core/src/window.rs`

- [ ] **Step 1: Failing test** for the selection heuristic against a fixture window list
  (port v1's `find_phone_window`: app/title contains `iPhone`/`镜像`/`Mirroring`, bounds
  200–900 × 400–1600, prefer on-screen + largest):

```rust
#[test] fn picks_largest_onscreen_mirroring_window() { /* feed Vec<WindowInfo> */ }
#[test] fn errors_when_no_mirroring_window() {}
```

- [ ] **Step 2:** `cargo test -p core window::` → FAIL.
- [ ] **Step 3:** Implement the pure selector over a `WindowInfo` struct; add a thin
  platform fn that fills `WindowInfo` from `SCShareableContent` (or cua-driver fallback
  per S1 gate). Keep the selector pure/testable; the OS query is a separate, untested fn.
- [ ] **Step 4:** Tests → PASS; manually confirm the OS query finds the real window.
- [ ] **Step 5:** Commit `feat(core): iPhone Mirroring window finder`.

## Task 5: `server::protocol` — input/control messages (TDD)

**Files:**
- Create: `crates/server/src/protocol.rs`

- [ ] **Step 1: Failing tests** for serde round-trip + stale-move dropping:

```rust
// messages: Down{n}, Move{n,seq}, Up{n}, Key{name}, Text{s}, Shortcut{name},
//           Acquire, Release  (n = normalized coords)
#[test] fn parse_each_message_variant() {}
#[test] fn move_dropper_discards_out_of_order_seq() {} // keep max seq seen, drop older
```

- [ ] **Step 2:** `cargo test -p server protocol::` → FAIL.
- [ ] **Step 3:** Implement the enum, serde derives, and a `MoveDropper` keeping the last
  seq and rejecting stale moves.
- [ ] **Step 4:** Tests → PASS.
- [ ] **Step 5:** Commit `feat(server): input/control wire protocol + stale-move dropper`.

## Task 6: `server::http` + `auth` wiring — login & static page

**Files:**
- Create: `crates/server/src/http.rs`; Modify: `crates/server/src/main.rs`
- Create: `web/index.html` (login form only for now)

- [ ] **Step 1:** axum app: `GET /` → redirect `/phone`; `GET /login` page; `POST /login`
  (verify password → set signed-cookie 302 to `/phone`); `GET /logout`; `GET /phone`
  (auth-gated, serves `web/index.html`). Reuse v1 security headers + add cookie `Secure`.
- [ ] **Step 2:** Integration test with `reqwest`/`axum::test`: wrong password → 401;
  right password → cookie set; `/phone` without cookie → 302 `/login`.
- [ ] **Step 3:** `cargo test -p server http::` → PASS.
- [ ] **Step 4:** Manually load `http://127.0.0.1:8787/login`, log in.
- [ ] **Step 5:** Commit `feat(server): axum http + login gate`.

## Task 7: `server::webrtc` + `signaling` — video track to the browser

**Files:**
- Create: `crates/server/src/webrtc.rs`, `crates/server/src/signaling.rs`
- Create: `crates/core/src/capture.rs`, `crates/core/src/encode.rs`
  (productionized from S0)

- [ ] **Step 1:** `core::capture` — `SCStream` → `tokio::sync::mpsc` frame channel with
  the §3.1 invariants (30fps default, queue depth 2–3, drop late, PTS pass-through,
  restart on window/display change).
- [ ] **Step 2:** `core::encode` — VideoToolbox H.264 with the locked encoder settings;
  expose `force_keyframe()` and emit in-band SPS/PPS before every IDR.
- [ ] **Step 3:** `server::webrtc` — one `RTCPeerConnection` per viewer; add an H.264
  track fed from the encoder; call `force_keyframe()` on viewer join.
- [ ] **Step 4:** `server::signaling` — WS endpoint (auth-gated) exchanging SDP
  offer/answer + trickle ICE; ICE servers from the TURN-cred endpoint (stub creds OK on
  LAN for now).
- [ ] **Step 5:** Update `web/index.html`: establish `RTCPeerConnection`, attach the
  remote track to `<video autoplay playsinline muted>` + explicit play button.
- [ ] **Step 6:** Manual: open `/phone` on a desktop browser over LAN → see live phone
  video. Record latency.
- [ ] **Step 7:** Commit `feat: live H.264 WebRTC video from Mirroring window`.

## Task 8: `core::input` — CGEvent injection (from S1 gate)

**Files:**
- Create: `crates/core/src/input.rs`

- [ ] **Step 1:** Implement `inject(event: InputEvent, geo: &SessionGeometry)` mapping
  normalized coords via `core::coords` → screen point → CGEvent (or the focus-first /
  cua-driver path the S1 gate selected). Phases: down→`mouseDown`, move→`mouseDragged`,
  up→`mouseUp`; `Key`/`Text`/`Shortcut` per gate.
- [ ] **Step 2:** Unit-test the *mapping/decision* logic (which path, which CGEvent type)
  with the OS post mocked; the real post is manual.
- [ ] **Step 3:** `cargo test -p core input::` → PASS.
- [ ] **Step 4:** Manual: confirm a programmatic down→drag→up moves the phone.
- [ ] **Step 5:** Commit `feat(core): CGEvent input injection`.

## Task 9: Wire input end-to-end (two data channels)

**Files:**
- Modify: `crates/server/src/webrtc.rs`, `web/index.html`

- [ ] **Step 1:** Open **two** `RTCDataChannel`s: reliable-ordered (`down/up/key/text/
  shortcut/acquire/release`) and unordered-lossy (`maxRetransmits:0, ordered:false`) for
  `move` (with seq). Client sends pointer events relative to the **video content rect**.
- [ ] **Step 2:** Server routes parsed messages → `MoveDropper` → `core::input::inject`
  using the live `SessionGeometry` (published to the client on connect + on change).
- [ ] **Step 3:** Manual end-to-end: from a browser, tap an app icon and swipe a list on
  the **real iPhone**. Verify tap accuracy and swipe follows the finger.
- [ ] **Step 4:** Commit `feat: end-to-end touch over data channels`.

## Task 10: Phase 1 acceptance (iPhone Safari)

- [ ] **Step 1:** From **iPhone Safari** on the LAN: log in, see live video (play
  gesture), tap and swipe the controlled iPhone. Record latency + accuracy in
  `SPIKE-RESULTS.md` under "Phase 1 acceptance".
- [ ] **Step 2:** Use @superpowers:verification-before-completion to confirm the slice
  works as specified. Commit the acceptance note.

---

# Phase 2 — Arbitration + agent MCP front-end

> Expand to bite-sized steps right after the Phase 0 gate. Tasks:

- [ ] **Task 11 — `core::control` (TDD):** `control { holder: Human(session_id) |
  Agent(id), generation, last_active, pressed_state }`; short lock never held across
  `.await`/FFI/loop; `acquire`/`preempt`/`release`; idle auto-release (30s). Test:
  generation bump on preempt, stale token aborts a gesture loop, **preempt synthesizes
  `mouseUp`/key-up cleanup** (no stuck pointer), one-human-controller rule.
- [ ] **Task 12 — enforce control in the input path:** every injected event + gesture
  iteration checks `generation`; human pointer preempts agent; displaced party still
  observes. Manual: agent mid-swipe, human taps → agent stops cleanly, no stuck press.
- [ ] **Task 13 — `server::mcp` front-end:** MCP server (rmcp or hand-rolled stdio
  JSON-RPC) exposing `observe`, `screenshot` (JPEG from the live pipeline),
  `tap/swipe/type/key/shortcut`, `acquire_control/release_control`. `swipe` synthesizes
  intermediate `mouseDragged` points through `core::input` (same path as human). Token
  gates MCP. Test the tool schemas + arg validation; manual: Hermes drives the phone.
- [ ] **Task 14 — agent/human coexistence acceptance:** human observes an agent task;
  human takes over; agent re-acquires. Record.

---

# Phase 3 — Hardening + packaging

> Expand to bite-sized steps at the Phase 0 gate (alongside Phase 2). Tasks:

- [ ] **Task 15 — TCC preflight (fail-closed):** `serve` checks
  `CGPreflightScreenCaptureAccess`/`CGRequestScreenCaptureAccess` +
  `AXIsProcessTrustedWithOptions`; prints exact System Settings panes; refuses to start
  until granted. Fix a codesign identity so grants persist across upgrades.
- [ ] **Task 16 — `server::runtime_dir`:** per-user `0700` dir (`$TMPDIR/...-$UID`),
  atomic `O_CREAT|O_EXCL` no-follow file creation, validate owner+`0600` before reading
  secret/pid. Test perms + refusal on bad mode/owner.
- [ ] **Task 17 — TURN credential lifecycle:** **adds the `GET` TURN-creds route to
  `http.rs`** (stubbed/LAN in Task 7) that issues ephemeral Cloudflare creds with TTL >
  session; client refreshes via `setConfiguration()` before expiry; ICE restart on path
  loss. Manual: rotate mid-session, force a network change.
- [ ] **Task 18 — `serve`/`stop` + tunnel:** `serve` runs preflight, starts host, brings
  up `cloudflared` (external prereq; health-check it), prints local/tunnel URL +
  password; `stop` kills recorded pids. Replace the v1 bash scripts.
- [ ] **Task 19 — distribution:** `install.sh` (`curl … | sh`), per-platform binary build
  + attach to **GitHub Release** in `.github/workflows/release-binaries.yml` (no npm
  registry, per project convention). README rewrite for v2 (HTML per global doc rule if
  user-facing; repo README stays `.md`).
- [ ] **Task 20 — retire v1:** delete `phone_remote_server.py` + `scripts/*` once v2
  reaches parity; update README "current implementation" section.

---

## Execution notes

- **Commit after every step** (TDD: test → fail → impl → pass → commit).
- Pure-logic crates (`auth`, `coords`, `protocol`, `control`) carry the test weight;
  OS/codec/WebRTC glue is integration + manual.
- Reference @superpowers:test-driven-development for the pure-logic tasks and
  @superpowers:verification-before-completion at each phase acceptance.
- Do not start Phase 1 before the **Phase 0 gate** is recorded in `SPIKE-RESULTS.md`.
