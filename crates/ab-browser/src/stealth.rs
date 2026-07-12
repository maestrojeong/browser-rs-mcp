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

/// Minimal launch flags. The only fingerprint-relevant one is
/// `--disable-blink-features=AutomationControlled`, which keeps
/// `navigator.webdriver` naturally false without any page-visible patch. The
/// rest just suppress first-run/keychain noise. We intentionally keep this list
/// short: every extra flag is a way the launch can differ from a human's Chrome.
pub fn launch_flags() -> Vec<String> {
    [
        "--disable-blink-features=AutomationControlled",
        "--no-first-run",
        "--no-default-browser-check",
        "--no-service-autorun",
        "--password-store=basic",
        "--use-mock-keychain",
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
  // --- toString hardening -------------------------------------------------
  // Our hooks are plain JS functions; a detector that reads their .toString()
  // sees source instead of "[native code]" and flags automation. Route
  // Function.prototype.toString through a proxy that returns a native-looking
  // string for functions we mark (and for toString itself). Proven pattern
  // (puppeteer-extra-stealth); guarded against recursion.
  let mark, markProp;
  try {
    const nativeToString = Function.prototype.toString;
    const nativeToStringStr = nativeToString.call(nativeToString); // "function toString() { [native code] }"
    const faux = new WeakMap();
    const proxy = new Proxy(nativeToString, {
      apply(target, thisArg, args) {
        if (faux.has(thisArg)) return faux.get(thisArg);
        if (thisArg === proxy) return nativeToStringStr;
        return Reflect.apply(target, thisArg, args);
      },
    });
    Function.prototype.toString = proxy;
    mark = (fn, name) => {
      if (typeof fn === 'function') faux.set(fn, 'function ' + name + '() { [native code] }');
      return fn;
    };
    markProp = (obj, prop, name) => {
      const d = Object.getOwnPropertyDescriptor(obj, prop);
      if (d && d.get) mark(d.get, name);
    };
  } catch (_) {
    mark = (fn) => fn;
    markProp = () => {};
  }

  // navigator.webdriver -> undefined
  try {
    Object.defineProperty(Navigator.prototype, 'webdriver', {
      get: () => undefined,
      configurable: true,
    });
    markProp(Navigator.prototype, 'webdriver', 'get webdriver');
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
      markProp(navigator, 'plugins', 'get plugins');
    }
  } catch (_) {}

  // languages should not be empty.
  try {
    if (!navigator.languages || navigator.languages.length === 0) {
      Object.defineProperty(navigator, 'languages', {
        get: () => ['en-US', 'en'],
        configurable: true,
      });
      markProp(navigator, 'languages', 'get languages');
    }
  } catch (_) {}

  // Real Chrome clamps navigator.deviceMemory to a max of 8 (privacy). Headless
  // can report the true RAM (e.g. 16/32), which is itself a tell. Clamp to 8.
  try {
    if (typeof navigator.deviceMemory === 'number' && navigator.deviceMemory > 8) {
      Object.defineProperty(navigator, 'deviceMemory', { get: () => 8, configurable: true });
      markProp(navigator, 'deviceMemory', 'get deviceMemory');
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
      proto.getParameter = mark(function getParameter(p) {
        const real = orig.call(this, p);
        if (p === UNMASKED_RENDERER && isSoftware(real)) return FAKE_RENDERER;
        if (p === UNMASKED_VENDOR && isSoftware(orig.call(this, UNMASKED_RENDERER)))
          return FAKE_VENDOR;
        return real;
      }, 'getParameter');
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
      window.navigator.permissions.query = mark(function query(params) {
        return params && params.name === 'notifications'
          ? Promise.resolve({ state: Notification.permission })
          : orig.call(this, params);
      }, 'query');
    }
  } catch (_) {}
})();
"#;
