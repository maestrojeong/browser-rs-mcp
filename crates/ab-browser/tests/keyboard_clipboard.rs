use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ab_browser::{Browser, LaunchOptions};
use base64::{engine::general_purpose::STANDARD, Engine as _};

fn temporary_profile_dir() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time is after the Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "ab-browser-clipboard-{}-{nonce}",
        std::process::id()
    ))
}

#[tokio::test]
#[ignore = "requires a locally installed headful Chrome or Chromium"]
async fn trusted_shortcuts_round_trip_through_the_clipboard() -> anyhow::Result<()> {
    const SOURCE: &str = "clipboard round-trip";
    let html = format!(
        r#"<!doctype html>
          <input id="source" value="{SOURCE}">
          <input id="destination" value="">
          <script>
            document.addEventListener('copy', event => {{
              document.body.dataset.copyTrusted = String(event.isTrusted);
            }});
            document.addEventListener('paste', event => {{
              document.body.dataset.pasteTrusted = String(event.isTrusted);
            }});
          </script>"#
    );
    let url = format!("data:text/html;base64,{}", STANDARD.encode(html));
    let profile_dir = temporary_profile_dir();

    #[cfg(target_os = "macos")]
    {
        let status = std::process::Command::new("pbcopy")
            .stdin(std::process::Stdio::null())
            .status()?;
        anyhow::ensure!(status.success(), "failed to clear the macOS pasteboard");
    }

    let browser = Browser::launch(LaunchOptions {
        headless: false,
        user_data_dir: Some(profile_dir.clone()),
        ..Default::default()
    })
    .await?;
    let page = browser.new_page(&url).await?;
    let activation = page.activate().await?;
    anyhow::ensure!(activation.activated, "Chrome page did not become active");

    #[cfg(target_os = "macos")]
    let modifier = "Meta";
    #[cfg(not(target_os = "macos"))]
    let modifier = "Control";

    page.evaluate("document.querySelector('#source').focus()")
        .await?;
    page.press(&format!("{modifier}+a")).await?;
    page.press(&format!("{modifier}+c")).await?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    #[cfg(target_os = "macos")]
    let os_clipboard = String::from_utf8(std::process::Command::new("pbpaste").output()?.stdout)?;

    page.evaluate("document.querySelector('#destination').focus()")
        .await?;
    page.press(&format!("{modifier}+v")).await?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let state = page
        .evaluate(
            "({ value: document.querySelector('#destination').value, copyTrusted: document.body.dataset.copyTrusted, pasteTrusted: document.body.dataset.pasteTrusted })",
        )
        .await?;

    browser.close().await;
    let _ = std::fs::remove_dir_all(profile_dir);

    #[cfg(target_os = "macos")]
    assert_eq!(
        os_clipboard, SOURCE,
        "copy did not update the OS pasteboard"
    );
    assert_eq!(state["value"], SOURCE, "paste did not populate the input");
    assert_eq!(state["copyTrusted"], "true");
    assert_eq!(state["pasteTrusted"], "true");
    Ok(())
}
