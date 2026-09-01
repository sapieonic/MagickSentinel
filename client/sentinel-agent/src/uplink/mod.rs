//! The uplink: spool → `WSS /v1/ingest` (spec 6.6, wire.md).
//!
//! Invariants this module exists to hold:
//!
//! * **A segment is deleted only after the server acks it.** Not on `call.end`, not
//!   on shutdown, not on sign-out.
//! * **`call_id` is client-generated and re-sent verbatim.** That is what makes
//!   retry-after-reconnect idempotent, and it is why a reconnect replays the stored
//!   `call.start` rather than minting a new one.
//! * **A fatal `call.error` seals the call and reports the loss.** The audio can never
//!   be accepted, so holding it just fills the disk — but silent data loss is
//!   unacceptable in a compliance product, so the eviction emits an event.

pub mod resume;
pub mod transport;

use sentinel_core::backoff::Backoff;
use sentinel_core::events::{ClientEvent, EventKind};
use sentinel_core::protocol::{
    Channel, ControlMessage, MediaRecord, SEGMENTS_PER_MESSAGE,
};
use sentinel_core::spool::{SegmentRow, Spool, SpoolStats};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;
use transport::{Incoming, Transport, TransportError};

/// How long `pump` waits for a message before returning to its caller. Short enough
/// that the heartbeat and the capture pipeline are not starved; long enough that the
/// loop is not a spin.
const RECV_SLICE: Duration = Duration::from_millis(50);

/// Segments read out of the spool per `pump` pass. One WebSocket message carries
/// `SEGMENTS_PER_MESSAGE`; four messages a pass keeps a reconnect draining a backlog
/// briskly without monopolising the loop.
const SEGMENTS_PER_PASS: usize = SEGMENTS_PER_MESSAGE * 4;

/// Opens connections to the ingest endpoint. A trait so the uplink can be driven
/// against an in-process gateway in CI.
pub trait TransportFactory: Send {
    fn connect(&mut self) -> Result<Box<dyn Transport>, TransportError>;
}

/// What one `pump` pass did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PumpOutcome {
    pub connected: bool,
    /// Segments handed to the socket this pass.
    pub sent: usize,
    /// Segments the server acked (and the spool therefore deleted) this pass.
    pub acked: usize,
    /// The server accepted something from us, so the user token verified server-side
    /// just now. Feeds the offline-grace clock.
    pub verified: bool,
    /// A `4403` arrived. Terminal until an operator acts: capture MUST stop.
    pub device_revoked: bool,
    /// A `4401` arrived: refresh the ID token before reconnecting.
    pub token_rejected: bool,
    /// The newest `policy_version` the server reported, if it differs from ours.
    pub policy_version: Option<i64>,
}

pub struct Uplink {
    spool: Spool,
    factory: Box<dyn TransportFactory>,
    transport: Option<Box<dyn Transport>>,
    backoff: Backoff,
    /// Earliest clock reading at which a reconnect may be attempted.
    next_attempt_ms: u64,
    /// Calls whose `call.start` has been sent on the *current* connection. Cleared on
    /// every reconnect, because the server needs it again to reply with `resume`.
    started_this_connection: BTreeSet<String>,
    /// Calls whose `call.end` has been sent on the current connection.
    ended_this_connection: BTreeSet<String>,
    /// Calls the spool holds, including ones that have produced no segments yet.
    known_calls: BTreeSet<String>,
    /// Per-call resume point, from the server's `resume` frame combined with ours.
    resume_points: BTreeMap<String, resume::ResumePoint>,
    events: Vec<ClientEvent>,
    policy_version: i64,
}

impl Uplink {
    pub fn new(spool: Spool, factory: Box<dyn TransportFactory>) -> Self {
        let known_calls = spool.pending_calls().unwrap_or_default().into_iter().collect();
        Uplink {
            spool,
            factory,
            transport: None,
            backoff: Backoff::uplink(),
            next_attempt_ms: 0,
            started_this_connection: BTreeSet::new(),
            ended_this_connection: BTreeSet::new(),
            known_calls,
            resume_points: BTreeMap::new(),
            events: Vec::new(),
            policy_version: 0,
        }
    }

