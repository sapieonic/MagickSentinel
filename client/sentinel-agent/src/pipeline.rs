//! The capture pipeline: audio in, spooled segments out (spec 6.3–6.5).
//!
//! Two channels, kept separate end to end. Channel 0 (`far`) is render loopback
//! carrying the borrower; channel 1 (`near`) is microphone capture carrying the
//! agent. Each has its own VAD, its own Opus encoder and its own sequence space; they
//! share exactly one thing, the call-scoped clock, because channel drift breaks
//! transcript alignment.
//!
//! The pipeline is driven by a [`CaptureSource`], which is why the whole of it —
//! detection, encoding, spooling, and the handoff to the uplink — runs in CI against
//! `WavReplaySource` with no sound card.

use crate::encode::{SegmentEncoder, SEGMENT_MS};
use crate::uplink::Uplink;
use sentinel_capture::device::{resolve_pinned, AudioDevice, Direction};
use sentinel_capture::foreign::{Decision, ForeignAudioSuppressor};
use sentinel_capture::source::{CaptureSource, StreamHandle};
use sentinel_capture::{Vad, FRAME_MS, FRAME_SAMPLES, SAMPLE_RATE};
use sentinel_core::config::Policy;
use sentinel_core::events::{ClientEvent, EventKind};
use sentinel_core::protocol::{
    CallEnd, CallStart, CaptureTier, Channel, ControlMessage, Direction as CallDirection, EndReason,
};
use sentinel_core::spool::SegmentRow;
use sentinel_core::state::{BlockReason, CallState, Detector, DetectorInput, Transition};
use std::collections::BTreeMap;

/// Segments of pre-roll kept per channel while `ARMED`, matching the 60 s RAM ring
/// buffer the spec calls for. One segment is one second.
pub const PREROLL_MAX_SEGMENTS: usize = 60;

/// Samples read from a source in one go. 20 ms at 16 kHz is one Opus frame; asking
/// for a whole segment's worth at a time would add up to a second of latency to the
/// VAD, and the detector's 300 ms confirmation threshold would then be meaningless.
pub const READ_SAMPLES: usize = FRAME_SAMPLES;

/// One channel's audio path.
struct ChannelPipe {
    channel: Channel,
    stream: StreamHandle,
    vad: Vad,
    encoder: SegmentEncoder,
    /// Samples consumed on this channel, the basis for its own gap accounting.
    samples_seen: u64,
    buffer: Vec<i16>,
    /// Segments the encoder has finished but that have not been spooled yet.
    ///
    /// While `ARMED` this is the pre-roll: the seconds before the far-channel VAD
    /// confirmed a human. Keeping it is what stops every call losing its opening
    /// words — the borrower's "hello" is the 300 ms that confirmed the call.
    /// `ArmDiscarded` throws it away; `CallStarted` spools it.
    ready: Vec<crate::encode::Segment>,
}

impl ChannelPipe {
    /// Start a fresh sequence space. Sequence numbers are per `(call_id, channel)` and
    /// start at 0, so the encoder is rebuilt whenever a new call's audio begins.
    fn restart(&mut self) -> anyhow::Result<()> {
        self.encoder = SegmentEncoder::new(self.channel)?;
        self.ready.clear();
        Ok(())
    }
}

/// What one `step` produced, for the caller to act on.
#[derive(Debug, Default, PartialEq)]
pub struct StepResult {
    pub transition: Option<Transition>,
    pub segments_spooled: usize,
    pub call_id: Option<String>,
}

/// The running capture pipeline for one signed-in user.
pub struct Pipeline {
    detector: Detector,
    far: Option<ChannelPipe>,
    near: Option<ChannelPipe>,
    suppressor: ForeignAudioSuppressor,
    tier: CaptureTier,
    device_id: String,
    user_uid: String,
    /// The call-scoped clock, shared by both channels. Advanced by the far channel's
    /// sample count so a stall on one channel cannot drag the other's timestamps.
    clock_ms: u64,
    current_call: Option<CurrentCall>,
    events: Vec<ClientEvent>,
    /// Highest sequence produced per channel in the current call, for `call.end`.
    last_seq: BTreeMap<Channel, u32>,
    account_ref: Option<String>,
}

struct CurrentCall {
    call_id: String,
    started_at: String,
}

