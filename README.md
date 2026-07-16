# browser-rs

A high-performance, **stealth-first** browser served only over **MCP** — no
bundled agent, no LLM, no chat UI. Point any MCP client (Claude Code, Cursor,
your own agent) at it and drive a **real** browser.

Think of it as *patchright-mcp, rebuilt in Rust* — the browser an agent can
actually use well, in a single ~5 MB binary.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/maestrojeong/browser-rs-mcp/main/install.sh | sh

browser-rs --port 9321         # HTTP MCP: /mcp (streamable) + /sse (legacy)
browser-rs                     # or stdio
```

No npm/Node/Rust needed — the script downloads the prebuilt binary for your
platform. Alternatives: grab a binary from [Releases](../../releases), or
`cargo install --git https://github.com/maestrojeong/browser-rs-mcp ab-mcp`.

Full instructions (version pinning, direct download, MCP client setup,
updating): **[INSTALL.md](INSTALL.md)**.

Register with an MCP client:

```jsonc
{ "mcpServers": { "browser-rs": {
  "command": "browser-rs"                    // stdio
} } }
// HTTP: run `browser-rs --port 9321` and point the client at
//   http://127.0.0.1:9321/mcp?owner=user:group:topic
// The same owner may instead be sent as X-Browser-Owner.
```

## Shared profiles and tab ownership

HTTP clients can share one browser process and persistent profile while keeping
their tabs isolated. Give every topic, worker, or cron job a stable `owner`:

```text
http://127.0.0.1:9321/mcp?owner=42:100:research
X-Browser-Owner: 42:100:research
```

New tabs are assigned to the request owner. `browser_pages`, `browser_tabs`, and
every page operation resolve only pages owned by that connection. Knowing
another owner's concrete page id is not enough to access it. On an owner-scoped
connection, `browser_close` closes only that owner's tabs; it does not stop the
shared browser.

Hosts should clean up an owner when its topic or job is deleted:

```bash
curl -X DELETE 'http://127.0.0.1:9321/owners?owner=42%3A100%3Aresearch'
```

This closes the owner's real Chrome tabs and removes its in-memory mappings
without deleting the profile or affecting other owners. Connections without an
owner retain process-wide administrative access, so keep the HTTP listener on
localhost or behind a trusted proxy.

## vs mcp-patchright

| | mcp-patchright | **browser-rs** |
|---|---|---|
| Language / runtime | Node + Playwright | **Rust**, single static binary |
| Browser control | Playwright (Patchright) | **raw CDP**, one multiplexed WebSocket |
| Stealth approach | *patch away* automation tells | *don't create them* — **be a real browser** |
| Strongest mode | stealth-patched launch | **attach to your own Chrome** (`--connect`) — identical fingerprint |
| Agent's view | HTML / DOM dump | **accessibility tree + `[ref]`**, act returns a **settle-diff** |
| Tool surface | ~60 tools | **59 tools** (near-complete parity) |
| Transports | stdio + HTTP/SSE | **stdio + HTTP/SSE** (same CLI flags) |
| Footprint | ~79 MB install, ~182 MB RSS | **~5 MB binary, ~6 MB RSS** |
| Startup / per-op | baseline | **~100× faster start, ~2–3× per op** |
| License | — | **Apache-2.0** |

Stealth against detectors is on par (both show **no detections** on
rebrowser-bot-detector.net); the difference is a lighter Rust stack, a
different stealth philosophy, and an accessibility-first interface.

## Stealth: be a real browser

The reliable way to be undetectable is to **not differ from a human's Chrome in
the first place**. Injecting JS to override `navigator.webdriver`, `toString`,
`screen`, WebGL, `deviceMemory`, … passes naive detectors but each override is
itself an anomaly — and detectors (Akamai, Kasada, DataDome, and open ones like
CreepJS / incolumitas) flag exactly those inconsistent combinations.

So by default browser-rs **injects nothing**: it runs **headful** on real
hardware with a **persistent real profile**, sets only the
`AutomationControlled` launch flag (so `navigator.webdriver` is natively false),
never enables the detectable `Runtime`/`Console` CDP domains, and evaluates JS in
an **isolated world**. Nothing to hide, because nothing was faked.

