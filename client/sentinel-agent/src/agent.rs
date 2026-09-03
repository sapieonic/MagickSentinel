//! The agent's orchestration loop.
//!
//! Capture, detection, encoding, the uplink, the heartbeat and the widget all advance
//! on one tick rather than in separate threads talking to each other. That is a
//! deliberate choice about ordering, not about performance: there is a single sequence
//! of events to reason about, so audio cannot be spooled after the token it would be
//! attributed with has been cleared, and a `4403` cannot land between the detector
//! deciding to record and the recording starting.
//!
//! Everything the loop depends on is injected — the capture source, the API, the
//! widget shell, the clock — so the whole orchestration runs in CI against
//! `WavReplaySource` and a fake gateway.

use crate::api::{ApiError, SentinelApi};
use crate::auth::{AuthService, SignOutStep, SpoolFlush};
use crate::heartbeat::{self, HeartbeatInputs};
use crate::identity::{IdentityGate, IdentityStatus};
use crate::pipeline::Pipeline;
use crate::uplink::Uplink;
use crate::widget::{HostCall, WidgetShell, WidgetState};
use sentinel_capture::source::CaptureSource;
use sentinel_core::config::Policy;
use sentinel_core::events::ClientEvent;
use sentinel_core::protocol::CaptureTier;
use sentinel_core::state::{BlockReason, CallState};
use sentinel_service::telemetry;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long sign-out waits for the spool to drain before giving up and clearing the
/// token anyway. Long enough for a shift's backlog on a slow link; short enough that
/// an agent leaving at the end of a shift is not held at a spinner.
pub const SIGNOUT_FLUSH_DEADLINE: Duration = Duration::from_secs(30);

/// Static facts about this installation.
#[derive(Debug, Clone)]
pub struct AgentIdentityInfo {
    pub device_id: String,
    pub os_build: String,
    pub tier: Option<CaptureTier>,
    pub agent_restarts: u32,
}

/// Lets sign-out drain the uplink without owning it.
pub struct UplinkFlush {
    uplink: Arc<Mutex<Uplink>>,
    clock_ms: Arc<Mutex<u64>>,
}

impl SpoolFlush for UplinkFlush {
    fn flush(&self, deadline: Duration) -> anyhow::Result<u64> {
        let now = *self.clock_ms.lock().unwrap();
        Ok(self.uplink.lock().unwrap().flush(now, deadline))
    }
}

/// What one tick did, for the caller and for tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TickResult {
    pub segments_spooled: usize,
    pub segments_sent: usize,
    pub segments_acked: usize,
    pub heartbeat_sent: bool,
    pub capture_state: Option<CallState>,
    pub signed_out: bool,
}

pub struct Agent {
    pub uplink: Arc<Mutex<Uplink>>,
    auth: Arc<AuthService>,
    api: Arc<dyn SentinelApi>,
    widget: Box<dyn WidgetShell>,
    gate: IdentityGate,
    pipeline: Option<Pipeline>,
    policy: Policy,
    info: AgentIdentityInfo,
    clock_ms: Arc<Mutex<u64>>,
    /// `None` until the first heartbeat has been sent; a plain `0` cannot express
    /// "never sent" when the clock also starts at 0.
    last_heartbeat_ms: Option<u64>,
    signout_deadline: Duration,
    last_call_at: Option<String>,
    dialer_session_active: bool,
    pinned_device_present: bool,
    pending_events: Vec<ClientEvent>,
    /// Set once `4403` or a 403 heartbeat has been seen; terminal until an operator
    /// acts, per wire.md.
    revoked: bool,
    /// Capture state as of the previous tick, so a change can be reported once rather
    /// than every 20 ms.
    last_reported_state: Option<CallState>,
    /// Whether the last tick believed a user was signed in, same reason.
    last_reported_signed_in: Option<bool>,
}