    pub fn is_connected(&self) -> bool {
        self.transport.is_some()
    }

    pub fn stats(&self) -> SpoolStats {
        self.spool.stats().unwrap_or_default()
    }

    /// Calls the spool still holds unacked segments for. Read-only view for the
    /// heartbeat and for tests.
    pub fn spool_pending_calls(&self) -> Vec<String> {
        self.spool.pending_calls().unwrap_or_default()
    }

    /// Unacked segments for a call, in `(channel, seq)` order.
    pub fn spool_take_pending(&self, call_id: &str, limit: usize) -> Vec<SegmentRow> {
        self.spool.take_pending(call_id, limit).unwrap_or_default()
    }

    /// Client events accumulated since the last call, for the next heartbeat.
    pub fn take_events(&mut self) -> Vec<ClientEvent> {
        std::mem::take(&mut self.events)
    }

    /// Record `call.start` verbatim and open the call in the spool.
    pub fn begin_call(
        &mut self,
        call_id: &str,
        start: &ControlMessage,
        now_ms: u64,
    ) -> anyhow::Result<()> {
        let json = start.to_json()?;
        self.spool.begin_call(call_id, &json, now_ms)?;
        self.known_calls.insert(call_id.to_string());
        Ok(())
    }

    /// Record `call.end`. Note what this does *not* do: delete anything.
    pub fn end_call(&mut self, call_id: &str, end: &ControlMessage) -> anyhow::Result<()> {
        self.spool.end_call(call_id, &end.to_json()?)?;
        Ok(())
    }

    /// Spool one encoded segment.
    pub fn push_segment(&mut self, seg: &SegmentRow) -> anyhow::Result<()> {
        if let Some(ev) = self.spool.push(seg)? {
            self.events.push(ev);
        }
        Ok(())
    }

    /// Drive one pass: connect if due, send what is pending, read what has arrived.
    pub fn pump(&mut self, now_ms: u64) -> PumpOutcome {
        let mut outcome = PumpOutcome::default();

        if self.transport.is_none() {
            if now_ms < self.next_attempt_ms {
                return outcome;
            }
            match self.factory.connect() {
                Ok(t) => {
                    tracing::info!("ingest connected");
                    self.transport = Some(t);
                    self.started_this_connection.clear();
                    self.ended_this_connection.clear();
                    self.resume_points.clear();
                }
                Err(e) => {
                    let delay = self.backoff.next_delay();
                    self.next_attempt_ms = now_ms.saturating_add(delay.as_millis() as u64);
                    tracing::warn!(error = %e, retry_in_ms = delay.as_millis() as u64, "ingest connect failed");
                    return outcome;
                }
            }
        }
        outcome.connected = true;

        if let Err(e) = self.send_pending(&mut outcome) {
            self.drop_connection(now_ms, &e.to_string());
            outcome.connected = false;
            return outcome;
        }
        if let Err(e) = self.read_incoming(now_ms, &mut outcome) {
            self.drop_connection(now_ms, &e.to_string());
            outcome.connected = false;
        }
        outcome
    }

