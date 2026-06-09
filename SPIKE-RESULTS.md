# Phase 0 Spike Results

Gate date: 2026-06-09 JST
Machine: Leo's macOS host, real iPhone connected via iPhone Mirroring (`iPhone镜像`, bundle `com.apple.ScreenContinuity`).
Branch tested: `feat/webrtc-rebuild` at `63ba89e`.

## GATE DECISION

**FAIL / STOP before Phase 1.**

Neither blocker spike passed on this real hardware run:

- **S0a ScreenCaptureKit capture probe:** FAIL for current probe/runtime. It finds the real iPhone Mirroring window, then aborts before writing frames with `CGS_REQUIRE_INIT`.
- **S1 CGEventPostToPid input probe:** FAIL. Posted events completed successfully, but the mirrored iPhone did not react. The same target coordinate was verified to work through the existing `iphone-act`/`cua-driver` path.

This result means the WebRTC rebuild should not proceed to Phase 1 as a self-contained SCK + CGEvent design until the blocker assumptions are reworked or fixed.

## Environment / prerequisites

- Rust was not initially installed (`cargo: command not found`); installed via Homebrew `brew install rust`.
- `cargo build --workspace` then passed.
- iPhone Mirroring window discovered by both `cua-driver` and the S0 probe:
  - app/title: `iPhone镜像`
  - bundle: `com.apple.ScreenContinuity`
  - pid: `40374`
  - window id: `353`
  - bounds: `312x694 @ (23,377)`, `onScreen=true`
- Control screenshot before S1: `/tmp/phase0-home.png`.

## S0a — ScreenCaptureKit capture of real Mirroring window

Command:

```bash
cd /Users/leo/clawd/iphone-remote-panel
cargo build --workspace
rm -rf s0-frames
/Users/leo/bin/iphone-shot /tmp/phase0-before-s0.png >/dev/null
cargo run -p spikes --bin s0_capture
```

Observed output:

```text
s0_capture — ScreenCaptureKit iPhone Mirroring capture probe
chosen window: id=353 owner="iPhone镜像" bundle="com.apple.ScreenContinuity" title="iPhone镜像"
  bounds: 312x694 @ (23,377) onScreen=true
Assertion failed: (did_initialize), function CGS_REQUIRE_INIT, file CGInitialization.c, line 44.
Abort trap: 6
```

Re-run with `RUST_BACKTRACE=1 target/debug/s0_capture` produced the same abort before any Rust backtrace or frame output.

Result:

- non-black/changing frames: **FAIL** — no frames written; process aborts before capture output.
- state matrix: **not reached**.
- browser decode: **not reached**.
- latency/fps/cpu: **not measured**.
- notes: The probe does find the correct real Mirroring window. The failure is currently at ScreenCaptureKit / WindowServer initialization (`CGS_REQUIRE_INIT`) before the hardware capture question can be answered from PNG evidence. Treat as gate fail for this branch, not as proof that SCK can never capture Mirroring.

## S1 — CGEventPostToPid input injection

Setup:

```bash
/Users/leo/bin/iphone-act home
/Users/leo/bin/iphone-shot /tmp/phase0-home.png
/Users/leo/.local/bin/cua-driver call list_windows '{}'
```

Target chosen:

- safe target: Shortcuts (`快捷指令`) icon on the iPhone home screen.
- iPhone screenshot pixel coordinate verified by control path: approximately `(105,840)`.
- logical/global coordinate derived from window bounds and Retina scale: approximately `(76,790)`.

Frontmost-style probe #1 (logical/global coordinate):

```bash
osascript -e 'tell application "iPhone Mirroring" to activate' || true
cargo run -p spikes --bin s1_input -- 40374 76 790
/Users/leo/bin/iphone-shot /tmp/phase0-s1-frontmost-after.png
```

Observed output showed all CGEvents posted:

```text
posting mouseDown at (76, 790) to pid 40374
posting mouseDragged at (76, 794) to pid 40374
posting mouseDragged at (76, 798) to pid 40374
posting mouseDragged at (76, 802) to pid 40374
posting mouseDragged at (76, 806) to pid 40374
posting mouseDragged at (76, 810) to pid 40374
posting mouseUp at (76, 810) to pid 40374
done — gesture complete
```

