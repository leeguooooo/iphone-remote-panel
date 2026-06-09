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
