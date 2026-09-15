use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use ab_browser::{Browser, LaunchOptions};
use base64::{engine::general_purpose::STANDARD, Engine as _};

fn temporary_profile_dir() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time is after the Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "ab-browser-eval-fn-literal-{}-{nonce}",
        std::process::id()
    ))
}

/// Callers commonly copy the Playwright `page.evaluate(() => ...)` idiom and
/// pass a bare arrow/`function` literal. `Runtime.evaluate` does not call the
/// string it's given — it only evaluates it — so a bare function literal used
/// to silently come back as `{}` instead of the caller's intended result.
/// `eval_raw` now notices the result is a function and retries once, wrapped
/// as an immediately-invoked call.
#[tokio::test]
#[ignore = "requires a locally installed headful Chrome or Chromium"]
async fn bare_arrow_function_is_invoked_instead_of_returned_as_a_value() -> anyhow::Result<()> {
    let html = "<!doctype html><title>eval fn literal</title><body>hi</body>";
    let url = format!("data:text/html;base64,{}", STANDARD.encode(html));
    let profile_dir = temporary_profile_dir();

    let browser = Browser::launch(LaunchOptions {
        headless: false,
        user_data_dir: Some(profile_dir.clone()),
        ..Default::default()
    })
    .await?;
    let page = browser.new_page(&url).await?;

    // Bare arrow function: must run and return its value, not `{}`.
    let v = page.evaluate("() => 1 + 1").await?;
    assert_eq!(v, serde_json::json!(2), "bare arrow function was not invoked");

    // Bare `function` literal: same expectation.
    let v = page.evaluate("function() { return document.title; }").await?;
    assert_eq!(
        v,
        serde_json::json!("eval fn literal"),
        "bare function literal was not invoked"
    );

    // Ordinary expressions must be completely unaffected.
    let v = page.evaluate("1 + 1").await?;
    assert_eq!(v, serde_json::json!(2));

    let v = page.evaluate("document.title").await?;
    assert_eq!(v, serde_json::json!("eval fn literal"));

    // An already-invoked IIFE must be unaffected (no double-invocation).
    let v = page.evaluate("(() => 21 * 2)()").await?;
    assert_eq!(v, serde_json::json!(42));

    // A function literal that itself returns a function must NOT be
    // recursively invoked — only the first bare-function result is retried
    // once, so the (unserializable) inner function comes back as `{}`
    // rather than being silently called down to `5`.
    let v = page.evaluate("() => () => 5").await?;
    assert_ne!(
        v,
        serde_json::json!(5),
        "returning a function-from-a-function must not be recursively invoked"
    );

    let _ = std::fs::remove_dir_all(&profile_dir);
    Ok(())
}
