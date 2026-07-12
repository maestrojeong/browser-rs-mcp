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
      `snapshot` / `screenshot`
- [x] Verified end-to-end against real Chrome (`navigator.webdriver = undefined`)
- [ ] `ab-mcp` — rmcp server exposing `browser_*` tools
- [ ] `act` (click/type/fill by ref) + post-action settle diff
- [ ] tabs / windows / network / download / pdf tools
- [ ] fingerprint self-test tool

## Try it

```bash
cargo run -p ab-browser --example smoke -- https://example.com
```

Set a specific browser with `AB_CHROME=/path/to/chrome`.

## Layout

```
crates/
  ab-cdp/      # CDP transport: one WS, flatten sessions, command/event routing
  ab-browser/  # Browser + Page: launch, stealth, snapshot, screenshot, evaluate
  ab-mcp/      # MCP server (rmcp) — the only serving surface  [WIP]
```

## License

Apache-2.0. See [LICENSE](LICENSE).
