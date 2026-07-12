//! agent-browser MCP server entrypoint.
//!
//! TODO(next): wire the ab-browser API into an rmcp stdio + streamable-http
//! server exposing browser_* tools (navigate, snapshot, act, evaluate,
//! screenshot, tabs, ...). For now this is a placeholder that validates the
//! browser core boots.

use ab_browser::{Browser, LaunchOptions};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    eprintln!("agent-browser: MCP server not wired yet — running boot check.");
    let browser = Browser::launch(LaunchOptions::default()).await?;
    let page = browser.new_page("https://example.com").await?;
    eprintln!("boot ok: {}", page.url().await?);
    browser.close().await;
    Ok(())
}
