//! browser-rs MCP server.
//!
//! Exposes the ab-browser core as `browser_*` MCP tools over stdio. No agent,
//! no LLM — just the browser, driven by whatever MCP client connects.
//!
//! Core loop the tools encode: **snapshot -> act -> verify**.

use std::collections::HashMap;
use std::sync::Arc;

use ab_browser::{Browser, ConsoleLog, LaunchOptions, NetworkLog, Page};
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{
    CallToolResult, ClientJsonRpcMessage, ContentBlock, ServerCapabilities, ServerInfo,
    ServerJsonRpcMessage,
};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::info;

const INSTRUCTIONS: &str = r#"browser-rs — a real Chrome driven over CDP, no bundled agent.

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
    consolelog: Option<ConsoleLog>,
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

#[derive(Debug, Deserialize, JsonSchema)]
struct CookieSetArgs {
    page: String,
    name: String,
    value: String,
    /// Target URL (or provide domain). One of url/domain is required by Chrome.
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    domain: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    secure: Option<bool>,
    #[serde(default)]
    http_only: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CookieDeleteArgs {
    page: String,
    name: String,
    #[serde(default)]
    domain: Option<String>,
    #[serde(default)]
    path: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CookieGetArgs {
    page: String,
    /// Cookie name to fetch.
    name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct StorageKeyArgs {
    page: String,
    key: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct StorageSetArgs {
    page: String,
    key: String,
    value: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct OfflineArgs {
    page: String,
    /// true = simulate offline, false = back online.
    offline: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ApiRequestArgs {
    page: String,
    url: String,
    #[serde(default)]
    method: Option<String>,
    /// Request headers as a JSON object.
    #[serde(default)]
    headers: Option<serde_json::Value>,
    /// Request body (for POST/PUT).
    #[serde(default)]
    data: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct UploadArgs {
    page: String,
    #[serde(default, rename = "ref")]
    ref_: Option<String>,
    #[serde(default)]
    selector: Option<String>,
    /// Absolute file paths to set on the file input.
    paths: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct IframeClickArgs {
    page: String,
    /// CSS selector for the <iframe> element.
    frame_selector: String,
    /// CSS selector for the element inside the iframe.
    selector: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct IframeFillArgs {
    page: String,
    frame_selector: String,
    selector: String,
    value: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RunCodeArgs {
    page: String,
    /// JavaScript body. Receives `args` (your provided array) in scope.
    script: String,
    #[serde(default)]
    args: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DragArgs {
    page: String,
    #[serde(default)]
    source_ref: Option<String>,
    #[serde(default)]
    source_selector: Option<String>,
    #[serde(default)]
    target_ref: Option<String>,
    #[serde(default)]
    target_selector: Option<String>,
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
        Self::with_state(Arc::new(Mutex::new(State::default())))
    }

    /// Build a handler that shares an existing `State` (one Chrome + tabs) across
    /// sessions. In HTTP mode every MCP session (each turn's `/sse` or `/mcp`
    /// connection) is handed a clone of ONE process-wide state, so the browser
    /// stays resident between turns instead of being relaunched per connection.
    fn with_state(state: Arc<Mutex<State>>) -> Self {
        Self {
            state,
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

    // Web-storage helpers shared by the localStorage/sessionStorage tools.
    async fn storage_list(&self, page_id: &str, kind: &str) -> Result<CallToolResult, McpError> {
        let page = self.page_of(page_id).await?;
        let v = page.web_storage_list(kind).await.map_err(fail)?;
        Ok(ok(serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".into())))
    }
    async fn storage_get(&self, page_id: &str, kind: &str, key: &str) -> Result<CallToolResult, McpError> {
        let page = self.page_of(page_id).await?;
        let v = page.web_storage_get(kind, key).await.map_err(fail)?;
        Ok(ok(v.as_str().map(str::to_string).unwrap_or_else(|| "(null)".into())))
    }
    async fn storage_set(&self, page_id: &str, kind: &str, key: &str, value: &str) -> Result<CallToolResult, McpError> {
        let page = self.page_of(page_id).await?;
        page.web_storage_set(kind, key, value).await.map_err(fail)?;
        Ok(ok(format!("set {kind}[{key}]")))
    }
    async fn storage_delete(&self, page_id: &str, kind: &str, key: &str) -> Result<CallToolResult, McpError> {
        let page = self.page_of(page_id).await?;
        page.web_storage_delete(kind, key).await.map_err(fail)?;
        Ok(ok(format!("deleted {kind}[{key}]")))
    }
    async fn storage_clear(&self, page_id: &str, kind: &str) -> Result<CallToolResult, McpError> {
        let page = self.page_of(page_id).await?;
        page.web_storage_clear(kind).await.map_err(fail)?;
        Ok(ok(format!("cleared {kind}")))
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
                consolelog: None,
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

    /// Clear all URL blocking rules.
    #[tool(description = "Clear all request-blocking rules")]
    async fn browser_route_clear(
        &self,
        Parameters(a): Parameters<PageArg>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        page.clear_blocked_urls().await.map_err(fail)?;
        Ok(ok(format!("cleared blocking rules on {}", a.page)))
    }

    /// Toggle offline network emulation.
    #[tool(description = "Set the page offline/online (network emulation)")]
    async fn browser_network_state_set(
        &self,
        Parameters(a): Parameters<OfflineArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        page.set_offline(a.offline).await.map_err(fail)?;
        Ok(ok(format!(
            "{} on {}",
            if a.offline { "offline" } else { "online" },
            a.page
        )))
    }

    /// Make an HTTP request from the page context (sends the page's cookies).
    #[tool(description = "HTTP request from the page context (uses session cookies); returns status + body")]
    async fn browser_api_request(
        &self,
        Parameters(a): Parameters<ApiRequestArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let headers = a.headers.unwrap_or_else(|| serde_json::json!({}));
        let v = page
            .api_request(&a.url, a.method.as_deref().unwrap_or("GET"), &headers, a.data.as_deref())
            .await
            .map_err(fail)?;
        Ok(ok(v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())))
    }

    /// Set files on a file input (by ref or selector).
    #[tool(description = "Upload files to a file input by ref/selector")]
    async fn browser_file_upload(
        &self,
        Parameters(a): Parameters<UploadArgs>,
    ) -> Result<CallToolResult, McpError> {
        let backend = self.resolve(&a.page, &a.ref_, &a.selector).await?;
        let page = self.page_of(&a.page).await?;
        page.upload_files(backend, &a.paths).await.map_err(fail)?;
        Ok(ok(format!("set {} file(s) on {}", a.paths.len(), a.page)))
    }

    /// Drag from one element to another (by ref or selector).
    #[tool(description = "Drag from source to target (each by ref or selector); returns settle-diff")]
    async fn browser_drag(
        &self,
        Parameters(a): Parameters<DragArgs>,
    ) -> Result<CallToolResult, McpError> {
        let from = self.resolve(&a.page, &a.source_ref, &a.source_selector).await?;
        let to = self.resolve(&a.page, &a.target_ref, &a.target_selector).await?;
        let page = self.page_of(&a.page).await?;
        page.drag(from, to).await.map_err(fail)?;
        let diff = self.settle_diff(&a.page, &page).await?;
        Ok(ok(format!("dragged on {}\n\n{}", a.page, diff)))
    }

    // ---- cookies (granular) ----
    /// List cookies for a page (optionally filtered by name).
    #[tool(description = "List cookies (all, or one by name)")]
    async fn browser_cookie_list(
        &self,
        Parameters(a): Parameters<PageArg>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let c = page.cookies().await.map_err(fail)?;
        Ok(ok(serde_json::to_string_pretty(&c).unwrap_or_else(|_| "[]".into())))
    }

    /// Get a single cookie's value by name.
    #[tool(description = "Get a cookie by name")]
    async fn browser_cookie_get(
        &self,
        Parameters(a): Parameters<CookieGetArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let c = page.cookies().await.map_err(fail)?;
        let found = c
            .as_array()
            .and_then(|arr| arr.iter().find(|ck| ck.get("name").and_then(|n| n.as_str()) == Some(&a.name)));
        Ok(ok(match found {
            Some(ck) => serde_json::to_string_pretty(ck).unwrap_or_default(),
            None => format!("(no cookie named {:?})", a.name),
        }))
    }

    /// Set a cookie.
    #[tool(description = "Set a cookie (name, value, url or domain)")]
    async fn browser_cookie_set(
        &self,
        Parameters(a): Parameters<CookieSetArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let mut ck = serde_json::json!({ "name": a.name, "value": a.value });
        if let Some(u) = &a.url {
            ck["url"] = serde_json::json!(u);
        }
        if let Some(d) = &a.domain {
            ck["domain"] = serde_json::json!(d);
        }
        if let Some(p) = &a.path {
            ck["path"] = serde_json::json!(p);
        }
        if let Some(s) = a.secure {
            ck["secure"] = serde_json::json!(s);
        }
        if let Some(h) = a.http_only {
            ck["httpOnly"] = serde_json::json!(h);
        }
        page.cookie_set(&ck).await.map_err(fail)?;
        Ok(ok(format!("set cookie {}", a.name)))
    }

    /// Delete cookies by name (+ optional domain/path).
    #[tool(description = "Delete a cookie by name (optional domain/path)")]
    async fn browser_cookie_delete(
        &self,
        Parameters(a): Parameters<CookieDeleteArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        page.cookie_delete(&a.name, a.domain.as_deref(), a.path.as_deref())
            .await
            .map_err(fail)?;
        Ok(ok(format!("deleted cookie {}", a.name)))
    }

    /// Clear all cookies.
    #[tool(description = "Clear all browser cookies")]
    async fn browser_cookie_clear(
        &self,
        Parameters(a): Parameters<PageArg>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        page.cookie_clear().await.map_err(fail)?;
        Ok(ok("cleared cookies".to_string()))
    }

    // ---- localStorage / sessionStorage (granular) ----
    /// List all localStorage entries.
    #[tool(description = "List localStorage entries")]
    async fn browser_localstorage_list(&self, Parameters(a): Parameters<PageArg>) -> Result<CallToolResult, McpError> {
        self.storage_list(&a.page, "localStorage").await
    }
    /// Get a localStorage value.
    #[tool(description = "Get a localStorage item by key")]
    async fn browser_localstorage_get(&self, Parameters(a): Parameters<StorageKeyArgs>) -> Result<CallToolResult, McpError> {
        self.storage_get(&a.page, "localStorage", &a.key).await
    }
    /// Set a localStorage value.
    #[tool(description = "Set a localStorage item")]
    async fn browser_localstorage_set(&self, Parameters(a): Parameters<StorageSetArgs>) -> Result<CallToolResult, McpError> {
        self.storage_set(&a.page, "localStorage", &a.key, &a.value).await
    }
    /// Delete a localStorage key.
    #[tool(description = "Delete a localStorage item by key")]
    async fn browser_localstorage_delete(&self, Parameters(a): Parameters<StorageKeyArgs>) -> Result<CallToolResult, McpError> {
        self.storage_delete(&a.page, "localStorage", &a.key).await
    }
    /// Clear localStorage.
    #[tool(description = "Clear all localStorage")]
    async fn browser_localstorage_clear(&self, Parameters(a): Parameters<PageArg>) -> Result<CallToolResult, McpError> {
        self.storage_clear(&a.page, "localStorage").await
    }
    /// List all sessionStorage entries.
    #[tool(description = "List sessionStorage entries")]
    async fn browser_sessionstorage_list(&self, Parameters(a): Parameters<PageArg>) -> Result<CallToolResult, McpError> {
        self.storage_list(&a.page, "sessionStorage").await
    }
    /// Get a sessionStorage value.
    #[tool(description = "Get a sessionStorage item by key")]
    async fn browser_sessionstorage_get(&self, Parameters(a): Parameters<StorageKeyArgs>) -> Result<CallToolResult, McpError> {
        self.storage_get(&a.page, "sessionStorage", &a.key).await
    }
    /// Set a sessionStorage value.
    #[tool(description = "Set a sessionStorage item")]
    async fn browser_sessionstorage_set(&self, Parameters(a): Parameters<StorageSetArgs>) -> Result<CallToolResult, McpError> {
        self.storage_set(&a.page, "sessionStorage", &a.key, &a.value).await
    }
    /// Delete a sessionStorage key.
    #[tool(description = "Delete a sessionStorage item by key")]
    async fn browser_sessionstorage_delete(&self, Parameters(a): Parameters<StorageKeyArgs>) -> Result<CallToolResult, McpError> {
        self.storage_delete(&a.page, "sessionStorage", &a.key).await
    }
    /// Clear sessionStorage.
    #[tool(description = "Clear all sessionStorage")]
    async fn browser_sessionstorage_clear(&self, Parameters(a): Parameters<PageArg>) -> Result<CallToolResult, McpError> {
        self.storage_clear(&a.page, "sessionStorage").await
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

    /// Get recent console messages for a page. NOTE: enables the Runtime CDP
    /// domain on first use (a stealth tell) and captures messages from then on.
    #[tool(description = "Get console messages (enables Runtime on first use — a stealth tradeoff)")]
    async fn browser_console_messages(
        &self,
        Parameters(a): Parameters<PageArg>,
    ) -> Result<CallToolResult, McpError> {
        // Lazily enable + store the console log for this page.
        let existing = {
            let st = self.state.lock().await;
            st.pages.get(&a.page).and_then(|e| e.consolelog.clone())
        };
        let log = match existing {
            Some(l) => l,
            None => {
                let page = self.page_of(&a.page).await?;
                let l = page.enable_console_log().await.map_err(fail)?;
                let mut st = self.state.lock().await;
                if let Some(e) = st.pages.get_mut(&a.page) {
                    e.consolelog = Some(l.clone());
                }
                // Give a brief moment for buffered messages after enabling.
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                l
            }
        };
        let lines = log.recent(200);
        Ok(ok(if lines.is_empty() {
            "(no console messages captured yet — capture starts when this tool is first called)".to_string()
        } else {
            lines.join("\n")
        }))
    }

    /// Click an element inside a same-origin iframe.
    #[tool(description = "Click an element inside a same-origin iframe")]
    async fn browser_iframe_click(
        &self,
        Parameters(a): Parameters<IframeClickArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        page.iframe_click(&a.frame_selector, &a.selector).await.map_err(fail)?;
        let diff = self.settle_diff(&a.page, &page).await?;
        Ok(ok(format!("iframe-clicked on {}\n\n{}", a.page, diff)))
    }

    /// Fill an input inside a same-origin iframe.
    #[tool(description = "Fill an input inside a same-origin iframe")]
    async fn browser_iframe_fill(
        &self,
        Parameters(a): Parameters<IframeFillArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        page.iframe_fill(&a.frame_selector, &a.selector, &a.value).await.map_err(fail)?;
        Ok(ok(format!("iframe-filled on {}", a.page)))
    }

    /// Run arbitrary JavaScript with args (isolated world). Returns the result.
    #[tool(description = "Run a JS body with an args array (isolated world); returns its result")]
    async fn browser_run_code(
        &self,
        Parameters(a): Parameters<RunCodeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let args = a.args.unwrap_or_else(|| serde_json::json!([]));
        let wrapped = format!(
            "(function(args){{ {} }})({})",
            a.script,
            serde_json::to_string(&args).unwrap_or_else(|_| "[]".into())
        );
        let v = page.evaluate(&wrapped).await.map_err(fail)?;
        Ok(ok(serde_json::to_string(&v).unwrap_or_else(|_| "null".into())))
    }

    /// Switch focus to a page (returns its current snapshot).
    #[tool(description = "Switch to a page and return its snapshot")]
    async fn browser_switch_page(
        &self,
        Parameters(a): Parameters<PageArg>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let snap = page.snapshot().await.map_err(fail)?;
        self.store_snapshot(&a.page, snap.refs.clone(), snap.text.clone()).await;
        Ok(ok(format!("page {}\n\n{}", a.page, snap.text)))
    }

    /// Close the browser and drop all pages.
    #[tool(description = "Close the browser (all pages)")]
    async fn browser_close(&self) -> Result<CallToolResult, McpError> {
        let browser = {
            let mut st = self.state.lock().await;
            st.pages.clear();
            st.browser.take()
        };
        if let Some(b) = browser {
            b.close().await;
            Ok(ok("browser closed".to_string()))
        } else {
            Ok(ok("(no browser running)".to_string()))
        }
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
        info.server_info.name = "browser-rs".to_string();
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

    let cli = parse_cli();
    if cli.help {
        print!("{USAGE}");
        return Ok(());
    }

    // HTTP mode if --port (or AB_HTTP) is given; otherwise stdio.
    let port = cli
        .port
        .or_else(|| std::env::var("AB_HTTP").ok().and_then(|v| v.split(':').last()?.parse().ok()));
    if let Some(port) = port {
        return serve_http(&format!("{}:{}", cli.host, port)).await;
    }

    info!("browser-rs MCP server starting on stdio");
    let service = BrowserServer::new().serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

const USAGE: &str = "browser-rs — stealth MCP browser (stdio or HTTP)\n\
\n\
Usage:\n\
  browser-rs                          # stdio MCP transport\n\
  browser-rs --port 9321 [options]    # HTTP MCP transport at /mcp\n\
\n\
Options:\n\
  --host <host>            HTTP bind host (default 127.0.0.1)\n\
  --port <port>            Enable HTTP mode on this port\n\
  --user-data-dir <path>   Persistent browser profile directory\n\
  --headless / --headed    Run headless or headful (default headful)\n\
  --stealth                Inject the JS stealth-patch layer (for headless)\n\
  --connect <port|url>     Attach to a Chrome already running with\n\
                           --remote-debugging-port (identical fingerprint)\n\
  -h, --help               Show this help\n\
\n\
Env equivalents: AB_HTTP, AB_PROFILE, AB_HEADLESS, AB_STEALTH, AB_CONNECT, AB_CHROME.\n";

struct Cli {
    port: Option<u16>,
    host: String,
    help: bool,
}

/// Parse patchright-style CLI flags, mapping them onto the AB_* env vars that
/// `make_browser` reads. This makes browser-rs a drop-in for hosts that
/// allocate a port + profile and spawn the server (like clawgram does for
/// playwright): `browser-rs --port N --user-data-dir <dir> --headless`.
fn parse_cli() -> Cli {
    let mut c = Cli {
        port: None,
        host: "127.0.0.1".to_string(),
        help: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--port" => c.port = it.next().and_then(|v| v.parse().ok()),
            "--host" => {
                if let Some(h) = it.next() {
                    c.host = h;
                }
            }
            "--user-data-dir" | "--profile" => {
                if let Some(p) = it.next() {
                    std::env::set_var("AB_PROFILE", p);
                }
            }
            "--headless" => std::env::set_var("AB_HEADLESS", "1"),
            "--headed" => std::env::remove_var("AB_HEADLESS"),
            "--stealth" => std::env::set_var("AB_STEALTH", "1"),
            "--connect" | "--cdp-endpoint" => {
                if let Some(v) = it.next() {
                    // Accept "9222" or "http://host:9222" → keep the port.
                    let port = v.rsplit(':').next().unwrap_or(&v).trim_end_matches('/');
                    std::env::set_var("AB_CONNECT", port);
                }
            }
            "-h" | "--help" => c.help = true,
            _ => {}
        }
    }
    c
}

/// Serve over the MCP Streamable HTTP transport (endpoint: `/mcp`). Each client
/// session gets its own BrowserServer (and thus its own browser).
// --- Legacy SSE transport (`/sse` + `/message`) ------------------------------
//
// rmcp 2.2 ships only the streamable-HTTP server (`/mcp`); it has no legacy SSE
// server. But some MCP clients (e.g. the Claude Agent SDK's `type: "sse"`) still
// speak the older HTTP+SSE transport. Serving it too makes browser-rs a true
// drop-in for `mcp-patchright`, which exposes both `/sse` and `/mcp`.
//
// Protocol: client GETs `/sse` → server opens a `text/event-stream`, first emits
// an `endpoint` event pointing at `/message?sessionId=<id>`, then relays every
// server→client JSON-RPC message as a `message` event. Client POSTs its
// JSON-RPC to that endpoint. We bridge each session to rmcp's service by wiring
// a `(Sink, Stream)` pair (futures unbounded channels) into `serve()`.

type SseSessions = Arc<Mutex<HashMap<String, futures::channel::mpsc::UnboundedSender<ClientJsonRpcMessage>>>>;

#[derive(Clone)]
struct SseState {
    sessions: SseSessions,
    /// Process-wide browser state shared across all SSE sessions, so Chrome
    /// stays resident between turns (each turn opens a fresh SSE connection).
    browser: Arc<Mutex<State>>,
}

fn new_session_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{nanos:016x}{n:08x}")
}

async fn sse_get(
    axum::extract::State(state): axum::extract::State<SseState>,
) -> axum::response::sse::Sse<impl futures::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>>
{
    use axum::response::sse::{Event, KeepAlive, Sse};
    use futures::StreamExt;

    let session_id = new_session_id();
    // server → client (TX): rmcp writes here, the SSE stream drains it.
    let (to_client_tx, to_client_rx) = futures::channel::mpsc::unbounded::<ServerJsonRpcMessage>();
    // client → server (RX): POST handler pushes here, rmcp reads it.
    let (from_client_tx, from_client_rx) = futures::channel::mpsc::unbounded::<ClientJsonRpcMessage>();

    state
        .sessions
        .lock()
        .await
        .insert(session_id.clone(), from_client_tx);

    let sessions = state.sessions.clone();
    let shared = state.browser.clone();
    let sid = session_id.clone();
    tokio::spawn(async move {
        match BrowserServer::with_state(shared)
            .serve((to_client_tx, from_client_rx))
            .await
        {
            Ok(service) => {
                let _ = service.waiting().await;
            }
            Err(e) => tracing::warn!("sse session {sid} serve error: {e}"),
        }
        sessions.lock().await.remove(&sid);
        tracing::info!("sse session {sid} closed");
    });

    let endpoint = futures::stream::once(async move {
        Ok::<_, std::convert::Infallible>(
            Event::default()
                .event("endpoint")
                .data(format!("/message?sessionId={session_id}")),
        )
    });
    let messages = to_client_rx.map(|msg| {
        let data = serde_json::to_string(&msg).unwrap_or_default();
        Ok::<_, std::convert::Infallible>(Event::default().event("message").data(data))
    });

    Sse::new(endpoint.chain(messages)).keep_alive(KeepAlive::default())
}

async fn sse_post(
    axum::extract::State(state): axum::extract::State<SseState>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
    body: String,
) -> axum::http::StatusCode {
    use axum::http::StatusCode;
    let Some(session_id) = params.get("sessionId") else {
        return StatusCode::BAD_REQUEST;
    };
    let msg: ClientJsonRpcMessage = match serde_json::from_str(&body) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("sse /message bad payload: {e}");
            return StatusCode::BAD_REQUEST;
        }
    };
    let tx = state.sessions.lock().await.get(session_id).cloned();
    match tx {
        Some(tx) => {
            if tx.unbounded_send(msg).is_err() {
                return StatusCode::GONE;
            }
            StatusCode::ACCEPTED
        }
        None => StatusCode::NOT_FOUND,
    }
}

async fn serve_http(addr: &str) -> anyhow::Result<()> {
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    };

    let bind = if addr.contains(':') {
        addr.to_string()
    } else {
        format!("127.0.0.1:{addr}")
    };

    // ONE process-wide browser state shared by every session on this port, so
    // Chrome stays resident across turns (each turn = a fresh /sse or /mcp
    // connection). The Arc is held by the streamable-http factory closure AND
    // the SSE state for the whole process lifetime, so the browser is never
    // dropped between sessions — only when the server process exits.
    let shared_state: Arc<Mutex<State>> = Arc::new(Mutex::new(State::default()));

    let mcp_state = shared_state.clone();
    let service: StreamableHttpService<BrowserServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(BrowserServer::with_state(mcp_state.clone())),
            Default::default(),
            StreamableHttpServerConfig::default(),
        );

    let sse_state = SseState {
        sessions: Arc::new(Mutex::new(HashMap::new())),
        browser: shared_state,
    };

    let router = axum::Router::new()
        .route("/sse", axum::routing::get(sse_get))
        .route("/message", axum::routing::post(sse_post))
        .nest_service("/mcp", service)
        .with_state(sse_state);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    info!("browser-rs MCP server on http://{bind}/mcp (streamable HTTP) + http://{bind}/sse (legacy SSE)");
    axum::serve(listener, router).await?;
    Ok(())
}
