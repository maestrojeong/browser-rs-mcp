//! Stealth layer.
//!
//! Two fronts:
//!  1. **Launch flags** — strip Chrome's automation tells before the process
//!     even starts (`AutomationControlled`, default-browser noise). We never
//!     pass `--enable-automation`, so `navigator.webdriver` is not forced on.
//!  2. **Injected script** — runs via `Page.addScriptToEvaluateOnNewDocument`
//!     before any page JS, patching the residual fingerprints a site can read.
//!
//! Crucially, page introspection (`Runtime.evaluate`, `Accessibility.*`) is
//! done **without** calling `Runtime.enable` / `Console.enable`, which are the
//! high-signal CDP tells Patchright removes. Not enabling them = nothing to hide.

/// Command-line flags that reduce the automation fingerprint.
pub fn stealth_flags() -> Vec<String> {
    [
        "--disable-blink-features=AutomationControlled",
        "--no-first-run",
        "--no-default-browser-check",
        "--no-service-autorun",
        "--password-store=basic",
        "--use-mock-keychain",
        "--disable-background-networking",
        "--disable-component-update",
        "--disable-features=Translate,OptimizationHints,MediaRouter",
        "--disable-hang-monitor",
        "--disable-popup-blocking",
        "--disable-prompt-on-repost",
        "--disable-sync",
        "--metrics-recording-only",
        "--mute-audio",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// JS injected into every new document before the page's own scripts run.
///
/// Kept intentionally small: over-patching is itself detectable. We normalize
/// the handful of properties headless/automated Chrome gets wrong.
pub const STEALTH_INIT_SCRIPT: &str = r#"
(() => {
  // navigator.webdriver -> undefined
  try {
    Object.defineProperty(Navigator.prototype, 'webdriver', {
      get: () => undefined,
      configurable: true,
    });
  } catch (_) {}

  // A non-empty, plausible plugins/mimeTypes surface.
  try {
    const make = (arr) => {
      arr.item = (i) => arr[i];
      arr.namedItem = (n) => arr.find((p) => p.name === n) || null;
      arr.refresh = () => {};
      return arr;
    };
    if (navigator.plugins && navigator.plugins.length === 0) {
      const plugins = make([
        { name: 'PDF Viewer', filename: 'internal-pdf-viewer', description: 'Portable Document Format' },
        { name: 'Chrome PDF Viewer', filename: 'internal-pdf-viewer', description: '' },
      ]);
      Object.defineProperty(navigator, 'plugins', { get: () => plugins, configurable: true });
    }
  } catch (_) {}

  // languages should not be empty.
  try {
    if (!navigator.languages || navigator.languages.length === 0) {
      Object.defineProperty(navigator, 'languages', {
        get: () => ['en-US', 'en'],
        configurable: true,
      });
    }
  } catch (_) {}

  // window.chrome runtime shim (present in real Chrome, missing when driven).
  try {
    if (!window.chrome) {
      window.chrome = {};
    }
    if (!window.chrome.runtime) {
      window.chrome.runtime = {};
    }
  } catch (_) {}

  // Headless reports outerWidth/outerHeight === 0. Mirror the inner size (plus
  // typical chrome) so the window looks like a real one.
  try {
    if (window.outerWidth === 0) {
      Object.defineProperty(window, 'outerWidth', {
        get: () => window.innerWidth,
        configurable: true,
      });
    }
    if (window.outerHeight === 0) {
      Object.defineProperty(window, 'outerHeight', {
        get: () => window.innerHeight + 74,
        configurable: true,
      });
    }
  } catch (_) {}

  // WebGL renderer: headless/GPU-less Chrome reports a software rasterizer
  // (SwiftShader / llvmpipe) — a strong automation tell. When (and only when)
  // the real renderer is software, present a common hardware GPU instead. On a
  // real GPU we leave the true values untouched to stay consistent.
  try {
    const UNMASKED_VENDOR = 0x9245;   // 37445
    const UNMASKED_RENDERER = 0x9246; // 37446
    const FAKE_VENDOR = 'Google Inc. (Intel)';
    const FAKE_RENDERER =
      'ANGLE (Intel, Intel(R) UHD Graphics (0x00009BC4) Direct3D11 vs_5_0 ps_5_0, D3D11)';
    const isSoftware = (s) => /swiftshader|llvmpipe|software|mesa/i.test(String(s));
    const patch = (proto) => {
      if (!proto || !proto.getParameter) return;
      const orig = proto.getParameter;
      proto.getParameter = function (p) {
        const real = orig.call(this, p);
        if (p === UNMASKED_RENDERER && isSoftware(real)) return FAKE_RENDERER;
        if (p === UNMASKED_VENDOR && isSoftware(orig.call(this, UNMASKED_RENDERER)))
          return FAKE_VENDOR;
        return real;
      };
    };
    patch(window.WebGLRenderingContext && WebGLRenderingContext.prototype);
    patch(window.WebGL2RenderingContext && WebGL2RenderingContext.prototype);
  } catch (_) {}

  // Headless screen defaults to 800x600, which can be smaller than the window
  // (an implausible combination). Normalize the screen to at least the window.
  try {
    const sw = Math.max(screen.width | 0, window.innerWidth | 0, 1280);
    const sh = Math.max(screen.height | 0, window.innerHeight | 0, 800);
    const defs = { width: sw, height: sh, availWidth: sw, availHeight: sh - 40 };
    for (const k in defs) {
      const v = defs[k];
      Object.defineProperty(screen, k, { get: () => v, configurable: true });
    }
  } catch (_) {}

  // Permissions.query for 'notifications' should mirror Notification.permission.
  try {
    const orig = window.navigator.permissions && window.navigator.permissions.query;
    if (orig) {
      window.navigator.permissions.query = (params) =>
        params && params.name === 'notifications'
          ? Promise.resolve({ state: Notification.permission })
          : orig(params);
    }
  } catch (_) {}
})();
"#;
