//! Configuration delivered by `GET /v1/policy`, plus the local settings the agent
//! needs before it can reach the server.
//!
//! Everything tunable is here rather than in constants: a tenant changing a VAD
//! threshold or a call-hours window must not require shipping 200 desktops.

use serde::{Deserialize, Serialize};

/// Timings for the call state machine, all in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VadConfig {
    /// Far-channel speech required in `ARMED` before a call is confirmed.
    pub speech_ms_to_confirm: u64,
    /// How long `ARMED` waits for that speech before discarding the arm.
    pub armed_timeout_ms: u64,
    /// Continuous silence on both channels, alongside an Inactive session, that
    /// counts as a hangup.
    pub hangup_silence_ms: u64,
    /// How long `WRAP` waits for a hold to resume before finalizing.
    pub wrap_ms: u64,
    /// Minimum interval between transitions out of a steady state.
    pub debounce_ms: u64,
}

impl Default for VadConfig {
    fn default() -> Self {
        VadConfig {
            speech_ms_to_confirm: 300,
            armed_timeout_ms: 20_000,
            hangup_silence_ms: 8_000,
            wrap_ms: 3_000,
            debounce_ms: 2_000,
        }
    }
}

/// An audio endpoint an admin has pinned for this tenant.
///
/// Matched by container ID, with the friendly name as a fallback. Capture never
/// starts from the system default device: on Tier B that would record whatever else
/// the machine is playing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedDevice {
    pub container_id: String,
    #[serde(default)]
    pub friendly_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SoftphoneConfig {
    /// Process names to resolve a PID from, in preference order (OPEN-8).
    #[serde(default)]
    pub process_names: Vec<String>,
    /// UI Automation selector for the account reference. `None` disables the scrape;
    /// the server then reconciles against dialer CDR on `(agent_id, started_at)`.
    #[serde(default)]
    pub uia_account_ref_selector: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retention {
    pub audio_days: u32,
    pub transcript_days: u32,
}

impl Default for Retention {
    fn default() -> Self {
        Retention { audio_days: 30, transcript_days: 365 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Policy {
    pub version: i64,
    pub pinned_devices: Vec<PinnedDevice>,
    pub softphone: SoftphoneConfig,
    /// How long capture may continue with no reachable server before it must stop.
    /// We never silently record with no verifiable identity attached to the audio.
    pub offline_grace_hours: u32,
    pub idle_signout_minutes: u32,
    pub rules_version: i64,
    pub allow_agent_audio_playback: bool,
    pub retention: Retention,
    pub vad: VadConfig,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            version: 0,
            pinned_devices: Vec::new(),
            softphone: SoftphoneConfig::default(),
            offline_grace_hours: 8,
            idle_signout_minutes: 30,
            rules_version: 0,
            allow_agent_audio_playback: false,
            retention: Retention::default(),
            vad: VadConfig::default(),
        }
    }
}

impl Policy {
    pub fn offline_grace_ms(&self) -> u64 {
        self.offline_grace_hours as u64 * 3_600_000
    }
}

/// Spool limits. Whichever cap is reached first triggers oldest-first eviction, and
/// every eviction emits an event: silent data loss is unacceptable in a compliance
/// product.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SpoolLimits {
    pub max_bytes: u64,
    pub max_age_ms: u64,
}

impl Default for SpoolLimits {
    fn default() -> Self {
        SpoolLimits {
            max_bytes: 2 * 1024 * 1024 * 1024, // 2 GB
            max_age_ms: 72 * 3_600_000,        // 72 h
        }
    }
}

/// The tenant's OIDC / Identity Platform endpoints and client identity.
///
/// These were hard-coded in `SentinelAgent`'s `main` until this landed, which was
/// wrong in a way that only shows up at the second customer: the authorize endpoint,
/// the OAuth client id and the Identity Platform tenant are **per tenant**, and a
/// binary carrying one BPO's values cannot sign in the next BPO's agents. They are
/// written by the installer (`APIBASEURL`, `TENANTHINT` and the OIDC properties) into
/// the machine-scoped config the service owns, so changing an IdP is a config push
/// rather than an MSI rollout to 200 desktops.
///
/// OPEN-2 (Entra ID or Identity Platform) is deliberately **not** resolved here.
/// The RFC 8252 + PKCE flow is identical either way — an authorize endpoint, a token
/// endpoint, a public client id, no client secret — so nothing in this type or in
/// `sentinel_agent::auth` names a provider. `identity_platform_tenant` is optional
/// precisely because a provider that has no equivalent of Identity Platform's
/// `tenantId` parameter simply leaves it unset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OidcSettings {
    /// The IdP's authorization endpoint, opened in the system browser.
    ///
    /// Deliberately empty by default rather than defaulted to a Google URL. A
    /// plausible-looking default is worse than an empty one here: it would point
    /// every unconfigured deployment at one shared client id, sign-in would fail
    /// with an IdP error about an unregistered redirect, and nobody reading that
    /// error would look in the installer's properties. Empty makes
    /// [`LocalConfig::oidc_problem`] able to say exactly which value is missing.
    pub authorize_endpoint: String,

    /// Where the authorization code and refresh tokens are exchanged.
    ///
    /// `None` means the gateway's own token endpoint, `{api_base_url}/v1/oauth/token`
    /// — which is the production answer. The gateway proxies the exchange so the
    /// endpoint holds no client secret and so the exchange is one more thing that
    /// happens over the single authenticated egress these machines have. The wire
    /// contract for it is fixed: `application/x-www-form-urlencoded` request, JSON
    /// response as `sentinel_agent::api::TokenResponse` parses it. This field moves
    /// *where the URL comes from*, never the protocol.
    pub token_endpoint: Option<String>,

    /// Public OAuth client id for the desktop app (RFC 8252 native client — no
    /// secret; PKCE is what replaces it).
    pub client_id: String,

    /// Identity Platform tenant for this BPO, sent as `tenantId`. One per customer,
    /// which is what gives hard isolation at the auth layer. `None` on a provider
    /// with no tenant concept.
    pub identity_platform_tenant: Option<String>,

    /// Requested scopes. Protocol-standard rather than tenant-specific, so unlike
    /// the rest of this struct it carries a real default.
    pub scopes: Vec<String>,
}

