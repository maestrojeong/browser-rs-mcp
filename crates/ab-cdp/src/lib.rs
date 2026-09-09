//! Minimal, high-performance Chrome DevTools Protocol client.
//!
//! One WebSocket multiplexes the browser target and every attached page
//! session (CDP "flatten" mode). We deliberately keep full control over which
//! CDP domains get enabled — this is what lets browser-rs avoid the
//! `Runtime.enable` fingerprint that anti-bot systems watch for.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::Duration;

use arc_swap::ArcSwapOption;
use futures_util::{SinkExt, StreamExt};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};
use tokio::sync::{broadcast, oneshot, Mutex};
use tokio::time::{timeout_at, Instant};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};

#[derive(Debug, thiserror::Error)]
pub enum CdpError {
    #[error("websocket connect failed: {0}")]
    Connect(String),
    #[error("transport closed")]
    Closed,
    #[error("request timed out after {0:?}")]
    Timeout(Duration),
    #[error("session command stalled after {0:?}")]
    Stalled(Duration),
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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CdpTransportError {
    pub kind: String,
    pub phase: String,
    pub method: Option<String>,
    pub at_ms: u64,
}

type Pending = oneshot::Sender<Result<Value>>;
type CdpSink = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;

struct Inner {
    next_id: AtomicU64,
    /// Liveness of the underlying WebSocket. Set to `false` the moment the
    /// reader observes a close/error, a write fails, a writer stalls, or the
    /// diagnostic probe confirms an unanswered command reflects transport
    /// loss. Read atomically so health never contends on `sink`/`pending`.
    connected: AtomicBool,
    suspect: AtomicBool,
    pending: StdMutex<HashMap<u64, Pending>>,
    sink: Mutex<Option<CdpSink>>,
    events: broadcast::Sender<CdpEvent>,
    request_timeout: Duration,
    transport_timeout: Duration,
    probe_timeout: Duration,
    probe_inflight: AtomicBool,
    reader_cancel: CancellationToken,
    last_error: ArcSwapOption<CdpTransportError>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.reader_cancel.cancel();
    }
}