| Mode | How | Fingerprint |
|---|---|---|
| **Default** | headful, persistent profile, no patching | a real Chrome's |
| **Connect** (strongest) | `--connect 9222` → attach to a Chrome you started with `--remote-debugging-port=9222` | *literally your everyday browser* |
| **Headless fallback** | `--headless --stealth` → opt-in JS patch layer | best-effort; only where headful is impossible |

Result: **0 detections** on
[rebrowser-bot-detector.net](https://bot-detector.rebrowser.net) and
[bot.sannysoft.com](https://bot.sannysoft.com), and **0% stealth** on CreepJS —
all with zero page patching.

## Tools (59)

**Read/see:** `navigate` · `new_page` · `snapshot` · `read` (markdown) ·
`get_visible_html` · `get_visible_text` · `find` · `take_screenshot` · `save_pdf` · `pages` · `tabs` ·
`switch_page` · `status`
**Act (by `ref` or CSS `selector`):** `click` · `type` · `press_key` · `hover` ·
`select_option` · `fill_form` · `drag` · `file_upload` · `navigate_back` · `wait_for` · `resize` ·
`evaluate` · `run_code_unsafe` · `iframe_click` · `iframe_fill` · `close_page` · `close`
**Network:** `network_requests` · `route_block` · `route_clear` ·
`network_state_set` (offline) · `route_mock` · `api_request`
**Cookies:** `cookie_{list,get,set,delete,clear}`
**Web storage:** `localstorage_{list,get,set,delete,clear}` ·
`sessionstorage_{list,get,set,delete,clear}` · `storage_save` · `storage_load`
**Diagnostics:** `console_messages` · `fingerprint_check`
**Dialogs/debug:** `handle_dialog` · `highlight` · `hide_highlight`
**Auth:** `webauthn` (install a virtual authenticator so passkey prompts don't block; sites fall back to password)

Act tools take a snapshot `ref` **or** a CSS `selector`, wait for the page to
settle, and return a **diff of the accessibility tree** — the "did it work"
signal. Clicks/typing use human-like mouse paths and key timing.

## CLI / flags (patchright-compatible)

```
browser-rs                          # stdio MCP transport
browser-rs --port 9321 [options]    # HTTP MCP transport at /mcp
  --host <host>            HTTP bind host (default 127.0.0.1)
  --user-data-dir <path>   persistent browser profile directory
  --headless | --headed    run headless or headful (default headful)
  --connect <port|url>     attach to a Chrome on --remote-debugging-port
  --stealth                inject the headless JS stealth-patch layer
```

Every flag has an env equivalent (`AB_HTTP`, `AB_PROFILE`, `AB_HEADLESS`,
`AB_CONNECT`, `AB_STEALTH`, `AB_CHROME`). Because it takes `--port` +
`--user-data-dir` like `mcp-patchright`, a host can allocate one port per
profile and connect multiple owner-scoped sessions to it.

## Benchmarks (browser + detector co-evolve)

The repo ships its own bot detector and CI gates on it — a new detector check
that fails must be met by a stealth fix in the same commit.

```bash
node bench/run.mjs        target/release/browser-rs   # headless fallback layer (CI gate)
node bench/external.mjs   target/release/browser-rs   # headful vs bot.sannysoft.com
node bench/rebrowser.mjs  target/release/browser-rs   # CDP tells vs rebrowser-bot-detector.net
```

## Layout

```
crates/
  ab-cdp/      # CDP transport: one WS, flatten sessions, command/event routing
  ab-browser/  # Browser + Page: launch, stealth, snapshot, act, network, storage
  ab-mcp/      # MCP server (rmcp) — stdio + HTTP/SSE, the only serving surface
bench/         # the bot-detection page + runners (CI regression gate)
install.sh     # curl | sh installer (downloads the prebuilt binary)
```

## Releasing

Cutting a new version is one commit + one tag — CI builds the binaries and the
`curl | sh` installer picks up the latest release automatically:

```bash
# bump the version in Cargo.toml (workspace.package.version), then:
git commit -am "Release vX.Y.Z"
git tag vX.Y.Z && git push origin main vX.Y.Z
```

Pushing a `v*` tag triggers `.github/workflows/release.yml`, which builds
macOS-arm64 + Linux-x64 binaries (with SHA-256 sums) and attaches them to the
GitHub Release. `install.sh` defaults to `releases/latest`, so no other change
is needed (pin a version with `AB_VERSION=vX.Y.Z`).

## License

Apache-2.0. See [LICENSE](LICENSE).
