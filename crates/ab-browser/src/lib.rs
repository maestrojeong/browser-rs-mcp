//! High-level, agent-friendly browser control on top of `ab-cdp`.
//!
//! `Browser` owns the Chrome process and the CDP connection. `Page` is a single
//! attached tab (flatten-mode session). Everything is designed so an LLM agent
//! can run the loop: `snapshot -> act -> verify`.

pub mod pointer;
pub mod snapshot;
pub mod stealth;

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ab_cdp::CdpClient;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

pub use pointer::{PointerAction, PointerLocation, PointerOutcome, PointerRequest};
pub use snapshot::{DocumentIdentity, ElementRef, Snapshot};

/// One logged network request/response.
#[derive(Debug, Clone)]
pub struct NetEntry {
    pub url: String,
    pub method: String,
    pub resource_type: String,
    pub status: Option<i64>,
    pub failed: bool,
}

#[derive(Default)]
struct NetState {
    entries: Vec<NetEntry>,
    index: HashMap<String, usize>,
}

/// A live, growing log of a page's network activity (from CDP Network events).
#[derive(Clone, Default)]
pub struct NetworkLog {
    state: Arc<Mutex<NetState>>,
}

/// A live log of a page's console messages (needs Runtime.enable — a stealth
/// tell, so only started on demand).
#[derive(Clone, Default)]
pub struct ConsoleLog {
    lines: Arc<Mutex<Vec<String>>>,
}

impl ConsoleLog {
    pub fn recent(&self, limit: usize) -> Vec<String> {
        let v = self.lines.lock().unwrap();
        let start = v.len().saturating_sub(limit);
        v[start..].to_vec()
    }
}

impl NetworkLog {
    /// The most recent `limit` entries, optionally filtered by URL substring.
    pub fn recent(&self, limit: usize, filter: Option<&str>) -> Vec<NetEntry> {
        let st = self.state.lock().unwrap();
        let mut v: Vec<NetEntry> = st
            .entries
            .iter()
            .filter(|e| filter.is_none_or(|f| e.url.contains(f)))
            .cloned()
            .collect();
        if v.len() > limit {
            v = v.split_off(v.len() - limit);
        }
        v
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BrowserError {
    #[error("chrome executable not found; set AB_CHROME to its path")]
    ChromeNotFound,
    #[error("failed to launch chrome: {0}")]
    Launch(String),
    #[error("failed to discover devtools endpoint: {0}")]
    Discovery(String),
    #[error("cdp: {0}")]
    Cdp(#[from] ab_cdp::CdpError),
    #[error("unexpected protocol response: {0}")]
    Protocol(String),
    #[error("typing cancelled")]
    TypingCancelled,
}

pub type Result<T> = std::result::Result<T, BrowserError>;

/// Verified state after bringing a page target to the foreground.
#[derive(Debug, Clone, Serialize)]
pub struct PageActivation {
    pub activated: bool,
    pub visibility: String,
    pub window_focused: bool,
    pub attempts: u8,
}

#[derive(Debug, Clone)]
pub struct LaunchOptions {
    /// Headless is a strong fingerprint tell. Off by default — a real headful
    /// window on real hardware is what makes the fingerprint match a human's.
    pub headless: bool,
    /// Inject the self-guarding JS stealth layer into launched browsers.
    /// Enabled by default for both headful and headless launches; callers can
    /// disable it when an entirely untouched browser surface is required.
    pub inject_stealth: bool,
    pub chrome_path: Option<PathBuf>,
    /// Persistent profile directory. A stable, aged profile (cookies, history)
    /// looks human; a fresh temp profile every run is itself suspicious. When
    /// None, a persistent per-user default is used (not a temp dir).
    pub user_data_dir: Option<PathBuf>,
    pub port: u16,
    pub extra_args: Vec<String>,
    pub window_size: (u32, u32),
}

impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            headless: false,
            inject_stealth: true,
            chrome_path: None,
            user_data_dir: None,
            port: 0, // 0 => let Chrome pick, we read it back from DevToolsActivePort
            extra_args: Vec::new(),
            window_size: (1280, 800),
        }
    }
}

/// The browser process + CDP client.
pub struct Browser {
    client: CdpClient,
    child: Option<Child>,
    /// UA override applied to new pages (only set in headless+stealth mode).
    user_agent: String,
    /// Whether to inject the JS stealth-patching layer into new pages.
    inject_stealth: bool,
}

impl Browser {
    pub fn client(&self) -> &CdpClient {
        &self.client
    }

    /// Launch Chrome and connect over CDP.
    ///
    /// Default mode is headful with a persistent profile and the self-guarding
    /// stealth initialization script. The `AutomationControlled` blink feature
    /// is also disabled so `navigator.webdriver` is naturally false.
    pub async fn launch(opts: LaunchOptions) -> Result<Self> {
        let inject_stealth = opts.inject_stealth && std::env::var("AB_NO_STEALTH").is_err();
        let chrome = opts
            .chrome_path
            .clone()
            .or_else(detect_chrome)
            .ok_or(BrowserError::ChromeNotFound)?;

        let data_dir = match &opts.user_data_dir {
            Some(d) => d.clone(),
            None => default_profile_dir()?,
        };

        // A persistent profile keeps a stale `DevToolsActivePort` from the
        // previous run; if we read it before the new Chrome rewrites it we get
        // the wrong port ("no webSocketDebuggerUrl"). Remove it first. Also drop
        // Singleton* lock files left by an unclean (SIGKILL) exit.
        for f in [
            "DevToolsActivePort",
            "SingletonLock",
            "SingletonSocket",
            "SingletonCookie",
        ] {
            let _ = std::fs::remove_file(data_dir.join(f));
        }

        let mut args: Vec<String> = vec![
            format!("--remote-debugging-port={}", opts.port),
            format!("--user-data-dir={}", data_dir.display()),
            format!(
                "--window-size={},{}",
                opts.window_size.0, opts.window_size.1
            ),
            "--remote-allow-origins=*".to_string(),
        ];
        if opts.headless {
            args.push("--headless=new".to_string());
        }
        args.extend(stealth::launch_flags());
        args.extend(opts.extra_args.clone());
        // Extra flags from the environment (e.g. `--no-sandbox` when running as
        // root in CI/containers). Space-separated.
        if let Ok(flags) = std::env::var("AB_CHROME_FLAGS") {
            args.extend(flags.split_whitespace().map(String::from));
        }
        args.push("about:blank".to_string());

        debug!("launching chrome: {} {:?}", chrome.display(), args);
        let child = Command::new(&chrome)
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| BrowserError::Launch(e.to_string()))?;

        // Read the actual port Chrome bound (works even when port=0).
        let port = read_active_port(&data_dir).await?;
        let ws_url = discover_ws_url(port).await?;
        info!("connecting to devtools: {ws_url}");
        let client = CdpClient::connect(&ws_url).await?;

        client
            .send("Target.setDiscoverTargets", json!({ "discover": true }))
            .await?;

        // A UA override is only needed to hide the "Headless" token, i.e. only
        // when we're forced to run headless. Headful reports a real UA.
        let user_agent = if inject_stealth && opts.headless {
            client
                .send("Browser.getVersion", json!({}))
                .await
                .ok()
                .and_then(|v| v.get("userAgent").and_then(Value::as_str).map(String::from))
                .map(|ua| ua.replace("HeadlessChrome", "Chrome"))
                .unwrap_or_default()
        } else {
            String::new()
        };

        Ok(Self {
            client,
            child: Some(child),
            user_agent,
            inject_stealth,
        })
    }

    /// Attach to a Chrome the user is already running with
    /// `--remote-debugging-port=<port>`. This is the strongest identity mode:
    /// the fingerprint is exactly that of the user's own everyday browser,
    /// because it *is* their browser. No process is spawned or killed by us.
    pub async fn connect(port: u16) -> Result<Self> {
        let ws_url = discover_ws_url(port).await?;
        info!("attaching to existing chrome: {ws_url}");
        let client = CdpClient::connect(&ws_url).await?;
        client
            .send("Target.setDiscoverTargets", json!({ "discover": true }))
            .await?;
        Ok(Self {
            client,
            child: None,
            user_agent: String::new(),
            inject_stealth: false,
        })
    }

    /// Open a new tab and attach to it (flatten-mode session).
    pub async fn new_page(&self, url: &str) -> Result<Page> {
        let created = self
            .client
            .send("Target.createTarget", json!({ "url": "about:blank" }))
            .await?;
        let target_id = created
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| BrowserError::Protocol("no targetId".into()))?
            .to_string();

        let page = self.attach_page(&target_id).await?;
        if self.inject_stealth {
            page.init_stealth().await?;
            if !self.user_agent.is_empty() {
                page.set_user_agent(&self.user_agent).await?;
            }
        }
        if !url.is_empty() && url != "about:blank" {
            page.navigate(url).await?;
        }
        Ok(page)
    }

    /// Return page targets currently known to Chrome. Used by the MCP layer to
    /// discover tabs opened by target=_blank/window.open rather than by a tool.
    pub async fn page_targets(&self) -> Result<Vec<(String, String, Option<String>)>> {
        let result = self.client.send("Target.getTargets", json!({})).await?;
        let targets = result
            .get("targetInfos")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|info| info.get("type").and_then(Value::as_str) == Some("page"))
            .filter_map(|info| {
                let id = info.get("targetId")?.as_str()?.to_string();
                let url = info
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let opener_id = info
                    .get("openerId")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                Some((id, url, opener_id))
            })
            .collect();
        Ok(targets)
    }

    /// Attach to an existing page target, such as a popup discovered after a click.
    pub async fn attach_page(&self, target_id: &str) -> Result<Page> {
        let attached = self
            .client
            .send(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
            )
            .await?;
        let session_id = attached
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| BrowserError::Protocol("no sessionId".into()))?
            .to_string();

        Ok(Page {
            client: self.client.clone(),
            session_id,
            target_id: target_id.to_string(),
            frame_sessions: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(target_os = "macos")]
            browser_pid: self.child.as_ref().and_then(Child::id),
            pointer: Arc::new(Mutex::new(None)),
            pointer_mutation: Arc::new(tokio::sync::Mutex::new(())),
            dialog: Arc::new(Mutex::new((true, None))),
            dialog_handler_started: Arc::new(AtomicBool::new(false)),
            routes: Arc::new(Mutex::new(RouteState::default())),
        })
    }

    /// Terminate the browser process (only if we launched it; connect() no-op).
    pub async fn close(mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = self.client.send("Browser.close", json!({})).await;
            let _ = child.kill().await;
        }
    }
}

/// A mocked route: requests whose URL matches `pattern` are fulfilled with this
/// canned response instead of hitting the network.
#[derive(Clone)]
struct RouteMock {
    pattern: String,
    status: i64,
    body: String,
    content_type: String,
}

/// Per-page network-routing state (mock rules + whether the Fetch intercept
/// loop is already running).
#[derive(Default)]
struct RouteState {
    mocks: Vec<RouteMock>,
    loop_started: bool,
}

/// What to extract in `iframe_read` / `FrameAction::Read`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadMode {
    /// `element.outerHTML`.
    Html,
    /// `element.innerText` (falls back to `textContent`).
    Text,
}

/// Action to perform on the element resolved at the bottom of an iframe chain
/// (see `Page::descend_and_act`).
enum FrameAction {
    Point,
    Focus { clear: bool },
    Read(ReadMode),
}

/// Split a `frame_selector` argument into a chain of CSS selectors. Chains
/// are written Playwright-style as `"sel1 >> sel2 >> sel3"`, each hop naming
/// the `<iframe>` to descend into next; the last selector passed separately
/// to `iframe_click`/`iframe_type` targets the element inside the innermost
/// frame. A plain selector with no `>>` is a chain of length one (single
/// iframe hop), matching the pre-existing single-level API.
///
/// Known limitation: this is a naive `str::split`, not a CSS-aware parser.
/// A selector containing a literal `>>` substring inside a quoted attribute
/// value (e.g. `iframe[src*="a>>b"]`) will be incorrectly split into two
/// bogus hops. This is not expected in practice (`>>` is vanishingly rare in
/// real URLs/selectors) but is a known gap, not a supported escape hatch.
fn split_frame_chain(frame_selector: &str) -> Vec<&str> {
    frame_selector
        .split(">>")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

/// Split and validate a `frame_selector` argument, rejecting empty/
/// whitespace/`>>`-only input up front. Without this, an accidentally empty
/// `frame_selector` (e.g. a caller-side templating bug) would silently
/// resolve to a zero-length chain and `descend_and_act` would act directly
/// on the *top-level* document — which for iframe actions means acting on
/// something on the main page while the caller
/// believes they're targeting content inside an iframe.
fn require_frame_chain(frame_selector: &str) -> Result<Vec<&str>> {
    let chain = split_frame_chain(frame_selector);
    if chain.is_empty() {
        return Err(BrowserError::Protocol(format!(
            "frame_selector must contain at least one iframe CSS selector \
             (got {frame_selector:?}, which is empty/whitespace/'>>'-only)"
        )));
    }
    Ok(chain)
}

/// Build the JS body executed at each hop of `descend_and_act`. It walks
/// `chain` from the current document, hopping through `contentDocument` at
/// each step. If every hop is same-origin, it performs `action` on
/// `selector` in the innermost document and returns `{ok:true, ...}` (with a
/// `value` field for `FrameAction::Read`). The first time `contentDocument`
/// comes back null — cross-origin — it stops and returns
/// `{ok:false, index, src, name}` describing the boundary iframe element
/// (whose attributes remain readable even though its document does not), so
/// the caller can resume via CDP.
fn build_descend_js(chain: &[&str], selector: &str, action: &FrameAction) -> String {
    let chain_json = serde_json::to_string(chain).unwrap_or_else(|_| "[]".into());
    let sel_json = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into());
    let act_js = match action {
        FrameAction::Point => "el.scrollIntoView({block:'center',inline:'center'}); const r=el.getBoundingClientRect(); if(r.width<=0||r.height<=0) throw new Error('element has no visible box'); return {ok:true,x:offsetX+r.left+r.width/2,y:offsetY+r.top+r.height/2,halfWidth:r.width/2,halfHeight:r.height/2};".to_string(),
        FrameAction::Focus { clear } => {
            let select_js = if *clear {
                " if (typeof el.select === 'function') el.select(); else if (typeof el.setSelectionRange === 'function') el.setSelectionRange(0, (el.value || '').length);"
            } else {
                ""
            };
            format!("el.focus();{select_js} return {{ ok: true }};")
        }
        FrameAction::Read(ReadMode::Html) => "return { ok: true, value: el.outerHTML };".to_string(),
        FrameAction::Read(ReadMode::Text) => {
            "return { ok: true, value: (el.innerText !== undefined ? el.innerText : (el.textContent || '')) };"
                .to_string()
        }
    };
    let locate = matches!(action, FrameAction::Point);
    format!(
        r#"(() => {{
  const chain = {chain_json};
  const locate = {locate};
  let doc = document;
  let offsetX = 0, offsetY = 0;
  for (let i = 0; i < chain.length; i++) {{
    const f = doc.querySelector(chain[i]);
    if (!f) throw new Error('iframe not found at step ' + i + ': ' + chain[i]);
    if (locate) {{
      f.scrollIntoView({{block:'center',inline:'center'}});
      const r = f.getBoundingClientRect();
      offsetX += r.left + (f.clientLeft || 0);
      offsetY += r.top + (f.clientTop || 0);
    }}
    let inner = null;
    try {{ inner = f.contentDocument; }} catch (e) {{ inner = null; }}
    if (!inner) {{
      return {{ ok: false, index: i, src: f.src || f.getAttribute('src') || '', name: f.name || f.getAttribute('name') || '', offsetX, offsetY }};
    }}
    doc = inner;
  }}
  const el = doc.querySelector({sel_json});
  if (!el) throw new Error('element not found: ' + {sel_json});
  {act_js}
}})()"#,
        locate = locate,
    )
}

