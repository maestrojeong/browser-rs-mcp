# Installing browser-rs

A single prebuilt binary — no npm, Node, or Rust toolchain required.

## Quick install (curl)

```bash
curl -fsSL https://raw.githubusercontent.com/maestrojeong/browser-rs-mcp/main/install.sh | sh
```

Downloads the binary for your platform (macOS arm64 / Linux x64) and installs
it as `browser-rs` in `/usr/local/bin` (or `~/.local/bin` if that isn't
writable).

Verify:

```bash
browser-rs --help
```

## Options

```bash
# Pin a specific version
AB_VERSION=v0.1.9 curl -fsSL https://raw.githubusercontent.com/maestrojeong/browser-rs-mcp/main/install.sh | sh

# Choose the install directory
AB_BIN_DIR=~/bin curl -fsSL https://raw.githubusercontent.com/maestrojeong/browser-rs-mcp/main/install.sh | sh
```

If `~/.local/bin` isn't on your `PATH`:

```bash
export PATH="$HOME/.local/bin:$PATH"   # add to ~/.zshrc or ~/.bashrc
```

## Direct binary download (no script)

```bash
# macOS (Apple Silicon)
curl -fsSL -o browser-rs https://github.com/maestrojeong/browser-rs-mcp/releases/latest/download/browser-rs-macos-arm64
# Linux (x64)
curl -fsSL -o browser-rs https://github.com/maestrojeong/browser-rs-mcp/releases/latest/download/browser-rs-linux-x64

chmod +x browser-rs
./browser-rs --help
```

SHA-256 sums are published alongside each asset (e.g.
`browser-rs-macos-arm64.sha256`).

## Build from source

```bash
cargo install --git https://github.com/maestrojeong/browser-rs-mcp ab-mcp
```

## Run

```bash
browser-rs                     # stdio MCP transport
browser-rs --port 9321         # HTTP MCP: http://127.0.0.1:9321/mcp (streamable) + /sse (legacy)
```

Common flags: `--host <host>`, `--user-data-dir <path>` (persistent profile),
`--headless` / `--headed`, `--connect <port|url>` (attach to your own Chrome),
`--stealth` (headless JS patch layer). Each has an env equivalent (`AB_HTTP`,
`AB_PROFILE`, `AB_HEADLESS`, `AB_CONNECT`, `AB_STEALTH`, `AB_CHROME`).

## Register with an MCP client

```jsonc
{ "mcpServers": { "browser-rs": {
  "command": "browser-rs"                    // stdio
} } }
```

For HTTP, run `browser-rs --port 9321` and point the client at:

- `http://127.0.0.1:9321/mcp` — streamable HTTP (e.g. Codex, `type: "http"`)
- `http://127.0.0.1:9321/sse` — legacy SSE (e.g. Claude SDK, `type: "sse"`)

For a shared profile, append a stable owner to each client connection:

```text
http://127.0.0.1:9321/mcp?owner=user%3Agroup%3Atopic
```

The equivalent `X-Browser-Owner` header is also accepted. Owner-scoped clients
can list and control only their own tabs. When deleting a topic or worker, call
`DELETE /owners?owner=...` to close only its tabs. Ownerless connections have
administrative access to all tabs, so do not expose the HTTP port publicly.

## Updating

Re-run the curl one-liner; it always fetches the latest release. To replace a
binary that may be running, install to a temp path and `mv` it into place
(overwriting a running executable in place can corrupt it):

```bash
curl -fsSL -o /tmp/browser-rs.new https://github.com/maestrojeong/browser-rs-mcp/releases/latest/download/browser-rs-macos-arm64
chmod +x /tmp/browser-rs.new && mv -f /tmp/browser-rs.new "$(command -v browser-rs)"
```