impl Pipeline {
    pub fn new(
        policy: &Policy,
        tier: CaptureTier,
        device_id: String,
        user_uid: String,
    ) -> anyhow::Result<Self> {
        let mut detector = Detector::new(policy.vad);
        detector.feed(0, DetectorInput::UserChanged { user_uid: user_uid.clone() });
        Ok(Pipeline {
            detector,
            far: None,
            near: None,
            suppressor: match tier {
                // Tier A captures only the softphone's process tree, so nothing
                // foreign can reach the stream and suppression is a no-op.
                CaptureTier::A => ForeignAudioSuppressor::for_tier_a(),
                CaptureTier::B => ForeignAudioSuppressor::new(Default::default()),
            },
            tier,
            device_id,
            user_uid,
            clock_ms: 0,
            current_call: None,
            events: Vec::new(),
            last_seq: BTreeMap::new(),
            account_ref: None,
        })
    }

    pub fn state(&self) -> CallState {
        self.detector.state()
    }

    pub fn clock_ms(&self) -> u64 {
        self.clock_ms
    }

    pub fn current_call_id(&self) -> Option<&str> {
        self.current_call.as_ref().map(|c| c.call_id.as_str())
    }

    pub fn take_events(&mut self) -> Vec<ClientEvent> {
        std::mem::take(&mut self.events)
    }

    /// Best-effort account reference from the UIA scrape (spec 6.4 signal 3). Null is
    /// normal: the server reconciles against dialer CDR on `(agent_id, started_at)`.
    pub fn set_account_ref(&mut self, account_ref: Option<String>) {
        self.account_ref = account_ref;
    }

    /// Open both channels against the pinned endpoint.
    ///
    /// Never the system default device: on tier B that captures whatever else the
    /// machine is playing, which is the privacy problem device pinning exists to
    /// prevent. If the pinned device is absent, capture does not start and the
    /// detector is told why.
    pub fn open(
        &mut self,
        source: &mut dyn CaptureSource,
        policy: &Policy,
    ) -> anyhow::Result<()> {
        let devices = source.enumerate()?;
        let render = pick(&devices, policy, Direction::Render);
        let capture = pick(&devices, policy, Direction::Capture);

        let (Some(render), Some(capture)) = (render, capture) else {
            self.detector
                .feed(self.clock_ms, DetectorInput::Block(BlockReason::PinnedDeviceMissing));
            self.events
                .push(ClientEvent::new(EventKind::DeviceLost, self.clock_ms));
            anyhow::bail!("the pinned audio endpoint is not present");
        };

        let far = source.open(&render.id, Direction::Render)?;
        let near = source.open(&capture.id, Direction::Capture)?;
        self.far = Some(ChannelPipe {
            channel: Channel::Far,
            stream: far,
            vad: Vad::default(),
            encoder: SegmentEncoder::new(Channel::Far)?,
            samples_seen: 0,
            buffer: vec![0i16; READ_SAMPLES],
            ready: Vec::new(),
        });
        self.near = Some(ChannelPipe {
            channel: Channel::Near,
            stream: near,
            vad: Vad::default(),
            encoder: SegmentEncoder::new(Channel::Near)?,
            samples_seen: 0,
            buffer: vec![0i16; READ_SAMPLES],
            ready: Vec::new(),
        });
        self.detector.feed(self.clock_ms, DetectorInput::Unblock);
        Ok(())
    }

    /// Tell the pipeline the softphone's audio session changed state.
    ///
    /// The primary detection signal (spec 6.4). It also drives foreign-audio
    /// suppression: loopback energy while the session is Inactive is, by definition,
    /// not call audio.
    pub fn on_session_state(&mut self, active: bool) -> Option<Transition> {
        self.suppressor.set_session_active(self.clock_ms, active);
        Some(self.detector.feed(self.clock_ms, DetectorInput::SessionState { active }))
    }

    /// The signed-in user changed. Closes the current record rather than letting one
    /// row carry two identities.
    pub fn on_user_changed(&mut self, uid: &str) -> Option<Transition> {
        self.user_uid = uid.to_string();
        Some(self.detector.feed(self.clock_ms, DetectorInput::UserChanged { user_uid: uid.into() }))
    }

    pub fn block(&mut self, reason: BlockReason) -> Transition {
        self.detector.feed(self.clock_ms, DetectorInput::Block(reason))
    }

    pub fn unblock(&mut self) -> Transition {
        self.detector.feed(self.clock_ms, DetectorInput::Unblock)
    }