impl Agent {
    pub fn new(
        uplink: Uplink,
        auth: Arc<AuthService>,
        api: Arc<dyn SentinelApi>,
        widget: Box<dyn WidgetShell>,
        policy: Policy,
        info: AgentIdentityInfo,
    ) -> Self {
        let mut gate = IdentityGate::new(policy.offline_grace_ms());
        gate.set_signed_in(auth.state().is_signed_in());
        Agent {
            uplink: Arc::new(Mutex::new(uplink)),
            auth,
            api,
            widget,
            gate,
            pipeline: None,
            policy,
            info,
            clock_ms: Arc::new(Mutex::new(0)),
            last_heartbeat_ms: None,
            signout_deadline: SIGNOUT_FLUSH_DEADLINE,
            last_call_at: None,
            dialer_session_active: false,
            pinned_device_present: false,
            pending_events: Vec::new(),
            revoked: false,
            last_reported_state: None,
            last_reported_signed_in: None,
        }
    }

    /// Shorten the sign-out flush deadline. Only tests do this; a real sign-out gives
    /// the spool the full window.
    pub fn with_signout_deadline(mut self, d: Duration) -> Self {
        self.signout_deadline = d;
        self
    }

    /// Attach a client event to the next heartbeat.
    ///
    /// The way anything outside the capture pipeline gets a machine-state fact in front
    /// of an operator. `main` uses it for the two start-up failures that block capture
    /// without the pipeline ever existing to report them — no spool key, and no device
    /// credential — because a machine that silently never records is the product's
    /// worst failure mode and the heartbeat is where it has to show up.
    pub fn record_event(&mut self, event: ClientEvent) {
        self.pending_events.push(event);
    }

    pub fn identity_status(&self, now_ms: u64) -> IdentityStatus {
        self.gate.status(now_ms)
    }

    pub fn capture_state(&self) -> CallState {
        self.pipeline.as_ref().map_or(CallState::Blocked, |p| p.state())
    }

    /// Open the capture pipeline against the pinned endpoint.
    ///
    /// Deliberately separate from `new`: capture only starts once there is a verified
    /// identity to attribute it to, and the caller is what knows that.
    pub fn open_capture(
        &mut self,
        source: &mut dyn CaptureSource,
        tier: CaptureTier,
        user_uid: &str,
    ) -> anyhow::Result<()> {
        let mut pipeline = Pipeline::new(
            &self.policy,
            tier,
            self.info.device_id.clone(),
            user_uid.to_string(),
        )?;
        match pipeline.open(source, &self.policy) {
            Ok(()) => {
                self.pinned_device_present = true;
                self.pipeline = Some(pipeline);
                Ok(())
            }
            Err(e) => {
                // "Headset not detected" in the widget, and the state reaches the
                // heartbeat so a supervisor sees it before the agent complains.
                self.pinned_device_present = false;
                self.pending_events.extend(pipeline.take_events());
                self.pipeline = None;
                Err(e)
            }
        }
    }

    /// The softphone's audio session changed state — the primary detection signal.
    pub fn on_session_state(&mut self, active: bool) {
        self.dialer_session_active = active;
        if let Some(p) = self.pipeline.as_mut() {
            p.on_session_state(active);
        }
    }

