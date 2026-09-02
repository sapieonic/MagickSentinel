//! `SentinelAgent.exe` — the user-session process.
//!
//! Launched by the service at logon (never from a `Run` key, which the user can
//! delete). One instance per interactive session, guarded by a session-local named
//! mutex.
//!
//! This file is deliberately thin: it resolves configuration, builds the concrete
//! implementations, and hands them to [`sentinel_agent::agent::Agent`], which owns the
//! ordering. Everything worth testing lives in the library.

use sentinel_agent::agent::{Agent, AgentIdentityInfo};
use sentinel_agent::api::{HttpApi, HttpTokenEndpoint, SentinelApi};
use sentinel_agent::auth::pkce::OidcConfig;
use sentinel_agent::auth::{AuthService, TokenStore};
use sentinel_agent::ipc::ServiceClient;
use sentinel_agent::uplink::transport::{ConnectParams, ClientCertificate, Transport, TransportError, WsTransport};
use sentinel_agent::uplink::{TransportFactory, Uplink};
use sentinel_agent::widget::{HeadlessWidget, WidgetShell};
use sentinel_capture::source::CaptureSource;
use sentinel_core::config::LocalConfig;
use sentinel_core::protocol::CaptureTier;
use sentinel_core::spool::Spool;
use sentinel_service::ipc::{ConfigSnapshot, Request, Response};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How often the loop turns. 20 ms is one Opus frame: the pipeline reads a frame per
/// tick, so a slower loop would starve capture and a faster one would spin on an
/// endpoint buffer that has nothing in it yet.
const TICK: Duration = Duration::from_millis(20);

fn main() -> anyhow::Result<()> {
    sentinel_agent::init_logging();

    let _instance = match sentinel_agent::instance::acquire() {
        Ok(g) => g,
        Err(e) => {
            // Not a non-zero exit: the service can legitimately race a relaunch
            // against an agent that has not finished exiting, and an error here would
            // be counted as a crash and backed off.
            tracing::info!(reason = %e, "another agent already owns this session; exiting");
            return Ok(());
        }
    };

    let snapshot = load_config();
    tracing::info!(
        tier = ?snapshot.capture_tier,
        os_build = %snapshot.os_build,
        version = sentinel_agent::VERSION,
        "sentinel agent starting"
    );
    run(snapshot)
}

/// Ask the service for machine config, falling back to defaults.
///
/// Falling back rather than failing: the agent must reach the point of showing a
/// widget even when the service is not up, or an agent whose service failed to start
/// sees nothing at all and reports the product as broken.
fn load_config() -> ConfigSnapshot {
    match service_client().request(Request::GetConfig) {
        Ok(Response::Config(c)) => *c,
        Ok(_) | Err(_) => {
            tracing::warn!("service config unavailable; continuing on local defaults");
            ConfigSnapshot {
                local: LocalConfig::default(),
                policy: None,
                capture_tier: None,
                os_build: "0.0.0".into(),
                service_version: "unknown".into(),
                agent_restarts: 0,
            }
        }
    }
}

fn service_client() -> Box<dyn ServiceClient> {
    #[cfg(windows)]
    {
        Box::new(sentinel_agent::ipc::PipeServiceClient)
    }
    #[cfg(not(windows))]
    {
        Box::new(sentinel_agent::ipc::NullServiceClient)
    }
}

/// Opens ingest sockets on demand.
struct WssFactory {
    url: String,
    /// Read fresh on every connect: the ID token is refreshed every 50 minutes, and a
    /// reconnect after that must not present the old one.
    token: Arc<Mutex<String>>,
    cert: Option<ClientCertificate>,
}

impl TransportFactory for WssFactory {
    fn connect(&mut self) -> Result<Box<dyn Transport>, TransportError> {
        Ok(Box::new(WsTransport::connect(&ConnectParams {
            url: self.url.clone(),
            bearer_token: self.token.lock().unwrap().clone(),
            client_cert: self.cert.clone(),
        })?))
    }
}

fn run(snapshot: ConfigSnapshot) -> anyhow::Result<()> {
    let policy = snapshot.policy.clone().unwrap_or_default();
    let started = Instant::now();
    let token: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

    let spool = Spool::open(
        std::path::Path::new(&snapshot.local.spool_path),
        &spool_key(),
        snapshot.local.spool_limits,
    )?;
    let uplink = Uplink::new(
        spool,
        Box::new(WssFactory {
            url: ingest_url(&snapshot.local),
            token: token.clone(),
            cert: device_certificate(),
        }),
    );

    let api: Arc<dyn SentinelApi> = Arc::new(HttpApi::new(&snapshot.local.api_base_url, None)?);
    let auth = Arc::new(AuthService::new(
        oidc_config(&snapshot.local),
        token_store(),
        Arc::new(HttpTokenEndpoint::new()),
        browser_launcher(),
    ));

    let widget: Box<dyn WidgetShell> = Box::new(HeadlessWidget::default());
    let tier = match snapshot.capture_tier.as_deref() {
        Some("A") => Some(CaptureTier::A),
        Some("B") => Some(CaptureTier::B),
        // Tier C. The installer blocks these machines, so reaching here means an
        // in-place OS downgrade or a hand-copied binary. Capture stays blocked.
        _ => None,
    };
    let mut agent = Agent::new(
        uplink,
        auth.clone(),
        api,
        widget,
        policy,
        AgentIdentityInfo {
            device_id: snapshot.local.device_id.clone().unwrap_or_default(),
            os_build: snapshot.os_build.clone(),
            tier,
            agent_restarts: snapshot.agent_restarts,
        },
    );

    // Restore a session from Credential Manager, if the last user of this profile left
    // one. Capture still waits for the server to verify the token: a cached sign-in is
    // not a verified identity (spec 7.5).
    match auth.restore(0) {
        Ok(Some(creds)) => {
            *token.lock().unwrap() = creds.id_token.clone();
            agent.on_signed_in(&creds.uid);
        }
        Ok(None) => tracing::info!("no stored session; widget starts locked"),
        Err(e) => tracing::warn!(error = %e, "restoring the stored session failed"),
    }

    let mut source = open_capture_source(&mut agent, tier, auth.state().uid());

    loop {
        let now_ms = started.elapsed().as_millis() as u64;
        // Keep the uplink's bearer token current.
        if let Some(t) = auth.state().id_token() {
            let mut held = token.lock().unwrap();
            if *held != t {
                *held = t.to_string();
            }
        }
        if let Err(e) = agent.tick(now_ms, source.as_deref_mut()) {
            tracing::error!(error = %e, "agent tick failed");
        }
        std::thread::sleep(TICK);
    }
}

