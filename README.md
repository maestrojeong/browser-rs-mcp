# agent-browser

A high-performance, stealth-first browser **served only over MCP** — no bundled
agent, no LLM, no chat UI. Point any MCP client (Claude Code, Cursor, your own
agent) at it and drive a real browser.

Think of it as *patchright-mcp, rebuilt in Rust for maximum performance and
capability* — the browser an agent can actually use well.

## agent-browser vs patchright-mcp

| | patchright-mcp | **agent-browser** |
|---|---|---|
| Language | Node/TS | **Rust** (tokio, single static binary) |
| Browser control | Playwright (Patchright) | **raw CDP**, one multiplexed WebSocket |
| Stealth approach | *remove* automation tells from a stock binary | *don't create them* — **be a real headful Chrome** |
| Strongest mode | stealth-patched launch | **`AB_CONNECT` → your own running Chrome** (identical fingerprint) |
| Agent's view of a page | HTML / DOM dump | **accessibility tree + `[ref]`**, act returns a **settle-diff** |
| Network control | route/block/mock | ✅ `network_requests` + `route_block` |
| Session reuse | storage state | ✅ `storage_save` / `storage_load` (cookies + localStorage) |
| Serving | MCP | **MCP only** (no bundled agent) |
| License | — | **Apache-2.0** |

Same lineage (an external driver, not a Chromium fork), but a different stealth
philosophy, a Rust hot path, and an accessibility-first agent interface.

## Why

