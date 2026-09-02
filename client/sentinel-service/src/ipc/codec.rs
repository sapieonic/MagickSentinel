//! Framing and message types for `\\.\pipe\magickvoice-sentinel-v1` (spec 6.1).
//!
//! Kept free of any Windows API so both sides of the pipe agree on one definition and
//! the whole codec is round-trip tested on any platform. `ipc::server` (service side)
//! and `sentinel_agent::ipc` (agent side) are thin transports over this.
//!
//! **Framing:** a 4-byte little-endian length prefix followed by that many bytes of
//! UTF-8 JSON. A named pipe opened in byte mode does not preserve message boundaries
//! — `ReadFile` can and does return a partial message or two messages at once — so
//! the length prefix is what makes a message a message. [`FrameReader`] exists to be
//! fed whatever arbitrary chunk sizes the pipe hands over.

use serde::{Deserialize, Serialize};
use sentinel_core::config::{LocalConfig, Policy};
use sentinel_core::events::ClientEvent;

/// The pipe the service hosts and the agent connects to.
pub const PIPE_NAME: &str = r"\\.\pipe\magickvoice-sentinel-v1";

/// Security descriptor for the pipe, in SDDL.
///
/// * `(A;;GA;;;SY)` — full control (`GENERIC_ALL`) to `NT AUTHORITY\SYSTEM`.
/// * `(A;;GRGW;;;BU)` — `GENERIC_READ | GENERIC_WRITE` to `BUILTIN\Users`.
///
/// `BU` and not `AU`: `BUILTIN\Users` is what the spec names, and unlike
/// `Authenticated Users` it excludes machine accounts and service logons that have no
/// business talking to this pipe. There is deliberately no `WD` (Everyone) ACE — an
/// anonymous logon must not be able to ask the service for tenant config.
///
/// Note the missing owner/group fields: a `D:`-only descriptor inherits the creating
/// process's default owner, which for the service is SYSTEM. That is what we want,
/// and spelling out `O:SY` would fail to convert on a machine where SYSTEM is not the
/// process token owner.
pub const PIPE_SDDL: &str = "D:(A;;GA;;;SY)(A;;GRGW;;;BU)";

/// Largest single IPC message. Config payloads are a few kilobytes; anything at this
/// size is a bug or an attack, and a length prefix read from a pipe must never be
/// trusted enough to allocate from.
pub const MAX_FRAME_BYTES: usize = 256 * 1024;

/// Length-prefix width, in bytes.
pub const LENGTH_PREFIX_BYTES: usize = 4;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IpcError {
    #[error("frame of {0} bytes exceeds the {MAX_FRAME_BYTES} byte limit")]
    FrameTooLarge(usize),
    #[error("zero-length frame")]
    EmptyFrame,
    #[error("frame is not valid UTF-8")]
    NotUtf8,
    #[error("frame is not a valid IPC message: {0}")]
    BadJson(String),
}

// ------------------------------------------------------------------- messages

/// Agent → service. The four message types the spec names, and nothing else: every
/// addition here is a new privilege the user session gains over a SYSTEM process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum Request {
    /// Fetch the machine-scoped local config and the last tenant policy the service
    /// synced. The agent asks rather than reading `%PROGRAMDATA%` itself so there is
    /// one writer.
    GetConfig,

    /// Liveness and capture health, forwarded into the service's next heartbeat and
    /// used by the watchdog to tell "agent alive but not capturing" from "agent gone".
    ReportHealth(HealthReport),

    /// Ask the service to check for an update now instead of at its next poll.
    RequestUpdateCheck,

    /// Record a client event. These reach the server in the heartbeat body, so the
    /// same no-PII rule applies: machine state only, never call content.
    LogEvent(ClientEvent),
}

