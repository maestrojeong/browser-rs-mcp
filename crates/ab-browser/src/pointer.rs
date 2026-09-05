//! Unified browser-internal pointer actions.
//!
//! All input uses browser-generated CDP events. Synthetic DOM-event delivery is
//! intentionally absent because it exposes isTrusted == false to page scripts.

use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::{sample_lognormal_ms, BrowserError, ElementRef, Page, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PointerAction {
    Click,
    Hover,
    RightClick,
    DoubleClick,
    Scroll,
    Drag,
}

impl PointerAction {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "click" => Ok(Self::Click),
            "hover" => Ok(Self::Hover),
            "right_click" => Ok(Self::RightClick),
            "double_click" => Ok(Self::DoubleClick),
            "scroll" => Ok(Self::Scroll),
            "drag" => Ok(Self::Drag),
            _ => Err(BrowserError::Protocol(format!(
                "unknown pointer action {value:?}"
            ))),
        }
    }
}

#[derive(Debug, Clone)]
pub enum PointerLocation {
    Element(ElementRef),
    Coordinates { x: f64, y: f64 },
}

#[derive(Debug, Clone)]
pub struct PointerRequest {
    pub action: PointerAction,
    pub origin: PointerLocation,
    pub destination: Option<PointerLocation>,
    pub delta_x: f64,
    pub delta_y: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PointerOutcome {
    pub action: PointerAction,
    pub trusted: bool,
    pub dispatched: bool,
    /// `changed`, `unchanged`, or `unknown`.
    pub observed: &'static str,
    pub retryable: bool,
}

impl Page {
    pub async fn dispatch_pointer(&self, request: &PointerRequest) -> Result<PointerOutcome> {
        validate_request(request)?;
        let _mutation = self.pointer_mutation.lock().await;
        self.validate_pointer_refs(request).await?;
        self.dispatch_trusted_pointer(request).await
    }

    async fn validate_pointer_refs(&self, request: &PointerRequest) -> Result<()> {
        if let PointerLocation::Element(element) = &request.origin {
            self.validate_element_ref(element).await?;
        }
        if let Some(PointerLocation::Element(element)) = &request.destination {
            self.validate_element_ref(element).await?;
        }
        if request.action == PointerAction::Drag {
            if let (PointerLocation::Element(origin), Some(PointerLocation::Element(destination))) =
                (&request.origin, &request.destination)
            {
                if !same_document(origin, destination) {
                    return Err(BrowserError::Protocol(
                        "drag endpoints must belong to the exact same live document".into(),
                    ));
                }
            }
        }
        Ok(())
    }

    async fn point_for_location(&self, location: &PointerLocation) -> Result<(f64, f64, f64)> {
        match location {
            PointerLocation::Coordinates { x, y } => Ok((*x, *y, 24.0)),
            PointerLocation::Element(element) => {
                self.scroll_into_view(element.backend_node_id).await;
                self.node_pointer_target(element.backend_node_id)
                    .await?
                    .ok_or_else(|| {
                        BrowserError::Protocol("pointer target has no layout box".into())
                    })
            }
        }
    }

