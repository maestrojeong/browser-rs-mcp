//! High-level, agent-friendly browser control on top of `ab-cdp`.
//!
//! `Browser` owns the Chrome process and the CDP connection. `Page` is a single
//! attached tab (flatten-mode session). Everything is designed so an LLM agent
//! can run the loop: `snapshot -> act -> verify`.

pub mod snapshot;
pub mod stealth;

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use ab_cdp::CdpClient;
use serde_json::{json, Value};
use tokio::process::{Child, Command};
use tracing::{debug, info};

pub use snapshot::Snapshot;

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
}

pub type Result<T> = std::result::Result<T, BrowserError>;

#[derive(Debug, Clone)]
pub struct LaunchOptions {
    /// Headless is a strong fingerprint tell. Off by default — a real headful
    /// window on real hardware is what makes the fingerprint match a human's.
    pub headless: bool,
    /// Inject the JS stealth-patching layer. Off by default: patching each
    /// property is itself an anomaly that sophisticated defenses flag. Only
    /// turn this on as a best-effort fallback when forced to run headless.
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
            inject_stealth: false,
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
    /// Default mode is headful with a persistent profile and NO JS patching, so
    /// the page's fingerprint is that of a real, human-driven Chrome. Only the
    /// `AutomationControlled` blink feature is disabled (a launch flag, not a
    /// page-visible patch) so `navigator.webdriver` is naturally false.
    pub async fn launch(opts: LaunchOptions) -> Result<Self> {
        let chrome = opts
            .chrome_path
            .clone()
            .or_else(detect_chrome)
            .ok_or(BrowserError::ChromeNotFound)?;

        let data_dir = match &opts.user_data_dir {
            Some(d) => d.clone(),
            None => default_profile_dir()?,
        };

        let mut args: Vec<String> = vec![
            format!("--remote-debugging-port={}", opts.port),
            format!("--user-data-dir={}", data_dir.display()),
            format!("--window-size={},{}", opts.window_size.0, opts.window_size.1),
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
        let user_agent = if opts.inject_stealth && opts.headless {
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
            inject_stealth: opts.inject_stealth,
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

        let page = Page {
            client: self.client.clone(),
            session_id,
            target_id,
        };
        // No page patching by default: an untouched real Chrome is the goal.
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

    /// Terminate the browser process (only if we launched it; connect() no-op).
    pub async fn close(mut self) {
        let _ = self.client.send("Browser.close", json!({})).await;
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill().await;
        }
    }
}

/// A single attached tab.
#[derive(Clone)]
pub struct Page {
    client: CdpClient,
    session_id: String,
    target_id: String,
}

impl Page {
    pub fn target_id(&self) -> &str {
        &self.target_id
    }

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
                Err(_) => return Ok(()),      // timeout: proceed best-effort
            }
        }
    }

    /// One-shot JS evaluation in page context. No Runtime.enable (stealth).
    pub async fn evaluate(&self, expression: &str) -> Result<Value> {
        let res = self
            .client
            .send_on(
                &self.session_id,
                "Runtime.evaluate",
                json!({
                    "expression": expression,
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
        Ok(snapshot::render(&nodes))
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
        Ok(self
            .evaluate(js)
            .await?
            .as_str()
            .unwrap_or("")
            .to_string())
    }
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
        let xs = [q[0].as_f64(), q[2].as_f64(), q[4].as_f64(), q[6].as_f64()];
        let ys = [q[1].as_f64(), q[3].as_f64(), q[5].as_f64(), q[7].as_f64()];
        let cx = xs.iter().flatten().sum::<f64>() / 4.0;
        let cy = ys.iter().flatten().sum::<f64>() / 4.0;
        Ok(Some((cx, cy)))
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
        Ok(res
            .ok()
            .and_then(|r| r.get("object").and_then(|o| o.get("objectId")).and_then(Value::as_str).map(String::from)))
    }

    /// Click a node by ref. Prefers a real synthesized mouse click at the
    /// element center; falls back to a DOM `.click()` when there is no box.
    pub async fn click(&self, backend: i64) -> Result<()> {
        if let Some((x, y)) = self.node_center(backend).await? {
            for kind in ["mousePressed", "mouseReleased"] {
                self.client
                    .send_on(
                        &self.session_id,
                        "Input.dispatchMouseEvent",
                        json!({
                            "type": kind,
                            "x": x,
                            "y": y,
                            "button": "left",
                            "buttons": 1,
                            "clickCount": 1,
                        }),
                    )
                    .await?;
            }
            return Ok(());
        }
        // Fallback: JS click via objectId.
        if let Some(obj) = self.resolve_object(backend).await? {
            self.client
                .send_on(
                    &self.session_id,
                    "Runtime.callFunctionOn",
                    json!({
                        "objectId": obj,
                        "functionDeclaration": "function(){ this.click(); }",
                    }),
                )
                .await?;
            return Ok(());
        }
        Err(BrowserError::Protocol("element not clickable".into()))
    }

    /// Focus a node and type text via real key/insert events.
    /// When `clear` is set, existing content is selected and replaced.
    pub async fn type_text(&self, backend: i64, text: &str, clear: bool) -> Result<()> {
        self.client
            .send_on(&self.session_id, "DOM.focus", json!({ "backendNodeId": backend }))
            .await?;
        if clear {
            if let Some(obj) = self.resolve_object(backend).await? {
                // Select all existing text so insertText replaces it.
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
        self.client
            .send_on(
                &self.session_id,
                "Input.insertText",
                json!({ "text": text }),
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
                        "Page.frameStartedLoading" | "Page.frameRequestedNavigation"
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
            Ok(Some(true)) => {}                        // already loaded
            Ok(Some(false)) => self.wait_for_load().await.unwrap_or(()), // nav in flight
            _ => tokio::time::sleep(Duration::from_millis(350)).await, // no nav: DOM grace
        }
    }

    /// Press a single named key (e.g. "Enter", "Tab", "Escape").
    pub async fn press(&self, key: &str) -> Result<()> {
        let (code, vk) = match key {
            "Enter" => ("Enter", 13),
            "Tab" => ("Tab", 9),
            "Escape" => ("Escape", 27),
            "Backspace" => ("Backspace", 8),
            "ArrowDown" => ("ArrowDown", 40),
            "ArrowUp" => ("ArrowUp", 38),
            _ => (key, 0),
        };
        for kind in ["keyDown", "keyUp"] {
            self.client
                .send_on(
                    &self.session_id,
                    "Input.dispatchKeyEvent",
                    json!({
                        "type": kind,
                        "key": code,
                        "code": code,
                        "windowsVirtualKeyCode": vk,
                        "nativeVirtualKeyCode": vk,
                    }),
                )
                .await?;
        }
        Ok(())
    }
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
                .map(|h| PathBuf::from(h).join(".agent-browser").join("profile"))
        })
        .ok_or_else(|| BrowserError::Launch("cannot resolve profile dir; set AB_PROFILE".into()))?;
    std::fs::create_dir_all(&base).map_err(|e| BrowserError::Launch(e.to_string()))?;
    Ok(base)
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
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
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
