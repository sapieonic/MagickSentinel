//! `SentinelAgent.exe` — the user-session process.
//!
//! Launched by the service at logon (never from a `Run` key, which the user can
//! delete). One instance per interactive session, guarded by a session-local named
//! mutex.
//!
//! This file is deliberately thin: it resolves configuration, builds the concrete
//! implementations, and hands them to [`sentinel_agent::agent::Agent`], which owns the
//! ordering. Everything worth testing lives in the library.
//!
//! Three things it resolves are worth naming, because each one used to be a `TODO`
//! here and each one is a security property rather than a wiring detail:
//!
//! * **The device certificate** ([`device_certificate`]) is loaded from the credential
//!   the service enrolled, and its private key is a non-exportable CNG key that this
//!   process signs with and cannot read.
//! * **The spool key** ([`spool_key`]) is unwrapped from a DPAPI machine-scope blob.
//!   There is no fallback: no key means no capture, not plaintext audio.
//! * **The OIDC endpoints** come from `LocalConfig`, written per tenant by the
//!   installer, rather than from constants compiled into a binary shipped to every
//!   customer.

use sentinel_agent::agent::{Agent, AgentIdentityInfo};
use sentinel_agent::api::{HttpApi, HttpTokenEndpoint, SentinelApi};
use sentinel_agent::auth::pkce::OidcConfig;
use sentinel_agent::auth::{AuthService, TokenStore};
use sentinel_agent::device::DeviceCredential;
use sentinel_agent::ipc::ServiceClient;
use sentinel_agent::uplink::transport::{
    ClientCertificate, ConnectParams, Transport, TransportError, WsTransport,
};
use sentinel_agent::uplink::{TransportFactory, Uplink};
use sentinel_agent::widget::{HeadlessWidget, WidgetShell};
use sentinel_capture::source::CaptureSource;
use sentinel_core::config::LocalConfig;
use sentinel_core::events::{ClientEvent, EventKind};
use sentinel_core::protocol::CaptureTier;
use sentinel_core::spool::Spool;
use sentinel_service::ipc::{ConfigSnapshot, Request, Response};
use sentinel_service::telemetry;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How often the loop turns. 20 ms is one Opus frame: the pipeline reads a frame per
/// tick, so a slower loop would starve capture and a faster one would spin on an
/// endpoint buffer that has nothing in it yet.
const TICK: Duration = Duration::from_millis(20);

fn main() -> anyhow::Result<()> {
    // Configuration is read before logging is installed, because whether telemetry is
    // exported at all — and where to — is part of that configuration, and a
    // `tracing` subscriber can only be installed once. Anything worth saying about the
    // config load is carried out in `note` and logged as soon as there is somewhere to
    // log it.
    let (snapshot, note) = load_config();
    let _telemetry = init_logging(&snapshot);
    if let Some(note) = note {
        tracing::warn!("{note}");
    }

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

    tracing::info!(
        tier = ?snapshot.capture_tier,
        os_build = %snapshot.os_build,
        version = sentinel_agent::VERSION,
        "sentinel agent starting"
    );
    // Tier detection is the service's answer, re-detected on every service start
    // because an in-place OS upgrade changes it. Reported here because a floor that
    // silently came out tier B — or tier C, which cannot capture at all — is a
    // coverage problem an operator has to see before the calls are missed.
    tracing::info!(
        target: telemetry::TARGET,
        event = telemetry::event::TIER_DETECTED,
        capture_tier = snapshot.capture_tier.as_deref().unwrap_or("none"),
        os_build = %snapshot.os_build,
        supported = snapshot.capture_tier.is_some(),
        "capture tier detected"
    );

    run(snapshot)
}

/// Install logging, and OTLP export if this tenant turned it on.
///
/// The returned handle owns the exporter thread; dropping it stops the thread, so
/// `main` holds it for the life of the process.
fn init_logging(snapshot: &ConfigSnapshot) -> Option<telemetry::TelemetryHandle> {
    let Some(url) = snapshot.local.otlp_logs_url() else {
        // The default. Nothing is sent and no thread is started.
        sentinel_agent::init_logging();
        return None;
    };
    let via_gateway = snapshot.local.otlp_goes_via_gateway(&url);
    let resource = telemetry::Resource {
        service_name: "sentinel-agent".into(),
        service_version: sentinel_agent::VERSION.into(),
        tenant_id: snapshot.local.tenant_hint.clone(),
        device_id: snapshot.local.device_id.clone(),
    };
    let (layer, handle) = telemetry::OtlpLayer::new(
        resource,
        Box::new(sentinel_service::http::HttpOtlpShipper::new(url.clone())),
    );
    sentinel_agent::init_logging_with(Some(layer));
    if via_gateway {
        tracing::info!(endpoint = %url, "telemetry export enabled, relayed by the gateway");
    } else {
        // Loud, because it is a second egress from a desktop on a bank's network and
        // that is a decision somebody has to have made on purpose.
        tracing::warn!(
            endpoint = %url,
            "telemetry is being sent DIRECTLY to a collector rather than through the \
             gateway. This opens a second egress from this endpoint and is intended \
             for development only."
        );
    }
    Some(handle)
}

