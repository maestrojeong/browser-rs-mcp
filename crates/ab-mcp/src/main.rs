//! browser-rs MCP server.
//!
//! Exposes the ab-browser core as `browser_*` MCP tools over stdio. No agent,
//! no LLM — just the browser, driven by whatever MCP client connects.
//!
//! Core loop the tools encode: **snapshot -> act -> verify**.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock, Weak};

use ab_browser::{
    Browser, ConsoleLog, ElementRef, LaunchOptions, NetworkLog, Page, PointerAction,
    PointerLocation, PointerRequest,
};
use rmcp::handler::server::{router::tool::ToolRouter, tool::ToolCallContext, wrapper::Parameters};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientJsonRpcMessage, ContentBlock, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, ServerJsonRpcMessage,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{tool, tool_router, ErrorData as McpError, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::info;

mod http_security;
mod secret_broker;

const INSTRUCTIONS: &str = r#"browser-rs — a real Chrome driven over CDP, no bundled agent.

Loop: browser_navigate -> browser_snapshot -> act (click/type) -> re-snapshot to verify.
- snapshot renders the page as an accessibility tree; interactive nodes carry snapshot-scoped [ref] handles.
- act on them by ref with browser_click / browser_type / browser_press_key.
- browser_type waits for completion by default; use wait=false only when a later browser_cancel_typing call is needed.
- browser_activate_page explicitly foregrounds a tab and verifies visibility.
- browser_wheel sends native CDP mouse-wheel input for lazy-loaded feeds.
- refs go stale when the page changes — re-snapshot before reusing them.
- browser_evaluate runs one-shot JS in an isolated world; browser_take_screenshot saves a PNG.
Stealth: observable diagnostics are blocked by default. Start with
--allow-detectable-tools only when main-world JS or Runtime-enabled console
capture is explicitly required. All interaction tools use trusted CDP input."#;

const DETECTABLE_TOOLS: &[&str] = &["browser_console_messages"];

tokio::task_local! {
    static REQUEST_OWNER: Option<String>;
}

fn request_owner() -> Option<String> {
    REQUEST_OWNER.try_with(Clone::clone).ok().flatten()
}

fn configured_allowed_tools() -> Option<Arc<HashSet<String>>> {
    static ALLOWED: OnceLock<Option<Arc<HashSet<String>>>> = OnceLock::new();
    ALLOWED
        .get_or_init(|| {
            let configured = parse_allowed_tools(std::env::var("AB_ALLOWED_TOOLS").ok());
            std::env::remove_var("AB_ALLOWED_TOOLS");
            configured.map(Arc::new)
        })
        .clone()
}

fn parse_allowed_tools(value: Option<String>) -> Option<HashSet<String>> {
    value.map(|value| {
        value
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect()
    })
}

fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn detectable_argument_violation(request: &CallToolRequestParams) -> Option<&'static str> {
    let arguments = request.arguments.as_ref()?;
    match request.name.as_ref() {
        "browser_evaluate"
            if arguments
                .get("main_world")
                .and_then(serde_json::Value::as_bool)
                == Some(true) =>
        {
            Some("browser_evaluate main_world=true executes in the page's observable JS world")
        }
        _ => None,
    }
}

fn force_scoped_owner_argument(request: &mut CallToolRequestParams, owner: &str) {
    if matches!(
        request.name.as_ref(),
        "browser_claim_page" | "browser_release_page"
    ) {
        request
            .arguments
            .get_or_insert_with(Default::default)
            .insert(
                "owner".to_string(),
                serde_json::Value::String(owner.to_string()),
            );
    }
}

fn enforce_scoped_owner(requested_owner: &str, operation: &str) -> Result<(), McpError> {
    if let Some(scoped_owner) = request_owner() {
        if requested_owner != scoped_owner {
            return Err(fail(format!(
                "this MCP connection may only {operation} owner '{scoped_owner}'"
            )));
        }
    }
    Ok(())
}

fn release_owner_claim(
    st: &mut State,
    owner: &str,
    preserve_durable_owner: bool,
) -> Option<String> {
    let page = st.owners.remove(owner)?;
    if !preserve_durable_owner {
        st.page_owners.retain(|_, page_owner| page_owner != owner);
    }
    Some(page)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WebAuthnConfig {
    transport: String,
    user_verified: bool,
    resident_key: bool,
}

struct PageEntry {
    page: Page,
    refs: HashMap<String, ElementRef>,
    last_text: String,
    active_typing: Option<Weak<CancellationToken>>,
    netlog: Option<NetworkLog>,
    consolelog: Option<ConsoleLog>,
    webauthn_authenticator: Option<(String, WebAuthnConfig)>,
}

/// Order-insensitive line diff: what appeared / disappeared between snapshots.
/// Cheap post-action signal — trims noise so the agent sees only the delta.
fn snapshot_diff(old: &str, new: &str) -> String {
    use std::collections::HashSet;
    let normalize = |line: &str| {
        let mut value = line.trim().to_string();
        while let Some(start) = value.find(" [ref=") {
            let Some(end) = value[start..].find(']') else {
                break;
            };
            value.replace_range(start..=start + end, "");
        }
        value
    };
    let old_lines: HashSet<String> = old.lines().map(&normalize).collect();
    let new_lines: HashSet<String> = new.lines().map(&normalize).collect();
    let mut out = String::new();
    for line in new.lines() {
        let t = normalize(line);
        if !t.is_empty() && !old_lines.contains(&t) {
            out.push_str("+ ");
            out.push_str(&t);
            out.push('\n');
        }
    }
    for line in old.lines() {
        let t = normalize(line);
        if !t.is_empty() && !new_lines.contains(&t) {
            out.push_str("- ");
            out.push_str(&t);
            out.push('\n');
        }
    }
    if out.is_empty() {
        "(no visible change)".to_string()
    } else {
        out
    }
}

/// Default ceiling on caller-supplied `maxLength`/`maxBytes` when
/// `AB_MAX_OUTPUT_LIMIT` is not set. Comfortably above the tool defaults
/// (100_000 / 200_000 chars) so it never interferes with normal use; it only
/// stops a caller from requesting something like `usize::MAX`.
const DEFAULT_MAX_OUTPUT_LIMIT: usize = 5_000_000; // 5 MB

/// Absolute ceiling on caller-supplied `maxLength`/`maxBytes`, regardless of
/// what a tenant requests. Without this, a managed-mode caller could ask for
/// `usize::MAX` and force browser-rs (and the secret broker round-trip) to
/// hold and ship an unbounded amount of page content, degrading the shared
/// process for every other tenant. Hosts that need a different ceiling can
/// set `AB_MAX_OUTPUT_LIMIT` (bytes) at startup.
fn configured_max_output_limit() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        let configured = std::env::var("AB_MAX_OUTPUT_LIMIT")
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .filter(|&value| value > 0);
        std::env::remove_var("AB_MAX_OUTPUT_LIMIT");
        configured.unwrap_or(DEFAULT_MAX_OUTPUT_LIMIT)
    })
}

/// Clamp a caller-supplied output limit to the configured ceiling so no
/// request can force an unbounded allocation/response, no matter what value
/// is sent.
fn clamp_output_limit(requested: usize) -> usize {
    requested.min(configured_max_output_limit())
}

fn truncate_text(mut value: String, limit: usize) -> String {
    let limit = clamp_output_limit(limit);
    if value.len() <= limit {
        return value;
    }
    let mut boundary = limit.min(value.len());
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value.push_str("\n… (truncated)");
    value
}

#[derive(Default)]
struct State {
    browser: Option<Browser>,
    pages: HashMap<String, PageEntry>,
    /// Stable caller-selected aliases for pages. This lets an agent claim pN
    /// once and use its owner name in every later `page` argument.
    owners: HashMap<String, String>,
    /// Durable ownership for every page; one owner may have multiple tabs.
    page_owners: HashMap<String, String>,
    next: u64,
}