    /// Read one slice from each channel, run detection, encode and spool.
    ///
    /// Returns what happened, so the caller can emit `call.start` / `call.end` on the
    /// uplink. Reading both channels every step is what keeps their sample counts —
    /// and therefore their timestamps — in step.
    pub fn step(
        &mut self,
        source: &mut dyn CaptureSource,
        uplink: &mut Uplink,
    ) -> anyhow::Result<StepResult> {
        let mut result = StepResult::default();

        let far_read = self.read_channel(source, Channel::Far)?;
        let near_read = self.read_channel(source, Channel::Near)?;
        if far_read == 0 && near_read == 0 {
            return Ok(result);
        }

        // Advance the shared clock by the far channel's audio. Both channels stamp
        // frames from it: a per-channel clock drifts, and the transcript alignment
        // that separate channels exist to give would drift with it.
        let advance_ms = (far_read.max(near_read) as u64 * 1000) / SAMPLE_RATE as u64;
        self.clock_ms += advance_ms;

        let far_voiced = self.far.as_ref().is_some_and(|p| p.vad.is_voiced());
        let near_voiced = self.near.as_ref().is_some_and(|p| p.vad.is_voiced());

        // Tier B: energy on the loopback channel while the softphone session is
        // Inactive is Spotify, Teams or a notification — stored so we can prove what
        // was discarded, never transcribed.
        let decision = self.suppressor.judge(self.clock_ms, far_voiced, advance_ms);
        if decision == Decision::MarkForeign {
            if let Some(p) = self.far.as_mut() {
                p.encoder.mark_foreign();
            }
        }
        if let Some(ev) = self.suppressor.take_event(self.clock_ms) {
            self.events.push(ev);
        }

        // Foreign audio must not confirm a call: the whole point of suppression is
        // that this is not the borrower speaking.
        let far_for_detector = far_voiced && decision != Decision::MarkForeign;
        let t1 = self
            .detector
            .feed(self.clock_ms, DetectorInput::FarVoice { voiced: far_for_detector });
        let t2 = self
            .detector
            .feed(self.clock_ms, DetectorInput::NearVoice { voiced: near_voiced });
        let transition = if t1 != Transition::None { t1 } else { t2 };

        self.apply_transition(&transition, uplink, &mut result)?;

        // Spool only while a call is open. Audio outside a call is not evidence of
        // anything and would be unattributable to a `call_id`.
        if self.current_call.is_some() {
            result.segments_spooled += self.drain_encoders(uplink)?;
        }

        self.bound_preroll();

        if transition != Transition::None {
            result.transition = Some(transition);
        }
        result.call_id = self.current_call.as_ref().map(|c| c.call_id.clone());
        Ok(result)
    }

    /// Keep the pre-roll bounded (spec 6.5: a 60 s RAM ring buffer per channel).
    ///
    /// Outside a call and outside `ARMED` there is nothing to keep at all; inside
    /// `ARMED` the arm times out after 20 s, so the cap is only ever reached if the
    /// detector is wedged. Either way an unbounded buffer on a process that runs for
    /// a whole shift is a leak, not a design.
    fn bound_preroll(&mut self) {
        let armed = self.detector.state() == CallState::Armed;
        let in_call = self.current_call.is_some();
        for pipe in [self.far.as_mut(), self.near.as_mut()].into_iter().flatten() {
            if !armed && !in_call {
                pipe.ready.clear();
            } else if pipe.ready.len() > PREROLL_MAX_SEGMENTS {
                let excess = pipe.ready.len() - PREROLL_MAX_SEGMENTS;
                pipe.ready.drain(..excess);
            }
        }
    }

    /// Handle a device that went away mid-call, per spec 6.3.
    pub fn on_device_lost(&mut self) -> Transition {
        self.far = None;
        self.near = None;
        self.events
            .push(ClientEvent::new(EventKind::DeviceLost, self.clock_ms));
        self.detector
            .feed(self.clock_ms, DetectorInput::Block(BlockReason::PinnedDeviceMissing))
    }

