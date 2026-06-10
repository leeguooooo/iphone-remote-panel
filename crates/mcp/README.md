# iphone-remote-mcp

MCP stdio server that wraps the `iphone-remote` daemon's agent HTTP API,
giving MCP clients (Claude Desktop, Claude Code, etc.) native tool access
to an iPhone via iPhone Mirroring.

## Prerequisites

The `iphone-remote` daemon must already be running on the Mac that has iPhone
Mirroring open:

```bash
iphone-remote serve
```

## Installation

Build from the workspace:

```bash
cargo build --release -p iphone-remote-mcp
# Binary at: target/release/iphone-remote-mcp
```

## Configuration

| Env var | Default | Description |
|---|---|---|
| `PHONE_REMOTE_URL` | `http://127.0.0.1:8787` | Base URL of the daemon |
| `PHONE_REMOTE_TOKEN` | _(none)_ | Bearer token / password (omit for open-mode daemons) |

## MCP client config

### Claude Desktop (`~/Library/Application Support/Claude/claude_desktop_config.json`)

```json
{
  "mcpServers": {
    "iphone-remote": {
      "command": "/path/to/iphone-remote-mcp",
      "env": {
        "PHONE_REMOTE_URL": "http://127.0.0.1:8787",
        "PHONE_REMOTE_TOKEN": "your-password"
      }
    }
  }
}
```

### Claude Code (project `.claude/settings.json` or `~/.claude/settings.json`)

```json
{
  "mcpServers": {
    "iphone-remote": {
      "command": "/path/to/iphone-remote-mcp",
      "env": {
        "PHONE_REMOTE_URL": "http://127.0.0.1:8787",
        "PHONE_REMOTE_TOKEN": "your-password"
      }
    }
  }
}
```

Remote Mac (daemon on a different machine on the LAN):

```json
{
  "mcpServers": {
    "iphone-remote": {
      "command": "/path/to/iphone-remote-mcp",
      "env": {
        "PHONE_REMOTE_URL": "http://192.168.1.50:8787",
        "PHONE_REMOTE_TOKEN": "your-password"
      }
    }
  }
}
```

## Tools exposed

| Tool | Arguments | Description |
|---|---|---|
| `phone_status` | — | Query daemon status; returns `{"ok":true,"phone_target":bool}` |
| `phone_screenshot` | — | Capture current screen → PNG image content |
| `phone_tap` | `x`, `y` (0–1) | Single tap at normalized position |
| `phone_scroll` | `x`, `y`, `dx`, `dy` | Scroll-wheel gesture; negative `dy` scrolls content up |
| `phone_type` | `text` | Type US-ASCII text (CGEvent injection) |
| `phone_key` | `name` | Named key: `return`, `escape`, `space`, `tab`, `delete`, `up`, `down`, `left`, `right` |
| `phone_shortcut` | `name` | System shortcut: `home`, `spotlight`, `switcher` |

## Typical session flow

1. `phone_status()` — confirm `ok=true` and `phone_target=true`
2. `phone_screenshot()` — see the current screen
3. `phone_tap(x, y)` — tap a button (coordinates from screenshot)
4. Repeat from step 2

## Implementation notes

- Uses the official [rmcp](https://crates.io/crates/rmcp) crate v1.7.0 with
  `transport-io` (stdio) and `schemars` features.
- All daemon errors surface as MCP tool errors with the daemon's HTTP status
  and body text.
- Logs to **stderr** at `INFO` level by default; set `RUST_LOG=debug` for
  verbose output.  Stderr is forwarded to the MCP client's diagnostics view
  and never mixed with the stdio protocol stream.