#[derive(Clone)]
struct BrowserServer {
    state: Arc<Mutex<State>>,
    tool_router: ToolRouter<Self>,
    default_owner: Option<String>,
    allowed_tools: Option<Arc<HashSet<String>>>,
    allow_detectable_tools: bool,
    secret_broker: Option<secret_broker::SecretBroker>,
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
    /// Page id (e.g. "p1") or owner alias from browser_claim_page.
    page: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct BoundedPageArg {
    /// Page id (e.g. "p1") or owner alias from browser_claim_page.
    page: String,
    /// Maximum output characters. Managed hosts may raise this so secrets are
    /// redacted before the caller-visible limit is applied.
    #[serde(default, rename = "maxLength")]
    max_length: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WheelArgs {
    /// Page id (e.g. "p1") or owner alias from browser_claim_page.
    page: String,
    /// Vertical wheel delta in CSS pixels. Positive scrolls down.
    delta_y: f64,
    /// Viewport x coordinate where the wheel event is dispatched.
    x: f64,
    /// Viewport y coordinate where the wheel event is dispatched.
    y: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PointerArgs {
    /// Page id (e.g. "p1") or owner alias.
    page: String,
    /// click, hover, right_click, double_click, scroll, or drag.
    action: String,
    #[serde(default, rename = "ref")]
    ref_: Option<String>,
    #[serde(default)]
    selector: Option<String>,
    #[serde(default)]
    x: Option<f64>,
    #[serde(default)]
    y: Option<f64>,
    #[serde(default)]
    destination_ref: Option<String>,
    #[serde(default)]
    destination_selector: Option<String>,
    #[serde(default)]
    to_x: Option<f64>,
    #[serde(default)]
    to_y: Option<f64>,
    #[serde(default)]
    delta_x: Option<f64>,
    #[serde(default)]
    delta_y: Option<f64>,
}

struct PointerLocationInput<'a> {
    ref_: &'a Option<String>,
    selector: &'a Option<String>,
    x: Option<f64>,
    y: Option<f64>,
    label: &'static str,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ClaimPageArgs {
    /// Stable caller identity, such as a topic or agent name.
    owner: String,
    /// Concrete page id to claim (e.g. "p1").
    page: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct OwnerArg {
    /// Owner alias previously registered with browser_claim_page.
    owner: String,
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
    /// Wait for typing to finish and return the resulting page diff. Set false
    /// only when the typing must remain cancellable by a later tool call.
    #[serde(default = "default_true")]
    wait: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PressArgs {
    page: String,
    /// Key name or modifier combo, such as Enter, Meta+c, or Control+Shift+v.
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
struct WebAuthnArgs {
    page: String,
    /// Authenticator transport: "internal" (platform passkey, default), "usb", "nfc", "ble".
    #[serde(default)]
    transport: Option<String>,
    /// Report user verification as satisfied (default true).
    #[serde(default)]
    user_verified: Option<bool>,
    /// Support resident keys / discoverable credentials (default true).
    #[serde(default)]
    resident_key: Option<bool>,
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
struct RouteMockArgs {
    page: String,
    /// URL wildcard pattern to intercept (e.g. "*/api/user", "*doubleclick*").
    pattern: String,
    /// Response body to return (default empty).
    #[serde(default)]
    body: Option<String>,
    /// HTTP status code (default 200).
    #[serde(default)]
    status: Option<i64>,
    /// Content-Type header (default "application/json").
    #[serde(default)]
    content_type: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DialogArgs {
    page: String,
    /// Accept (OK) the dialog if true, dismiss (Cancel) if false. Default true.
    #[serde(default)]
    accept: Option<bool>,
    /// Text to enter for prompt() dialogs.
    #[serde(default)]
    prompt_text: Option<String>,
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
    /// Maximum response-body characters. Managed hosts may raise this so
    /// secrets are redacted before the caller-visible limit is applied.
    #[serde(default, rename = "maxBytes")]
    max_bytes: Option<usize>,
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

// Known limitations shared by all iframe_* tools' `frame_selector`:
//
// - `>>` is a naive string split, not a CSS-aware parser. A selector
//   containing a *literal* `>>` substring inside a quoted attribute value
//   (e.g. `iframe[src*="a>>b"]`) will be mis-split into two bogus hops.
//   `>>` is vanishingly rare in real selectors/URLs, but this is a real gap,
//   not a supported escape hatch — avoid `>>` inside selector strings.
// - Cross-origin frames are normally resolved from the selected iframe DOM
//   node, including OOPIFs in a separate renderer process. `name`/`src`
//   matching against the CDP frame tree remains a compatibility fallback.
//   If that fallback is needed, redirects and duplicate attributes can make
//   it unresolvable or ambiguous; use a specific chain hop or unique id/name.

#[derive(Debug, Deserialize, JsonSchema)]
struct IframeClickArgs {
    page: String,
    /// CSS selector for the <iframe> element. For nested iframes, chain
    /// selectors with " >> " (e.g. "iframe.wrapper >> iframe.popup") to
    /// descend through each level. Same-origin and cross-origin frames are
    /// both supported and require no special handling from the caller. See
    /// the `frame_selector` known-limitations note above this struct.
    frame_selector: String,
    /// CSS selector for the element inside the innermost iframe. Click and
    /// hover resolution pierce open and closed shadow roots.
    selector: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct IframeTypeArgs {
    page: String,
    /// CSS selector for the <iframe> element, or a " >> "-separated chain
    /// for nested iframes. Same-origin and cross-origin frames are supported.
    frame_selector: String,
    /// CSS selector for the input inside the innermost iframe.
    selector: String,
    /// Text to type through trusted CDP keyboard input.
    text: String,
    /// Replace existing content instead of appending.
    #[serde(default)]
    clear: bool,
    /// Wait for typing to finish and return the resulting page diff. Set false
    /// only when the typing must remain cancellable by browser_cancel_typing.
    #[serde(default = "default_true")]
    wait: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct IframeReadArgs {
    page: String,
    /// CSS selector for the <iframe> element, or a " >> "-separated chain
    /// for nested iframes. Same-origin and cross-origin frames are both
    /// supported and require no special handling from the caller. See the
    /// `frame_selector` known-limitations note above this struct.
    frame_selector: String,
    /// CSS selector for the element to read inside the innermost iframe.
    /// Defaults to "html" (`document.querySelector("html")`) if omitted,
    /// which reads the whole frame document — but only for HTML documents;
    /// an XML/SVG-served frame has no <html> element, so pass an explicit
    /// selector (or ":root") for those.
    #[serde(default)]
    selector: Option<String>,
    /// "html" for outerHTML, "text" for rendered innerText. Defaults to
    /// "text".
    #[serde(default)]
    mode: Option<String>,
    /// Maximum output characters. Managed hosts may raise this so secrets
    /// are redacted before the caller-visible limit is applied.
    #[serde(default, rename = "maxLength")]
    max_length: Option<usize>,
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

/// Build the browser per environment. Default: headful, real profile, and the
/// self-guarding JS stealth layer. Overrides:
///   AB_CONNECT=<port>  attach to a Chrome the user already launched (strongest)
///   AB_HEADLESS=1      run headless (a strong fingerprint tell)
///   AB_NO_STEALTH=1    disable stealth injection for launched browsers
///   AB_PROFILE=<dir>   persistent profile location
async fn make_browser() -> ab_browser::Result<Browser> {
    if let Ok(port) = std::env::var("AB_CONNECT") {
        return Browser::connect(port.trim().parse().unwrap_or(9222)).await;
    }
    Browser::launch(LaunchOptions {
        headless: std::env::var("AB_HEADLESS").is_ok(),
        inject_stealth: std::env::var("AB_NO_STEALTH").is_err(),
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

fn validate_wheel_input(delta_y: f64, x: f64, y: f64) -> Result<(), &'static str> {
    if !delta_y.is_finite() || !x.is_finite() || !y.is_finite() {
        return Err("delta_y, x, and y must be finite numbers");
    }
    if x < 0.0 || y < 0.0 {
        return Err("x and y must be non-negative viewport coordinates");
    }
    Ok(())
}

fn webdriver_value_is_human(value: &serde_json::Value) -> bool {
    matches!(value.as_str(), Some("undefined" | "false"))
}

impl BrowserServer {
    #[cfg(test)]
    fn new() -> Self {
        Self::with_state_and_broker(Arc::new(Mutex::new(State::default())), None, None)
    }

    fn with_state_and_broker(
        state: Arc<Mutex<State>>,
        default_owner: Option<String>,
        secret_broker: Option<secret_broker::SecretBroker>,
    ) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
            default_owner,
            allowed_tools: configured_allowed_tools(),
            allow_detectable_tools: env_flag_enabled("AB_ALLOW_DETECTABLE_TOOLS"),
            secret_broker,
        }
    }

    fn tool_is_allowed(&self, name: &str) -> bool {
        name.starts_with("browser_")
            && (self.allow_detectable_tools || !DETECTABLE_TOOLS.contains(&name))
            && self
                .allowed_tools
                .as_ref()
                .is_none_or(|allowed| allowed.contains(name))
    }

    fn enforce_detectable_argument_policy(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<(), McpError> {
        if self.allow_detectable_tools {
            return Ok(());
        }
        if let Some(reason) = detectable_argument_violation(request) {
            return Err(McpError::invalid_request(
                format!(
                    "detectable browser path blocked by strict mode: {reason}; restart with \
                     --allow-detectable-tools to opt in"
                ),
                None,
            ));
        }
        Ok(())
    }

    fn resolve_page_id(st: &State, id_or_owner: &str) -> Option<String> {
        if let Some(owner) = request_owner() {
            if id_or_owner == owner {
                return st.owners.get(&owner).cloned();
            }
            if st.page_owners.get(id_or_owner) == Some(&owner) {
                return Some(id_or_owner.to_string());
            }
            return None;
        }
        if st.pages.contains_key(id_or_owner) {
            Some(id_or_owner.to_string())
        } else {
            st.owners.get(id_or_owner).cloned()
        }
    }

    /// Resolve either a concrete page id or a claimed owner alias.
    async fn canonical_page_id(&self, id_or_owner: &str) -> Result<String, McpError> {
        let st = self.state.lock().await;
        Self::resolve_page_id(&st, id_or_owner)
            .ok_or_else(|| fail(format!("unknown page or owner '{id_or_owner}'")))
    }

    /// Clone the Page for a given id/owner (does not hold the lock across ops).
    async fn page_of(&self, id: &str) -> Result<Page, McpError> {
        let st = self.state.lock().await;
        let page_id = Self::resolve_page_id(&st, id)
            .ok_or_else(|| fail(format!("unknown page or owner '{id}'")))?;
        st.pages
            .get(&page_id)
            .map(|e| e.page.clone())
            .ok_or_else(|| fail(format!("unknown page '{page_id}'")))
    }

    async fn begin_typing(
        &self,
        id: &str,
    ) -> Result<(String, Page, Arc<CancellationToken>), McpError> {
        let mut st = self.state.lock().await;
        let page_id = Self::resolve_page_id(&st, id)
            .ok_or_else(|| fail(format!("unknown page or owner '{id}'")))?;
        let entry = st
            .pages
            .get_mut(&page_id)
            .ok_or_else(|| fail(format!("unknown page '{page_id}'")))?;

        if let Some(active) = entry.active_typing.as_ref().and_then(Weak::upgrade) {
            active.cancel();
        }
        let cancel = Arc::new(CancellationToken::new());
        entry.active_typing = Some(Arc::downgrade(&cancel));
        Ok((page_id, entry.page.clone(), cancel))
    }

    async fn finish_typing(&self, page_id: &str, cancel: &Arc<CancellationToken>) {
        let mut st = self.state.lock().await;
        if let Some(entry) = st.pages.get_mut(page_id) {
            let should_clear = entry
                .active_typing
                .as_ref()
                .and_then(Weak::upgrade)
                .is_none_or(|active| Arc::ptr_eq(&active, cancel));
            if should_clear {
                entry.active_typing = None;
            }
        }
    }

    async fn element_ref_of(&self, id: &str, ref_: &str) -> Result<ElementRef, McpError> {
        let st = self.state.lock().await;
        let page_id = Self::resolve_page_id(&st, id)
            .ok_or_else(|| fail(format!("unknown page or owner '{id}'")))?;
        let entry = st
            .pages
            .get(&page_id)
            .ok_or_else(|| fail(format!("unknown page '{page_id}'")))?;
        entry
            .refs
            .get(ref_)
            .cloned()
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
            let element = self.element_ref_of(page_id, r).await?;
            let page = self.page_of(page_id).await?;
            page.validate_element_ref(&element).await.map_err(fail)?;
            return Ok(element.backend_node_id);
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

    async fn pointer_location(
        &self,
        page_id: &str,
        page: &Page,
        input: PointerLocationInput<'_>,
    ) -> Result<Option<PointerLocation>, McpError> {
        let coordinate = match (input.x, input.y) {
            (None, None) => None,
            (Some(x), Some(y)) if x.is_finite() && y.is_finite() && x >= 0.0 && y >= 0.0 => {
                Some(PointerLocation::Coordinates { x, y })
            }
            (Some(_), Some(_)) => {
                return Err(fail(format!(
                    "{} coordinates must be finite and non-negative",
                    input.label
                )))
            }
            _ => {
                return Err(fail(format!(
                    "provide both {} x/y, or neither",
                    input.label
                )))
            }
        };
        let element = match (input.ref_, input.selector) {
            (Some(_), Some(_)) => {
                return Err(fail(format!(
                    "provide either {} ref or selector, not both",
                    input.label
                )))
            }
            (Some(reference), None) => Some(
                self.element_ref_of(page_id, reference)
                    .await
                    .map(PointerLocation::Element)?,
            ),
            (None, Some(selector)) => {
                let backend = page
                    .backend_for_selector(selector)
                    .await
                    .map_err(fail)?
                    .ok_or_else(|| fail(format!("no element matches selector {selector:?}")))?;
                Some(PointerLocation::Element(
                    page.element_ref_for_backend(backend).await.map_err(fail)?,
                ))
            }
            (None, None) => None,
        };
        match (element, coordinate) {
            (Some(_), Some(_)) => Err(fail(format!(
                "provide either {} element or coordinates, not both",
                input.label
            ))),
            (Some(element), None) => Ok(Some(element)),
            (None, Some(coordinate)) => Ok(Some(coordinate)),
            (None, None) => Ok(None),
        }
    }

    /// Persist a fresh snapshot (refs + text) for a page.
    async fn store_snapshot(&self, id: &str, refs: HashMap<String, ElementRef>, text: String) {
        let mut st = self.state.lock().await;
        let Some(page_id) = Self::resolve_page_id(&st, id) else {
            return;
        };
        if let Some(e) = st.pages.get_mut(&page_id) {
            e.refs = refs;
            e.last_text = text;
        }
    }

    async fn last_text(&self, id: &str) -> String {
        let st = self.state.lock().await;
        let Some(page_id) = Self::resolve_page_id(&st, id) else {
            return String::new();
        };
        st.pages
            .get(&page_id)
            .map(|e| e.last_text.clone())
            .unwrap_or_default()
    }

    async fn netlog_of(&self, id: &str) -> Option<NetworkLog> {
        let st = self.state.lock().await;
        let page_id = Self::resolve_page_id(&st, id)?;
        st.pages.get(&page_id).and_then(|e| e.netlog.clone())
    }

    async fn pages_text(&self) -> String {
        let _ = self.sync_external_pages().await;
        let scoped_owner = request_owner();
        let entries: Vec<(String, Page, Vec<String>)> = {
            let st = self.state.lock().await;
            st.pages
                .iter()
                .filter(|(page_id, _)| {
                    scoped_owner.as_ref().is_none_or(|owner| {
                        st.page_owners
                            .get(*page_id)
                            .is_some_and(|owned| owned == owner)
                    })
                })
                .map(|(k, v)| {
                    let owners = st.page_owners.get(k).cloned().into_iter().collect();
                    (k.clone(), v.page.clone(), owners)
                })
                .collect()
        };
        if entries.is_empty() {
            return "(no open pages)".to_string();
        }
        let mut out = String::new();
        for (id, page, owners) in entries {
            let title = page.title().await.unwrap_or_default();
            let url = page.url().await.unwrap_or_default();
            let owner = if owners.is_empty() {
                "-".to_string()
            } else {
                owners.join(",")
            };
            out.push_str(&format!("{id}  owner={owner}  {title:?}  {url}\n"));
        }
        out
    }

    /// Import tabs created by page JavaScript or target=_blank into the MCP page map.
    async fn sync_external_pages(&self) -> Result<Vec<String>, McpError> {
        let mut st = self.state.lock().await;
        let Some(browser) = st.browser.as_ref() else {
            return Ok(Vec::new());
        };
        let targets = browser.page_targets().await.map_err(fail)?;
        let known: std::collections::HashSet<String> = st
            .pages
            .values()
            .map(|entry| entry.page.target_id().to_string())
            .collect();
        let target_to_page: HashMap<String, String> = st
            .pages
            .iter()
            .map(|(page_id, entry)| (entry.page.target_id().to_string(), page_id.clone()))
            .collect();
        let new_targets: Vec<(String, String, Option<String>)> = targets
            .into_iter()
            .filter(|(target_id, url, _)| !known.contains(target_id) && url != "about:blank")
            .collect();

        let mut added = Vec::new();
        for (target_id, _, opener_id) in new_targets {
            let inherited_owner = opener_id
                .as_ref()
                .and_then(|target_id| target_to_page.get(target_id))
                .and_then(|page_id| st.page_owners.get(page_id))
                .cloned();
            let page = st
                .browser
                .as_ref()
                .unwrap()
                .attach_page(&target_id)
                .await
                .map_err(fail)?;
            let netlog = page.enable_network_log().await.ok();
            let snap = page.snapshot().await.map_err(fail)?;
            st.next += 1;
            let id = format!("p{}", st.next);
            st.pages.insert(
                id.clone(),
                PageEntry {
                    page,
                    refs: snap.refs,
                    last_text: snap.text,
                    active_typing: None,
                    netlog,
                    consolelog: None,
                    webauthn_authenticator: None,
                },
            );
            if let Some(owner) = inherited_owner {
                st.owners.insert(owner.clone(), id.clone());
                st.page_owners.insert(id.clone(), owner);
            }
            added.push(id);
        }
        Ok(added)
    }

    // Web-storage helpers shared by the localStorage/sessionStorage tools.
    async fn storage_list(&self, page_id: &str, kind: &str) -> Result<CallToolResult, McpError> {
        let page = self.page_of(page_id).await?;
        let v = page.web_storage_list(kind).await.map_err(fail)?;
        Ok(ok(
            serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".into())
        ))
    }
    async fn storage_get(
        &self,
        page_id: &str,
        kind: &str,
        key: &str,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(page_id).await?;
        let v = page.web_storage_get(kind, key).await.map_err(fail)?;
        Ok(ok(v
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| "(null)".into())))
    }
    async fn storage_set(
        &self,
        page_id: &str,
        kind: &str,
        key: &str,
        value: &str,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(page_id).await?;
        page.web_storage_set(kind, key, value).await.map_err(fail)?;
        Ok(ok(format!("set {kind}[{key}]")))
    }
    async fn storage_delete(
        &self,
        page_id: &str,
        kind: &str,
        key: &str,
    ) -> Result<CallToolResult, McpError> {
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
                active_typing: None,
                netlog,
                consolelog: None,
                webauthn_authenticator: None,
            },
        );
        if let Some(owner) = request_owner() {
            st.owners.insert(owner.clone(), id.clone());
            st.page_owners.insert(id.clone(), owner);
        }
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
        let new_pages = self.sync_external_pages().await?;
        if new_pages.is_empty() {
            Ok(diff)
        } else {
            Ok(format!("{diff}\nnew pages: {}", new_pages.join(", ")))
        }
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
            self.store_snapshot(pid, snap.refs.clone(), snap.text.clone())
                .await;
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
        Parameters(a): Parameters<BoundedPageArg>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let snap = page.snapshot().await.map_err(fail)?;
        self.store_snapshot(&a.page, snap.refs.clone(), snap.text.clone())
            .await;
        let output = format!("page {}\n\n{}", a.page, snap.text);
        Ok(ok(match a.max_length {
            Some(limit) => truncate_text(output, limit),
            None => output,
        }))
    }

    /// Activate a page target and verify foreground/visibility state.
    #[tool(description = "Bring a page tab to the foreground and verify document visibility/focus")]
    async fn browser_activate_page(
        &self,
        Parameters(a): Parameters<PageArg>,
    ) -> Result<CallToolResult, McpError> {
        let page_id = self.canonical_page_id(&a.page).await?;
        let page = self.page_of(&page_id).await?;
        let activation = page.activate().await.map_err(fail)?;
        Ok(ok(serde_json::to_string_pretty(&serde_json::json!({
            "page": page_id,
            "activated": activation.activated,
            "visibility": activation.visibility,
            "window_focused": activation.window_focused,
            "attempts": activation.attempts,
        }))
        .unwrap_or_else(|_| "{}".into())))
    }

    /// Click an element by its snapshot ref, then report what changed.
    #[tool(description = "Click an element by ref (trusted CDP mouse click); returns settle-diff")]
    async fn browser_click(
        &self,
        Parameters(a): Parameters<RefArgs>,
    ) -> Result<CallToolResult, McpError> {
        let backend = self.resolve(&a.page, &a.ref_, &a.selector).await?;
        let page = self.page_of(&a.page).await?;
        let origin = page.element_ref_for_backend(backend).await.map_err(fail)?;
        page.dispatch_pointer(&PointerRequest {
            action: PointerAction::Click,
            origin: PointerLocation::Element(origin),
            destination: None,
            delta_x: 0.0,
            delta_y: 0.0,
        })
        .await
        .map_err(fail)?;
        let diff = self.settle_diff(&a.page, &page).await?;
        Ok(ok(format!("clicked on {}\n\n{}", a.page, diff)))
    }

    /// Send a native CDP mouse-wheel event at viewport coordinates.
    #[tool(
        description = "Scroll with a real CDP mouseWheel event at viewport coordinates; returns settle-diff"
    )]
    async fn browser_wheel(
        &self,
        Parameters(a): Parameters<WheelArgs>,
    ) -> Result<CallToolResult, McpError> {
        validate_wheel_input(a.delta_y, a.x, a.y).map_err(fail)?;

        let page_id = self.canonical_page_id(&a.page).await?;
        let page = self.page_of(&page_id).await?;
        page.dispatch_pointer(&PointerRequest {
            action: PointerAction::Scroll,
            origin: PointerLocation::Coordinates { x: a.x, y: a.y },
            destination: None,
            delta_x: 0.0,
            delta_y: a.delta_y,
        })
        .await
        .map_err(fail)?;
        let diff = self.settle_diff(&page_id, &page).await?;
        Ok(ok(serde_json::to_string_pretty(&serde_json::json!({
            "page": page_id,
            "dispatched": true,
            "delta_y": a.delta_y,
            "x": a.x,
            "y": a.y,
            "diff": diff,
        }))
        .unwrap_or_else(|_| "{}".into())))
    }

    /// Extended pointer actions using trusted browser-generated input.
    #[tool(
        description = "Click, hover, right-click, double-click, scroll, or drag by ref/selector/coordinates using trusted CDP input"
    )]
    async fn browser_pointer(
        &self,
        Parameters(a): Parameters<PointerArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page_id = self.canonical_page_id(&a.page).await?;
        let page = self.page_of(&page_id).await?;
        let action = PointerAction::parse(&a.action).map_err(fail)?;
        let origin = self
            .pointer_location(
                &page_id,
                &page,
                PointerLocationInput {
                    ref_: &a.ref_,
                    selector: &a.selector,
                    x: a.x,
                    y: a.y,
                    label: "origin",
                },
            )
            .await?
            .ok_or_else(|| fail("provide origin ref, selector, or x/y"))?;
        let destination = self
            .pointer_location(
                &page_id,
                &page,
                PointerLocationInput {
                    ref_: &a.destination_ref,
                    selector: &a.destination_selector,
                    x: a.to_x,
                    y: a.to_y,
                    label: "destination",
                },
            )
            .await?;
        let request = PointerRequest {
            action,
            origin,
            destination,
            delta_x: a.delta_x.unwrap_or(0.0),
            delta_y: a.delta_y.unwrap_or(0.0),
        };
        let outcome = match page.dispatch_pointer(&request).await {
            Ok(outcome) => outcome,
            Err(error) => {
                let message = error.to_string();
                let code = if message.contains("stale ref") {
                    "browser_ref_stale"
                } else if message.contains("same live document")
                    || message.contains("drag endpoint")
                {
                    "browser_wrong_target_refused"
                } else {
                    "browser_action_unavailable"
                };
                return Ok(CallToolResult::structured_error(serde_json::json!({
                    "status": "refused",
                    "refusal": { "code": code, "message": message },
                    "retryable": false,
                })));
            }
        };
        let diff = self.settle_diff(&page_id, &page).await?;
        let mut value = serde_json::to_value(outcome).map_err(fail)?;
        if let Some(object) = value.as_object_mut() {
            object.insert("status".into(), serde_json::json!("ok"));
            object.insert("page".into(), serde_json::json!(page_id));
            object.insert("diff".into(), serde_json::json!(diff));
        }
        Ok(CallToolResult::structured(value))
    }

    /// Type text into an element by ref (optionally clearing it first).
    #[tool(
        description = "Type text into an element by ref; waits for completion and returns settle-diff by default. Set wait=false to start cancellable background typing"
    )]
    async fn browser_type(
        &self,
        Parameters(a): Parameters<TypeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let backend = self.resolve(&a.page, &a.ref_, &a.selector).await?;
        let (page_id, page, cancel) = self.begin_typing(&a.page).await?;
        if a.wait {
            let result = page
                .type_text_cancellable(backend, &a.text, a.clear, &cancel)
                .await;
            self.finish_typing(&page_id, &cancel).await;
            result.map_err(fail)?;
            let diff = self.settle_diff(&page_id, &page).await?;
            return Ok(ok(format!("typed into {page_id}\n\n{diff}")));
        }

        let server = self.clone();
        let task_page_id = page_id.clone();
        tokio::spawn(async move {
            let result = page
                .type_text_cancellable(backend, &a.text, a.clear, &cancel)
                .await;
            server.finish_typing(&task_page_id, &cancel).await;
            if let Err(e) = result {
                tracing::warn!("background typing on {task_page_id} failed: {e}");
            }
        });
        Ok(ok(format!("typing started on {page_id}")))
    }

    /// Cancel the active per-character typing request for a page.
    #[tool(
        description = "Cancel active browser_type typing on a page; already-entered text remains"
    )]
    async fn browser_cancel_typing(
        &self,
        Parameters(a): Parameters<PageArg>,
    ) -> Result<CallToolResult, McpError> {
        let page_id = self.canonical_page_id(&a.page).await?;
        let active = {
            let st = self.state.lock().await;
            st.pages
                .get(&page_id)
                .and_then(|entry| entry.active_typing.as_ref())
                .and_then(Weak::upgrade)
        };
        if let Some(cancel) = active {
            cancel.cancel();
            Ok(ok(format!("typing cancellation requested on {page_id}")))
        } else {
            Ok(ok(format!("no active typing on {page_id}")))
        }
    }

    /// Press a named key on a page, then report what changed.
    #[tool(
        description = "Press a key or modifier combo (Enter, Meta+c, Control+v, ...); returns settle-diff"
    )]
    async fn browser_press_key(
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
                e.status
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "…".into())
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

