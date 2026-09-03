# browser-rs — Design

## Goal

An open-source, **MCP-only** browser server that an agent can use *better* and
*faster* than any existing option. No agent/LLM is bundled — this is purely the
controllable browser + its MCP surface. Performance and low detectability are
first-class.

Positioning: the deep-CDP power of a Chromium fork (like BrowserOS) minus the
fork's build/maintenance cost, combined with patchright's stealth intent —
delivered as a lean Rust MCP server driving stock Chrome.

## Non-goals

- No bundled agent, planner, or model routing.
- No Chromium fork (initially). We drive stock Chrome/Chromium via CDP. The
  stealth design is deliberately *forward-compatible* with a future fork that
  removes fingerprints at the C++ source level.
- No Playwright dependency — it adds latency and hides CDP control we need.

## Core theses

1. **Raw CDP > Playwright** for both performance and control. One WebSocket
   multiplexes the browser target and every page session ("flatten" mode).
2. **Be a real browser, don't fake one.** The reliable path to an undetectable
   fingerprint is to not differ from a human's Chrome at all: run **headful**
   with a **persistent real profile** and inject **nothing** into the page.
   Overriding properties from JS (webdriver, toString, screen, WebGL,
   deviceMemory) passes naive detectors but creates anomalous, mutually
   inconsistent combinations that advanced defenses flag. On top of that,
   **stealth by omission** at the protocol level: never send the detectable
   `Runtime.enable` / `Console.enable` (`Runtime.evaluate` works one-shot
   without `enable`; the accessibility tree needs only the non-page-visible
   `Accessibility.enable`).
3. **The agent's world model is the accessibility tree,** not raw HTML. It is
   smaller (fewer tokens), more stable across re-renders, and maps cleanly to
   "act by ref". Interactive nodes carry snapshot-scoped refs that bind a
   backendDOMNodeId to the page target and main-frame loader identity.
4. **Rust hot path.** tokio + tokio-tungstenite, no GC pauses, single small
   static binary — ideal for an MCP server spawned per session.

## Architecture

```
MCP client (Claude Code / agent)
        │  stdio or HTTP (MCP + owner identity)
        ▼
   ab-mcp   ── rmcp server, browser_* tools
        │
        ▼
 ab-browser ── Browser (process + connection) / Page (tab session)
        │        launch flags + native page APIs + snapshot/act/screenshot
        ▼
   ab-cdp   ── one WebSocket, flatten sessions, id→response routing, event bus
        │
        ▼
   Chrome (stock) via --remote-debugging-port
```

### ab-cdp
- `CdpClient::connect(ws_url)` → spawns a reader task.
- `send` / `send_on(session_id, …)` → id-tagged request, `oneshot` reply.
- Events fan out on a `broadcast` channel, tagged with optional `sessionId`.
- Deliberately dumb: it does not auto-enable any domain.

### ab-browser
- `Browser::launch(LaunchOptions)`: spawn Chrome (persistent user-data-dir, minimal
  flags, `--remote-debugging-port=0`), read `DevToolsActivePort`, discover the
  WS endpoint via `/json/version`, connect.
- `Browser::new_page(url)`: `Target.createTarget` + `attachToTarget{flatten}`;
  apply the headless-only user-agent override when requested, then navigate.
- `Page`: `navigate` (Page.enable + load wait), `evaluate` (Runtime.evaluate,
  no enable), `snapshot` (Accessibility.getFullAXTree → compact outline),
  `screenshot` (Page.captureScreenshot).
- `stealth.rs`: minimal launch flags; page-visible API behavior stays native.
- `snapshot.rs`: prune ignored/noise nodes, print `role "name"` indented, assign
  `[ref]` to interactive roles, keep a ref→backendDOMNodeId map for act tools.

### ab-mcp
- rmcp `ServerHandler` over stdio and streamable-HTTP.
- Tools mirror the proven minimal set: `browser_navigate`, `browser_snapshot`,
  `browser_act`, `browser_evaluate`, `browser_screenshot`, `browser_read`,
  `browser_tabs`, `browser_wait`, `browser_download`, `browser_pdf`.
- Core agent loop the tools encode: **snapshot → act → verify** (act reads back
  a settle diff so the agent doesn't blind-retry).

### HTTP ownership isolation

One HTTP server represents one Chrome process and one persistent profile.
Multiple clients share that process by connecting with distinct owner values in
the `owner` query parameter or `X-Browser-Owner` header.

`State` tracks both `pages: pageId -> Page` and
`page_owners: pageId -> owner`. All page-taking tools pass through the common
page resolver, which rejects a page unless its owner matches the request owner.
Page listing uses the same filter. This makes isolation an enforcement rule,
not merely a filtered UI.

Owner lifecycle is explicit. `DELETE /owners?owner=...` closes every real tab
owned by that identity and removes its page and alias mappings. The profile and
other owners remain live. `browser_close` has the same owner-scoped behavior
when called through an owner connection; only an ownerless administrative
connection may close the complete browser.

An embedding host may turn owner isolation into an authentication boundary with
`AB_MANAGED=1`. The process receives a random root capability, but clients only
receive `HMAC-SHA256(root, owner)`. Both HTTP transports bind their MCP session
to that verified owner; `/owners` remains an administrative root-only endpoint.
Standalone stdio and unauthenticated loopback HTTP remain available when managed
mode is not enabled.

Credential transformation remains host-owned. When
`AB_SECRET_BROKER_SOCKET` and `AB_SECRET_BROKER_TOKEN` are configured,
Browser.rs calls the Unix-socket broker before and after every tool invocation:
`transform_input` returns the substituted input and a one-time lease, then
`redact_output` consumes that lease. Redaction happens before caller-requested
truncation. A configured broker is a security boundary, so connection,
protocol, timeout, or redaction errors return a sanitized failure instead of
the unprocessed tool result.

## Roadmap

1. **ab-mcp MVP** — stdio server, tools: navigate / snapshot / evaluate /
   screenshot / act(click,type,fill by ref) / tabs.
2. **act settle-diff** — after an action, wait out DOM churn and return a diff of
   the accessibility tree (cheap "did it work" signal).
3. **Network + interception** — `browser_route_block/mock`, request logging
   (Fetch domain; still no Runtime.enable).
4. **Fingerprint self-test** — a `browser_fingerprint_check` tool that loads a
   detector page and reports leaks; regression-guards stealth.
5. **Multi-tab / windows / hidden window** — isolation for parallel subtasks.
6. **(Optional) fork mode** — swap stock Chrome for a patched build that removes
   `navigator.webdriver` at Blink source and routes introspection off the
   detectable Runtime path. Same MCP surface, deeper stealth.

## Open decisions

- rmcp version/API surface (mirror BrowserOS's `rmcp 2.x` usage).
- Snapshot ref stability across re-renders (backendDOMNodeId vs a stamped attr).
- Whether to keep a persistent user-data-dir option for logged-in sessions.
