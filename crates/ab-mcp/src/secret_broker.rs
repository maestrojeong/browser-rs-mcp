use std::sync::Arc;

use anyhow::Context;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Clone)]
pub struct SecretBroker {
    socket_path: Arc<str>,
    token: Arc<str>,
}

#[derive(Debug)]
pub struct TransformedInput {
    pub value: Value,
    pub lease: String,
    pub boundary: Option<Value>,
}

#[derive(Deserialize)]
struct BrokerResponse {
    id: Option<String>,
    ok: bool,
    value: Option<Value>,
    lease: Option<String>,
    boundary: Option<Value>,
    error: Option<String>,
}

impl SecretBroker {
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        let socket_path = std::env::var("AB_SECRET_BROKER_SOCKET")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let token = std::env::var("AB_SECRET_BROKER_TOKEN")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        std::env::remove_var("AB_SECRET_BROKER_SOCKET");
        std::env::remove_var("AB_SECRET_BROKER_TOKEN");

        match (socket_path, token) {
            (None, None) => Ok(None),
            (Some(socket_path), Some(token)) => Ok(Some(Self {
                socket_path: socket_path.into(),
                token: token.into(),
            })),
            _ => anyhow::bail!(
                "AB_SECRET_BROKER_SOCKET and AB_SECRET_BROKER_TOKEN must be configured together"
            ),
        }
    }

    pub async fn transform_input(
        &self,
        tool: &str,
        value: Value,
    ) -> anyhow::Result<TransformedInput> {
        let response = self
            .request(json!({
                "op": "transform_input",
                "tool": tool,
                "value": value,
            }))
            .await?;
        Ok(TransformedInput {
            value: response
                .value
                .context("secret broker omitted transformed input")?,
            lease: response.lease.context("secret broker omitted lease")?,
            boundary: response.boundary,
        })
    }

    pub async fn redact_output(
        &self,
        lease: String,
        boundary: Option<Value>,
        value: Value,
    ) -> anyhow::Result<Value> {
        self.request(json!({
            "op": "redact_output",
            "lease": lease,
            "boundary": boundary,
            "value": value,
        }))
        .await?
        .value
        .context("secret broker omitted redacted output")
    }

    async fn request(&self, mut request: Value) -> anyhow::Result<BrokerResponse> {
        use tokio::time::{timeout, Duration};

        let request_id = random_id();
        request["id"] = Value::String(request_id.clone());
        request["token"] = Value::String(self.token.to_string());
        let encoded = serde_json::to_vec(&request)?;
        let response = timeout(Duration::from_secs(3), async {
            let stream = connect_broker(self.socket_path.as_ref()).await?;
            exchange(stream, &encoded).await
        })
        .await
        .context("secret broker timed out")??;
        anyhow::ensure!(
            response.id.as_deref() == Some(request_id.as_str()),
            "secret broker response id mismatch"
        );
        if !response.ok {
            anyhow::bail!(
                "secret broker rejected request: {}",
                response.error.as_deref().unwrap_or("unknown error")
            );
        }
        Ok(response)
    }
}

/// One newline-delimited JSON round trip, independent of the transport.
///
/// The framing is identical on every platform; only the connect differs, so
/// keeping the protocol here means a Unix run and a Windows run exercise the
/// same code.
async fn exchange<S>(stream: S, encoded: &[u8]) -> anyhow::Result<BrokerResponse>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let mut stream = stream;
    stream.write_all(encoded).await?;
    stream.write_all(b"\n").await?;
    stream.flush().await?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await?;
    anyhow::ensure!(!line.is_empty(), "secret broker closed without a response");
    serde_json::from_str::<BrokerResponse>(&line).map_err(Into::into)
}

#[cfg(unix)]
async fn connect_broker(path: &str) -> anyhow::Result<tokio::net::UnixStream> {
    Ok(tokio::net::UnixStream::connect(path).await?)
}

/// Windows has no Unix-domain socket in this position: the host listens on a
/// named pipe (`\\.\pipe\...`) and hands its name over the same
/// `AB_SECRET_BROKER_SOCKET` variable.
///
/// A pipe with every instance busy answers `ERROR_PIPE_BUSY` rather than
/// queueing, which is a "try again", not a failure — the host frees an
/// instance as soon as it finishes the request in flight. The enclosing 3s
/// timeout still bounds the retry loop.
#[cfg(windows)]
async fn connect_broker(
    path: &str,
) -> anyhow::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    use tokio::net::windows::named_pipe::ClientOptions;
    use tokio::time::{sleep, Duration};

    const ERROR_PIPE_BUSY: i32 = 231;

    loop {
        match ClientOptions::new().open(path) {
            Ok(client) => return Ok(client),
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                sleep(Duration::from_millis(20)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn random_id() -> String {
    use rand::{rngs::OsRng, RngCore};
    let mut bytes = [0_u8; 16];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(all(test, unix))]
mod tests {
    use std::sync::Arc;

    use serde_json::{json, Value};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    use super::SecretBroker;

    #[tokio::test]
    async fn broker_round_trip_preserves_lease_and_boundary() {
        let path = std::path::Path::new("/tmp")
            .join(format!("br-broker-{}.sock", &super::random_id()[..16]));
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            for operation in ["transform_input", "redact_output"] {
                let (stream, _) = listener.accept().await.unwrap();
                let mut line = String::new();
                let mut stream = BufReader::new(stream);
                stream.read_line(&mut line).await.unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["token"], "broker-token");
                assert_eq!(request["op"], operation);
                let response = if operation == "transform_input" {
                    json!({
                        "id": request["id"],
                        "ok": true,
                        "value": {"value": "secret"},
                        "lease": "lease-1",
                        "boundary": {"field": "text", "limit": 10}
                    })
                } else {
                    assert_eq!(request["lease"], "lease-1");
                    json!({
                        "id": request["id"],
                        "ok": true,
                        "value": {"content": [{"type": "text", "text": "[REDACTED]"}]}
                    })
                };
                stream
                    .get_mut()
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        let broker = SecretBroker {
            socket_path: Arc::from(path.to_string_lossy().as_ref()),
            token: Arc::from("broker-token"),
        };
        let transformed = broker
            .transform_input("browser_snapshot", json!({"value": "{{TOKEN}}"}))
            .await
            .unwrap();
        assert_eq!(transformed.value["value"], "secret");
        let secured = broker
            .redact_output(
                transformed.lease,
                transformed.boundary,
                json!({"content": []}),
            )
            .await
            .unwrap();
        assert_eq!(secured["content"][0]["text"], "[REDACTED]");
        server.await.unwrap();
        let _ = std::fs::remove_file(path);
    }
}
