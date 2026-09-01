//! `POST /v1/heartbeat` (spec 6.8, `contracts/openapi.yaml` → `Heartbeat`).
//!
//! Every 30 s, carrying capture state, tier, version, spool depth, last call time and
//! whatever client events accumulated since the last one. This is the tamper signal:
//! the server alerts on "device online, user signed in, dialer session active, no
//! capture", and it can only do that if the client keeps saying what it is doing.
//!
//! **No PII, at any level.** No transcript text, no account reference, no borrower
//! name, and no user identity — the server already knows who is signed in from the
//! bearer token, so putting a UID in the body would be gratuitous as well as
//! forbidden. `no_pii_fields` below is the enforcement.

use sentinel_core::events::ClientEvent;
use serde::{Deserialize, Serialize};

/// Interval between heartbeats (spec 6.8).
pub const INTERVAL_MS: u64 = 30_000;

/// The request body. Field names and shapes come from `Heartbeat` in
/// `contracts/openapi.yaml`; `contracts/` is authoritative and a change starts there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Heartbeat {
    pub device_id: String,
    /// `CallState::as_str()` — one of IDLE, ARMED, IN_CALL, WRAP, FINALIZE, BLOCKED.
    pub capture_state: String,
    /// `"A"` or `"B"`. Absent on a machine where tier detection found neither, which
    /// is a tier C machine the installer should have blocked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_tier: Option<String>,
    pub os_build: String,
    pub agent_version: String,
    /// Unacked segments on disk.
    pub spool_depth: u64,
    pub spool_bytes: u64,
    /// RFC3339, or null if no call has completed on this device yet.
    pub last_call_at: Option<String>,
    pub dialer_session_active: bool,
    pub signed_in: bool,
    /// Relaunches the service has performed since its counter last reset.
    pub agent_restarts: u32,
    pub pinned_device_present: bool,
    /// Client-side events since the last heartbeat.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub events: Vec<HeartbeatEvent>,
    pub sent_at: String,
}

/// A client event with its clock reading converted to an absolute timestamp.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeartbeatEvent {
    pub kind: String,
    pub at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Everything the heartbeat reports, gathered from around the agent.
#[derive(Debug, Clone, Default)]
pub struct HeartbeatInputs {
    pub device_id: String,
    pub capture_state: String,
    pub capture_tier: Option<String>,
    pub os_build: String,
    pub agent_version: String,
    pub spool_depth: u64,
    pub spool_bytes: u64,
    pub last_call_at: Option<String>,
    pub dialer_session_active: bool,
    pub signed_in: bool,
    pub agent_restarts: u32,
    pub pinned_device_present: bool,
}

/// Build the body.
///
/// Events carry a monotonic clock reading, not a wall-clock time; converting them
/// here against `now_ms` / `sent_at` is the only place both are in hand. Doing it
/// where the event is created would need a wall clock in the capture threads, and a
/// clock adjustment mid-shift would then reorder them.
pub fn build(
    inputs: &HeartbeatInputs,
    events: &[ClientEvent],
    now_ms: u64,
    sent_at: time::OffsetDateTime,
) -> Heartbeat {
    Heartbeat {
        device_id: inputs.device_id.clone(),
        capture_state: inputs.capture_state.clone(),
        capture_tier: inputs.capture_tier.clone(),
        os_build: inputs.os_build.clone(),
        agent_version: inputs.agent_version.clone(),
        spool_depth: inputs.spool_depth,
        spool_bytes: inputs.spool_bytes,
        last_call_at: inputs.last_call_at.clone(),
        dialer_session_active: inputs.dialer_session_active,
        signed_in: inputs.signed_in,
        agent_restarts: inputs.agent_restarts,
        pinned_device_present: inputs.pinned_device_present,
        events: events
            .iter()
            .map(|e| HeartbeatEvent {
                kind: event_kind_str(e.kind),
                at: rfc3339_millis(sent_at - std::time::Duration::from_millis(now_ms.saturating_sub(e.at_ms))),
                count: e.count,
                detail: e.detail.clone(),
            })
            .collect(),
        sent_at: rfc3339_millis(sent_at),
    }
}

/// Snake-case names, matching the `events[].kind` enum in the OpenAPI document.
fn event_kind_str(kind: sentinel_core::events::EventKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "capture_error".into())
}

