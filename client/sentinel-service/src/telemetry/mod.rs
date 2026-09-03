//! Endpoint telemetry: OTLP log records, off by default, relayed through the gateway.
//!
//! The repository had no telemetry of any kind before this. What it has now is
//! deliberately narrow: a `tracing` layer that picks up events on one dedicated
//! target, batches them, and ships them as OTLP/HTTP JSON to a single endpoint.
//!
//! # Why it goes through the gateway
//!
//! **Telemetry is relayed by the gateway, never sent straight to a collector.** Giving
//! 200 collections desktops on a bank's network a route to our observability backend is
//! a security-review conversation with nothing on the other side of it. It is a second
//! egress to justify to the customer's network team, a second TLS trust decision on
//! machines that already have one, a second set of firewall rules to maintain per
//! floor, and — the part that actually matters — a second place data can leave the
//! building from a machine that handles borrower call audio. The gateway is already the
//! one authenticated egress these endpoints have; it already terminates mTLS with the
//! device certificate, so it is also the only party that can attribute a telemetry
//! record to a tenant without the client asserting one.
//!
//! **The gateway relay endpoint does not exist yet.** The client side is implemented
//! against `POST {api_base}/v1/telemetry/otlp/v1/logs`
//! ([`sentinel_core::config::OTLP_RELAY_PATH`]), which is an OTLP/HTTP logs endpoint
//! with the gateway's `/v1/telemetry/otlp` prefix in front of the standard `/v1/logs`
//! suffix, so a collector can be put behind it with a path rewrite and nothing else.
//! Until that route exists the exporter posts into a 404, logs that it is doing so once
//! per batch failure, and drops the batch. Nothing else in the client changes behaviour
//! as a result — which is the point of it being off by default.
//!
//! # What is instrumented, and what must never be
//!
//! Only events on [`TARGET`] are exported. That is not a filter for tidiness: it is the
//! containment boundary. Every ordinary `tracing::info!` in the client stays local, so
//! adding a log line somewhere cannot start shipping it off the machine, and the review
//! question "what leaves the endpoint?" has a greppable answer —
//! `target: "sentinel.telemetry"`.
//!
//! Acceptable attributes are machine state: `tenant.id`, `device.id`, capture state,
//! block reasons, tier, spool depth, connect counts, ack lag. Never acceptable: a user
//! UID, an account reference, a borrower name, transcript text, or anything derived
//! from audio. [`FORBIDDEN_ATTRIBUTES`] drops the field names that carry those if one
//! is ever attached, and `no_pii_attributes` in the tests asserts it — the same
//! belt-and-braces shape as `heartbeat::no_pii_fields`.
//!
//! # Shape of the exporter
//!
//! One bounded channel and one background thread. The channel is bounded because the
//! alternative is unbounded: an endpoint that loses its uplink is exactly the endpoint
//! generating the most telemetry, and a queue that grows without limit there would take
//! memory away from capture. When it is full, records are dropped and counted, and the
//! count is reported in the next batch — the same principle the spool applies to
//! eviction. A compliance product that quietly drops data is worse than one that
//! admits it did, and that applies to its own diagnostics too.
//!
//! No async runtime. `AGENTS.md`: the client is synchronous and blocking by design.

pub mod layer;
pub mod otlp;

pub use layer::{OtlpLayer, TelemetryHandle};
pub use otlp::{AttrValue, Record, Resource, Severity};

/// The one `tracing` target that is exported off the machine.
///
/// Everything else stays in the local log file. Changing this constant changes what
/// leaves the endpoint, so it is one line and it is here.
pub const TARGET: &str = "sentinel.telemetry";

/// Field names that are dropped rather than exported, whatever they contain.
///
/// A denylist rather than an allowlist, and that is a considered trade. An allowlist
/// would be strictly safer, but every new instrumentation point would need a second
/// edit in a second file, and the failure mode of forgetting it — an attribute silently
/// missing from a dashboard — is the kind of thing that gets "fixed" by widening the
/// allowlist to everything. The denylist covers the identifiers that actually exist in
/// this codebase and would be plausible to attach by accident, the export target is
/// itself opt-in per call site, and the test below pins the list.
pub const FORBIDDEN_ATTRIBUTES: &[&str] = &[
    // The user identity. The server already knows who is signed in, from the bearer
    // token; the endpoint has no reason to restate it, and a telemetry backend is a
    // much worse place for it than a token is.
    "user_uid",
    "uid",
    "firebase_uid",
    "user.id",
    "enduser.id",
    "email",
    "display_name",
    // Borrower-identifying. `account_ref` is a loan reference.
    "account_ref",
    "account",
    "borrower",
    "phone",
    "msisdn",
    // Audio-derived. None of this should exist on an endpoint at all — analysis is
    // server-side — but a future local VAD or UIA scrape could produce it.
    "transcript",
    "text",
    "utterance",
    "summary",
    "audio",
    "payload",
    // Credentials, for completeness. Nothing constructs these today.
    "token",
    "id_token",
    "refresh_token",
    "authorization",
    "enrollment_token",
    "spool_key",
    "private_key",
];

/// Is this field name safe to export?
///
/// Case-insensitive, and matches on the last dotted segment as well as the whole name,
/// so `capture.user_uid` is refused along with `user_uid`.
pub fn attribute_permitted(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    let last = lower.rsplit('.').next().unwrap_or(&lower);
    !FORBIDDEN_ATTRIBUTES
        .iter()
        .any(|f| lower == *f || last == *f)
}