    /// Advance everything by one tick.
    pub fn tick(
        &mut self,
        now_ms: u64,
        source: Option<&mut (dyn CaptureSource + '_)>,
    ) -> anyhow::Result<TickResult> {
        *self.clock_ms.lock().unwrap() = now_ms;
        let mut result = TickResult::default();

        // 1. Identity first. Capture must never run ahead of the decision about
        //    whether it is allowed to.
        let status = self.gate.status(now_ms);
        if let Some(p) = self.pipeline.as_mut() {
            match status.block_reason() {
                Some(reason) if p.state() != CallState::Blocked => {
                    p.block(reason);
                }
                None if p.state() == CallState::Blocked => {
                    p.unblock();
                }
                _ => {}
            }
        }

        // 2. Capture, but only while the identity gate allows it.
        if status.may_capture() {
            if let (Some(pipeline), Some(source)) = (self.pipeline.as_mut(), source) {
                let mut uplink = self.uplink.lock().unwrap();
                let step = pipeline.step(source, &mut uplink)?;
                result.segments_spooled = step.segments_spooled;
                if let Some(sentinel_core::state::Transition::CallEnded(_)) = step.transition {
                    self.last_call_at =
                        Some(heartbeat::rfc3339_millis(time::OffsetDateTime::now_utc()));
                }
            }
        }
        if let Some(p) = self.pipeline.as_mut() {
            self.pending_events.extend(p.take_events());
        }
        result.capture_state = Some(self.capture_state());
        self.report_capture_state(now_ms, status);
        self.report_sign_in_state();

        // 3. Uplink. Not while revoked: wire.md makes `4403` terminal until an
        //    operator acts, and a client that kept reconnecting would spend a revoked
        //    floor's uplink on handshakes the gateway is going to refuse. The spool is
        //    left alone — the audio stays, and it uploads if the device is reinstated.
        if !self.revoked {
            let mut uplink = self.uplink.lock().unwrap();
            let outcome = uplink.pump(now_ms);
            result.segments_sent = outcome.sent;
            result.segments_acked = outcome.acked;
            if outcome.verified {
                self.gate.observe_verified(now_ms);
            }
            if outcome.device_revoked {
                self.revoked = true;
            }
            self.pending_events.extend(uplink.take_events());
        }
        if self.revoked {
            self.gate.set_device_revoked(true);
        }

        // 4. Heartbeat.
        // `map_or(true, ..)` rather than `is_none_or`: the workspace MSRV is 1.78 and
        // `Option::is_none_or` only stabilised in 1.82.
        let heartbeat_due = self
            .last_heartbeat_ms
            .map_or(true, |last| now_ms.saturating_sub(last) >= heartbeat::INTERVAL_MS);
        if heartbeat_due {
            self.last_heartbeat_ms = Some(now_ms);
            result.heartbeat_sent = self.send_heartbeat(now_ms);
        }

        // 5. The widget, last, so it shows the state this tick actually produced.
        let state = self.widget_state(now_ms);
        self.widget.post_state(&state)?;
        for call in self.widget.drain_host_calls() {
            if matches!(call, HostCall::SignOut) {
                self.sign_out()?;
                result.signed_out = true;
            }
        }
        Ok(result)
    }

    fn send_heartbeat(&mut self, now_ms: u64) -> bool {
        let Some(token) = self.auth.state().id_token().map(str::to_string) else {
            // Nothing to authenticate with. The server learns the device is alive from
            // the next signed-in heartbeat; sending an unauthenticated one would only
            // be rejected.
            return false;
        };
        let stats = self.uplink.lock().unwrap().stats();
        let inputs = HeartbeatInputs {
            device_id: self.info.device_id.clone(),
            capture_state: self.capture_state().as_str().to_string(),
            capture_tier: self.info.tier.map(tier_name),
            os_build: self.info.os_build.clone(),
            agent_version: crate::VERSION.to_string(),
            spool_depth: stats.segments,
            spool_bytes: stats.bytes,
            last_call_at: self.last_call_at.clone(),
            dialer_session_active: self.dialer_session_active,
            signed_in: true,
            agent_restarts: self.info.agent_restarts,
            pinned_device_present: self.pinned_device_present,
        };
        let events = std::mem::take(&mut self.pending_events);
        // Spool depth is sampled here rather than on every tick: it changes constantly
        // during a call and what matters is the trend, which the heartbeat cadence
        // already captures. Eviction, which is data loss, is reported the moment it
        // happens instead.
        tracing::info!(
            target: telemetry::TARGET,
            event = telemetry::event::SPOOL_DEPTH,
            spool_depth = stats.segments,
            spool_bytes = stats.bytes,
            capture_state = %inputs.capture_state,
            "spool depth sampled"
        );
        for ev in &events {
            report_client_event(ev);
        }
        let body = heartbeat::build(&inputs, &events, now_ms, time::OffsetDateTime::now_utc());
        match self.api.heartbeat(&token, &body) {
            Ok(_) => {
                self.gate.observe_verified(now_ms);
                true
            }
            Err(ApiError::Forbidden) => {
                // Revoked. Terminal until an operator acts.
                if !self.revoked {
                    tracing::error!(
                        target: telemetry::TARGET,
                        event = telemetry::event::DEVICE_REVOKED,
                        source = "heartbeat_403",
                        "the gateway refused this device; capture stops and the uplink \
                         stops reconnecting until an operator reinstates it"
                    );
                }
                self.revoked = true;
                self.gate.set_device_revoked(true);
                false
            }
            Err(e) => {
                // The events were taken; put them back so they are not lost to a
                // transient network failure. A `spool_eviction` that never reaches the
                // server is silent data loss by another route.
                self.pending_events = events;
                tracing::warn!(error = %e, "heartbeat failed");
                false
            }
        }
    }

    /// Sign out in the order that does not orphan audio (spec 7.4).
    pub fn sign_out(&mut self) -> anyhow::Result<()> {
        let flush = UplinkFlush {
            uplink: self.uplink.clone(),
            clock_ms: self.clock_ms.clone(),
        };
        // The closure is the "stop capture" step: nothing new may enter the spool
        // while it drains, or the flush chases its own tail.
        let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop_flag.clone();
        self.auth
            .sign_out(&move || flag.store(true, std::sync::atomic::Ordering::SeqCst), &flush, self.signout_deadline)?;
        if stop_flag.load(std::sync::atomic::Ordering::SeqCst) {
            if let Some(p) = self.pipeline.as_mut() {
                p.block(BlockReason::SignedOut);
            }
        }
        self.gate.set_signed_in(false);
        Ok(())
    }

    /// Record a completed sign-in.
    pub fn on_signed_in(&mut self, user_uid: &str) {
        self.gate.set_signed_in(true);
        if let Some(p) = self.pipeline.as_mut() {
            // Never split attribution within a record: a user change closes the open
            // call and opens a new one.
            p.on_user_changed(user_uid);
        }
    }

    /// The steps the last sign-out took, in order.
    pub fn last_signout_steps(&self) -> Vec<SignOutStep> {
        self.auth.last_signout_steps()
    }

    /// Report a capture state change once, with the reason when it is a block.
    ///
    /// This is the instrumentation that matters most on an endpoint. The product's
    /// dangerous failure is not a crash — a crash is visible — it is a machine that
    /// reports itself healthy, has a signed-in user and a live dialer session, and
    /// records nothing. The server-side alert for that needs the client to keep saying
    /// what it is doing and, when it is not recording, **why**.
    fn report_capture_state(&mut self, now_ms: u64, status: IdentityStatus) {
        let state = self.capture_state();
        if self.last_reported_state == Some(state) {
            return;
        }
        let previous = self.last_reported_state;
        self.last_reported_state = Some(state);

        let armed = matches!(state, CallState::Armed | CallState::InCall | CallState::Wrap);
        let was_armed = matches!(
            previous,
            Some(CallState::Armed | CallState::InCall | CallState::Wrap)
        );

        let event = if state == CallState::Blocked {
            telemetry::event::CAPTURE_BLOCKED
        } else if armed && !was_armed {
            telemetry::event::CAPTURE_ARMED
        } else if was_armed && !armed {
            telemetry::event::CAPTURE_DISARMED
        } else {
            // Idle to Finalize and back: real, uninteresting, and not worth a record
            // per call transition on 200 desktops.
            return;
        };

        // `reason` is the whole point of the blocked case. `IdentityStatus` knows about
        // sign-out, revocation and the grace clock; the pinned device is the one cause
        // it cannot see, so it is checked separately.
        let reason = if state == CallState::Blocked {
            Some(match status.block_reason() {
                Some(BlockReason::SignedOut) => "signed_out",
                Some(BlockReason::PinnedDeviceMissing) => "pinned_device_missing",
                Some(BlockReason::OfflineGraceExpired) => "offline_grace_expired",
                Some(BlockReason::DeviceRevoked) => "device_revoked",
                Some(BlockReason::Shutdown) => "shutdown",
                None if !self.pinned_device_present => "pinned_device_missing",
                None if self.pipeline.is_none() => "no_capture_pipeline",
                None => "unknown",
            })
        } else {
            None
        };

        tracing::info!(
            target: telemetry::TARGET,
            event,
            capture_state = state.as_str(),
            previous_state = previous.map(CallState::as_str).unwrap_or("none"),
            reason = reason.unwrap_or(""),
            dialer_session_active = self.dialer_session_active,
            pinned_device_present = self.pinned_device_present,
            signed_in = self.auth.state().is_signed_in(),
            capture_tier = self.info.tier.map(tier_name).unwrap_or_else(|| "none".into()),
            uptime_ms = now_ms,
            "capture state changed"
        );
    }

    /// Report a change in sign-in state once.
    ///
    /// Read from `AuthService` on the tick rather than emitted from `on_signed_in` and
    /// `sign_out`, because those are not the only ways the state changes: a refresh
    /// that fails past the token's lifetime signs the session out from inside the auth
    /// service, and that is precisely the case an operator needs to see — an agent
    /// sitting at a locked widget for a whole shift because the IdP stopped answering.
    fn report_sign_in_state(&mut self) {
        let signed_in = self.auth.state().is_signed_in();
        if self.last_reported_signed_in == Some(signed_in) {
            return;
        }
        self.last_reported_signed_in = Some(signed_in);
        // No UID. The server already knows who signed in, from the bearer token it
        // verified; putting one in a telemetry backend would be gratuitous as well as
        // forbidden (spec 12.10).
        tracing::info!(
            target: telemetry::TARGET,
            event = telemetry::event::SIGN_IN_STATE,
            signed_in,
            "sign-in state changed"
        );
    }

    fn widget_state(&self, now_ms: u64) -> WidgetState {
        let status = self.gate.status(now_ms);
        let capture_state = self.capture_state();
        WidgetState {
            auth_state: if self.auth.state().is_signed_in() {
                "signed_in".into()
            } else {
                "signed_out".into()
            },
            capture_state: capture_state.as_str().into(),
            tier: self.info.tier.map(tier_name),
            coverage: None,
            call_id: self
                .pipeline
                .as_ref()
                .and_then(|p| p.current_call_id().map(str::to_string)),
            message: status.widget_message().map(str::to_string),
            // Non-dismissible while capture is live (spec 12.4).
            recording: matches!(capture_state, CallState::InCall | CallState::Wrap),
            spool_depth: self.uplink.lock().unwrap().stats().segments,
        }
    }
}

/// Mirror a client event into telemetry.
///
/// The same events the heartbeat carries, so a floor that has telemetry on and a floor
/// that does not both surface them — the heartbeat stays the authoritative signal and
/// this is the one an operator can graph. Eviction is separated out because it is data
/// loss and deserves its own name and its own severity.
fn report_client_event(ev: &ClientEvent) {
    use sentinel_core::events::EventKind;
    match ev.kind {
        EventKind::SpoolEviction => tracing::error!(
            target: telemetry::TARGET,
            event = telemetry::event::SPOOL_EVICTED,
            // The count is the number of segments of borrower audio that will never
            // reach the server. It is the reason this event exists.
            evicted = ev.count.unwrap_or(0),
            "the spool evicted audio that was never acknowledged"
        ),
        other => tracing::warn!(
            target: telemetry::TARGET,
            event = "client.event",
            // `detail` is machine state by construction — `sentinel_core::events`
            // states the rule and the enum shape enforces it.
            kind = ?other,
            count = ev.count.unwrap_or(0),
            detail = ev.detail.as_deref().unwrap_or(""),
            "client event"
        ),
    }
}

/// The tier as it appears in the heartbeat and the widget: `"A"` or `"B"`, matching
/// the `CaptureTier` enum in `contracts/openapi.yaml`.
fn tier_name(t: CaptureTier) -> String {
    match t {
        CaptureTier::A => "A".into(),
        CaptureTier::B => "B".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{HeartbeatAck, SessionResponse};
    use crate::auth::browser::RecordingLauncher;
    use crate::auth::pkce::OidcConfig;
    use crate::auth::store::MemoryTokenStore;
    use crate::auth::{AuthError, TokenEndpoint, TokenSet, TokenStore};
    use crate::heartbeat::Heartbeat;
    use crate::uplink::{Transport, TransportError, TransportFactory};
    use crate::widget::HeadlessWidget;
    use sentinel_core::config::{PinnedDevice, SpoolLimits};
    use sentinel_core::spool::{SegmentRow, Spool};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct FakeApi {
        heartbeats: Mutex<Vec<Heartbeat>>,
        forbid: AtomicBool,
        failures: AtomicUsize,
    }

    impl Default for FakeApi {
        fn default() -> Self {
            FakeApi {
                heartbeats: Mutex::new(Vec::new()),
                forbid: AtomicBool::new(false),
                failures: AtomicUsize::new(0),
            }
        }
    }

    impl SentinelApi for FakeApi {
        fn open_session(&self, _t: &str) -> Result<SessionResponse, ApiError> {
            Err(ApiError::Transport("not used".into()))
        }
        fn get_policy(&self, _t: &str) -> Result<Policy, ApiError> {
            Ok(Policy::default())
        }
        fn heartbeat(&self, _t: &str, body: &Heartbeat) -> Result<HeartbeatAck, ApiError> {
            if self.forbid.load(Ordering::SeqCst) {
                return Err(ApiError::Forbidden);
            }
            if self.failures.load(Ordering::SeqCst) > 0 {
                self.failures.fetch_sub(1, Ordering::SeqCst);
                return Err(ApiError::Transport("offline".into()));
            }
            self.heartbeats.lock().unwrap().push(body.clone());
            Ok(HeartbeatAck {
                policy_version: 1,
                server_time: "2026-09-01T10:00:00.000Z".into(),
                commands: vec![],
            })
        }
        fn end_session(&self, _t: &str) -> Result<(), ApiError> {
            Ok(())
        }
    }

    struct FakeTokens;
    impl TokenEndpoint for FakeTokens {
        fn exchange_code(
            &self,
            _c: &OidcConfig,
            _code: &str,
            _v: &str,
            _r: &str,
        ) -> Result<TokenSet, AuthError> {
            Ok(TokenSet {
                id_token: "id".into(),
                refresh_token: Some("rt".into()),
                expires_in_s: 3600,
                uid: "uid-agent-a".into(),
            })
        }
        fn refresh(&self, _c: &OidcConfig, _rt: &str) -> Result<TokenSet, AuthError> {
            Ok(TokenSet {
                id_token: "id".into(),
                refresh_token: Some("rt".into()),
                expires_in_s: 3600,
                uid: "uid-agent-a".into(),
            })
        }
    }

    struct Offline;
    impl TransportFactory for Offline {
        fn connect(&mut self) -> Result<Box<dyn Transport>, TransportError> {
            Err(TransportError::Connect("offline".into()))
        }
    }

    fn policy() -> Policy {
        Policy {
            pinned_devices: vec![PinnedDevice {
                container_id: "cont-headset".into(),
                friendly_name: None,
            }],
            offline_grace_hours: 8,
            ..Policy::default()
        }
    }

    fn agent(api: Arc<FakeApi>) -> (Agent, Arc<AuthService>, Arc<dyn TokenStore>) {
        let store: Arc<dyn TokenStore> = Arc::new(MemoryTokenStore::new());
        store.save_refresh_token("rt").unwrap();
        let auth = Arc::new(AuthService::new(
            OidcConfig {
                authorize_endpoint: "https://idp/auth".into(),
                token_endpoint: "https://idp/token".into(),
                client_id: "sentinel-desktop".into(),
                tenant_id: None,
                scopes: vec![],
            },
            store.clone(),
            Arc::new(FakeTokens),
            Arc::new(RecordingLauncher::default()),
        ));
        auth.restore(0).unwrap();

        let uplink = Uplink::new(
            Spool::open_in_memory(SpoolLimits::default()).unwrap(),
            Box::new(Offline),
        );
        let agent = Agent::new(
            uplink,
            auth.clone(),
            api,
            Box::new(HeadlessWidget::default()),
            policy(),
            AgentIdentityInfo {
                device_id: "1b4e28ba-2fa1-11d2-883f-0016d3cca427".into(),
                os_build: "10.0.22631".into(),
                tier: Some(CaptureTier::A),
                agent_restarts: 2,
            },
        )
        .with_signout_deadline(Duration::from_millis(50));
        (agent, auth, store)
    }

    fn spool_a_segment(agent: &Agent, call_id: &str, seq: u32) {
        let mut u = agent.uplink.lock().unwrap();
        let start = sentinel_core::protocol::ControlMessage::CallStart(
            sentinel_core::protocol::CallStart {
                call_id: call_id.into(),
                started_at: "2026-09-01T10:14:02.113Z".into(),
                user_uid: "uid-agent-a".into(),
                device_id: "dev".into(),
                tier: CaptureTier::A,
                account_ref: None,
                dialer_call_id: None,
                direction: sentinel_core::protocol::Direction::Outbound,
                codec: "opus".into(),
                rate: 16_000,
            },
        );
        u.begin_call(call_id, &start, 0).unwrap();
        u.push_segment(&SegmentRow {
            call_id: call_id.into(),
            channel: sentinel_core::protocol::Channel::Far,
            seq,
            timestamp_ms: seq as u64 * 1000,
            flags: Default::default(),
            payload: vec![7; 100],
            created_ms: 0,
        })
        .unwrap();
    }

    #[test]
    fn the_first_tick_sends_a_heartbeat_matching_the_contract() {
        let api = Arc::new(FakeApi::default());
        let (mut a, _auth, _store) = agent(api.clone());
        let r = a.tick(0, None).unwrap();
        assert!(r.heartbeat_sent);

        let hbs = api.heartbeats.lock().unwrap();
        assert_eq!(hbs.len(), 1);
        let v = serde_json::to_value(&hbs[0]).unwrap();
        assert_eq!(v["capture_tier"], "A");
        assert_eq!(v["os_build"], "10.0.22631");
        assert_eq!(v["agent_restarts"], 2);
        assert_eq!(v["signed_in"], true);
        assert_eq!(v["capture_state"], "BLOCKED", "no capture opened yet");
    }

    #[test]
    fn heartbeats_are_thirty_seconds_apart() {
        let api = Arc::new(FakeApi::default());
        let (mut a, _auth, _store) = agent(api.clone());
        a.tick(0, None).unwrap();
        for t in [1_000u64, 10_000, 29_999] {
            assert!(!a.tick(t, None).unwrap().heartbeat_sent, "too early at {t} ms");
        }
        assert!(a.tick(30_000, None).unwrap().heartbeat_sent);
        assert_eq!(api.heartbeats.lock().unwrap().len(), 2);
    }

    #[test]
    fn events_survive_a_failed_heartbeat_rather_than_being_dropped() {
        // A `spool_eviction` that never reaches the server is silent data loss by
        // another route.
        let api = Arc::new(FakeApi::default());
        let (mut a, _auth, _store) = agent(api.clone());
        a.pending_events.push(ClientEvent::new(
            sentinel_core::events::EventKind::SpoolEviction,
            0,
        ).with_count(11));
        api.failures.store(1, Ordering::SeqCst);

        assert!(!a.tick(0, None).unwrap().heartbeat_sent);
        assert!(a.tick(30_000, None).unwrap().heartbeat_sent);

        let hbs = api.heartbeats.lock().unwrap();
        assert_eq!(hbs.len(), 1);
        assert_eq!(hbs[0].events.len(), 1, "the event was retried, not lost");
        assert_eq!(hbs[0].events[0].kind, "spool_eviction");
        assert_eq!(hbs[0].events[0].count, Some(11));
    }

    #[test]
    fn a_403_heartbeat_revokes_the_device_and_stops_capture() {
        // Spec 7.2: revoking a device MUST terminate its capture within 60 s. The
        // heartbeat is every 30 s, so this is the path that guarantees the bound.
        let api = Arc::new(FakeApi::default());
        let (mut a, _auth, _store) = agent(api.clone());
        a.gate.set_pinned_device_present(true);
        a.gate.observe_verified(0);
        assert!(a.identity_status(0).may_capture());

        api.forbid.store(true, Ordering::SeqCst);
        a.tick(30_000, None).unwrap();
        assert!(!a.identity_status(30_000).may_capture());
        assert_eq!(
            a.identity_status(30_000),
            IdentityStatus::Blocked(BlockReason::DeviceRevoked)
        );
    }

    #[test]
    fn capture_stops_when_the_offline_grace_window_expires() {
        // Spec 7.5. Never silently record with no verifiable identity attached.
        let api = Arc::new(FakeApi::default());
        api.failures.store(1000, Ordering::SeqCst); // offline for the whole test
        let (mut a, _auth, _store) = agent(api);
        a.gate.set_pinned_device_present(true);
        a.gate.observe_verified(0);

        let grace = policy().offline_grace_ms();
        a.tick(grace - 1, None).unwrap();
        assert!(a.identity_status(grace - 1).may_capture(), "inside grace, capture continues");

        a.tick(grace, None).unwrap();
        let status = a.identity_status(grace);
        assert_eq!(status, IdentityStatus::Expired);
        assert!(!status.may_capture());
        assert_eq!(status.widget_message(), Some("Reconnect to continue recording"));
    }

    #[test]
    fn sign_out_flushes_the_spool_before_clearing_the_token() {
        // Spec 7.4, through the real Uplink rather than a stand-in: the spooled audio
        // uploads on a socket authenticated with this user's bearer token.
        let api = Arc::new(FakeApi::default());
        let (mut a, _auth, store) = agent(api);
        spool_a_segment(&a, "01J8ZQ8H2Q7X9K3M4N5P6R7S8T", 0);
        assert_eq!(a.uplink.lock().unwrap().stats().segments, 1);
        assert!(store.load_refresh_token().unwrap().is_some());

        a.sign_out().unwrap();

        assert_eq!(
            a.last_signout_steps(),
            vec![
                SignOutStep::StopCapture,
                SignOutStep::FlushSpool,
                SignOutStep::ClearTokens,
                SignOutStep::EndServerSession
            ]
        );
        assert!(store.load_refresh_token().unwrap().is_none());
        // The uplink could not connect, so the audio is still on disk — which is the
        // correct outcome. It uploads at the next sign-in; what must not happen is the
        // token being cleared before the attempt.
        assert_eq!(a.uplink.lock().unwrap().stats().segments, 1);
        assert!(!a.identity_status(0).may_capture());
    }

    #[test]
    fn a_sign_out_from_the_widget_is_honoured() {
        let api = Arc::new(FakeApi::default());
        let (mut a, _auth, store) = agent(api);
        // The widget is headless here; push the host call the bundle would send.
        let mut headless = HeadlessWidget::default();
        headless.pending_calls.push(HostCall::SignOut);
        a.widget = Box::new(headless);

        let r = a.tick(0, None).unwrap();
        assert!(r.signed_out);
        assert!(store.load_refresh_token().unwrap().is_none());
    }
}