    /// Send `call.start`, then media, then `call.end`, for every call the spool holds.
    fn send_pending(&mut self, outcome: &mut PumpOutcome) -> Result<(), TransportError> {
        // Refresh from the spool so a call whose segments were all acked drops out and
        // one that was recovered from disk at startup appears.
        for c in self.spool.pending_calls().unwrap_or_default() {
            self.known_calls.insert(c);
        }
        let calls: Vec<String> = self.known_calls.iter().cloned().collect();

        for call_id in calls {
            if self.spool.is_sealed(&call_id).unwrap_or(false) {
                self.known_calls.remove(&call_id);
                continue;
            }

            if !self.started_this_connection.contains(&call_id) {
                // Verbatim: re-sending the original frame is what lets the server
                // recognise the call and answer with `resume` instead of creating a
                // second row for the same conversation.
                if let Ok(Some(json)) = self.spool.call_start_json(&call_id) {
                    self.transport_mut()?.send_text(&json)?;
                    self.started_this_connection.insert(call_id.clone());
                }
            }

            let point = self.resume_point_for(&call_id);
            let pending = self
                .spool
                .take_pending(&call_id, SEGMENTS_PER_PASS)
                .unwrap_or_default();

            let mut batch: Vec<u8> = Vec::new();
            let mut in_batch = 0usize;
            for row in pending {
                if !resume::should_send(&point, row.channel, row.seq) {
                    continue;
                }
                let record = row.to_record(call_id_bytes(&call_id));
                if record.encode_into(&mut batch).is_err() {
                    // A segment too large for the u16 length field cannot exist at
                    // 24 kbps; if one does, dropping it is better than desynchronising
                    // the whole message.
                    tracing::error!(seq = row.seq, "segment too large to encode; skipped");
                    continue;
                }
                in_batch += 1;
                outcome.sent += 1;
                if in_batch == SEGMENTS_PER_MESSAGE {
                    self.transport_mut()?.send_binary(&batch)?;
                    batch.clear();
                    in_batch = 0;
                }
            }
            if in_batch > 0 {
                self.transport_mut()?.send_binary(&batch)?;
            }

            // `call.end` last, after this pass's media: it carries `last_seq`, and the
            // server holds finalization open until every sequence below it arrives.
            if !self.ended_this_connection.contains(&call_id) {
                if let Ok(Some(json)) = self.spool.call_end_json(&call_id) {
                    self.transport_mut()?.send_text(&json)?;
                    self.ended_this_connection.insert(call_id.clone());
                }
            }
        }
        Ok(())
    }

    fn read_incoming(&mut self, now_ms: u64, outcome: &mut PumpOutcome) -> Result<(), TransportError> {
        loop {
            let msg = self.transport_mut()?.recv(RECV_SLICE)?;
            let Some(msg) = msg else { return Ok(()) };
            match msg {
                Incoming::Text(text) => self.handle_control(&text, now_ms, outcome),
                Incoming::Binary(_) => {
                    // The gateway never sends binary on this socket.
                    tracing::debug!("ignoring unexpected binary frame from the gateway");
                }
                Incoming::Closed { code } => {
                    match code {
                        Some(transport::close::FORBIDDEN) => {
                            // Terminal until an operator acts: revoked device, tenant
                            // mismatch, or a role that may not ingest.
                            outcome.device_revoked = true;
                            tracing::error!("ingest refused: device revoked or not permitted");
                        }
                        Some(transport::close::TOKEN_INVALID) => {
                            outcome.token_rejected = true;
                        }
                        Some(transport::close::TOO_MANY) => {
                            // Another agent instance holds the device's connection.
                            // Backing off is the whole remedy; retrying immediately
                            // would spin against a limit that is not about us.
                            tracing::warn!("ingest refused: too many connections for this device");
                        }
                        _ => {}
                    }
                    return Err(TransportError::Closed(code));
                }
            }
        }
    }