| | Playwright / patchright-mcp | **agent-browser** |
|---|---|---|
| Language | Node/TS | **Rust** (tokio, zero-GC) |
| Browser control | Playwright abstraction | **raw CDP** over one multiplexed WebSocket |
| CDP domain control | library decides | **we do** — never send `Runtime.enable` (the #1 automation tell) |
| Agent view | HTML / DOM dump | **accessibility snapshot** with `[ref=eN]` handles |
| Stealth | don't fake it — **be a real browser** | headful + real profile + no JS patching (patchright philosophy) |
| Serving | MCP | **MCP only** (stdio + streamable HTTP) |
| License | — | **Apache-2.0** |

## Design in one line

> Don't patch the leaks — don't create them. Run a real headful Chrome with a
> real profile and touch nothing on the page, never enable the detectable CDP
> domains, and hand the agent a token-efficient accessibility tree — all in Rust.

## Stealth philosophy: be a real browser

The reliable way to be undetectable is to **not differ from a human's Chrome in
the first place**. Injecting JS to override `navigator.webdriver`,
`Function.prototype.toString`, `screen`, WebGL, `deviceMemory`, etc. passes naive
detectors but each override is itself an anomaly — and an *inconsistent* one
(e.g. a clamped `deviceMemory` that no longer matches the real machine). Advanced
defenses (Akamai, Kasada, DataDome) flag exactly those abnormal combinations.

So by default agent-browser:

- runs **headful** on real hardware (headless is the single biggest tell),
- uses a **persistent real profile** (aged cookies/history look human; a fresh
  temp profile every run does not),
- **injects nothing** into the page — the fingerprint is a real Chrome's because
  it *is* one,
- only sets the `AutomationControlled` launch flag (so `navigator.webdriver` is
  natively false) and never enables the `Runtime`/`Console` CDP domains.

Result: it passes [bot.sannysoft.com](https://bot.sannysoft.com) with **0
failures without touching a single page property**.

### Modes

| Mode | How | Fingerprint |
|---|---|---|
| **Default** | headful launch, persistent profile, no patching | a real Chrome's |
| **Connect** (strongest) | `AB_CONNECT=9222` → attach to a Chrome you launched with `--remote-debugging-port=9222` | *literally your everyday browser* |
| **Headless fallback** | `AB_HEADLESS=1 AB_STEALTH=1` → opt-in JS patch layer | best-effort; use only where headful is impossible |

Env: `AB_PROFILE=<dir>` sets the profile location; `AB_CHROME=<path>` picks the
browser.

## Status

- [x] `ab-cdp` — multiplexed CDP client (flatten sessions, event stream)
- [x] `ab-browser` — Chrome launcher, stealth layer, `navigate` / `evaluate` /
      `snapshot` / `screenshot` + act (`click` / `type` / `press` by ref)
- [x] `ab-mcp` — rmcp stdio server exposing **24** `browser_*` tools
- [x] "be a real browser" stealth: headful + real profile + no patching; passes
      **bot.sannysoft.com with 0 failures** with zero page patching
- [x] `AB_CONNECT` — attach to your own running Chrome (identical fingerprint)
- [x] post-action **settle-diff** (act tools return the accessibility-tree delta)
- [x] network log + URL blocking, cookie/localStorage session save/load
- [x] dialogs auto-accepted so automation never hangs
- [ ] file upload / drag / streamable-HTTP transport

## Tools (24)

**Read/see:** `navigate` · `snapshot` · `read` (markdown) · `get_html` ·
`screenshot` · `pdf` · `pages` · `tabs`
**Act:** `click` · `type` · `press` · `hover` · `select` · `fill_form` · `back` ·
`wait` · `evaluate` · `resize` · `close_page`
**Network/session:** `network_requests` · `route_block` · `storage_save` ·
`storage_load`
**Stealth:** `fingerprint_check`

Act tools wait for the page to settle and return a **diff of the accessibility
tree** — the cheap "did it work" signal.

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
detect automation — plus two runners. They grow together: a new detector check
that fails must be met by a fix **in the same commit**. CI gates on it.

```bash
node bench/run.mjs target/release/agent-browser        # headless fallback layer (CI gate)
node bench/external.mjs target/release/agent-browser   # default headful mode vs bot.sannysoft.com
node bench/rebrowser.mjs target/release/agent-browser  # CDP-automation tells vs rebrowser-bot-detector.net
```

Against **rebrowser-bot-detector.net** (which targets Playwright/Puppeteer CDP
leaks) agent-browser shows **no 🔴 detections**: `navigatorWebdriver`,
`runtimeEnableLeak`, `pwInitScripts`, `viewport`, `useragent` all green, and the
interactive `mainWorldExecution` / `dummyFn` probes stay untriggered because
`browser_evaluate` runs in an isolated world.

```
✓  navigator.webdriver      undefined
✓  navigator.plugins        5
✓  navigator.languages      ...
✓  userAgent !headless      Mozilla/5.0 ...
✓  viewport plausible       outer 1280x787, screen 1280x800
score: 8/8 — looks human ✓
```

It also passes the real-world **[bot.sannysoft.com](https://bot.sannysoft.com)**
detector with **0 failures** — every WebDriver / HeadlessChrome / PhantomJS /
Selenium / CDP-devtools probe comes back clean:

```bash
node bench/external.mjs target/release/agent-browser   # informational, needs network
```

Scored checks are environment-independent (WebGL renderer is spoofed to a
hardware GPU when the real one is software, so it scores consistently on
GPU-less CI). The CDP-console probe stays **informational** — reliable CDP
self-detection from JS doesn't exist; agent-browser's defense is architectural
(it never enables the Runtime/Console domains).

## Headless fallback patch layer

In the default headful mode none of this is needed — the values below are all
naturally correct. But when you're forced to run headless (`AB_HEADLESS=1
AB_STEALTH=1`), an opt-in pre-document script normalizes the signals headless
Chrome gets wrong. It's a best-effort compromise, not the recommended path:

| Signal a site reads | Headless / automated tell | What agent-browser does |
|---|---|---|
| `navigator.webdriver` | `true` | force `undefined` |
| `navigator.plugins` / `mimeTypes` | empty | plausible non-empty set |
| `navigator.languages` | empty | fill if empty |
| `window.chrome` | missing | shimmed |
| User-Agent | contains `HeadlessChrome` | de-headlessed via CDP override |
| `window.outerWidth/Height` | `0` | mirror inner size |
| `screen.width/height` | `800×600` < window | normalized ≥ window |
| WebGL `UNMASKED_RENDERER` | SwiftShader / llvmpipe | spoof a hardware GPU **only when software** |
| `navigator.deviceMemory` | true RAM (e.g. 16) | clamp to ≤ 8 (as real Chrome does) |
| hooked fn `.toString()` | reveals JS source | masked as `[native code]` |
| CDP `Runtime.enable` | detectable | never sent (architectural) |

Every row is guarded by the `bench/` detector (run in fallback mode) so a
regression turns CI red.

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
  `target/release/agent-browser`, or grab a prebuilt binary from GitHub
  Releases — pushing a `v*` tag builds macOS-arm64 and Linux-x64 binaries
  (with SHA-256 sums) and attaches them to the release.
- **Detector:** stays in this monorepo. It is valuable first as a **CI
  regression gate**; publishing it as a public benchmark page (GitHub Pages
  from `bench/detector.html`) is an optional, zero-infra follow-up.

## License

Apache-2.0. See [LICENSE](LICENSE).