    /// Mock a network response for URLs matching a wildcard pattern.
    #[tool(
        description = "Mock the response for requests matching a URL wildcard pattern (e.g. */api/*): return a canned body/status/content-type instead of hitting the network. Repeatable for multiple patterns; cleared by browser_route_clear."
    )]
    async fn browser_route_mock(
        &self,
        Parameters(a): Parameters<RouteMockArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let status = a.status.unwrap_or(200);
        let body = a.body.unwrap_or_default();
        let content_type = a
            .content_type
            .unwrap_or_else(|| "application/json".to_string());
        page.route_mock(&a.pattern, status, &body, &content_type)
            .await
            .map_err(fail)?;
        Ok(ok(format!(
            "mocking {:?} on {} → {} ({}, {} bytes)",
            a.pattern,
            a.page,
            status,
            content_type,
            body.len()
        )))
    }

    /// Clear all URL blocking + mock rules.
    #[tool(description = "Clear all request-blocking and mock rules")]
    async fn browser_route_clear(
        &self,
        Parameters(a): Parameters<PageArg>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        page.clear_blocked_urls().await.map_err(fail)?;
        page.clear_routes().await.map_err(fail)?;
        Ok(ok(format!("cleared blocking + mock rules on {}", a.page)))
    }

    /// Set how JS dialogs (alert/confirm/prompt) are handled.
    #[tool(
        description = "Set JS dialog handling: accept (OK) or dismiss (Cancel) upcoming alert/confirm/prompt/beforeunload dialogs, with optional prompt text. Default is accept."
    )]
    async fn browser_handle_dialog(
        &self,
        Parameters(a): Parameters<DialogArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let accept = a.accept.unwrap_or(true);
        page.set_dialog_policy(accept, a.prompt_text.clone());
        let started = page.enable_dialog_handler().await.map_err(fail)?;
        Ok(ok(format!(
            "dialog handling {} on {}; upcoming dialogs will be {}{}",
            if started { "enabled" } else { "updated" },
            a.page,
            if accept { "accepted" } else { "dismissed" },
            a.prompt_text
                .map(|t| format!(" (prompt text: {t:?})"))
                .unwrap_or_default()
        )))
    }

    /// Draw a highlight box over an element (debug aid).
    #[tool(
        description = "Highlight an element by ref/selector with an overlay box (debug/inspection)"
    )]
    async fn browser_highlight(
        &self,
        Parameters(a): Parameters<RefArgs>,
    ) -> Result<CallToolResult, McpError> {
        let backend = self.resolve(&a.page, &a.ref_, &a.selector).await?;
        let page = self.page_of(&a.page).await?;
        page.highlight(backend).await.map_err(fail)?;
        Ok(ok(format!("highlighted element on {}", a.page)))
    }

    /// Remove the highlight box.
    #[tool(description = "Remove the highlight overlay box")]
    async fn browser_hide_highlight(
        &self,
        Parameters(a): Parameters<PageArg>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        page.hide_highlight().await.map_err(fail)?;
        Ok(ok(format!("highlight removed on {}", a.page)))
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
    #[tool(
        description = "HTTP request from the page context (uses session cookies); returns status + body"
    )]
    async fn browser_api_request(
        &self,
        Parameters(a): Parameters<ApiRequestArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let headers = a.headers.unwrap_or_else(|| serde_json::json!({}));
        let v = page
            .api_request(
                &a.url,
                a.method.as_deref().unwrap_or("GET"),
                &headers,
                a.data.as_deref(),
            )
            .await
            .map_err(fail)?;
        let output = v
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| v.to_string());
        Ok(ok(match a.max_bytes {
            Some(limit) => truncate_text(output, limit),
            None => output,
        }))
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
    #[tool(
        description = "Drag from source to target (each by ref or selector); returns settle-diff"
    )]
    async fn browser_drag(
        &self,
        Parameters(a): Parameters<DragArgs>,
    ) -> Result<CallToolResult, McpError> {
        let from = self
            .resolve(&a.page, &a.source_ref, &a.source_selector)
            .await?;
        let to = self
            .resolve(&a.page, &a.target_ref, &a.target_selector)
            .await?;
        let page = self.page_of(&a.page).await?;
        let origin = page.element_ref_for_backend(from).await.map_err(fail)?;
        let destination = page.element_ref_for_backend(to).await.map_err(fail)?;
        page.dispatch_pointer(&PointerRequest {
            action: PointerAction::Drag,
            origin: PointerLocation::Element(origin),
            destination: Some(PointerLocation::Element(destination)),
            delta_x: 0.0,
            delta_y: 0.0,
        })
        .await
        .map_err(fail)?;
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
        Ok(ok(
            serde_json::to_string_pretty(&c).unwrap_or_else(|_| "[]".into())
        ))
    }

    /// Get a single cookie's value by name.
    #[tool(description = "Get a cookie by name")]
    async fn browser_cookie_get(
        &self,
        Parameters(a): Parameters<CookieGetArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let c = page.cookies().await.map_err(fail)?;
        let found = c.as_array().and_then(|arr| {
            arr.iter()
                .find(|ck| ck.get("name").and_then(|n| n.as_str()) == Some(&a.name))
        });
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
    async fn browser_localstorage_list(
        &self,
        Parameters(a): Parameters<PageArg>,
    ) -> Result<CallToolResult, McpError> {
        self.storage_list(&a.page, "localStorage").await
    }
    /// Get a localStorage value.
    #[tool(description = "Get a localStorage item by key")]
    async fn browser_localstorage_get(
        &self,
        Parameters(a): Parameters<StorageKeyArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.storage_get(&a.page, "localStorage", &a.key).await
    }
    /// Set a localStorage value.
    #[tool(description = "Set a localStorage item")]
    async fn browser_localstorage_set(
        &self,
        Parameters(a): Parameters<StorageSetArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.storage_set(&a.page, "localStorage", &a.key, &a.value)
            .await
    }
    /// Delete a localStorage key.
    #[tool(description = "Delete a localStorage item by key")]
    async fn browser_localstorage_delete(
        &self,
        Parameters(a): Parameters<StorageKeyArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.storage_delete(&a.page, "localStorage", &a.key).await
    }
    /// Clear localStorage.
    #[tool(description = "Clear all localStorage")]
    async fn browser_localstorage_clear(
        &self,
        Parameters(a): Parameters<PageArg>,
    ) -> Result<CallToolResult, McpError> {
        self.storage_clear(&a.page, "localStorage").await
    }
    /// List all sessionStorage entries.
    #[tool(description = "List sessionStorage entries")]
    async fn browser_sessionstorage_list(
        &self,
        Parameters(a): Parameters<PageArg>,
    ) -> Result<CallToolResult, McpError> {
        self.storage_list(&a.page, "sessionStorage").await
    }
    /// Get a sessionStorage value.
    #[tool(description = "Get a sessionStorage item by key")]
    async fn browser_sessionstorage_get(
        &self,
        Parameters(a): Parameters<StorageKeyArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.storage_get(&a.page, "sessionStorage", &a.key).await
    }
    /// Set a sessionStorage value.
    #[tool(description = "Set a sessionStorage item")]
    async fn browser_sessionstorage_set(
        &self,
        Parameters(a): Parameters<StorageSetArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.storage_set(&a.page, "sessionStorage", &a.key, &a.value)
            .await
    }
    /// Delete a sessionStorage key.
    #[tool(description = "Delete a sessionStorage item by key")]
    async fn browser_sessionstorage_delete(
        &self,
        Parameters(a): Parameters<StorageKeyArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.storage_delete(&a.page, "sessionStorage", &a.key).await
    }
    /// Clear sessionStorage.
    #[tool(description = "Clear all sessionStorage")]
    async fn browser_sessionstorage_clear(
        &self,
        Parameters(a): Parameters<PageArg>,
    ) -> Result<CallToolResult, McpError> {
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
        tokio::fs::write(
            &a.path,
            serde_json::to_vec_pretty(&blob).unwrap_or_default(),
        )
        .await
        .map_err(fail)?;
        let n = cookies.as_array().map(|c| c.len()).unwrap_or(0);
        Ok(ok(format!(
            "saved {n} cookies + localStorage to {}",
            a.path
        )))
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
        Ok(ok(format!(
            "loaded session from {} (reload the page to apply)",
            a.path
        )))
    }

    /// Hover the pointer over an element by ref.
    #[tool(description = "Hover an element by ref; returns settle-diff")]
    async fn browser_hover(
        &self,
        Parameters(a): Parameters<RefArgs>,
    ) -> Result<CallToolResult, McpError> {
        let backend = self.resolve(&a.page, &a.ref_, &a.selector).await?;
        let page = self.page_of(&a.page).await?;
        let origin = page.element_ref_for_backend(backend).await.map_err(fail)?;
        page.dispatch_pointer(&PointerRequest {
            action: PointerAction::Hover,
            origin: PointerLocation::Element(origin),
            destination: None,
            delta_x: 0.0,
            delta_y: 0.0,
        })
        .await
        .map_err(fail)?;
        let diff = self.settle_diff(&a.page, &page).await?;
        Ok(ok(format!("hovered on {}\n\n{}", a.page, diff)))
    }

    /// Select an <option> using trusted browser-generated keyboard input.
    #[tool(
        description = "Select an enabled dropdown option by value using trusted CDP keyboard input; returns settle-diff"
    )]
    async fn browser_select_option(
        &self,
        Parameters(a): Parameters<SelectArgs>,
    ) -> Result<CallToolResult, McpError> {
        let backend = self.resolve(&a.page, &a.ref_, &a.selector).await?;
        let page = self.page_of(&a.page).await?;
        page.select_option(backend, &a.value).await.map_err(fail)?;
        let diff = self.settle_diff(&a.page, &page).await?;
        Ok(ok(format!(
            "selected {:?} on {}\n\n{}",
            a.value, a.page, diff
        )))
    }

    /// Install a virtual authenticator so WebAuthn/passkey prompts don't block.
    #[tool(
        description = "Install a CDP virtual authenticator on the page so WebAuthn/passkey prompts resolve programmatically instead of blocking on the native OS passkey dialog. With no registered credential, navigator.credentials.get() fails fast so the site falls back to another method (e.g. password). Call BEFORE a passkey challenge appears."
    )]
    async fn browser_webauthn(
        &self,
        Parameters(a): Parameters<WebAuthnArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page_id = self.canonical_page_id(&a.page).await?;
        let (page, existing) = {
            let st = self.state.lock().await;
            let entry = st
                .pages
                .get(&page_id)
                .ok_or_else(|| fail(format!("unknown page or owner '{}'", a.page)))?;
            (entry.page.clone(), entry.webauthn_authenticator.clone())
        };
        let requested = WebAuthnConfig {
            transport: a.transport.unwrap_or_else(|| "internal".into()),
            user_verified: a.user_verified.unwrap_or(true),
            resident_key: a.resident_key.unwrap_or(true),
        };
        let (id, already_installed) = if let Some((id, installed)) = existing {
            if installed != requested {
                return Err(fail(
                    format!(
                        "this page already has a WebAuthn authenticator \
                         (transport={}, user_verified={}, resident_key={}); replacing it is not supported",
                        installed.transport, installed.user_verified, installed.resident_key
                    ),
                ));
            }
            (id, true)
        } else {
            let id = page
                .webauthn_enable(
                    &requested.transport,
                    requested.user_verified,
                    requested.resident_key,
                )
                .await
                .map_err(fail)?;
            if let Some(entry) = self.state.lock().await.pages.get_mut(&page_id) {
                entry.webauthn_authenticator = Some((id.clone(), requested.clone()));
            }
            (id, false)
        };
        Ok(ok(format!(
            "virtual authenticator {} on {} (transport={}, authenticatorId={id}). Passkey prompts will no longer block; sites fall back to password when no credential matches.",
            if already_installed { "already installed" } else { "installed" },
            a.page,
            requested.transport,
        )))
    }

    /// Navigate back one entry in the page's history.
    #[tool(description = "Go back one history entry; returns settle-diff")]
    async fn browser_navigate_back(
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
    async fn browser_wait_for(
        &self,
        Parameters(a): Parameters<WaitArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let ms = a.timeout_ms.unwrap_or(10_000);
        let (found, what) = if let Some(t) = &a.text {
            (
                page.wait_for_text(t, ms).await.map_err(fail)?,
                format!("text {t:?}"),
            )
        } else if let Some(s) = &a.selector {
            (
                page.wait_for_selector(s, ms).await.map_err(fail)?,
                format!("selector {s:?}"),
            )
        } else {
            return Err(fail("provide `text` or `selector`"));
        };
        Ok(ok(format!(
            "{} {} on {}",
            if found {
                "found"
            } else {
                "TIMEOUT waiting for"
            },
            what,
            a.page
        )))
    }

    /// Run one-shot JavaScript. Isolated world by default (undetectable); pass
    /// main_world=true to read page-set window globals.
    #[tool(
        description = "Evaluate JS (isolated world by default; main_world=true for page globals)"
    )]
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
        let mut output = serde_json::to_string(&v).unwrap_or_else(|_| "null".into());
        let new_pages = self.sync_external_pages().await?;
        if !new_pages.is_empty() {
            output.push_str(&format!("\nnew pages: {}", new_pages.join(", ")));
        }
        Ok(ok(output))
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
            let element = self.element_ref_of(&a.page, &f.ref_).await?;
            page.validate_element_ref(&element).await.map_err(fail)?;
            let backend = element.backend_node_id;
            page.type_text(backend, &f.value, true)
                .await
                .map_err(fail)?;
            done += 1;
        }
        let diff = self.settle_diff(&a.page, &page).await?;
        Ok(ok(format!(
            "filled {done} field(s) on {}\n\n{}",
            a.page, diff
        )))
    }

    /// Save the page as a PDF file; returns the path. (Headless mode only.)
    #[tool(description = "Save the page as a PDF file (headless mode only); returns the path")]
    async fn browser_save_pdf(
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
    async fn browser_get_visible_html(
        &self,
        Parameters(a): Parameters<BoundedPageArg>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let html = page.html().await.map_err(fail)?;
        Ok(ok(truncate_text(html, a.max_length.unwrap_or(200_000))))
    }

    /// Extract the page's visible text (innerText).
    #[tool(description = "Get the page's visible text (innerText)")]
    async fn browser_get_visible_text(
        &self,
        Parameters(a): Parameters<BoundedPageArg>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let text = page.text().await.map_err(fail)?;
        Ok(ok(truncate_text(text, a.max_length.unwrap_or(100_000))))
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
        let open_pages = request_owner().map_or_else(
            || st.pages.len(),
            |owner| {
                st.page_owners
                    .values()
                    .filter(|page_owner| *page_owner == &owner)
                    .count()
            },
        );
        let virtual_authenticators = request_owner().map_or_else(
            || {
                st.pages
                    .values()
                    .filter(|entry| entry.webauthn_authenticator.is_some())
                    .count()
            },
            |owner| {
                st.pages
                    .iter()
                    .filter(|(page_id, entry)| {
                        entry.webauthn_authenticator.is_some()
                            && st.page_owners.get(*page_id) == Some(&owner)
                    })
                    .count()
            },
        );
        let dialog_handlers = request_owner().map_or_else(
            || {
                st.pages
                    .values()
                    .filter(|entry| entry.page.dialog_handler_enabled())
                    .count()
            },
            |owner| {
                st.pages
                    .iter()
                    .filter(|(page_id, entry)| {
                        entry.page.dialog_handler_enabled()
                            && st.page_owners.get(*page_id) == Some(&owner)
                    })
                    .count()
            },
        );
        let mode = if std::env::var("AB_CONNECT").is_ok() {
            "connect"
        } else if std::env::var("AB_HEADLESS").is_ok() {
            if std::env::var("AB_NO_STEALTH").is_ok() {
                "headless (stealth disabled)"
            } else {
                "headless+stealth"
            }
        } else if std::env::var("AB_NO_STEALTH").is_ok() {
            "headful (stealth disabled)"
        } else {
            "headful+stealth"
        };
        let detectable_diagnostics = if self.allow_detectable_tools {
            "allowed (explicit opt-in)"
        } else {
            "blocked (strict default)"
        };
        Ok(ok(format!(
            "running: {running}\nmode: {mode}\ndetectable diagnostics: {detectable_diagnostics}\nopen pages: {open_pages}\nvirtual authenticators: {virtual_authenticators}\ndialog handlers: {dialog_handlers}"
        )))
    }

    /// Get recent console messages for a page. NOTE: enables the Runtime CDP
    /// domain on first use (a stealth tell) and captures messages from then on.
    #[tool(
        description = "Get console messages (enables Runtime on first use — a stealth tradeoff)"
    )]
    async fn browser_console_messages(
        &self,
        Parameters(a): Parameters<PageArg>,
    ) -> Result<CallToolResult, McpError> {
        let page_id = self.canonical_page_id(&a.page).await?;
        // Lazily enable + store the console log for this page.
        let existing = {
            let st = self.state.lock().await;
            st.pages.get(&page_id).and_then(|e| e.consolelog.clone())
        };
        let log = match existing {
            Some(l) => l,
            None => {
                let page = self.page_of(&a.page).await?;
                let l = page.enable_console_log().await.map_err(fail)?;
                let mut st = self.state.lock().await;
                if let Some(e) = st.pages.get_mut(&page_id) {
                    e.consolelog = Some(l.clone());
                }
                // Give a brief moment for buffered messages after enabling.
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                l
            }
        };
        let lines = log.recent(200);
        Ok(ok(if lines.is_empty() {
            "(no console messages captured yet — capture starts when this tool is first called)"
                .to_string()
        } else {
            lines.join("\n")
        }))
    }

    /// Click inside an iframe with trusted browser-generated pointer input.
    #[tool(
        description = "Click an element inside a same-origin or cross-origin iframe using trusted CDP pointer input; chain nested iframes in frame_selector with ' >> '"
    )]
    async fn browser_iframe_click(
        &self,
        Parameters(a): Parameters<IframeClickArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        page.iframe_click(&a.frame_selector, &a.selector)
            .await
            .map_err(fail)?;
        let diff = self.settle_diff(&a.page, &page).await?;
        Ok(ok(format!("iframe-clicked on {}\n\n{}", a.page, diff)))
    }

    /// Hover inside an iframe with trusted browser-generated pointer input.
    #[tool(
        description = "Hover an element inside a same-origin or cross-origin iframe using trusted CDP pointer input; pierces closed shadow roots; chain nested iframes in frame_selector with ' >> '"
    )]
    async fn browser_iframe_hover(
        &self,
        Parameters(a): Parameters<IframeClickArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        page.iframe_hover(&a.frame_selector, &a.selector)
            .await
            .map_err(fail)?;
        let diff = self.settle_diff(&a.page, &page).await?;
        Ok(ok(format!("iframe-hovered on {}\n\n{}", a.page, diff)))
    }

    /// Type into an input inside an iframe using trusted CDP keyboard input.
    #[tool(
        description = "Type text into an element inside a same-origin or cross-origin iframe using trusted CDP keyboard input; chain nested iframes in frame_selector with ' >> '; waits by default and supports browser_cancel_typing when wait=false"
    )]
    async fn browser_iframe_type(
        &self,
        Parameters(a): Parameters<IframeTypeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let (page_id, page, cancel) = self.begin_typing(&a.page).await?;
        if a.wait {
            let result = page
                .iframe_type_text_cancellable(
                    &a.frame_selector,
                    &a.selector,
                    &a.text,
                    a.clear,
                    &cancel,
                )
                .await;
            self.finish_typing(&page_id, &cancel).await;
            result.map_err(fail)?;
            let diff = self.settle_diff(&page_id, &page).await?;
            return Ok(ok(format!("iframe-typed into {page_id}\n\n{diff}")));
        }

        let server = self.clone();
        let task_page_id = page_id.clone();
        tokio::spawn(async move {
            let result = page
                .iframe_type_text_cancellable(
                    &a.frame_selector,
                    &a.selector,
                    &a.text,
                    a.clear,
                    &cancel,
                )
                .await;
            server.finish_typing(&task_page_id, &cancel).await;
            if let Err(e) = result {
                tracing::warn!("background iframe typing on {task_page_id} failed: {e}");
            }
        });
        Ok(ok(format!("iframe typing started on {page_id}")))
    }

    /// Read HTML or text from inside an iframe (same-origin or cross-origin,
    /// including nested chains via " >> "). Use this to inspect content
    /// hidden behind a cross-origin iframe that `browser_get_visible_html`
    /// / `browser_get_visible_text` / `browser_snapshot` can't see into.
    #[tool(
        description = "Read outerHTML or innerText from inside an iframe. Handles same-origin and cross-origin frames automatically; chain nested iframes in frame_selector with ' >> '; selector defaults to the whole frame document"
    )]
    async fn browser_iframe_read(
        &self,
        Parameters(a): Parameters<IframeReadArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let mode = match a.mode.as_deref() {
            Some("html") => ab_browser::ReadMode::Html,
            Some("text") | None => ab_browser::ReadMode::Text,
            Some(other) => {
                return Err(fail(format!(
                    "invalid mode `{other}` (expected \"html\" or \"text\")"
                )))
            }
        };
        let selector = a.selector.as_deref().unwrap_or("html");
        let content = page
            .iframe_read(&a.frame_selector, selector, mode)
            .await
            .map_err(fail)?;
        Ok(ok(truncate_text(content, a.max_length.unwrap_or(100_000))))
    }

    /// Run arbitrary JavaScript with args (isolated world). Returns the result.
    #[tool(description = "Run a JS body with an args array (isolated world); returns its result")]
    async fn browser_run_code_unsafe(
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
        Ok(ok(
            serde_json::to_string(&v).unwrap_or_else(|_| "null".into())
        ))
    }

    /// Switch focus to a page (returns its current snapshot).
    #[tool(description = "Switch to a page and return its snapshot")]
    async fn browser_switch_page(
        &self,
        Parameters(a): Parameters<PageArg>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let snap = page.snapshot().await.map_err(fail)?;
        self.store_snapshot(&a.page, snap.refs.clone(), snap.text.clone())
            .await;
        Ok(ok(format!("page {}\n\n{}", a.page, snap.text)))
    }

    /// Close the browser and drop all pages.
    #[tool(description = "Close the browser (all pages)")]
    async fn browser_close(&self) -> Result<CallToolResult, McpError> {
        if let Some(owner) = request_owner() {
            let pages = {
                let st = self.state.lock().await;
                st.page_owners
                    .iter()
                    .filter(|(_, page_owner)| *page_owner == &owner)
                    .filter_map(|(page_id, _)| {
                        st.pages
                            .get(page_id)
                            .map(|entry| (page_id.clone(), entry.page.clone()))
                    })
                    .collect::<Vec<_>>()
            };
            let mut closed = Vec::new();
            for (page_id, page) in pages {
                if page.close().await.is_ok() {
                    closed.push(page_id);
                }
            }
            let mut st = self.state.lock().await;
            for page_id in &closed {
                st.pages.remove(page_id);
                st.page_owners.remove(page_id);
            }
            st.owners.remove(&owner);
            return Ok(ok(format!("closed {} owner page(s)", closed.len())));
        }

        let browser = {
            let mut st = self.state.lock().await;
            st.pages.clear();
            st.owners.clear();
            st.page_owners.clear();
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
    async fn browser_take_screenshot(
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
        self.sync_external_pages().await?;
        let st = self.state.lock().await;
        let ids: Vec<String> = if let Some(owner) = request_owner() {
            st.page_owners
                .iter()
                .filter(|(_, page_owner)| *page_owner == &owner)
                .map(|(page_id, _)| page_id.clone())
                .collect()
        } else {
            st.pages.keys().cloned().collect()
        };
        Ok(ok(if ids.is_empty() {
            "(no open pages)".to_string()
        } else {
            ids.join(", ")
        }))
    }

    /// List open pages with their current URL and title.
    #[tool(description = "List open pages with id, title, and URL")]
    async fn browser_pages(&self) -> Result<CallToolResult, McpError> {
        Ok(ok(self.pages_text().await))
    }

    /// Show this server's profile path and all tabs in that profile.
    #[tool(description = "Show the current browser profile path and its open tabs")]
    async fn browser_profile(&self) -> Result<CallToolResult, McpError> {
        let profile = std::env::var("AB_PROFILE").unwrap_or_else(|_| "(ephemeral)".into());
        let page_text = self.pages_text().await;
        Ok(ok(format!("profile {profile}\n{page_text}")))
    }

    /// Claim a concrete page under a stable owner alias.
    #[tool(
        description = "Claim a page for an owner; later pass the owner as `page` to any browser tool"
    )]
    async fn browser_claim_page(
        &self,
        Parameters(a): Parameters<ClaimPageArgs>,
    ) -> Result<CallToolResult, McpError> {
        let owner = a.owner.trim();
        if owner.is_empty() {
            return Err(fail("owner must not be empty"));
        }
        let mut st = self.state.lock().await;
        enforce_scoped_owner(owner, "claim")?;
        if !st.pages.contains_key(&a.page) {
            return Err(fail(format!("unknown page '{}'", a.page)));
        }
        if let Some(other) = st
            .page_owners
            .get(&a.page)
            .filter(|other| other.as_str() != owner)
        {
            return Err(fail(format!(
                "page '{}' is already claimed by owner '{}'",
                a.page, other
            )));
        }
        st.page_owners.insert(a.page.clone(), owner.to_string());
        if let Some(previous) = st.owners.insert(owner.to_string(), a.page.clone()) {
            if previous != a.page {
                return Ok(ok(format!(
                    "owner {owner} moved from {previous} to {}",
                    a.page
                )));
            }
        }
        Ok(ok(format!("owner {owner} claimed {}", a.page)))
    }

    /// Release an owner's primary alias without closing the tab. On an
    /// owner-scoped HTTP connection, durable page ownership remains in place
    /// so another owner cannot take over a guessed page id.
    #[tool(
        description = "Release the owner's primary page alias without closing the page; owner-scoped connections retain durable page ownership for isolation"
    )]
    async fn browser_release_page(
        &self,
        Parameters(a): Parameters<OwnerArg>,
    ) -> Result<CallToolResult, McpError> {
        enforce_scoped_owner(&a.owner, "release")?;
        let preserve_durable_owner = request_owner().is_some();
        let mut st = self.state.lock().await;
        match release_owner_claim(&mut st, &a.owner, preserve_durable_owner) {
            Some(page) if preserve_durable_owner => Ok(ok(format!(
                "released primary alias {} from {page}; durable page ownership retained",
                a.owner
            ))),
            Some(page) => Ok(ok(format!("released owner {} from {page}", a.owner))),
            None => Err(fail(format!("unknown owner '{}'", a.owner))),
        }
    }

    /// Resize a page's viewport.
    #[tool(description = "Resize the page viewport (width x height)")]
    async fn browser_resize(
        &self,
        Parameters(a): Parameters<ResizeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        page.resize(a.width, a.height).await.map_err(fail)?;
        Ok(ok(format!(
            "resized {} to {}x{}",
            a.page, a.width, a.height
        )))
    }

    /// Close a page and forget its refs.
    #[tool(description = "Close a page by id")]
    async fn browser_close_page(
        &self,
        Parameters(a): Parameters<PageArg>,
    ) -> Result<CallToolResult, McpError> {
        let page_id = self.canonical_page_id(&a.page).await?;
        let page = self.page_of(&page_id).await?;
        page.close().await.map_err(fail)?;
        let mut st = self.state.lock().await;
        if st.pages.remove(&page_id).is_some() {
            st.owners.retain(|_, claimed| claimed != &page_id);
            st.page_owners.remove(&page_id);
            Ok(ok(format!("closed {page_id}")))
        } else {
            Err(fail(format!("unknown page '{page_id}'")))
        }
    }

    /// Probe the page for common automation fingerprints and grade the stealth.
    #[tool(description = "Self-test: report automation fingerprints visible to the page")]
    async fn browser_fingerprint_check(
        &self,
        Parameters(a): Parameters<PageArg>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let js = r#"(async () => {
            let webglVendor = '';
            let webglRenderer = '';
            try {
                const canvas = document.createElement('canvas');
                const gl = canvas.getContext('webgl2') || canvas.getContext('webgl');
                const ext = gl && gl.getExtension('WEBGL_debug_renderer_info');
                if (gl && ext) {
                    webglVendor = String(gl.getParameter(ext.UNMASKED_VENDOR_WEBGL) || '');
                    webglRenderer = String(gl.getParameter(ext.UNMASKED_RENDERER_WEBGL) || '');
                }
            } catch (_) {}
            let notificationPermission = 'unavailable';
            let notificationQuery = 'unavailable';
            try {
                notificationPermission = Notification.permission;
                notificationQuery = (await navigator.permissions.query({ name: 'notifications' })).state;
            } catch (_) {}
            return JSON.stringify({
                webdriver: navigator.webdriver === undefined ? 'undefined' : String(navigator.webdriver),
                plugins: navigator.plugins.length,
                languages: (navigator.languages || []).join(','),
                hasChrome: !!window.chrome,
                hasChromeRuntime: !!(window.chrome && window.chrome.runtime),
                headlessUA: /headless/i.test(navigator.userAgent),
                userAgent: navigator.userAgent,
                webglVendor,
                webglRenderer,
                softwareWebgl: /swiftshader|llvmpipe|software|mesa/i.test(webglVendor + ' ' + webglRenderer),
                voices: speechSynthesis ? speechSynthesis.getVoices().length : 0,
                notificationPermission,
                notificationQuery,
                outerWidth: window.outerWidth,
                outerHeight: window.outerHeight,
                screenWidth: screen.width,
                screenHeight: screen.height,
                screenAvailWidth: screen.availWidth,
                screenAvailHeight: screen.availHeight,
                innerWidth: window.innerWidth,
                innerHeight: window.innerHeight
            });
        })()"#;
        let raw = page.evaluate(js).await.map_err(fail)?;
        let s = raw.as_str().unwrap_or("{}");
        let v: serde_json::Value = serde_json::from_str(s).unwrap_or(serde_json::Value::Null);

        let mut report = String::from("fingerprint check\n");
        let mut checks: Vec<(bool, String)> = Vec::new();
        let get = |k: &str| v.get(k).cloned().unwrap_or(serde_json::Value::Null);

        let wd = get("webdriver");
        checks.push((
            webdriver_value_is_human(&wd),
            format!("navigator.webdriver = {wd}"),
        ));
        let plugins = get("plugins").as_u64().unwrap_or(0);
        checks.push((plugins > 0, format!("navigator.plugins = {plugins}")));
        let langs = get("languages");
        checks.push((
            langs.as_str().map(|x| !x.is_empty()).unwrap_or(false),
            format!("navigator.languages = {langs}"),
        ));
        checks.push((
            get("hasChrome").as_bool().unwrap_or(false),
            "window.chrome present".into(),
        ));
        checks.push((
            get("hasChromeRuntime").as_bool().unwrap_or(false),
            "chrome.runtime present".into(),
        ));
        let headless = get("headlessUA").as_bool().unwrap_or(false);
        checks.push((!headless, format!("headless in UA = {headless}")));
        let webgl_vendor = get("webglVendor").as_str().unwrap_or("").to_string();
        let webgl_renderer = get("webglRenderer").as_str().unwrap_or("").to_string();
        let software_webgl = get("softwareWebgl").as_bool().unwrap_or(true);
        checks.push((
            !webgl_renderer.is_empty() && !software_webgl,
            format!("WebGL = {webgl_vendor} / {webgl_renderer}"),
        ));
        let voices = get("voices").as_u64().unwrap_or(0);
        checks.push((voices > 0, format!("speechSynthesis voices = {voices}")));
        let notification_permission = get("notificationPermission")
            .as_str()
            .unwrap_or("unavailable")
            .to_string();
        let notification_query = get("notificationQuery")
            .as_str()
            .unwrap_or("unavailable")
            .to_string();
        checks.push((
            notification_permission != "unavailable"
                && notification_permission == notification_query,
            format!(
                "Notification.permission/query = {notification_permission}/{notification_query}"
            ),
        ));
        let outer_width = get("outerWidth").as_u64().unwrap_or(0);
        let outer_height = get("outerHeight").as_u64().unwrap_or(0);
        checks.push((
            outer_width > 0 && outer_height > 0,
            format!("outer size = {outer_width}x{outer_height}"),
        ));
        let screen_width = get("screenWidth").as_u64().unwrap_or(0);
        let screen_height = get("screenHeight").as_u64().unwrap_or(0);
        let avail_width = get("screenAvailWidth").as_u64().unwrap_or(0);
        let avail_height = get("screenAvailHeight").as_u64().unwrap_or(0);
        let inner_width = get("innerWidth").as_u64().unwrap_or(0);
        let inner_height = get("innerHeight").as_u64().unwrap_or(0);
        let sane_screen = screen_width >= inner_width
            && screen_height >= inner_height
            && avail_width > 0
            && avail_height > 0
            && avail_width <= screen_width
            && avail_height <= screen_height;
        checks.push((
            sane_screen,
            format!(
                "screen/available/inner = {screen_width}x{screen_height} / {avail_width}x{avail_height} / {inner_width}x{inner_height}"
            ),
        ));

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
        report.push_str(
            "\nlimited probe: does not cover input-layer code/keyCode, canvas/audio, TLS/JA3, or CDP tells",
        );
        Ok(ok(report))
    }
}

