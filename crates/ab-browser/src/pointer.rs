//! Unified browser-internal pointer actions.
//!
//! Trusted input retains browser-rs's humanized CDP path. DOM events are an
//! explicit, untrusted background compatibility route and never an automatic
//! fallback.

use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::{rand_u64, BrowserError, ElementRef, Page, Result};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputRoute {
    Trusted,
    DomEvent,
}

impl InputRoute {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "trusted" => Ok(Self::Trusted),
            "dom_event" => Ok(Self::DomEvent),
            _ => Err(BrowserError::Protocol(format!(
                "unknown input route {value:?}"
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
    pub route: InputRoute,
    pub origin: PointerLocation,
    pub destination: Option<PointerLocation>,
    pub delta_x: f64,
    pub delta_y: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PointerOutcome {
    pub action: PointerAction,
    pub route: InputRoute,
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
        match request.route {
            InputRoute::Trusted => self.dispatch_trusted_pointer(request).await,
            InputRoute::DomEvent => self.dispatch_dom_pointer(request).await,
        }
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

    async fn point_for_location(&self, location: &PointerLocation) -> Result<(f64, f64)> {
        match location {
            PointerLocation::Coordinates { x, y } => Ok((*x, *y)),
            PointerLocation::Element(element) => {
                self.scroll_into_view(element.backend_node_id).await;
                self.node_center(element.backend_node_id)
                    .await?
                    .ok_or_else(|| {
                        BrowserError::Protocol("pointer target has no layout box".into())
                    })
            }
        }
    }

    async fn dispatch_trusted_pointer(&self, request: &PointerRequest) -> Result<PointerOutcome> {
        let ((x, y), drag_destination) = if request.action == PointerAction::Drag {
            let destination = request.destination.as_ref().expect("validated drag");
            if let PointerLocation::Element(element) = destination {
                self.scroll_into_view(element.backend_node_id).await;
            }
            if let PointerLocation::Element(element) = &request.origin {
                self.scroll_into_view(element.backend_node_id).await;
            }
            let origin = self.point_without_scroll(&request.origin).await?;
            let destination = self.point_without_scroll(destination).await?;
            self.require_visible_hit(&request.origin, origin).await?;
            self.require_visible_hit(request.destination.as_ref().unwrap(), destination)
                .await?;
            (origin, Some(destination))
        } else {
            (self.point_for_location(&request.origin).await?, None)
        };
        self.require_visible_hit(&request.origin, (x, y)).await?;
        self.human_move_to(x, y).await?;

        match request.action {
            PointerAction::Click => {
                tokio::time::sleep(Duration::from_millis(rand_u64(20, 70))).await;
                self.mouse_button("mousePressed", x, y, "left", 1, 1)
                    .await?;
                tokio::time::sleep(Duration::from_millis(rand_u64(40, 110))).await;
                self.mouse_button("mouseReleased", x, y, "left", 0, 1)
                    .await?;
            }
            PointerAction::Hover => {}
            PointerAction::RightClick => {
                tokio::time::sleep(Duration::from_millis(rand_u64(20, 70))).await;
                self.mouse_button("mousePressed", x, y, "right", 2, 1)
                    .await?;
                tokio::time::sleep(Duration::from_millis(rand_u64(40, 110))).await;
                self.mouse_button("mouseReleased", x, y, "right", 0, 1)
                    .await?;
            }
            PointerAction::DoubleClick => {
                for count in [1, 2] {
                    self.mouse_button("mousePressed", x, y, "left", 1, count)
                        .await?;
                    tokio::time::sleep(Duration::from_millis(rand_u64(35, 95))).await;
                    self.mouse_button("mouseReleased", x, y, "left", 0, count)
                        .await?;
                    if count == 1 {
                        tokio::time::sleep(Duration::from_millis(rand_u64(55, 145))).await;
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
                let (to_x, to_y) = drag_destination.expect("validated drag point");
                self.mouse_button("mousePressed", x, y, "left", 1, 1)
                    .await?;
                for step in 1..=8 {
                    let progress = f64::from(step) / 8.0;
                    self.client
                        .send_on(
                            &self.session_id,
                            "Input.dispatchMouseEvent",
                            json!({
                                "type": "mouseMoved",
                                "x": x + (to_x - x) * progress,
                                "y": y + (to_y - y) * progress,
                                "button": "left", "buttons": 1,
                            }),
                        )
                        .await?;
                    tokio::time::sleep(Duration::from_millis(rand_u64(8, 22))).await;
                }
                self.mouse_button("mouseReleased", to_x, to_y, "left", 0, 1)
                    .await?;
                *self.pointer.lock().unwrap() = Some((to_x, to_y));
            }
        }

        Ok(PointerOutcome {
            action: request.action,
            route: request.route,
            trusted: true,
            dispatched: true,
            observed: "unknown",
            retryable: false,
        })
    }

    async fn point_without_scroll(&self, location: &PointerLocation) -> Result<(f64, f64)> {
        match location {
            PointerLocation::Coordinates { x, y } => Ok((*x, *y)),
            PointerLocation::Element(element) => self
                .node_center(element.backend_node_id)
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
                    "drag endpoint is outside the visible hit target".into(),
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
        self.client
            .send_on(
                &self.session_id,
                "Input.dispatchMouseEvent",
                json!({
                    "type": kind, "x": x, "y": y, "button": button,
                    "buttons": buttons, "clickCount": click_count,
                }),
            )
            .await?;
        Ok(())
    }

    async fn dispatch_dom_pointer(&self, request: &PointerRequest) -> Result<PointerOutcome> {
        let PointerLocation::Element(origin) = &request.origin else {
            return Err(BrowserError::Protocol(
                "dom_event pointer actions require a snapshot ref".into(),
            ));
        };
        let object_id = self
            .resolve_object(origin.backend_node_id)
            .await?
            .ok_or_else(|| BrowserError::Protocol("pointer ref has no live object".into()))?;

        let (function, arguments) = match request.action {
            PointerAction::Click => (
                "function(){const el=this;const base={bubbles:true,cancelable:true,composed:true,button:0,pointerId:1,pointerType:'mouse',isPrimary:true,view:el.ownerDocument.defaultView};const down={...base,buttons:1};const up={...base,buttons:0};el.dispatchEvent(new PointerEvent('pointerover',down));el.dispatchEvent(new PointerEvent('pointerenter',{...down,bubbles:false}));el.dispatchEvent(new MouseEvent('mouseover',down));el.dispatchEvent(new PointerEvent('pointerdown',down));el.dispatchEvent(new MouseEvent('mousedown',down));try{el.focus&&el.focus();}catch(_){}el.dispatchEvent(new PointerEvent('pointerup',up));el.dispatchEvent(new MouseEvent('mouseup',up));el.dispatchEvent(new MouseEvent('click',up));return true;}",
                json!([]),
            ),
            PointerAction::Hover => (
                "function(){const o={bubbles:true,composed:true,view:this.ownerDocument.defaultView};this.dispatchEvent(new PointerEvent('pointerover',o));this.dispatchEvent(new PointerEvent('pointerenter',{...o,bubbles:false}));this.dispatchEvent(new MouseEvent('mouseover',o));this.dispatchEvent(new MouseEvent('mouseenter',{...o,bubbles:false}));return true;}",
                json!([]),
            ),
            PointerAction::RightClick => (
                "function(){const o={bubbles:true,cancelable:true,composed:true,button:2,buttons:2,view:this.ownerDocument.defaultView};this.dispatchEvent(new PointerEvent('pointerdown',o));this.dispatchEvent(new MouseEvent('mousedown',o));this.dispatchEvent(new MouseEvent('mouseup',{...o,buttons:0}));this.dispatchEvent(new PointerEvent('pointerup',{...o,buttons:0}));this.dispatchEvent(new MouseEvent('contextmenu',{...o,buttons:0}));return true;}",
                json!([]),
            ),
            PointerAction::DoubleClick => (
                "function(){const o={bubbles:true,cancelable:true,composed:true,button:0,view:this.ownerDocument.defaultView};for(let detail=1;detail<=2;detail++){this.dispatchEvent(new PointerEvent('pointerdown',{...o,buttons:1,detail}));this.dispatchEvent(new MouseEvent('mousedown',{...o,buttons:1,detail}));this.dispatchEvent(new MouseEvent('mouseup',{...o,buttons:0,detail}));this.dispatchEvent(new PointerEvent('pointerup',{...o,buttons:0,detail}));this.dispatchEvent(new MouseEvent('click',{...o,buttons:0,detail}));}this.dispatchEvent(new MouseEvent('dblclick',{...o,buttons:0,detail:2}));return true;}",
                json!([]),
            ),
            PointerAction::Scroll => (
                "function(dx,dy){const o={deltaX:dx,deltaY:dy,bubbles:true,cancelable:true,composed:true,view:this.ownerDocument.defaultView};this.dispatchEvent(new WheelEvent('wheel',o));let n=this;while(n&&n!==this.ownerDocument.documentElement){const s=this.ownerDocument.defaultView.getComputedStyle(n);if(/(auto|scroll)/.test(s.overflow+s.overflowX+s.overflowY))break;n=n.parentElement;}const target=n||this.ownerDocument.scrollingElement||this.ownerDocument.documentElement;const bx=target.scrollLeft,by=target.scrollTop;target.scrollBy(dx,dy);return target.scrollLeft!==bx||target.scrollTop!==by;}",
                json!([{ "value": request.delta_x }, { "value": request.delta_y }]),
            ),
            PointerAction::Drag => {
                let destination = request.destination.as_ref().expect("validated drag");
                let args = match destination {
                    PointerLocation::Element(element) => {
                        let destination_id = self
                            .resolve_object(element.backend_node_id)
                            .await?
                            .ok_or_else(|| BrowserError::Protocol("drag destination is stale".into()))?;
                        json!([{ "objectId": destination_id }, { "value": Value::Null }, { "value": Value::Null }])
                    }
                    PointerLocation::Coordinates { x, y } => {
                        json!([{ "value": Value::Null }, { "value": x }, { "value": y }])
                    }
                };
                (
                    "function(destination,x,y){const doc=this.ownerDocument,dest=destination||doc.elementFromPoint(x,y);if(!dest||dest.ownerDocument!==doc)return false;let data=null;try{data=new DataTransfer();}catch(_){}const o={bubbles:true,cancelable:true,composed:true};this.dispatchEvent(new PointerEvent('pointerdown',{...o,button:0,buttons:1}));this.dispatchEvent(new MouseEvent('mousedown',{...o,button:0,buttons:1}));this.dispatchEvent(new DragEvent('dragstart',{...o,dataTransfer:data}));dest.dispatchEvent(new DragEvent('dragenter',{...o,dataTransfer:data}));dest.dispatchEvent(new DragEvent('dragover',{...o,dataTransfer:data}));dest.dispatchEvent(new DragEvent('drop',{...o,dataTransfer:data}));this.dispatchEvent(new DragEvent('dragend',{...o,dataTransfer:data}));dest.dispatchEvent(new MouseEvent('mouseup',{...o,button:0,buttons:0}));dest.dispatchEvent(new PointerEvent('pointerup',{...o,button:0,buttons:0}));return true;}",
                    args,
                )
            }
        };

        let response = self
            .client
            .send_on(
                &self.session_id,
                "Runtime.callFunctionOn",
                json!({
                    "objectId": object_id,
                    "functionDeclaration": function,
                    "arguments": arguments,
                    "returnByValue": true,
                }),
            )
            .await?;
        if let Some(exception) = response.get("exceptionDetails") {
            return Err(BrowserError::Protocol(format!(
                "DOM pointer event raised an exception: {exception}"
            )));
        }
        let result = response.pointer("/result/value").and_then(Value::as_bool);
        if matches!(request.action, PointerAction::Scroll | PointerAction::Drag)
            && result != Some(true)
        {
            let reason = if request.action == PointerAction::Scroll {
                "synthetic scroll target did not move"
            } else {
                "synthetic drag destination did not resolve in the same document"
            };
            return Err(BrowserError::Protocol(reason.into()));
        }

        Ok(PointerOutcome {
            action: request.action,
            route: request.route,
            trusted: false,
            dispatched: true,
            observed: if request.action == PointerAction::Scroll {
                "changed"
            } else {
                "unknown"
            },
            retryable: false,
        })
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
    if request.route == InputRoute::DomEvent
        && !matches!(request.origin, PointerLocation::Element(_))
    {
        return Err(BrowserError::Protocol(
            "dom_event requires a snapshot ref".into(),
        ));
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
            route: InputRoute::Trusted,
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
    fn dom_event_rejects_coordinates() {
        let mut value = request(PointerAction::Hover);
        value.route = InputRoute::DomEvent;
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