/// Build an expression that returns the iframe element at `index`, walking
/// any preceding same-origin iframe hops from the current execution context.
fn build_frame_element_js(chain: &[&str], index: usize) -> String {
    let path = chain.get(..=index).unwrap_or(chain);
    let path_json = serde_json::to_string(path).unwrap_or_else(|_| "[]".into());
    format!(
        r#"(() => {{
  const chain = {path_json};
  let doc = document;
  for (let i = 0; i < chain.length; i++) {{
    const f = doc.querySelector(chain[i]);
    if (!f) throw new Error('iframe not found at step ' + i + ': ' + chain[i]);
    if (i === chain.length - 1) return f;
    let inner = null;
    try {{ inner = f.contentDocument; }} catch (e) {{ inner = null; }}
    if (!inner) throw new Error('iframe at step ' + i + ' became cross-origin');
    doc = inner;
  }}
  throw new Error('iframe path is empty');
}})()"#
    )
}

/// Flatten a `Page.getFrameTree` response into `(frameId, name, url)` triples
/// for every frame, regardless of nesting depth or origin. Pulled out as a
/// standalone function (rather than a closure inside `frame_id_by_hint`) so
/// the matching logic in `resolve_frame_id` can be unit-tested against
/// hand-built fixtures without a live CDP connection.
fn collect_frames<'a>(node: &'a Value, out: &mut Vec<(&'a str, &'a str, &'a str)>) {
    if let Some(frame) = node.get("frame") {
        let id = frame.get("id").and_then(Value::as_str).unwrap_or("");
        let fname = frame.get("name").and_then(Value::as_str).unwrap_or("");
        let furl = frame.get("url").and_then(Value::as_str).unwrap_or("");
        if !id.is_empty() {
            out.push((id, fname, furl));
        }
    }
    for child in node
        .get("childFrames")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        collect_frames(child, out);
    }
}

/// Resolve a single frame id from a flattened frame list by `name` or `src`
/// hint. `name` is preferred and matched exactly; `src` is matched exactly
/// against the frame's committed `url` first, then by substring (to
/// tolerate relative-vs-absolute differences). Any hint that matches *more
/// than one* frame is rejected as ambiguous rather than silently resolved to
/// the first entry — see `Page::frame_id_by_hint` doc comment for the
/// rationale and known limitations (non-unique names/URLs, redirects).
fn resolve_frame_id(frames: &[(&str, &str, &str)], name: &str, src: &str) -> Result<String> {
    let ambiguous = |what: &str, matched: &[&str]| {
        BrowserError::Protocol(format!(
            "ambiguous cross-origin iframe: {} frames match {what} (name={name:?}, src={src:?}); \
             give the iframe a unique name/id, or use a `>>` chain hop that narrows the search",
            matched.len()
        ))
    };

    if !name.is_empty() {
        let matches: Vec<&str> = frames
            .iter()
            .filter(|(_, fname, _)| *fname == name)
            .map(|(id, _, _)| *id)
            .collect();
        match matches.len() {
            0 => {} // fall through to src matching
            1 => return Ok(matches[0].to_string()),
            _ => return Err(ambiguous("by name", &matches)),
        }
    }

    if !src.is_empty() {
        let exact: Vec<&str> = frames
            .iter()
            .filter(|(_, _, furl)| *furl == src)
            .map(|(id, _, _)| *id)
            .collect();
        match exact.len() {
            1 => return Ok(exact[0].to_string()),
            n if n > 1 => return Err(ambiguous("exactly by url", &exact)),
            _ => {}
        }

        let sub: Vec<&str> = frames
            .iter()
            .filter(|(_, _, furl)| !furl.is_empty() && (furl.contains(src) || src.contains(*furl)))
            .map(|(id, _, _)| *id)
            .collect();
        match sub.len() {
            1 => return Ok(sub[0].to_string()),
            n if n > 1 => return Err(ambiguous("loosely by url substring", &sub)),
            _ => {}
        }
    }

    Err(BrowserError::Protocol({
        let urls: Vec<&str> = frames
            .iter()
            .map(|(_, _, u)| *u)
            .filter(|u| !u.is_empty())
            .collect();
        format!(
            "cross-origin iframe not found in frame tree (name={name:?}, src={src:?}); \
             {n} frames in tree: {urls:?}. \
             note: a frame that redirected or navigated after load may no longer have a \
             URL containing its original `src` attribute; \
             the caller may retry by resolving the iframe DOM node directly",
            n = frames.len(),
        )
    }))
}

/// A single attached tab.
#[derive(Clone)]
pub struct Page {
    client: CdpClient,
    session_id: String,
    target_id: String,
    /// Flatten-mode CDP sessions attached to out-of-process iframe targets,
    /// keyed by the frame/target id returned from `DOM.describeNode`.
    frame_sessions: Arc<Mutex<HashMap<String, String>>>,
    /// Browser process launched by this crate. Used only for a best-effort
    /// macOS foreground fallback; externally connected browsers leave it unset.
    #[cfg(target_os = "macos")]
    browser_pid: Option<u32>,
    /// Last known pointer position (shared across clones of this page) so mouse
    /// motion is *continuous* — the next move starts where the last one ended,
    /// instead of teleporting to a fresh random origin every click.
    pointer: Arc<Mutex<Option<(f64, f64)>>>,
    /// Serialize pointer validation and dispatch by the real page target.
    pointer_mutation: Arc<tokio::sync::Mutex<()>>,
    /// JS-dialog handling policy: (accept, prompt_text). Read by the auto-handler.
    dialog: Arc<Mutex<(bool, Option<String>)>>,
    /// Whether explicit dialog handling has installed its Page-domain listener.
    dialog_handler_started: Arc<AtomicBool>,
    /// Network mock rules + intercept-loop state.
    routes: Arc<Mutex<RouteState>>,
}

#[derive(Clone, Debug)]
struct FrameExecutionContext {
    session_id: String,
    context_id: i64,
}

impl Page {
    pub fn target_id(&self) -> &str {
        &self.target_id
    }

    /// Close this tab at the browser target level.
    pub async fn close(&self) -> Result<()> {
        self.client
            .send("Target.closeTarget", json!({ "targetId": self.target_id }))
            .await?;
        Ok(())
    }

    /// Bring this target to the foreground and verify the document is visible.
    ///
    /// `Target.activateTarget` is the canonical CDP operation. On macOS, when
    /// this crate launched Chrome and CDP activation alone did not focus the
    /// window, a best-effort process-level foreground request is made before
    /// retrying. Externally connected browsers are never guessed by app name.
    pub async fn activate(&self) -> Result<PageActivation> {
        let mut visibility = String::from("unknown");
        let mut window_focused = false;

        for attempts in 1..=3_u8 {
            self.client
                .send(
                    "Target.activateTarget",
                    json!({ "targetId": self.target_id }),
                )
                .await?;

            if attempts > 1 {
                self.bring_browser_process_to_front().await;
            }
            tokio::time::sleep(Duration::from_millis(100 * u64::from(attempts))).await;

            let state = match self
                .evaluate(
                    "({visibility: document.visibilityState, windowFocused: document.hasFocus()})",
                )
                .await
            {
                Ok(state) => state,
                Err(error) if attempts < 3 => {
                    debug!(
                        "page activation visibility check failed on attempt {attempts}: {error}"
                    );
                    continue;
                }
                Err(error) => return Err(error),
            };
            visibility = state
                .get("visibility")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            window_focused = state
                .get("windowFocused")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            if activation_verified(&visibility, window_focused) {
                return Ok(PageActivation {
                    activated: true,
                    visibility,
                    window_focused,
                    attempts,
                });
            }
        }

        Ok(PageActivation {
            activated: activation_verified(&visibility, window_focused),
            visibility,
            window_focused,
            attempts: 3,
        })
    }

    #[cfg(target_os = "macos")]
    async fn bring_browser_process_to_front(&self) {
        let Some(pid) = self.browser_pid else {
            return;
        };
        let script = format!(
            "tell application \"System Events\" to set frontmost of first process whose unix id is {pid} to true"
        );
        let _ = Command::new("osascript")
            .args(["-e", &script])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }

    #[cfg(not(target_os = "macos"))]
    async fn bring_browser_process_to_front(&self) {}

    async fn init_stealth(&self) -> Result<()> {
        // Inject before any page script. Does NOT require Runtime.enable.
        self.client
            .send_on(
                &self.session_id,
                "Page.addScriptToEvaluateOnNewDocument",
                json!({ "source": stealth::STEALTH_INIT_SCRIPT }),
            )
            .await?;
        Ok(())
    }

    /// Override the User-Agent for this page (session-scoped, not page-visible).
    pub async fn set_user_agent(&self, ua: &str) -> Result<()> {
        self.client
            .send_on(
                &self.session_id,
                "Emulation.setUserAgentOverride",
                json!({ "userAgent": ua }),
            )
            .await?;
        Ok(())
    }

    /// Navigate and wait for the load event.
    pub async fn navigate(&self, url: &str) -> Result<()> {
        // Enable Page domain only (needed for lifecycle); avoid Runtime.enable.
        self.client
            .send_on(&self.session_id, "Page.enable", json!({}))
            .await?;
        self.client
            .send_on(&self.session_id, "Page.navigate", json!({ "url": url }))
            .await?;
        self.wait_for_load().await?;
        Ok(())
    }