Visual result: **no change**; still on the same home screen.

Frontmost-style probe #2 (raw screenshot-like coordinate):

```bash
/Users/leo/bin/iphone-act home
cargo run -p spikes --bin s1_input -- 40374 105 840
/Users/leo/bin/iphone-shot /tmp/phase0-s1-frontmost-rawpixel-after.png
```

Visual result: **no change**; still on the same home screen.

Control check with existing input path:

```bash
/Users/leo/bin/iphone-act tap 105,840
/Users/leo/bin/iphone-shot /tmp/phase0-coordinate-control-after.png
```

Visual result: **PASS for control path** — Shortcuts opened (`所有快捷指令` screen). This confirms the target was safe and the coordinate was meaningful; the S1 failure is specific to the raw `CGEventPostToPid` probe/path, not to the phone state or target selection.

Result:

- frontmost: **FAIL** — posted events completed, but did not drive iPhone Mirroring.
- backgrounded: **not run** — frontmost already failed the blocker.
- covered: **not run** — frontmost already failed the blocker.
- across-display: **not run**.
- coordinate space confirmed: **not confirmed for CGEventPostToPid**. Tested both logical/global and raw screenshot-like coordinates with no effect.
- DECISION: **keep `cua-driver` fallback / re-brainstorm input architecture**. Do not assume self-contained CGEvent input.

## Artifacts

- `/tmp/phase0-before-s0.png` — phone screenshot before S0 run.
- `/tmp/phase0-home.png` — home screen used for S1 target selection.
- `/tmp/phase0-s1-frontmost-after.png` — S1 logical/global coordinate result; unchanged.
- `/tmp/phase0-s1-frontmost-rawpixel-after.png` — S1 raw-coordinate result; unchanged.
- `/tmp/phase0-coordinate-control-after.png` — existing `iphone-act` control path opened Shortcuts, proving target coordinate.

## Recommended next action

Stop Phase 1. Re-brainstorm around:

1. Why `s0_capture` aborts with `CGS_REQUIRE_INIT` after successful window enumeration. This may be a probe/runtime issue to fix before judging SCK itself.
2. Whether input should remain delegated to `cua-driver`/AX, or whether a different in-process event strategy is needed. Raw `CGEventPostToPid` is not validated on this hardware run.

---

# Root-cause investigation + re-probes (2026-06-09, post-gate)

Both failures were investigated with the systematic-debugging skill. **Both have proven
working references in this same environment** (screenpipe captures via SCK-in-Rust;
`iphone-act`/`cua-driver` inject input), which strongly indicates integration bugs, not
walls. Each got a minimal **single-variable** re-probe.

## S0 crash — root cause: missing CG/WindowServer app context

`SCShareableContent` enumeration succeeded; the abort is at `SCStream.startCapture`.
`CGS_REQUIRE_INIT` / `did_initialize` is CoreGraphics asserting that the process never
initialised its WindowServer/AppKit app connection — a bare CLI binary is a non-GUI
process. The fix is to call **`NSApplicationLoad()`** (bootstraps the AppKit/CG app
context) before any ScreenCaptureKit call — the same context screenpipe's SCK capture
runs inside. This is NOT evidence that Mirroring is DRM-black; we never reached a frame.

- **Re-probe:** `s0_capture` now calls `objc2_app_kit::NSApplicationLoad()` at the top of
  `main()` (one added line; frame delivery already runs on an SCK delegate thread, so no
  run loop is needed yet). If the assertion clears but frames never arrive, the next
  iteration adds a main-thread `CFRunLoop`.

## S1 input no-op — root cause: wrong event-tap target

