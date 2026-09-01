//! The WebSocket transport for `WSS /v1/ingest`.
//!
//! mTLS with the device certificate plus `Authorization: Bearer <id token>`
//! (wire.md section 1). The gateway derives `tenant_id` and `device_id` from the
//! **certificate** and `user_uid` from the **token**, and closes with `4403` if the
//! two disagree — so both have to be right and neither can substitute for the other.
//!
//! Synchronous, because the uplink is one socket doing one thing and an async runtime
//! would buy nothing. [`Transport`] is a trait so the uplink can be driven against an
//! in-process fake gateway in CI.

use std::time::Duration;
use tungstenite::client::IntoClientRequest;
use tungstenite::Message;

/// Sub-protocol the gateway expects.
pub const SUBPROTOCOL: &str = "sentinel.v1";

/// Close codes from wire.md section 1.
pub mod close {
    /// Token expired or invalid: refresh and reconnect.
    pub const TOKEN_INVALID: u16 = 4401;
    /// Device revoked, tenant mismatch, or role not permitted. Terminal until an
    /// operator acts: the client MUST stop capture within 60 s.
    pub const FORBIDDEN: u16 = 4403;
    /// Idle timeout — no frames for 120 s. Reconnect lazily.
    pub const IDLE: u16 = 4408;
    /// Too many connections for this device. Back off; do not spin.
    pub const TOO_MANY: u16 = 4429;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    Text(String),
    Binary(Vec<u8>),
    Closed { code: Option<u16> },
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("connect failed: {0}")]
    Connect(String),
    #[error("the connection was closed with code {0:?}")]
    Closed(Option<u16>),
    #[error("transport error: {0}")]
    Io(String),
}

/// One connection to the ingest endpoint.
pub trait Transport: Send {
    fn send_text(&mut self, s: &str) -> Result<(), TransportError>;
    fn send_binary(&mut self, b: &[u8]) -> Result<(), TransportError>;
    /// Read the next message, or `None` if `timeout` elapsed with nothing to read.
    fn recv(&mut self, timeout: Duration) -> Result<Option<Incoming>, TransportError>;
    fn close(&mut self) -> Result<(), TransportError>;
}

/// Everything needed to open the socket.
#[derive(Debug, Clone)]
pub struct ConnectParams {
    /// `wss://…/v1/ingest` in production; `ws://…` is accepted only against a
    /// loopback address, for the CI gateway.
    pub url: String,
    pub bearer_token: String,
    /// PEM chain and key for the device certificate. `None` only for the loopback
    /// test gateway.
    pub client_cert: Option<ClientCertificate>,
}

#[derive(Debug, Clone)]
pub struct ClientCertificate {
    pub chain_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
}

/// Reject a plaintext WebSocket to anything that is not loopback.
///
/// The audio on this socket is call recordings and the header carries a bearer token.
/// A misconfigured `ws://` base URL must fail loudly at connect time rather than
/// quietly shipping both in the clear across a BPO's LAN.
pub fn validate_url(url: &str) -> Result<(), TransportError> {
    if url.starts_with("wss://") {
        return Ok(());
    }
    if let Some(rest) = url.strip_prefix("ws://") {
        let host = rest.split(['/', ':']).next().unwrap_or("");
        if host == "127.0.0.1" || host == "localhost" || host == "[::1]" {
            return Ok(());
        }
        return Err(TransportError::Connect(format!(
            "refusing an unencrypted uplink to {host}: audio and the bearer token \
             would cross the network in the clear"
        )));
    }
    Err(TransportError::Connect(format!("unsupported uplink scheme in {url}")))
}

/// The real transport.
pub struct WsTransport {
    socket: tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
}

impl WsTransport {
    pub fn connect(params: &ConnectParams) -> Result<Self, TransportError> {
        validate_url(&params.url)?;

        let mut request = params
            .url
            .as_str()
            .into_client_request()
            .map_err(|e| TransportError::Connect(e.to_string()))?;
        {
            let headers = request.headers_mut();
            headers.insert(
                "Authorization",
                format!("Bearer {}", params.bearer_token)
                    .parse()
                    .map_err(|_| TransportError::Connect("malformed bearer token".into()))?,
            );
            headers.insert(
                "Sec-WebSocket-Protocol",
                SUBPROTOCOL
                    .parse()
                    .map_err(|_| TransportError::Connect("bad subprotocol".into()))?,
            );
        }

        let connector = match &params.client_cert {
            Some(cert) => Some(tungstenite::Connector::Rustls(std::sync::Arc::new(
                build_mtls_config(cert)?,
            ))),
            None => None,
        };

        let (socket, _response) = tungstenite::client_tls_with_config(
            request,
            std::net::TcpStream::connect(host_port(&params.url)?)
                .map_err(|e| TransportError::Connect(e.to_string()))?,
            None,
            connector,
        )
        .map_err(|e| TransportError::Connect(e.to_string()))?;

        Ok(WsTransport { socket })
    }

    fn stream(&self) -> Option<&std::net::TcpStream> {
        match self.socket.get_ref() {
            tungstenite::stream::MaybeTlsStream::Plain(s) => Some(s),
            tungstenite::stream::MaybeTlsStream::Rustls(s) => Some(s.get_ref()),
            _ => None,
        }
    }
}