/// RFC3339 UTC with millisecond precision, matching `started_at` in wire.md.
pub fn rfc3339_millis(t: time::OffsetDateTime) -> String {
    let t = t.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        t.year(),
        t.month() as u8,
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.millisecond()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_core::events::EventKind;
    use time::macros::datetime;

    fn inputs() -> HeartbeatInputs {
        HeartbeatInputs {
            device_id: "1b4e28ba-2fa1-11d2-883f-0016d3cca427".into(),
            capture_state: "IN_CALL".into(),
            capture_tier: Some("B".into()),
            os_build: "10.0.19045".into(),
            agent_version: "0.1.0".into(),
            spool_depth: 12,
            spool_bytes: 42_000,
            last_call_at: Some("2026-09-01T10:19:44.802Z".into()),
            dialer_session_active: true,
            signed_in: true,
            agent_restarts: 2,
            pinned_device_present: true,
        }
    }

    fn now() -> time::OffsetDateTime {
        datetime!(2026-09-01 10:20:00.500 UTC)
    }

    #[test]
    fn the_body_carries_every_field_the_contract_requires() {
        // openapi.yaml: required [device_id, capture_state, capture_tier, os_build,
        // agent_version, spool_depth, sent_at].
        let hb = build(&inputs(), &[], 60_000, now());
        let v = serde_json::to_value(&hb).unwrap();
        for field in [
            "device_id",
            "capture_state",
            "capture_tier",
            "os_build",
            "agent_version",
            "spool_depth",
            "sent_at",
        ] {
            assert!(v.get(field).is_some(), "required field {field} is missing");
        }
        assert_eq!(v["capture_state"], "IN_CALL");
        assert_eq!(v["capture_tier"], "B");
        assert_eq!(v["spool_depth"], 12);
        assert_eq!(v["agent_restarts"], 2);
        assert_eq!(v["sent_at"], "2026-09-01T10:20:00.500Z");
    }

    #[test]
    fn capture_state_is_one_of_the_contracts_enum_values() {
        // openapi.yaml CaptureState.
        const ALLOWED: [&str; 6] = ["IDLE", "ARMED", "IN_CALL", "WRAP", "FINALIZE", "BLOCKED"];
        use sentinel_core::state::CallState;
        for state in [
            CallState::Idle,
            CallState::Armed,
            CallState::InCall,
            CallState::Wrap,
            CallState::Finalize,
            CallState::Blocked,
        ] {
            assert!(
                ALLOWED.contains(&state.as_str()),
                "{} is not in the CaptureState enum",
                state.as_str()
            );
        }
    }

    #[test]
    fn event_kinds_match_the_contracts_enum() {
        // openapi.yaml Heartbeat.events[].kind.
        const ALLOWED: [&str; 7] = [
            "tier_downgrade",
            "spool_eviction",
            "device_lost",
            "device_restored",
            "agent_restart",
            "capture_error",
            "foreign_audio_suppressed",
        ];
        for kind in [
            EventKind::TierDowngrade,
            EventKind::SpoolEviction,
            EventKind::DeviceLost,
            EventKind::DeviceRestored,
            EventKind::AgentRestart,
            EventKind::CaptureError,
            EventKind::ForeignAudioSuppressed,
        ] {
            let s = event_kind_str(kind);
            assert!(ALLOWED.contains(&s.as_str()), "{s} is not in the contract's enum");
        }
    }

    #[test]
    fn event_clock_readings_become_absolute_timestamps_in_the_past() {
        // The event happened 5 s before this heartbeat was assembled.
        let events = vec![ClientEvent::new(EventKind::SpoolEviction, 55_000).with_count(7)];
        let hb = build(&inputs(), &events, 60_000, now());
        assert_eq!(hb.events.len(), 1);
        assert_eq!(hb.events[0].kind, "spool_eviction");
        assert_eq!(hb.events[0].count, Some(7));
        assert_eq!(hb.events[0].at, "2026-09-01T10:19:55.500Z");
        assert!(hb.events[0].at < hb.sent_at);
    }

    #[test]
    fn an_empty_event_list_is_omitted_rather_than_sent_as_null() {
        let v = serde_json::to_value(build(&inputs(), &[], 0, now())).unwrap();
        assert!(v.get("events").is_none());
    }

    #[test]
    fn a_device_with_no_completed_calls_reports_null_not_a_fabricated_time() {
        let mut i = inputs();
        i.last_call_at = None;
        let v = serde_json::to_value(build(&i, &[], 0, now())).unwrap();
        assert!(v["last_call_at"].is_null(), "nullable in the contract");
    }

    #[test]
    fn no_pii_fields() {
        // Spec 12.10. The server derives the user from the bearer token; a UID here
        // would be gratuitous as well as forbidden, and an account_ref would be a
        // borrower's loan reference in a log-adjacent payload.
        let mut i = inputs();
        i.capture_state = "IN_CALL".into();
        let events = vec![
            ClientEvent::new(EventKind::DeviceLost, 0).with_detail("container_id_absent".into()),
        ];
        let json = serde_json::to_string(&build(&i, &events, 0, now())).unwrap();
        for forbidden in [
            "account_ref",
            "user_uid",
            "borrower",
            "transcript",
            "display_name",
            "email",
            "phone",
        ] {
            assert!(!json.contains(forbidden), "heartbeat leaked {forbidden}: {json}");
        }
    }

    #[test]
    fn timestamps_have_millisecond_precision_and_a_z_suffix() {
        // wire.md: RFC3339 UTC, millisecond precision. A server parsing strictly will
        // reject "+00:00" or a bare second.
        assert_eq!(rfc3339_millis(datetime!(2026-01-02 03:04:05.006 UTC)), "2026-01-02T03:04:05.006Z");
        assert_eq!(rfc3339_millis(datetime!(2026-12-31 23:59:59.999 UTC)), "2026-12-31T23:59:59.999Z");
        // A non-UTC offset is normalised, not emitted with its offset.
        assert_eq!(
            rfc3339_millis(datetime!(2026-01-02 08:34:05.006 +05:30)),
            "2026-01-02T03:04:05.006Z"
        );
    }
}
