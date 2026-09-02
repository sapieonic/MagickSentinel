//! Client-side events reported in the next heartbeat.
//!
//! These are the tamper- and health-detection signal (spec section 6.8). None of them
//! may carry PII: no transcript text, no account reference, no borrower name. The
//! `detail` field is for machine state, not call content.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// Tier A activation failed and the session fell back to endpoint loopback.
    TierDowngrade,
    /// Audio was dropped from the spool. Always carries a count.
    SpoolEviction,
    /// The pinned audio endpoint disappeared.
    DeviceLost,
    DeviceRestored,
    /// The service relaunched the agent.
    AgentRestart,
    CaptureError,
    /// Tier B: loopback energy while the softphone session was Inactive.
    ForeignAudioSuppressed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientEvent {
    pub kind: EventKind,
    /// Client clock, milliseconds. Converted to an absolute timestamp against the
    /// server's clock when the heartbeat is assembled.
    pub at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl ClientEvent {
    pub fn new(kind: EventKind, at_ms: u64) -> Self {
        ClientEvent { kind, at_ms, count: None, detail: None }
    }

    pub fn with_count(mut self, count: u64) -> Self {
        self.count = Some(count);
        self
    }

    pub fn with_detail(mut self, detail: String) -> Self {
        self.detail = Some(detail);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_serialise_with_snake_case_kinds() {
        let e = ClientEvent::new(EventKind::SpoolEviction, 42).with_count(7);
        let v: serde_json::Value = serde_json::to_value(&e).unwrap();
        assert_eq!(v["kind"], "spool_eviction");
        assert_eq!(v["count"], 7);
        assert!(v.get("detail").is_none(), "empty fields are omitted, not null");
    }
}
