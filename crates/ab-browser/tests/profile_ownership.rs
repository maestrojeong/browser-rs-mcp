use ab_browser::{Browser, BrowserError, LaunchOptions};

#[tokio::test]
#[ignore = "requires a locally installed Chrome or Chromium"]
async fn one_profile_allows_only_one_live_chrome_owner() {
    let profile = tempfile::tempdir().unwrap();
    let options = || LaunchOptions {
        headless: true,
        user_data_dir: Some(profile.path().to_path_buf()),
        ..Default::default()
    };

    let first = Browser::launch(options()).await.unwrap();
    let second = Browser::launch(options()).await;
    assert!(matches!(second, Err(BrowserError::ProfileBusy(_))));

    first.close().await;
    let replacement = Browser::launch(options()).await.unwrap();
    replacement.close().await;
}