/// `host:port` for the TCP connect, defaulting the port by scheme.
fn host_port(url: &str) -> Result<String, TransportError> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| TransportError::Connect(format!("malformed url {url}")))?;
    let authority = rest.split('/').next().unwrap_or("");
    if authority.is_empty() {
        return Err(TransportError::Connect(format!("no host in {url}")));
    }
    if authority.contains(':') {
        Ok(authority.to_string())
    } else if scheme == "wss" {
        Ok(format!("{authority}:443"))
    } else {
        Ok(format!("{authority}:80"))
    }
}

/// rustls client config presenting the device certificate.
fn build_mtls_config(cert: &ClientCertificate) -> Result<rustls::ClientConfig, TransportError> {
    let mut roots = rustls::RootCertStore::empty();
    // The platform trust store: the gateway's server certificate chains to a public
    // CA, and a pinned private root would have to be rotated across 200 desktops.
    for c in rustls_native_certs()? {
        let _ = roots.add(c);
    }

    let chain: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert.chain_pem.as_slice())
            .collect::<Result<_, _>>()
            .map_err(|e| TransportError::Connect(format!("device certificate: {e}")))?;
    if chain.is_empty() {
        return Err(TransportError::Connect("device certificate chain is empty".into()));
    }
    let key = rustls_pemfile::private_key(&mut cert.key_pem.as_slice())
        .map_err(|e| TransportError::Connect(format!("device key: {e}")))?
        .ok_or_else(|| TransportError::Connect("no private key in the device key file".into()))?;

    rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(chain, key)
        .map_err(|e| TransportError::Connect(format!("mTLS configuration: {e}")))
}

fn rustls_native_certs() -> Result<Vec<rustls::pki_types::CertificateDer<'static>>, TransportError>
{
    let result = rustls_native_certs::load_native_certs();
    if result.certs.is_empty() {
        if let Some(e) = result.errors.into_iter().next() {
            return Err(TransportError::Connect(format!("no system trust roots: {e}")));
        }
    }
    Ok(result.certs)
}

impl Transport for WsTransport {
    fn send_text(&mut self, s: &str) -> Result<(), TransportError> {
        self.socket
            .send(Message::Text(s.into()))
            .map_err(|e| TransportError::Io(e.to_string()))
    }

    fn send_binary(&mut self, b: &[u8]) -> Result<(), TransportError> {
        self.socket
            .send(Message::Binary(b.to_vec().into()))
            .map_err(|e| TransportError::Io(e.to_string()))
    }

    fn recv(&mut self, timeout: Duration) -> Result<Option<Incoming>, TransportError> {
        // A read timeout on the underlying socket is how a synchronous WebSocket gets
        // a bounded `recv`. Without it the uplink thread blocks past the point where
        // it should be sending a heartbeat, and the gateway closes it with 4408.
        if let Some(s) = self.stream() {
            let _ = s.set_read_timeout(Some(timeout));
        }
        match self.socket.read() {
            Ok(Message::Text(t)) => Ok(Some(Incoming::Text(t.to_string()))),
            Ok(Message::Binary(b)) => Ok(Some(Incoming::Binary(b.to_vec()))),
            Ok(Message::Close(frame)) => Ok(Some(Incoming::Closed {
                code: frame.map(|f| u16::from(f.code)),
            })),
            // Ping/pong are handled by tungstenite; surface them as "nothing yet"
            // rather than as a message the uplink has to know about.
            Ok(_) => Ok(None),
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                Ok(Some(Incoming::Closed { code: None }))
            }
            Err(e) => Err(TransportError::Io(e.to_string())),
        }
    }

    fn close(&mut self) -> Result<(), TransportError> {
        let _ = self.socket.close(None);
        let _ = self.socket.flush();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plaintext_uplinks_are_refused_except_to_loopback() {
        // The socket carries call audio and a bearer token. A ws:// base URL that
        // slipped into tenant config must fail at connect, not ship both in the clear.
        assert!(validate_url("wss://api.sentinel.magickvoice.com/v1/ingest").is_ok());
        assert!(validate_url("ws://127.0.0.1:8080/v1/ingest").is_ok());
        assert!(validate_url("ws://localhost:8080/v1/ingest").is_ok());
        assert!(validate_url("ws://api.sentinel.magickvoice.com/v1/ingest").is_err());
        assert!(validate_url("ws://10.0.0.5:8080/v1/ingest").is_err());
        assert!(validate_url("https://api.example.com/v1/ingest").is_err());
        assert!(validate_url("").is_err());
    }

    #[test]
    fn the_default_port_follows_the_scheme() {
        assert_eq!(host_port("wss://api.example.com/v1/ingest").unwrap(), "api.example.com:443");
        assert_eq!(host_port("ws://127.0.0.1/x").unwrap(), "127.0.0.1:80");
        assert_eq!(host_port("ws://127.0.0.1:9001/x").unwrap(), "127.0.0.1:9001");
        assert!(host_port("not a url").is_err());
    }

    #[test]
    fn the_close_codes_match_the_wire_contract() {
        assert_eq!(close::TOKEN_INVALID, 4401);
        assert_eq!(close::FORBIDDEN, 4403);
        assert_eq!(close::IDLE, 4408);
        assert_eq!(close::TOO_MANY, 4429);
    }
}
