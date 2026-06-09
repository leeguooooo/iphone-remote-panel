# v1 → v2 feature-parity checklist

Audit of v1 (`phone_remote_server.py`, `README.md`, `scripts/`) so the WebRTC rebuild
drops nothing. Full inventory in the audit; this is the actionable parity + gaps.

## v1 endpoints
`/` →302 `/phone`; `/login` (GET page, POST password→HMAC cookie); `/logout`; `/phone`
(auth-gated UI); `GET /api/screenshot` (returncode 0 **or 4** tolerated); `POST /api/tap`
{x,y}; `POST /api/swipe` {direction∈up/down/left/right, size∈short/medium/long};
`POST /api/shortcut` {name∈home/spotlight/switcher}; `POST /api/type` {text, ≤500 chars,
cua-driver type_text→type fallback}. Auth: password→`exp:nonce:sig` HMAC cookie, 8h TTL,
`SameSite=Lax; HttpOnly` (no Secure), headers `Cache-Control:no-store` + `X-Frame-Options:DENY`
+ `Referrer-Policy:no-referrer`. Locks: `screenshot_lock` + `action_lock`.

## v1 web UI affordances + exact timings
Refresh; Auto-refresh toggle (default on, **1200ms** interval, only when `auto && !busy`);
**12×24 grid overlay** (numbered 1–288); Home/Search(spotlight)/Switcher buttons; text input
(Enter sends); Logout; tap-on-screenshot (**<18px=tap**, maps via naturalWidth/rect); drag→swipe
(≥18px; dir by dominant axis; size **>220=long, >110=medium, else short**); tap dot (350ms fade);
toast (1600ms); status line. Reload delays: tap 350 / swipe 650 / shortcut 600 / type 500 ms.

## Parity checklist → v2 component
| v1 feature | v2 component | status |
|---|---|---|
| Live view | capture/encode/webrtc + web | upgrade (poll→WebRTC) |
| Tap / drag→swipe | core::input + web | ✅ (continuous CGEvents; coord map carries over) |
| **Home / Search / Switcher buttons** | core::input + web + mcp | ⚠️ only Home was implied — name Search+Switcher explicitly |
| **Text input forwarding** | core::input + http + mcp + web | ⚠️ add typing path (CGEvent keystroke or cua-driver) + UI field |
| **12×24 numbered grid overlay** | web client only | ⚠️ NOT in v2 design — re-add as canvas over `<video>` |
| Password login + HMAC token + headers | core::auth (done) + front::http | ⚠️ gate the **WebRTC signaling**, not just the page; add `Secure` |
| screenshot/action locks | core::control (done) | ✅ superseded by control lease |
| Start/stop scripts (tunnel/pid/secret) | deployment | → LaunchAgent (signing, auto-login) |
| MCP/agent control | front::mcp | NEW; must reach tap/swipe/shortcut/type parity, **connect-in not stdio-spawn** |

## Gaps to add to the plan (drop-risk)
1. **Search + Switcher** as first-class input actions + web buttons + MCP verbs.
2. **Text input** path + UI field + MCP `type` (v1: ≤500 chars).
3. **12×24 grid overlay** — client canvas (agent coordinate aid).
4. **Status/latency/connection display** + tap-dot + toast (latency display more important under
   WebRTC — no per-frame timestamp).
5. **Swipe API:** human sends continuous gestures, but **keep discrete `{direction,size}` verbs
   in MCP/HTTP** for agent ergonomics (v1 thresholds 18/110/220px).
6. **Freeze/pause** control (replaces the now-meaningless auto-refresh toggle; lets an agent tap
   carefully).
7. **Auth parity:** HMAC token 8h, `SameSite=Lax; HttpOnly; Secure`, random password+secret, the
   3 security headers — gating signaling.
8. **Security-notes doc** carried forward (loopback bind, temporary tunnels, never share URL/
   password, stop after use, **don't leave payment/2FA/private-chat open**) + TURN-relay + lease
   caveats.
9. **config overrides** (host/port/ttl/state-dir/cua-driver) — covered by `server::config` (done).
10. Capture: tolerate the benign "returncode 4"-equivalent (v1 accepted it) — i.e. drop idle SCK
    samples (already handled).