Events posted via `CGEvent::post_to_pid` are injected into the process's private queue;
iPhone Mirroring (`com.apple.ScreenContinuity`) forwards only input it observes on the
**system session/HID event tap** at the global cursor location — the path
`iphone-act`/`cua-driver` use (cf. cua-driver's `bring_to_front` + `dispatch:"foreground"`).
`iphone-act tap 105,840` working is the proof. `core-graphics 0.25` exposes
`CGEvent::post(CGEventTapLocation::{HID,Session})` for exactly this.

- **Architectural consequence:** input must flow through the global session/HID tap with
  the Mirroring window **frontmost** → it **commandeers the real Mac cursor** and the
  window must be front during control. Continuous native-feel drag still works (stream
  `mouseDragged` via the tap); the human/agent control lease already enforces a single
  owner, consistent with a single global cursor. This replaces the spec's "CGEvent to
  pid" model — fold back into the spec once confirmed.
- **Re-probe:** `s1b_session_input <x> <y> [hid|session]` — identical gesture to S1 but
  posts via the session/HID tap at a **global screen** point, window frontmost. Single
  variable changed: post target.

## Re-probe RUNBOOK (real hardware)

```bash
cd /Users/leo/github.com/iphone-remote-panel
git pull            # get the re-probes
cargo build --bin s0_capture --bin s1b_session_input
```

**R0 — capture (expect the abort to be gone):**
```bash
cargo run --bin s0_capture        # Screen Recording TCC already granted
```
- PASS if it prints `NSApplicationLoad() -> true`, then frame lines, and `s0-frames/*.png`
  show the live iPhone screen (changing as you interact). FAIL-still-crashes → tell me.
- FAIL-hangs (no crash, no frames) → it needs a run loop; tell me, that's the next probe.

**R1 — input via session tap (window FRONTMOST):**
```bash
# Mirroring window is at (23,377) size 312x694. Pick a global screen point INSIDE it —
# e.g. roughly the Shortcuts icon. As a quick sanity target, the window centre ≈ (179,724).
# Bring iPhone Mirroring to the front, then:
cargo run --bin s1b_session_input -- 179 724 hid
# if no reaction, try the session tap:
cargo run --bin s1b_session_input -- 179 724 session
```
- PASS if the phone reacts (a tap/drag at that point). Note which tap (`hid`/`session`)
  worked and whether the Mac cursor jumped. Then optionally test backgrounded/covered to
  confirm the "must be frontmost" constraint.

**Report back (minimal):**
```
R0: NSApplicationLoad=true/false  crash gone? Y/N  frames live? Y/N
R1: hid=Y/N  session=Y/N  cursor-commandeered? Y/N  reacted-when-not-frontmost? Y/N
```

---

# Authorized local run results (2026-06-09, c3d8f16)

This run was executed from the local Hermes/macOS context against the real `iPhone镜像`
window, after pulling `feat/webrtc-rebuild` to `c3d8f16`.

## R0 — S0 capture

Command:

```bash
cd /Users/leo/clawd/iphone-remote-panel
cargo build --workspace
/Users/leo/bin/iphone-shot /tmp/phase0-c3d8-before.png >/dev/null
rm -rf s0-frames
./target/debug/s0_capture
```

Result:

- `NSApplicationLoad() -> true`: **Y**
- crash gone: **Y**
- true iPhone live frames: **Y**
- output: 22 PNG frames written under `s0-frames/` before timeout.
- dimensions: `312x694`.
- luminance: non-black (`avg_lum` roughly 145–165; `nonzero_px` about 201k–214k).
- frame difference check: first-to-later frames differ substantially; later frames become
  mostly static once the home screen stops changing.
- visual inspection: frames show the actual iPhone home screen, not a black/blank or
  placeholder window.

Notes:

- Intermittent `frame extraction failed: get_pixel_buffer: CouldNotGetDataBuffer` messages
  occurred between valid frames. This is not a gate failure because valid real frames were
  produced; production code should tolerate/drop those samples.
- The run eventually timed out after 22 saved frames rather than reaching 30. For the gate
  question (can SCK capture real non-black Mirroring frames?), this is still **PASS**.

## R1 — S1b session/HID input

Setup:

```bash
/Users/leo/bin/iphone-act home
/Users/leo/bin/iphone-shot /tmp/s1b-before.png
osascript -e 'tell application "iPhone Mirroring" to activate'
```

Commands:

```bash
./target/debug/s1b_session_input 179 724 hid
./target/debug/s1b_session_input 179 724 session
```

Result:

- `hid`: **Y** — phone reacted; home screen entered edit/wiggle mode.
- `session`: **Y** — independently confirmed from a normal home screen; phone entered
  edit/wiggle mode.
- cursor commandeered: **Y** — session/HID posting uses the real global cursor path; the
  cursor ends at the target point (`179,724` top-left coordinate; AppKit reported the
  corresponding bottom-origin mouse location as `179,425`).
- reacted when not frontmost: **not tested in this run**.

Artifacts:

- `/tmp/s1b-before.png`
- `/tmp/s1b-after-hid-center.png`
- `/tmp/s1b-after-session-center.png`
- `/tmp/s1b-session-before-isolated.png`
- `/tmp/s1b-session-after-isolated.png`

## Updated gate decision

**Phase 0 hardware gate: PASS for the two immediate blockers tested here.**

- S0 capture assumption is validated: SCK can capture real iPhone Mirroring frames on this
  machine after `NSApplicationLoad()` and the window-picker fixes.
- S1 input assumption is validated in the frontmost/window-focused case: session/HID tap
  events drive the mirrored iPhone, but they use the global cursor path.

Architecture implication: proceed with the GUI-session authorized daemon model. The daemon
must run as a GUI LaunchAgent (or equivalent logged-in desktop session process) with Screen
Recording + Accessibility granted to the signed binary. SSH/Hermes/control clients should
connect to that daemon instead of trying to capture/input from their own unsigned context.

---

## Input vertical — full hardware validation (2026-06-09, HEAD 2bd2bba)

Validated on .190 (real iPhone via Mirroring) by Hermes; code authored on .13.

| Capability | Verdict | Notes |
|---|---|---|
| Video stream (WebRTC H.264) | PASS | 312×694, `video.readyState=4` |
| Tap | PASS | clean sequence; YT Music play/pause toggled |
| Shortcuts (home/spotlight/switcher) | PASS | `cua-driver call press_key` cmd+1/2/3 |
| Text input (US keycodes + real Shift) | PASS (English/digits) | see IME caveat below |
| **Scroll (scroll-wheel)** | **PASS** | swipe→`CGEvent::new_scroll_event`; dir `SCROLL_DIR_V=1.0`, `SCROLL_SCALE=1.3` |
| Drag (mouse-drag) | N/A for scroll | Mirroring reads mouse-drag as long-press/reorder, never scroll — hence the scroll-wheel path |
| LAN WebRTC | PASS | trickle ICE, fe80 filtered |

### Key findings

1. **iPhone Mirroring scrolls only on scroll-wheel events**, not mouse-drag. A finger
   swipe must map to `CGEvent::new_scroll_event` (PIXEL units), not `LeftMouseDragged`.
   The client gesture model was reworked: tap→tap, swipe→scroll packets (7-byte),
   long-press→mouse hold, long-press-then-drag→mouse-drag. The old eager per-gesture
   `down` was dropped (it was being read as long-press/icon-reorder).

2. **Text keycode injection is correct.** "Missing digit" symptom (`Hello123`→`Hello23`)
   is the iPhone **Chinese Pinyin IME hijacking number keys as candidate-selectors**
   (`a1b2c3`→`啊不c3`); pure digits (`2341`,`91`) land perfectly. Mirroring forwards
   keycodes + the real Shift KEY (not `CGEventFlagShift`). Real CJK needs an on-phone IME.

3. **HID tap requires the Mirroring window frontmost.** A tap no-op was traced to the
   test harness (screenshots/CDP/Chrome) stealing frontmost; `cua-driver call click`
   (pid/window_id-targeted) is frontmost-independent and worked throughout. In production
   the Mac is idle with Mirroring frontmost, so this is not a blocker — but routing taps
   through cua-driver (like key/text/shortcut already are) would harden it.
