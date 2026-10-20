//! Regression: `iframe_type` / `iframe_read` must resolve their target with
//! the same shadow-piercing CDP DOM walk as `iframe_click` / `iframe_hover`.
//! They used to run a plain `document.querySelector` inside the frame, which
//! cannot see into shadow roots, so every shadow-hosted form field failed
//! with "element not found" even right after `iframe_click` hit it.

use std::time::Duration;

use ab_browser::{Browser, LaunchOptions, ReadMode};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use tokio_util::sync::CancellationToken;

/// Cross-origin (opaque `data:` origin) frame with an *open* shadow root.
const CROSS_FRAME: &str = r#"<!doctype html>
  <div id="deep-host"></div>
  <div id="deep-echo">deep pending</div>
  <script>
    const echo = document.querySelector('#deep-echo');
    const root = document.querySelector('#deep-host').attachShadow({mode: 'open'});
    const input = document.createElement('input');
    input.id = 'deep-input';
    input.addEventListener('input', () => echo.textContent = input.value);
    const label = document.createElement('span');
    label.id = 'deep-label';
    label.textContent = 'inside open shadow across origins';
    root.append(label, input);
  </script>"#;

fn page_html() -> String {
    let cross = format!("data:text/html;base64,{}", STANDARD.encode(CROSS_FRAME));
    format!(
        r#"<!doctype html>
      <iframe id="outer" style="width: 480px; height: 360px;"></iframe>
      <script>
        document.querySelector('#outer').srcdoc = `<!doctype html>
          <div id="shadow-host"></div>
          <input id="plain">
          <div id="echo">echo pending</div>
          <div id="plain-echo">plain pending</div>
          <iframe id="cross" src="{cross}" style="width: 400px; height: 120px;"></iframe>
          <script>
            const echo = document.querySelector('#echo');
            const plain = document.querySelector('#plain');
            plain.addEventListener('input', () =>
              document.querySelector('#plain-echo').textContent = plain.value);
            const root = document.querySelector('#shadow-host').attachShadow({{mode: 'closed'}});
            const input = document.createElement('input');
            input.id = 'shadow-input';
            input.addEventListener('input', () => echo.textContent = input.value);
            const text = document.createElement('p');
            text.id = 'shadow-text';
            text.textContent = 'inside closed shadow';
            root.append(text, input);
          <\/script>`;
      </script>"#
    )
}

#[tokio::test]
#[ignore = "requires a locally installed Chrome or Chromium"]
async fn iframe_type_and_read_pierce_shadow_roots() -> anyhow::Result<()> {
    let url = format!("data:text/html;base64,{}", STANDARD.encode(page_html()));
    let profile = tempfile::tempdir()?;
    let browser = Browser::launch(LaunchOptions {
        headless: true,
        user_data_dir: Some(profile.path().to_path_buf()),
        ..Default::default()
    })
    .await?;
    let page = browser.new_page(&url).await?;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let cancel = CancellationToken::new();

    // Read inside a closed shadow root.
    assert_eq!(
        page.iframe_read("#outer", "#shadow-text", ReadMode::Text)
            .await?,
        "inside closed shadow"
    );
    let html = page
        .iframe_read("#outer", "#shadow-input", ReadMode::Html)
        .await?;
    assert!(html.contains(r#"id="shadow-input""#), "{html}");

    // Type into a closed-shadow input; `clear` replaces the previous value.
    page.iframe_type_text_cancellable("#outer", "#shadow-input", "hello", false, &cancel)
        .await?;
    assert_eq!(
        page.iframe_read("#outer", "#echo", ReadMode::Text).await?,
        "hello"
    );
    page.iframe_type_text_cancellable("#outer", "#shadow-input", "world", true, &cancel)
        .await?;
    assert_eq!(
        page.iframe_read("#outer", "#echo", ReadMode::Text).await?,
        "world"
    );

    // Plain light-DOM targets keep working, including the whole document.
    page.iframe_type_text_cancellable("#outer", "#plain", "light", false, &cancel)
        .await?;
    assert_eq!(
        page.iframe_read("#outer", "#plain-echo", ReadMode::Text)
            .await?,
        "light"
    );
    let doc = page.iframe_read("#outer", "html", ReadMode::Html).await?;
    assert!(doc.starts_with("<html"), "{doc}");

    // `>>` chain through a cross-origin hop into an open shadow root.
    assert_eq!(
        page.iframe_read("#outer >> #cross", "#deep-label", ReadMode::Text)
            .await?,
        "inside open shadow across origins"
    );
    page.iframe_type_text_cancellable("#outer >> #cross", "#deep-input", "deep", false, &cancel)
        .await?;
    assert_eq!(
        page.iframe_read("#outer >> #cross", "#deep-echo", ReadMode::Text)
            .await?,
        "deep"
    );

    let error = page
        .iframe_read("#outer", "#missing", ReadMode::Text)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("element not found: #missing"), "{error}");
    Ok(())
}
