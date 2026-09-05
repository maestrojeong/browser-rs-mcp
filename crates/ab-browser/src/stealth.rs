//! Stealth launch configuration.
//!
//! Browser-visible values are left to Chrome. JavaScript shims are more
//! fingerprintable than the native browser behavior they try to imitate.
//!
//! Crucially, page introspection (`Runtime.evaluate`, `Accessibility.*`) is
//! done **without** calling `Runtime.enable` / `Console.enable`, which are the
//! high-signal CDP tells Patchright removes. Not enabling them = nothing to hide.

/// Minimal launch flags. Kept short deliberately: every extra flag is a way
/// the launch can differ from a human's Chrome.
pub fn launch_flags() -> Vec<String> {
    [
        // The only fingerprint-relevant flag: keeps navigator.webdriver
        // naturally false without a page-visible patch.
        "--disable-blink-features=AutomationControlled",
        // Hide the "Chrome is being controlled by automated software" infobar
        // (ported from patchright's default args).
        "--disable-infobars",
        // Suppress first-run / choice-screen / keychain noise.
        "--no-first-run",
        "--no-default-browser-check",
        "--no-service-autorun",
        "--disable-search-engine-choice-screen",
        "--disable-sync",
        "--password-store=basic",
        "--use-mock-keychain",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}