    fn read_channel(
        &mut self,
        source: &mut dyn CaptureSource,
        channel: Channel,
    ) -> anyhow::Result<usize> {
        let pipe = match channel {
            Channel::Far => self.far.as_mut(),
            Channel::Near => self.near.as_mut(),
        };
        let Some(pipe) = pipe else { return Ok(0) };

        let n = match source.read_frames(pipe.stream, &mut pipe.buffer) {
            Ok(n) => n,
            Err(sentinel_capture::source::CaptureError::DeviceInvalidated) => {
                // AUDCLNT_E_DEVICE_INVALIDATED: the headset was unplugged. Reported
                // upward so the caller can re-resolve the pinned device; agents
                // unplug USB headsets constantly and this must not lose a call.
                return Err(anyhow::anyhow!("device invalidated"));
            }
            Err(e) => return Err(e.into()),
        };
        if n == 0 {
            return Ok(0);
        }
        pipe.samples_seen += n as u64;
        let samples: Vec<i16> = pipe.buffer[..n].to_vec();
        for frame in samples.chunks(FRAME_SAMPLES) {
            if frame.len() == FRAME_SAMPLES {
                pipe.vad.push_frame(frame);
            }
        }
        // Whatever the encoder finished on this push is held on the pipe until the
        // detector says whether it belongs to a call. Discarding it here — the easy
        // mistake — silently loses every segment, because the encoder only ever emits
        // from `push_samples`.
        let finished = pipe.encoder.push_samples(&samples)?;
        pipe.ready.extend(finished);
        Ok(n)
    }

