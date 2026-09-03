//! Unified browser-internal pointer actions.
//!
//! All input uses browser-generated CDP events. Synthetic DOM-event delivery is
//! intentionally absent because it exposes isTrusted == false to page scripts.

use std::time::Duration;

use serde::Serialize;
use serde_json::json;

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
}