    fn handle_control(&mut self, text: &str, now_ms: u64, outcome: &mut PumpOutcome) {
        let Ok(msg) = ControlMessage::from_json(text) else {
            tracing::warn!("undecodable control frame from the gateway");
            return;
        };
        match msg {
            ControlMessage::Ack { call_id, channel, through_seq } => {
                let Ok(ch) = Channel::from_u8(channel) else { return };
                match self.spool.ack(&call_id, ch, through_seq) {
                    Ok(deleted) => {
                        outcome.acked += deleted;
                        outcome.verified = true;
                        self.backoff.reset();
                    }
                    Err(e) => tracing::warn!(error = %e, "ack could not be applied"),
                }
            }
            ControlMessage::Resume { call_id, acked } => {
                let local = self.spool.acked_through(&call_id).unwrap_or_default();
                let point = resume::plan(&local, &acked);
                tracing::info!(
                    far_from = point.from_seq(Channel::Far),
                    near_from = point.from_seq(Channel::Near),
                    "resuming a call after reconnect"
                );
                self.resume_points.insert(call_id, point);
                outcome.verified = true;
            }
            ControlMessage::CallError { call_id, code, message, fatal } => {
                if fatal {
                    // The audio can never be accepted. Seal it, and say how much was
                    // lost: a compliance product that quietly drops audio is worse
                    // than one that admits it did.
                    match self.spool.seal(&call_id, &code, now_ms) {
                        Ok(ev) => {
                            tracing::error!(code = %code, "call rejected fatally; spool sealed");
                            self.events.push(ev);
                        }
                        Err(e) => tracing::error!(error = %e, "sealing the spool failed"),
                    }
                    self.known_calls.remove(&call_id);
                } else {
                    // No PII: `message` is the gateway's text and could in principle
                    // echo something from the frame, so only the code is logged.
                    let _ = message;
                    tracing::warn!(code = %code, "recoverable call error");
                }
            }
            ControlMessage::HeartbeatAck { policy_version, .. } => {
                outcome.verified = true;
                if policy_version != self.policy_version {
                    self.policy_version = policy_version;
                    outcome.policy_version = Some(policy_version);
                }
            }
            other => tracing::debug!(?other, "unexpected control frame direction"),
        }
    }

    /// Liveness and clock-skew probe on the ingest socket (wire.md 3.6). The
    /// authoritative heartbeat is `POST /v1/heartbeat`.
    pub fn send_socket_heartbeat(&mut self, sent_at: &str, capture_state: &str) -> bool {
        let depth = self.stats().segments;
        let msg = ControlMessage::Heartbeat {
            sent_at: sent_at.to_string(),
            capture_state: capture_state.to_string(),
            spool_depth: depth,
        };
        match (self.transport.as_mut(), msg.to_json()) {
            (Some(t), Ok(json)) => t.send_text(&json).is_ok(),
            _ => false,
        }
    }

