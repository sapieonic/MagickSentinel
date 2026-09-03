//! REST client for the routes the agent needs (`contracts/openapi.yaml`).
//!
//! `POST /v1/sessions`, `GET /v1/policy`, `POST /v1/heartbeat`,
//! `DELETE /v1/sessions/current`, and the IdP token endpoint. Device-scoped routes
//! carry the device certificate as well as the bearer token: the gateway derives
//! `tenant_id` and `device_id` from the certificate and `user_uid` from the token, and
//! refuses the request if the two disagree.
//!
//! Behind a trait so the agent loop can be driven against a fake in CI.

use crate::auth::pkce::{percent_encode, OidcConfig};
use crate::auth::{AuthError, TokenEndpoint, TokenSet};
use crate::heartbeat::Heartbeat;
use sentinel_core::config::Policy;
use serde::Deserialize;
use std::time::Duration;

/// Request timeout. Long enough for a congested BPO uplink, short enough that a
/// heartbeat cannot pile up behind the previous one.
pub const TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("transport error: {0}")]
    Transport(String),
    #[error("unauthorized: the ID token was rejected")]
    Unauthorized,
    #[error("forbidden: the device is revoked or not permitted")]
    Forbidden,
    #[error("server returned {status}")]
    Status { status: u16 },
    #[error("malformed response: {0}")]
    Decode(String),
}

/// The heartbeat response (`openapi.yaml` → the 200 body of `POST /v1/heartbeat`).
#[derive(Debug, Clone, Deserialize)]
pub struct HeartbeatAck {
    pub policy_version: i64,
    pub server_time: String,
    #[serde(default)]
    pub commands: Vec<ServerCommand>,
}

