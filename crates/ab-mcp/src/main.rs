//! agent-browser MCP server.
//!
//! Exposes the ab-browser core as `browser_*` MCP tools over stdio. No agent,
//! no LLM — just the browser, driven by whatever MCP client connects.
//!
//! Core loop the tools encode: **snapshot -> act -> verify**.

use std::collections::HashMap;
use std::sync::Arc;

use ab_browser::{Browser, LaunchOptions, NetworkLog, Page};
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::info;

const INSTRUCTIONS: &str = r#"agent-browser — a real Chrome driven over CDP, no bundled agent.

Loop: browser_navigate -> browser_snapshot -> act (click/type) -> re-snapshot to verify.
- snapshot renders the page as an accessibility tree; interactive nodes carry [ref=eN] handles.
- act on them by ref with browser_click / browser_type / browser_press.
- refs go stale when the page changes — re-snapshot before reusing them.
- browser_evaluate runs one-shot JS; browser_screenshot saves a PNG.
Stealth: this browser never enables the detectable CDP domains (no Runtime.enable)."#;

struct PageEntry {
    page: Page,
    refs: HashMap<String, i64>,
    last_text: String,
    netlog: Option<NetworkLog>,
}

/// Order-insensitive line diff: what appeared / disappeared between snapshots.
/// Cheap post-action signal — trims noise so the agent sees only the delta.
fn snapshot_diff(old: &str, new: &str) -> String {
    use std::collections::HashSet;
    let old_lines: HashSet<&str> = old.lines().map(str::trim).collect();
    let new_lines: HashSet<&str> = new.lines().map(str::trim).collect();
    let mut out = String::new();
    for line in new.lines() {
        let t = line.trim();
        if !t.is_empty() && !old_lines.contains(t) {
            out.push_str("+ ");
            out.push_str(line);
            out.push('\n');
        }
    }
    for line in old.lines() {
        let t = line.trim();
        if !t.is_empty() && !new_lines.contains(t) {
            out.push_str("- ");
            out.push_str(line);
            out.push('\n');
        }
    }
    if out.is_empty() {
        "(no visible change)".to_string()
    } else {
        out
    }
}

#[derive(Default)]
struct State {
    browser: Option<Browser>,
    pages: HashMap<String, PageEntry>,
    next: u64,
}

#[derive(Clone)]
struct BrowserServer {
    state: Arc<Mutex<State>>,
    tool_router: ToolRouter<Self>,
}

// ---- tool parameter schemas ----