    async fn dispatch_trusted_pointer(&self, request: &PointerRequest) -> Result<PointerOutcome> {
        let ((x, y, target_width), drag_destination) = if request.action == PointerAction::Drag {
            let destination = request.destination.as_ref().expect("validated drag");
            if let PointerLocation::Element(element) = destination {
                self.scroll_into_view(element.backend_node_id).await;
            }
            if let PointerLocation::Element(element) = &request.origin {
                self.scroll_into_view(element.backend_node_id).await;
            }
            let origin = self.point_without_scroll(&request.origin).await?;
            let destination = self.point_without_scroll(destination).await?;
            self.require_visible_hit(&request.origin, (origin.0, origin.1))
                .await?;
            self.require_visible_hit(
                request.destination.as_ref().unwrap(),
                (destination.0, destination.1),
            )
            .await?;
            (origin, Some(destination))
        } else {
            (self.point_for_location(&request.origin).await?, None)
        };
        self.require_visible_hit(&request.origin, (x, y)).await?;
        self.human_move_to(x, y, target_width).await?;

        match request.action {
            PointerAction::Click => {
                tokio::time::sleep(Duration::from_millis(sample_lognormal_ms(
                    34.0, 0.30, 12, 110,
                )))
                .await;
                self.require_click_target_after_move(&request.origin, (x, y))
                    .await?;
                self.mouse_button("mousePressed", x, y, "left", 1, 1)
                    .await?;
                tokio::time::sleep(Duration::from_millis(sample_lognormal_ms(
                    62.0, 0.32, 20, 180,
                )))
                .await;
                self.mouse_button("mouseReleased", x, y, "left", 0, 1)
                    .await?;
            }
            PointerAction::Hover => {}
            PointerAction::RightClick => {
                tokio::time::sleep(Duration::from_millis(sample_lognormal_ms(
                    34.0, 0.30, 12, 110,
                )))
                .await;
                self.require_click_target_after_move(&request.origin, (x, y))
                    .await?;
                self.mouse_button("mousePressed", x, y, "right", 2, 1)
                    .await?;
                tokio::time::sleep(Duration::from_millis(sample_lognormal_ms(
                    62.0, 0.32, 20, 180,
                )))
                .await;
                self.mouse_button("mouseReleased", x, y, "right", 0, 1)
                    .await?;
            }
            PointerAction::DoubleClick => {
                for count in [1, 2] {
                    self.require_click_target_after_move(&request.origin, (x, y))
                        .await?;
                    self.mouse_button("mousePressed", x, y, "left", 1, count)
                        .await?;
                    tokio::time::sleep(Duration::from_millis(sample_lognormal_ms(
                        54.0, 0.30, 18, 150,
                    )))
                    .await;
                    self.mouse_button("mouseReleased", x, y, "left", 0, count)
                        .await?;
                    if count == 1 {
                        tokio::time::sleep(Duration::from_millis(sample_lognormal_ms(
                            88.0, 0.28, 35, 240,
                        )))
                        .await;
                    }
                }
            }
            PointerAction::Scroll => {
                self.client
                    .send_on(
                        &self.session_id,
                        "Input.dispatchMouseEvent",
                        json!({
                            "type": "mouseWheel", "x": x, "y": y,
                            "deltaX": request.delta_x, "deltaY": request.delta_y,
                        }),
                    )
                    .await?;
            }
            PointerAction::Drag => {
                let (to_x, to_y, to_width) = drag_destination.expect("validated drag point");
                self.mouse_button("mousePressed", x, y, "left", 1, 1)
                    .await?;
                self.move_pointer_path(x, y, to_x, to_y, to_width, 1)
                    .await?;
                self.mouse_button("mouseReleased", to_x, to_y, "left", 0, 1)
                    .await?;
            }
        }

        Ok(PointerOutcome {
            action: request.action,
            trusted: true,
            dispatched: true,
            observed: "unknown",
            retryable: false,
        })
    }

    async fn point_without_scroll(&self, location: &PointerLocation) -> Result<(f64, f64, f64)> {
        match location {
            PointerLocation::Coordinates { x, y } => Ok((*x, *y, 24.0)),
            PointerLocation::Element(element) => self
                .node_pointer_target(element.backend_node_id)
                .await?
                .ok_or_else(|| BrowserError::Protocol("pointer target has no layout box".into())),
        }
    }

    async fn require_visible_hit(
        &self,
        location: &PointerLocation,
        point: (f64, f64),
    ) -> Result<()> {
        if let PointerLocation::Element(element) = location {
            if !self
                .point_hits_node(element.backend_node_id, point.0, point.1)
                .await?
            {
                return Err(BrowserError::Protocol(
                    "pointer target is outside the visible hit area".into(),
                ));
            }
        }
        Ok(())
    }

