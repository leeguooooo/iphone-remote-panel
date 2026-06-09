# Video pipeline hardware runbook (`core::capture` + `core::encode`)

How to smoke-test the productionised capture+encode pipeline on a granted macOS
host. The unit tests cover the pure logic (AVCC→Annex-B, keepalive decision,
config); the OS/SCK/VT path can only be validated against real hardware with an
iPhone Mirroring window + Screen Recording (TCC) granted.

## What ships
- `crates/core/src/capture.rs` — `CaptureStream`: finds the Mirroring window
  (via the pure `window::select_mirroring` picker), opens an `SCStream` (BGRA,
  30 fps, no cursor), feeds each frame to a callback as a `Frame { image_buffer,
  pts_micros, width, height }`. A watcher thread restarts the stream on
  window-id change.
- `crates/core/src/encode.rs` — `VideoPipeline` trait + `start_pipeline()`:
  VideoToolbox H.264 (Constrained Baseline, realtime, no B-frames), AVCC→Annex-B
  with in-band SPS/PPS on keyframes, `tokio::sync::broadcast` of `EncodedFrame`,
  `request_keyframe()` force-IDR, and a **keepalive thread that re-encodes the
  last buffer as a forced IDR after ~500 ms idle**.

## Public interface (what the WebRTC layer consumes)
```rust
pub struct EncodedFrame { pub data: bytes::Bytes, pub is_keyframe: bool, pub pts_micros: u64 }
pub struct PipelineConfig { fps, bitrate, max_keyframe_interval, keepalive_millis, channel_capacity, show_cursor }
pub trait VideoPipeline: Send + Sync {
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<EncodedFrame>;
    fn request_keyframe(&self);
}
pub fn start_pipeline(cfg: PipelineConfig) -> anyhow::Result<Arc<dyn VideoPipeline>>;
// pure, unit-tested helpers:
pub fn avcc_to_annex_b(avcc: &[u8], param_sets: Option<(&[u8], &[u8])>) -> Result<Vec<u8>, String>;
pub fn keepalive_should_fire(idle: Duration, keepalive: Duration, have_buffer: bool) -> bool;
```

## Preconditions
1. A real iPhone connected via **iPhone Mirroring** with the phone window visible.
2. **Screen Recording** permission granted to whatever binary runs the test
   (Terminal, or the built test harness) — System Settings → Privacy & Security
   → Screen Recording. Without it, `SCShareableContent::get` errors / yields no
   frames.
3. `NSApplicationLoad()` must be called once at process start (the spikes do this
   in `main`; any binary embedding the pipeline must do the same to avoid
   `CGS_REQUIRE_INIT`).

## Smoke test — drive the pipeline directly
Easiest path: add a throwaway bin (or a `#[ignore]` integration test) that starts
the pipeline, subscribes, and dumps frame stats. Sketch:

```rust
fn main() -> anyhow::Result<()> {
    objc2_app_kit::NSApplicationLoad();
    let rt = tokio::runtime::Runtime::new()?;
    let pipe = core::encode::start_pipeline(core::encode::PipelineConfig::default())?;
    let mut rx = pipe.subscribe();
    pipe.request_keyframe(); // simulate a viewer join
    rt.block_on(async move {
        let mut n = 0;
        let mut keyframes = 0;
        let mut out = std::fs::File::create("pipeline.h264").unwrap();
        use std::io::Write;
        while n < 300 { // ~10s @ 30fps
            match rx.recv().await {
                Ok(f) => {
                    out.write_all(&f.data).unwrap();
                    if f.is_keyframe { keyframes += 1; }
                    println!("frame {n}: {} bytes pts={}us{}", f.data.len(), f.pts_micros,
                        if f.is_keyframe { "  [KEYFRAME]" } else { "" });
                    n += 1;
                }
                Err(e) => { eprintln!("recv: {e:?}"); break; }
            }
        }
        println!("{n} frames, {keyframes} keyframes -> pipeline.h264");
    });
    Ok(())
}
```

## What to assert (PASS criteria)
1. **First frame is a keyframe** (idx 0 is force-IDR), carrying SPS+PPS — the
   first emitted `EncodedFrame.is_keyframe == true` and its `data` starts with
   `00 00 00 01 67…` (SPS) then `…68…` (PPS) then the IDR slice.
2. **`request_keyframe()` forces an IDR** — call it mid-stream; the very next
   emitted frame is a keyframe.
3. **Static-screen keepalive** — stop touching the phone so the screen is fully
   static. SCK delivers no frames, but the stream must NOT stall: a fresh
   **keyframe** must appear roughly every 500 ms (the keepalive IDR). Confirm the
   keepalive frames are keyframes, not repeated P-frames.
4. **Playback** — `ffplay pipeline.h264` (or VLC) shows the live phone screen,
   clean, with no color-block drift during static periods.
5. **Window restart** — quit + relaunch iPhone Mirroring while running; the
   watcher should pick up the new window id and resume frames within ~1 s.

## Failure modes / notes
- No frames at all → permission not granted, or no Mirroring window. The
  `find_mirroring_window` error lists all on-screen windows to debug the picker.
- `CouldNotGetDataBuffer` is normal SCK idle behaviour and is dropped silently
  inside `capture.rs` — not an error.
- Color-block drift during static screen ⇒ keepalive is emitting non-keyframes
  (regression); the keepalive path force-IDRs by design.
- Bitrate/fps/keepalive are tunable via `PipelineConfig`.

## Concerns to flag for the WebRTC integrator
- **PTS source**: live frames carry the SCK-derived PTS (microseconds);
  keepalive frames synthesize a wall-clock-derived PTS to stay monotonic. If the
  RTP payloader needs strictly frame-interval-spaced timestamps, normalize on the
  WebRTC side.
- **Resolution change on window restart**: a new window can have different
  dimensions, but the VT session is created once at the initial size. If the
  Mirroring window changes resolution mid-session, the encoder should be torn
  down and `start_pipeline` re-run (the WebRTC layer would renegotiate). This is
  a known limitation — flagged for a follow-up if dynamic resize is required.
- **Broadcast lag**: a slow viewer that can't keep up drops the oldest frames
  (bounded channel). On `RecvError::Lagged`, the WebRTC layer should call
  `request_keyframe()` so the viewer re-syncs on the next IDR.
```
