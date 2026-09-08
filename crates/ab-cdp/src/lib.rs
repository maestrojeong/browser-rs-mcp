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
use tokio::time::{timeout_at, Instant};
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
type CdpSink = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;

struct Inner {
    next_id: AtomicU64,
    /// Liveness of the underlying WebSocket. Set to `false` the moment the
    /// reader task observes a close/error, a write fails, or a request times
    /// out. Read atomically so health probes never contend on `sink`/`pending`
    /// -- a probe must not block behind an in-flight CDP request.
    connected: AtomicBool,
    pending: Mutex<HashMap<u64, Pending>>,
    sink: Mutex<Option<CdpSink>>,
    events: broadcast::Sender<CdpEvent>,
    request_timeout: Duration,
}

impl Inner {
    /// Permanently invalidate this transport and wake every registered request.
    ///
    /// `held_sink` lets a sender reuse the same operation without reacquiring a
    /// lock it already owns. Other callers only try the sink lock: invalidation
    /// must never become a new unbounded wait behind a stalled writer.
    async fn invalidate(self: &Arc<Self>, held_sink: Option<&mut Option<CdpSink>>) {
        {
            let mut pending = self.pending.lock().await;
            self.connected.store(false, Ordering::SeqCst);
            for (_, tx) in pending.drain() {
                let _ = tx.send(Err(CdpError::Closed));
            }
        }

        match held_sink {
            Some(sink) => {
                sink.take();
            }
            None => {
                if let Ok(mut sink) = self.sink.try_lock() {
                    sink.take();
                } else {
                    // Cleanup must not delay the caller, but the sink should
                    // still be released once the current writer leaves its
                    // critical section.
                    let inner = Arc::clone(self);
                    tokio::spawn(async move {
                        inner.sink.lock().await.take();
                    });
                }
            }
        }
    }
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
            r.invalidate(None).await;
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
    /// The load is `SeqCst` to stay ordered against transport invalidation,
    /// which publishes the disconnection under the `pending` lock.
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
        // One deadline covers queueing for the shared writer, writing the
        // command, and waiting for Chrome's response. Starting a fresh timeout
        // only after `sink.send()` allowed a blocked writer to hang forever.
        let deadline = Instant::now() + self.inner.request_timeout;
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
            let mut guard = match timeout_at(deadline, self.inner.sink.lock()).await {
                Ok(guard) => guard,
                Err(_) => {
                    warn!(
                        request_id = id,
                        method,
                        session_id,
                        timeout_ms = self.inner.request_timeout.as_millis() as u64,
                        phase = "writer-lock",
                        "cdp request timed out; invalidating transport"
                    );
                    self.inner.invalidate(None).await;
                    return Err(CdpError::Timeout(self.inner.request_timeout));
                }
            };
            if !self.inner.connected.load(Ordering::SeqCst) {
                return Err(CdpError::Closed);
            }
            let Some(sink) = guard.as_mut() else {
                self.inner.invalidate(Some(&mut *guard)).await;
                return Err(CdpError::Closed);
            };
            let text = serde_json::to_string(&msg)?;
            trace!("-> {text}");
            match timeout_at(deadline, sink.send(Message::Text(text))).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    // A write failure means the socket is unusable even if the
                    // reader task has not observed the close yet.
                    self.inner.invalidate(Some(&mut *guard)).await;
                    debug!("cdp write failed, marking transport closed: {e}");
                    return Err(CdpError::Closed);
                }
                Err(_) => {
                    warn!(
                        request_id = id,
                        method,
                        session_id,
                        timeout_ms = self.inner.request_timeout.as_millis() as u64,
                        phase = "write",
                        "cdp request timed out; invalidating transport"
                    );
                    self.inner.invalidate(Some(&mut *guard)).await;
                    return Err(CdpError::Timeout(self.inner.request_timeout));
                }
            }
        }

        match timeout_at(deadline, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err(CdpError::Closed),
            Err(_) => {
                // An unanswered command means this transport can no longer be
                // trusted. Invalidate every shared client instead of leaving
                // them attached to a half-open socket.
                warn!(
                    request_id = id,
                    method,
                    session_id,
                    timeout_ms = self.inner.request_timeout.as_millis() as u64,
                    phase = "response",
                    "cdp request timed out; invalidating transport"
                );
                self.inner.invalidate(None).await;
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
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;
    use tokio_tungstenite::accept_async;

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

    #[tokio::test(start_paused = true)]
    async fn request_timeout_invalidates_transport_and_releases_all_waiters() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (request_tx, mut requests) = mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            while let Some(message) = websocket.next().await {
                if message.unwrap().is_text() {
                    request_tx.send(()).unwrap();
                }
            }
        });

        let client = CdpClient::connect(&format!("ws://{addr}")).await.unwrap();
        let (waiter_one_tx, waiter_one_rx) = oneshot::channel();
        let (waiter_two_tx, waiter_two_rx) = oneshot::channel();
        {
            let mut pending = client.inner.pending.lock().await;
            pending.insert(10_001, waiter_one_tx);
            pending.insert(10_002, waiter_two_tx);
        }

        let first_client = client.clone();
        let first = tokio::spawn(async move { first_client.send("Test.first", json!({})).await });
        requests.recv().await.unwrap();
        drop(client.inner.sink.lock().await);

        tokio::time::advance(Duration::from_secs(30)).await;
        tokio::task::yield_now().await;

        assert!(matches!(
            first.await.unwrap(),
            Err(CdpError::Timeout(duration)) if duration == Duration::from_secs(30)
        ));
        assert!(matches!(
            waiter_one_rx.await.unwrap(),
            Err(CdpError::Closed)
        ));
        assert!(matches!(
            waiter_two_rx.await.unwrap(),
            Err(CdpError::Closed)
        ));
        assert!(!client.is_connected());
        assert!(client.inner.pending.lock().await.is_empty());
        assert!(client.inner.sink.lock().await.is_none());
        assert!(matches!(
            client.send("Test.later", json!({})).await,
            Err(CdpError::Closed)
        ));

        server.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn writer_lock_timeout_returns_without_waiting_for_the_lock_holder() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            while websocket.next().await.is_some() {}
        });

        let client = CdpClient::connect(&format!("ws://{addr}")).await.unwrap();
        let sink_guard = client.inner.sink.lock().await;
        let blocked_client = client.clone();
        let blocked =
            tokio::spawn(async move { blocked_client.send("Test.blocked", json!({})).await });

        // Wait until the request is registered and blocked on the held writer
        // lock before advancing its single end-to-end deadline.
        loop {
            if !client.inner.pending.lock().await.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(30)).await;
        tokio::task::yield_now().await;

        assert!(matches!(
            blocked.await.unwrap(),
            Err(CdpError::Timeout(duration)) if duration == Duration::from_secs(30)
        ));
        assert!(!client.is_connected());
        assert!(client.inner.pending.lock().await.is_empty());

        // Invalidation returns before this guard is released. Its detached
        // cleanup then removes the sink as soon as the holder exits.
        drop(sink_guard);
        tokio::task::yield_now().await;
        assert!(client.inner.sink.lock().await.is_none());
        assert!(matches!(
            client.send("Test.later", json!({})).await,
            Err(CdpError::Closed)
        ));

        server.abort();
    }
}
