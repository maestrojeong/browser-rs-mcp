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
- [x] `ab-mcp` — rmcp stdio server exposing 10 `browser_*` tools
- [x] Verified end-to-end against real Chrome (**5/5** fingerprint self-test,
      click-by-ref navigates the page)
- [x] post-action **settle-diff** (act tools return the accessibility-tree delta)
- [x] `browser_fingerprint_check` self-test + UA de-headless
- [ ] tabs switching / windows / network interception / download / pdf tools
- [ ] streamable-HTTP transport

## Tools

`browser_navigate` · `browser_snapshot` · `browser_click` · `browser_type` ·
`browser_press` · `browser_evaluate` · `browser_screenshot` · `browser_tabs` ·
`browser_close_page` · `browser_fingerprint_check`

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

## Layout

```
crates/
  ab-cdp/      # CDP transport: one WS, flatten sessions, command/event routing
  ab-browser/  # Browser + Page: launch, stealth, snapshot, screenshot, evaluate
  ab-mcp/      # MCP server (rmcp) — the only serving surface  [WIP]
```

## License

Apache-2.0. See [LICENSE](LICENSE).