    fn apply_transition(
        &mut self,
        transition: &Transition,
        uplink: &mut Uplink,
        result: &mut StepResult,
    ) -> anyhow::Result<()> {
        match transition {
            Transition::Armed => {
                // Begin the pre-roll. The sequence space restarts here so that if this
                // arm becomes a call, its first segment is seq 0 as the wire contract
                // requires.
                for pipe in [self.far.as_mut(), self.near.as_mut()].into_iter().flatten() {
                    pipe.restart()?;
                }
            }
            Transition::CallStarted => {
                let call_id = ulid::Ulid::new().to_string();
                let started_at = crate::heartbeat::rfc3339_millis(time::OffsetDateTime::now_utc());
                let start = ControlMessage::CallStart(CallStart {
                    call_id: call_id.clone(),
                    started_at: started_at.clone(),
                    // Stamped at call.start and never changed: if the signed-in user
                    // changes mid-call the record is closed and a new one opened.
                    user_uid: self.user_uid.clone(),
                    device_id: self.device_id.clone(),
                    tier: self.tier,
                    account_ref: self.account_ref.clone(),
                    dialer_call_id: None,
                    direction: CallDirection::Outbound,
                    codec: "opus".into(),
                    rate: SAMPLE_RATE,
                });
                uplink.begin_call(&call_id, &start, self.clock_ms)?;
                self.last_seq.clear();
                self.current_call = Some(CurrentCall { call_id: call_id.clone(), started_at });
                result.call_id = Some(call_id);
            }
            Transition::CallEnded(reason) => {
                self.finish_call(*reason, uplink, result)?;
            }
            Transition::ArmDiscarded => {
                // Ringback or hold music for the whole window: no human, no call. The
                // buffered pre-roll is discarded rather than spooled, because audio
                // with no `call_id` is unattributable and recording an agent who is
                // not on a call is the surveillance complaint this product cannot
                // afford.
                for pipe in [self.far.as_mut(), self.near.as_mut()].into_iter().flatten() {
                    pipe.restart()?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Flush the encoders and emit `call.end`.
    fn finish_call(
        &mut self,
        reason: EndReason,
        uplink: &mut Uplink,
        result: &mut StepResult,
    ) -> anyhow::Result<()> {
        let Some(call) = self.current_call.take() else { return Ok(()) };

        // Flush whole and partial segments *before* call.end: last_seq must name the
        // highest sequence the client actually produced, and the server holds
        // finalization open until everything below it arrives.
        for pipe in [self.far.as_mut(), self.near.as_mut()].into_iter().flatten() {
            let mut tail: Vec<crate::encode::Segment> = std::mem::take(&mut pipe.ready);
            if let Some(seg) = pipe.encoder.flush()? {
                tail.push(seg);
            }
            for seg in tail {
                let row = to_row(&call.call_id, &seg, self.clock_ms);
                uplink.push_segment(&row)?;
                result.segments_spooled += 1;
                self.last_seq.insert(pipe.channel, seg.seq);
            }
        }

        let last_seq: std::collections::BTreeMap<String, u32> = self
            .last_seq
            .iter()
            .map(|(ch, seq)| (ch.as_u8().to_string(), *seq))
            .collect();
        let end = ControlMessage::CallEnd(CallEnd {
            call_id: call.call_id.clone(),
            ended_at: crate::heartbeat::rfc3339_millis(time::OffsetDateTime::now_utc()),
            reason,
            last_seq,
        });
        uplink.end_call(&call.call_id, &end)?;
        let _ = call.started_at;
        Ok(())
    }

    fn drain_encoders(&mut self, uplink: &mut Uplink) -> anyhow::Result<usize> {
        let Some(call) = self.current_call.as_ref() else { return Ok(0) };
        let call_id = call.call_id.clone();
        let clock = self.clock_ms;
        let mut spooled = 0;

        // Move everything the encoders have finished — including the pre-roll buffered
        // while ARMED — into the spool.
        for pipe in [self.far.as_mut(), self.near.as_mut()].into_iter().flatten() {
            for seg in std::mem::take(&mut pipe.ready) {
                let row = to_row(&call_id, &seg, clock);
                uplink.push_segment(&row)?;
                self.last_seq.insert(pipe.channel, seg.seq);
                spooled += 1;
            }
        }
        Ok(spooled)
    }
}

/// Pick the pinned endpoint for a direction, or nothing.
fn pick<'a>(
    devices: &'a [AudioDevice],
    policy: &Policy,
    direction: Direction,
) -> Option<&'a AudioDevice> {
    resolve_pinned(&policy.pinned_devices, devices, direction)
}

fn to_row(call_id: &str, seg: &crate::encode::Segment, created_ms: u64) -> SegmentRow {
    SegmentRow {
        call_id: call_id.to_string(),
        channel: seg.channel,
        seq: seg.seq,
        timestamp_ms: seg.timestamp_ms,
        flags: seg.flags,
        payload: seg.payload.clone(),
        created_ms,
    }
}

/// Segment duration, re-exported so callers sizing buffers agree with the encoder.
pub const SEGMENT_DURATION_MS: u64 = SEGMENT_MS;

/// Milliseconds of audio in one read slice.
pub const READ_MS: u64 = FRAME_MS as u64;

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_capture::replay::WavReplaySource;
    use sentinel_core::config::{PinnedDevice, SpoolLimits};
    use sentinel_core::spool::Spool;
    use crate::uplink::{Transport, TransportError, TransportFactory};
    use std::f32::consts::TAU;

    /// The pipeline tests are about audio, not the socket, so the uplink is given a
    /// factory that never connects: everything stays in the spool where it can be
    /// inspected. The socket path has its own tests.
    struct NeverConnects;
    impl TransportFactory for NeverConnects {
        fn connect(&mut self) -> Result<Box<dyn Transport>, TransportError> {
            Err(TransportError::Connect("offline".into()))
        }
    }

    fn speech(ms: usize) -> Vec<i16> {
        let n = ms * SAMPLE_RATE as usize / 1000;
        (0..n)
            .map(|i| {
                let t = i as f32 / SAMPLE_RATE as f32;
                let env = 0.3 + 0.2 * (TAU * 3.0 * t).sin();
                (env * i16::MAX as f32 * (TAU * 240.0 * t).sin()) as i16
            })
            .collect()
    }

    fn silence(ms: usize) -> Vec<i16> {
        vec![0i16; ms * SAMPLE_RATE as usize / 1000]
    }

    fn policy() -> Policy {
        Policy {
            pinned_devices: vec![PinnedDevice {
                container_id: "cont-headset".into(),
                friendly_name: Some("Jabra Evolve 20".into()),
            }],
            ..Policy::default()
        }
    }

    fn source(far: Vec<i16>, near: Vec<i16>) -> WavReplaySource {
        let mut s = WavReplaySource::new().without_realtime_pacing();
        s.add_samples("ep-render", "cont-headset", "Jabra Evolve 20", Direction::Render, far);
        s.add_samples("ep-capture", "cont-headset", "Jabra Evolve 20", Direction::Capture, near);
        s
    }

    fn uplink() -> Uplink {
        Uplink::new(
            Spool::open_in_memory(SpoolLimits::default()).unwrap(),
            Box::new(NeverConnects),
        )
    }

    #[test]
    fn the_pinned_device_is_required_and_the_default_is_never_used() {
        let mut src = WavReplaySource::new().without_realtime_pacing();
        src.add_samples("ep-other", "cont-other", "Speakers", Direction::Render, silence(100));
        let mut p =
            Pipeline::new(&policy(), CaptureTier::B, "dev".into(), "uid-a".into()).unwrap();
        assert!(p.open(&mut src, &policy()).is_err());
        assert_eq!(p.state(), CallState::Blocked);
        assert!(p
            .take_events()
            .iter()
            .any(|e| e.kind == EventKind::DeviceLost));
    }

    #[test]
    fn a_replayed_call_is_detected_encoded_and_spooled_on_both_channels() {
        // Spec 14: WavReplaySource drives the full path with no audio hardware.
        let far = [silence(200), speech(4000), silence(12_000)].concat();
        let near = [silence(400), speech(3000), silence(12_800)].concat();
        let mut src = source(far, near);
        let mut p =
            Pipeline::new(&policy(), CaptureTier::A, "dev-1".into(), "uid-a".into()).unwrap();
        p.open(&mut src, &policy()).unwrap();
        let mut up = uplink();

        p.on_session_state(true);
        let mut started = false;
        let mut ended = false;
        for i in 0..1200 {
            // The softphone session goes Inactive after the speech, which combined
            // with silence on both channels is what a hangup looks like.
            if i == 250 {
                p.on_session_state(false);
            }
            let r = p.step(&mut src, &mut up).unwrap();
            match r.transition {
                Some(Transition::CallStarted) => started = true,
                Some(Transition::CallEnded(_)) => {
                    ended = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(started, "speech on the far channel must confirm a call");
        assert!(ended, "an Inactive session plus 8 s of silence must end it");

        let stats = up.stats();
        assert!(stats.segments > 0, "audio reached the spool");
        assert!(stats.bytes > 0);

        let calls = up.spool_pending_calls();
        assert_eq!(calls.len(), 1, "one call, not three: the transitions are debounced");
    }

    #[test]
    fn the_two_channels_share_one_clock_so_their_timestamps_agree() {
        let mut src = source(speech(3000), speech(3000));
        let mut p =
            Pipeline::new(&policy(), CaptureTier::A, "dev-1".into(), "uid-a".into()).unwrap();
        p.open(&mut src, &policy()).unwrap();
        let mut up = uplink();
        p.on_session_state(true);
        for _ in 0..200 {
            p.step(&mut src, &mut up).unwrap();
        }
        let Some(call_id) = p.current_call_id().map(str::to_string) else {
            panic!("a call should be open");
        };
        let rows = up.spool_take_pending(&call_id, 1000);
        let far: Vec<u64> = rows
            .iter()
            .filter(|r| r.channel == Channel::Far)
            .map(|r| r.timestamp_ms)
            .collect();
        let near: Vec<u64> = rows
            .iter()
            .filter(|r| r.channel == Channel::Near)
            .map(|r| r.timestamp_ms)
            .collect();
        assert!(!far.is_empty() && !near.is_empty());
        let n = far.len().min(near.len());
        assert_eq!(far[..n], near[..n], "channel drift breaks transcript alignment");
    }

    #[test]
    fn nothing_is_spooled_outside_a_call() {
        // Audio with no call_id is unattributable, and recording an agent who is not
        // on a call is the surveillance complaint this product cannot afford.
        let mut src = source(speech(2000), speech(2000));
        let mut p =
            Pipeline::new(&policy(), CaptureTier::A, "dev-1".into(), "uid-a".into()).unwrap();
        p.open(&mut src, &policy()).unwrap();
        let mut up = uplink();
        // Session never goes Active, so the detector never arms.
        for _ in 0..150 {
            p.step(&mut src, &mut up).unwrap();
        }
        assert_eq!(p.state(), CallState::Idle);
        assert_eq!(up.stats().segments, 0);
    }

    #[test]
    fn a_lost_device_blocks_capture_and_reports_it() {
        let mut src = source(speech(1000), speech(1000));
        let mut p =
            Pipeline::new(&policy(), CaptureTier::B, "dev-1".into(), "uid-a".into()).unwrap();
        p.open(&mut src, &policy()).unwrap();
        p.on_device_lost();
        assert_eq!(p.state(), CallState::Blocked);
        assert!(p.take_events().iter().any(|e| e.kind == EventKind::DeviceLost));
    }
}
