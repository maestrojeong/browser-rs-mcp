//! Minimal, high-performance Chrome DevTools Protocol client.
//!
//! One WebSocket multiplexes the browser target and every attached page
//! session (CDP "flatten" mode). We deliberately keep full control over which
//! CDP domains get enabled — this is what lets the stealth layer avoid the
//! `Runtime.enable` fingerprint that anti-bot systems watch for.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tokio::sync::{broadcast, oneshot, Mutex};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, trace, warn};

#[derive(Debug, thiserror::Error)]
pub enum CdpError {
    #[error("websocket connect failed: {0}")]
    Connect(String),
    #[error("transport closed")]
    Closed,
    #[error("request timed out after {0:?}")]
    Timeout(Duration),
    #[error("cdp protocol error: {0}")]
    Protocol(String),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, CdpError>;

/// A CDP event, optionally scoped to a page session.
#[derive(Debug, Clone)]
pub struct CdpEvent {
    pub session_id: Option<String>,
    pub method: String,
    pub params: Value,
}

type Pending = oneshot::Sender<Result<Value>>;

struct Inner {
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, Pending>>,
    sink: Mutex<
        Option<
            futures_util::stream::SplitSink<
                tokio_tungstenite::WebSocketStream<
                    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
                >,
                Message,
            >,
        >,
    >,
    events: broadcast::Sender<CdpEvent>,
    request_timeout: Duration,
}

#[derive(Clone)]
pub struct CdpClient {
    inner: Arc<Inner>,
}

impl CdpClient {
    /// Connect to a CDP WebSocket debugger URL (ws://host:port/devtools/browser/<id>).
    pub async fn connect(ws_url: &str) -> Result<Self> {
        let (ws, _resp) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| CdpError::Connect(e.to_string()))?;
        let (sink, mut stream) = ws.split();
        let (events_tx, _) = broadcast::channel(4096);

        let inner = Arc::new(Inner {
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            sink: Mutex::new(Some(sink)),
            events: events_tx,
            request_timeout: Duration::from_secs(30),
        });

        // Reader task: routes responses to waiters and events to the broadcast.
        let r = inner.clone();
        tokio::spawn(async move {
            while let Some(msg) = stream.next().await {
                match msg {
                    Ok(Message::Text(txt)) => dispatch(&r, &txt).await,
                    Ok(Message::Close(_)) => break,
                    Ok(_) => {}
                    Err(e) => {
                        warn!("cdp reader error: {e}");
                        break;
                    }
                }
            }
            // Fail all outstanding requests on disconnect.
            let mut pending = r.pending.lock().await;
            for (_, tx) in pending.drain() {
                let _ = tx.send(Err(CdpError::Closed));
            }
        });

        Ok(Self { inner })
    }

    /// Subscribe to the raw CDP event stream.
    pub fn events(&self) -> broadcast::Receiver<CdpEvent> {
        self.inner.events.subscribe()
    }

    /// Send a browser-scoped CDP command.
    pub async fn send(&self, method: &str, params: Value) -> Result<Value> {
        self.send_inner(method, params, None).await
    }

    /// Send a command scoped to a page/target session (flatten mode).
    pub async fn send_on(&self, session_id: &str, method: &str, params: Value) -> Result<Value> {
        self.send_inner(method, params, Some(session_id)).await
    }

    /// Typed convenience wrapper.
    pub async fn call<T: DeserializeOwned>(
        &self,
        session_id: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<T> {
        let v = self.send_inner(method, params, session_id).await?;
        Ok(serde_json::from_value(v)?)
    }

    async fn send_inner(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<Value> {
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(sid) = session_id {
            msg["sessionId"] = json!(sid);
        }

        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().await.insert(id, tx);

        {
            let mut guard = self.inner.sink.lock().await;
            let sink = guard.as_mut().ok_or(CdpError::Closed)?;
            let text = serde_json::to_string(&msg)?;
            trace!("-> {text}");
            sink.send(Message::Text(text))
                .await
                .map_err(|e| CdpError::Protocol(e.to_string()))?;
        }

        match tokio::time::timeout(self.inner.request_timeout, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err(CdpError::Closed),
            Err(_) => {
                self.inner.pending.lock().await.remove(&id);
                Err(CdpError::Timeout(self.inner.request_timeout))
            }
        }
    }
}

async fn dispatch(inner: &Arc<Inner>, txt: &str) {
    trace!("<- {txt}");
    let v: Value = match serde_json::from_str(txt) {
        Ok(v) => v,
        Err(e) => {
            warn!("cdp: bad json: {e}");
            return;
        }
    };

    // Response to a command (has "id").
    if let Some(id) = v.get("id").and_then(Value::as_u64) {
        if let Some(tx) = inner.pending.lock().await.remove(&id) {
            if let Some(err) = v.get("error") {
                let _ = tx.send(Err(CdpError::Protocol(err.to_string())));
            } else {
                let _ = tx.send(Ok(v.get("result").cloned().unwrap_or(Value::Null)));
            }
        }
        return;
    }

    // Otherwise it's an event.
    if let Some(method) = v.get("method").and_then(Value::as_str) {
        let ev = CdpEvent {
            session_id: v.get("sessionId").and_then(Value::as_str).map(String::from),
            method: method.to_string(),
            params: v.get("params").cloned().unwrap_or(Value::Null),
        };
        debug!("event {} (session={:?})", ev.method, ev.session_id);
        let _ = inner.events.send(ev);
    }
}
