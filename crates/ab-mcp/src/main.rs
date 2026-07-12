//! agent-browser MCP server.
//!
//! Exposes the ab-browser core as `browser_*` MCP tools over stdio. No agent,
//! no LLM — just the browser, driven by whatever MCP client connects.
//!
//! Core loop the tools encode: **snapshot -> act -> verify**.

use std::collections::HashMap;
use std::sync::Arc;

use ab_browser::{Browser, LaunchOptions, Page};
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
    /// URL to open (a new tab is created).
    url: String,
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
    /// Element ref from the latest snapshot (e.g. "e3").
    #[serde(rename = "ref")]
    ref_: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TypeArgs {
    page: String,
    #[serde(rename = "ref")]
    ref_: String,
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
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EvalArgs {
    page: String,
    /// JavaScript expression evaluated in page context.
    expression: String,
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
    /// Open a URL in a new tab and return its page id plus an accessibility snapshot.
    #[tool(description = "Open a URL in a new browser tab; returns page id + snapshot")]
    async fn browser_navigate(
        &self,
        Parameters(a): Parameters<NavigateArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().await;
        if st.browser.is_none() {
            let b = Browser::launch(LaunchOptions::default())
                .await
                .map_err(fail)?;
            st.browser = Some(b);
        }
        let page = st
            .browser
            .as_ref()
            .unwrap()
            .new_page(&a.url)
            .await
            .map_err(fail)?;
        let snap = page.snapshot().await.map_err(fail)?;
        st.next += 1;
        let id = format!("p{}", st.next);
        st.pages.insert(
            id.clone(),
            PageEntry {
                page,
                refs: snap.refs.clone(),
                last_text: snap.text.clone(),
            },
        );
        Ok(ok(format!("page {id}\nurl {}\n\n{}", a.url, snap.text)))
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
        let backend = self.backend_of(&a.page, &a.ref_).await?;
        let page = self.page_of(&a.page).await?;
        page.click(backend).await.map_err(fail)?;
        let diff = self.settle_diff(&a.page, &page).await?;
        Ok(ok(format!("clicked {} on {}\n\n{}", a.ref_, a.page, diff)))
    }

    /// Type text into an element by ref (optionally clearing it first).
    #[tool(description = "Type text into an element by ref; returns settle-diff")]
    async fn browser_type(
        &self,
        Parameters(a): Parameters<TypeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let backend = self.backend_of(&a.page, &a.ref_).await?;
        let page = self.page_of(&a.page).await?;
        page.type_text(backend, &a.text, a.clear)
            .await
            .map_err(fail)?;
        let diff = self.settle_diff(&a.page, &page).await?;
        Ok(ok(format!("typed into {} on {}\n\n{}", a.ref_, a.page, diff)))
    }

    /// Press a named key on a page, then report what changed.
    #[tool(description = "Press a key (Enter, Tab, Escape, ...); returns settle-diff")]
    async fn browser_press(
        &self,
        Parameters(a): Parameters<PressArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        page.press(&a.key).await.map_err(fail)?;
        let diff = self.settle_diff(&a.page, &page).await?;
        Ok(ok(format!("pressed {} on {}\n\n{}", a.key, a.page, diff)))
    }

    /// Run one-shot JavaScript in page context (no Runtime.enable).
    #[tool(description = "Evaluate a JavaScript expression in page context")]
    async fn browser_evaluate(
        &self,
        Parameters(a): Parameters<EvalArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let v = page.evaluate(&a.expression).await.map_err(fail)?;
        Ok(ok(serde_json::to_string(&v).unwrap_or_else(|_| "null".into())))
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

    info!("agent-browser MCP server starting on stdio");
    let service = BrowserServer::new().serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
