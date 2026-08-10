# iphone-use — Deployment Operator Notes

## Default deployment: direct WDA

`PHONE_REMOTE_BACKEND=direct` is the product default. The daemon talks to services on the
iPhone through localhost relays:

- WDA control: `PHONE_REMOTE_WDA_URL=http://127.0.0.1:8100`
- WDA video: `PHONE_REMOTE_WDA_MJPEG_URL=http://127.0.0.1:9100`
- browser video: authenticated `GET /agent/mjpeg`
- browser input: cookie-authenticated `POST /control`
- agent input: bearer-authenticated `POST /agent/input`

Direct mode needs neither iPhone Mirroring nor macOS Screen Recording/Accessibility
permission. If WDA is unavailable, commands fail closed; operators must not configure a
Mac-side cursor injector as an automatic fallback.

The packaged daemon is still a per-user LaunchAgent (`com.leeguoo.iphone-use`). This keeps
configuration, logs, app signing, and the WDA supervisor in one user-owned installation.
Clients connect to that already-running daemon; do not start competing copies.

## First-device prerequisites

Before treating a host as ready:

1. Install **full Xcode.app**. Command Line Tools alone are insufficient.
2. In Xcode → Settings → Accounts, sign in and select an Apple development team.
3. Enable Developer Mode on the iPhone.
4. Pair the iPhone with the Mac, accept **Trust** on the phone, and use USB for initial
   setup. Install `libimobiledevice` (`iproxy`); the supported path binds both Mac-side
   relays to `127.0.0.1`. There is no automatic Wi-Fi/`socat` fallback.
5. Keep the target iPhone unlocked, awake, and connected while WDA builds and starts.
6. On multi-device Macs, set `PHONE_REMOTE_UDID` for the daemon and the same classic UDID
   as `WDA_UDID` when running the setup helper.

WDA cannot unlock a phone or bypass Face ID/passcode. A free Personal Team profile may
need periodic renewal.

## Auto-login requirement for unattended machines

Because the shipped service is a LaunchAgent, it starts only after that user logs in. On
an unattended lab Mac, either arrange an approved login/session strategy or expect the
operator to log in after reboot. If policy permits auto-login:

> System Settings → Users & Groups → *your account* → "Automatically log in as…"

Without auto-login, the LaunchAgent never loads after a reboot.

## Legacy mirror backend

Select `PHONE_REMOTE_BACKEND=mirror` only for compatibility with the old
ScreenCaptureKit + WebRTC + CGEvent design. That mode requires iPhone Mirroring plus
Screen Recording and Accessibility grants in an Aqua session. An SSH-spawned process or
session-0 LaunchDaemon cannot satisfy those UI/TCC requirements.

On macOS versions that periodically re-prompt for Screen Recording, an operator must
answer in the GUI (or deploy an approved MDM PPPC profile), then restart the LaunchAgent.
This maintenance does not apply to the direct backend.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh
```

For local / dev installs (pre-built .app):

```sh
./install.sh /path/to/iPhoneUse.app
```

`install.sh` installs/configures the daemon and copies the WDA helper; it does not prove
that Xcode signing, the selected device, WDA, both relays, and the browser path work on
real hardware. The same atomic app replacement also installs the release-matched MCP
bridge at `~/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp`; configure MCP
clients with that absolute path. Complete device setup separately:

```sh
~/.iphone-use/setup-wda.sh doctor
~/.iphone-use/setup-wda.sh
~/.iphone-use/setup-wda.sh status
```

For multiple paired iPhones, persist a target before installing and pass the same value
to setup:

```sh
export PHONE_REMOTE_UDID=00008…
curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh
WDA_UDID="$PHONE_REMOTE_UDID" ~/.iphone-use/setup-wda.sh
```

Only a deliberate mirror deployment should grant Screen Recording and Accessibility.

## Runtime configuration

| Variable | Direct default | Operator responsibility |
|---|---:|---|
| `PHONE_REMOTE_BACKEND` | `direct` | Use `mirror` only for explicit legacy compatibility. |
| `PHONE_REMOTE_UDID` | auto-detect | Pin the classic UDID on multi-device hosts; keep it aligned with setup's `WDA_UDID`. |
| `PHONE_REMOTE_WDA_URL` | `http://127.0.0.1:8100` | The control relay must answer `/status`. |
| `PHONE_REMOTE_WDA_MJPEG_URL` | `http://127.0.0.1:9100` | The MJPEG relay must remain reachable for live browser video. |
| `PHONE_REMOTE_WDA_MANAGED` | `true` for installer-owned loopback WDA | Keep `false` for external/custom WDA; the daemon must not stop or rebootstrap a service it does not own. |
| `PHONE_REMOTE_IDLE_RELEASE_SECS` | `300` | `0` disables release; otherwise ensure reconnect can restart WDA with the phone unlocked. |

WDA itself does not authenticate `8100/9100`. Loopback-bound `iproxy` prevents exposing
the Mac relays, but it does not firewall the WDA listener on the iPhone. Use this Phase 1
backend only on a trusted, isolated network (or turn off iPhone Wi-Fi while using USB).

## Useful launchctl commands