impl Default for OidcSettings {
    fn default() -> Self {
        OidcSettings {
            authorize_endpoint: String::new(),
            token_endpoint: None,
            client_id: String::new(),
            identity_platform_tenant: None,
            scopes: vec!["openid".into(), "email".into(), "profile".into()],
        }
    }
}

/// The path the gateway relays OTLP log records on, appended to the API base.
///
/// Telemetry goes **through the gateway**, never straight to a collector. Giving 200
/// collections desktops on a bank's network a route to our observability backend is a
/// security-review conversation with no upside: it is a second egress to justify, a
/// second TLS trust decision, a second set of firewall rules, and a second place
/// endpoint-side data can leave the building. The gateway is already the one
/// authenticated egress these machines have, it already terminates mTLS with the
/// device certificate, and it already knows which tenant a device belongs to — so it
/// is also the only place that can attribute a telemetry record without the client
/// asserting a tenant.
pub const OTLP_RELAY_PATH: &str = "/v1/telemetry/otlp/v1/logs";

/// Endpoint telemetry settings.
///
/// Off by default, and off means *nothing is sent and no thread is started*. A
/// compliance product that ships telemetry from a bank's desktops by default has
/// answered a question the customer did not get to ask.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TelemetrySettings {
    pub enabled: bool,
    /// OTLP/HTTP base URL. `None` with `enabled` means the gateway relay derived from
    /// `api_base_url`, which is the only production answer. Setting this to anything
    /// else opens a second egress and is a development affordance; the exporter logs
    /// a warning when it is used.
    pub otlp_endpoint: Option<String>,
}

