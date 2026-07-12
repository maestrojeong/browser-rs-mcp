//! Smoke test: launch Chrome, navigate, snapshot, screenshot — no MCP, no agent.
//!
//! Run: `cargo run -p ab-browser --example smoke -- https://example.com`

use ab_browser::{Browser, LaunchOptions};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ab_browser=info,ab_cdp=warn".into()),
        )
        .init();

    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "https://example.com".to_string());

    let browser = Browser::launch(LaunchOptions {
        headless: true,
        ..Default::default()
    })
    .await?;
    println!("✓ browser launched");

    let page = browser.new_page(&url).await?;
    println!("✓ navigated to {}", page.url().await?);

    let title = page.evaluate("document.title").await?;
    println!("✓ title: {title}");

    // Stealth check: navigator.webdriver should be undefined.
    let wd = page.evaluate("String(navigator.webdriver)").await?;
    println!("✓ navigator.webdriver = {wd}");

    let snap = page.snapshot().await?;
    println!("--- accessibility snapshot ({} refs) ---", snap.refs.len());
    for line in snap.text.lines().take(30) {
        println!("{line}");
    }

    let png = page.screenshot().await?;
    std::fs::write("/tmp/ab-smoke.png", &png)?;
    println!("✓ screenshot: /tmp/ab-smoke.png ({} bytes)", png.len());

    browser.close().await;
    Ok(())
}
