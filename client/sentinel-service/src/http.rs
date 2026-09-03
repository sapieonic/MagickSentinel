//! The service's HTTP client: the enrollment exchange and the OTLP relay.
//!
//! Synchronous `ureq`, matching the rest of the client — `AGENTS.md` is explicit that
//! the client is blocking by design and that tokio is not to be introduced. Both
//! requests here are one round trip on a schedule; there is nothing for an async
//! runtime to overlap.
//!
//! Neither request presents a client certificate, and for different reasons:
//!
//! * **Enrollment** cannot: the machine has no certificate yet, which is the whole
//!   point of the exchange. `/v1/devices/enroll` is one of the two routes the gateway
//!   serves without mTLS, and it is why the listener uses
//!   `tls.VerifyClientCertIfGiven` rather than `RequireAndVerifyClientCert`.
//! * **The OTLP relay** does not need to: telemetry is diagnostic, it carries no call
//!   content, and requiring the device credential would mean the exporter could not
//!   report the one class of failure most worth reporting — a device whose credential
//!   is missing or broken. The relay is expected to accept the device certificate when
//!   one is present and to attribute the records to that device; see
//!   `LocalConfig::OTLP_RELAY_PATH`.

use crate::enroll::{EnrollError, EnrollRequest, EnrollResponse, EnrollTransport};
use std::time::Duration;

/// Request timeout. Long enough for a congested BPO uplink at install time, short
/// enough that a silent black hole does not hold an MSI's deferred action open.
pub const TIMEOUT: Duration = Duration::from_secs(30);

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .user_agent(concat!("SentinelService/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// `POST /v1/devices/enroll` over TLS.
pub struct HttpEnrollTransport {
    agent: ureq::Agent,
}

impl Default for HttpEnrollTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpEnrollTransport {
    pub fn new() -> Self {
        HttpEnrollTransport { agent: agent() }
    }
}

impl EnrollTransport for HttpEnrollTransport {
    fn post(&self, url: &str, body: &EnrollRequest) -> Result<EnrollResponse, EnrollError> {
        // The distinction this match exists to preserve: the enrollment token is
        // consumed atomically server-side *before* the certificate is signed, so a
        // retry with the same token cannot succeed once the handler has seen it. Only
        // a transport-level failure — DNS, connect, TLS — is safe to retry, and only
        // that maps to `Unreachable`. Every status code maps to something terminal,
        // including 500: the handler may have consumed the token and failed after.
        let mut response = match self.agent.post(url).send_json(body) {
            Ok(r) => r,
            Err(ureq::Error::StatusCode(401)) => return Err(EnrollError::TokenUnusable),
            Err(ureq::Error::StatusCode(503)) => {
                // `no_ca`. The gateway has no certificate authority configured, which
                // `docs/security.md` records as the outstanding gap: `main.go` never
                // sets `Server.CA`, so a production gateway answers 503 to every
                // enrollment. Distinguished because it is an operator's problem on the
                // server and not a bad token on the endpoint.
                return Err(EnrollError::NoCertificateAuthority);
            }
            Err(ureq::Error::StatusCode(status)) => {
                return Err(EnrollError::Rejected { status, code: String::new() })
            }
            Err(other) => return Err(EnrollError::Unreachable(other.to_string())),
        };
        response
            .body_mut()
            .read_json()
            .map_err(|e| EnrollError::Decode(e.to_string()))
    }
}

/// `POST` of an OTLP payload, used by [`crate::telemetry`].
///
/// Returns the HTTP status on a rejection rather than an error type of its own: the
/// exporter's only decisions are "drop this batch" and "log that the endpoint is
/// rejecting us", and it makes both from a status code.
pub struct HttpOtlpShipper {
    agent: ureq::Agent,
    url: String,
}

impl HttpOtlpShipper {
    pub fn new(url: String) -> Self {
        HttpOtlpShipper { agent: agent(), url }
    }
}

impl crate::telemetry::Shipper for HttpOtlpShipper {
    fn ship(&self, payload: &[u8]) -> Result<(), String> {
        self.agent
            .post(&self.url)
            // OTLP/HTTP with JSON encoding. The protobuf encoding is the other half of
            // the same specification; JSON is chosen because it needs no code
            // generation step and no protobuf runtime in a binary that is shipped to
            // 200 desktops and signed with an EV certificate.
            .header("Content-Type", "application/json")
            .send(payload)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn endpoint(&self) -> &str {
        &self.url
    }
}