/// Local, machine-scoped settings, written by the installer and updated by the
/// service. Contains no secrets: the spool key is held by DPAPI at machine scope and
/// the refresh token by Credential Manager at user scope.
///
/// Every field added after the first release carries `#[serde(default)]` rather than
/// the container carrying it. A container-level default would make `api_base_url`
/// optional too, and a config file that lost its API base would then parse
/// successfully into a client pointed at the wrong host instead of failing to parse
/// and falling back to a known default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocalConfig {
    pub api_base_url: String,
    pub tenant_hint: Option<String>,
    pub device_id: Option<String>,
    pub spool_path: String,
    #[serde(default)]
    pub spool_limits: SpoolLimits,
    #[serde(default)]
    pub log_dir: Option<String>,
    #[serde(default)]
    pub oidc: OidcSettings,
    /// Directory holding the device certificate and the wrapped spool key. `None`
    /// means the installer's location — `%PROGRAMDATA%\MagickVoice\Sentinel\device`,
    /// which the MSI ACLs read-only for `Users` because it holds machine identity.
    /// Overriding it is a development affordance; see
    /// `sentinel_service::device::credential_dir`.
    #[serde(default)]
    pub device_dir: Option<String>,
    #[serde(default)]
    pub telemetry: TelemetrySettings,
}

impl Default for LocalConfig {
    fn default() -> Self {
        LocalConfig {
            api_base_url: "https://api.sentinel.magickvoice.com".into(),
            tenant_hint: None,
            device_id: None,
            spool_path: default_spool_path(),
            spool_limits: SpoolLimits::default(),
            log_dir: None,
            oidc: OidcSettings::default(),
            device_dir: None,
            telemetry: TelemetrySettings::default(),
        }
    }
}

impl LocalConfig {
    /// `api_base_url` with any trailing slash removed, so callers can concatenate a
    /// path without producing a double slash the gateway's router will not match.
    pub fn api_base(&self) -> &str {
        self.api_base_url.trim_end_matches('/')
    }

    /// The token endpoint to use: the configured override, else the gateway's.
    pub fn token_endpoint(&self) -> String {
        match &self.oidc.token_endpoint {
            Some(url) if !url.trim().is_empty() => url.trim().to_string(),
            // Fixed wire contract: POST {api_base}/v1/oauth/token,
            // application/x-www-form-urlencoded in, JSON out.
            _ => format!("{}/v1/oauth/token", self.api_base()),
        }
    }

    /// Where OTLP log records go, or `None` when telemetry is off.
    ///
    /// Resolution order — environment first so an operator debugging one desktop does
    /// not have to push a config change, then tenant config, then the gateway relay:
    ///
    /// 1. `OTEL_EXPORTER_OTLP_ENDPOINT` (the standard variable). Per the OTLP/HTTP
    ///    specification the value is a base URL and the signal path is appended, so
    ///    `http://127.0.0.1:4318` becomes `http://127.0.0.1:4318/v1/logs`.
    /// 2. `telemetry.otlp_endpoint` from tenant config, same appending rule.
    /// 3. `telemetry.enabled` alone → the gateway relay, [`OTLP_RELAY_PATH`].
    /// 4. Otherwise `None`: telemetry is off, which is the default.
    pub fn otlp_logs_url(&self) -> Option<String> {
        self.otlp_logs_url_with_env(std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok().as_deref())
    }

    /// [`LocalConfig::otlp_logs_url`] with the environment passed in, so the
    /// resolution order is testable without mutating the process environment — which
    /// two tests running in parallel in one process cannot do safely.
    pub fn otlp_logs_url_with_env(&self, env_endpoint: Option<&str>) -> Option<String> {
        if let Some(base) = env_endpoint {
            let base = base.trim();
            if !base.is_empty() {
                return Some(format!("{}/v1/logs", base.trim_end_matches('/')));
            }
        }
        if let Some(base) = self.telemetry.otlp_endpoint.as_deref() {
            let base = base.trim();
            if !base.is_empty() {
                return Some(format!("{}/v1/logs", base.trim_end_matches('/')));
            }
        }
        if self.telemetry.enabled {
            return Some(format!("{}{}", self.api_base(), OTLP_RELAY_PATH));
        }
        None
    }

