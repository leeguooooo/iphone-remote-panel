# iPhone Remote — Deployment Operator Notes

## Why a GUI-session LaunchAgent (not a LaunchDaemon)

iphone-remote requires **Screen Recording** (ScreenCaptureKit) and **Accessibility** (CGEventPost)
TCC grants. Both grants are evaluated by macOS against the *responsible-process chain*:

- A **LaunchDaemon** runs in session 0, which has no WindowServer. ScreenCaptureKit will
  refuse to capture and the grants are not evaluated. **Wrong tool.**
- An **SSH-spawned process** inherits the SSH terminal's chain, not the GUI user's Accessibility
  grant. CGEventPost is silently dropped.
- A **GUI-session LaunchAgent** (label `work.pwtk.iphone-remote`, `LimitLoadToSessionType=Aqua`)
  is a direct child of `launchd` in the Aqua session, so macOS evaluates the daemon's own TCC
  entry. **This is the only correct deployment mode.**

**Consequence:** no client (SSH session, MCP call, Hermes, iPhone app) may spawn its own copy
of `iphone-remote`. All callers connect to the *already-running* LaunchAgent over HTTP/IPC.

## Auto-login requirement for unattended machines

The daemon only starts after a desktop (Aqua) user login. On headless / lab machines:

> System Settings → Users & Groups → *your account* → "Automatically log in as…"

Without auto-login, the LaunchAgent never loads after a reboot.

## Monthly Screen Recording re-prompt (macOS 15+)

macOS ~monthly resets the Screen Recording authorization and shows a re-prompt dialog.
The dialog **must be answered in the GUI** by a logged-in user — it cannot be suppressed
without an MDM PPPC profile. The daemon detects the loss via `CGPreflightScreenCaptureAccess`
and exits cleanly (KeepAlive restarts it), but capture will fail until re-granted.

Operational checklist for unattended boxes:
1. Monitor `~/Library/Logs/iPhoneRemote/iphone-remote.err` for "Screen Capture permission lost".
2. Log in to the GUI (VNC / Screen Sharing) and re-grant Screen Recording.
3. Restart: `launchctl kickstart -k gui/$(id -u)/work.pwtk.iphone-remote`

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh
```

For local / dev installs (pre-built .app):

```sh
./install.sh /path/to/iPhoneRemote.app
```

After install, grant **Screen Recording** and **Accessibility** in System Settings when prompted.

## Useful launchctl commands

```sh
UID=$(id -u)
LABEL=work.pwtk.iphone-remote

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
tail -f ~/Library/Logs/iPhoneRemote/iphone-remote.log
tail -f ~/Library/Logs/iPhoneRemote/iphone-remote.err
```

## Uninstall

```sh
UID=$(id -u)
LABEL=work.pwtk.iphone-remote

launchctl bootout gui/$UID/$LABEL 2>/dev/null || true
launchctl disable gui/$UID/$LABEL 2>/dev/null || true
rm -f ~/Library/LaunchAgents/$LABEL.plist
rm -rf ~/Applications/iPhoneRemote.app
rm -rf ~/Library/Logs/iPhoneRemote
```

To also revoke TCC grants: System Settings → Privacy & Security → Screen Recording / Accessibility,
find iPhoneRemote and toggle off.

## Codesigning and TCC grant stability

TCC grants are keyed on the code-signing **Designated Requirement** (bundle-id + signing identity).
To preserve grants across upgrades:

- The bundle id **must remain `work.pwtk.iphone-remote`** in every release.
- The signing identity must be reused. CI uses Developer ID (via `APPLE_SIGNING_CERTIFICATE` secret)
  when available; otherwise `install.sh` creates a persistent self-signed cert named
  `"iPhoneRemote Local Signing"` in the login keychain and re-signs the downloaded .app.
  Either way the DR is stable — re-granting after an upgrade should not be required.

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