    async fn wait_for_load(&self) -> Result<()> {
        let mut rx = self.client.events();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Ok(ev)) => {
                    if ev.session_id.as_deref() == Some(&self.session_id)
                        && ev.method == "Page.loadEventFired"
                    {
                        return Ok(());
                    }
                }
                Ok(Err(_)) => return Ok(()), // lagged/closed: proceed best-effort
                Err(_) => return Ok(()),     // timeout: proceed best-effort
            }
        }
    }

    /// One-shot JS evaluation. Defaults to an **isolated world** so the
    /// execution isn't observable by the page (avoids the `mainWorldExecution`
    /// automation tell). Never enables the Runtime domain. Note: isolated-world
    /// code shares the DOM but cannot see JS globals the page set on `window` —
    /// use `evaluate_main` for that.
    pub async fn evaluate(&self, expression: &str) -> Result<Value> {
        self.eval_raw(expression, true).await
    }

    /// Evaluate in the page's **main world** (can read page-set `window`
    /// globals, but the execution is observable). Prefer `evaluate`.
    pub async fn evaluate_main(&self, expression: &str) -> Result<Value> {
        self.eval_raw(expression, false).await
    }

    async fn main_frame_id(&self) -> Result<String> {
        let tree = self
            .client
            .send_on(&self.session_id, "Page.getFrameTree", json!({}))
            .await?;
        tree.get("frameTree")
            .and_then(|t| t.get("frame"))
            .and_then(|f| f.get("id"))
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| BrowserError::Protocol("no main frame id".into()))
    }

    async fn eval_raw(&self, expression: &str, isolated: bool) -> Result<Value> {
        let mut params = json!({
            "expression": expression,
            "returnByValue": true,
            "awaitPromise": true,
        });
        if isolated {
            // Create a fresh isolated world (valid after navigation) and target it.
            // Fall back to the main world if the page domain isn't ready.
            if let Ok(frame) = self.main_frame_id().await {
                if let Ok(w) = self
                    .client
                    .send_on(
                        &self.session_id,
                        "Page.createIsolatedWorld",
                        json!({
                            "frameId": frame,
                            "worldName": "ab_isolated",
                            "grantUniversalAccess": false,
                        }),
                    )
                    .await
                {
                    if let Some(ctx) = w.get("executionContextId").and_then(Value::as_i64) {
                        params["contextId"] = json!(ctx);
                    }
                }
            }
        }
        let res = self
            .client
            .send_on(&self.session_id, "Runtime.evaluate", params)
            .await?;
        if let Some(exc) = res.get("exceptionDetails") {
            return Err(BrowserError::Protocol(format!("JS exception: {exc}")));
        }
        Ok(res
            .get("result")
            .and_then(|r| r.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// Current document URL.
    pub async fn url(&self) -> Result<String> {
        Ok(self
            .evaluate("location.href")
            .await?
            .as_str()
            .unwrap_or("")
            .to_string())
    }

    /// Accessibility-tree snapshot with [ref] handles for interactive nodes.
    pub async fn snapshot(&self) -> Result<Snapshot> {
        self.client
            .send_on(&self.session_id, "Accessibility.enable", json!({}))
            .await?;
        let res = self
            .client
            .send_on(&self.session_id, "Accessibility.getFullAXTree", json!({}))
            .await?;
        let nodes = res
            .get("nodes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let document = self.current_main_document_identity().await?;
        Ok(snapshot::render_with_document(&nodes, Some(document)))
    }

    async fn current_main_document_identity(&self) -> Result<DocumentIdentity> {
        let tree = self
            .client
            .send_on(&self.session_id, "Page.getFrameTree", json!({}))
            .await?;
        let frame = tree
            .get("frameTree")
            .and_then(|tree| tree.get("frame"))
            .ok_or_else(|| BrowserError::Protocol("no main frame identity".into()))?;
        let frame_id = frame
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| BrowserError::Protocol("main frame has no id".into()))?;
        let loader_id = frame
            .get("loaderId")
            .and_then(Value::as_str)
            .ok_or_else(|| BrowserError::Protocol("main frame has no loaderId".into()))?;
        Ok(DocumentIdentity {
            target_id: self.target_id.clone(),
            frame_id: frame_id.to_string(),
            loader_id: loader_id.to_string(),
        })
    }

    /// Re-prove that a snapshot ref still belongs to this exact live document.
    pub async fn validate_element_ref(&self, element: &ElementRef) -> Result<()> {
        let expected = element.document.as_ref().ok_or_else(|| {
            BrowserError::Protocol("unproven ref: re-snapshot the live page".into())
        })?;
        let live = self.current_main_document_identity().await?;
        if &live != expected {
            return Err(BrowserError::Protocol(
                "stale ref: the page target or document changed; re-snapshot".into(),
            ));
        }
        let resolved = self
            .client
            .send_on(
                &self.session_id,
                "DOM.resolveNode",
                json!({ "backendNodeId": element.backend_node_id }),
            )
            .await
            .map_err(|_| BrowserError::Protocol("stale ref: node no longer resolves".into()))?;
        if resolved
            .pointer("/object/objectId")
            .and_then(Value::as_str)
            .is_none()
        {
            return Err(BrowserError::Protocol(
                "stale ref: node has no live object".into(),
            ));
        }
        Ok(())
    }

    /// Bind a selector-resolved backend node to the current document identity.
    pub async fn element_ref_for_backend(&self, backend_node_id: i64) -> Result<ElementRef> {
        Ok(ElementRef {
            backend_node_id,
            document: Some(self.current_main_document_identity().await?),
        })
    }

    /// Full-page PNG screenshot, returned as raw bytes.
    pub async fn screenshot(&self) -> Result<Vec<u8>> {
        let res = self
            .client
            .send_on(
                &self.session_id,
                "Page.captureScreenshot",
                json!({ "format": "png", "captureBeyondViewport": true }),
            )
            .await?;
        let b64 = res
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| BrowserError::Protocol("no screenshot data".into()))?;
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| BrowserError::Protocol(e.to_string()))
    }

    /// Extract readable page text (best-effort, main content).
    pub async fn text(&self) -> Result<String> {
        Ok(self
            .evaluate("document.body ? document.body.innerText : ''")
            .await?
            .as_str()
            .unwrap_or("")
            .to_string())
    }

    /// Extract the page as Markdown (headings, links, lists, code, quotes).
    /// A pragmatic DOM walker — smaller and more stable than raw HTML.
    pub async fn read_markdown(&self) -> Result<String> {
        let js = r#"(() => {
          const skip = new Set(['SCRIPT','STYLE','NOSCRIPT','SVG','CANVAS','IFRAME','HEAD','NAV','FOOTER']);
          const out = [];
          const inline = (el) => {
            let s = '';
            el.childNodes.forEach((n) => {
              if (n.nodeType === 3) s += n.textContent;
              else if (n.nodeType === 1) {
                const t = n.tagName;
                if (t === 'A' && n.getAttribute('href')) s += '[' + inline(n).trim() + '](' + n.href + ')';
                else if (t === 'STRONG' || t === 'B') s += '**' + inline(n).trim() + '**';
                else if (t === 'EM' || t === 'I') s += '*' + inline(n).trim() + '*';
                else if (t === 'CODE') s += '`' + n.textContent + '`';
                else if (t === 'BR') s += '\n';
                else s += inline(n);
              }
            });
            return s;
          };
          const walk = (el) => {
            for (const n of el.children) {
              const t = n.tagName;
              if (skip.has(t)) continue;
              if (/^H[1-6]$/.test(t)) { const s = inline(n).trim(); if (s) out.push('#'.repeat(+t[1]) + ' ' + s); }
              else if (t === 'P') { const s = inline(n).trim(); if (s) out.push(s); }
              else if (t === 'LI') { const s = inline(n).trim(); if (s) out.push('- ' + s); }
              else if (t === 'PRE') { const s = n.textContent.trim(); if (s) out.push('```\n' + s + '\n```'); }
              else if (t === 'BLOCKQUOTE') { const s = inline(n).trim(); if (s) out.push('> ' + s); }
              else walk(n);
            }
          };
          walk(document.body || document.documentElement);
          return out.join('\n\n');
        })()"#;
        Ok(self.evaluate(js).await?.as_str().unwrap_or("").to_string())
    }
}

fn ax_hit_has_backend_ancestor(nodes: &[Value], hit_backend: i64, target_backend: i64) -> bool {
    let mut current = nodes
        .iter()
        .find(|node| node.get("backendDOMNodeId").and_then(Value::as_i64) == Some(hit_backend));
    for _ in 0..nodes.len() {
        let Some(node) = current else { return false };
        if node.get("backendDOMNodeId").and_then(Value::as_i64) == Some(target_backend) {
            return true;
        }
        let Some(parent_id) = node.get("parentId").and_then(Value::as_str) else {
            return false;
        };
        current = nodes
            .iter()
            .find(|candidate| candidate.get("nodeId").and_then(Value::as_str) == Some(parent_id));
    }
    false
}