impl Inner {
    /// Permanently invalidate this transport and wake every registered request.
    ///
    /// `held_sink` lets a sender reuse the same operation without reacquiring a
    /// lock it already owns. Other callers only try the sink lock: invalidation
    /// must never become a new unbounded wait behind a stalled writer.
    async fn invalidate(self: &Arc<Self>, held_sink: Option<&mut Option<CdpSink>>) {
        {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            self.connected.store(false, Ordering::SeqCst);
            self.suspect.store(false, Ordering::Release);
            for (_, tx) in pending.drain() {
                let _ = tx.send(Err(CdpError::Closed));
            }
        }
        self.reader_cancel.cancel();

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

    fn record_error(&self, kind: &str, phase: &str, method: Option<&str>) {
        let at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.last_error.store(Some(Arc::new(CdpTransportError {
            kind: kind.to_string(),
            phase: phase.to_string(),
            method: method.map(str::to_string),
            at_ms,
        })));
    }
}

#[derive(Clone, Copy)]
enum ResponseTimeoutPolicy {
    Probe { session_scoped: bool },
    Invalidate,
}

struct PendingRegistration {
    inner: Weak<Inner>,
    id: u64,
}

impl Drop for PendingRegistration {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            inner
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&self.id);
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
        const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
        let (ws, _resp) =
            tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(ws_url))
                .await
                .map_err(|_| CdpError::Connect(format!("timed out after {CONNECT_TIMEOUT:?}")))?
                .map_err(|e| CdpError::Connect(e.to_string()))?;
        let (sink, mut stream) = ws.split();
        let (events_tx, _) = broadcast::channel(4096);

        let inner = Arc::new(Inner {
            next_id: AtomicU64::new(1),
            connected: AtomicBool::new(true),
            suspect: AtomicBool::new(false),
            pending: StdMutex::new(HashMap::new()),
            sink: Mutex::new(Some(sink)),
            events: events_tx,
            request_timeout: Duration::from_secs(30),
            transport_timeout: Duration::from_secs(10),
            probe_timeout: Duration::from_secs(5),
            probe_inflight: AtomicBool::new(false),
            reader_cancel: CancellationToken::new(),
            last_error: ArcSwapOption::empty(),
        });

        // Reader task: routes responses to waiters and events to the broadcast.
        let reader = Arc::downgrade(&inner);
        let cancel = inner.reader_cancel.clone();
        tokio::spawn(async move {
            loop {
                let msg = tokio::select! {
                    _ = cancel.cancelled() => return,
                    msg = stream.next() => msg,
                };
                let Some(msg) = msg else { break };
                match msg {
                    Ok(Message::Text(txt)) => {
                        let Some(inner) = reader.upgrade() else {
                            return;
                        };
                        dispatch(&inner, &txt).await;
                    }
                    Ok(Message::Close(_)) => break,
                    Ok(_) => {}
                    Err(e) => {
                        if let Some(inner) = reader.upgrade() {
                            inner.record_error("closed", "reader", None);
                        }
                        warn!("cdp reader error: {e}");
                        break;
                    }
                }
            }
            // Chrome is gone (close frame, transport error, or EOF).
            if let Some(inner) = reader.upgrade() {
                inner.record_error("closed", "reader", None);
                inner.invalidate(None).await;
            }
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

    pub fn is_suspect(&self) -> bool {
        self.inner.suspect.load(Ordering::Acquire)
    }

    pub fn last_transport_error(&self) -> Option<CdpTransportError> {
        self.inner
            .last_error
            .load_full()
            .map(|error| error.as_ref().clone())
    }

    /// Subscribe to the raw CDP event stream.
    pub fn events(&self) -> broadcast::Receiver<CdpEvent> {
        self.inner.events.subscribe()
    }

    /// Send a browser-scoped CDP command.
    pub async fn send(&self, method: &str, params: Value) -> Result<Value> {
        self.send_inner(
            method,
            params,
            None,
            self.inner.request_timeout,
            ResponseTimeoutPolicy::Probe {
                session_scoped: false,
            },
        )
        .await
    }

    /// Send a command scoped to a page/target session (flatten mode).
    pub async fn send_on(&self, session_id: &str, method: &str, params: Value) -> Result<Value> {
        self.send_inner(
            method,
            params,
            Some(session_id),
            self.inner.request_timeout,
            ResponseTimeoutPolicy::Probe {
                session_scoped: true,
            },
        )
        .await
    }

    /// Typed convenience wrapper.
    pub async fn call<T: DeserializeOwned>(
        &self,
        session_id: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<T> {
        let policy = ResponseTimeoutPolicy::Probe {
            session_scoped: session_id.is_some(),
        };
        let v = self
            .send_inner(
                method,
                params,
                session_id,
                self.inner.request_timeout,
                policy,
            )
            .await?;
        Ok(serde_json::from_value(v)?)
    }

    async fn send_inner(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
        request_timeout: Duration,
        response_policy: ResponseTimeoutPolicy,
    ) -> Result<Value> {
        // One deadline covers queueing for the shared writer, writing the
        // command, and waiting for Chrome's response. Starting a fresh timeout
        // only after `sink.send()` allowed a blocked writer to hang forever.
        let started_at = Instant::now();
        let deadline = started_at + request_timeout;
        let transport_deadline = (started_at + self.inner.transport_timeout).min(deadline);
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
            let mut pending = self.inner.pending.lock().unwrap_or_else(|e| e.into_inner());
            if !self.inner.connected.load(Ordering::SeqCst) {
                return Err(CdpError::Closed);
            }
            pending.insert(id, tx);
        }
        let _registration = PendingRegistration {
            inner: Arc::downgrade(&self.inner),
            id,
        };

        {
            let mut guard = match timeout_at(transport_deadline, self.inner.sink.lock()).await {
                Ok(guard) => guard,
                Err(_) => {
                    self.inner
                        .record_error("timeout", "writer-lock", Some(method));
                    warn!(
                        request_id = id,
                        method,
                        session_id,
                        timeout_ms = self.inner.transport_timeout.as_millis() as u64,
                        phase = "writer-lock",
                        "cdp request timed out; invalidating transport"
                    );
                    self.inner.invalidate(None).await;
                    return Err(CdpError::Timeout(self.inner.transport_timeout));
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
            match timeout_at(transport_deadline, sink.send(Message::Text(text))).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    self.inner.record_error("write", "write", Some(method));
                    // A write failure means the socket is unusable even if the
                    // reader task has not observed the close yet.
                    self.inner.invalidate(Some(&mut *guard)).await;
                    debug!("cdp write failed, marking transport closed: {e}");
                    return Err(CdpError::Closed);
                }
                Err(_) => {
                    self.inner.record_error("timeout", "write", Some(method));
                    warn!(
                        request_id = id,
                        method,
                        session_id,
                        timeout_ms = self.inner.transport_timeout.as_millis() as u64,
                        phase = "write",
                        "cdp request timed out; invalidating transport"
                    );
                    self.inner.invalidate(Some(&mut *guard)).await;
                    return Err(CdpError::Timeout(self.inner.transport_timeout));
                }
            }
        }

        match timeout_at(deadline, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err(CdpError::Closed),
            Err(_) => {
                self.inner.record_error("timeout", "response", Some(method));
                warn!(
                    request_id = id,
                    method,
                    session_id,
                    timeout_ms = request_timeout.as_millis() as u64,
                    phase = "response",
                    "cdp response timed out"
                );
                match response_policy {
                    ResponseTimeoutPolicy::Probe { session_scoped } => {
                        self.inner.suspect.store(true, Ordering::Release);
                        self.spawn_probe_if_idle();
                        if session_scoped {
                            Err(CdpError::Stalled(request_timeout))
                        } else {
                            Err(CdpError::Timeout(request_timeout))
                        }
                    }
                    ResponseTimeoutPolicy::Invalidate => {
                        self.inner.invalidate(None).await;
                        Err(CdpError::Timeout(request_timeout))
                    }
                }
            }
        }
    }

    fn spawn_probe_if_idle(&self) {
        if self
            .inner
            .probe_inflight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }

        let client = self.clone();
        tokio::spawn(async move {
            let timeout = client.inner.probe_timeout;
            let result = client
                .send_inner(
                    "Browser.getVersion",
                    json!({}),
                    None,
                    timeout,
                    ResponseTimeoutPolicy::Invalidate,
                )
                .await;
            match result {
                Ok(_) => {
                    client.inner.suspect.store(false, Ordering::Release);
                    debug!("transport probe succeeded after response timeout");
                }
                Err(error) => {
                    warn!(%error, "transport probe failed after response timeout");
                    client.inner.invalidate(None).await;
                }
            }
            client.inner.probe_inflight.store(false, Ordering::Release);
        });
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
        if let Some(tx) = inner
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id)
        {
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

    #[tokio::test]
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
            let mut pending = client
                .inner
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            pending.insert(10_001, waiter_one_tx);
            pending.insert(10_002, waiter_two_tx);
        }

        let first_client = client.clone();
        let first = tokio::spawn(async move { first_client.send("Test.first", json!({})).await });
        requests.recv().await.unwrap();
        drop(client.inner.sink.lock().await);
        tokio::time::pause();

        tokio::time::advance(Duration::from_secs(30)).await;
        tokio::task::yield_now().await;

        assert!(matches!(
            first.await.unwrap(),
            Err(CdpError::Timeout(duration)) if duration == Duration::from_secs(30)
        ));

        // A browser-scoped response timeout is only suspicion. The dedicated
        // five-second probe must fail before the shared transport is closed.
        requests.recv().await.unwrap();
        drop(client.inner.sink.lock().await);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        assert!(matches!(
            waiter_one_rx.await.unwrap(),
            Err(CdpError::Closed)
        ));
        assert!(matches!(
            waiter_two_rx.await.unwrap(),
            Err(CdpError::Closed)
        ));
        assert!(!client.is_connected());
        assert!(client.inner.reader_cancel.is_cancelled());
        assert!(client
            .inner
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty());
        assert!(client.inner.sink.lock().await.is_none());
        assert!(matches!(
            client.send("Test.later", json!({})).await,
            Err(CdpError::Closed)
        ));

        server.abort();
    }

    #[tokio::test]
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
        tokio::time::pause();
        let blocked_client = client.clone();
        let blocked =
            tokio::spawn(async move { blocked_client.send("Test.blocked", json!({})).await });

        // Wait until the request is registered and blocked on the held writer
        // lock before advancing its single end-to-end deadline.
        loop {
            if !client
                .inner
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(10)).await;
        tokio::task::yield_now().await;

        assert!(matches!(
            blocked.await.unwrap(),
            Err(CdpError::Timeout(duration)) if duration == Duration::from_secs(10)
        ));
        assert!(!client.is_connected());
        assert!(client
            .inner
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty());

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

    #[tokio::test]
    async fn session_stall_keeps_other_owners_connected() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (method_tx, mut methods) = mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(text))) = websocket.next().await {
                let request: Value = serde_json::from_str(&text).unwrap();
                let method = request["method"].as_str().unwrap().to_string();
                if method != "Test.stalledPage" {
                    websocket
                        .send(Message::Text(
                            json!({ "id": request["id"], "result": {} }).to_string(),
                        ))
                        .await
                        .unwrap();
                }
                method_tx.send(method).unwrap();
            }
        });

        let client = CdpClient::connect(&format!("ws://{addr}")).await.unwrap();
        let stalled_client = client.clone();
        let stalled = tokio::spawn(async move {
            stalled_client
                .send_on("owner-a", "Test.stalledPage", json!({}))
                .await
        });
        assert_eq!(methods.recv().await.as_deref(), Some("Test.stalledPage"));
        drop(client.inner.sink.lock().await);
        tokio::time::pause();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(30)).await;
        tokio::task::yield_now().await;

        assert!(matches!(
            stalled.await.unwrap(),
            Err(CdpError::Stalled(duration)) if duration == Duration::from_secs(30)
        ));
        assert!(client.is_connected());

        tokio::time::resume();
        assert_eq!(methods.recv().await.as_deref(), Some("Browser.getVersion"));
        while client.inner.probe_inflight.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        assert!(client.is_connected());

        let healthy_client = client.clone();
        let healthy = tokio::spawn(async move {
            healthy_client
                .send_on("owner-b", "Test.followup", json!({}))
                .await
        });
        assert_eq!(methods.recv().await.as_deref(), Some("Test.followup"));
        assert_eq!(healthy.await.unwrap().unwrap(), json!({}));
        assert!(client.is_connected());

        server.abort();
    }

    #[tokio::test]
    async fn cancelling_a_request_removes_its_pending_entry() {
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
        let pending_client = client.clone();
        let request = tokio::spawn(async move {
            pending_client
                .send_on("owner-a", "Test.cancelled", json!({}))
                .await
        });
        requests.recv().await.unwrap();
        request.abort();
        let _ = request.await;

        let pending_ids: Vec<u64> = client
            .inner
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .copied()
            .collect();
        assert!(pending_ids.is_empty(), "pending ids: {pending_ids:?}");
        assert!(client.is_connected());
        server.abort();
    }
}