```sh
UID=$(id -u)
LABEL=com.leeguoo.iphone-use

# Status
launchctl print gui/$UID/$LABEL

# Restart (kills running instance, starts fresh)
launchctl kickstart -k gui/$UID/$LABEL

# Stop (stays disabled until bootstrapped again)
launchctl bootout gui/$UID/$LABEL

# Load from plist (re-install or after manual edit)
launchctl bootstrap gui/$UID ~/Library/LaunchAgents/$LABEL.plist

# Enable auto-start at next login
launchctl enable gui/$UID/$LABEL

# View logs
tail -f ~/Library/Logs/iPhoneUse/iphone-use.log
tail -f ~/Library/Logs/iPhoneUse/iphone-use.err
```

## Live acceptance checklist

Do not mark a host ready from installation output, source inspection, unit tests, or a
healthy launchd job alone. On the intended physical phone:

1. With iPhone Mirroring closed and iPhoneUse absent from Screen Recording and
   Accessibility, verify `/agent/status` reports `backend:"direct"`, the pinned target,
   `wda:true`, `wda_actionable:true`, and `drivable:true`.
2. From a second browser/device, verify `/phone` receives a continuously updating
   `/agent/mjpeg` stream and that the UI reports a stopped `9100` relay honestly.
3. Verify tap, drag, long-press, scroll, ASCII, and CJK input through `/control`, including
   explicit failure when WDA is stopped. Confirm the Mac cursor never moves.
4. Verify bearer-authenticated `/agent/elements`, `/agent/screenshot`, and `/agent/input`.
5. Exercise idle release/reconnect, phone lock/unlock, USB removal/reconnect, daemon
   restart, Mac reboot, and WDA re-sign/reinstall.
6. On a multi-device Mac, prove recovery never switches to another paired phone.

Record the device model/iOS, Mac/macOS, Xcode version, development-team type, transport,
and observed results. Keep component proof, automated tests, daemon proof, and end-to-end
hardware proof as separate evidence.

## Uninstall

The repository ships an ownership-gated, idempotent uninstaller. Preview its exact
actions first:

```sh
~/.iphone-use/uninstall.sh --dry-run
~/.iphone-use/uninstall.sh
```

It verifies LaunchAgent labels and `ProgramArguments`, validates stored PID/start-time
metadata before signalling WDA processes, and deletes only fixed iphone-use paths. It
removes the current daemon and WDA supervisor plists, verified legacy daemon artifacts,
`iPhoneUse.app`, product logs, the official WDA checkout, and product state under
`~/.iphone-use`. Unknown files and ownership mismatches are preserved and reported.
Shared Xcode/Homebrew installations, `iproxy`, and `socat` are never removed.
An official WDA checkout with tracked changes or untracked files is also preserved.
If any supervisor/process cannot be ownership-verified and stopped, its plist, PID
records, logs, setup checkout, and the uninstaller itself remain available for recovery
and the command exits nonzero.

The WDA runner app on the iPhone is retained by default. To remove it too, first get the
canonical device UDID already stored by iphone-use and the exact installed runner bundle
identifier (the installed test runner often ends in `.xctrunner`), then opt in explicitly:

```sh
~/.iphone-use/uninstall.sh --remove-phone-runner \
  --udid 00008110-001234567890001E \
  --bundle-id com.example.iphone-use.wda.xctrunner
```

Phone removal first requires the supplied UDID and bundle identity to match the
ownership-verified daemon/WDA configuration; it cannot be used to switch to another
connected phone during uninstall. It then uses a bounded `devicectl` operation against
that one explicit device and bundle. If the phone is unavailable or the deadline expires,
local cleanup continues and the script prints the exact command to retry.

If this host previously used the legacy mirror backend, also revoke its old TCC grants:
System Settings → Privacy & Security → Screen Recording / Accessibility, find iPhoneUse,
and toggle it off. Direct deployments should not have these grants.

## Signing boundaries

There are two separate signatures:

- The macOS `iPhoneUse.app` signature covers Gatekeeper and, for legacy mirror mode,
  TCC's Designated Requirement. Keep bundle id `com.leeguoo.iphone-use` and reuse its
  signing identity across upgrades.
- WebDriverAgent is signed by the operator's Apple development team for installation on
  the iPhone. Its provisioning lifetime and device trust are independent of the macOS
  app signature.

CI uses Developer ID when secrets are configured; otherwise `install.sh` creates/reuses
the local `"iPhoneUse Local Signing"` identity and re-signs the downloaded app. This does
not sign, trust, start, or hardware-validate WDA.

## Secrets for CI (optional — only needed for Developer ID signing + notarization)

| Secret | Purpose |
|---|---|
| `APPLE_SIGNING_CERTIFICATE` | Base64-encoded `.p12` Developer ID Application cert |
| `APPLE_SIGNING_CERTIFICATE_PASSWORD` | Password for the `.p12` |
| `APPLE_SIGN_IDENTITY` | Cert name, e.g. `Developer ID Application: Foo Bar (TEAMID)` |
| `APPLE_ID` | Apple ID email for notarytool |
| `APPLE_ID_PASSWORD` | App-specific password for notarytool |
| `APPLE_TEAM_ID` | 10-character Apple Developer Team ID |

Without these secrets the CI ships an unsigned `.app` and `install.sh` signs it locally.
`GITHUB_TOKEN` is always available automatically — no extra setup needed.