impl rmcp::ServerHandler for BrowserServer {
    async fn call_tool(
        &self,
        mut request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if !self.tool_is_allowed(request.name.as_ref()) {
            return Err(McpError::invalid_request(
                format!("browser tool '{}' is not allowed", request.name),
                None,
            ));
        }
        let owner = context
            .extensions
            .get::<http::request::Parts>()
            .and_then(|parts| {
                http_security::canonical_owner(
                    parts
                        .headers
                        .get("x-browser-owner")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_string),
                    parts.uri.query().and_then(|query| {
                        url::form_urlencoded::parse(query.as_bytes())
                            .find(|(key, _)| key == "owner")
                            .map(|(_, value)| value.into_owned())
                    }),
                )
            })
            .or_else(|| self.default_owner.clone());
        if let Some(owner) = owner.as_ref() {
            force_scoped_owner_argument(&mut request, owner);
        }
        self.enforce_detectable_argument_policy(&request)?;
        let broker_context = if let Some(broker) = self.secret_broker.as_ref() {
            let input = serde_json::Value::Object(request.arguments.clone().unwrap_or_default());
            let transformed = broker
                .transform_input(request.name.as_ref(), input)
                .await
                .map_err(|_| {
                    McpError::internal_error("browser input was blocked by secure broker", None)
                })?;
            request.arguments = match transformed.value {
                serde_json::Value::Object(arguments) => Some(arguments),
                _ => {
                    return Err(McpError::internal_error(
                        "secure broker returned invalid browser arguments",
                        None,
                    ))
                }
            };
            if let Some(owner) = owner.as_ref() {
                force_scoped_owner_argument(&mut request, owner);
            }
            Some((broker.clone(), transformed.lease, transformed.boundary))
        } else {
            None
        };
        let result = REQUEST_OWNER
            .scope(owner, async {
                self.tool_router
                    .call(ToolCallContext::new(self, request, context))
                    .await
            })
            .await;
        let Some((broker, lease, boundary)) = broker_context else {
            return result;
        };
        let unredacted = match result {
            Ok(value) => value,
            Err(error) => CallToolResult::error(vec![ContentBlock::text(error.to_string())]),
        };
        let value = serde_json::to_value(unredacted).map_err(|_| {
            McpError::internal_error("browser output was blocked before secure redaction", None)
        })?;
        let secured = broker
            .redact_output(lease, boundary, value)
            .await
            .map_err(|_| {
                McpError::internal_error("browser output was blocked by secure redaction", None)
            })?;
        serde_json::from_value(secured).map_err(|_| {
            McpError::internal_error("secure broker returned invalid browser output", None)
        })
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            tools: self
                .tool_router
                .list_all()
                .into_iter()
                .filter(|tool| self.tool_is_allowed(tool.name.as_ref()))
                .collect(),
            ..Default::default()
        })
    }

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

    let cli = parse_cli()?;
    if cli.version {
        println!("browser-rs {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if cli.help {
        print!("{USAGE}");
        return Ok(());
    }

    // HTTP mode if --port (or AB_HTTP) is given; otherwise stdio.
    let env_port = std::env::var("AB_HTTP")
        .ok()
        .map(|value| parse_port(value.split(':').next_back().unwrap_or_default(), "AB_HTTP"))
        .transpose()?;
    let port = cli.port.or(env_port);
    if let Some(port) = port {
        return serve_http(&format!("{}:{}", cli.host, port)).await;
    }

    info!("browser-rs MCP server starting on stdio");
    let secret_broker = secret_broker::SecretBroker::from_env()?;
    let service = BrowserServer::with_state_and_broker(
        Arc::new(Mutex::new(State::default())),
        None,
        secret_broker,
    )
    .serve(rmcp::transport::stdio())
    .await?;
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
  --stealth                Compatibility no-op (stealth is enabled by default)\n\
  --allow-detectable-tools Allow main-world JS and Runtime-enabled console capture\n\
  --connect <port|url>     Attach to a Chrome already running with\n\
                           --remote-debugging-port (identical fingerprint)\n\
  -h, --help               Show this help\n\
  -V, --version            Show the browser-rs version\n\
\n\
Env equivalents: AB_HTTP, AB_HTTP_CAPABILITY, AB_PROFILE, AB_HEADLESS, AB_NO_STEALTH, AB_CONNECT, AB_CHROME, AB_ALLOW_DETECTABLE_TOOLS.\n";

#[cfg(test)]
mod tests {
    use super::{
        bind_address_is_loopback, constant_time_secret_eq, enforce_scoped_owner,
        force_scoped_owner_argument, parse_allowed_tools, parse_cli_from, parse_connect_port,
        release_owner_claim, snapshot_diff, truncate_text, validate_wheel_input,
        webdriver_value_is_human, BrowserServer, IframeTypeArgs, State, TypeArgs, WebAuthnConfig,
        DEFAULT_MAX_OUTPUT_LIMIT, REQUEST_OWNER,
    };
    use rmcp::model::CallToolRequestParams;

    #[test]
    fn capability_comparison_requires_an_exact_match() {
        assert!(constant_time_secret_eq("한국어-token", "한국어-token"));
        assert!(!constant_time_secret_eq("한국어-token", "other-token"));
        assert!(!constant_time_secret_eq("short", "longer"));
    }

    #[test]
    fn snapshot_diff_ignores_snapshot_scoped_ref_churn() {
        let old = "button \"Save\" [ref=e1_1]\nStaticText \"Ready\"\n";
        let new = "button \"Save\" [ref=e2_1]\nStaticText \"Ready\"\n";
        assert_eq!(snapshot_diff(old, new), "(no visible change)");
    }

    #[test]
    fn requested_output_limit_cannot_exceed_the_absolute_ceiling() {
        // A caller requesting `usize::MAX` (or anything above the ceiling)
        // must not force an unbounded allocation/response: the effective
        // limit is capped, and oversized input is still truncated.
        let oversized: String = "a".repeat(DEFAULT_MAX_OUTPUT_LIMIT + 10);
        let truncated = truncate_text(oversized, usize::MAX);
        assert!(truncated.len() <= DEFAULT_MAX_OUTPUT_LIMIT + "\n… (truncated)".len());
        assert!(truncated.ends_with("\n… (truncated)"));
    }

    #[test]
    fn text_truncation_respects_utf8_boundaries() {
        assert_eq!(truncate_text("abcdef".into(), 3), "abc\n… (truncated)");
        assert_eq!(truncate_text("한abcd".into(), 4), "한a\n… (truncated)");
        assert_eq!(truncate_text("한글".into(), usize::MAX), "한글");
    }

    #[tokio::test]
    async fn owner_mutations_are_pinned_to_the_request_scope() {
        REQUEST_OWNER
            .scope(Some("한국어-owner".to_string()), async {
                assert!(enforce_scoped_owner("한국어-owner", "release").is_ok());
                assert!(enforce_scoped_owner("other-owner", "release").is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn scoped_page_resolution_blocks_other_owners_for_activation_and_wheel() {
        let mut state = State::default();
        state.owners.insert("owner-a".into(), "p1".into());
        state.owners.insert("owner-b".into(), "p2".into());
        state.page_owners.insert("p1".into(), "owner-a".into());
        state.page_owners.insert("p2".into(), "owner-b".into());

        REQUEST_OWNER
            .scope(Some("owner-a".to_string()), async {
                assert_eq!(
                    BrowserServer::resolve_page_id(&state, "owner-a"),
                    Some("p1".into())
                );
                assert_eq!(
                    BrowserServer::resolve_page_id(&state, "p1"),
                    Some("p1".into())
                );
                assert_eq!(BrowserServer::resolve_page_id(&state, "owner-b"), None);
                assert_eq!(BrowserServer::resolve_page_id(&state, "p2"), None);
            })
            .await;
    }

    #[test]
    fn scoped_release_preserves_durable_page_ownership() {
        let mut state = State::default();
        state.owners.insert("owner-a".into(), "p1".into());
        state.page_owners.insert("p1".into(), "owner-a".into());

        assert_eq!(
            release_owner_claim(&mut state, "owner-a", true),
            Some("p1".into())
        );
        assert!(!state.owners.contains_key("owner-a"));
        assert_eq!(
            state.page_owners.get("p1").map(String::as_str),
            Some("owner-a")
        );
    }

    #[test]
    fn configured_empty_allowlist_denies_every_tool() {
        assert!(parse_allowed_tools(None).is_none());
        assert_eq!(
            parse_allowed_tools(Some(" , ".into())),
            Some(Default::default())
        );
        assert_eq!(
            parse_allowed_tools(Some("browser_pages, browser_pages, browser_close".into()))
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn strict_mode_hides_observable_console_diagnostics_only() {
        let mut server = BrowserServer::new();
        server.allow_detectable_tools = false;
        assert!(!server.tool_is_allowed("browser_console_messages"));
        assert!(server.tool_is_allowed("browser_click"));
        assert!(server.tool_is_allowed("browser_evaluate"));
        assert!(server.tool_is_allowed("browser_iframe_click"));
        assert!(server.tool_is_allowed("browser_select_option"));

        server.allow_detectable_tools = true;
        assert!(server.tool_is_allowed("browser_console_messages"));
    }

    #[test]
    fn strict_mode_rejects_detectable_options_inside_allowed_tools() {
        let mut server = BrowserServer::new();
        server.allow_detectable_tools = false;
        let request = |tool: &'static str, arguments: serde_json::Value| {
            CallToolRequestParams::new(tool).with_arguments(
                arguments
                    .as_object()
                    .expect("test arguments must be an object")
                    .clone(),
            )
        };

        assert!(server
            .enforce_detectable_argument_policy(&request(
                "browser_evaluate",
                serde_json::json!({"page":"p1", "expression":"1", "main_world":true})
            ))
            .is_err());
        assert!(server
            .enforce_detectable_argument_policy(&request(
                "browser_evaluate",
                serde_json::json!({"page":"p1", "expression":"1"})
            ))
            .is_ok());
        server.allow_detectable_tools = true;
        assert!(server
            .enforce_detectable_argument_policy(&request(
                "browser_evaluate",
                serde_json::json!({"page":"p1", "expression":"1", "main_world":true})
            ))
            .is_ok());
    }

    #[test]
    fn scoped_claim_and_release_arguments_cannot_select_another_owner() {
        for tool in ["browser_claim_page", "browser_release_page"] {
            let mut request =
                CallToolRequestParams::new(tool).with_arguments(serde_json::Map::from_iter([(
                    "owner".into(),
                    serde_json::Value::String("attacker-selected".into()),
                )]));
            force_scoped_owner_argument(&mut request, "authenticated-owner");
            assert_eq!(
                request.arguments.unwrap().get("owner"),
                Some(&serde_json::Value::String("authenticated-owner".into()))
            );
        }
    }

    #[test]
    fn cli_parser_accepts_split_and_inline_values() {
        let cli = parse_cli_from(
            ["--port=9321", "--host", "127.0.0.1"]
                .into_iter()
                .map(str::to_string),
        )
        .unwrap();
        assert_eq!(cli.port, Some(9321));
        assert_eq!(cli.host, "127.0.0.1");
    }

    #[test]
    fn cli_parser_accepts_detectable_tools_opt_in() {
        std::env::remove_var("AB_ALLOW_DETECTABLE_TOOLS");
        parse_cli_from(["--allow-detectable-tools"].into_iter().map(str::to_string)).unwrap();
        assert_eq!(
            std::env::var("AB_ALLOW_DETECTABLE_TOOLS").as_deref(),
            Ok("1")
        );
        std::env::remove_var("AB_ALLOW_DETECTABLE_TOOLS");
    }

    #[test]
    fn cli_parser_rejects_missing_invalid_and_unknown_options() {
        for args in [vec!["--port"], vec!["--port", "0"], vec!["--porrt", "9321"]] {
            assert!(parse_cli_from(args.into_iter().map(str::to_string)).is_err());
        }
    }

    #[test]
    fn connect_parser_requires_an_explicit_valid_port() {
        assert_eq!(parse_connect_port("9222").unwrap(), 9222);
        assert_eq!(parse_connect_port("http://127.0.0.1:9222").unwrap(), 9222);
        assert!(parse_connect_port("http://127.0.0.1").is_err());
    }

    #[test]
    fn loopback_detection_does_not_trust_hostname_prefixes() {
        assert!(bind_address_is_loopback("127.0.0.1:9321"));
        assert!(bind_address_is_loopback("[::1]:9321"));
        assert!(bind_address_is_loopback("localhost:9321"));
        assert!(!bind_address_is_loopback("127.example.com:9321"));
        assert!(!bind_address_is_loopback("0.0.0.0:9321"));
    }

    #[test]
    fn explicit_webauthn_config_distinguishes_incompatible_reinstall_requests() {
        let installed = WebAuthnConfig {
            transport: "internal".into(),
            user_verified: true,
            resident_key: true,
        };
        assert_eq!(installed, installed.clone());
        assert_ne!(
            installed,
            WebAuthnConfig {
                transport: "usb".into(),
                user_verified: true,
                resident_key: true,
            }
        );
    }

    #[test]
    fn foreground_pointer_wheel_and_typing_tools_are_publicly_registered() {
        let tools = BrowserServer::new().tool_router.list_all();
        let names = tools
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect::<Vec<_>>();
        assert!(names.contains(&"browser_activate_page"));
        assert!(names.contains(&"browser_pointer"));
        assert!(names.contains(&"browser_wheel"));
        assert!(names.contains(&"browser_cancel_typing"));
        assert!(names.contains(&"browser_iframe_hover"));
        assert!(names.contains(&"browser_iframe_type"));
    }

    #[test]
    fn browser_type_waits_by_default_and_background_typing_is_explicit() {
        let blocking: TypeArgs = serde_json::from_value(serde_json::json!({
            "page": "p1",
            "selector": "#message",
            "text": "hello"
        }))
        .unwrap();
        assert!(blocking.wait);

        let background: TypeArgs = serde_json::from_value(serde_json::json!({
            "page": "p1",
            "selector": "#message",
            "text": "hello",
            "wait": false
        }))
        .unwrap();
        assert!(!background.wait);

        let iframe: IframeTypeArgs = serde_json::from_value(serde_json::json!({
            "page": "p1",
            "frame_selector": "iframe#outer >> iframe#inner",
            "selector": "#phone",
            "text": "01012345678"
        }))
        .unwrap();
        assert!(iframe.wait);
    }

    #[test]
    fn wheel_input_rejects_invalid_coordinates_and_non_finite_numbers() {
        assert!(validate_wheel_input(700.0, 650.0, 500.0).is_ok());
        assert!(validate_wheel_input(-700.0, 650.0, 500.0).is_ok());
        assert!(validate_wheel_input(700.0, -1.0, 500.0).is_err());
        assert!(validate_wheel_input(700.0, 650.0, -1.0).is_err());
        assert!(validate_wheel_input(f64::INFINITY, 650.0, 500.0).is_err());
        assert!(validate_wheel_input(700.0, f64::NAN, 500.0).is_err());
    }

    #[test]
    fn webdriver_false_and_undefined_are_both_human_browser_states() {
        assert!(webdriver_value_is_human(&serde_json::json!("undefined")));
        assert!(webdriver_value_is_human(&serde_json::json!("false")));
        assert!(!webdriver_value_is_human(&serde_json::json!("true")));
    }
}

struct Cli {
    port: Option<u16>,
    host: String,
    help: bool,
    version: bool,
}

/// Parse patchright-style CLI flags, mapping them onto the AB_* env vars that
/// `make_browser` reads. This makes browser-rs a drop-in for hosts that
/// allocate a port + profile and spawn the server (like clawgram does for
/// playwright): `browser-rs --port N --user-data-dir <dir> --headless`.
fn parse_port(value: &str, source: &str) -> anyhow::Result<u16> {
    let port = value
        .parse::<u16>()
        .map_err(|_| anyhow::anyhow!("invalid {source} port: {value:?}"))?;
    if port == 0 {
        anyhow::bail!("invalid {source} port: 0");
    }
    Ok(port)
}

fn parse_connect_port(value: &str) -> anyhow::Result<u16> {
    if !value.contains("://") {
        return parse_port(value, "--connect");
    }
    let endpoint = url::Url::parse(value)
        .map_err(|error| anyhow::anyhow!("invalid --connect URL {value:?}: {error}"))?;
    endpoint
        .port()
        .ok_or_else(|| anyhow::anyhow!("--connect URL must include an explicit port: {value:?}"))
}

fn parse_cli() -> anyhow::Result<Cli> {
    parse_cli_from(std::env::args().skip(1))
}

fn option_value(
    it: &mut impl Iterator<Item = String>,
    inline_value: Option<&str>,
    flag: &str,
) -> anyhow::Result<String> {
    inline_value
        .map(str::to_string)
        .or_else(|| it.next())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{flag} requires a value"))
}

fn parse_cli_from(args: impl IntoIterator<Item = String>) -> anyhow::Result<Cli> {
    let mut c = Cli {
        port: None,
        host: "127.0.0.1".to_string(),
        help: false,
        version: false,
    };
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let (flag, inline_value) = a
            .split_once('=')
            .map_or((a.as_str(), None), |(flag, value)| (flag, Some(value)));
        match flag {
            "--port" => {
                c.port = Some(parse_port(
                    &option_value(&mut it, inline_value, flag)?,
                    "--port",
                )?)
            }
            "--host" => {
                c.host = option_value(&mut it, inline_value, flag)?;
            }
            "--user-data-dir" | "--profile" => {
                std::env::set_var("AB_PROFILE", option_value(&mut it, inline_value, flag)?);
            }
            "--headless" if inline_value.is_none() => std::env::set_var("AB_HEADLESS", "1"),
            "--headed" if inline_value.is_none() => std::env::remove_var("AB_HEADLESS"),
            "--stealth" if inline_value.is_none() => std::env::set_var("AB_STEALTH", "1"),
            "--allow-detectable-tools" if inline_value.is_none() => {
                std::env::set_var("AB_ALLOW_DETECTABLE_TOOLS", "1")
            }
            "--connect" | "--cdp-endpoint" => {
                let port = parse_connect_port(&option_value(&mut it, inline_value, flag)?)?;
                std::env::set_var("AB_CONNECT", port.to_string());
            }
            "-h" | "--help" if inline_value.is_none() => c.help = true,
            "-V" | "--version" if inline_value.is_none() => c.version = true,
            _ => anyhow::bail!("unknown option: {a}"),
        }
    }
    Ok(c)
}

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

#[derive(Clone)]
struct SseSession {
    sender: futures::channel::mpsc::UnboundedSender<ClientJsonRpcMessage>,
    message_token: String,
}

type SseSessions = Arc<Mutex<HashMap<String, SseSession>>>;

#[derive(Clone)]
struct SseState {
    sessions: SseSessions,
    /// Process-wide browser state shared across all SSE sessions, so Chrome
    /// stays resident between turns (each turn opens a fresh SSE connection).
    browser: Arc<Mutex<State>>,
    security: http_security::HttpSecurity,
    secret_broker: Option<secret_broker::SecretBroker>,
}

fn new_session_id() -> String {
    random_token()
}

fn random_token() -> String {
    use rand::{rngs::OsRng, RngCore};
    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

struct SseSessionStream {
    inner: std::pin::Pin<
        Box<
            dyn futures::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>
                + Send,
        >,
    >,
    sessions: SseSessions,
    session_id: String,
}

impl futures::Stream for SseSessionStream {
    type Item = Result<axum::response::sse::Event, std::convert::Infallible>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(context)
    }
}

impl Drop for SseSessionStream {
    fn drop(&mut self) {
        let sessions = self.sessions.clone();
        let session_id = self.session_id.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                sessions.lock().await.remove(&session_id);
            });
        }
    }
}

async fn sse_get(
    axum::extract::State(state): axum::extract::State<SseState>,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    headers: axum::http::HeaderMap,
) -> axum::response::sse::Sse<
    impl futures::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
> {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use futures::StreamExt;

    let session_id = new_session_id();
    let message_token = random_token();
    let query_owner = uri.query().and_then(|query| {
        url::form_urlencoded::parse(query.as_bytes())
            .find(|(key, _)| key == "owner")
            .map(|(_, value)| value.into_owned())
    });
    let owner = http_security::canonical_owner(
        headers
            .get("x-browser-owner")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
        query_owner,
    );
    // server → client (TX): rmcp writes here, the SSE stream drains it.
    let (to_client_tx, to_client_rx) = futures::channel::mpsc::unbounded::<ServerJsonRpcMessage>();
    // client → server (RX): POST handler pushes here, rmcp reads it.
    let (from_client_tx, from_client_rx) =
        futures::channel::mpsc::unbounded::<ClientJsonRpcMessage>();

    state.sessions.lock().await.insert(
        session_id.clone(),
        SseSession {
            sender: from_client_tx,
            message_token: message_token.clone(),
        },
    );

    let sessions = state.sessions.clone();
    let shared = state.browser.clone();
    let sid = session_id.clone();
    tokio::spawn(async move {
        match BrowserServer::with_state_and_broker(shared, owner, state.secret_broker.clone())
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

    let endpoint_session_id = session_id.clone();
    let endpoint = futures::stream::once(async move {
        Ok::<_, std::convert::Infallible>(Event::default().event("endpoint").data(format!(
            "/message?sessionId={endpoint_session_id}&token={message_token}"
        )))
    });
    let messages = to_client_rx.map(|msg| {
        let data = serde_json::to_string(&msg).unwrap_or_default();
        Ok::<_, std::convert::Infallible>(Event::default().event("message").data(data))
    });

    let stream = SseSessionStream {
        inner: Box::pin(endpoint.chain(messages)),
        sessions: state.sessions,
        session_id,
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
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
    let Some(message_token) = params.get("token") else {
        return StatusCode::UNAUTHORIZED;
    };
    let session = state.sessions.lock().await.get(session_id).cloned();
    let Some(session) = session else {
        return StatusCode::NOT_FOUND;
    };
    if !constant_time_secret_eq(message_token, &session.message_token) {
        return StatusCode::UNAUTHORIZED;
    }
    let msg: ClientJsonRpcMessage = match serde_json::from_str(&body) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("sse /message bad payload: {e}");
            return StatusCode::BAD_REQUEST;
        }
    };
    if session.sender.unbounded_send(msg).is_err() {
        state.sessions.lock().await.remove(session_id);
        return StatusCode::GONE;
    }
    StatusCode::ACCEPTED
}

async fn health(
    axum::extract::State(state): axum::extract::State<SseState>,
) -> axum::Json<serde_json::Value> {
    axum::Json(state.security.health())
}

async fn close_owner_pages(
    axum::extract::State(state): axum::extract::State<SseState>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
    use axum::http::StatusCode;

    let Some(owner) = params
        .get("owner")
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    else {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({ "ok": false, "error": "owner is required" })),
        );
    };

    let pages = {
        let st = state.browser.lock().await;
        st.page_owners
            .iter()
            .filter(|(_, page_owner)| page_owner.as_str() == owner)
            .filter_map(|(page_id, _)| {
                st.pages
                    .get(page_id)
                    .map(|entry| (page_id.clone(), entry.page.clone()))
            })
            .collect::<Vec<_>>()
    };

    let mut closed = Vec::new();
    for (page_id, page) in pages {
        if page.close().await.is_ok() {
            closed.push(page_id);
        }
    }

    let mut st = state.browser.lock().await;
    for page_id in &closed {
        st.pages.remove(page_id);
        st.page_owners.remove(page_id);
    }
    st.owners.remove(owner);

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "ok": true, "owner": owner, "closed": closed.len() })),
    )
}

