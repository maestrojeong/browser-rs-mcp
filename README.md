# agent-browser

A high-performance, stealth-first browser **served only over MCP** — no bundled
agent, no LLM, no chat UI. Point any MCP client (Claude Code, Cursor, your own
agent) at it and drive a real browser.

Think of it as *patchright-mcp, rebuilt in Rust for maximum performance and
capability* — the browser an agent can actually use well.

## Why

| | Playwright / patchright-mcp | **agent-browser** |
|---|---|---|
| Language | Node/TS | **Rust** (tokio, zero-GC) |
| Browser control | Playwright abstraction | **raw CDP** over one multiplexed WebSocket |
| CDP domain control | library decides | **we do** — never send `Runtime.enable` (the #1 automation tell) |
| Agent view | HTML / DOM dump | **accessibility snapshot** with `[ref=eN]` handles |
| Stealth | runtime patching of stock binary | launch flags + pre-document injection, source-patchable later |
| Serving | MCP | **MCP only** (stdio + streamable HTTP) |
| License | — | **Apache-2.0** |

## Design in one line

> Never enable the detectable CDP domains. Drive the browser through raw CDP,
> hand the agent a token-efficient accessibility tree, keep the whole hot path
> in Rust.

## Status

- [x] `ab-cdp` — multiplexed CDP client (flatten sessions, event stream)
- [x] `ab-browser` — Chrome launcher, stealth layer, `navigate` / `evaluate` /
      `snapshot` / `screenshot` + act (`click` / `type` / `press` by ref)
- [x] `ab-mcp` — rmcp stdio server exposing 11 `browser_*` tools
- [x] Verified end-to-end against real Chrome (**5/5** fingerprint self-test,
      click-by-ref navigates the page)
- [x] post-action **settle-diff** (act tools return the accessibility-tree delta)
- [x] `browser_fingerprint_check` self-test + UA de-headless
- [ ] tabs switching / windows / network interception / download / pdf tools
- [ ] streamable-HTTP transport

## Tools

`browser_navigate` · `browser_snapshot` · `browser_read` · `browser_click` ·
`browser_type` · `browser_press` · `browser_evaluate` · `browser_screenshot` ·
`browser_tabs` · `browser_close_page` · `browser_fingerprint_check`

Act tools (`click` / `type` / `press`) wait for the page to settle and return a
**diff of the accessibility tree** — the cheap "did it work" signal.

## Run as an MCP server

```bash
cargo build --release          # produces target/release/agent-browser
```

Register with an MCP client (e.g. Claude Code / Cursor):

```json
{
  "mcpServers": {
    "agent-browser": {
      "command": "/absolute/path/to/agent-browser/target/release/agent-browser"
    }
  }
}
```

Set a specific browser with `AB_CHROME=/path/to/chrome`.

## Try the core directly (no MCP)

```bash
cargo run -p ab-browser --example smoke -- https://example.com
```

## Stealth benchmark (browser + detector co-evolve)

The repo ships its own **bot detector** — a single page whose only job is to
detect automation — plus a runner that drives agent-browser against it and
grades the result. They grow together: a new detector check that fails must be
met by a stealth fix **in the same commit**. CI gates on it.

```bash
node bench/run.mjs target/release/agent-browser
```

```
✓  navigator.webdriver      undefined
✓  navigator.plugins        5
✓  navigator.languages      ...
✓  userAgent !headless      Mozilla/5.0 ...
✓  viewport plausible       outer 1280x787, screen 1280x800
score: 8/8 — looks human ✓
```

Scored checks are environment-independent (WebGL renderer is spoofed to a
hardware GPU when the real one is software, so it scores consistently on
GPU-less CI). The CDP-console probe stays **informational** — reliable CDP
self-detection from JS doesn't exist; agent-browser's defense is architectural
(it never enables the Runtime/Console domains).

## Layout (monorepo)

Everything lives in one repo on purpose — the browser and the detector that
tests it must move in lockstep.

```
crates/
  ab-cdp/      # CDP transport: one WS, flatten sessions, command/event routing
  ab-browser/  # Browser + Page: launch, stealth, snapshot, act, screenshot
  ab-mcp/      # MCP server (rmcp) — the only serving surface
bench/
  detector.html  # the bot-detection page (also hostable as a public site)
  run.mjs        # drives agent-browser against it; exits nonzero on any leak
.github/workflows/ci.yml   # build + test + stealth-benchmark gate
```

## Distribution

- **Primary:** ship the MCP server binary. `cargo build --release` →
  `target/release/agent-browser`, or grab it from GitHub Releases (planned:
  cross-compiled binaries via CI).
- **Detector:** stays in this monorepo. It is valuable first as a **CI
  regression gate**; publishing it as a public benchmark page (GitHub Pages
  from `bench/detector.html`) is an optional, zero-infra follow-up.

## License

Apache-2.0. See [LICENSE](LICENSE).
