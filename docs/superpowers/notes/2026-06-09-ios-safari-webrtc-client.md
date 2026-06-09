# iOS Safari WebRTC client playbook (web client)

Research June 2026, iOS 17/18 Safari. For the web-client task. `⚠️` = flagged.

## Video element + playback
- `<video id="screen" autoplay playsinline webkit-playsinline muted>` — **all four load-bearing**:
  `playsinline` (else iOS forces native fullscreen, kills the touch overlay), `muted`
  (required for autoplay gate), `autoplay` (best-effort). NO `controls`. Add
  `disablePictureInPicture disableRemotePlayback`.
- Attach: `pc.ontrack = e => { video.srcObject = e.streams[0] ?? new MediaStream([e.track]); tryPlay(); }`.
  Never `createObjectURL`.
- **`play()` fallback (required):** autoplay can stall to black. Call `video.play()`, on
  rejected promise show a "Tap to start" overlay whose click calls `video.play()` inside the
  gesture (always allowed). Re-run on `visibilitychange→visible` (iOS pauses video when
  backgrounded). `video.paused===true` after attach = show overlay.
- **Latency:** `playoutDelayHint`/`jitterBufferTarget` are ~no-ops on iOS — set defensively,
  don't rely. Real levers are sender-side (no B-frames, short GOP). Don't set `preload`,
  don't reattach `srcObject` needlessly. Monitor `pc.getStats()` inbound-rtp
  `jitterBufferDelay`, `freezeCount`, `framesDropped`.

## H.264 profile (daemon interop — DECISIVE)
- **Safari advertises/prefers `profile-level-id=42e01f`** (Constrained Baseline 3.1).
  **Daemon offer fmtp MUST be byte-identical: `level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f`.** Do NOT offer only `42001f` (plain Baseline) — older WebKit
  may not select it. (S0b-2 probe used `42001f` for desktop — swap to `42e01f` for prod/iOS.)
- ⚠️ Level `1f`=3.1 (~720p ceiling). Full-screen iPhone res may exceed it — bump the level
  byte (`42e029`=4.1) to match real resolution×fps, else profile/level-mismatch decode
  artifacts (WebKit bug 195124). Test on-device. We downscale to the ~312×694 window so 3.1
  is likely fine.

## Signaling + ICE
- **Daemon = offerer, browser = answerer** (recommended): daemon owns media + both data
  channels + dictates fmtp; browser just `setRemoteDescription(offer)`→`createAnswer`→
  `setLocalDescription`. Video arrives as `recvonly` via `ontrack`; data channels via
  `pc.ondatachannel`.
- **iOS `.local` mDNS host candidates** (no cam/mic grant → obfuscated). LAN-direct needs the
  daemon to resolve mDNS, else fall back to relay. ⚠️ iOS Local Network Permission decline →
  no host candidates at all. **A remote mDNS candidate can't pair with a local relay
  candidate** — ensure both ends have relay/srflx (not mDNS-vs-relay).
- **Provide a relay-only fallback ICE config** (Cloudflare TURN, `iceTransportPolicy:'relay'`
  both ends) — most reliable cross-network, sidesteps mDNS/Local-Network issues.
- **ICE restart (daemon-initiated):** browser detects `iceConnectionState` `disconnected`→
  schedule restart, `failed`→signal daemon to `createOffer({iceRestart:true})`. Re-`play()` +
  restart on `visibilitychange→visible`. Network change (wifi↔cell) is NOT seamless on iOS →
  must restart. Debounce.

## Data channels (daemon creates both)
- `control` `{ordered:true}` (down/up/key/text); `move` `{ordered:false, maxRetransmits:0}`
  (can't set both maxRetransmits+maxPacketLifeTime). `channel.binaryType='arraybuffer'`.
  Messages ≤16 KiB. Don't send until `readyState==='open'`.
- **Backpressure: DROP move, don't queue.** `if (moveCh.bufferedAmount > 256*1024) return;`
  Pack move as compact binary (2×uint16 of x/y*65535 + flags), not JSON.

## Touch capture → normalized content-rect coords
- **Pointer Events** (`pointerdown/move/up/cancel`) + `setPointerCapture`. `getCoalescedEvents()`
  is **iOS 18+ only** (absent iOS 17) → feature-detect, fall back to single point.
- Suppress gestures: CSS `touch-action:none; user-select:none; -webkit-touch-callout:none;
  -webkit-tap-highlight-color:transparent` on video+overlay; `html,body{overscroll-behavior:none;
  overflow:hidden}`; `e.preventDefault()` in non-passive pointer handlers;
  `document.addEventListener('gesturestart', e=>e.preventDefault())`. ⚠️ `user-scalable=no`
  is IGNORED in a normal Safari tab (honored only in standalone PWA).
- **Content-rect mapping (object-fit:contain):** compute letterbox from `videoWidth/videoHeight`
  vs element rect AR; `nx=(px-offX)/contentW`, `ny=(py-offY)/contentH`; **return null if outside
  [0,1]** (tap on letterbox bar). Use CSS px throughout (clientX + getBoundingClientRect both
  CSS px). Daemon's `coords` module multiplies [0,1] by phone pixels — **this matches `core::coords`.**
- Tap vs drag vs long-press: TAP_MS 250, TAP_SLOP 10px, LONGPRESS_MS 500. Stream `move` on the
  lossy channel via coalesced events; control verbs on the reliable channel.

## Fullscreen / PWA / viewport
- `<meta viewport ... viewport-fit=cover>`; `#screen{width:100vw;height:100dvh;object-fit:contain;
  background:#000}` (use `100dvh` not `100vh`); safe-area insets on chrome via `env(safe-area-inset-*)`.
- Standalone PWA (`apple-mobile-web-app-capable`, manifest `display:standalone`) removes Safari
  chrome AND honors `user-scalable=no`. ⚠️ Re-test `play()` gesture in standalone.
- **Wake Lock** `navigator.wakeLock.request('screen')` works in a tab from iOS 16.4+; ⚠️ BROKEN
  in standalone PWA until iOS 18.4 (WebKit bug 254545). Re-acquire on visibility. Backstop: the
  live playing `<video>` already inhibits auto-lock substantially.