/// An out-of-band instruction from the server.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ServerCommand {
    /// `refetch_policy` | `stop_capture` | `update_now` | `flush_spool`.
    pub kind: String,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionResponse {
    pub user: SessionUser,
    pub policy: Policy,
    pub server_time: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionUser {
    pub firebase_uid: String,
    pub tenant_id: String,
    pub role: String,
    pub display_name: String,
    pub status: String,
}

/// What the agent needs from the API.
pub trait SentinelApi: Send + Sync {
    fn open_session(&self, id_token: &str) -> Result<SessionResponse, ApiError>;
    fn get_policy(&self, id_token: &str) -> Result<Policy, ApiError>;
    fn heartbeat(&self, id_token: &str, body: &Heartbeat) -> Result<HeartbeatAck, ApiError>;
    fn end_session(&self, id_token: &str) -> Result<(), ApiError>;
}

/// The real client.
pub struct HttpApi {
    base_url: String,
    agent: ureq::Agent,
}

impl HttpApi {
    /// A client with no device certificate.
    ///
    /// `/v1/policy` and `/v1/heartbeat` sit behind `api.RequireDevice` and will answer
    /// 403 to this, which is the correct outcome for an unenrolled machine: the agent
    /// still runs, still shows a widget, and still spools, and the failure is visible
    /// rather than being a silent unauthenticated upload.
    pub fn new(base_url: &str) -> Result<Self, ApiError> {
        Self::build(base_url, None)
    }

    /// A client presenting the enrolled device certificate on every request.
    ///
    /// `credential` supplies both halves, and they are not symmetrical: the certificate
    /// chain is data, but the *key* may be a CNG handle this process cannot read, so it
    /// arrives as a `CryptoProvider` whose key provider yields the signer rather than
    /// as bytes. See `crate::device` for why that seam exists.
    pub fn with_device(
        base_url: &str,
        credential: &crate::device::DeviceCredential,
    ) -> Result<Self, ApiError> {
        Self::build(base_url, Some(credential))
    }

    fn build(
        base_url: &str,
        credential: Option<&crate::device::DeviceCredential>,
    ) -> Result<Self, ApiError> {
        let mut tls = ureq::tls::TlsConfig::builder();
        if let Some(credential) = credential {
            tls = tls
                .client_cert(Some(credential.ureq_client_cert()))
                // Marked "unversioned" by `ureq` because it exposes a `rustls` type
                // across ureq's semver boundary. Used deliberately: it is the only way
                // to give ureq a private key that cannot be expressed as DER, which is
                // exactly what a non-exportable CNG key is. A `ureq` upgrade that
                // changes this API is a compile error here, which is the failure mode
                // we want — the alternative is an agent that silently stops presenting
                // its certificate and starts getting 403s from device-scoped routes.
                .unversioned_rustls_crypto_provider(credential.ureq_crypto_provider());
        }
        let config = ureq::Agent::config_builder()
            .tls_config(tls.build())
            .timeout_global(Some(TIMEOUT))
            .user_agent(concat!("SentinelAgent/", env!("CARGO_PKG_VERSION")))
            .build();
        Ok(HttpApi {
            base_url: base_url.trim_end_matches('/').to_string(),
            agent: config.into(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }
}

/// Map a `ureq` failure onto our error type, keeping 401 and 403 distinct.
///
/// They mean very different things to the agent: 401 is "refresh the token and carry
/// on", 403 is "stop capture, an operator has to act". Collapsing them would either
/// keep recording after a revocation or sign the agent out on a routine expiry.
fn map_error(e: ureq::Error) -> ApiError {
    match e {
        ureq::Error::StatusCode(401) => ApiError::Unauthorized,
        ureq::Error::StatusCode(403) => ApiError::Forbidden,
        ureq::Error::StatusCode(s) => ApiError::Status { status: s },
        other => ApiError::Transport(other.to_string()),
    }
}

impl SentinelApi for HttpApi {
    fn open_session(&self, id_token: &str) -> Result<SessionResponse, ApiError> {
        self.agent
            .post(self.url("/v1/sessions"))
            .header("Authorization", &format!("Bearer {id_token}"))
            .send_empty()
            .map_err(map_error)?
            .body_mut()
            .read_json()
            .map_err(|e| ApiError::Decode(e.to_string()))
    }

    fn get_policy(&self, id_token: &str) -> Result<Policy, ApiError> {
        self.agent
            .get(self.url("/v1/policy"))
            .header("Authorization", &format!("Bearer {id_token}"))
            .call()
            .map_err(map_error)?
            .body_mut()
            .read_json()
            .map_err(|e| ApiError::Decode(e.to_string()))
    }

    fn heartbeat(&self, id_token: &str, body: &Heartbeat) -> Result<HeartbeatAck, ApiError> {
        self.agent
            .post(self.url("/v1/heartbeat"))
            .header("Authorization", &format!("Bearer {id_token}"))
            .send_json(body)
            .map_err(map_error)?
            .body_mut()
            .read_json()
            .map_err(|e| ApiError::Decode(e.to_string()))
    }

    fn end_session(&self, id_token: &str) -> Result<(), ApiError> {
        self.agent
            .delete(self.url("/v1/sessions/current"))
            .header("Authorization", &format!("Bearer {id_token}"))
            .call()
            .map_err(map_error)?;
        Ok(())
    }
}

/// The IdP's OAuth token endpoint.
pub struct HttpTokenEndpoint {
    agent: ureq::Agent,
}

impl Default for HttpTokenEndpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpTokenEndpoint {
    pub fn new() -> Self {
        HttpTokenEndpoint {
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(TIMEOUT))
                .build()
                .into(),
        }
    }

    fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<TokenSet, AuthError> {
        let body = encode_form(form);
        let mut response = self
            .agent
            .post(url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .send(&body)
            .map_err(|e| AuthError::Token(e.to_string()))?;
        let raw: TokenResponse = response
            .body_mut()
            .read_json()
            .map_err(|e| AuthError::Token(format!("malformed token response: {e}")))?;
        raw.into_token_set()
    }
}

/// `application/x-www-form-urlencoded`.
///
/// Percent-encoded rather than `+`-encoded even for spaces: the unreserved set is
/// accepted by every server, whereas `+` is only correct in a form body and some
/// strict endpoints hand back a scope list containing literal plus signs.
pub fn encode_form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

impl TokenResponse {
    fn into_token_set(self) -> Result<TokenSet, AuthError> {
        // Identity Platform returns `id_token`; a plain OIDC provider configured for
        // the same flow may only return `access_token`. The gateway verifies an ID
        // token, so prefer it and fail loudly rather than sending an access token
        // that will be rejected with a message about signatures.
        let id_token = self
            .id_token
            .or(self.access_token)
            .ok_or_else(|| AuthError::Token("token response carried no id_token".into()))?;
        let uid = subject_of(&id_token)
            .ok_or_else(|| AuthError::Token("id_token has no subject claim".into()))?;
        Ok(TokenSet {
            id_token,
            refresh_token: self.refresh_token,
            // A provider that omits `expires_in` is treated as issuing the standard
            // one-hour token; the refresh schedule then still lands inside it.
            expires_in_s: self.expires_in.unwrap_or(3600),
            uid,
        })
    }
}

/// Read the `sub` claim out of a JWT without verifying it.
///
/// The client does not and must not verify: the gateway does that on every request
/// against Google's public keys, and duplicating it here would mean shipping and
/// rotating a key set to 200 desktops. This is only used to label the local session,
/// so a forged token buys nothing — the server would reject it anyway.
pub fn subject_of(jwt: &str) -> Option<String> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;

    let payload = jwt.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    // `user_id` is Firebase's alias for `sub` in some token shapes.
    v.get("sub")
        .or_else(|| v.get("user_id"))
        .and_then(|s| s.as_str())
        .map(str::to_string)
}

impl TokenEndpoint for HttpTokenEndpoint {
    fn exchange_code(
        &self,
        cfg: &OidcConfig,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> Result<TokenSet, AuthError> {
        self.post_form(
            &cfg.token_endpoint,
            &[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect_uri),
                ("client_id", &cfg.client_id),
                // No client secret: RFC 8252 native clients are public, and a secret
                // shipped in an MSI to 200 desktops is not a secret. PKCE is what
                // replaces it.
                ("code_verifier", verifier),
            ],
        )
    }

    fn refresh(&self, cfg: &OidcConfig, refresh_token: &str) -> Result<TokenSet, AuthError> {
        self.post_form(
            &cfg.token_endpoint,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", &cfg.client_id),
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;

    fn jwt_with(payload: serde_json::Value) -> String {
        format!(
            "{}.{}.{}",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256"}"#),
            URL_SAFE_NO_PAD.encode(payload.to_string()),
            URL_SAFE_NO_PAD.encode(b"not-a-real-signature")
        )
    }

    #[test]
    fn the_subject_is_read_from_the_token_payload() {
        assert_eq!(
            subject_of(&jwt_with(serde_json::json!({"sub":"uid-abc","aud":"proj"}))).as_deref(),
            Some("uid-abc")
        );
        assert_eq!(
            subject_of(&jwt_with(serde_json::json!({"user_id":"uid-legacy"}))).as_deref(),
            Some("uid-legacy")
        );
    }

    #[test]
    fn a_malformed_token_yields_no_subject_rather_than_panicking() {
        for bad in ["", "a.b", "not-a-jwt", "a.!!!!.c", &jwt_with(serde_json::json!({}))] {
            assert_eq!(subject_of(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_token_response_without_expiry_assumes_the_standard_hour() {
        let ts = TokenResponse {
            id_token: Some(jwt_with(serde_json::json!({"sub":"u"}))),
            access_token: None,
            refresh_token: Some("rt".into()),
            expires_in: None,
        }
        .into_token_set()
        .unwrap();
        assert_eq!(ts.expires_in_s, 3600);
        assert_eq!(ts.uid, "u");
    }

    #[test]
    fn a_token_response_with_no_token_at_all_is_an_error() {
        let e = TokenResponse {
            id_token: None,
            access_token: None,
            refresh_token: Some("rt".into()),
            expires_in: Some(3600),
        }
        .into_token_set()
        .unwrap_err();
        assert!(matches!(e, AuthError::Token(_)));
    }

    #[test]
    fn form_encoding_escapes_the_reserved_characters() {
        let body = encode_form(&[
            ("grant_type", "authorization_code"),
            ("code", "4/0AY0e-Cg&x=1"),
            ("redirect_uri", "http://127.0.0.1:49712/callback"),
        ]);
        assert!(body.contains("code=4%2F0AY0e-Cg%26x%3D1"), "got {body}");
        assert!(body.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A49712%2Fcallback"));
        assert_eq!(body.matches('&').count(), 2, "only the separators are literal ampersands");
    }

    #[test]
    fn the_token_exchange_carries_the_verifier_and_no_client_secret() {
        // A client secret shipped in an MSI to 200 desktops is not a secret; PKCE is
        // what replaces it for a public native client.
        let cfg = OidcConfig {
            authorize_endpoint: "https://idp/auth".into(),
            token_endpoint: "https://idp/token".into(),
            client_id: "sentinel-desktop".into(),
            tenant_id: None,
            scopes: vec![],
        };
        let body = encode_form(&[
            ("grant_type", "authorization_code"),
            ("code", "the-code"),
            ("redirect_uri", "http://127.0.0.1:1/callback"),
            ("client_id", &cfg.client_id),
            ("code_verifier", "the-verifier"),
        ]);
        assert!(body.contains("code_verifier=the-verifier"));
        assert!(!body.contains("client_secret"));
    }

    #[test]
    fn unauthorized_and_forbidden_stay_distinct() {
        // 401 means refresh and carry on; 403 means stop capture. Collapsing them
        // either keeps recording after a revocation or signs the agent out on a
        // routine expiry.
        assert!(matches!(
            map_error(ureq::Error::StatusCode(401)),
            ApiError::Unauthorized
        ));
        assert!(matches!(
            map_error(ureq::Error::StatusCode(403)),
            ApiError::Forbidden
        ));
        assert!(matches!(
            map_error(ureq::Error::StatusCode(500)),
            ApiError::Status { status: 500 }
        ));
    }
}
