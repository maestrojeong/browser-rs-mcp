use std::time::Duration;

use ab_browser::{
    Browser, ElementRef, LaunchOptions, Page, PointerAction, PointerLocation, PointerRequest,
    Snapshot,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};

fn element(snapshot: &Snapshot, needle: &str) -> anyhow::Result<ElementRef> {
    let line = snapshot
        .text
        .lines()
        .find(|line| line.contains(needle) && line.contains("[ref="))
        .ok_or_else(|| anyhow::anyhow!("snapshot has no ref containing {needle:?}"))?;
    let start = line.find("[ref=").expect("checked above") + 5;
    let end = start
        + line[start..]
            .find(']')
            .ok_or_else(|| anyhow::anyhow!("malformed ref line: {line}"))?;
    snapshot
        .refs
        .get(&line[start..end])
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("ref is absent from snapshot map: {line}"))
}

async fn act(page: &Page, action: PointerAction, target: ElementRef) -> anyhow::Result<()> {
    page.dispatch_pointer(&PointerRequest {
        action,
        origin: PointerLocation::Element(target),
        destination: None,
        delta_x: 0.0,
        delta_y: 0.0,
    })
    .await?;
    Ok(())
}

async fn snapshot_after(page: &Page, millis: u64) -> anyhow::Result<Snapshot> {
    tokio::time::sleep(Duration::from_millis(millis)).await;
    Ok(page.snapshot().await?)
}

