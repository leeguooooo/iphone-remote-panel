# macOS deployment playbook — TCC + LaunchAgent + codesign

Research June 2026 (macOS 14 Sonoma / 15 Sequoia). For Tasks 15/18/19. `⚠️` = flagged uncertainty.

## §0 — The load-bearing finding: responsible-process chain (read first)

On macOS 15, `AXIsProcessTrusted()` / `CGEventPost` evaluate the **responsible-process /
caller chain**, not just the daemon's own TCC entry. Empirically (15.7.3):
- SSH terminal → spawns daemon → trusted **true** (inherits Terminal's grants)
- `node bot` → `spawn(daemon)` → trusted **FALSE** even with a valid Accessibility entry + matching cdhash
- **LaunchAgent (launchd-direct child) → daemon's OWN entry is evaluated → true once granted**

**Architectural rule:** the daemon MUST be a direct child of `launchd` via LaunchAgent.
**No client (SSH / Hermes / iPhone) may `spawn` its own copy** — they connect to the
already-running LaunchAgent over IPC/HTTP. **This is why `front::mcp` must be a
connect-in surface (HTTP/SSE/socket to the running daemon), NOT a per-call stdio-spawn
MCP** — a spawned child loses the grant and `CGEventPost` is silently dropped.

(Basis: user's own memory `feedback_tcc_responsible_process_chain_2026_05_21.md`.)

## 1. LaunchAgent
- `~/Library/LaunchAgents/<label>.plist` (reverse-DNS label, filename matches). Keys:
  `Label`, `ProgramArguments` (ABSOLUTE path to binary inside the .app + `serve`),
  `RunAtLoad=true`, `KeepAlive=true` (launchd throttles relaunch ≥10 s),
  `StandardOutPath`/`StandardErrorPath` (create parent dir first), `ProcessType=Interactive`
  (scheduling only — no TCC effect), `LimitLoadToSessionType=Aqua` (explicit = safe).
- Modern launchctl (write these, not legacy `load -w`):
  `launchctl bootstrap gui/$UID <plist>` (load), `bootout gui/$UID/<label>` (unload),
  `kickstart -k gui/$UID/<label>` (restart, kills running), `print gui/$UID/<label>`
  (status — grep `state = running`, `last exit code`), `enable gui/$UID/<label>`.
  Idempotent install = `bootout || true` then `bootstrap`. Run as the logged-in GUI user, **no sudo**.
- **Gotchas:** a LaunchAgent runs only after a GUI user logs in (Aqua session) — a
  headless/SSH-only box needs **auto-login** enabled or the agent never starts (Screen
  Recording needs an active WindowServer). A LaunchDaemon (session 0) has NO WindowServer
  → wrong tool. ⚠️ macOS 15 re-prompts Screen Recording ~monthly; must be answered in the
  GUI, can't be suppressed without MDM PPPC — operational caveat for unattended boxes.

## 2. TCC grants
- TCC keys on the code-signing **Designated Requirement** (identity + bundle id; cdhash as
  handle), NOT path. Unsigned/ad-hoc → cdhash changes each build → grant lost. **Stable
  signing identity (Developer ID or a reused self-signed cert) → grant persists across
  rebuilds.** Keep ONE stable bundle id.
- Can't grant non-interactively (`tccutil` only resets). Can detect + prompt:
  Screen Recording — `CGPreflightScreenCaptureAccess()` (check, no dialog),
  `CGRequestScreenCaptureAccess()` (first call shows dialog; **process must relaunch after
  toggle** — have `serve` detect via preflight and `exit(0)` so KeepAlive restarts).
  Accessibility — `AXIsProcessTrusted()` (check), `AXIsProcessTrustedWithOptions({prompt:true})`.
  Deep-link panes: `open "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"` / `…?Privacy_Accessibility`.
- **Ship a `.app` bundle** (stable `CFBundleIdentifier`, `NSScreenCaptureUsageDescription`),
  not a bare CLI — macOS may silently refuse to list an unsigned bare binary. Mirrors
  CuaDriver.app / screenpipe.

## 3. Codesigning
- Reusable self-signed Code Signing cert (Keychain Assistant) gives **local** TCC stability;
  sign nested binary then outer .app: `codesign --force --options runtime --sign "<identity>" <path>`.
  Re-sign every build with the SAME cert + bundle id → DR stable → grant survives.
- Entitlements: ScreenCaptureKit + CGEventPost are **TCC-gated, not entitlement-gated** →
  often need NO `com.apple.security.cs.*`. Hardened runtime needed only for notarization.
- Distribution: best = CI Developer-ID-sign + notarize the .app, attach to GitHub Release
  (grants persist across released upgrades, Gatekeeper-clean). Fallback = ship unsigned +
  `install.sh` generates/reuses a persistent self-signed cert and re-signs locally +
  `xattr -dr com.apple.quarantine`.

## 4. install.sh shape
Download+place .app → (sign if unsigned) → verify bundle-id in signature → mkdir logs →
write plist → `bootout||true; bootstrap; enable; kickstart -k` → open the two Settings
panes + daemon raises CG/AX prompts on first serve → `print` status. **User must click:**
toggle Screen Recording + Accessibility, re-confirm monthly, enable auto-login if headless.
