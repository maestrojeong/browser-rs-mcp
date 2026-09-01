use std::{collections::hash_map::Entry, collections::HashMap, sync::Arc};

use axum::{
    extract::Request,
    http::{header::HeaderName, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use tokio::sync::Mutex;

pub const CAPABILITY_HEADER: &str = "x-browser-capability";
const OWNER_HEADER: &str = "x-browser-owner";
const SESSION_HEADER: &str = "mcp-session-id";

#[derive(Clone)]
pub struct HttpSecurity {
    pub managed: bool,
    root_capability: Option<Arc<str>>,
    spawn_nonce: Option<Arc<str>>,
    session_owners: Arc<Mutex<HashMap<String, String>>>,
}

struct RequestAuth {
    path: String,
    provided: Option<String>,
    owner: Option<String>,
    session_id: Option<String>,
}

fn request_auth(request: &Request) -> RequestAuth {
    RequestAuth {
        path: request.uri().path().to_string(),
        provided: request
            .headers()
            .get(CAPABILITY_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
        owner: request_owner(request),
        session_id: request_session(request),
    }
}

fn is_mcp_path(path: &str) -> bool {
    path == "/mcp" || path.starts_with("/mcp/")
}

impl HttpSecurity {
    pub fn from_env(is_loopback: bool) -> anyhow::Result<Self> {
        let managed = std::env::var("AB_MANAGED").is_ok_and(|value| value == "1");
        let root_capability = std::env::var("AB_HTTP_CAPABILITY")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(Arc::<str>::from);
        let spawn_nonce = std::env::var("AB_SPAWN_NONCE")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(Arc::<str>::from);

        if managed && root_capability.is_none() {
            anyhow::bail!("AB_HTTP_CAPABILITY is required in managed mode");
        }
        if managed && spawn_nonce.is_none() {
            anyhow::bail!("AB_SPAWN_NONCE is required in managed mode");
        }
        if root_capability.is_none() && !is_loopback {
            anyhow::bail!("AB_HTTP_CAPABILITY is required when binding outside loopback");
        }

        // Chrome and renderer descendants must not inherit server credentials.
        std::env::remove_var("AB_HTTP_CAPABILITY");
        std::env::remove_var("AB_SPAWN_NONCE");

        Ok(Self {
            managed,
            root_capability,
            spawn_nonce,
            session_owners: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn health(&self) -> serde_json::Value {
        if self.managed {
            serde_json::json!({
                "ok": true,
                "name": "negotium-browser-gateway",
                "backend": "browser-rs",
                "version": env!("CARGO_PKG_VERSION"),
                "spawnNonce": self.spawn_nonce.as_deref(),
            })
        } else {
            serde_json::json!({
                "ok": true,
                "name": "browser-rs",
                "version": env!("CARGO_PKG_VERSION"),
            })
        }
    }

    async fn authorize_request(&self, auth: RequestAuth) -> Result<Option<String>, Box<Response>> {
        let RequestAuth {
            path,
            provided,
            owner,
            session_id,
        } = auth;
        if path == "/health" || (self.managed && path == "/message") {
            return Ok(None);
        }

        if !self.managed {
            if let Some(expected) = self.root_capability.as_deref() {
                if !provided
                    .as_deref()
                    .is_some_and(|value| constant_time_eq(value, expected))
                {
                    return Err(Box::new(unauthorized("invalid browser capability")));
                }
            }
            return Ok(None);
        }

        let Some(root) = self.root_capability.as_deref() else {
            return Err(Box::new(unauthorized("browser capability is unavailable")));
        };
        if path == "/owners" {
            if !provided
                .as_deref()
                .is_some_and(|value| constant_time_eq(value, root))
            {
                return Err(Box::new(unauthorized(
                    "invalid browser administrative capability",
                )));
            }
            return Ok(None);
        }

        let Some(owner) = owner else {
            return Err(Box::new(
                (StatusCode::BAD_REQUEST, "browser owner is required").into_response(),
            ));
        };
        let expected = owner_capability(root, &owner);
        if !provided
            .as_deref()
            .is_some_and(|value| constant_time_eq(value, &expected))
        {
            return Err(Box::new(unauthorized("invalid browser owner capability")));
        }

        if let Some(session_id) = session_id {
            let owners = self.session_owners.lock().await;
            if owners.get(&session_id) != Some(&owner) {
                return Err(Box::new(
                    (
                        StatusCode::FORBIDDEN,
                        "browser owner does not match this session",
                    )
                        .into_response(),
                ));
            }
        }
        Ok(Some(owner))
    }

    async fn bind_session(&self, session_id: String, owner: String) -> bool {
        match self.session_owners.lock().await.entry(session_id) {
            Entry::Vacant(entry) => {
                entry.insert(owner);
                true
            }
            Entry::Occupied(entry) => entry.get() == &owner,
        }
    }
}

pub fn owner_capability(root: &str, owner: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(root.as_bytes()).expect("HMAC accepts keys of every length");
    mac.update(owner.as_bytes());
    hex_lower(&mac.finalize().into_bytes())
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.as_bytes()
        .iter()
        .zip(right.as_bytes())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

pub fn request_owner(request: &Request) -> Option<String> {
    let header_owner = request
        .headers()
        .get(OWNER_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let query_owner = request.uri().query().and_then(|query| {
        url::form_urlencoded::parse(query.as_bytes())
            .find(|(key, _)| key == "owner")
            .map(|(_, value)| value.into_owned())
    });
    canonical_owner(header_owner, query_owner)
}

pub fn canonical_owner(
    header_owner: Option<String>,
    query_owner: Option<String>,
) -> Option<String> {
    header_owner
        .or(query_owner)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty() && value.len() <= 256)
}

fn request_session(request: &Request) -> Option<String> {
    request
        .headers()
        .get(SESSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn unauthorized(message: &'static str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({ "ok": false, "error": message })),
    )
        .into_response()
}

pub async fn authorize_http(security: HttpSecurity, request: Request, next: Next) -> Response {
    let path = request.uri().path().to_string();
    let request_session = request_session(&request);
    let owner = match security.authorize_request(request_auth(&request)).await {
        Ok(owner) => owner,
        Err(response) => return *response,
    };

    let method = request.method().clone();
    let mut response = next.run(request).await;
    if is_mcp_path(&path) {
        if response.status().is_success() {
            if let (Some(session_id), Some(owner)) = (
                response
                    .headers()
                    .get(HeaderName::from_static(SESSION_HEADER))
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string),
                owner,
            ) {
                if !security.bind_session(session_id, owner).await {
                    response = (
                        StatusCode::FORBIDDEN,
                        "browser session is already bound to another owner",
                    )
                        .into_response();
                }
            }
        }
        if method == Method::DELETE && response.status().is_success() {
            if let Some(session_id) = request_session {
                security.session_owners.lock().await.remove(&session_id);
            }
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use axum::{body::Body, extract::Request, http::StatusCode};

    use super::{
        canonical_owner, is_mcp_path, owner_capability, request_auth, HttpSecurity,
        CAPABILITY_HEADER,
    };

    fn managed_security() -> HttpSecurity {
        HttpSecurity {
            managed: true,
            root_capability: Some("root-secret".into()),
            spawn_nonce: Some("spawn-nonce".into()),
            session_owners: Default::default(),
        }
    }

    #[test]
    fn owner_capability_matches_node_hmac_sha256() {
        assert_eq!(
            owner_capability("secret-capability", "topic:한국어"),
            "a00b7023e8a55e13b0a977bde802dc1b8e61dfbbf195fa3d669bf0c1d734783a"
        );
    }

    #[test]
    fn canonical_owner_uses_header_precedence_and_rejects_invalid_values() {
        assert_eq!(
            canonical_owner(Some(" owner-a ".into()), Some("owner-b".into())).as_deref(),
            Some("owner-a")
        );
        assert_eq!(
            canonical_owner(None, Some(" owner-b ".into())).as_deref(),
            Some("owner-b")
        );
        assert_eq!(
            canonical_owner(Some(" ".into()), Some("owner-b".into())),
            None
        );
        assert_eq!(canonical_owner(Some("x".repeat(257)), None), None);
    }

    #[test]
    fn all_paths_under_the_mcp_mount_are_session_bound() {
        assert!(is_mcp_path("/mcp"));
        assert!(is_mcp_path("/mcp/"));
        assert!(is_mcp_path("/mcp/anything"));
        assert!(!is_mcp_path("/mcproxy"));
    }

    #[tokio::test]
    async fn owners_endpoint_accepts_only_the_root_capability() {
        let security = managed_security();
        let owner_token = owner_capability("root-secret", "owner-a");
        let owner_request = Request::builder()
            .uri("/owners?owner=owner-a")
            .header(CAPABILITY_HEADER, owner_token)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            security
                .authorize_request(request_auth(&owner_request))
                .await
                .unwrap_err()
                .status(),
            StatusCode::UNAUTHORIZED
        );

        let root_request = Request::builder()
            .uri("/owners?owner=owner-a")
            .header(CAPABILITY_HEADER, "root-secret")
            .body(Body::empty())
            .unwrap();
        assert!(security
            .authorize_request(request_auth(&root_request))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn streamable_sessions_cannot_be_rebound_or_used_by_another_owner() {
        let security = managed_security();
        assert!(
            security
                .bind_session("session-a".into(), "owner-a".into())
                .await
        );
        assert!(
            !security
                .bind_session("session-a".into(), "owner-b".into())
                .await
        );

        let request = Request::builder()
            .uri("/mcp?owner=owner-b")
            .header(
                CAPABILITY_HEADER,
                owner_capability("root-secret", "owner-b"),
            )
            .header("mcp-session-id", "session-a")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            security
                .authorize_request(request_auth(&request))
                .await
                .unwrap_err()
                .status(),
            StatusCode::FORBIDDEN
        );
    }
}