#[derive(Debug, Deserialize, JsonSchema)]
struct NavigateArgs {
    /// URL to open.
    url: String,
    /// Existing page id to navigate. Omit to open a new tab.
    #[serde(default)]
    page: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct NewPageArgs {
    /// URL to open in the new tab (default about:blank).
    #[serde(default)]
    url: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FindArgs {
    page: String,
    /// Text (or regex) to search for in the page's visible text.
    query: String,
    #[serde(default)]
    regex: bool,
    #[serde(default)]
    ignore_case: bool,
    /// Max matches to return (default 10).
    #[serde(default)]
    max: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PageArg {
    /// Page id returned by browser_navigate (e.g. "p1").
    page: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RefArgs {
    /// Page id (e.g. "p1").
    page: String,
    /// Element ref from the latest snapshot (e.g. "e3"). Provide this OR selector.
    #[serde(default, rename = "ref")]
    ref_: Option<String>,
    /// CSS selector for the element. Provide this OR ref.
    #[serde(default)]
    selector: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TypeArgs {
    page: String,
    #[serde(default, rename = "ref")]
    ref_: Option<String>,
    #[serde(default)]
    selector: Option<String>,
    /// Text to type into the focused element.
    text: String,
    /// Replace existing content instead of appending.
    #[serde(default)]
    clear: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PressArgs {
    page: String,
    /// Key name: Enter, Tab, Escape, Backspace, ArrowUp, ArrowDown, or a character.
    key: String,
    /// Optionally focus this ref/selector before pressing.
    #[serde(default, rename = "ref")]
    ref_: Option<String>,
    #[serde(default)]
    selector: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EvalArgs {
    page: String,
    /// JavaScript expression evaluated in page context.
    expression: String,
    /// Run in the page's main world (can read page-set `window` globals, but the
    /// execution is observable/detectable). Default false = isolated world.
    #[serde(default)]
    main_world: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SelectArgs {
    page: String,
    #[serde(default, rename = "ref")]
    ref_: Option<String>,
    #[serde(default)]
    selector: Option<String>,
    /// The option value to select.
    value: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WaitArgs {
    page: String,
    /// Wait until this text appears anywhere on the page.
    #[serde(default)]
    text: Option<String>,
    /// Wait until this CSS selector matches.
    #[serde(default)]
    selector: Option<String>,
    /// Timeout in milliseconds (default 10000).
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct NetArgs {
    page: String,
    /// Only include requests whose URL contains this substring.
    #[serde(default)]
    filter: Option<String>,
    /// Max entries to return (default 100).
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct BlockArgs {
    page: String,
    /// URL wildcard patterns to block (e.g. "*.png", "*doubleclick*").
    patterns: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct StorageArgs {
    page: String,
    /// File path to save to / load from (JSON: cookies + localStorage).
    path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FormField {
    #[serde(rename = "ref")]
    ref_: String,
    value: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FillFormArgs {
    page: String,
    /// Fields to fill: each { ref, value }. Existing content is replaced.
    fields: Vec<FormField>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ResizeArgs {
    page: String,
    width: u32,
    height: u32,
}

/// Build the browser per environment. Default: headful, real profile, no JS
/// patching (fingerprint == a real human Chrome). Overrides:
///   AB_CONNECT=<port>  attach to a Chrome the user already launched (strongest)
///   AB_HEADLESS=1      run headless (a tell; enable AB_STEALTH to compensate)
///   AB_STEALTH=1       inject the JS stealth-patch fallback (headless only)
///   AB_PROFILE=<dir>   persistent profile location
async fn make_browser() -> ab_browser::Result<Browser> {
    if let Ok(port) = std::env::var("AB_CONNECT") {
        return Browser::connect(port.trim().parse().unwrap_or(9222)).await;
    }
    Browser::launch(LaunchOptions {
        headless: std::env::var("AB_HEADLESS").is_ok(),
        inject_stealth: std::env::var("AB_STEALTH").is_ok(),
        ..Default::default()
    })
    .await
}

fn ok(s: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(s.into())])
}

fn fail<E: std::fmt::Display>(e: E) -> McpError {
    McpError::internal_error(e.to_string(), None)
}

impl BrowserServer {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            tool_router: Self::tool_router(),
        }
    }

    /// Clone the Page for a given id (does not hold the lock across ops).
    async fn page_of(&self, id: &str) -> Result<Page, McpError> {
        let st = self.state.lock().await;
        st.pages
            .get(id)
            .map(|e| e.page.clone())
            .ok_or_else(|| fail(format!("unknown page '{id}'")))
    }

    async fn backend_of(&self, id: &str, ref_: &str) -> Result<i64, McpError> {
        let st = self.state.lock().await;
        let entry = st
            .pages
            .get(id)
            .ok_or_else(|| fail(format!("unknown page '{id}'")))?;
        entry
            .refs
            .get(ref_)
            .copied()
            .ok_or_else(|| fail(format!("unknown ref '{ref_}' (re-snapshot?)")))
    }

    /// Resolve a target to a backend node id from either a snapshot ref or a
    /// CSS selector (patchright-style: act tools accept either).
    async fn resolve(
        &self,
        page_id: &str,
        ref_: &Option<String>,
        selector: &Option<String>,
    ) -> Result<i64, McpError> {
        if let Some(r) = ref_ {
            return self.backend_of(page_id, r).await;
        }
        if let Some(sel) = selector {
            let page = self.page_of(page_id).await?;
            return page
                .backend_for_selector(sel)
                .await
                .map_err(fail)?
                .ok_or_else(|| fail(format!("no element matches selector {sel:?}")));
        }
        Err(fail("provide `ref` or `selector`"))
    }

    /// Persist a fresh snapshot (refs + text) for a page.
    async fn store_snapshot(&self, id: &str, refs: HashMap<String, i64>, text: String) {
        let mut st = self.state.lock().await;
        if let Some(e) = st.pages.get_mut(id) {
            e.refs = refs;
            e.last_text = text;
        }
    }

    async fn last_text(&self, id: &str) -> String {
        let st = self.state.lock().await;
        st.pages.get(id).map(|e| e.last_text.clone()).unwrap_or_default()
    }

    async fn netlog_of(&self, id: &str) -> Option<NetworkLog> {
        let st = self.state.lock().await;
        st.pages.get(id).and_then(|e| e.netlog.clone())
    }

    /// Open a fresh tab (launching the browser if needed), navigate it, and
    /// register it. Returns (page_id, snapshot_text).
    async fn open_page(&self, url: &str) -> Result<(String, String), McpError> {
        let mut st = self.state.lock().await;
        if st.browser.is_none() {
            st.browser = Some(make_browser().await.map_err(fail)?);
        }
        // Blank page first so the network log captures the navigation itself.
        let page = st
            .browser
            .as_ref()
            .unwrap()
            .new_page("about:blank")
            .await
            .map_err(fail)?;
        let netlog = page.enable_network_log().await.ok();
        let _ = page.enable_dialog_auto_accept().await;
        if !url.is_empty() && url != "about:blank" {
            page.navigate(url).await.map_err(fail)?;
        }
        let snap = page.snapshot().await.map_err(fail)?;
        st.next += 1;
        let id = format!("p{}", st.next);
        st.pages.insert(
            id.clone(),
            PageEntry {
                page,
                refs: snap.refs.clone(),
                last_text: snap.text.clone(),
                netlog,
            },
        );
        Ok((id, snap.text))
    }

    /// After an action: wait for settle, re-snapshot, diff vs the previous
    /// snapshot, persist the new one, and return the diff for the agent.
    async fn settle_diff(&self, id: &str, page: &Page) -> Result<String, McpError> {
        let before = self.last_text(id).await;
        page.settle().await;
        let snap = page.snapshot().await.map_err(fail)?;
        let diff = snapshot_diff(&before, &snap.text);
        self.store_snapshot(id, snap.refs, snap.text).await;
        Ok(diff)
    }
}

#[tool_router(router = tool_router)]
impl BrowserServer {
    /// Navigate: reuse an existing page (if `page` given) or open a new tab.
    #[tool(description = "Navigate a page to a URL (reuses `page` if given, else opens a new tab)")]
    async fn browser_navigate(
        &self,
        Parameters(a): Parameters<NavigateArgs>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(pid) = &a.page {
            let page = self.page_of(pid).await?;
            page.navigate(&a.url).await.map_err(fail)?;
            let snap = page.snapshot().await.map_err(fail)?;
            self.store_snapshot(pid, snap.refs.clone(), snap.text.clone()).await;
            return Ok(ok(format!("page {pid}\nurl {}\n\n{}", a.url, snap.text)));
        }
        let (id, text) = self.open_page(&a.url).await?;
        Ok(ok(format!("page {id}\nurl {}\n\n{}", a.url, text)))
    }

    /// Open a new tab (optionally at a URL); returns its page id + snapshot.
    #[tool(description = "Open a new browser tab (optional url); returns page id + snapshot")]
    async fn browser_new_page(
        &self,
        Parameters(a): Parameters<NewPageArgs>,
    ) -> Result<CallToolResult, McpError> {
        let url = a.url.unwrap_or_default();
        let (id, text) = self.open_page(&url).await?;
        Ok(ok(format!("page {id}\n\n{text}")))
    }

    /// Re-render the accessibility snapshot for a page (refreshes [ref] handles).
    #[tool(description = "Accessibility-tree snapshot of a page, with [ref] handles")]
    async fn browser_snapshot(
        &self,
        Parameters(a): Parameters<PageArg>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let snap = page.snapshot().await.map_err(fail)?;
        self.store_snapshot(&a.page, snap.refs.clone(), snap.text.clone())
            .await;
        Ok(ok(format!("page {}\n\n{}", a.page, snap.text)))
    }

    /// Click an element by its snapshot ref, then report what changed.
    #[tool(description = "Click an element by ref (synthesized mouse click); returns settle-diff")]
    async fn browser_click(
        &self,
        Parameters(a): Parameters<RefArgs>,
    ) -> Result<CallToolResult, McpError> {
        let backend = self.resolve(&a.page, &a.ref_, &a.selector).await?;
        let page = self.page_of(&a.page).await?;
        page.click(backend).await.map_err(fail)?;
        let diff = self.settle_diff(&a.page, &page).await?;
        Ok(ok(format!("clicked on {}\n\n{}", a.page, diff)))
    }

    /// Type text into an element by ref (optionally clearing it first).
    #[tool(description = "Type text into an element by ref; returns settle-diff")]
    async fn browser_type(
        &self,
        Parameters(a): Parameters<TypeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let backend = self.resolve(&a.page, &a.ref_, &a.selector).await?;
        let page = self.page_of(&a.page).await?;
        page.type_text(backend, &a.text, a.clear)
            .await
            .map_err(fail)?;
        let diff = self.settle_diff(&a.page, &page).await?;
        Ok(ok(format!("typed into {}\n\n{}", a.page, diff)))
    }

    /// Press a named key on a page, then report what changed.
    #[tool(description = "Press a key (Enter, Tab, Escape, ...); returns settle-diff")]
    async fn browser_press(
        &self,
        Parameters(a): Parameters<PressArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        if a.ref_.is_some() || a.selector.is_some() {
            let backend = self.resolve(&a.page, &a.ref_, &a.selector).await?;
            page.focus(backend).await.map_err(fail)?;
        }
        page.press(&a.key).await.map_err(fail)?;
        let diff = self.settle_diff(&a.page, &page).await?;
        Ok(ok(format!("pressed {} on {}\n\n{}", a.key, a.page, diff)))
    }

    /// List recent network requests for a page (URL, method, status).
    #[tool(description = "List recent network requests (url, method, status)")]
    async fn browser_network_requests(
        &self,
        Parameters(a): Parameters<NetArgs>,
    ) -> Result<CallToolResult, McpError> {
        let log = self
            .netlog_of(&a.page)
            .await
            .ok_or_else(|| fail(format!("no network log for '{}'", a.page)))?;
        let entries = log.recent(a.limit.unwrap_or(100), a.filter.as_deref());
        if entries.is_empty() {
            return Ok(ok("(no requests)".to_string()));
        }
        let mut out = String::new();
        for e in &entries {
            let status = if e.failed {
                "FAIL".to_string()
            } else {
                e.status.map(|s| s.to_string()).unwrap_or_else(|| "…".into())
            };
            out.push_str(&format!(
                "{:>4} {:<6} {:<10} {}\n",
                status, e.method, e.resource_type, e.url
            ));
        }
        Ok(ok(out))
    }

    /// Block requests matching URL wildcard patterns (ads, trackers, media).
    #[tool(description = "Block requests by URL wildcard patterns (e.g. *.png, *doubleclick*)")]
    async fn browser_route_block(
        &self,
        Parameters(a): Parameters<BlockArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        page.set_blocked_urls(&a.patterns).await.map_err(fail)?;
        Ok(ok(format!(
            "blocking {} pattern(s) on {}: {}",
            a.patterns.len(),
            a.page,
            a.patterns.join(", ")
        )))
    }

    /// Save cookies + localStorage of a page to a JSON file (session capture).
    #[tool(description = "Save cookies + localStorage to a JSON file")]
    async fn browser_storage_save(
        &self,
        Parameters(a): Parameters<StorageArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let cookies = page.cookies().await.map_err(fail)?;
        let local = page.local_storage().await.unwrap_or(serde_json::json!({}));
        let blob = serde_json::json!({ "cookies": cookies, "localStorage": local });
        tokio::fs::write(&a.path, serde_json::to_vec_pretty(&blob).unwrap_or_default())
            .await
            .map_err(fail)?;
        let n = cookies.as_array().map(|c| c.len()).unwrap_or(0);
        Ok(ok(format!("saved {n} cookies + localStorage to {}", a.path)))
    }

    /// Restore cookies + localStorage from a JSON file (re-auth a session).
    #[tool(description = "Load cookies + localStorage from a JSON file")]
    async fn browser_storage_load(
        &self,
        Parameters(a): Parameters<StorageArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let raw = tokio::fs::read(&a.path).await.map_err(fail)?;
        let blob: serde_json::Value = serde_json::from_slice(&raw).map_err(fail)?;
        if let Some(cookies) = blob.get("cookies") {
            page.set_cookies(cookies).await.map_err(fail)?;
        }
        if let Some(local) = blob.get("localStorage") {
            let _ = page.set_local_storage(local).await;
        }
        Ok(ok(format!("loaded session from {} (reload the page to apply)", a.path)))
    }

    /// Hover the pointer over an element by ref.
    #[tool(description = "Hover an element by ref; returns settle-diff")]
    async fn browser_hover(
        &self,
        Parameters(a): Parameters<RefArgs>,
    ) -> Result<CallToolResult, McpError> {
        let backend = self.resolve(&a.page, &a.ref_, &a.selector).await?;
        let page = self.page_of(&a.page).await?;
        page.hover(backend).await.map_err(fail)?;
        let diff = self.settle_diff(&a.page, &page).await?;
        Ok(ok(format!("hovered on {}\n\n{}", a.page, diff)))
    }

    /// Select an <option> in a dropdown by ref + value.
    #[tool(description = "Select a dropdown option by ref and value; returns settle-diff")]
    async fn browser_select(
        &self,
        Parameters(a): Parameters<SelectArgs>,
    ) -> Result<CallToolResult, McpError> {
        let backend = self.resolve(&a.page, &a.ref_, &a.selector).await?;
        let page = self.page_of(&a.page).await?;
        page.select_option(backend, &a.value).await.map_err(fail)?;
        let diff = self.settle_diff(&a.page, &page).await?;
        Ok(ok(format!("selected {:?} on {}\n\n{}", a.value, a.page, diff)))
    }

    /// Navigate back one entry in the page's history.
    #[tool(description = "Go back one history entry; returns settle-diff")]
    async fn browser_back(
        &self,
        Parameters(a): Parameters<PageArg>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        page.go_back().await.map_err(fail)?;
        let diff = self.settle_diff(&a.page, &page).await?;
        Ok(ok(format!("went back on {}\n\n{}", a.page, diff)))
    }

    /// Wait until text appears or a selector matches (whichever is given).
    #[tool(description = "Wait for text or a CSS selector to appear")]
    async fn browser_wait(
        &self,
        Parameters(a): Parameters<WaitArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let ms = a.timeout_ms.unwrap_or(10_000);
        let (found, what) = if let Some(t) = &a.text {
            (page.wait_for_text(t, ms).await.map_err(fail)?, format!("text {t:?}"))
        } else if let Some(s) = &a.selector {
            (page.wait_for_selector(s, ms).await.map_err(fail)?, format!("selector {s:?}"))
        } else {
            return Err(fail("provide `text` or `selector`"));
        };
        Ok(ok(format!(
            "{} {} on {}",
            if found { "found" } else { "TIMEOUT waiting for" },
            what,
            a.page
        )))
    }

    /// Run one-shot JavaScript. Isolated world by default (undetectable); pass
    /// main_world=true to read page-set window globals.
    #[tool(description = "Evaluate JS (isolated world by default; main_world=true for page globals)")]
    async fn browser_evaluate(
        &self,
        Parameters(a): Parameters<EvalArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let v = if a.main_world {
            page.evaluate_main(&a.expression).await.map_err(fail)?
        } else {
            page.evaluate(&a.expression).await.map_err(fail)?
        };
        Ok(ok(serde_json::to_string(&v).unwrap_or_else(|_| "null".into())))
    }

    /// Extract the page as Markdown (headings, links, lists, code).
    #[tool(description = "Read the page as Markdown (token-efficient content extract)")]
    async fn browser_read(
        &self,
        Parameters(a): Parameters<PageArg>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let md = page.read_markdown().await.map_err(fail)?;
        Ok(ok(md))
    }

    /// Fill several fields in one call (each replaces existing content).
    #[tool(description = "Fill multiple fields at once by ref; returns settle-diff")]
    async fn browser_fill_form(
        &self,
        Parameters(a): Parameters<FillFormArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let mut done = 0;
        for f in &a.fields {
            let backend = self.backend_of(&a.page, &f.ref_).await?;
            page.type_text(backend, &f.value, true).await.map_err(fail)?;
            done += 1;
        }
        let diff = self.settle_diff(&a.page, &page).await?;
        Ok(ok(format!("filled {done} field(s) on {}\n\n{}", a.page, diff)))
    }

    /// Save the page as a PDF file; returns the path. (Headless mode only.)
    #[tool(description = "Save the page as a PDF file (headless mode only); returns the path")]
    async fn browser_pdf(
        &self,
        Parameters(a): Parameters<StorageArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let bytes = page.pdf().await.map_err(fail)?;
        tokio::fs::write(&a.path, &bytes).await.map_err(fail)?;
        Ok(ok(format!("{} ({} bytes)", a.path, bytes.len())))
    }

    /// Return the page's full serialized HTML.
    #[tool(description = "Get the page's full HTML (document.documentElement.outerHTML)")]
    async fn browser_get_html(
        &self,
        Parameters(a): Parameters<PageArg>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let mut html = page.html().await.map_err(fail)?;
        const MAX: usize = 200_000;
        if html.len() > MAX {
            html.truncate(MAX);
            html.push_str("\n… (truncated)");
        }
        Ok(ok(html))
    }

    /// Extract the page's visible text (innerText).
    #[tool(description = "Get the page's visible text (innerText)")]
    async fn browser_get_text(
        &self,
        Parameters(a): Parameters<PageArg>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let mut txt = page.text().await.map_err(fail)?;
        const MAX: usize = 100_000;
        if txt.len() > MAX {
            txt.truncate(MAX);
            txt.push_str("\n… (truncated)");
        }
        Ok(ok(txt))
    }

    /// Search a page's visible text for a query (substring or regex).
    #[tool(description = "Find text on the page (substring or regex); returns matching snippets")]
    async fn browser_find(
        &self,
        Parameters(a): Parameters<FindArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let v = page
            .find(&a.query, a.regex, a.ignore_case, a.max.unwrap_or(10))
            .await
            .map_err(fail)?;
        let matches = v.as_array().cloned().unwrap_or_default();
        if matches.is_empty() {
            return Ok(ok(format!("no matches for {:?}", a.query)));
        }
        let out: Vec<String> = matches
            .iter()
            .filter_map(|m| m.as_str().map(|s| format!("- {s}")))
            .collect();
        Ok(ok(out.join("\n")))
    }

    /// Report browser status: running, mode, open page count.
    #[tool(description = "Browser status: running, mode, open pages")]
    async fn browser_status(&self) -> Result<CallToolResult, McpError> {
        let st = self.state.lock().await;
        let running = st.browser.is_some();
        let mode = if std::env::var("AB_CONNECT").is_ok() {
            "connect"
        } else if std::env::var("AB_HEADLESS").is_ok() {
            if std::env::var("AB_STEALTH").is_ok() {
                "headless+stealth"
            } else {
                "headless"
            }
        } else {
            "headful (be-real)"
        };
        Ok(ok(format!(
            "running: {running}\nmode: {mode}\nopen pages: {}",
            st.pages.len()
        )))
    }

    /// Save a full-page PNG screenshot to a temp file; returns its path.
    #[tool(description = "Capture a full-page PNG screenshot; returns the file path")]
    async fn browser_screenshot(
        &self,
        Parameters(a): Parameters<PageArg>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let png = page.screenshot().await.map_err(fail)?;
        let path = std::env::temp_dir().join(format!("ab-{}.png", a.page));
        tokio::fs::write(&path, &png).await.map_err(fail)?;
        Ok(ok(format!("{} ({} bytes)", path.display(), png.len())))
    }

    /// List open pages.
    #[tool(description = "List open page ids")]
    async fn browser_tabs(&self) -> Result<CallToolResult, McpError> {
        let st = self.state.lock().await;
        let ids: Vec<String> = st.pages.keys().cloned().collect();
        Ok(ok(if ids.is_empty() {
            "(no open pages)".to_string()
        } else {
            ids.join(", ")
        }))
    }

    /// List open pages with their current URL and title.
    #[tool(description = "List open pages with id, title, and URL")]
    async fn browser_pages(&self) -> Result<CallToolResult, McpError> {
        let entries: Vec<(String, Page)> = {
            let st = self.state.lock().await;
            st.pages.iter().map(|(k, v)| (k.clone(), v.page.clone())).collect()
        };
        if entries.is_empty() {
            return Ok(ok("(no open pages)".to_string()));
        }
        let mut out = String::new();
        for (id, page) in entries {
            let title = page.title().await.unwrap_or_default();
            let url = page.url().await.unwrap_or_default();
            out.push_str(&format!("{id}  {title:?}  {url}\n"));
        }
        Ok(ok(out))
    }

    /// Resize a page's viewport.
    #[tool(description = "Resize the page viewport (width x height)")]
    async fn browser_resize(
        &self,
        Parameters(a): Parameters<ResizeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        page.resize(a.width, a.height).await.map_err(fail)?;
        Ok(ok(format!("resized {} to {}x{}", a.page, a.width, a.height)))
    }

    /// Close a page and forget its refs.
    #[tool(description = "Close a page by id")]
    async fn browser_close_page(
        &self,
        Parameters(a): Parameters<PageArg>,
    ) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().await;
        if st.pages.remove(&a.page).is_some() {
            Ok(ok(format!("closed {}", a.page)))
        } else {
            Err(fail(format!("unknown page '{}'", a.page)))
        }
    }

    /// Probe the page for common automation fingerprints and grade the stealth.
    #[tool(description = "Self-test: report automation fingerprints visible to the page")]
    async fn browser_fingerprint_check(
        &self,
        Parameters(a): Parameters<PageArg>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let js = r#"JSON.stringify({
            webdriver: navigator.webdriver === undefined ? 'undefined' : String(navigator.webdriver),
            plugins: navigator.plugins.length,
            languages: (navigator.languages || []).join(','),
            hasChrome: !!window.chrome,
            hasChromeRuntime: !!(window.chrome && window.chrome.runtime),
            headlessUA: /headless/i.test(navigator.userAgent),
            userAgent: navigator.userAgent
        })"#;
        let raw = page.evaluate(js).await.map_err(fail)?;
        let s = raw.as_str().unwrap_or("{}");
        let v: serde_json::Value = serde_json::from_str(s).unwrap_or(serde_json::Value::Null);

        let mut report = String::from("fingerprint check\n");
        let mut checks: Vec<(bool, String)> = Vec::new();
        let get = |k: &str| v.get(k).cloned().unwrap_or(serde_json::Value::Null);

        let wd = get("webdriver");
        checks.push((
            wd.as_str() == Some("undefined"),
            format!("navigator.webdriver = {wd}"),
        ));
        let plugins = get("plugins").as_u64().unwrap_or(0);
        checks.push((plugins > 0, format!("navigator.plugins = {plugins}")));
        let langs = get("languages");
        checks.push((
            langs.as_str().map(|x| !x.is_empty()).unwrap_or(false),
            format!("navigator.languages = {langs}"),
        ));
        checks.push((get("hasChrome").as_bool().unwrap_or(false), "window.chrome present".into()));
        let headless = get("headlessUA").as_bool().unwrap_or(false);
        checks.push((!headless, format!("headless in UA = {headless}")));

        let mut passed = 0;
        for (good, label) in &checks {
            report.push_str(if *good { "  ✓ " } else { "  ✗ " });
            report.push_str(label);
            report.push('\n');
            if *good {
                passed += 1;
            }
        }
        report.push_str(&format!("\nscore: {passed}/{} passed", checks.len()));
        Ok(ok(report))
    }
}

#[tool_handler(router = self.tool_router)]
impl rmcp::ServerHandler for BrowserServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build());
        info.server_info.name = "agent-browser".to_string();
        info.server_info.version = env!("CARGO_PKG_VERSION").to_string();
        info.instructions = Some(INSTRUCTIONS.to_string());
        info
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ab_mcp=info,ab_browser=info,ab_cdp=warn".into()),
        )
        .init();

    // AB_HTTP=<port|host:port> → serve over streamable HTTP; otherwise stdio.
    if let Ok(addr) = std::env::var("AB_HTTP") {
        return serve_http(&addr).await;
    }

    info!("agent-browser MCP server starting on stdio");
    let service = BrowserServer::new().serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// Serve over the MCP Streamable HTTP transport (endpoint: `/mcp`). Each client
/// session gets its own BrowserServer (and thus its own browser).
async fn serve_http(addr: &str) -> anyhow::Result<()> {
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    };

    let bind = if addr.contains(':') {
        addr.to_string()
    } else {
        format!("127.0.0.1:{addr}")
    };

    let service: StreamableHttpService<BrowserServer, LocalSessionManager> =
        StreamableHttpService::new(
            || Ok(BrowserServer::new()),
            Default::default(),
            StreamableHttpServerConfig::default(),
        );

    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    info!("agent-browser MCP server on http://{bind}/mcp (streamable HTTP)");
    axum::serve(listener, router).await?;
    Ok(())
}
