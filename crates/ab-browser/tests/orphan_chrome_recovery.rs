// The scenario is Windows-only, and so is everything in this file. Gating the
// test function alone would leave the import behind on other hosts, where it
// is then an unused-import error under `-D warnings`.
#![cfg(windows)]

use ab_browser::{Browser, LaunchOptions};

/// Simulates the crash-restart scenario: a Chrome process is still holding a
/// profile because whatever launched it (an earlier `browser-rs.exe`) was
/// killed out-of-band without it, leaving no `ProfileLock` and no
/// `DevToolsActivePort` behind. A fresh `launch()` against the same profile
/// must reclaim it rather than spawning a second Chrome that silently
/// hands off to the orphan and never gets its own devtools port.
#[tokio::test]
#[ignore = "requires a locally installed Chrome or Chromium (Windows-only scenario)"]
async fn launch_reclaims_a_profile_orphaned_by_an_unmanaged_chrome() {
    let profile = tempfile::tempdir().unwrap();

    let chrome = std::env::var("AB_CHROME")
        .unwrap_or_else(|_| r"C:\Program Files\Google\Chrome\Application\chrome.exe".to_string());
    let mut orphan = std::process::Command::new(&chrome)
        .arg("--remote-debugging-port=0")
        .arg(format!("--user-data-dir={}", profile.path().display()))
        .arg("--no-first-run")
        .arg("about:blank")
        .spawn()
        .expect("failed to spawn orphan chrome");

    // Give the orphan time to fully come up (own devtools port written) so
    // it's a realistic stand-in for a Chrome that was genuinely running,
    // not one still mid-launch.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let browser = Browser::launch(LaunchOptions {
        headless: true,
        user_data_dir: Some(profile.path().to_path_buf()),
        ..Default::default()
    })
    .await
    .expect("launch should reclaim the profile from the orphan, not time out");

    browser.close().await;

    // Best-effort cleanup; the orphan should already be gone if reclamation
    // killed it, but don't fail the test either way. Go through the handle
    // rather than its pid — the pid may already have been recycled, and the
    // child has to be reaped here regardless.
    let _ = orphan.kill();
    let _ = orphan.wait();
}