/// Ask the service for machine config, falling back to defaults.
///
/// Falling back rather than failing: the agent must reach the point of showing a
/// widget even when the service is not up, or an agent whose service failed to start
/// sees nothing at all and reports the product as broken.
fn load_config() -> (ConfigSnapshot, Option<&'static str>) {
    match service_client().request(Request::GetConfig) {
        Ok(Response::Config(c)) => (*c, None),
        Ok(_) | Err(_) => (
            ConfigSnapshot {
                local: LocalConfig::default(),
                policy: None,
                capture_tier: None,
                os_build: "0.0.0".into(),
                service_version: "unknown".into(),
                agent_restarts: 0,
            },
            Some("service config unavailable; continuing on local defaults"),
        ),
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
    let device_dir = device_dir(&snapshot.local);

    let credential = device_certificate(&device_dir);
    // Every start-up condition that must block capture, collected before anything is
    // built, so the decision is made once and in one place.
    let mut blockers: Vec<ClientEvent> = Vec::new();

    // The spool. A failure to resolve the key is NOT a reason to open a plaintext
    // database: the whole requirement (spec 6.5, 12.3) is that call audio at rest is
    // encrypted, and a spool that quietly holds unencrypted borrower audio is worse
    // than a spool that holds nothing. So the key failure blocks capture, and the
    // in-memory database below exists only because `Uplink` needs one — with capture
    // blocked nothing can write to it.
    let spool = match spool_key(&device_dir) {
        Ok(key) => Spool::open(
            Path::new(&snapshot.local.spool_path),
            &key,
            snapshot.local.spool_limits,
        )?,
        Err(e) => {
            tracing::error!(
                target: telemetry::TARGET,
                event = telemetry::event::SPOOL_KEY_UNAVAILABLE,
                error = %e,
                "no spool encryption key: CAPTURE IS BLOCKED. The agent will keep \
                 heartbeating so this is visible in the fleet view, but it will not \
                 record, because recording would mean writing unencrypted call audio \
                 to disk."
            );
            blockers.push(
                ClientEvent::new(EventKind::CaptureError, 0)
                    .with_detail("spool_key_unavailable".into()),
            );
            Spool::open_in_memory(snapshot.local.spool_limits)?
        }
    };

    let uplink = Uplink::new(
        spool,
        Box::new(WssFactory {
            url: ingest_url(&snapshot.local),
            token: token.clone(),
            cert: credential.as_ref().map(|c| ClientCertificate {
                certified_key: c.certified_key(),
            }),
        }),
    );

    let api: Arc<dyn SentinelApi> = Arc::new(match &credential {
        Some(c) => HttpApi::with_device(&snapshot.local.api_base_url, c)?,
        None => HttpApi::new(&snapshot.local.api_base_url)?,
    });

    let oidc = OidcConfig::from_local(&snapshot.local);
    if let Some(problem) = snapshot.local.oidc_problem() {
        // Named rather than generic: an agent who is told "sign-in failed" restarts
        // the machine, and a support engineer who is told which installer property is
        // empty fixes it. The message is a static string, so nothing tenant-specific
        // reaches the log.
        tracing::error!(problem, "sign-in cannot start: the tenant's OIDC settings are incomplete");
    }
    let auth = Arc::new(AuthService::new(
        oidc,
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
    // A blocked start-up condition wins over the detected tier. `open_capture_source`
    // takes `None` to mean "no capture", which is exactly the outcome wanted, and it
    // means the block is expressed once rather than checked at each capture call site.
    let effective_tier = if blockers.is_empty() { tier } else { None };

    let mut agent = Agent::new(
        uplink,
        auth.clone(),
        api,
        widget,
        policy,
        AgentIdentityInfo {
            // The certificate is authoritative for the device id: the gateway derives
            // it from the certificate, and a `LocalConfig` value that disagreed would
            // put a heartbeat's `device_id` at odds with the connection it arrived on.
            device_id: credential
                .as_ref()
                .map(|c| c.device_id.clone())
                .or_else(|| snapshot.local.device_id.clone())
                .unwrap_or_default(),
            os_build: snapshot.os_build.clone(),
            tier,
            agent_restarts: snapshot.agent_restarts,
        },
    );
    for event in blockers {
        agent.record_event(event);
    }
    if credential.is_none() {
        agent.record_event(
            ClientEvent::new(EventKind::CaptureError, 0)
                .with_detail("device_certificate_missing".into()),
        );
    }

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

    let mut source = open_capture_source(&mut agent, effective_tier, auth.state().uid());

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
        // Say which of the two is missing. "Capture did not start" with no reason is
        // the report this product must never produce.
        tracing::info!(
            target: telemetry::TARGET,
            event = telemetry::event::CAPTURE_OPEN_FAILED,
            reason = if tier.is_none() { "no_supported_tier" } else { "no_signed_in_user" },
            "capture was not opened"
        );
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
                    tracing::warn!(
                        target: telemetry::TARGET,
                        event = telemetry::event::CAPTURE_OPEN_FAILED,
                        reason = "pipeline_open_failed",
                        error = %e,
                        "capture could not be opened"
                    );
                }
                Some(src)
            }
            Err(e) => {
                tracing::error!(
                    target: telemetry::TARGET,
                    event = telemetry::event::CAPTURE_OPEN_FAILED,
                    reason = "no_capture_source",
                    error = %e,
                    "no capture source available"
                );
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

/// Where the device credential and the wrapped spool key live.
///
/// `%PROGRAMDATA%\MagickVoice\Sentinel\device` unless tenant config overrides it. That
/// directory is ACLed read-only for `Users` by the MSI precisely because it holds
/// machine identity: the SYSTEM service writes it at enrollment and this process, which
/// runs as the signed-in user, only ever reads it.
fn device_dir(local: &LocalConfig) -> PathBuf {
    match local.device_dir.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => sentinel_service::device::credential_dir(),
    }
}

/// The device certificate for mTLS.
///
/// Loads the credential the service enrolled: the certificate chain from disk and a
/// signer over the non-exportable CNG key it was issued against
/// (`sentinel_agent::device`). Renewal is not this process's job — the service owns the
/// certificate's lifecycle, re-certifying the same key when it enters the 30-day window
/// (`sentinel_service::enroll::renewal_decision`) — and the agent picks up the new
/// certificate the next time it starts. Revocation is: a `4403` on the ingest socket or
/// a 403 on the heartbeat stops capture within the tick and stops the uplink
/// reconnecting, which is what wire.md requires.
///
/// `None` is preserved as the correct failure, unchanged from the stub this replaced:
/// no certificate means the gateway refuses the connection, and capture **spools
/// locally** rather than silently downgrading to an unauthenticated upload. There is no
/// configuration that turns mTLS off.
fn device_certificate(device_dir: &Path) -> Option<DeviceCredential> {
    match sentinel_agent::device::load(device_dir) {
        Ok(credential) => {
            if credential.meets_non_exportable_requirement() {
                tracing::info!(
                    target: telemetry::TARGET,
                    event = "device.credential_loaded",
                    device_id = %credential.device_id,
                    not_after = %credential.not_after,
                    key_kind = credential.key_kind.as_str(),
                    "device certificate loaded"
                );
            } else {
                // Never quietly. A software key is a development affordance that does
                // not meet spec 7.2, and a machine running on one has to be findable.
                tracing::error!(
                    target: telemetry::TARGET,
                    event = "device.credential_loaded",
                    device_id = %credential.device_id,
                    key_kind = credential.key_kind.as_str(),
                    meets_non_exportable_requirement = false,
                    "device certificate loaded with a SOFTWARE key: the private key is \
                     a file on disk and does not meet the non-exportable-key \
                     requirement (spec 7.2). Development builds only."
                );
            }
            Some(credential)
        }
        Err(e) => {
            tracing::error!(
                target: telemetry::TARGET,
                event = telemetry::event::DEVICE_CREDENTIAL_MISSING,
                error = %e,
                "no device certificate: the gateway will refuse the uplink and capture \
                 will spool locally until one exists. This is the correct failure, not \
                 a reason to connect without one."
            );
            None
        }
    }
}

fn ingest_url(local: &LocalConfig) -> String {
    let base = local
        .api_base_url
        .trim_end_matches('/')
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    format!("{base}/v1/ingest")
}

/// The SQLCipher key.
///
/// Read from the DPAPI-wrapped blob the service wrote at enrollment and unwrapped at
/// **machine** scope — unlike the refresh token, which is correctly user scope, because
/// the service and the agent run as different principals and a user-scoped wrap would
/// leave one of them unable to open the file (`docs/architecture.md`,
/// `sentinel_service::spoolkey`).
///
/// This returns a `Result` and has no default. The shape it replaced —
/// `env::var(..).unwrap_or_else(|_| "unconfigured".into())` — was worse than no
/// encryption at all, because it produced a spool that looked encrypted while every
/// machine on the floor used the same key. The caller blocks capture on an error; see
/// [`run`].
fn spool_key(device_dir: &Path) -> Result<String, sentinel_service::spoolkey::SpoolKeyError> {
    sentinel_service::spoolkey::resolve(device_dir, sentinel_service::spoolkey::wrapper().as_ref())
}
