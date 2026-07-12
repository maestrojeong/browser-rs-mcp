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

    async fn set_refs(&self, id: &str, refs: HashMap<String, i64>) {
        let mut st = self.state.lock().await;
        if let Some(e) = st.pages.get_mut(id) {
            e.refs = refs;
        }
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
        self.set_refs(&a.page, snap.refs.clone()).await;
        Ok(ok(format!("page {}\n\n{}", a.page, snap.text)))
    }

    /// Click an element by its snapshot ref.
    #[tool(description = "Click an element by ref (synthesized mouse click)")]
    async fn browser_click(
        &self,
        Parameters(a): Parameters<RefArgs>,
    ) -> Result<CallToolResult, McpError> {
        let backend = self.backend_of(&a.page, &a.ref_).await?;
        let page = self.page_of(&a.page).await?;
        page.click(backend).await.map_err(fail)?;
        Ok(ok(format!("clicked {} on {}", a.ref_, a.page)))
    }

    /// Type text into an element by ref (optionally clearing it first).
    #[tool(description = "Type text into an element by ref")]
    async fn browser_type(
        &self,
        Parameters(a): Parameters<TypeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let backend = self.backend_of(&a.page, &a.ref_).await?;
        let page = self.page_of(&a.page).await?;
        page.type_text(backend, &a.text, a.clear)
            .await
            .map_err(fail)?;
        Ok(ok(format!("typed into {} on {}", a.ref_, a.page)))
    }

    /// Press a named key on a page.
    #[tool(description = "Press a key (Enter, Tab, Escape, ...)")]
    async fn browser_press(
        &self,
        Parameters(a): Parameters<PressArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        page.press(&a.key).await.map_err(fail)?;
        Ok(ok(format!("pressed {} on {}", a.key, a.page)))
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
