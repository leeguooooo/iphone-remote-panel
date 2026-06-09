# iPhone Remote Panel

A small authenticated web remote for macOS iPhone Mirroring.

It serves a local page at `http://127.0.0.1:8787/phone` with:

- live iPhone Mirroring screenshots
- tap-on-screenshot control
- drag-to-swipe control
- Home / Search / App Switcher shortcuts
- text input forwarding
- optional 12×24 click grid overlay
- temporary password login
- optional Cloudflare quick tunnel for remote access

## Requirements

- macOS with iPhone Mirroring available
- [`cloudflared`](https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/) for public tunnel support
- Existing local helper commands:
  - `/Users/leo/bin/iphone-shot`
  - `/Users/leo/bin/iphone-act`
  - `/Users/leo/.local/bin/cua-driver`

The helper paths can be overridden with environment variables:

```bash
IPHONE_SHOT=/path/to/iphone-shot \
IPHONE_ACT=/path/to/iphone-act \
CUA_DRIVER=/path/to/cua-driver \
./scripts/phone-remote-start
```

## Start

```bash
./scripts/phone-remote-start
```

Example output:

```text
local=http://127.0.0.1:8787/phone
tunnel=https://example.trycloudflare.com/phone
password=<temporary-password>
server_pid=123 tunnel_pid=456
logs=/tmp/hermes-phone-remote/server.log /tmp/hermes-phone-remote/cloudflared.log
```

Open the local or tunnel URL and enter the temporary password.

## Stop

```bash
./scripts/phone-remote-stop
```

## Security notes

This tool exposes live phone control through a browser. Treat the tunnel URL and password like sensitive credentials.

Recommended usage:

- keep the server bound to `127.0.0.1`
- use temporary Cloudflare quick tunnels only when needed
- do not share the tunnel URL or password
- stop the service after use
- do not leave payment apps, private chats, or 2FA screens open while sharing the tunnel

The generated password/secret and logs live under `/tmp/hermes-phone-remote` by default and are not intended for commit.

## Files

- `phone_remote_server.py` — Python HTTP server and web UI
- `scripts/phone-remote-start` — starts the server and Cloudflare tunnel
- `scripts/phone-remote-stop` — stops the processes recorded in state files
