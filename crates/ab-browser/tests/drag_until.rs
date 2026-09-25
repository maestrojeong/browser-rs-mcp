use ab_browser::{
    Browser, DragRange, DragUntil, DragUntilRequest, LaunchOptions, PointerAction, PointerLocation,
    PointerRequest, UntilMode,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};

// A slider whose response is deliberately non-linear and non-monotone, so the
// release position cannot be worked out from where the pointer started. The
// page score is 1.0 exactly at handle position TARGET and falls away as the
// *response* moves away from the response at TARGET.
const HTML: &str = r#"<!doctype html>
<style>
  body { margin: 0; }
  #track { position: absolute; left: 100px; top: 100px; width: 400px; height: 40px; background: #ddd; }
  #h { position: absolute; left: 0; top: 0; width: 40px; height: 40px; background: #222; touch-action: none; }
</style>
<div id="track"><div id="h"></div></div>
<script>
  const h = document.getElementById('h');
  const track = document.getElementById('track');
  document.body.dataset.up = '0';
  document.body.dataset.down = '0';
  let grab = null;
  h.addEventListener('pointerdown', e => {
    grab = e.clientX - h.getBoundingClientRect().left;
    document.body.dataset.down = '1';
    h.setPointerCapture(e.pointerId);
  });
  h.addEventListener('pointermove', e => {
    if (grab === null) return;
    const x = Math.max(0, Math.min(360, e.clientX - track.getBoundingClientRect().left - grab));
    h.style.left = x + 'px';
  });
  const up = () => {
    if (grab !== null) {
      grab = null;
      document.body.dataset.down = '0';
      document.body.dataset.up = String(+document.body.dataset.up + 1);
    }
  };
  h.addEventListener('pointerup', up);
  h.addEventListener('pointercancel', up);
</script>"#;

// The condition runs in the page's isolated world, so it reads the DOM (not page globals).
const SCORE: &str = "(() => { const h = document.getElementById('h'); \
    const f = x => x + 45 * Math.sin(x / 35); /* non-linear, not monotone */ \
    return Math.max(0, 1 - Math.abs(f(h.offsetLeft) - f(213)) / 30); })()";
const X: &str = "document.getElementById('h').offsetLeft";
const UP: &str = "+document.body.dataset.up";
const DOWN: &str = "+document.body.dataset.down";

fn until(expression: &str, mode: UntilMode, threshold: Option<f64>) -> DragUntil {
    DragUntil {
        expression: expression.into(),
        mode,
        threshold,
        step_px: 6.0,
        settle_ms: 20,
        dwell_ms: 0,
        max_ms: 20_000,
    }
}

fn request(until: DragUntil) -> DragUntilRequest {
    DragUntilRequest {
        // centre of the handle at its start position
        origin: PointerLocation::Coordinates { x: 120.0, y: 120.0 },
        range: DragRange::Along {
            dx: 1.0,
            dy: 0.0,
            distance: 340.0,
        },
        until,
    }
}

#[tokio::test]
#[ignore = "requires a locally installed Chrome or Chromium"]
async fn drag_until_stops_on_condition_and_at_best_score() -> anyhow::Result<()> {
    let url = format!("data:text/html;base64,{}", STANDARD.encode(HTML));
    let profile = tempfile::tempdir()?;
    let browser = Browser::launch(LaunchOptions {
        headless: true,
        user_data_dir: Some(profile.path().to_path_buf()),
        ..Default::default()
    })
    .await?;
    let page = browser.new_page(&url).await?;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // 1) "true" mode with a numeric score + threshold: stops as soon as it fits.
    let outcome = page
        .drag_until(&request(until(SCORE, UntilMode::True, Some(0.95))))
        .await?;
    assert!(outcome.condition_met, "{outcome:?}");
    assert_eq!(outcome.reason, "condition_met");
    let x = page.evaluate(X).await?.as_f64().unwrap();
    assert!(
        (x - 213.0).abs() < 12.0,
        "released at {x}, outcome {outcome:?}"
    );
    assert_eq!(page.evaluate(UP).await?.as_i64(), Some(1));

    // reset the slider for the next scenario
    page.evaluate("document.getElementById('h').style.left='0px'")
        .await?;

    // 2) "max" mode: sweeps the whole range, then settles on the best position.
    let outcome = page
        .drag_until(&request(until(SCORE, UntilMode::Max, None)))
        .await?;
    assert_eq!(outcome.reason, "best_of_sweep", "{outcome:?}");
    assert!(outcome.best_score.unwrap() > 0.9, "{outcome:?}");
    let x = page.evaluate(X).await?.as_f64().unwrap();
    assert!(
        (x - 213.0).abs() < 14.0,
        "settled at {x}, outcome {outcome:?}"
    );
    assert_eq!(page.evaluate(UP).await?.as_i64(), Some(2));

    page.evaluate("document.getElementById('h').style.left='0px'")
        .await?;

    // 3) A condition that never holds runs to the end of the range without hanging...
    let outcome = page
        .drag_until(&request(until("false", UntilMode::True, None)))
        .await?;
    assert!(!outcome.condition_met);
    assert_eq!(outcome.reason, "range_end");
    assert_eq!(page.evaluate(UP).await?.as_i64(), Some(3));

    page.evaluate("document.getElementById('h').style.left='0px'")
        .await?;

    // 4) ...and a broken expression fails, but the button is still released.
    let failed = page
        .drag_until(&request(until("'nope'", UntilMode::True, None)))
        .await;
    assert!(failed.is_err());
    assert_eq!(page.evaluate(UP).await?.as_i64(), Some(4));

    page.evaluate("document.getElementById('h').style.left='0px'")
        .await?;

    // 5) A pending promise is bounded by max_ms, including the evaluate call.
    let mut timed = until("new Promise(() => {})", UntilMode::True, None);
    timed.settle_ms = 0;
    timed.max_ms = 150;
    let started = std::time::Instant::now();
    let outcome = page.drag_until(&request(timed)).await?;
    assert_eq!(outcome.reason, "timeout", "{outcome:?}");
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    assert_eq!(page.evaluate(UP).await?.as_i64(), Some(5));

    page.evaluate("document.getElementById('h').style.left='0px'")
        .await?;

    // 6) Dropping the caller cancels the loop, but the owned cleanup still
    // releases the button and mutation lock.
    let mut cancellable = until("false", UntilMode::True, None);
    cancellable.settle_ms = 200;
    let drag_page = page.clone();
    let drag = tokio::spawn(async move { drag_page.drag_until(&request(cancellable)).await });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if page.evaluate(DOWN).await.ok().and_then(|v| v.as_i64()) == Some(1) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await?;
    drag.abort();
    let _ = drag.await;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if page.evaluate(UP).await.ok().and_then(|v| v.as_i64()) == Some(6) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await?;
    assert_eq!(page.evaluate(DOWN).await?.as_i64(), Some(0));
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        page.dispatch_pointer(&PointerRequest {
            action: PointerAction::Hover,
            origin: PointerLocation::Coordinates { x: 120.0, y: 120.0 },
            destination: None,
            delta_x: 0.0,
            delta_y: 0.0,
        }),
    )
    .await??;

    browser.close().await;
    Ok(())
}
