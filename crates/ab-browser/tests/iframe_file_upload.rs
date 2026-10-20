//! Regression: a `<input type="file">` living inside a cross-origin iframe
//! (optionally behind a closed shadow root) could not be filled at all.
//! `browser_file_upload` / `BrowserSession::upload_files` always issues
//! `DOM.setFileInputFiles` on `self.session_id`, the top-level frame's CDP
//! session, so the backend node id of an element living in a different
//! frame's session (cross-origin iframe, OOPIF) never resolves. There was no
//! iframe-aware equivalent of `iframe_click`/`iframe_type`/`iframe_read` for
//! file inputs. `iframe_upload_files` fixes this by resolving the file input
//! with the same shadow-piercing CDP DOM walk and sending
//! `DOM.setFileInputFiles` on the innermost frame's own session.

use std::io::Write as _;
use std::time::Duration;

use ab_browser::{Browser, LaunchOptions, ReadMode};
use base64::{engine::general_purpose::STANDARD, Engine as _};

/// Cross-origin (opaque `data:` origin) frame with a file input hidden
/// behind a *closed* shadow root, echoing the uploaded file's name so the
/// test can assert the upload actually landed without relying on JS
/// property reads the `iframe_read` HTML/text modes don't expose.
const CROSS_FRAME: &str = r#"<!doctype html>
  <div id="upload-host"></div>
  <div id="upload-echo">upload pending</div>
  <script>
    const echo = document.querySelector('#upload-echo');
    const root = document.querySelector('#upload-host').attachShadow({mode: 'closed'});
    const input = document.createElement('input');
    input.type = 'file';
    input.id = 'file-input';
    input.addEventListener('change', () => {
      echo.textContent = input.files.length
        ? input.files[0].name
        : 'no files';
    });
    root.append(input);
  </script>"#;

fn page_html() -> String {
    let cross = format!("data:text/html;base64,{}", STANDARD.encode(CROSS_FRAME));
    format!(
        r#"<!doctype html>
      <iframe id="outer" src="{cross}" style="width: 480px; height: 360px;"></iframe>"#
    )
}

#[tokio::test]
#[ignore = "requires a locally installed Chrome or Chromium"]
async fn iframe_upload_sets_files_on_cross_origin_input() -> anyhow::Result<()> {
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

    let upload_dir = tempfile::tempdir()?;
    let upload_path = upload_dir.path().join("profile-photo.png");
    let mut file = std::fs::File::create(&upload_path)?;
    file.write_all(b"not a real png, just upload test bytes")?;
    drop(file);

    // Before the fix, resolving the selector inside the cross-origin frame's
    // own session and setting the files there is exactly what was missing;
    // this is the call under test.
    page.iframe_upload_files(
        "#outer",
        "#file-input",
        &[upload_path.to_string_lossy().into_owned()],
    )
    .await?;

    assert_eq!(
        page.iframe_read("#outer", "#upload-echo", ReadMode::Text)
            .await?,
        "profile-photo.png"
    );

    let error = page
        .iframe_upload_files(
            "#outer",
            "#missing-input",
            &[upload_path.to_string_lossy().into_owned()],
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("no element matches selector"), "{error}");
    Ok(())
}