    /// Humanized travel can leave a hover-sensitive container and close its
    /// menu. Re-check element refs immediately before pressing so that failure
    /// is reported instead of silently clicking whatever replaced the target.
    async fn require_click_target_after_move(
        &self,
        location: &PointerLocation,
        point: (f64, f64),
    ) -> Result<()> {
        if let PointerLocation::Element(element) = location {
            if !self
                .point_hits_node(element.backend_node_id, point.0, point.1)
                .await?
            {
                return Err(BrowserError::Protocol(
                    "pointer target moved, closed, or became occluded before the click landed; retry with a fresh ref"
                        .into(),
                ));
            }
        }
        Ok(())
    }

    async fn mouse_button(
        &self,
        kind: &str,
        x: f64,
        y: f64,
        button: &str,
        buttons: u8,
        click_count: u8,
    ) -> Result<()> {
        self.mouse_button_on(&self.session_id, kind, x, y, button, (buttons, click_count))
            .await
    }

    async fn mouse_button_on(
        &self,
        session_id: &str,
        kind: &str,
        x: f64,
        y: f64,
        button: &str,
        state: (u8, u8),
    ) -> Result<()> {
        // Single choke point for every left-button press this crate issues —
        // by ref, by raw coordinates, as a drag origin, or against an
        // out-of-process iframe session. Checking here (instead of in each
        // caller) is what makes the macOS native-<select> guard below apply
        // uniformly instead of only to the ref-based click path.
        if kind == "mousePressed" && button == "left" {
            self.reject_if_native_modal_target(session_id, x, y).await?;
        }
        let (buttons, click_count) = state;
        self.client
            .send_on(
                session_id,
                "Input.dispatchMouseEvent",
                json!({
                    "type": kind, "x": x, "y": y, "button": button,
                    "buttons": buttons, "clickCount": click_count,
                }),
            )
            .await?;
        Ok(())
    }