/// Service → agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum Response {
    // Boxed: the config snapshot carries a whole `Policy` and dwarfs every other
    // variant, so an unboxed enum would make even an `Ok` reply several hundred bytes
    // on the stack. Serde treats `Box<T>` as `T`, so the wire format is unchanged.
    Config(Box<ConfigSnapshot>),
    /// Acknowledgement for requests with no payload.
    Ok,
    UpdateStatus(UpdateStatus),
    Error {
        code: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigSnapshot {
    pub local: LocalConfig,
    /// `None` before the service has ever reached `/v1/policy`. The agent then holds
    /// capture blocked rather than guessing at a pinned device.
    pub policy: Option<Policy>,
    /// Detected on every service start: in-place OS upgrades change the answer.
    pub capture_tier: Option<String>,
    pub os_build: String,
    pub service_version: String,
    /// Agent relaunches the service has performed since its counter last reset. The
    /// agent forwards this in the heartbeat so tamper shows up server-side.
    pub agent_restarts: u32,
}

/// Health as the agent sees it. No call content, no account reference, no user name —
/// `user_signed_in` is a boolean precisely so this message cannot carry an identity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthReport {
    /// `CallState::as_str()`, e.g. `"IN_CALL"`.
    pub capture_state: String,
    pub user_signed_in: bool,
    pub spool_depth: u64,
    pub spool_bytes: u64,
    pub pinned_device_present: bool,
    pub agent_version: String,
    /// Milliseconds since the agent started; a wall-clock timestamp here would be one
    /// more thing to disagree about across a session boundary.
    pub uptime_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdateStatus {
    pub current_version: String,
    pub staged_version: Option<String>,
    pub checking: bool,
}

// --------------------------------------------------------------------- coding

/// Encode a message as a length-prefixed JSON frame.
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, IpcError> {
    let json = serde_json::to_vec(msg).map_err(|e| IpcError::BadJson(e.to_string()))?;
    if json.len() > MAX_FRAME_BYTES {
        return Err(IpcError::FrameTooLarge(json.len()));
    }
    let mut out = Vec::with_capacity(LENGTH_PREFIX_BYTES + json.len());
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(&json);
    Ok(out)
}

/// Decode one complete frame body (the JSON, without the length prefix).
pub fn decode<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<T, IpcError> {
    if body.is_empty() {
        return Err(IpcError::EmptyFrame);
    }
    let s = std::str::from_utf8(body).map_err(|_| IpcError::NotUtf8)?;
    serde_json::from_str(s).map_err(|e| IpcError::BadJson(e.to_string()))
}

/// Incremental reassembler for a byte stream that does not preserve message
/// boundaries.
#[derive(Debug, Default)]
pub struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    pub fn new() -> Self {
        FrameReader { buf: Vec::new() }
    }

    /// Add bytes as they arrive from the pipe.
    pub fn feed(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// Bytes buffered but not yet forming a complete frame. A caller that sees this
    /// grow without bound is talking to something that is not the agent.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Pop the next complete frame body, if one has arrived.
    ///
    /// An oversized length prefix is a hard error rather than a "wait for more data":
    /// waiting would let a peer make the service buffer forever by announcing 4 GiB
    /// and then sending nothing.
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, IpcError> {
        if self.buf.len() < LENGTH_PREFIX_BYTES {
            return Ok(None);
        }
        let len = u32::from_le_bytes(self.buf[..LENGTH_PREFIX_BYTES].try_into().unwrap()) as usize;
        if len == 0 {
            return Err(IpcError::EmptyFrame);
        }
        if len > MAX_FRAME_BYTES {
            return Err(IpcError::FrameTooLarge(len));
        }
        let total = LENGTH_PREFIX_BYTES + len;
        if self.buf.len() < total {
            return Ok(None);
        }
        let body = self.buf[LENGTH_PREFIX_BYTES..total].to_vec();
        self.buf.drain(..total);
        Ok(Some(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_core::events::EventKind;

    fn health() -> HealthReport {
        HealthReport {
            capture_state: "IN_CALL".into(),
            user_signed_in: true,
            spool_depth: 12,
            spool_bytes: 40_960,
            pinned_device_present: true,
            agent_version: "0.1.0".into(),
            uptime_ms: 61_000,
        }
    }

    fn all_requests() -> Vec<Request> {
        vec![
            Request::GetConfig,
            Request::ReportHealth(health()),
            Request::RequestUpdateCheck,
            Request::LogEvent(ClientEvent::new(EventKind::AgentRestart, 900).with_count(2)),
        ]
    }

    fn all_responses() -> Vec<Response> {
        vec![
            Response::Config(Box::new(ConfigSnapshot {
                local: LocalConfig::default(),
                policy: Some(Policy::default()),
                capture_tier: Some("B".into()),
                os_build: "10.0.19045".into(),
                service_version: "0.1.0".into(),
                agent_restarts: 3,
            })),
            Response::Ok,
            Response::UpdateStatus(UpdateStatus {
                current_version: "0.1.0".into(),
                staged_version: Some("0.1.1".into()),
                checking: false,
            }),
            Response::Error { code: "not_ready".into(), message: "no policy yet".into() },
        ]
    }

    #[test]
    fn every_request_round_trips() {
        for msg in all_requests() {
            let frame = encode(&msg).unwrap();
            let mut r = FrameReader::new();
            r.feed(&frame);
            let body = r.next_frame().unwrap().expect("one complete frame");
            assert_eq!(decode::<Request>(&body).unwrap(), msg);
            assert_eq!(r.next_frame().unwrap(), None, "no trailing frame");
        }
    }

    #[test]
    fn every_response_round_trips() {
        for msg in all_responses() {
            let frame = encode(&msg).unwrap();
            let mut r = FrameReader::new();
            r.feed(&frame);
            let body = r.next_frame().unwrap().unwrap();
            assert_eq!(decode::<Response>(&body).unwrap(), msg);
        }
    }

    #[test]
    fn a_frame_split_byte_by_byte_reassembles() {
        // This is the case a byte-mode pipe actually produces, and the one a naive
        // "read once, parse once" implementation gets wrong under load.
        let frame = encode(&Request::ReportHealth(health())).unwrap();
        let mut r = FrameReader::new();
        for (i, b) in frame.iter().enumerate() {
            assert_eq!(r.next_frame().unwrap(), None, "premature frame at byte {i}");
            r.feed(&[*b]);
        }
        let body = r.next_frame().unwrap().expect("frame completes on the last byte");
        assert!(matches!(decode::<Request>(&body).unwrap(), Request::ReportHealth(_)));
    }

    #[test]
    fn several_frames_in_one_read_all_come_out_in_order() {
        let mut wire = Vec::new();
        for msg in all_requests() {
            wire.extend_from_slice(&encode(&msg).unwrap());
        }
        let mut r = FrameReader::new();
        r.feed(&wire);
        let mut got = Vec::new();
        while let Some(body) = r.next_frame().unwrap() {
            got.push(decode::<Request>(&body).unwrap());
        }
        assert_eq!(got, all_requests());
        assert_eq!(r.buffered(), 0);
    }

    #[test]
    fn an_oversized_length_prefix_is_rejected_immediately() {
        // Not "wait for more data": a peer that announces 4 GiB and sends nothing
        // would otherwise pin the service's buffer forever.
        let mut r = FrameReader::new();
        r.feed(&u32::MAX.to_le_bytes());
        assert_eq!(r.next_frame(), Err(IpcError::FrameTooLarge(u32::MAX as usize)));

        let mut r = FrameReader::new();
        r.feed(&((MAX_FRAME_BYTES + 1) as u32).to_le_bytes());
        assert!(matches!(r.next_frame(), Err(IpcError::FrameTooLarge(_))));
    }

    #[test]
    fn a_zero_length_frame_is_rejected() {
        let mut r = FrameReader::new();
        r.feed(&0u32.to_le_bytes());
        assert_eq!(r.next_frame(), Err(IpcError::EmptyFrame));
    }

    #[test]
    fn garbage_inside_a_well_framed_message_is_a_decode_error_not_a_panic() {
        let mut wire = Vec::new();
        let body = b"{\"t\":\"NoSuchMessage\"}";
        wire.extend_from_slice(&(body.len() as u32).to_le_bytes());
        wire.extend_from_slice(body);
        let mut r = FrameReader::new();
        r.feed(&wire);
        let body = r.next_frame().unwrap().unwrap();
        assert!(matches!(decode::<Request>(&body), Err(IpcError::BadJson(_))));
    }

    #[test]
    fn invalid_utf8_is_rejected() {
        assert_eq!(decode::<Request>(&[0xff, 0xfe, 0xfd]), Err(IpcError::NotUtf8));
    }

    #[test]
    fn the_sddl_grants_only_system_and_builtin_users() {
        // Spelled out as a test because getting this wrong is silent: a pipe with a
        // NULL DACL accepts everyone, and nothing about it looks broken in testing.
        assert_eq!(PIPE_SDDL, "D:(A;;GA;;;SY)(A;;GRGW;;;BU)");
        assert!(!PIPE_SDDL.contains(";WD)"), "Everyone must not appear in the DACL");
        assert!(!PIPE_SDDL.contains(";AN)"), "Anonymous must not appear in the DACL");
        assert!(PIPE_SDDL.starts_with("D:("), "a DACL with no ACEs would deny everyone");
    }

    #[test]
    fn health_reports_carry_no_identity() {
        // The heartbeat that carries this reaches the server, so the no-PII rule
        // applies. `user_signed_in` is a boolean on purpose.
        let json = serde_json::to_string(&Request::ReportHealth(health())).unwrap();
        for pii in ["uid", "email", "account", "display_name", "user_uid"] {
            assert!(!json.contains(pii), "health report leaked {pii}: {json}");
        }
    }
}