    /// Whether the resolved OTLP endpoint is the gateway relay rather than a
    /// collector reached directly. The exporter warns on the latter.
    pub fn otlp_goes_via_gateway(&self, url: &str) -> bool {
        url == format!("{}{}", self.api_base(), OTLP_RELAY_PATH)
    }

    /// Which OIDC value the installer failed to write, if any.
    ///
    /// Returned as a static string so it can be logged and shown in the widget
    /// without interpolating anything tenant-specific into a log line. A sign-in that
    /// cannot start must say *why* — "the installer did not write an OAuth client id"
    /// is actionable, "sign-in failed" is not.
    pub fn oidc_problem(&self) -> Option<&'static str> {
        if self.oidc.authorize_endpoint.trim().is_empty() {
            return Some("no OIDC authorize endpoint is configured for this tenant");
        }
        if self.oidc.client_id.trim().is_empty() {
            return Some("no OAuth client id is configured for this tenant");
        }
        if !self.oidc.authorize_endpoint.starts_with("https://") {
            // The authorize URL carries the PKCE challenge and the state value into
            // the browser. Over plaintext both are observable and the flow is not
            // PKCE any more, so this is refused rather than warned about.
            return Some("the OIDC authorize endpoint must be https");
        }
        None
    }
}

pub fn default_spool_path() -> String {
    if cfg!(windows) {
        let base = std::env::var("PROGRAMDATA").unwrap_or_else(|_| "C:\\ProgramData".into());
        format!("{base}\\MagickVoice\\Sentinel\\spool.db")
    } else {
        // Development only; the shipping client is Windows-only.
        "/var/lib/magickvoice-sentinel/spool.db".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_defaults_match_the_spec() {
        let p = Policy::default();
        assert_eq!(p.offline_grace_hours, 8);
        assert_eq!(p.idle_signout_minutes, 30);
        assert!(!p.allow_agent_audio_playback, "playback is opt-in per tenant");
        assert_eq!(p.vad.speech_ms_to_confirm, 300);
        assert_eq!(p.vad.hangup_silence_ms, 8_000);
        assert_eq!(p.retention.audio_days, 30);
    }

    #[test]
    fn partial_policy_json_fills_in_defaults() {
        // The server may add fields before clients are updated, and older clients
        // must not fail to parse a newer policy.
        let p: Policy = serde_json::from_str(
            r#"{"version":7,"offline_grace_hours":2,"vad":{"hangup_silence_ms":5000}}"#,
        )
        .unwrap();
        assert_eq!(p.version, 7);
        assert_eq!(p.offline_grace_hours, 2);
        assert_eq!(p.vad.hangup_silence_ms, 5_000);
        assert_eq!(p.vad.speech_ms_to_confirm, 300, "unspecified fields keep defaults");
    }

    #[test]
    fn a_config_file_written_before_the_oidc_block_existed_still_parses() {
        // Field-level defaults, not a container default: an installed machine's
        // config.json predates these fields and must keep working, while a file that
        // lost `api_base_url` must still fail to parse rather than silently becoming
        // a client pointed somewhere else.
        let c: LocalConfig = serde_json::from_str(
            r#"{"api_base_url":"https://api.example.com/","tenant_hint":"t-1",
                "device_id":null,"spool_path":"C:\\spool.db"}"#,
        )
        .unwrap();
        assert_eq!(c.api_base(), "https://api.example.com");
        assert_eq!(c.oidc, OidcSettings::default());
        assert!(!c.telemetry.enabled);
        assert!(serde_json::from_str::<LocalConfig>(r#"{"tenant_hint":"t-1"}"#).is_err());
    }

    #[test]
    fn the_token_endpoint_defaults_to_the_gateways_and_keeps_its_contract() {
        // POST {api_base}/v1/oauth/token is fixed; only where the URL comes from
        // moved into config.
        let mut c = LocalConfig {
            api_base_url: "https://api.example.com/".into(),
            ..LocalConfig::default()
        };
        assert_eq!(c.token_endpoint(), "https://api.example.com/v1/oauth/token");
        c.oidc.token_endpoint = Some("  ".into());
        assert_eq!(
            c.token_endpoint(),
            "https://api.example.com/v1/oauth/token",
            "a blank override is not an override"
        );
        c.oidc.token_endpoint = Some("https://idp.example.com/oauth2/token".into());
        assert_eq!(c.token_endpoint(), "https://idp.example.com/oauth2/token");
    }

    #[test]
    fn unconfigured_oidc_names_the_missing_value_rather_than_guessing_one() {
        // The alternative — defaulting to a Google URL and one shared client id —
        // fails at the IdP with an error nobody traces back to installer properties.
        let mut c = LocalConfig::default();
        assert_eq!(
            c.oidc_problem(),
            Some("no OIDC authorize endpoint is configured for this tenant")
        );
        c.oidc.authorize_endpoint = "https://idp.example.com/authorize".into();
        assert_eq!(
            c.oidc_problem(),
            Some("no OAuth client id is configured for this tenant")
        );
        c.oidc.client_id = "sentinel-desktop".into();
        assert_eq!(c.oidc_problem(), None);
        // The authorize URL carries the PKCE challenge into the browser.
        c.oidc.authorize_endpoint = "http://idp.example.com/authorize".into();
        assert_eq!(c.oidc_problem(), Some("the OIDC authorize endpoint must be https"));
    }

    #[test]
    fn telemetry_is_off_until_a_tenant_turns_it_on_and_then_goes_via_the_gateway() {
        let mut c = LocalConfig {
            api_base_url: "https://api.example.com".into(),
            ..LocalConfig::default()
        };
        assert_eq!(c.otlp_logs_url_with_env(None), None, "off by default");

        c.telemetry.enabled = true;
        let url = c.otlp_logs_url_with_env(None).unwrap();
        assert_eq!(url, "https://api.example.com/v1/telemetry/otlp/v1/logs");
        assert!(c.otlp_goes_via_gateway(&url), "the default target is the gateway relay");

        // An explicit collector is a development affordance and is not the gateway.
        c.telemetry.otlp_endpoint = Some("http://127.0.0.1:4318/".into());
        let url = c.otlp_logs_url_with_env(None).unwrap();
        assert_eq!(url, "http://127.0.0.1:4318/v1/logs", "OTLP/HTTP appends the signal path");
        assert!(!c.otlp_goes_via_gateway(&url));

        // The standard environment variable wins over tenant config, so one desktop
        // can be pointed at a collector without a config push.
        assert_eq!(
            c.otlp_logs_url_with_env(Some("http://collector:4318")).unwrap(),
            "http://collector:4318/v1/logs"
        );
        assert_eq!(
            c.otlp_logs_url_with_env(Some("   ")).unwrap(),
            "http://127.0.0.1:4318/v1/logs",
            "a blank variable is not a value"
        );
        // An empty variable on a machine with telemetry off is still off.
        let off = LocalConfig { api_base_url: "https://api.example.com".into(), ..LocalConfig::default() };
        assert_eq!(off.otlp_logs_url_with_env(None), None);
    }

    #[test]
    fn spool_limits_are_two_gigabytes_and_seventytwo_hours() {
        let l = SpoolLimits::default();
        assert_eq!(l.max_bytes, 2 * 1024 * 1024 * 1024);
        assert_eq!(l.max_age_ms, 72 * 3_600_000);
    }
}