/// Ships an encoded OTLP payload. A trait so the batching, the drop accounting and the
/// PII filter are all testable with no network.
pub trait Shipper: Send + Sync {
    fn ship(&self, payload: &[u8]) -> Result<(), String>;
    /// For the one log line that reports a failing endpoint.
    fn endpoint(&self) -> &str;
}

/// Event names used in instrumentation, so the strings that a dashboard queries on are
/// declared in one place rather than spelled slightly differently at each call site.
///
/// These are the occurrences that matter on an endpoint. The first two are the
/// product's dangerous silent failure: a machine that reports itself healthy, has a
/// signed-in user and a live dialer session, and is not recording. Everything the
/// server-side alert for that needs has to be said out loud by the client.
pub mod event {
    /// Capture armed for a call.
    pub const CAPTURE_ARMED: &str = "capture.armed";
    /// Capture left the armed/in-call states.
    pub const CAPTURE_DISARMED: &str = "capture.disarmed";
    /// Capture could **not** arm, with `reason`. The silent failure made loud.
    pub const CAPTURE_BLOCKED: &str = "capture.blocked";
    /// The capture pipeline could not be opened at all (no pinned device, no source).
    pub const CAPTURE_OPEN_FAILED: &str = "capture.open_failed";
    /// Tier detection outcome, including "neither" for a tier C machine.
    pub const TIER_DETECTED: &str = "capture.tier_detected";
    /// Spool depth and byte count, sampled with the heartbeat.
    pub const SPOOL_DEPTH: &str = "spool.depth";
    /// Segments dropped by an eviction, with the count.
    pub const SPOOL_EVICTED: &str = "spool.evicted";
    /// The ingest socket connected.
    pub const UPLINK_CONNECTED: &str = "uplink.connected";
    /// The ingest socket dropped, with the close code where there was one.
    pub const UPLINK_DISCONNECTED: &str = "uplink.disconnected";
    /// A connect attempt failed and a retry was scheduled.
    pub const UPLINK_CONNECT_FAILED: &str = "uplink.connect_failed";
    /// Time between a segment being handed to the socket and its ack arriving.
    pub const UPLINK_ACK_LAG: &str = "uplink.ack_lag";
    /// The gateway refused the device: `4403` or a 403 on the heartbeat.
    pub const DEVICE_REVOKED: &str = "device.revoked";
    /// A user signed in or out.
    pub const SIGN_IN_STATE: &str = "auth.sign_in_state";
    /// The device credential could not be loaded, so the uplink has no client
    /// certificate and capture spools locally.
    pub const DEVICE_CREDENTIAL_MISSING: &str = "device.credential_missing";
    /// The spool key could not be resolved, so capture is blocked rather than writing
    /// unencrypted audio.
    pub const SPOOL_KEY_UNAVAILABLE: &str = "spool.key_unavailable";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_target_leaves_the_machine() {
        // The containment boundary. If this string changes, what the endpoint exports
        // changes, and that is a review question rather than a refactor.
        assert_eq!(TARGET, "sentinel.telemetry");
    }

    #[test]
    fn no_pii_attributes() {
        // Spec 12.10 and 15, applied to telemetry as well as logs. The server derives
        // the user from the bearer token, and audio-derived content does not exist on
        // an endpoint at all — analysis is server-side.
        for forbidden in [
            "user_uid",
            "USER_UID",
            "capture.user_uid",
            "account_ref",
            "borrower",
            "transcript",
            "email",
            "display_name",
            "id_token",
            "refresh_token",
            "enrollment_token",
            "spool_key",
            "private_key",
        ] {
            assert!(!attribute_permitted(forbidden), "{forbidden} must not be exported");
        }
    }

    #[test]
    fn the_machine_state_attributes_are_permitted() {
        for allowed in [
            "tenant.id",
            "device.id",
            "capture.state",
            "capture.tier",
            "reason",
            "spool.depth",
            "spool.bytes",
            "uplink.close_code",
            "ack.lag_ms",
            "signed_in",
            "key_kind",
            "agent.restarts",
        ] {
            assert!(attribute_permitted(allowed), "{allowed} is machine state");
        }
    }

    #[test]
    fn a_prefixed_forbidden_name_is_still_forbidden() {
        // The denylist matches the last dotted segment as well as the whole key, so
        // namespacing a field does not smuggle it out.
        assert!(!attribute_permitted("call.account_ref"));
        assert!(!attribute_permitted("a.b.c.transcript"));
        // ...but a name that merely contains a forbidden word is fine, or every
        // `token_endpoint`-shaped field would vanish from dashboards.
        assert!(attribute_permitted("token_endpoint_configured"));
        assert!(attribute_permitted("account_count"));
    }

    #[test]
    fn the_event_names_are_the_occurrences_that_matter_on_an_endpoint() {
        // Pinned so a rename shows up as a test change rather than as a dashboard
        // that silently stops matching.
        assert_eq!(event::CAPTURE_BLOCKED, "capture.blocked");
        assert_eq!(event::TIER_DETECTED, "capture.tier_detected");
        assert_eq!(event::SPOOL_EVICTED, "spool.evicted");
        assert_eq!(event::UPLINK_ACK_LAG, "uplink.ack_lag");
        assert_eq!(event::SIGN_IN_STATE, "auth.sign_in_state");
    }
}