    /// On macOS, a left `mousePressed` landing on a collapsed single-option
    /// `<select>` (native `NSPopUpButton`) or an enabled `<input
    /// type="file">` (native `NSOpenPanel`) hands off to a Cocoa modal
    /// run loop that blocks the browser process's main thread until it's
    /// dismissed by *native* input CDP cannot send — every later CDP call
    /// (mouseReleased, hit-test/settle checks) then hangs until its own
    /// timeout fires, which reads to callers as a multi-minute stall or a
    /// dead connection. `multiple` selects, `size>1` listboxes, and
    /// `disabled` elements don't open either native modal and are left
    /// alone, so this doesn't dead-end into the trusted alternatives
    /// (`browser_select_option`, `browser_file_upload`), which only cover
    /// that same non-modal-triggering case anyway.
    ///
    /// Fails *closed*: if the hit-test or node inspection itself errors, the
    /// coordinates are treated as unsafe rather than silently proceeding
    /// into a possible hang.
    async fn reject_if_native_modal_target(&self, session_id: &str, x: f64, y: f64) -> Result<()> {
        if !cfg!(target_os = "macos") {
            return Ok(());
        }
        fn unsafe_to_verify(error: impl std::fmt::Display) -> BrowserError {
            BrowserError::Protocol(format!(
                "cannot verify this click target is safe on macOS before \
                 mousePressed; refusing rather than risking a native-popup \
                 hang: {error}"
            ))
        }
        let metrics = self
            .client
            .send_on(session_id, "Page.getLayoutMetrics", json!({}))
            .await
            .map_err(unsafe_to_verify)?;
        let viewport = metrics
            .get("cssVisualViewport")
            .or_else(|| metrics.get("visualViewport"));
        let page_x = viewport
            .and_then(|v| v.get("pageX"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let page_y = viewport
            .and_then(|v| v.get("pageY"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let hit = self
            .client
            .send_on(
                session_id,
                "DOM.getNodeForLocation",
                json!({
                    "x": (x + page_x).round() as i64,
                    "y": (y + page_y).round() as i64,
                }),
            )
            .await
            .map_err(unsafe_to_verify)?;
        let Some(backend) = hit.get("backendNodeId").and_then(Value::as_i64) else {
            // Nothing hit-testable at this point (e.g. blank canvas) — safe.
            return Ok(());
        };
        let described = self
            .client
            .send_on(
                session_id,
                "DOM.describeNode",
                json!({ "backendNodeId": backend }),
            )
            .await
            .map_err(unsafe_to_verify)?;
        let Some(node) = described.get("node") else {
            return Err(BrowserError::Protocol(
                "cannot verify this click target is safe on macOS before \
                 mousePressed; refusing rather than risking a native-popup hang"
                    .into(),
            ));
        };
        if is_popup_capable_select(node) {
            return Err(BrowserError::Protocol(
                "clicking a native single-option <select> on macOS opens an \
                 OS-level popup menu that blocks CDP for minutes; use \
                 browser_select_option (trusted type-ahead) instead of \
                 browser_click/browser_pointer for this element"
                    .into(),
            ));
        }
        if is_native_file_picker_input(node) {
            return Err(BrowserError::Protocol(
                "clicking a native <input type=\"file\"> on macOS opens an \
                 OS-level file picker that blocks CDP for minutes; use \
                 browser_file_upload (trusted DOM.setFileInputFiles) \
                 instead of browser_click/browser_pointer for this element"
                    .into(),
            ));
        }
        Ok(())
    }

    async fn left_click_at(&self, x: f64, y: f64) -> Result<()> {
        self.left_click_at_on(&self.session_id, x, y).await
    }

    async fn left_click_at_on(&self, session_id: &str, x: f64, y: f64) -> Result<()> {
        tokio::time::sleep(Duration::from_millis(sample_lognormal_ms(
            34.0, 0.30, 12, 110,
        )))
        .await;
        self.mouse_button_on(session_id, "mousePressed", x, y, "left", (1, 1))
            .await?;
        tokio::time::sleep(Duration::from_millis(sample_lognormal_ms(
            62.0, 0.32, 20, 180,
        )))
        .await;
        self.mouse_button_on(session_id, "mouseReleased", x, y, "left", (0, 1))
            .await
    }

    pub(crate) async fn trusted_click_at(&self, x: f64, y: f64) -> Result<()> {
        if !x.is_finite() || !y.is_finite() || x < 0.0 || y < 0.0 {
            return Err(BrowserError::Protocol(
                "trusted click coordinates must be finite and non-negative".into(),
            ));
        }
        let _mutation = self.pointer_mutation.lock().await;
        self.human_move_to(x, y, 24.0).await?;
        self.left_click_at(x, y).await
    }

    pub(crate) async fn trusted_frame_click_at(
        &self,
        session_id: &str,
        root_point: (f64, f64),
        frame_point: (f64, f64),
    ) -> Result<()> {
        for value in [root_point.0, root_point.1, frame_point.0, frame_point.1] {
            if !value.is_finite() || value < 0.0 {
                return Err(BrowserError::Protocol(
                    "trusted frame click coordinates must be finite and non-negative".into(),
                ));
            }
        }
        let _mutation = self.pointer_mutation.lock().await;
        self.human_move_to(root_point.0, root_point.1, 24.0).await?;
        self.left_click_at_on(session_id, frame_point.0, frame_point.1)
            .await
    }

    pub(crate) async fn trusted_frame_hover_at(
        &self,
        session_id: &str,
        root_point: (f64, f64),
        frame_point: (f64, f64),
    ) -> Result<()> {
        for value in [root_point.0, root_point.1, frame_point.0, frame_point.1] {
            if !value.is_finite() || value < 0.0 {
                return Err(BrowserError::Protocol(
                    "trusted frame hover coordinates must be finite and non-negative".into(),
                ));
            }
        }
        let _mutation = self.pointer_mutation.lock().await;
        self.human_move_to(root_point.0, root_point.1, 24.0).await?;
        if session_id != self.session_id {
            self.client
                .send_on(
                    session_id,
                    "Input.dispatchMouseEvent",
                    json!({ "type": "mouseMoved", "x": frame_point.0, "y": frame_point.1 }),
                )
                .await?;
        }
        Ok(())
    }
}

fn same_document(origin: &ElementRef, destination: &ElementRef) -> bool {
    origin.document.is_some() && origin.document == destination.document
}

/// A `DOM.describeNode` `node`'s tag name, uppercased.
fn node_tag(node: &Value) -> String {
    node.get("nodeName")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_uppercase()
}

/// An attribute value from a `DOM.describeNode` `node`'s flat
/// `[name, value, name, value, ...]` attributes array.
fn node_attr<'a>(node: &'a Value, name: &str) -> Option<&'a str> {
    node.get("attributes")?
        .as_array()?
        .chunks(2)
        .find(|pair| pair.first().and_then(Value::as_str) == Some(name))
        .and_then(|pair| pair.get(1))
        .and_then(Value::as_str)
}

/// Whether a `DOM.describeNode` `node` payload is a `<select>` that Chrome
/// renders as a real popup menu on macOS (see `reject_if_native_modal_target`
/// for why that matters). `multiple`, `size > 1`, and `disabled` selects
/// don't open a native popup, so they're excluded here.
fn is_popup_capable_select(node: &Value) -> bool {
    if node_tag(node) != "SELECT" {
        return false;
    }
    let is_multiple = node_attr(node, "multiple").is_some();
    let size: i64 = node_attr(node, "size")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let is_disabled = node_attr(node, "disabled").is_some();
    select_is_popup_capable(is_multiple, size, is_disabled)
}

/// Whether a `DOM.describeNode` `node` payload is an enabled
/// `<input type="file">`, which opens a native OS file picker
/// (`NSOpenPanel` on macOS) on click — the same modal-run-loop hang risk as
/// [`is_popup_capable_select`]. A `disabled` file input opens nothing.
fn is_native_file_picker_input(node: &Value) -> bool {
    if node_tag(node) != "INPUT" {
        return false;
    }
    let type_attr = node_attr(node, "type").unwrap_or("text");
    if !type_attr.eq_ignore_ascii_case("file") {
        return false;
    }
    node_attr(node, "disabled").is_none()
}

/// Shared predicate behind [`is_popup_capable_select`] (mouse path, reads
/// `DOM.describeNode` attributes) and the keyboard-focus check in
/// `Page::reject_if_key_would_open_focused_select_popup` (reads
/// `document.activeElement` fields), so the two paths can't drift apart on
/// what counts as "the collapsed, enabled, single-option case".
pub(crate) fn select_is_popup_capable(is_multiple: bool, size: i64, is_disabled: bool) -> bool {
    !is_multiple && size <= 1 && !is_disabled
}

fn validate_request(request: &PointerRequest) -> Result<()> {
    let finite = |location: &PointerLocation| match location {
        PointerLocation::Element(_) => true,
        PointerLocation::Coordinates { x, y } => x.is_finite() && y.is_finite(),
    };
    if !finite(&request.origin)
        || request
            .destination
            .as_ref()
            .is_some_and(|value| !finite(value))
        || !request.delta_x.is_finite()
        || !request.delta_y.is_finite()
    {
        return Err(BrowserError::Protocol(
            "pointer coordinates and deltas must be finite".into(),
        ));
    }
    if request.action == PointerAction::Drag && request.destination.is_none() {
        return Err(BrowserError::Protocol("drag requires a destination".into()));
    }
    if request.action != PointerAction::Drag && request.destination.is_some() {
        return Err(BrowserError::Protocol(
            "only drag accepts a destination".into(),
        ));
    }
    if request.action == PointerAction::Scroll && request.delta_x == 0.0 && request.delta_y == 0.0 {
        return Err(BrowserError::Protocol(
            "scroll requires a non-zero delta".into(),
        ));
    }
    if request.action != PointerAction::Scroll && (request.delta_x != 0.0 || request.delta_y != 0.0)
    {
        return Err(BrowserError::Protocol("only scroll accepts deltas".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DocumentIdentity;

    fn request(action: PointerAction) -> PointerRequest {
        PointerRequest {
            action,
            origin: PointerLocation::Coordinates { x: 10.0, y: 20.0 },
            destination: None,
            delta_x: 0.0,
            delta_y: 0.0,
        }
    }

    #[test]
    fn parses_all_pointer_actions() {
        for action in [
            "click",
            "hover",
            "right_click",
            "double_click",
            "scroll",
            "drag",
        ] {
            assert!(PointerAction::parse(action).is_ok());
        }
        assert!(PointerAction::parse("triple_click").is_err());
    }

    #[test]
    fn scroll_requires_nonzero_finite_delta() {
        let mut value = request(PointerAction::Scroll);
        assert!(validate_request(&value).is_err());
        value.delta_y = 120.0;
        assert!(validate_request(&value).is_ok());
        value.delta_y = f64::NAN;
        assert!(validate_request(&value).is_err());
    }

    #[test]
    fn drag_requires_destination() {
        let mut value = request(PointerAction::Drag);
        assert!(validate_request(&value).is_err());
        value.destination = Some(PointerLocation::Coordinates { x: 30.0, y: 40.0 });
        assert!(validate_request(&value).is_ok());
    }

    #[test]
    fn drag_document_match_includes_loader_identity() {
        let element = |loader: &str| ElementRef {
            backend_node_id: 1,
            document: Some(DocumentIdentity {
                target_id: "target".into(),
                frame_id: "frame".into(),
                loader_id: loader.into(),
            }),
        };
        assert!(same_document(&element("loader-a"), &element("loader-a")));
        assert!(!same_document(&element("loader-a"), &element("loader-b")));
        let mut unproven = element("loader-a");
        unproven.document = None;
        assert!(!same_document(&unproven, &unproven));
    }

    fn describe_node(tag: &str, attrs: &[(&str, &str)]) -> Value {
        let flat: Vec<Value> = attrs
            .iter()
            .flat_map(|(k, v)| [json!(k), json!(v)])
            .collect();
        json!({ "nodeName": tag, "attributes": flat })
    }

    #[test]
    fn collapsed_single_select_is_popup_capable() {
        assert!(is_popup_capable_select(&describe_node("select", &[])));
        assert!(is_popup_capable_select(&describe_node(
            "SELECT",
            &[("id", "primaryCategory")]
        )));
    }

    #[test]
    fn non_select_tag_is_never_popup_capable() {
        assert!(!is_popup_capable_select(&describe_node("div", &[])));
        assert!(!is_popup_capable_select(&describe_node("input", &[])));
    }

    #[test]
    fn multiple_select_is_not_popup_capable() {
        assert!(!is_popup_capable_select(&describe_node(
            "select",
            &[("multiple", "")]
        )));
    }

    #[test]
    fn listbox_sized_select_is_not_popup_capable() {
        assert!(!is_popup_capable_select(&describe_node(
            "select",
            &[("size", "4")]
        )));
        // size="1" is explicitly still the collapsed popup form.
        assert!(is_popup_capable_select(&describe_node(
            "select",
            &[("size", "1")]
        )));
    }

    #[test]
    fn disabled_select_is_not_popup_capable() {
        assert!(!is_popup_capable_select(&describe_node(
            "select",
            &[("disabled", "")]
        )));
    }

    #[test]
    fn enabled_file_input_is_a_native_picker() {
        assert!(is_native_file_picker_input(&describe_node(
            "input",
            &[("type", "file")]
        )));
        // Case-insensitive, matches HTML attribute matching.
        assert!(is_native_file_picker_input(&describe_node(
            "INPUT",
            &[("type", "FILE")]
        )));
    }

    #[test]
    fn disabled_file_input_is_not_a_native_picker() {
        assert!(!is_native_file_picker_input(&describe_node(
            "input",
            &[("type", "file"), ("disabled", "")]
        )));
    }

    #[test]
    fn non_file_input_is_not_a_native_picker() {
        assert!(!is_native_file_picker_input(&describe_node(
            "input",
            &[("type", "text")]
        )));
        // No `type` attribute defaults to "text" per the HTML spec.
        assert!(!is_native_file_picker_input(&describe_node("input", &[])));
        assert!(!is_native_file_picker_input(&describe_node("select", &[])));
    }
}
