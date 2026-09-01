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

/// Local, machine-scoped settings, written by the installer and updated by the
/// service. Contains no secrets: the spool key is held by DPAPI at machine scope and
/// the refresh token by Credential Manager at user scope.
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
        }
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
    fn spool_limits_are_two_gigabytes_and_seventytwo_hours() {
        let l = SpoolLimits::default();
        assert_eq!(l.max_bytes, 2 * 1024 * 1024 * 1024);
        assert_eq!(l.max_age_ms, 72 * 3_600_000);
    }
}