/// Build the capture source for this machine's tier, and open the pipeline on it.
///
/// Off Windows there is no capture source at all: the shipping client is Windows-only,
/// and the agent still runs so the loop, the uplink and the widget can be exercised.
fn open_capture_source(
    agent: &mut Agent,
    tier: Option<CaptureTier>,
    user_uid: Option<&str>,
) -> Option<Box<dyn CaptureSource>> {
    let (Some(tier), Some(uid)) = (tier, user_uid) else {
        return None;
    };

    #[cfg(windows)]
    {
        use sentinel_agent::windows::capture_source::WindowsCaptureSource;
        // OPEN-8: the softphone process names are tenant config. With none configured
        // there is no PID to target, and tier A downgrades to tier B for the session.
        let pid = None;
        let capture_tier = match tier {
            CaptureTier::A => sentinel_capture::tier::CaptureTier::A,
            CaptureTier::B => sentinel_capture::tier::CaptureTier::B,
        };
        match WindowsCaptureSource::new(capture_tier, pid) {
            Ok(src) => {
                let mut src: Box<dyn CaptureSource> = Box::new(src);
                if let Err(e) = agent.open_capture(src.as_mut(), tier, uid) {
                    tracing::warn!(error = %e, "capture could not be opened");
                }
                Some(src)
            }
            Err(e) => {
                tracing::error!(error = %e, "no capture source available");
                None
            }
        }
    }

    #[cfg(not(windows))]
    {
        let _ = (agent, tier, uid);
        tracing::info!("no capture source off Windows; running uplink and widget only");
        None
    }
}

fn token_store() -> Arc<dyn TokenStore> {
    #[cfg(windows)]
    {
        Arc::new(sentinel_agent::auth::store::CredentialManagerStore::new())
    }
    #[cfg(not(windows))]
    {
        // Deliberately in memory, not a file: a development build that persisted a
        // real refresh token in plaintext would eventually be run against a real
        // tenant.
        Arc::new(sentinel_agent::auth::store::MemoryTokenStore::new())
    }
}

fn browser_launcher() -> Arc<dyn sentinel_agent::auth::browser::BrowserLauncher> {
    #[cfg(windows)]
    {
        Arc::new(sentinel_agent::auth::browser::SystemBrowser)
    }
    #[cfg(not(windows))]
    {
        Arc::new(sentinel_agent::auth::browser::RecordingLauncher::default())
    }
}

/// The device certificate for mTLS.
///
/// TODO: load the certificate and its CNG key handle written at enrollment. Blocked on
/// `POST /v1/devices/enroll`, which is not implemented in this crate: the P-256 key
/// must be generated non-exportably in CNG and the CSR built against it, and
/// `windows-rs` 0.58 exposes no NCrypt surface for that. Returning `None` means the
/// uplink presents no client certificate and the gateway refuses the connection —
/// which is the correct failure. Capture still spools locally and uploads once a
/// certificate exists; it does not silently downgrade to an unauthenticated upload.
fn device_certificate() -> Option<ClientCertificate> {
    None
}

fn ingest_url(local: &LocalConfig) -> String {
    let base = local
        .api_base_url
        .trim_end_matches('/')
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    format!("{base}/v1/ingest")
}

fn oidc_config(local: &LocalConfig) -> OidcConfig {
    // TODO: these belong in `LocalConfig`, written per tenant by the installer.
    // `sentinel-core::config::LocalConfig` is owned by another crate this session may
    // not modify, so the endpoints are derived from the API base URL and the tenant
    // hint is passed through as the Identity Platform tenant.
    let base = local.api_base_url.trim_end_matches('/');
    OidcConfig {
        authorize_endpoint: "https://accounts.google.com/o/oauth2/v2/auth".into(),
        token_endpoint: format!("{base}/v1/oauth/token"),
        client_id: "sentinel-desktop".into(),
        tenant_id: local.tenant_hint.clone(),
        scopes: vec!["openid".into(), "email".into(), "profile".into()],
    }
}

/// The SQLCipher key.
///
/// TODO: read the wrapped key written at enrollment and unwrap it with
/// `CryptUnprotectData` at **machine** scope — unlike the refresh token, which is user
/// scope, because the service and the agent run as different principals. Blocked on
/// the same enrollment work as the device certificate: there is no key to unwrap until
/// enrollment generates one. Until then the spool is plain SQLite, which the
/// `sqlcipher` feature being off already implies; a production build MUST enable it.
fn spool_key() -> String {
    std::env::var("SENTINEL_SPOOL_KEY").unwrap_or_else(|_| "unconfigured".into())
}