    /// Drain everything outstanding, or give up at the deadline.
    ///
    /// Used by sign-out, which must flush before tokens are cleared. Returns the
    /// number of segments still unacked.
    pub fn flush(&mut self, now_ms: u64, deadline: Duration) -> u64 {
        let started = std::time::Instant::now();
        let mut clock = now_ms;
        while started.elapsed() < deadline {
            let before = self.stats().segments;
            if before == 0 {
                return 0;
            }
            let outcome = self.pump(clock);
            clock = clock.saturating_add(RECV_SLICE.as_millis() as u64);
            if !outcome.connected && outcome.sent == 0 && outcome.acked == 0 {
                // Not connected and not due to reconnect. Spinning would burn the
                // deadline without making progress.
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        self.stats().segments
    }

    fn resume_point_for(&self, call_id: &str) -> resume::ResumePoint {
        if let Some(p) = self.resume_points.get(call_id) {
            return p.clone();
        }
        // No `resume` yet — either a brand-new call, or one whose reply has not
        // arrived. Fall back to our own ack watermark: re-sending is cheap because
        // ingest is idempotent, whereas waiting for a `resume` the server will never
        // send for a new call would deadlock the call's first second.
        let local = self.spool.acked_through(call_id).unwrap_or_default();
        resume::plan(&local, &BTreeMap::new())
    }

    fn transport_mut(&mut self) -> Result<&mut Box<dyn Transport>, TransportError> {
        self.transport
            .as_mut()
            .ok_or_else(|| TransportError::Io("no connection".into()))
    }

    fn drop_connection(&mut self, now_ms: u64, why: &str) {
        if let Some(mut t) = self.transport.take() {
            let _ = t.close();
        }
        let delay = self.backoff.next_delay();
        self.next_attempt_ms = now_ms.saturating_add(delay.as_millis() as u64);
        tracing::warn!(reason = why, retry_in_ms = delay.as_millis() as u64, "ingest disconnected");
        self.events
            .push(ClientEvent::new(EventKind::CaptureError, now_ms).with_detail("uplink_reconnect".into()));
    }
}

/// A ULID's 16 binary bytes, for the media record header.
///
/// A `call_id` that is not a ULID cannot happen — the agent mints them — but a
/// zero-filled fallback is better than a panic in the upload path, and the server
/// will reject it visibly with `unknown_call`.
pub fn call_id_bytes(call_id: &str) -> [u8; 16] {
    match call_id.parse::<ulid::Ulid>() {
        Ok(u) => u.to_bytes(),
        Err(_) => {
            tracing::error!("call_id is not a ULID; media records will be rejected");
            [0u8; 16]
        }
    }
}

/// Decode media records the way the gateway does. Used by the integration test's fake
/// gateway and by anything verifying what went out.
pub fn decode_records(bytes: &[u8]) -> Result<Vec<MediaRecord>, sentinel_core::protocol::ProtocolError> {
    MediaRecord::decode_all(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_core::config::SpoolLimits;
    use sentinel_core::protocol::{
        CallEnd, CallStart, CaptureTier, Direction, EndReason, MediaFlags,
    };
    use std::sync::{Arc, Mutex};

    /// A transport that records what was sent and replays a scripted inbox.
    #[derive(Default)]
    struct FakeWire {
        sent_text: Vec<String>,
        sent_binary: Vec<Vec<u8>>,
        inbox: std::collections::VecDeque<Incoming>,
        closed: bool,
    }

    #[derive(Clone, Default)]
    struct FakeHandle(Arc<Mutex<FakeWire>>);

    struct FakeTransport(FakeHandle);

    impl Transport for FakeTransport {
        fn send_text(&mut self, s: &str) -> Result<(), TransportError> {
            self.0 .0.lock().unwrap().sent_text.push(s.to_string());
            Ok(())
        }
        fn send_binary(&mut self, b: &[u8]) -> Result<(), TransportError> {
            self.0 .0.lock().unwrap().sent_binary.push(b.to_vec());
            Ok(())
        }
        fn recv(&mut self, _t: Duration) -> Result<Option<Incoming>, TransportError> {
            Ok(self.0 .0.lock().unwrap().inbox.pop_front())
        }
        fn close(&mut self) -> Result<(), TransportError> {
            self.0 .0.lock().unwrap().closed = true;
            Ok(())
        }
    }

    struct FakeFactory {
        handle: FakeHandle,
        fail_next: Arc<Mutex<usize>>,
        connects: Arc<Mutex<usize>>,
    }

    impl TransportFactory for FakeFactory {
        fn connect(&mut self) -> Result<Box<dyn Transport>, TransportError> {
            let mut fail = self.fail_next.lock().unwrap();
            if *fail > 0 {
                *fail -= 1;
                return Err(TransportError::Connect("no route".into()));
            }
            *self.connects.lock().unwrap() += 1;
            Ok(Box::new(FakeTransport(self.handle.clone())))
        }
    }

    struct Rig {
        uplink: Uplink,
        wire: FakeHandle,
        fail_next: Arc<Mutex<usize>>,
        connects: Arc<Mutex<usize>>,
    }

    fn rig() -> Rig {
        let handle = FakeHandle::default();
        let fail_next = Arc::new(Mutex::new(0));
        let connects = Arc::new(Mutex::new(0));
        let factory = FakeFactory {
            handle: handle.clone(),
            fail_next: fail_next.clone(),
            connects: connects.clone(),
        };
        let spool = Spool::open_in_memory(SpoolLimits::default()).unwrap();
        Rig {
            uplink: Uplink::new(spool, Box::new(factory)),
            wire: handle,
            fail_next,
            connects,
        }
    }

    const CALL: &str = "01J8ZQ8H2Q7X9K3M4N5P6R7S8T";

    fn call_start() -> ControlMessage {
        ControlMessage::CallStart(CallStart {
            call_id: CALL.into(),
            started_at: "2026-09-01T10:14:02.113Z".into(),
            user_uid: "uid-a".into(),
            device_id: "dev-1".into(),
            tier: CaptureTier::A,
            account_ref: None,
            dialer_call_id: None,
            direction: Direction::Outbound,
            codec: "opus".into(),
            rate: 16000,
        })
    }

    fn segment(ch: Channel, seq: u32) -> SegmentRow {
        SegmentRow {
            call_id: CALL.into(),
            channel: ch,
            seq,
            timestamp_ms: seq as u64 * 1000,
            flags: MediaFlags::default(),
            payload: vec![seq as u8; 60],
            created_ms: seq as u64 * 1000,
        }
    }

    fn sent_records(wire: &FakeHandle) -> Vec<MediaRecord> {
        wire.0
            .lock()
            .unwrap()
            .sent_binary
            .iter()
            .flat_map(|m| decode_records(m).unwrap())
            .collect()
    }

    #[test]
    fn a_call_sends_its_start_frame_then_its_media() {
        let mut r = rig();
        r.uplink.begin_call(CALL, &call_start(), 0).unwrap();
        for seq in 0..3 {
            r.uplink.push_segment(&segment(Channel::Far, seq)).unwrap();
        }
        let out = r.uplink.pump(0);
        assert!(out.connected);
        assert_eq!(out.sent, 3);

        let wire = r.wire.0.lock().unwrap();
        assert_eq!(wire.sent_text.len(), 1);
        assert!(wire.sent_text[0].contains("\"t\":\"call.start\""));
        assert!(wire.sent_text[0].contains(CALL));
        drop(wire);

        let records = sent_records(&r.wire);
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].call_id, call_id_bytes(CALL));
        assert_eq!(records.iter().map(|r| r.seq).collect::<Vec<_>>(), [0, 1, 2]);
    }

    #[test]
    fn segments_are_deleted_only_when_the_server_acks_them() {
        let mut r = rig();
        r.uplink.begin_call(CALL, &call_start(), 0).unwrap();
        for seq in 0..5 {
            r.uplink.push_segment(&segment(Channel::Far, seq)).unwrap();
        }
        r.uplink.pump(0);
        assert_eq!(r.uplink.stats().segments, 5, "sending is not acking");

        // Ending the call must not delete anything either.
        r.uplink
            .end_call(
                CALL,
                &ControlMessage::CallEnd(CallEnd {
                    call_id: CALL.into(),
                    ended_at: "2026-09-01T10:19:44.802Z".into(),
                    reason: EndReason::Hangup,
                    last_seq: [("0".to_string(), 4u32)].into_iter().collect(),
                }),
            )
            .unwrap();
        r.uplink.pump(10);
        assert_eq!(r.uplink.stats().segments, 5, "call.end is not an ack");

        r.wire.0.lock().unwrap().inbox.push_back(Incoming::Text(
            serde_json::json!({"t":"ack","call_id":CALL,"channel":0,"through_seq":2}).to_string(),
        ));
        let out = r.uplink.pump(20);
        assert_eq!(out.acked, 3);
        assert!(out.verified, "an ack proves the token verified server-side");
        assert_eq!(r.uplink.stats().segments, 2);
    }

    #[test]
    fn a_reconnect_replays_call_start_verbatim_and_resumes_past_the_ack() {
        let mut r = rig();
        r.uplink.begin_call(CALL, &call_start(), 0).unwrap();
        for seq in 0..6 {
            r.uplink.push_segment(&segment(Channel::Far, seq)).unwrap();
        }
        r.uplink.pump(0);
        r.wire.0.lock().unwrap().inbox.push_back(Incoming::Text(
            serde_json::json!({"t":"ack","call_id":CALL,"channel":0,"through_seq":1}).to_string(),
        ));
        r.uplink.pump(10);
        assert_eq!(r.uplink.stats().segments, 4);

        // Sever the link.
        r.wire
            .0
            .lock()
            .unwrap()
            .inbox
            .push_back(Incoming::Closed { code: Some(1011) });
        let out = r.uplink.pump(20);
        assert!(!out.connected);
        assert!(!r.uplink.is_connected());

        // Reconnect, and the server reports it actually has one more than we recorded.
        r.wire.0.lock().unwrap().sent_text.clear();
        r.wire.0.lock().unwrap().sent_binary.clear();
        r.wire.0.lock().unwrap().inbox.push_back(Incoming::Text(
            serde_json::json!({"t":"resume","call_id":CALL,"acked":{"0":2}}).to_string(),
        ));
        r.uplink.pump(200_000);

        let texts = r.wire.0.lock().unwrap().sent_text.clone();
        assert!(
            texts.iter().any(|t| t.contains("\"t\":\"call.start\"") && t.contains(CALL)),
            "the original call.start is re-sent verbatim so the server replies with resume"
        );

        // The `resume` arrives during this pass, so the pass that follows it is the
        // one that honours it.
        r.wire.0.lock().unwrap().sent_binary.clear();
        r.uplink.pump(200_100);
        let seqs: Vec<u32> = sent_records(&r.wire).iter().map(|x| x.seq).collect();
        assert_eq!(seqs, vec![3, 4, 5], "resume from acked + 1, using the server's higher mark");
    }

    #[test]
    fn a_fatal_call_error_seals_the_call_and_reports_the_loss() {
        let mut r = rig();
        r.uplink.begin_call(CALL, &call_start(), 0).unwrap();
        for seq in 0..4 {
            r.uplink.push_segment(&segment(Channel::Far, seq)).unwrap();
        }
        r.uplink.pump(0);
        r.wire.0.lock().unwrap().inbox.push_back(Incoming::Text(
            serde_json::json!({
                "t":"call.error","call_id":CALL,"code":"tenant_mismatch",
                "message":"certificate tenant does not match token","fatal":true
            })
            .to_string(),
        ));
        r.uplink.pump(10);

        assert_eq!(r.uplink.stats().segments, 0, "audio that can never be accepted is dropped");
        let events = r.uplink.take_events();
        let eviction = events
            .iter()
            .find(|e| e.kind == EventKind::SpoolEviction)
            .expect("the loss is reported, not swallowed");
        assert_eq!(eviction.count, Some(4));
        assert!(r.uplink.spool.is_sealed(CALL).unwrap());
    }

    #[test]
    fn a_non_fatal_call_error_keeps_the_audio() {
        let mut r = rig();
        r.uplink.begin_call(CALL, &call_start(), 0).unwrap();
        r.uplink.push_segment(&segment(Channel::Far, 0)).unwrap();
        r.uplink.pump(0);
        r.wire.0.lock().unwrap().inbox.push_back(Incoming::Text(
            serde_json::json!({"t":"call.error","call_id":CALL,"code":"internal","message":"x","fatal":false})
                .to_string(),
        ));
        r.uplink.pump(10);
        assert_eq!(r.uplink.stats().segments, 1);
    }

    #[test]
    fn a_4403_close_reports_revocation_so_capture_can_stop() {
        let mut r = rig();
        r.uplink.begin_call(CALL, &call_start(), 0).unwrap();
        r.uplink.pump(0);
        r.wire
            .0
            .lock()
            .unwrap()
            .inbox
            .push_back(Incoming::Closed { code: Some(4403) });
        let out = r.uplink.pump(10);
        assert!(out.device_revoked);
        assert!(!out.connected);
    }

    #[test]
    fn a_4401_close_asks_for_a_token_refresh_rather_than_a_stop() {
        let mut r = rig();
        r.uplink.pump(0);
        r.wire
            .0
            .lock()
            .unwrap()
            .inbox
            .push_back(Incoming::Closed { code: Some(4401) });
        let out = r.uplink.pump(10);
        assert!(out.token_rejected);
        assert!(!out.device_revoked, "an expired token is not a revoked device");
    }

    #[test]
    fn a_failed_connect_backs_off_instead_of_spinning() {
        let mut r = rig();
        *r.fail_next.lock().unwrap() = 3;
        let mut attempts = 0;
        let mut now = 0u64;
        for _ in 0..200 {
            let out = r.uplink.pump(now);
            if out.connected {
                break;
            }
            attempts += 1;
            now += 100;
        }
        assert!(r.uplink.is_connected());
        assert!(
            attempts > 3,
            "the backoff must delay retries; a spin would connect on the fourth pass"
        );
        assert_eq!(*r.connects.lock().unwrap(), 1);
    }

    #[test]
    fn segments_are_batched_ten_to_a_message() {
        let mut r = rig();
        r.uplink.begin_call(CALL, &call_start(), 0).unwrap();
        for seq in 0..25 {
            r.uplink.push_segment(&segment(Channel::Far, seq)).unwrap();
        }
        r.uplink.pump(0);
        let sizes: Vec<usize> = r
            .wire
            .0
            .lock()
            .unwrap()
            .sent_binary
            .iter()
            .map(|m| decode_records(m).unwrap().len())
            .collect();
        assert_eq!(sizes, vec![10, 10, 5], "10 segments per message, remainder last");
    }

    #[test]
    fn both_channels_upload_and_are_acked_independently() {
        let mut r = rig();
        r.uplink.begin_call(CALL, &call_start(), 0).unwrap();
        for seq in 0..3 {
            r.uplink.push_segment(&segment(Channel::Far, seq)).unwrap();
            r.uplink.push_segment(&segment(Channel::Near, seq)).unwrap();
        }
        r.uplink.pump(0);
        assert_eq!(sent_records(&r.wire).len(), 6);

        r.wire.0.lock().unwrap().inbox.push_back(Incoming::Text(
            serde_json::json!({"t":"ack","call_id":CALL,"channel":1,"through_seq":2}).to_string(),
        ));
        r.uplink.pump(10);
        assert_eq!(r.uplink.stats().segments, 3, "only the near channel was acked");
    }

    #[test]
    fn a_heartbeat_ack_with_a_new_policy_version_is_surfaced() {
        let mut r = rig();
        r.uplink.pump(0);
        r.wire.0.lock().unwrap().inbox.push_back(Incoming::Text(
            serde_json::json!({"t":"heartbeat.ack","server_time":"2026-09-01T10:00:00Z","policy_version":7})
                .to_string(),
        ));
        let out = r.uplink.pump(10);
        assert_eq!(out.policy_version, Some(7));
        assert!(out.verified);

        // The same version again is not a change and must not trigger a re-fetch.
        r.wire.0.lock().unwrap().inbox.push_back(Incoming::Text(
            serde_json::json!({"t":"heartbeat.ack","server_time":"2026-09-01T10:00:30Z","policy_version":7})
                .to_string(),
        ));
        assert_eq!(r.uplink.pump(20).policy_version, None);
    }

    #[test]
    fn an_undecodable_control_frame_does_not_kill_the_connection() {
        let mut r = rig();
        r.uplink.pump(0);
        r.wire
            .0
            .lock()
            .unwrap()
            .inbox
            .push_back(Incoming::Text("{not json".into()));
        assert!(r.uplink.pump(10).connected);
    }

    #[test]
    fn a_call_recovered_from_the_spool_at_startup_is_uploaded() {
        // The agent was killed mid-call; the service relaunched it. Nothing in memory
        // knows about the call, so the spool has to be the source of truth.
        let handle = FakeHandle::default();
        let mut spool = Spool::open_in_memory(SpoolLimits::default()).unwrap();
        spool.begin_call(CALL, &call_start().to_json().unwrap(), 0).unwrap();
        for seq in 0..2 {
            spool.push(&segment(Channel::Far, seq)).unwrap();
        }
        let factory = FakeFactory {
            handle: handle.clone(),
            fail_next: Arc::new(Mutex::new(0)),
            connects: Arc::new(Mutex::new(0)),
        };
        let mut uplink = Uplink::new(spool, Box::new(factory));
        let out = uplink.pump(0);
        assert_eq!(out.sent, 2);
        assert!(handle.0.lock().unwrap().sent_text[0].contains("call.start"));
    }
}