fn constant_time_secret_eq(provided: &str, expected: &str) -> bool {
    if provided.len() != expected.len() {
        return false;
    }
    provided
        .as_bytes()
        .iter()
        .zip(expected.as_bytes())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn bind_address_is_loopback(bind: &str) -> bool {
    let Some((host, _port)) = bind.rsplit_once(':') else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
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
    let secret_broker = secret_broker::SecretBroker::from_env()?;

    let mcp_state = shared_state.clone();
    let mcp_secret_broker = secret_broker.clone();
    let streamable_config = StreamableHttpServerConfig::default();
    let cancellation_token = streamable_config.cancellation_token.clone();
    let service: StreamableHttpService<BrowserServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || {
                Ok(BrowserServer::with_state_and_broker(
                    mcp_state.clone(),
                    None,
                    mcp_secret_broker.clone(),
                ))
            },
            Default::default(),
            streamable_config,
        );

    let security = http_security::HttpSecurity::from_env(bind_address_is_loopback(&bind))?;
    let sse_state = SseState {
        sessions: Arc::new(Mutex::new(HashMap::new())),
        browser: shared_state,
        security: security.clone(),
        secret_broker: secret_broker.clone(),
    };

    let auth_security = security.clone();
    let auth_layer = axum::middleware::from_fn(move |request, next| {
        let security = auth_security.clone();
        async move { http_security::authorize_http(security, request, next).await }
    });

    let router = axum::Router::new()
        .route("/health", axum::routing::get(health))
        .route("/sse", axum::routing::get(sse_get))
        .route("/message", axum::routing::post(sse_post))
        .route("/owners", axum::routing::delete(close_owner_pages))
        .nest_service("/mcp", service)
        .with_state(sse_state.clone())
        .layer(auth_layer);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    info!("browser-rs MCP server on http://{bind}/mcp (streamable HTTP) + http://{bind}/sse (legacy SSE)");
    let shutdown_sessions = sse_state.sessions.clone();
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal(shutdown_sessions, cancellation_token))
        .await?;

    let browser = {
        let mut state = sse_state.browser.lock().await;
        state.pages.clear();
        state.page_owners.clear();
        state.owners.clear();
        state.browser.take()
    };
    if let Some(browser) = browser {
        browser.close().await;
    }
    Ok(())
}

async fn shutdown_signal(
    sessions: SseSessions,
    cancellation_token: tokio_util::sync::CancellationToken,
) {
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                tracing::warn!("failed to install SIGTERM handler: {error}");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        result = tokio::signal::ctrl_c() => {
            if let Err(error) = result {
                tracing::warn!("failed to listen for shutdown signal: {error}");
            }
        }
        _ = terminate => {}
    }
    cancellation_token.cancel();
    sessions.lock().await.clear();
}
