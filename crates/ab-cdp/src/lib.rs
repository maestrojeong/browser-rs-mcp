//! Minimal, high-performance Chrome DevTools Protocol client.
//!
//! One WebSocket multiplexes the browser target and every attached page
//! session (CDP "flatten" mode). We deliberately keep full control over which
//! CDP domains get enabled — this is what lets browser-rs avoid the
//! `Runtime.enable` fingerprint that anti-bot systems watch for.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    /// Liveness of the underlying WebSocket. Set to `false` the moment the
    /// reader task observes a close/error, or a write fails. Read with a
    /// relaxed atomic load so health probes never contend on `sink`/`pending`
    /// — a probe must not block behind an in-flight CDP request.
    connected: AtomicBool,
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
            connected: AtomicBool::new(true),
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
            // Chrome is gone (close frame, transport error, or EOF).
            //
            // Publish the disconnection *while holding `pending`*. A sender
            // checks `connected` under the same lock before registering, so
            // every request either lands before this drain and is woken here,
            // or observes the closed flag and fails immediately. Storing the
            // flag outside the lock left a window where a waiter was inserted
            // after the drain and then slept until the 30s request timeout.
            {
                let mut pending = r.pending.lock().await;
                r.connected.store(false, Ordering::SeqCst);
                for (_, tx) in pending.drain() {
                    let _ = tx.send(Err(CdpError::Closed));
                }
            }
            // Release the dead sink. Keeping it as `Some` meant later writes
            // reached a closed socket and produced a transport-level error in
            // place of a clear `Closed`.
            r.sink.lock().await.take();
        });

        Ok(Self { inner })
    }

    /// Whether the CDP transport is still usable.
    ///
    /// Lock-free by design: health endpoints call this while a tool request
    /// may be holding `sink`/`pending`, so it must never block. A `false`
    /// here means Chrome is gone and every later request will fail — the
    /// process needs a new browser, not a retry.
    ///
    /// The load is `SeqCst` to stay ordered against the reader task, which
    /// publishes the disconnection under the `pending` lock.
    pub fn is_connected(&self) -> bool {
        self.inner.connected.load(Ordering::SeqCst)
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

        // Register under the same lock the reader uses to publish a
        // disconnection, so this request cannot slip in behind the drain.
        // Checking `connected` before taking the lock would reintroduce that
        // window: the reader could close and drain in between.
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.inner.pending.lock().await;
            if !self.inner.connected.load(Ordering::SeqCst) {
                return Err(CdpError::Closed);
            }
            pending.insert(id, tx);
        }

        {
            let mut guard = self.inner.sink.lock().await;
            let sink = guard.as_mut().ok_or(CdpError::Closed)?;
            let text = serde_json::to_string(&msg)?;
            trace!("-> {text}");
            if let Err(e) = sink.send(Message::Text(text)).await {
                // A write failure means the socket is unusable even if the
                // reader task has not observed the close yet. Latch it and
                // wake *every* waiter: the reader may never see an EOF on a
                // half-open socket, so leaving the others registered would
                // park them until the 30s timeout each.
                {
                    let mut pending = self.inner.pending.lock().await;
                    self.inner.connected.store(false, Ordering::SeqCst);
                    for (_, waiter) in pending.drain() {
                        let _ = waiter.send(Err(CdpError::Closed));
                    }
                }
                *guard = None;
                debug!("cdp write failed, marking transport closed: {e}");
                return Err(CdpError::Closed);
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Supervisors (clawgram, negotium) pattern-match this string to tell
    /// "Chrome is gone, replace the process" apart from a retryable hiccup.
    /// Renaming it silently breaks their crash detection, so pin it here.
    #[test]
    fn closed_transport_error_is_stable_and_distinct() {
        assert_eq!(CdpError::Closed.to_string(), "transport closed");
        assert_ne!(
            CdpError::Closed.to_string(),
            CdpError::Protocol("Trying to work with closed connection".into()).to_string(),
        );
    }
}
