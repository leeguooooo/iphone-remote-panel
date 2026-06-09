# Integration cheat-sheet: webrtc-rs H.264 → iPhone Safari + Cloudflare TURN

Research-stamped June 2026 (for spikes S0b-2 and S3). `[VERIFY]` = confirm against a live
run before relying on it.

## A — webrtc-rs H.264 video track

- **Crate:** pin `webrtc = "0.17"` (0.17.1 stable; 0.20.0-alpha exists).
- **Track type:** `TrackLocalStaticSample` (NOT `TrackLocalStaticRTP`). It takes whole
  encoded NAL access units and runs the H264 RTP payloader (FU-A/STAP-A, timestamps)
  internally. Path: `webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample`.
  `Sample` = `webrtc::media::Sample`; `MIME_TYPE_H264` = `"video/H264"`.
- **Input format:** Annex-B (start-code-delimited NAL). VideoToolbox emits **AVCC**
  (length-prefixed) with SPS/PPS **out-of-band** in `CMVideoFormatDescription`. So the
  pipeline must: (a) convert AVCC length prefixes → Annex-B start codes, and (b) **inject
  SPS+PPS in-band before every IDR** (no `sprop-parameter-sets` in fmtp). The S0b-1 probe
  already emits in-band Annex-B SPS/PPS per keyframe — reuse that, but reshape for RTP
  (SPS/PPS as separate NALs, not a byte-stream blob).
- **Codec capability fmtp:** `level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f`
  (webrtc-rs `register_default_codecs` default, mirrors pion). **Safari nuance `[VERIFY]`:**
  Safari advertises `42e01f` (constrained-baseline marker). Try `42001f` first; if the
  video m-line drops in the answer, register a custom codec with `42e01f`. Both are CB 3.1.
- **Keyframes:** wire **PLI-driven** keyframes — read RTCP from the `rtp_sender`, force a
  VT IDR on PLI. Without it Safari freezes/blackens after any loss. (S0b-1 used a fixed 1s
  interval; production wants PLI + force-IDR-on-viewer-join.)
- **Skeleton:** `MediaEngine::register_default_codecs` → `register_default_interceptors`
  → `APIBuilder` → `new_peer_connection(config)` → `TrackLocalStaticSample::new(H264 cap)`
  → `pc.add_track` → `on_ice_candidate` (trickle to browser via our WS) → set remote
  offer / create+set answer → loop `track.write_sample(&Sample{ data: nal_annexb,
  duration: frame_interval, ..})`.
- **Safari/iOS gotchas:** profile match (above); in-band SPS/PPS per IDR mandatory; stick
  to Constrained Baseline + pm=1 + no B-frames (avoid High — narrow iOS HW decode); iOS
  emits `.local` mDNS host candidates (moot under relay-only); ensure interceptors run so
  PLI reaches the app `[VERIFY]`.

## B — Cloudflare Realtime TURN (formerly Calls)

- **Generate creds (server-side only — never ship the token to the browser):**
  ```
  POST https://rtc.live.cloudflare.com/v1/turn/keys/$TURN_KEY_ID/credentials/generate-ice-servers
  Authorization: Bearer $TURN_KEY_API_TOKEN
  {"ttl": 86400}
  ```
  → 201 with `{ "iceServers": { "urls": [stun/turn/turns...], "username": "...",
  "credential": "..." } }` (`iceServers` is an **object**, not array). `[VERIFY]` exact
  `urls` contents against a live response.
- **webrtc-rs config:** `RTCConfiguration { ice_servers: vec![RTCIceServer{ urls,
  username, credential, ..}], ice_transport_policy: RTCIceTransportPolicy::Relay, .. }`.
  Set `Relay` on **both** ends for the S3 TURN spike (forces relay); use `All` (default)
  in production so direct paths win. Browser mirror: `iceTransportPolicy: 'relay'`.
- **TTL/refresh:** max TTL 48 h. Established allocations keep working; re-mint before
  expiry for new allocations / ICE restart. Browser refresh: fetch new creds →
  `pc.setConfiguration({iceServers})` + ICE restart (`createOffer({iceRestart:true})`).
  webrtc-rs ICE-restart API name `[VERIFY]` (`restart_ice()` vs offer option) for 0.17.
- **Free tier:** first 1000 GB/mo TURN egress free, then $0.05/GB; STUN free. Needs a TURN
  Key (`TURN_KEY_ID` + bearer token) created in the Realtime dashboard.

### Open verifies before coding S0b-2 / S3
1. `42001f` vs `42e01f` for Safari negotiation.
2. AVCC→Annex-B + per-IDR SPS/PPS reshaping for RTP (our responsibility).
3. webrtc-rs 0.17 ICE-restart API name.
4. Live Cloudflare `generate-ice-servers` response shape + dashboard token scope.

Sources: docs.rs `webrtc` 0.17.1; webrtc-rs `examples/play-from-disk-h264`; pion
`mediaengine.go`; developers.cloudflare.com/realtime/turn/.