#[tokio::test]
#[ignore = "requires a locally installed Chrome or Chromium"]
async fn ref_pointer_actionability_regressions() -> anyhow::Result<()> {
    let html = r#"<!doctype html>
      <style>
        body { margin: 0; font: 16px sans-serif; }
        .spacer { height: 1300px; }
        button { width: 220px; height: 48px; margin: 12px; position: relative; }
        #nested-cover { position: absolute; inset: 0; display: grid; place-items: center; }
        #menu-items { display: none; margin-left: 12px; }
        #menu.open #menu-items { display: block; }
      </style>
      <div class="spacer"></div>
      <button id="nested"><span id="nested-cover">Nested target</span></button>
      <div id="nested-status">nested pending</div>
      <div id="shadow-host"></div>
      <div id="shadow-status">shadow pending</div>
      <div id="menu">
        <button id="menu-trigger">Payment method</button>
        <div id="menu-items"><button id="menu-option">Kakao Pay</button></div>
      </div>
      <div id="menu-status">menu pending</div>
      <div style="position: relative; width: 244px; height: 72px;">
        <button id="covered">Covered target</button>
        <button id="sibling-cover" style="position: absolute; inset: 12px; margin: 0;">Sibling cover</button>
      </div>
      <button id="vanish">Vanish on hover</button>
      <button id="replace">Replace after first click</button>
      <div id="replace-status">replace pending</div>
      <iframe id="action-frame" style="width: 360px; height: 180px;"></iframe>
      <script>
        const nested = document.querySelector('#nested');
        const nestedStatus = document.querySelector('#nested-status');
        const shadowStatus = document.querySelector('#shadow-status');
        const menu = document.querySelector('#menu');
        const menuOption = document.querySelector('#menu-option');
        const menuStatus = document.querySelector('#menu-status');
        const vanish = document.querySelector('#vanish');
        const replace = document.querySelector('#replace');
        const replaceStatus = document.querySelector('#replace-status');
        nested.addEventListener('click', () => nestedStatus.textContent = 'nested clicked');

        const root = document.querySelector('#shadow-host').attachShadow({mode: 'closed'});
        const shadowButton = document.createElement('button');
        shadowButton.textContent = 'Shadow action';
        shadowButton.addEventListener('click', () => shadowStatus.textContent = 'shadow clicked');
        root.append(shadowButton);

        let closeTimer = 0;
        menu.addEventListener('mouseenter', () => {
          clearTimeout(closeTimer);
          menu.classList.add('open');
        });
        menu.addEventListener('mouseleave', () => {
          closeTimer = setTimeout(() => menu.classList.remove('open'), 200);
        });
        menuOption.addEventListener('click', () => menuStatus.textContent = 'menu clicked');

        vanish.addEventListener('mouseenter', () => vanish.remove());
        replace.addEventListener('click', () => {
          replaceStatus.textContent = 'first click dispatched';
          const replacement = document.createElement('button');
          replacement.textContent = 'Replacement';
          replace.replaceWith(replacement);
        }, {once: true});

        const frame = document.querySelector('#action-frame');
        frame.srcdoc = `<!doctype html>
          <style>
            body { margin: 0; font: 16px sans-serif; }
            button { width: 220px; height: 48px; margin: 8px; }
          </style>
          <div id="shadow-host"></div>
          <div id="status">iframe pending</div>
          <script>
            const status = document.querySelector('#status');
            const root = document.querySelector('#shadow-host').attachShadow({mode: 'closed'});
            const direct = document.createElement('button');
            direct.id = 'shadow-direct';
            direct.textContent = 'Iframe shadow action';
            direct.addEventListener('click', () => status.textContent = 'iframe shadow clicked');
            const menu = document.createElement('div');
            const trigger = document.createElement('button');
            trigger.id = 'menu-trigger';
            trigger.textContent = 'Iframe payment method';
            const option = document.createElement('button');
            option.id = 'menu-option';
            option.textContent = 'Iframe Kakao Pay';
            option.style.display = 'none';
            trigger.addEventListener('mouseenter', () => option.style.display = 'block');
            option.addEventListener('click', () => status.textContent = 'iframe menu clicked');
            menu.append(trigger, option);
            root.append(direct, menu);
          <\/script>`;
      </script>"#;
    let url = format!("data:text/html;base64,{}", STANDARD.encode(html));
    let browser = Browser::launch(LaunchOptions {
        headless: true,
        ..Default::default()
    })
    .await?;
    let page = browser.new_page(&url).await?;

    let snapshot = snapshot_after(&page, 150).await?;
    act(
        &page,
        PointerAction::Click,
        element(&snapshot, "Nested target")?,
    )
    .await?;
    assert!(snapshot_after(&page, 50)
        .await?
        .text
        .contains("nested clicked"));

    let snapshot = page.snapshot().await?;
    act(
        &page,
        PointerAction::Click,
        element(&snapshot, "Shadow action")?,
    )
    .await?;
    assert!(snapshot_after(&page, 50)
        .await?
        .text
        .contains("shadow clicked"));

    let snapshot = page.snapshot().await?;
    act(
        &page,
        PointerAction::Hover,
        element(&snapshot, "Payment method")?,
    )
    .await?;
    let snapshot = snapshot_after(&page, 50).await?;
    act(
        &page,
        PointerAction::Click,
        element(&snapshot, "Kakao Pay")?,
    )
    .await?;
    assert!(snapshot_after(&page, 50)
        .await?
        .text
        .contains("menu clicked"));

    let snapshot = page.snapshot().await?;
    let error = act(
        &page,
        PointerAction::Click,
        element(&snapshot, "Covered target")?,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("outside the visible hit area"), "{error}");

    let snapshot = page.snapshot().await?;
    let error = act(
        &page,
        PointerAction::Click,
        element(&snapshot, "Vanish on hover")?,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("before the click landed"), "{error}");

    let snapshot = page.snapshot().await?;
    let error = act(
        &page,
        PointerAction::DoubleClick,
        element(&snapshot, "Replace after first click")?,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("before the click landed"), "{error}");
    assert!(page
        .snapshot()
        .await?
        .text
        .contains("first click dispatched"));

    page.iframe_click("#action-frame", "#shadow-direct").await?;
    assert_eq!(
        page.iframe_read("#action-frame", "#status", ab_browser::ReadMode::Text)
            .await?,
        "iframe shadow clicked"
    );

    page.iframe_hover("#action-frame", "#menu-trigger").await?;
    page.iframe_click("#action-frame", "#menu-option").await?;
    assert_eq!(
        page.iframe_read("#action-frame", "#status", ab_browser::ReadMode::Text)
            .await?,
        "iframe menu clicked"
    );

    browser.close().await;
    Ok(())
}