/// Actions driven by an accessibility `[ref]` (its backendDOMNodeId).
impl Page {
    /// Resolve the on-screen center of a node from its box model.
    async fn node_center(&self, backend: i64) -> Result<Option<(f64, f64)>> {
        let res = self
            .client
            .send_on(
                &self.session_id,
                "DOM.getBoxModel",
                json!({ "backendNodeId": backend }),
            )
            .await;
        let Ok(res) = res else { return Ok(None) };
        let quad = res
            .get("model")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array);
        let Some(q) = quad else { return Ok(None) };
        if q.len() < 8 {
            return Ok(None);
        }
        let xs: Vec<f64> = [q[0].as_f64(), q[2].as_f64(), q[4].as_f64(), q[6].as_f64()]
            .into_iter()
            .flatten()
            .collect();
        let ys: Vec<f64> = [q[1].as_f64(), q[3].as_f64(), q[5].as_f64(), q[7].as_f64()]
            .into_iter()
            .flatten()
            .collect();
        if xs.len() < 4 || ys.len() < 4 {
            return Ok(None);
        }
        let cx = xs.iter().sum::<f64>() / 4.0;
        let cy = ys.iter().sum::<f64>() / 4.0;
        // Humans never click the pixel-perfect center of a target — detectors
        // flag `hasClickedExactCenter`. Aim at an off-center point inside the box.
        let half_w = (xs.iter().cloned().fold(f64::MIN, f64::max)
            - xs.iter().cloned().fold(f64::MAX, f64::min))
            / 2.0;
        let half_h = (ys.iter().cloned().fold(f64::MIN, f64::max)
            - ys.iter().cloned().fold(f64::MAX, f64::min))
            / 2.0;
        Ok(Some((cx + off_center(half_w), cy + off_center(half_h))))
    }

    /// Resolve a CSS selector to a backendDOMNodeId (for act-by-selector).
    pub async fn backend_for_selector(&self, selector: &str) -> Result<Option<i64>> {
        let doc = self
            .client
            .send_on(&self.session_id, "DOM.getDocument", json!({ "depth": 0 }))
            .await?;
        let Some(root) = doc
            .get("root")
            .and_then(|r| r.get("nodeId"))
            .and_then(Value::as_i64)
        else {
            return Ok(None);
        };
        let q = self
            .client
            .send_on(
                &self.session_id,
                "DOM.querySelector",
                json!({ "nodeId": root, "selector": selector }),
            )
            .await;
        let nid = q
            .ok()
            .and_then(|v| v.get("nodeId").and_then(Value::as_i64))
            .filter(|n| *n != 0);
        let Some(nid) = nid else { return Ok(None) };
        let d = self
            .client
            .send_on(
                &self.session_id,
                "DOM.describeNode",
                json!({ "nodeId": nid }),
            )
            .await?;
        Ok(d.get("node")
            .and_then(|n| n.get("backendNodeId"))
            .and_then(Value::as_i64))
    }

    /// Search the page's visible text for a query; returns matching snippets.
    pub async fn find(
        &self,
        query: &str,
        regex: bool,
        ignore_case: bool,
        max: usize,
    ) -> Result<Value> {
        let js = format!(
            r#"(() => {{
              const q = {q}, rx = {rx}, ic = {ic}, max = {max};
              let re = null; try {{ if (rx) re = new RegExp(q, ic ? 'i' : ''); }} catch (_) {{}}
              const test = (s) => rx ? (re && re.test(s)) : (ic ? s.toLowerCase().includes(q.toLowerCase()) : s.includes(q));
              const out = [];
              for (const n of document.body ? document.body.querySelectorAll('*') : []) {{
                if (n.children.length) continue;
                const t = (n.innerText || n.textContent || '').trim();
                if (t && t.length < 300 && test(t)) out.push(t);
                if (out.length >= max * 3) break;
              }}
              return [...new Set(out)].slice(0, max);
            }})()"#,
            q = serde_json::to_string(query).unwrap_or_else(|_| "\"\"".into()),
            rx = regex,
            ic = ignore_case,
            max = max,
        );
        self.evaluate(&js).await
    }

    /// Actionability hit-test using only browser protocol domains, avoiding
    /// main-world Runtime execution that a page can observe. Returns true when
    /// the point lands on the target or its accessibility ancestor chain.
    async fn point_hits_node(&self, backend: i64, x: f64, y: f64) -> Result<bool> {
        // Input events use viewport coordinates, while DOM.getNodeForLocation
        // expects document coordinates.
        let metrics = self
            .client
            .send_on(&self.session_id, "Page.getLayoutMetrics", json!({}))
            .await?;
        let viewport = metrics
            .get("cssVisualViewport")
            .or_else(|| metrics.get("visualViewport"));
        let page_x = viewport
            .and_then(|value| value.get("pageX"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let page_y = viewport
            .and_then(|value| value.get("pageY"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let hit = self
            .client
            .send_on(
                &self.session_id,
                "DOM.getNodeForLocation",
                json!({
                    "x": (x + page_x).round() as i64,
                    "y": (y + page_y).round() as i64,
                }),
            )
            .await
            .map_err(|error| {
                BrowserError::Protocol(format!("DOM hit-test failed at ({x:.2}, {y:.2}): {error}"))
            })?;
        let Some(hit_backend) = hit.get("backendNodeId").and_then(Value::as_i64) else {
            return Ok(false);
        };
        if hit_backend == backend {
            return Ok(true);
        }

        let relatives = self
            .client
            .send_on(
                &self.session_id,
                "Accessibility.getPartialAXTree",
                json!({
                    "backendNodeId": hit_backend,
                    "fetchRelatives": true,
                }),
            )
            .await?;
        Ok(relatives
            .get("nodes")
            .and_then(Value::as_array)
            .is_some_and(|nodes| ax_hit_has_backend_ancestor(nodes, hit_backend, backend)))
    }

    /// Resolve a backend node to a Runtime objectId (for JS calls on it).
    async fn resolve_object(&self, backend: i64) -> Result<Option<String>> {
        let res = self
            .client
            .send_on(
                &self.session_id,
                "DOM.resolveNode",
                json!({ "backendNodeId": backend }),
            )
            .await;
        Ok(res.ok().and_then(|r| {
            r.get("object")
                .and_then(|o| o.get("objectId"))
                .and_then(Value::as_str)
                .map(String::from)
        }))
    }

    async fn resolve_object_in_context(
        &self,
        backend: i64,
        execution_context_id: i64,
    ) -> Result<Option<String>> {
        let res = self
            .client
            .send_on(
                &self.session_id,
                "DOM.resolveNode",
                json!({
                    "backendNodeId": backend,
                    "executionContextId": execution_context_id,
                }),
            )
            .await;
        Ok(res.ok().and_then(|response| {
            response
                .get("object")
                .and_then(|object| object.get("objectId"))
                .and_then(Value::as_str)
                .map(String::from)
        }))
    }

    /// Move the pointer to (x, y) like a human: a curved (cubic-Bézier) path
    /// with many small steps (~1 per few px, matching a real ~60-120 Hz
    /// pointer), an ease-in-out velocity profile, per-step jitter, and a final
    /// settle. Behavioral detectors flag the sparse, uniform jumps a naive
    /// automation makes; this produces dense, non-uniform, curved motion.
    async fn human_move_to(&self, x: f64, y: f64) -> Result<()> {
        // Continue from wherever the pointer currently is (continuous motion).
        // On the very first move, begin from a plausible resting point.
        let (sx, sy) = {
            let p = self.pointer.lock().unwrap();
            match *p {
                Some(pos) => pos,
                None => (x - 120.0 + rand_f64(90.0), y - 90.0 + rand_f64(70.0)),
            }
        };
        let dx = x - sx;
        let dy = y - sy;
        let dist = (dx * dx + dy * dy).sqrt().max(1.0);
        // ~1 step per 3.5 px so even fast flicks stay dense (small per-event
        // deltas, like a real 60-120 Hz pointer).
        let steps = ((dist / 3.5) as usize).clamp(16, 120);

        // Perpendicular unit vector, for a curved (not straight) path.
        let nx = -dy / dist;
        let ny = dx / dist;
        let bow = rand_f64(0.22 * dist); // curvature magnitude (signed)
        let c1x = sx + dx * 0.3 + nx * bow;
        let c1y = sy + dy * 0.3 + ny * bow;
        let c2x = sx + dx * 0.7 + nx * bow * 0.6;
        let c2y = sy + dy * 0.7 + ny * bow * 0.6;

        for i in 1..=steps {
            let raw = i as f64 / steps as f64;
            // ease-in-out → slow start, fast middle, slow end (human velocity)
            let t = raw * raw * (3.0 - 2.0 * raw);
            let mt = 1.0 - t;
            let px = mt * mt * mt * sx
                + 3.0 * mt * mt * t * c1x
                + 3.0 * mt * t * t * c2x
                + t * t * t * x
                + rand_f64(1.1);
            let py = mt * mt * mt * sy
                + 3.0 * mt * mt * t * c1y
                + 3.0 * mt * t * t * c2y
                + t * t * t * y
                + rand_f64(1.1);
            self.client
                .send_on(
                    &self.session_id,
                    "Input.dispatchMouseEvent",
                    json!({ "type": "mouseMoved", "x": px, "y": py }),
                )
                .await?;
            // Occasional longer dwell, like a human hesitating mid-move.
            let mut ms = rand_u64(3, 13);
            if rand_u64(0, 100) < 8 {
                ms += rand_u64(20, 70);
            }
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
        // Land exactly on the target and remember it as the current position.
        self.client
            .send_on(
                &self.session_id,
                "Input.dispatchMouseEvent",
                json!({ "type": "mouseMoved", "x": x, "y": y }),
            )
            .await?;
        *self.pointer.lock().unwrap() = Some((x, y));
        Ok(())
    }

    /// Playwright-style pre-action: bring the node into the viewport so its
    /// box-model coordinates are valid, then let layout settle briefly. Used by
    /// every coordinate-based action (click/hover/drag) and by focus/type so
    /// off-screen targets don't silently miss.
    async fn scroll_into_view(&self, backend: i64) {
        let _ = self
            .client
            .send_on(
                &self.session_id,
                "DOM.scrollIntoViewIfNeeded",
                json!({ "backendNodeId": backend }),
            )
            .await;
        tokio::time::sleep(Duration::from_millis(rand_u64(30, 80))).await;
    }

    /// Focus a node and enter text. Long text uses one trusted
    /// `Input.insertText` command, matching a paste/IME-style workflow;
    /// shorter text retains humanized per-character key events.
    pub async fn type_text(&self, backend: i64, text: &str, clear: bool) -> Result<()> {
        self.type_text_cancellable(backend, text, clear, &CancellationToken::new())
            .await
    }

    /// Like [`Page::type_text`], but stops between key events when `cancel`
    /// is triggered. Text already entered remains in the field.
    pub async fn type_text_cancellable(
        &self,
        backend: i64,
        text: &str,
        clear: bool,
        cancel: &CancellationToken,
    ) -> Result<()> {
        ensure_typing_active(cancel)?;
        self.scroll_into_view(backend).await;
        self.client
            .send_on(
                &self.session_id,
                "DOM.focus",
                json!({ "backendNodeId": backend }),
            )
            .await?;
        if clear {
            if let Some(obj) = self.resolve_object(backend).await? {
                self.client
                    .send_on(
                        &self.session_id,
                        "Runtime.callFunctionOn",
                        json!({
                            "objectId": obj,
                            "functionDeclaration":
                                "function(){ if (this.select) this.select(); else if (this.setSelectionRange) this.setSelectionRange(0, (this.value||'').length); }",
                        }),
                    )
                    .await?;
            }
        }

        self.dispatch_text_cancellable(&self.session_id, text, clear, cancel)
            .await
    }

    /// Dispatch text to the element currently focused in `session_id`.
    /// Keeping the session explicit lets iframe typing target an OOPIF's own
    /// renderer session instead of always falling back to the top-level page.
    async fn dispatch_text_cancellable(
        &self,
        session_id: &str,
        text: &str,
        clear: bool,
        cancel: &CancellationToken,
    ) -> Result<()> {
        if clear {
            // Delete the selection so typed keys replace it.
            for kind in ["keyDown", "keyUp"] {
                self.client
                    .send_on(
                        session_id,
                        "Input.dispatchKeyEvent",
                        json!({ "type": kind, "key": "Delete", "code": "Delete", "windowsVirtualKeyCode": 46, "nativeVirtualKeyCode": 46, "modifiers": 0 }),
                    )
                    .await?;
            }
        }

        ensure_typing_active(cancel)?;
        if uses_insert_text(text) {
            return self
                .dispatch_paste_cancellable(session_id, text, cancel)
                .await;
        }

        let mut shift_held = false;
        for ch in text.chars() {
            if cancel.is_cancelled() {
                if shift_held {
                    self.release_shift_on(session_id).await?;
                }
                return Err(BrowserError::TypingCancelled);
            }
            let s = ch.to_string();
            let (code, vk) = us_qwerty_key(ch);
            let need_shift = needs_shift(ch);
            // Real keyboards produce shifted chars (uppercase, @, !, …) only
            // while Shift is physically held. Detectors flag e.g. "@ typed
            // without a modifier". Press/release Shift around such characters.
            if need_shift && !shift_held {
                self.client
                    .send_on(
                        session_id,
                        "Input.dispatchKeyEvent",
                        json!({ "type": "keyDown", "key": "Shift", "code": "ShiftLeft", "windowsVirtualKeyCode": 16, "nativeVirtualKeyCode": 16, "modifiers": 8 }),
                    )
                    .await?;
                shift_held = true;
                if typing_delay(cancel, rand_u64(15, 45)).await.is_err() {
                    self.release_shift_on(session_id).await?;
                    return Err(BrowserError::TypingCancelled);
                }
            } else if !need_shift && shift_held {
                self.release_shift_on(session_id).await?;
                shift_held = false;
            }
            let modifiers = if need_shift { 8 } else { 0 };
            self.client
                .send_on(
                    session_id,
                    "Input.dispatchKeyEvent",
                    json!({ "type": "keyDown", "text": s, "key": s, "unmodifiedText": s, "code": code, "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk, "modifiers": modifiers }),
                )
                .await?;
            // Key hold time (press duration), then release.
            let cancelled = typing_delay(cancel, rand_u64(20, 90)).await.is_err();
            self.client
                .send_on(
                    session_id,
                    "Input.dispatchKeyEvent",
                    json!({ "type": "keyUp", "key": s, "code": code, "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk, "modifiers": modifiers }),
                )
                .await?;
            if cancelled {
                if shift_held {
                    self.release_shift_on(session_id).await?;
                }
                return Err(BrowserError::TypingCancelled);
            }
            // Inter-key gap with real human burstiness (bimodal): most keys are
            // moderate, ~12% are fast bursts, ~18% are longer "thinking" pauses.
            // High variance is itself a human signal — metronomic typing is a tell.
            let roll = rand_u64(0, 100);
            let gap = if roll < 12 {
                rand_u64(18, 55) // fast burst
            } else if roll < 30 {
                rand_u64(190, 560) // pause
            } else {
                rand_u64(55, 175) // normal
            };
            if typing_delay(cancel, gap).await.is_err() {
                if shift_held {
                    self.release_shift_on(session_id).await?;
                }
                return Err(BrowserError::TypingCancelled);
            }
        }
        if shift_held {
            self.release_shift_on(session_id).await?;
        }
        Ok(())
    }

    async fn dispatch_paste_cancellable(
        &self,
        session_id: &str,
        text: &str,
        cancel: &CancellationToken,
    ) -> Result<()> {
        ensure_typing_active(cancel)?;
        typing_delay(cancel, rand_u64(90, 240)).await?;

        #[cfg(target_os = "macos")]
        let (modifier_key, modifier_code, modifier_vk, modifiers) = ("Meta", "MetaLeft", 91, 4);
        #[cfg(not(target_os = "macos"))]
        let (modifier_key, modifier_code, modifier_vk, modifiers) =
            ("Control", "ControlLeft", 17, 2);

        self.client
            .send_on(
                session_id,
                "Input.dispatchKeyEvent",
                json!({ "type": "keyDown", "key": modifier_key, "code": modifier_code, "windowsVirtualKeyCode": modifier_vk, "nativeVirtualKeyCode": modifier_vk, "modifiers": modifiers }),
            )
            .await?;
        if typing_delay(cancel, rand_u64(20, 55)).await.is_err() {
            self.release_modifier_on(session_id, modifier_key, modifier_code, modifier_vk)
                .await?;
            return Err(BrowserError::TypingCancelled);
        }

        self.client
            .send_on(
                session_id,
                "Input.dispatchKeyEvent",
                json!({ "type": "keyDown", "key": "v", "code": "KeyV", "windowsVirtualKeyCode": 86, "nativeVirtualKeyCode": 86, "modifiers": modifiers }),
            )
            .await?;
        let cancelled = typing_delay(cancel, rand_u64(25, 75)).await.is_err();
        self.client
            .send_on(
                session_id,
                "Input.dispatchKeyEvent",
                json!({ "type": "keyUp", "key": "v", "code": "KeyV", "windowsVirtualKeyCode": 86, "nativeVirtualKeyCode": 86, "modifiers": modifiers }),
            )
            .await?;
        if cancelled {
            self.release_modifier_on(session_id, modifier_key, modifier_code, modifier_vk)
                .await?;
            return Err(BrowserError::TypingCancelled);
        }

        if typing_delay(cancel, rand_u64(10, 35)).await.is_err() {
            self.release_modifier_on(session_id, modifier_key, modifier_code, modifier_vk)
                .await?;
            return Err(BrowserError::TypingCancelled);
        }
        self.release_modifier_on(session_id, modifier_key, modifier_code, modifier_vk)
            .await?;
        ensure_typing_active(cancel)?;

        // CDP cannot populate a trusted ClipboardEvent's clipboardData without
        // an OS clipboard or observable main-world JS. insertText carries the
        // content after the trusted shortcut while preserving stealth policy.
        self.client
            .send_on(session_id, "Input.insertText", json!({ "text": text }))
            .await?;
        Ok(())
    }

    async fn release_modifier_on(
        &self,
        session_id: &str,
        key: &str,
        code: &str,
        vk: u32,
    ) -> Result<()> {
        self.client
            .send_on(
                session_id,
                "Input.dispatchKeyEvent",
                json!({ "type": "keyUp", "key": key, "code": code, "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk, "modifiers": 0 }),
            )
            .await?;
        Ok(())
    }

    async fn release_shift_on(&self, session_id: &str) -> Result<()> {
        self.client
            .send_on(
                session_id,
                "Input.dispatchKeyEvent",
                json!({ "type": "keyUp", "key": "Shift", "code": "ShiftLeft", "windowsVirtualKeyCode": 16, "nativeVirtualKeyCode": 16, "modifiers": 0 }),
            )
            .await?;
        Ok(())
    }

    /// Wait for the page to settle after an action: if a navigation starts,
    /// wait for its load; otherwise apply a short DOM grace period. This is the
    /// cheap "did something happen" signal the act tools read back.
    pub async fn settle(&self) {
        let mut rx = self.client.events();
        let sid = self.session_id.clone();
        // Phase 1: within a short window, detect whether a navigation began.
        let detected = tokio::time::timeout(Duration::from_millis(400), async {
            loop {
                match rx.recv().await {
                    Ok(ev) if ev.session_id.as_deref() == Some(&sid) => match ev.method.as_str() {
                        "Page.loadEventFired" => return Some(true),
                        "Page.frameStartedLoading"
                        | "Page.frameRequestedNavigation"
                        | "Page.navigatedWithinDocument" => return Some(false),
                        _ => {}
                    },
                    Ok(_) => {}
                    Err(_) => return None,
                }
            }
        })
        .await;

        match detected {
            Ok(Some(true)) => {}                                         // already loaded
            Ok(Some(false)) => self.wait_for_load().await.unwrap_or(()), // nav in flight
            _ => tokio::time::sleep(Duration::from_millis(350)).await,   // no nav: DOM grace
        }
    }

    /// Click an element inside an iframe with trusted browser-generated pointer
    /// input. `frame_selector` may chain multiple
    /// CSS selectors with `>>` to descend through nested iframes (e.g.
    /// `"iframe.wrapper >> iframe.popup"`). Same-origin frames are resolved
    /// with isolated-world DOM access; if a cross-origin boundary is
    /// hit, resolution automatically falls back to CDP (`Page.getFrameTree` +
    /// `Page.createIsolatedWorld`), which is not subject to the Same-Origin
    /// Policy. The resolved element box is translated into top-level viewport
    /// coordinates before CDP mouse press/release events are dispatched.
    pub async fn iframe_click(&self, frame_selector: &str, selector: &str) -> Result<()> {
        let chain = require_frame_chain(frame_selector)?;
        // The first pass brings every frame and the target into view. A deep
        // target scroll can move an ancestor browsing context, so resolve once
        // more after scrolling and dispatch only from the stable coordinates.
        self.descend_and_act_with_session(&chain, selector, &FrameAction::Point)
            .await?;
        let (point, session_id) = self
            .descend_and_act_with_session(&chain, selector, &FrameAction::Point)
            .await?;
        let x = point
            .get("x")
            .and_then(Value::as_f64)
            .ok_or_else(|| BrowserError::Protocol("iframe target has no viewport x".into()))?;
        let y = point
            .get("y")
            .and_then(Value::as_f64)
            .ok_or_else(|| BrowserError::Protocol("iframe target has no viewport y".into()))?;
        let half_width = point
            .get("halfWidth")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let half_height = point
            .get("halfHeight")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let dx = off_center(half_width);
        let dy = off_center(half_height);
        let root_point = (x + dx, y + dy);
        if session_id == self.session_id {
            self.trusted_click_at(root_point.0, root_point.1).await
        } else {
            let local_x = point
                .get("localX")
                .and_then(Value::as_f64)
                .ok_or_else(|| BrowserError::Protocol("iframe target has no local x".into()))?;
            let local_y = point
                .get("localY")
                .and_then(Value::as_f64)
                .ok_or_else(|| BrowserError::Protocol("iframe target has no local y".into()))?;
            self.trusted_frame_click_at(&session_id, root_point, (local_x + dx, local_y + dy))
                .await
        }
    }

    /// Focus an input inside a same-origin, cross-origin, or OOPIF iframe and
    /// type through CDP's Input domain. Controlled inputs receive the
    /// browser-generated keyboard/input event sequence.
    pub async fn iframe_type_text_cancellable(
        &self,
        frame_selector: &str,
        selector: &str,
        text: &str,
        clear: bool,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let chain = require_frame_chain(frame_selector)?;
        let (_, session_id) = self
            .descend_and_act_with_session(&chain, selector, &FrameAction::Focus { clear })
            .await?;
        self.dispatch_text_cancellable(&session_id, text, clear, cancel)
            .await
    }

    /// Read an element's `outerHTML` (`ReadMode::Html`) or rendered text
    /// (`ReadMode::Text`) from inside an iframe. `frame_selector` may be a
    /// single CSS selector or a `>>`-separated chain for nested iframes, and
    /// `selector` targets the element inside the innermost one (pass `"html"`
    /// to read the whole frame document). Same-origin and cross-origin
    /// frames are both supported — see `iframe_click` for the resolution
    /// mechanism, which this reuses verbatim.
    pub async fn iframe_read(
        &self,
        frame_selector: &str,
        selector: &str,
        mode: ReadMode,
    ) -> Result<String> {
        let chain = require_frame_chain(frame_selector)?;
        let result = self
            .descend_and_act(&chain, selector, &FrameAction::Read(mode))
            .await?;
        Ok(result
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string())
    }

    /// Descend through a chain of iframe selectors and perform `action` on
    /// `selector` inside the innermost one, returning the JS result object
    /// (`{ok:true, value?}` for `FrameAction::Read`; `{ok:true}` otherwise).
    /// Each hop first tries same-origin DOM access (`contentDocument`,
    /// cheap, one round trip per contiguous same-origin run). When that
    /// returns null — the SOP signature of a cross-origin frame — the
    /// boundary iframe element is resolved directly to its CDP `frameId` via
    /// `DOM.describeNode`; URL/name matching in `Page.getFrameTree` remains a
    /// compatibility fallback. A fresh isolated execution context is then
    /// created *inside that frame's own origin* via
    /// `Page.createIsolatedWorld`. CDP frame contexts are not subject to the
    /// Same-Origin Policy, so the loop can repeat across multiple origin
    /// boundaries until the full chain is consumed.
    async fn descend_and_act(
        &self,
        chain: &[&str],
        selector: &str,
        action: &FrameAction,
    ) -> Result<Value> {
        self.descend_and_act_with_session(chain, selector, action)
            .await
            .map(|(value, _)| value)
    }

    /// Variant of `descend_and_act` that also returns the CDP session owning
    /// the innermost frame. Keyboard input must be dispatched on that session
    /// when Site Isolation places the target frame in an OOPIF renderer.
    async fn descend_and_act_with_session(
        &self,
        chain: &[&str],
        selector: &str,
        action: &FrameAction,
    ) -> Result<(Value, String)> {
        let mut context: Option<FrameExecutionContext> = None;
        let mut remaining: Vec<&str> = chain.to_vec();
        let mut root_offset_x = 0.0;
        let mut root_offset_y = 0.0;
        // Bound the loop: at most one CDP hop per chain element, plus one.
        for _ in 0..=chain.len() {
            let js = build_descend_js(&remaining, selector, action);
            let mut result = match context.as_ref() {
                None => self.evaluate(&js).await?,
                Some(ctx) => self.eval_with_context(&js, ctx).await?,
            };
            if result.get("ok").and_then(Value::as_bool) == Some(true) {
                if matches!(action, FrameAction::Point) {
                    let local_x = result.get("x").and_then(Value::as_f64).ok_or_else(|| {
                        BrowserError::Protocol("iframe point result has no x".into())
                    })?;
                    let local_y = result.get("y").and_then(Value::as_f64).ok_or_else(|| {
                        BrowserError::Protocol("iframe point result has no y".into())
                    })?;
                    if let Some(object) = result.as_object_mut() {
                        object.insert("localX".into(), json!(local_x));
                        object.insert("localY".into(), json!(local_y));
                        object.insert("x".into(), json!(root_offset_x + local_x));
                        object.insert("y".into(), json!(root_offset_y + local_y));
                    }
                }
                let session_id = context
                    .as_ref()
                    .map(|ctx| ctx.session_id.clone())
                    .unwrap_or_else(|| self.session_id.clone());
                return Ok((result, session_id));
            }
            if matches!(action, FrameAction::Point) {
                root_offset_x += result.get("offsetX").and_then(Value::as_f64).unwrap_or(0.0);
                root_offset_y += result.get("offsetY").and_then(Value::as_f64).unwrap_or(0.0);
            }
            let index = result.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let src = result
                .get("src")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let name = result
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            // Resolve the exact DOM element first. Besides avoiding ambiguous
            // URL/name matching, this works when a nested frame is absent from
            // the frame tree returned for this CDP session.
            let frame_id = match self
                .frame_id_by_node(&remaining, index, context.as_ref())
                .await
            {
                Ok(id) => id,
                Err(node_err) if !name.is_empty() || !src.is_empty() => {
                    match self.frame_id_by_hint(&name, &src).await {
                        Ok(id) => id,
                        Err(hint_err) => {
                            return Err(BrowserError::Protocol(format!(
                                "cross-origin iframe could not be resolved from its DOM node: \
                                 {node_err}; frame-tree hint lookup also failed \
                                 (name={name:?}, src={src:?}): {hint_err}"
                            )));
                        }
                    }
                }
                Err(node_err) => {
                    return Err(BrowserError::Protocol(format!(
                        "cross-origin iframe `{}` has no name/src fallback and could not be \
                         resolved from its DOM node: {node_err}",
                        remaining.get(index).copied().unwrap_or("")
                    )));
                }
            };
            let parent_session = context
                .as_ref()
                .map(|ctx| ctx.session_id.as_str())
                .unwrap_or(&self.session_id);
            context = Some(
                self.create_isolated_world_for_frame(&frame_id, parent_session)
                    .await?,
            );
            remaining = remaining[index + 1..].to_vec();
        }
        Err(BrowserError::Protocol(
            "iframe descent exceeded max depth (possible frame-tree cycle)".into(),
        ))
    }

    /// Find a frame in the CDP frame tree by iframe `name` or `src` hint.
    ///
    /// `name` is matched exactly (CDP's `Frame.name` mirrors the iframe
    /// element's `name` attribute) and is preferred when present. `src`
    /// falls back to an exact match against the frame's resolved `url`
    /// first, then a substring match to tolerate relative-vs-absolute
    /// differences. This is not affected by cross-origin restrictions:
    /// `Page.getFrameTree` enumerates every frame regardless of origin.
    ///
    /// Known limitation: none of these hints are guaranteed unique. If two
    /// sibling iframes share a `name`/`url` (or a redirected/subsequently
    /// navigated frame's committed `url` no longer contains the iframe's
    /// original `src` attribute at all), this can fail to disambiguate or
    /// fail to match. Rather than silently guessing (and risking reading
    /// from or acting on the wrong origin), an ambiguous hint is treated as
    /// an error — see `IframeReadArgs`/tool docs for the `>>` chain
    /// workaround (make the hop more specific, e.g. by nesting further or
    /// giving the iframe a unique `name`/`id`).
    async fn frame_id_by_hint(&self, name: &str, src: &str) -> Result<String> {
        let tree = self
            .client
            .send_on(&self.session_id, "Page.getFrameTree", json!({}))
            .await?;
        let root = tree
            .get("frameTree")
            .ok_or_else(|| BrowserError::Protocol("no frame tree".into()))?;

        let mut frames = Vec::new();
        collect_frames(root, &mut frames);
        resolve_frame_id(&frames, name, src)
    }

    async fn create_world_in_session(&self, session_id: &str, frame_id: &str) -> Result<i64> {
        let world = self
            .client
            .send_on(
                session_id,
                "Page.createIsolatedWorld",
                json!({
                    "frameId": frame_id,
                    "worldName": "ab_cross_frame",
                    "grantUniveralAccess": false,
                }),
            )
            .await?;
        world
            .get("executionContextId")
            .and_then(Value::as_i64)
            .ok_or_else(|| {
                BrowserError::Protocol(
                    "failed to create isolated world for cross-origin frame".into(),
                )
            })
    }

    async fn attach_frame_target(&self, frame_id: &str) -> Result<String> {
        if let Some(session_id) = self.frame_sessions.lock().unwrap().get(frame_id).cloned() {
            return Ok(session_id);
        }

        // For an out-of-process iframe Chromium uses the frame token as the
        // iframe target id. A command scoped to the parent page session cannot
        // address that target's Page/Runtime domains, so attach a flattened
        // child session on the browser connection.
        let attached = self
            .client
            .send(
                "Target.attachToTarget",
                json!({ "targetId": frame_id, "flatten": true }),
            )
            .await?;
        let session_id = attached
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| BrowserError::Protocol("no OOPIF sessionId".into()))?
            .to_string();
        self.frame_sessions
            .lock()
            .unwrap()
            .insert(frame_id.to_string(), session_id.clone());
        Ok(session_id)
    }

    /// Create an isolated world for `frame_id`, routing the command to an
    /// OOPIF target session when the parent session does not own that frame.
    async fn create_isolated_world_for_frame(
        &self,
        frame_id: &str,
        parent_session: &str,
    ) -> Result<FrameExecutionContext> {
        match self.create_world_in_session(parent_session, frame_id).await {
            Ok(context_id) => Ok(FrameExecutionContext {
                session_id: parent_session.to_string(),
                context_id,
            }),
            Err(parent_err) => {
                let mut session_id = self.attach_frame_target(frame_id).await.map_err(|err| {
                    BrowserError::Protocol(format!(
                        "frame {frame_id:?} is not owned by parent CDP session: {parent_err}; \
                         attaching its OOPIF target also failed: {err}"
                    ))
                })?;

                let context_id = match self.create_world_in_session(&session_id, frame_id).await {
                    Ok(context_id) => context_id,
                    Err(cached_err) => {
                        // The cached session may have detached after a frame
                        // navigation. Drop it and attach a fresh target session.
                        self.frame_sessions.lock().unwrap().remove(frame_id);
                        session_id = self.attach_frame_target(frame_id).await?;
                        self.create_world_in_session(&session_id, frame_id)
                            .await
                            .map_err(|fresh_err| {
                                BrowserError::Protocol(format!(
                                    "failed to create an isolated world for OOPIF frame \
                                     {frame_id:?}; parent session: {parent_err}; cached child \
                                     session: {cached_err}; fresh child session: {fresh_err}"
                                ))
                            })?
                    }
                };
                Ok(FrameExecutionContext {
                    session_id,
                    context_id,
                })
            }
        }
    }

    /// Resolve a cross-origin iframe to its `frameId` by querying the iframe
    /// element from its *parent* document (the current execution context) and
    /// using `DOM.describeNode` to read the `frameId` directly.
    ///
    /// This bypasses `Page.getFrameTree` entirely and binds the selected DOM
    /// element to its frame without relying on non-unique URL/name hints.
    ///
    /// `chain[..=index]` is the path from the current execution context down
    /// to the cross-origin iframe element. The last hop (`chain[index]`) is the
    /// target iframe whose frameId we want; earlier hops are walked via
    /// same-origin `contentDocument` access (which the caller has already
    /// verified is possible).
    async fn frame_id_by_node(
        &self,
        chain: &[&str],
        index: usize,
        context: Option<&FrameExecutionContext>,
    ) -> Result<String> {
        let js = build_frame_element_js(chain, index);
        let res = self.eval_remote(&js, context).await?;
        let result = res
            .get("result")
            .ok_or_else(|| BrowserError::Protocol("DOM query returned no result".into()))?;
        let object_id = result
            .get("objectId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                BrowserError::Protocol(format!(
                    "iframe element `{}` did not return a remote object \
                     (type: {:?}, subtype: {:?}); the selector may not match \
                     an <iframe> element",
                    chain.get(index).copied().unwrap_or(""),
                    result.get("type").and_then(Value::as_str),
                    result.get("subtype").and_then(Value::as_str),
                ))
            })?;
        let session_id = context
            .map(|ctx| ctx.session_id.as_str())
            .unwrap_or(&self.session_id);
        let desc = self
            .client
            .send_on(
                session_id,
                "DOM.describeNode",
                json!({ "objectId": object_id }),
            )
            .await;
        // Runtime object handles otherwise remain alive until their context is
        // destroyed. Ignore release failures so they do not mask the result.
        let _ = self
            .client
            .send_on(
                session_id,
                "Runtime.releaseObject",
                json!({ "objectId": object_id }),
            )
            .await;
        let desc = desc?;
        desc.get("node")
            .and_then(|n| n.get("frameId"))
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| {
                BrowserError::Protocol(format!(
                    "iframe element `{}` resolved but DOM.describeNode returned no frameId \
                     (the element may not be an iframe, or it hasn't finished loading)",
                    chain.get(index).copied().unwrap_or("")
                ))
            })
    }

    /// Evaluate `expression` in a specific execution context (used for the
    /// isolated worlds created for cross-origin frames above).
    async fn eval_with_context(
        &self,
        expression: &str,
        context: &FrameExecutionContext,
    ) -> Result<Value> {
        let res = self
            .client
            .send_on(
                &context.session_id,
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "contextId": context.context_id,
                    "returnByValue": true,
                    "awaitPromise": true,
                }),
            )
            .await?;
        if let Some(exc) = res.get("exceptionDetails") {
            return Err(BrowserError::Protocol(format!("JS exception: {exc}")));
        }
        Ok(res
            .get("result")
            .and_then(|r| r.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// Like `eval_with_context` / `evaluate_main`, but with
    /// `returnByValue: false` so the result may include `objectId`
    /// references for non-primitive values (e.g. DOM elements).
    /// Used by `frame_id_by_node` to resolve an iframe element to its
    /// CDP frame id via `DOM.describeNode`.
    async fn eval_remote(
        &self,
        expression: &str,
        context: Option<&FrameExecutionContext>,
    ) -> Result<Value> {
        let mut params = json!({
            "expression": expression,
            "returnByValue": false,
            "awaitPromise": true,
        });
        if let Some(ctx) = context {
            params["contextId"] = json!(ctx.context_id);
        }
        let session_id = context
            .map(|ctx| ctx.session_id.as_str())
            .unwrap_or(&self.session_id);
        let res = self
            .client
            .send_on(session_id, "Runtime.evaluate", params)
            .await?;
        if let Some(exc) = res.get("exceptionDetails") {
            return Err(BrowserError::Protocol(format!("JS exception: {exc}")));
        }
        Ok(res)
    }

    /// Start capturing console messages. Enables the Runtime domain (a stealth
    /// tell), so this is opt-in. Captures messages from now on.
    pub async fn enable_console_log(&self) -> Result<ConsoleLog> {
        self.client
            .send_on(&self.session_id, "Runtime.enable", json!({}))
            .await?;
        let log = ConsoleLog::default();
        let mut rx = self.client.events();
        let sid = self.session_id.clone();
        let l = log.clone();
        tokio::spawn(async move {
            while let Ok(ev) = rx.recv().await {
                if ev.session_id.as_deref() != Some(&sid) {
                    continue;
                }
                match ev.method.as_str() {
                    "Runtime.consoleAPICalled" => {
                        let kind = ev
                            .params
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("log");
                        let args: Vec<String> = ev
                            .params
                            .get("args")
                            .and_then(Value::as_array)
                            .map(|a| {
                                a.iter()
                                    .map(|x| {
                                        x.get("value")
                                            .map(|v| v.to_string())
                                            .or_else(|| {
                                                x.get("description")
                                                    .and_then(Value::as_str)
                                                    .map(String::from)
                                            })
                                            .unwrap_or_default()
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        l.lines
                            .lock()
                            .unwrap()
                            .push(format!("[{kind}] {}", args.join(" ")));
                    }
                    "Runtime.exceptionThrown" => {
                        let txt = ev
                            .params
                            .get("exceptionDetails")
                            .and_then(|e| e.get("exception"))
                            .and_then(|e| e.get("description").or_else(|| e.get("value")))
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "exception".into());
                        l.lines.lock().unwrap().push(format!("[error] {txt}"));
                    }
                    _ => {}
                }
            }
        });
        Ok(log)
    }

    /// Focus an element by backend node id.
    pub async fn focus(&self, backend: i64) -> Result<()> {
        self.scroll_into_view(backend).await;
        self.client
            .send_on(
                &self.session_id,
                "DOM.focus",
                json!({ "backendNodeId": backend }),
            )
            .await?;
        Ok(())
    }

    /// Press a single named key (e.g. "Enter", "Tab", "Escape").
    pub async fn press(&self, key: &str) -> Result<()> {
        self.press_key_on(&self.session_id, key).await
    }

    async fn press_key_on(&self, session_id: &str, key: &str) -> Result<()> {
        let (event_key, code, vk) = match key {
            "Enter" => (key, "Enter", 13),
            "Tab" => (key, "Tab", 9),
            "Escape" => (key, "Escape", 27),
            "Backspace" => (key, "Backspace", 8),
            "ArrowDown" => (key, "ArrowDown", 40),
            "ArrowUp" => (key, "ArrowUp", 38),
            "Home" => (key, "Home", 36),
            "End" => (key, "End", 35),
            _ if key.chars().count() == 1 => {
                let (code, vk) = us_qwerty_key(key.chars().next().unwrap());
                (key, code, vk)
            }
            _ => (key, "", 0),
        };
        self.client
            .send_on(
                session_id,
                "Input.dispatchKeyEvent",
                json!({
                    "type": "keyDown",
                    "key": event_key,
                    "code": code,
                    "windowsVirtualKeyCode": vk,
                    "nativeVirtualKeyCode": vk,
                }),
            )
            .await?;
        tokio::time::sleep(Duration::from_millis(rand_u64(25, 95))).await;
        self.client
            .send_on(
                session_id,
                "Input.dispatchKeyEvent",
                json!({
                    "type": "keyUp",
                    "key": event_key,
                    "code": code,
                    "windowsVirtualKeyCode": vk,
                    "nativeVirtualKeyCode": vk,
                }),
            )
            .await?;
        tokio::time::sleep(Duration::from_millis(rand_u64(45, 140))).await;
        Ok(())
    }

    async fn type_select_search_on(&self, session_id: &str, label: &str) -> Result<()> {
        for ch in label.to_lowercase().chars() {
            let key = ch.to_string();
            let (code, vk) = us_qwerty_key(ch);
            let modifiers = if needs_shift(ch) { 8 } else { 0 };
            self.client
                .send_on(
                    session_id,
                    "Input.dispatchKeyEvent",
                    json!({
                        "type": "keyDown",
                        "text": key,
                        "key": key,
                        "unmodifiedText": key,
                        "code": code,
                        "windowsVirtualKeyCode": vk,
                        "nativeVirtualKeyCode": vk,
                        "modifiers": modifiers,
                    }),
                )
                .await?;
            tokio::time::sleep(Duration::from_millis(rand_u64(25, 85))).await;
            self.client
                .send_on(
                    session_id,
                    "Input.dispatchKeyEvent",
                    json!({ "type": "keyUp", "key": key, "code": code, "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk, "modifiers": modifiers }),
                )
                .await?;
            tokio::time::sleep(Duration::from_millis(rand_u64(35, 120))).await;
        }
        Ok(())
    }
}

/// More navigation / interaction primitives (parity with mature drivers).
impl Page {
    /// Select one enabled option using trusted browser-generated keyboard input.
    pub async fn select_option(&self, backend: i64, value: &str) -> Result<()> {
        self.scroll_into_view(backend).await;
        let frame_id = self.main_frame_id().await?;
        let context_id = self
            .create_world_in_session(&self.session_id, &frame_id)
            .await?;
        let obj = self
            .resolve_object_in_context(backend, context_id)
            .await?
            .ok_or_else(|| BrowserError::Protocol("cannot resolve element".into()))?;
        let result = async {
            let inspected = self
                .client
                .send_on(
                    &self.session_id,
                    "Runtime.callFunctionOn",
                    json!({
                        "objectId": obj,
                        "arguments": [{ "value": value }],
                        "functionDeclaration": "function(v){if(this.tagName!=='SELECT')return{error:'target is not a select'};if(this.disabled)return{error:'select is disabled'};if(this.multiple)return{error:'multiple select is not supported by trusted single-option input'};const enabled=Array.from(this.options).filter(o=>!o.disabled&&!(o.parentElement&&o.parentElement.tagName==='OPTGROUP'&&o.parentElement.disabled));const target=enabled.find(o=>o.value===v);if(!target)return{error:'enabled option value not found'};return{currentValue:this.value,targetLabel:target.label||target.textContent||''};}",
                        "returnByValue": true,
                    }),
                )
                .await?;
            if let Some(exception) = inspected.get("exceptionDetails") {
                return Err(BrowserError::Protocol(format!(
                    "select inspection failed: {exception}"
                )));
            }
            let metadata = inspected
                .pointer("/result/value")
                .ok_or_else(|| BrowserError::Protocol("select inspection returned no value".into()))?;
            if let Some(error) = metadata.get("error").and_then(Value::as_str) {
                return Err(BrowserError::Protocol(error.into()));
            }
            if metadata.get("currentValue").and_then(Value::as_str) == Some(value) {
                return Ok(());
            }
            let target_label = metadata
                .get("targetLabel")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|label| !label.is_empty())
                .ok_or_else(|| BrowserError::Protocol("select target label is missing".into()))?;
            if target_label.chars().count() > 200 {
                return Err(BrowserError::Protocol(
                    "select target label is too long for trusted type-ahead".into(),
                ));
            }

            self.client
                .send_on(
                    &self.session_id,
                    "DOM.focus",
                    json!({ "backendNodeId": backend }),
                )
                .await?;
            self.type_select_search_on(&self.session_id, target_label)
                .await?;

            let verified = self
                .client
                .send_on(
                    &self.session_id,
                    "Runtime.callFunctionOn",
                    json!({
                        "objectId": obj,
                        "functionDeclaration": "function(){return this.value;}",
                        "returnByValue": true,
                    }),
                )
                .await?;
            let actual = verified.pointer("/result/value").and_then(Value::as_str);
            if actual != Some(value) {
                return Err(BrowserError::Protocol(format!(
                    "trusted select input chose {:?}, expected {value:?}",
                    actual.unwrap_or("")
                )));
            }
            Ok(())
        }
        .await;
        let _ = self
            .client
            .send_on(
                &self.session_id,
                "Runtime.releaseObject",
                json!({ "objectId": obj }),
            )
            .await;
        result
    }

    /// Install a CDP virtual authenticator so WebAuthn / passkey prompts are
    /// handled programmatically instead of blocking on the native OS passkey
    /// dialog (which captures input and stalls automation). With no registered
    /// credential, `navigator.credentials.get()` fails fast, so sites fall back
    /// to another sign-in method (e.g. password) instead of hanging. Returns the
    /// authenticatorId. `transport` is typically "internal" (platform passkey).
    pub async fn webauthn_enable(
        &self,
        transport: &str,
        user_verified: bool,
        resident_key: bool,
    ) -> Result<String> {
        self.client
            .send_on(
                &self.session_id,
                "WebAuthn.enable",
                json!({ "enableUI": false }),
            )
            .await?;
        let res = self
            .client
            .send_on(
                &self.session_id,
                "WebAuthn.addVirtualAuthenticator",
                json!({
                    "options": {
                        "protocol": "ctap2",
                        "transport": transport,
                        "hasResidentKey": resident_key,
                        "hasUserVerification": true,
                        "isUserVerified": user_verified,
                        "automaticPresenceSimulation": true,
                    }
                }),
            )
            .await?;
        Ok(res
            .get("authenticatorId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string())
    }

    /// Navigate back one entry in the tab's history and wait for load.
    pub async fn go_back(&self) -> Result<()> {
        let hist = self
            .client
            .send_on(&self.session_id, "Page.getNavigationHistory", json!({}))
            .await?;
        let idx = hist
            .get("currentIndex")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if idx <= 0 {
            return Ok(());
        }
        let entries = hist.get("entries").and_then(Value::as_array);
        if let Some(entry) = entries.and_then(|e| e.get((idx - 1) as usize)) {
            if let Some(id) = entry.get("id").and_then(Value::as_i64) {
                self.client
                    .send_on(
                        &self.session_id,
                        "Page.navigateToHistoryEntry",
                        json!({ "entryId": id }),
                    )
                    .await?;
                let _ = self.wait_for_load().await;
            }
        }
        Ok(())
    }

    /// Poll until `text` appears in the page (or timeout). Returns whether found.
    pub async fn wait_for_text(&self, text: &str, timeout_ms: u64) -> Result<bool> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        let expr = format!(
            "(document.body ? document.body.innerText : '').includes({})",
            serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into())
        );
        loop {
            if self.evaluate(&expr).await?.as_bool().unwrap_or(false) {
                return Ok(true);
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }

    /// Poll until a CSS selector matches (or timeout). Returns whether found.
    pub async fn wait_for_selector(&self, selector: &str, timeout_ms: u64) -> Result<bool> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        let expr = format!(
            "!!document.querySelector({})",
            serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into())
        );
        loop {
            if self.evaluate(&expr).await?.as_bool().unwrap_or(false) {
                return Ok(true);
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }

    /// Enable the Network domain and start collecting request/response events
    /// into a `NetworkLog`. Network.enable is not page-observable (unlike
    /// Runtime.enable), so this is safe for stealth.
    pub async fn enable_network_log(&self) -> Result<NetworkLog> {
        self.client
            .send_on(&self.session_id, "Network.enable", json!({}))
            .await?;
        let log = NetworkLog::default();
        let mut rx = self.client.events();
        let sid = self.session_id.clone();
        let l = log.clone();
        tokio::spawn(async move {
            while let Ok(ev) = rx.recv().await {
                if ev.session_id.as_deref() != Some(&sid) {
                    continue;
                }
                let p = &ev.params;
                let rid = p.get("requestId").and_then(Value::as_str);
                match ev.method.as_str() {
                    "Network.requestWillBeSent" => {
                        if let (Some(id), Some(req)) = (rid, p.get("request")) {
                            let entry = NetEntry {
                                url: req
                                    .get("url")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                method: req
                                    .get("method")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                resource_type: p
                                    .get("type")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                status: None,
                                failed: false,
                            };
                            let mut st = l.state.lock().unwrap();
                            let idx = st.entries.len();
                            st.entries.push(entry);
                            st.index.insert(id.to_string(), idx);
                        }
                    }
                    "Network.responseReceived" => {
                        if let Some(id) = rid {
                            let status = p
                                .get("response")
                                .and_then(|r| r.get("status"))
                                .and_then(Value::as_i64);
                            let mut st = l.state.lock().unwrap();
                            if let Some(&idx) = st.index.get(id) {
                                if let Some(e) = st.entries.get_mut(idx) {
                                    e.status = status;
                                }
                            }
                        }
                    }
                    "Network.loadingFailed" => {
                        if let Some(id) = rid {
                            let mut st = l.state.lock().unwrap();
                            if let Some(&idx) = st.index.get(id) {
                                if let Some(e) = st.entries.get_mut(idx) {
                                    e.failed = true;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        });
        Ok(log)
    }

    /// Block requests whose URL matches any of the given wildcard patterns
    /// (e.g. "*.png", "*doubleclick*"). Uses Network.setBlockedURLs.
    pub async fn set_blocked_urls(&self, patterns: &[String]) -> Result<()> {
        self.client
            .send_on(&self.session_id, "Network.enable", json!({}))
            .await?;
        self.client
            .send_on(
                &self.session_id,
                "Network.setBlockedURLs",
                json!({ "urls": patterns }),
            )
            .await?;
        Ok(())
    }

    /// Clear all blocked-URL patterns.
    pub async fn clear_blocked_urls(&self) -> Result<()> {
        self.client
            .send_on(
                &self.session_id,
                "Network.setBlockedURLs",
                json!({ "urls": [] }),
            )
            .await?;
        Ok(())
    }

    /// Toggle offline emulation.
    pub async fn set_offline(&self, offline: bool) -> Result<()> {
        self.client
            .send_on(&self.session_id, "Network.enable", json!({}))
            .await?;
        self.client
            .send_on(
                &self.session_id,
                "Network.emulateNetworkConditions",
                json!({
                    "offline": offline,
                    "latency": 0,
                    "downloadThroughput": -1,
                    "uploadThroughput": -1,
                }),
            )
            .await?;
        Ok(())
    }

    /// Make an HTTP request from the page context (uses the page's cookies/session).
    /// Returns `{ status, ok, body }` (body truncated). Runs in the main world so
    /// same-origin credentials apply.
    pub async fn api_request(
        &self,
        url: &str,
        method: &str,
        headers: &Value,
        body: Option<&str>,
    ) -> Result<Value> {
        let opts = json!({
            "method": method,
            "headers": headers,
            "body": body,
            "credentials": "include",
        });
        let js = format!(
            r#"(async () => {{
              try {{
                const o = {opts};
                if (o.body == null) delete o.body;
                const r = await fetch({url}, o);
                const text = await r.text();
                return JSON.stringify({{ status: r.status, ok: r.ok, body: text.slice(0, 40000) }});
              }} catch (e) {{ return JSON.stringify({{ error: String(e) }}); }}
            }})()"#,
            opts = opts,
            url = serde_json::to_string(url).unwrap_or_else(|_| "\"\"".into()),
        );
        self.evaluate_main(&js).await
    }

    /// Set files on a file `<input>` by backend node id.
    pub async fn upload_files(&self, backend: i64, paths: &[String]) -> Result<()> {
        self.client
            .send_on(
                &self.session_id,
                "DOM.setFileInputFiles",
                json!({ "files": paths, "backendNodeId": backend }),
            )
            .await?;
        Ok(())
    }

    /// All cookies (browser-wide), as the CDP cookie array.
    pub async fn cookies(&self) -> Result<Value> {
        let r = self
            .client
            .send_on(&self.session_id, "Network.getAllCookies", json!({}))
            .await?;
        Ok(r.get("cookies").cloned().unwrap_or_else(|| json!([])))
    }

    /// Restore cookies from a CDP cookie array.
    pub async fn set_cookies(&self, cookies: &Value) -> Result<()> {
        self.client
            .send_on(&self.session_id, "Network.enable", json!({}))
            .await?;
        self.client
            .send_on(
                &self.session_id,
                "Network.setCookies",
                json!({ "cookies": cookies }),
            )
            .await?;
        Ok(())
    }

    /// Set a single cookie from a CDP cookie object (name/value + url or domain).
    pub async fn cookie_set(&self, cookie: &Value) -> Result<()> {
        self.client
            .send_on(&self.session_id, "Network.enable", json!({}))
            .await?;
        self.client
            .send_on(&self.session_id, "Network.setCookie", cookie.clone())
            .await?;
        Ok(())
    }

    /// Delete cookies matching name (+ optional domain/path).
    pub async fn cookie_delete(
        &self,
        name: &str,
        domain: Option<&str>,
        path: Option<&str>,
    ) -> Result<()> {
        let mut p = json!({ "name": name });
        if let Some(d) = domain {
            p["domain"] = json!(d);
        }
        if let Some(pp) = path {
            p["path"] = json!(pp);
        }
        self.client
            .send_on(&self.session_id, "Network.deleteCookies", p)
            .await?;
        Ok(())
    }

    /// Clear all browser cookies.
    pub async fn cookie_clear(&self) -> Result<()> {
        self.client
            .send_on(&self.session_id, "Network.clearBrowserCookies", json!({}))
            .await?;
        Ok(())
    }

    // --- Web storage (localStorage / sessionStorage). `kind` is validated by
    // the caller (only "localStorage" | "sessionStorage"), so it's injection-safe.
    pub async fn web_storage_get(&self, kind: &str, key: &str) -> Result<Value> {
        self.evaluate_main(&format!(
            "window.{kind}.getItem({})",
            serde_json::to_string(key).unwrap_or_else(|_| "\"\"".into())
        ))
        .await
    }
    pub async fn web_storage_set(&self, kind: &str, key: &str, value: &str) -> Result<()> {
        self.evaluate_main(&format!(
            "window.{kind}.setItem({},{})",
            serde_json::to_string(key).unwrap_or_else(|_| "\"\"".into()),
            serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into())
        ))
        .await?;
        Ok(())
    }
    pub async fn web_storage_list(&self, kind: &str) -> Result<Value> {
        self.evaluate_main(&format!(
            "JSON.parse(JSON.stringify(Object.fromEntries(Object.entries(window.{kind}))))"
        ))
        .await
    }
    pub async fn web_storage_delete(&self, kind: &str, key: &str) -> Result<()> {
        self.evaluate_main(&format!(
            "window.{kind}.removeItem({})",
            serde_json::to_string(key).unwrap_or_else(|_| "\"\"".into())
        ))
        .await?;
        Ok(())
    }
    pub async fn web_storage_clear(&self, kind: &str) -> Result<()> {
        self.evaluate_main(&format!("window.{kind}.clear()"))
            .await?;
        Ok(())
    }

    /// localStorage of the current origin as a `{ key: value }` object.
    pub async fn local_storage(&self) -> Result<Value> {
        self.evaluate_main(
            "JSON.parse(JSON.stringify(Object.fromEntries(Object.entries(localStorage))))",
        )
        .await
    }

    /// Restore localStorage for the current origin from a `{ key: value }` object.
    pub async fn set_local_storage(&self, data: &Value) -> Result<()> {
        let script = format!(
            "(() => {{ const d = {}; for (const k in d) try {{ localStorage.setItem(k, d[k]); }} catch(_){{}} }})()",
            serde_json::to_string(data).unwrap_or_else(|_| "{}".into())
        );
        self.evaluate_main(&script).await?;
        Ok(())
    }

    /// Render the page to a PDF (bytes). Note: Chrome only supports printToPDF
    /// in headless mode; in headful this returns a protocol error.
    pub async fn pdf(&self) -> Result<Vec<u8>> {
        let res = self
            .client
            .send_on(
                &self.session_id,
                "Page.printToPDF",
                json!({ "printBackground": true }),
            )
            .await?;
        let b64 = res
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| BrowserError::Protocol("no pdf data".into()))?;
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| BrowserError::Protocol(e.to_string()))
    }

    /// Full serialized HTML of the current document.
    pub async fn html(&self) -> Result<String> {
        Ok(self
            .evaluate("document.documentElement.outerHTML")
            .await?
            .as_str()
            .unwrap_or("")
            .to_string())
    }

    /// Current document title.
    pub async fn title(&self) -> Result<String> {
        Ok(self
            .evaluate("document.title")
            .await?
            .as_str()
            .unwrap_or("")
            .to_string())
    }

    /// Resize the page's viewport via device-metrics override.
    pub async fn resize(&self, width: u32, height: u32) -> Result<()> {
        self.client
            .send_on(
                &self.session_id,
                "Emulation.setDeviceMetricsOverride",
                json!({
                    "width": width,
                    "height": height,
                    "deviceScaleFactor": 1,
                    "mobile": false,
                }),
            )
            .await?;
        Ok(())
    }

    /// Start the explicit JavaScript-dialog handler. Idempotent per page.
    pub async fn enable_dialog_handler(&self) -> Result<bool> {
        if self.dialog_handler_started.swap(true, Ordering::AcqRel) {
            return Ok(false);
        }
        if let Err(error) = self
            .client
            .send_on(&self.session_id, "Page.enable", json!({}))
            .await
        {
            self.dialog_handler_started.store(false, Ordering::Release);
            return Err(error.into());
        }
        let mut rx = self.client.events();
        let sid = self.session_id.clone();
        let client = self.client.clone();
        let policy = self.dialog.clone();
        tokio::spawn(async move {
            while let Ok(ev) = rx.recv().await {
                if ev.session_id.as_deref() == Some(&sid)
                    && ev.method == "Page.javascriptDialogOpening"
                {
                    // Snapshot the policy without holding the lock across await.
                    let (accept, text) = {
                        let p = policy.lock().unwrap();
                        (p.0, p.1.clone())
                    };
                    let mut params = json!({ "accept": accept });
                    if let Some(t) = text {
                        params["promptText"] = json!(t);
                    }
                    let _ = client
                        .send_on(&sid, "Page.handleJavaScriptDialog", params)
                        .await;
                }
            }
        });
        Ok(true)
    }

    /// Set how the next JS dialogs (alert/confirm/prompt/beforeunload) are
    /// handled: accept vs dismiss, plus optional prompt text.
    pub fn set_dialog_policy(&self, accept: bool, prompt_text: Option<String>) {
        *self.dialog.lock().unwrap() = (accept, prompt_text);
    }

    pub fn dialog_handler_enabled(&self) -> bool {
        self.dialog_handler_started.load(Ordering::Acquire)
    }

    /// Draw a highlight box over an element (debug/inspection aid).
    pub async fn highlight(&self, backend: i64) -> Result<()> {
        self.scroll_into_view(backend).await;
        let Some(obj) = self.resolve_object(backend).await? else {
            return Err(BrowserError::Protocol("cannot resolve element".into()));
        };
        self.client
            .send_on(
                &self.session_id,
                "Runtime.callFunctionOn",
                json!({
                    "objectId": obj,
                    "functionDeclaration": r#"function(){
                        const r = this.getBoundingClientRect();
                        let o = document.getElementById('__brs_hl');
                        if (!o) { o = document.createElement('div'); o.id = '__brs_hl'; document.body.appendChild(o); }
                        o.style.cssText = 'position:fixed;pointer-events:none;z-index:2147483647;box-sizing:border-box;'
                          + 'border:2px solid #ff3b30;background:rgba(255,59,48,0.15);'
                          + `left:${r.left}px;top:${r.top}px;width:${r.width}px;height:${r.height}px`;
                    }"#,
                }),
            )
            .await?;
        Ok(())
    }

    /// Remove the highlight box.
    pub async fn hide_highlight(&self) -> Result<()> {
        self.evaluate("(()=>{ const o=document.getElementById('__brs_hl'); if(o) o.remove(); return true; })()")
            .await?;
        Ok(())
    }

    /// Mock all requests whose URL matches `pattern` (CDP wildcard, e.g.
    /// "*/api/*") with a canned response. Uses the Fetch domain; the intercept
    /// loop is started once and consults the accumulated mock rules.
    pub async fn route_mock(
        &self,
        pattern: &str,
        status: i64,
        body: &str,
        content_type: &str,
    ) -> Result<()> {
        // Register/replace the rule for this pattern.
        let start_loop = {
            let mut st = self.routes.lock().unwrap();
            st.mocks.retain(|m| m.pattern != pattern);
            st.mocks.push(RouteMock {
                pattern: pattern.to_string(),
                status,
                body: body.to_string(),
                content_type: content_type.to_string(),
            });
            let first = !st.loop_started;
            st.loop_started = true;
            first
        };

        // (Re)enable Fetch with the union of all mock patterns.
        let patterns: Vec<Value> = {
            let st = self.routes.lock().unwrap();
            st.mocks
                .iter()
                .map(|m| json!({ "urlPattern": m.pattern }))
                .collect()
        };
        self.client
            .send_on(
                &self.session_id,
                "Fetch.enable",
                json!({ "patterns": patterns }),
            )
            .await?;

        if start_loop {
            let mut rx = self.client.events();
            let sid = self.session_id.clone();
            let client = self.client.clone();
            let routes = self.routes.clone();
            tokio::spawn(async move {
                use base64::Engine;
                while let Ok(ev) = rx.recv().await {
                    if ev.session_id.as_deref() != Some(&sid) || ev.method != "Fetch.requestPaused"
                    {
                        continue;
                    }
                    let request_id = ev
                        .params
                        .get("requestId")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let url = ev
                        .params
                        .get("request")
                        .and_then(|r| r.get("url"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    // Find a matching mock (simple wildcard: '*' = any run).
                    let hit = {
                        let st = routes.lock().unwrap();
                        st.mocks
                            .iter()
                            .find(|m| wildcard_match(&m.pattern, url))
                            .cloned()
                    };
                    match hit {
                        Some(m) => {
                            let b64 =
                                base64::engine::general_purpose::STANDARD.encode(m.body.as_bytes());
                            let _ = client
                                .send_on(
                                    &sid,
                                    "Fetch.fulfillRequest",
                                    json!({
                                        "requestId": request_id,
                                        "responseCode": m.status,
                                        "responseHeaders": [
                                            { "name": "Content-Type", "value": m.content_type },
                                            { "name": "Access-Control-Allow-Origin", "value": "*" }
                                        ],
                                        "body": b64,
                                    }),
                                )
                                .await;
                        }
                        None => {
                            let _ = client
                                .send_on(
                                    &sid,
                                    "Fetch.continueRequest",
                                    json!({ "requestId": request_id }),
                                )
                                .await;
                        }
                    }
                }
            });
        }
        Ok(())
    }

    /// Drop all mock rules and disable request interception.
    pub async fn clear_routes(&self) -> Result<()> {
        {
            let mut st = self.routes.lock().unwrap();
            st.mocks.clear();
        }
        let _ = self
            .client
            .send_on(&self.session_id, "Fetch.disable", json!({}))
            .await;
        Ok(())
    }
}

fn activation_verified(visibility: &str, window_focused: bool) -> bool {
    visibility == "visible" && window_focused
}

/// Persistent per-user profile directory (aged profiles look human). Override
/// with `AB_PROFILE`. We deliberately avoid a throwaway temp dir.
fn default_profile_dir() -> Result<PathBuf> {
    let base = std::env::var("AB_PROFILE")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".browser-rs").join("profile"))
        })
        .ok_or_else(|| BrowserError::Launch("cannot resolve profile dir; set AB_PROFILE".into()))?;
    std::fs::create_dir_all(&base).map_err(|e| BrowserError::Launch(e.to_string()))?;
    Ok(base)
}

/// Cheap non-crypto randomness for input jitter (no extra dependency). Seeded
/// from the clock, xorshift-mixed — plenty for humanizing timings/paths.
fn rand_u64(min: u64, max: u64) -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut x = n
        .wrapping_mul(2654435761)
        .wrapping_add(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 13;
    x ^= x << 7;
    x ^= x >> 17;
    if max <= min {
        min
    } else {
        min + (x % (max - min + 1))
    }
}

/// Random offset in [-spread, +spread].
fn rand_f64(spread: f64) -> f64 {
    let r = rand_u64(0, 10_000) as f64 / 10_000.0; // 0..1
    (r * 2.0 - 1.0) * spread
}

/// Whether producing this character on a US keyboard requires the Shift key
/// (uppercase letters and the shifted symbol row). Typing e.g. '@' without a
/// modifier is physically impossible for a human — a behavioral tell.
fn needs_shift(ch: char) -> bool {
    ch.is_ascii_uppercase() || "~!@#$%^&*()_+{}|:\"<>?".contains(ch)
}

fn us_qwerty_key(ch: char) -> (&'static str, u32) {
    match ch {
        'a' | 'A' => ("KeyA", 65),
        'b' | 'B' => ("KeyB", 66),
        'c' | 'C' => ("KeyC", 67),
        'd' | 'D' => ("KeyD", 68),
        'e' | 'E' => ("KeyE", 69),
        'f' | 'F' => ("KeyF", 70),
        'g' | 'G' => ("KeyG", 71),
        'h' | 'H' => ("KeyH", 72),
        'i' | 'I' => ("KeyI", 73),
        'j' | 'J' => ("KeyJ", 74),
        'k' | 'K' => ("KeyK", 75),
        'l' | 'L' => ("KeyL", 76),
        'm' | 'M' => ("KeyM", 77),
        'n' | 'N' => ("KeyN", 78),
        'o' | 'O' => ("KeyO", 79),
        'p' | 'P' => ("KeyP", 80),
        'q' | 'Q' => ("KeyQ", 81),
        'r' | 'R' => ("KeyR", 82),
        's' | 'S' => ("KeyS", 83),
        't' | 'T' => ("KeyT", 84),
        'u' | 'U' => ("KeyU", 85),
        'v' | 'V' => ("KeyV", 86),
        'w' | 'W' => ("KeyW", 87),
        'x' | 'X' => ("KeyX", 88),
        'y' | 'Y' => ("KeyY", 89),
        'z' | 'Z' => ("KeyZ", 90),
        '0' | ')' => ("Digit0", 48),
        '1' | '!' => ("Digit1", 49),
        '2' | '@' => ("Digit2", 50),
        '3' | '#' => ("Digit3", 51),
        '4' | '$' => ("Digit4", 52),
        '5' | '%' => ("Digit5", 53),
        '6' | '^' => ("Digit6", 54),
        '7' | '&' => ("Digit7", 55),
        '8' | '*' => ("Digit8", 56),
        '9' | '(' => ("Digit9", 57),
        ' ' => ("Space", 32),
        '-' | '_' => ("Minus", 189),
        '=' | '+' => ("Equal", 187),
        '[' | '{' => ("BracketLeft", 219),
        ']' | '}' => ("BracketRight", 221),
        '\\' | '|' => ("Backslash", 220),
        ';' | ':' => ("Semicolon", 186),
        '\'' | '"' => ("Quote", 222),
        ',' | '<' => ("Comma", 188),
        '.' | '>' => ("Period", 190),
        '/' | '?' => ("Slash", 191),
        '`' | '~' => ("Backquote", 192),
        _ => ("", 0),
    }
}

// Short strings are typed key-by-key; long strings are more plausibly pasted.
const INSERT_TEXT_THRESHOLD: usize = 30;

fn uses_insert_text(text: &str) -> bool {
    text.chars().count() >= INSERT_TEXT_THRESHOLD
}

fn ensure_typing_active(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(BrowserError::TypingCancelled)
    } else {
        Ok(())
    }
}

async fn typing_delay(cancel: &CancellationToken, millis: u64) -> Result<()> {
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_millis(millis)) => Ok(()),
        _ = cancel.cancelled() => Err(BrowserError::TypingCancelled),
    }
}

/// A signed offset that is clearly *off* the center of a box axis: 12–40 % of
/// the half-dimension, so clicks land inside the element but never on its exact
/// center (which behavioral detectors flag).
fn off_center(half: f64) -> f64 {
    if half < 3.0 {
        return rand_f64(half.max(0.0));
    }
    let frac = 0.12 + (rand_u64(0, 28) as f64) / 100.0; // 0.12..0.40
    let sign = if rand_u64(0, 1) == 0 { -1.0 } else { 1.0 };
    sign * frac * half
}

/// Minimal glob matcher supporting `*` (matches any run, including empty), to
/// pick which mock rule a paused request URL belongs to. Mirrors the CDP
/// Fetch `urlPattern` wildcard semantics closely enough for routing.
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let mut pos = 0usize;
    let first = parts[0];
    if !text.starts_with(first) {
        return false;
    }
    pos += first.len();
    for seg in &parts[1..parts.len() - 1] {
        if seg.is_empty() {
            continue;
        }
        match text[pos..].find(seg) {
            Some(i) => pos += i + seg.len(),
            None => return false,
        }
    }
    let last = parts[parts.len() - 1];
    last.is_empty() || text[pos..].ends_with(last)
}

fn detect_chrome() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("AB_CHROME") {
        return Some(PathBuf::from(p));
    }
    let candidates = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/BrowserOS.app/Contents/MacOS/BrowserOS",
        "/usr/bin/google-chrome",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
    ];
    candidates.iter().map(PathBuf::from).find(|p| p.exists())
}

/// Chrome writes the chosen debugging port to `<user-data-dir>/DevToolsActivePort`.
async fn read_active_port(data_dir: &std::path::Path) -> Result<u16> {
    let path = data_dir.join("DevToolsActivePort");
    for _ in 0..100 {
        if let Ok(contents) = tokio::fs::read_to_string(&path).await {
            if let Some(line) = contents.lines().next() {
                if let Ok(port) = line.trim().parse::<u16>() {
                    return Ok(port);
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(BrowserError::Discovery(
        "DevToolsActivePort not written in time".into(),
    ))
}

async fn discover_ws_url(port: u16) -> Result<String> {
    let url = format!("http://127.0.0.1:{port}/json/version");
    for _ in 0..50 {
        if let Ok(resp) = reqwest::get(&url).await {
            if let Ok(v) = resp.json::<Value>().await {
                if let Some(ws) = v.get("webSocketDebuggerUrl").and_then(Value::as_str) {
                    return Ok(ws.to_string());
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(BrowserError::Discovery(format!(
        "no webSocketDebuggerUrl at {url}"
    )))
}

#[cfg(test)]
mod tests {
    use super::{
        activation_verified, ax_hit_has_backend_ancestor, build_descend_js, build_frame_element_js,
        require_frame_chain, resolve_frame_id, split_frame_chain, us_qwerty_key, uses_insert_text,
        FrameAction, ReadMode,
    };

    #[test]
    fn long_text_uses_atomic_trusted_insertion_by_unicode_character_count() {
        assert!(!uses_insert_text(&"한".repeat(29)));
        assert!(uses_insert_text(&"한".repeat(30)));
        assert!(uses_insert_text(&"a".repeat(30)));
    }

    #[test]
    fn us_qwerty_mapping_covers_shifted_and_unshifted_keys() {
        assert_eq!(us_qwerty_key('a'), ("KeyA", 65));
        assert_eq!(us_qwerty_key('A'), ("KeyA", 65));
        assert_eq!(us_qwerty_key('1'), ("Digit1", 49));
        assert_eq!(us_qwerty_key('!'), ("Digit1", 49));
        assert_eq!(us_qwerty_key('?'), ("Slash", 191));
        assert_eq!(us_qwerty_key('한'), ("", 0));
    }

    #[test]
    fn activation_requires_visible_and_focused_document() {
        assert!(activation_verified("visible", true));
        assert!(!activation_verified("visible", false));
        assert!(!activation_verified("hidden", true));
    }

    #[test]
    fn ax_relatives_follow_ancestors_without_accepting_siblings() {
        let nodes = serde_json::json!([
            { "nodeId": "hit", "parentId": "target", "backendDOMNodeId": 21 },
            { "nodeId": "target", "parentId": "root", "backendDOMNodeId": 10 },
            { "nodeId": "sibling", "parentId": "target", "backendDOMNodeId": 99 },
            { "nodeId": "root", "backendDOMNodeId": 1 }
        ]);
        let nodes = nodes.as_array().unwrap();

        assert!(ax_hit_has_backend_ancestor(nodes, 21, 10));
        assert!(ax_hit_has_backend_ancestor(nodes, 21, 1));
        assert!(!ax_hit_has_backend_ancestor(nodes, 21, 99));
        assert!(!ax_hit_has_backend_ancestor(nodes, 21, 404));
    }

    #[test]
    fn frame_chain_splits_on_double_arrow_and_trims_whitespace() {
        assert_eq!(split_frame_chain("iframe.a"), vec!["iframe.a"]);
        assert_eq!(
            split_frame_chain("iframe.wrapper >> iframe.popup"),
            vec!["iframe.wrapper", "iframe.popup"]
        );
        assert_eq!(
            split_frame_chain("  iframe.a  >>iframe.b>>  iframe.c "),
            vec!["iframe.a", "iframe.b", "iframe.c"]
        );
    }

    #[test]
    fn frame_chain_ignores_empty_segments() {
        // Defensive: a stray leading/trailing/double ">>" shouldn't produce
        // empty selectors that would blow up `document.querySelector("")`.
        assert_eq!(split_frame_chain(">> iframe.a >>"), vec!["iframe.a"]);
        assert_eq!(split_frame_chain(""), Vec::<&str>::new());
    }

    #[test]
    fn descend_js_locates_iframe_target_without_synthetic_clicks() {
        let js = build_descend_js(&["iframe#f"], "button#go", &FrameAction::Point);
        assert!(js.contains(r#"["iframe#f"]"#));
        assert!(js.contains(r#"querySelector("button#go")"#));
        assert!(js.contains("getBoundingClientRect"));
        assert!(js.contains("offsetX"));
        assert!(!js.contains("el.click();"));
        assert!(!js.contains("dispatchEvent"));
        assert!(js.contains("ok: false"));
        assert!(js.contains("ok:true"));
    }

    #[test]
    fn point_lookup_on_current_document_returns_a_box_only() {
        let js = build_descend_js(&[], "button#go", &FrameAction::Point);
        assert!(js.contains(r#"const chain = [];"#));
        assert!(js.contains("halfWidth"));
        assert!(js.contains("halfHeight"));
        assert!(!js.contains("dispatchEvent"));
    }

    #[test]
    fn descend_js_focus_selects_only_when_clear_is_requested() {
        let append = build_descend_js(
            &["iframe#f"],
            "input#phone",
            &FrameAction::Focus { clear: false },
        );
        assert!(append.contains("el.focus();"));
        assert!(!append.contains("el.select()"));

        let replace = build_descend_js(
            &["iframe#f"],
            "input#phone",
            &FrameAction::Focus { clear: true },
        );
        assert!(replace.contains("el.focus();"));
        assert!(replace.contains("el.select()"));
        assert!(!replace.contains("el.value="));
        assert!(!replace.contains("dispatchEvent"));
    }

    #[test]
    fn descend_js_read_html_returns_outer_html() {
        let js = build_descend_js(&["iframe#f"], "body", &FrameAction::Read(ReadMode::Html));
        assert!(js.contains("value: el.outerHTML"));
        assert!(js.contains("ok: true"));
    }

    #[test]
    fn descend_js_read_text_falls_back_to_text_content() {
        let js = build_descend_js(&[], "#result", &FrameAction::Read(ReadMode::Text));
        assert!(js.contains("el.innerText"));
        assert!(js.contains("el.textContent"));
    }

    #[test]
    fn frame_element_js_uses_path_relative_to_current_context() {
        let js = build_frame_element_js(&["iframe.same-origin", "iframe.cross-origin"], 1);
        assert!(js.contains(r#"["iframe.same-origin","iframe.cross-origin"]"#));

        // After crossing the first origin boundary, callers pass only the
        // remaining chain. The generated query must not restart at the page's
        // original top-level selector.
        let js = build_frame_element_js(&["iframe.inner-wrapper", "iframe.payment"], 1);
        assert!(js.contains(r#"["iframe.inner-wrapper","iframe.payment"]"#));
        assert!(!js.contains("iframe.same-origin"));
    }

    #[test]
    fn empty_or_whitespace_frame_selector_is_rejected() {
        assert!(require_frame_chain("").is_err());
        assert!(require_frame_chain("   ").is_err());
        assert!(require_frame_chain(">>").is_err());
        assert!(require_frame_chain(" >> >> ").is_err());
        assert_eq!(require_frame_chain("iframe.a").unwrap(), vec!["iframe.a"]);
    }

    #[test]
    fn resolve_frame_id_prefers_unique_name_over_url() {
        let frames = vec![
            ("f1", "main", "https://a.example/"),
            ("f2", "payment", "https://pay.example/checkout"),
        ];
        assert_eq!(resolve_frame_id(&frames, "payment", "").unwrap(), "f2");
        assert_eq!(
            resolve_frame_id(&frames, "", "https://pay.example/checkout").unwrap(),
            "f2"
        );
    }

    #[test]
    fn resolve_frame_id_rejects_ambiguous_name() {
        // Two sibling iframes with the same `name` — regression check for
        // the original bug where the first DFS hit silently won, risking
        // reading from / acting on the wrong origin.
        let frames = vec![
            ("f1", "ad-slot", "https://ads1.example/"),
            ("f2", "ad-slot", "https://ads2.example/"),
        ];
        let err = resolve_frame_id(&frames, "ad-slot", "")
            .unwrap_err()
            .to_string();
        assert!(err.contains("ambiguous"), "unexpected error: {err}");
    }

    #[test]
    fn resolve_frame_id_rejects_ambiguous_url_substring() {
        let frames = vec![
            ("f1", "", "https://cdn.example/widget?id=1"),
            ("f2", "", "https://cdn.example/widget?id=2"),
        ];
        let err = resolve_frame_id(&frames, "", "cdn.example/widget")
            .unwrap_err()
            .to_string();
        assert!(err.contains("ambiguous"), "unexpected error: {err}");
    }

    #[test]
    fn resolve_frame_id_exact_url_match_wins_over_multiple_substring_matches() {
        // Exact match should short-circuit before the looser substring pass
        // even when other frames would also substring-match.
        let frames = vec![
            ("f1", "", "https://cdn.example/widget"),
            ("f2", "", "https://cdn.example/widget/v2"),
        ];
        assert_eq!(
            resolve_frame_id(&frames, "", "https://cdn.example/widget").unwrap(),
            "f1"
        );
    }

    #[test]
    fn resolve_frame_id_errors_when_nothing_matches() {
        let frames = vec![("f1", "", "https://a.example/")];
        let err = resolve_frame_id(&frames, "nope", "https://b.example/")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"), "unexpected error: {err}");
    }
}
