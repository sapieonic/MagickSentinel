//! Call detection state machine (spec section 6.4).
//!
//! Three signals are combined, because any one alone produces unusable results:
//!
//! 1. **Softphone audio session state** (primary) — `IAudioSessionEvents::OnStateChanged`
//!    fires Active when call audio begins and Inactive when it ends.
//! 2. **Far-channel VAD** (confirmation) — distinguishes a human from ringback or
//!    hold music.
//! 3. **UI Automation metadata** (best effort) — the account reference, which is
//!    allowed to be absent.
//!
//! The machine is pure: it takes timestamped inputs and returns transitions, so the
//! whole of it — including hold-resume and mid-call sign-out — is unit-testable with
//! no audio hardware.

use crate::config::VadConfig;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallState {
    Idle,
    Armed,
    InCall,
    Wrap,
    Finalize,
    /// Capture cannot run: no signed-in user, pinned device missing, offline past
    /// grace, or the device was revoked. Distinct from Idle so the widget and the
    /// heartbeat can say which.
    Blocked,
}

impl CallState {
    pub fn as_str(self) -> &'static str {
        match self {
            CallState::Idle => "IDLE",
            CallState::Armed => "ARMED",
            CallState::InCall => "IN_CALL",
            CallState::Wrap => "WRAP",
            CallState::Finalize => "FINALIZE",
            CallState::Blocked => "BLOCKED",
        }
    }
}

/// Why capture is blocked. Surfaced verbatim in the widget's error state so an agent
/// is told what to fix rather than that "something went wrong".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockReason {
    SignedOut,
    PinnedDeviceMissing,
    OfflineGraceExpired,
    DeviceRevoked,
    Shutdown,
}

/// One observation fed to the machine. Every variant carries the monotonic
/// call-scoped clock reading so the machine never reads the wall clock itself.
#[derive(Debug, Clone, PartialEq)]
pub enum DetectorInput {
    /// The softphone's audio session became Active or Inactive.
    SessionState { active: bool },
    /// Voice activity on the far channel over the interval that just elapsed.
    FarVoice { voiced: bool },
    /// Voice activity on the near channel.
    NearVoice { voiced: bool },
    /// Time passing with no other observation.
    Tick,
    /// Capture became impossible, or possible again.
    Block(BlockReason),
    Unblock,
    /// The signed-in user changed. Never split attribution within a record: the
    /// current call is closed and a new one opens.
    UserChanged { user_uid: String },
}

/// What the caller must do as a result of feeding an input.
#[derive(Debug, Clone, PartialEq)]
pub enum Transition {
    /// No state change.
    None,
    /// Entered `Armed`; start buffering but do not open a call yet.
    Armed,
    /// A call has begun. Mint a ULID, emit `call.start`, begin spooling.
    CallStarted,
    /// Hangup suspected; keep the audio, do not emit anything yet.
    WrapStarted,
    /// Emit `call.end` with this reason and flush.
    CallEnded(crate::protocol::EndReason),
    /// Armed timed out with no speech: discard the buffered audio, no call happened.
    ArmDiscarded,
    /// Capture stopped.
    Blocked(BlockReason),
    /// Capture is possible again.
    Unblocked,
}

/// The state machine.
///
/// All timings come from [`VadConfig`] so a tenant can tune them without a client
/// release. Every transition is debounced: a borrower put on hold must not split one
/// call into three records.
#[derive(Debug, Clone)]
pub struct Detector {
    cfg: VadConfig,
    state: CallState,
    /// Clock reading at which the current state was entered.
    entered_at_ms: u64,
    now_ms: u64,
    /// Last time a transition was taken, for debouncing.
    last_transition_ms: u64,
    session_active: bool,
    /// Accumulated far-channel speech since entering `Armed`.
    armed_speech_ms: u64,
    /// Continuous silence on both channels while in `InCall`.
    silence_ms: u64,
    block_reason: Option<BlockReason>,
    user_uid: Option<String>,
    /// Set when `UserChanged` closed a call and a new one should open immediately.
    reopen_after_end: bool,
    /// Set when an arm timed out while the session was still Active. Without it the
    /// machine would re-arm on the very next tick and spin for as long as the
    /// softphone holds an active-but-silent session — a dialer parked on hold music
    /// would produce an endless stream of arm/discard cycles.
    arm_exhausted: bool,
}

impl Detector {
    pub fn new(cfg: VadConfig) -> Self {
        Detector {
            cfg,
            state: CallState::Blocked,
            entered_at_ms: 0,
            now_ms: 0,
            last_transition_ms: 0,
            session_active: false,
            armed_speech_ms: 0,
            silence_ms: 0,
            block_reason: Some(BlockReason::SignedOut),
            user_uid: None,
            reopen_after_end: false,
            arm_exhausted: false,
        }
    }

    pub fn state(&self) -> CallState {
        self.state
    }

    pub fn block_reason(&self) -> Option<BlockReason> {
        self.block_reason
    }

    pub fn user_uid(&self) -> Option<&str> {
        self.user_uid.as_deref()
    }

    pub fn time_in_state_ms(&self) -> u64 {
        self.now_ms.saturating_sub(self.entered_at_ms)
    }

    /// Advance the clock and feed one observation.
    ///
    /// `now_ms` must be monotonic; it is the same call-scoped clock the capture
    /// threads stamp frames with, so a transition and the frames around it agree.
    pub fn feed(&mut self, now_ms: u64, input: DetectorInput) -> Transition {
        debug_assert!(now_ms >= self.now_ms, "detector clock must be monotonic");
        let elapsed = now_ms.saturating_sub(self.now_ms);
        self.now_ms = now_ms;

        match input {
            DetectorInput::Block(reason) => return self.enter_blocked(reason),
            DetectorInput::Unblock => {
                if self.state == CallState::Blocked {
                    self.block_reason = None;
                    return self.goto(CallState::Idle, Transition::Unblocked);
                }
                return Transition::None;
            }
            DetectorInput::UserChanged { ref user_uid } => {
                let changed = self.user_uid.as_deref() != Some(user_uid.as_str());
                self.user_uid = Some(user_uid.clone());
                if changed && matches!(self.state, CallState::InCall | CallState::Wrap) {
                    // Close the record rather than let one row carry two identities.
                    self.reopen_after_end = self.session_active;
                    self.arm_exhausted = false;
                    return self.goto(
                        CallState::Idle,
                        Transition::CallEnded(crate::protocol::EndReason::SignedOut),
                    );
                }
                return Transition::None;
            }
            DetectorInput::SessionState { active } => {
                if !active {
                    // A full session cycle re-arms the machine.
                    self.arm_exhausted = false;
                }
                self.session_active = active;
            }
            DetectorInput::FarVoice { voiced } => {
                if voiced {
                    self.armed_speech_ms += elapsed;
                    self.silence_ms = 0;
                } else {
                    self.silence_ms += elapsed;
                }
            }
            DetectorInput::NearVoice { voiced } => {
                if voiced {
                    self.silence_ms = 0;
                } else {
                    self.silence_ms += elapsed;
                }
            }
            DetectorInput::Tick => {
                self.silence_ms += elapsed;
            }
        }

        self.evaluate()
    }

    fn evaluate(&mut self) -> Transition {
        match self.state {
            CallState::Blocked => Transition::None,

            CallState::Idle => {
                if self.session_active && !self.arm_exhausted && self.debounced() {
                    self.armed_speech_ms = 0;
                    self.goto(CallState::Armed, Transition::Armed)
                } else {
                    Transition::None
                }
            }

            CallState::Armed => {
                if self.armed_speech_ms >= self.cfg.speech_ms_to_confirm {
                    self.silence_ms = 0;
                    self.goto(CallState::InCall, Transition::CallStarted)
                } else if !self.session_active && self.debounced() {
                    self.goto(CallState::Idle, Transition::ArmDiscarded)
                } else if self.time_in_state_ms() >= self.cfg.armed_timeout_ms {
                    // Ringback or hold music for the whole window: no human, no call.
                    // Do not re-arm until the softphone session cycles.
                    self.arm_exhausted = true;
                    self.goto(CallState::Idle, Transition::ArmDiscarded)
                } else {
                    Transition::None
                }
            }

            CallState::InCall => {
                // Both conditions are required. Silence alone is a borrower thinking;
                // an Inactive session alone is a hold.
                if !self.session_active
                    && self.silence_ms >= self.cfg.hangup_silence_ms
                    && self.debounced()
                {
                    self.goto(CallState::Wrap, Transition::WrapStarted)
                } else {
                    Transition::None
                }
            }

            CallState::Wrap => {
                if self.session_active {
                    // Hold resumed. One call, not three records.
                    self.silence_ms = 0;
                    return self.goto(CallState::InCall, Transition::None);
                }
                if self.time_in_state_ms() >= self.cfg.wrap_ms {
                    self.goto(
                        CallState::Finalize,
                        Transition::CallEnded(crate::protocol::EndReason::Hangup),
                    )
                } else {
                    Transition::None
                }
            }

            CallState::Finalize => {
                if self.reopen_after_end && self.session_active {
                    self.reopen_after_end = false;
                    self.arm_exhausted = false;
                    self.armed_speech_ms = 0;
                    return self.goto(CallState::Armed, Transition::Armed);
                }
                self.goto(CallState::Idle, Transition::None)
            }
        }
    }

    fn enter_blocked(&mut self, reason: BlockReason) -> Transition {
        let was_in_call = matches!(self.state, CallState::InCall | CallState::Wrap);
        self.block_reason = Some(reason);
        self.state = CallState::Blocked;
        self.entered_at_ms = self.now_ms;
        self.last_transition_ms = self.now_ms;
        if was_in_call {
            // The audio already captured is still attributable and still uploads;
            // the call is simply cut short with an honest reason.
            Transition::CallEnded(match reason {
                BlockReason::SignedOut => crate::protocol::EndReason::SignedOut,
                BlockReason::PinnedDeviceMissing => crate::protocol::EndReason::DeviceLost,
                BlockReason::OfflineGraceExpired => crate::protocol::EndReason::Error,
                BlockReason::DeviceRevoked => crate::protocol::EndReason::Revoked,
                BlockReason::Shutdown => crate::protocol::EndReason::Shutdown,
            })
        } else {
            Transition::Blocked(reason)
        }
    }

    /// Transitions out of a steady state are debounced so jitter in the session
    /// state does not fragment a call.
    fn debounced(&self) -> bool {
        self.now_ms.saturating_sub(self.last_transition_ms) >= self.cfg.debounce_ms
    }

    fn goto(&mut self, next: CallState, t: Transition) -> Transition {
        self.state = next;
        self.entered_at_ms = self.now_ms;
        self.last_transition_ms = self.now_ms;
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::EndReason;

    fn detector() -> Detector {
        let mut d = Detector::new(VadConfig::default());
        d.feed(0, DetectorInput::UserChanged { user_uid: "agent-a".into() });
        assert_eq!(d.feed(0, DetectorInput::Unblock), Transition::Unblocked);
        d
    }

    /// Drive `ms` of clock in 100 ms slices, feeding the same input each slice.
    fn run(d: &mut Detector, from_ms: u64, ms: u64, input: impl Fn() -> DetectorInput)
        -> (u64, Vec<Transition>)
    {
        let mut t = from_ms;
        let mut out = Vec::new();
        let end = from_ms + ms;
        while t < end {
            t += 100;
            let tr = d.feed(t, input());
            if tr != Transition::None {
                out.push(tr);
            }
        }
        (t, out)
    }

    #[test]
    fn happy_path_idle_to_call_to_end() {
        let mut d = detector();
        // Debounce window has to pass before Idle -> Armed.
        let (t, _) = run(&mut d, 0, 2_000, || DetectorInput::Tick);
        assert_eq!(d.feed(t, DetectorInput::SessionState { active: true }), Transition::Armed);
        assert_eq!(d.state(), CallState::Armed);

        // 300 ms of far-channel speech confirms a human.
        let (t, trs) = run(&mut d, t, 400, || DetectorInput::FarVoice { voiced: true });
        assert!(trs.contains(&Transition::CallStarted), "{trs:?}");
        assert_eq!(d.state(), CallState::InCall);

        // Hangup: session Inactive plus 8 s of silence on both channels.
        let t = { d.feed(t + 100, DetectorInput::SessionState { active: false }); t + 100 };
        let (t, trs) = run(&mut d, t, 9_000, || DetectorInput::Tick);
        assert!(trs.contains(&Transition::WrapStarted), "{trs:?}");

        let (_, trs) = run(&mut d, t, 4_000, || DetectorInput::Tick);
        assert!(trs.contains(&Transition::CallEnded(EndReason::Hangup)), "{trs:?}");
        assert_eq!(d.state(), CallState::Idle);
    }

    #[test]
    fn ringback_with_no_speech_never_opens_a_call() {
        let mut d = detector();
        let (t, _) = run(&mut d, 0, 2_000, || DetectorInput::Tick);
        d.feed(t, DetectorInput::SessionState { active: true });
        // 25 s of session-active silence: ringback, then the dialer gives up.
        let (_, trs) = run(&mut d, t, 25_000, || DetectorInput::FarVoice { voiced: false });
        assert!(trs.contains(&Transition::ArmDiscarded), "{trs:?}");
        assert!(!trs.contains(&Transition::CallStarted));
        assert_eq!(d.state(), CallState::Idle);

        // Still Active but exhausted: no re-arm loop.
        let (t2, trs) = run(&mut d, t + 25_000, 30_000, || DetectorInput::Tick);
        assert!(trs.is_empty(), "must not spin re-arming a live silent session: {trs:?}");

        // A session cycle re-arms it.
        d.feed(t2 + 100, DetectorInput::SessionState { active: false });
        d.feed(t2 + 200, DetectorInput::SessionState { active: true });
        let (_, trs) = run(&mut d, t2 + 200, 3_000, || DetectorInput::FarVoice { voiced: true });
        assert!(trs.contains(&Transition::CallStarted), "{trs:?}");
    }

    #[test]
    fn hold_and_resume_stays_one_call() {
        let mut d = detector();
        let (t, _) = run(&mut d, 0, 2_000, || DetectorInput::Tick);
        d.feed(t, DetectorInput::SessionState { active: true });
        let (t, trs) = run(&mut d, t, 500, || DetectorInput::FarVoice { voiced: true });
        assert!(trs.contains(&Transition::CallStarted));

        // Borrower goes on hold: session Inactive, silence accrues, we enter Wrap.
        d.feed(t + 100, DetectorInput::SessionState { active: false });
        let (t, trs) = run(&mut d, t + 100, 9_000, || DetectorInput::Tick);
        assert!(trs.contains(&Transition::WrapStarted));
        assert_eq!(d.state(), CallState::Wrap);

        // Hold released inside the 3 s wrap window.
        d.feed(t + 100, DetectorInput::SessionState { active: true });
        let (t, trs) = run(&mut d, t + 100, 500, || DetectorInput::FarVoice { voiced: true });
        assert_eq!(d.state(), CallState::InCall, "hold resume must return to the same call");
        assert!(!trs.iter().any(|t| matches!(t, Transition::CallEnded(_))));
        assert!(!trs.contains(&Transition::CallStarted), "must not open a second record");

        // Real hangup now.
        d.feed(t + 100, DetectorInput::SessionState { active: false });
        let (t2, _) = run(&mut d, t + 100, 9_000, || DetectorInput::Tick);
        let (_, trs) = run(&mut d, t2, 4_000, || DetectorInput::Tick);
        assert!(trs.contains(&Transition::CallEnded(EndReason::Hangup)), "{trs:?}");
    }

    #[test]
    fn brief_session_flap_does_not_split_a_call() {
        let mut d = detector();
        let (t, _) = run(&mut d, 0, 2_000, || DetectorInput::Tick);
        d.feed(t, DetectorInput::SessionState { active: true });
        let (mut t, _) = run(&mut d, t, 500, || DetectorInput::FarVoice { voiced: true });

        for _ in 0..5 {
            d.feed(t + 50, DetectorInput::SessionState { active: false });
            d.feed(t + 100, DetectorInput::SessionState { active: true });
            t += 200;
            assert_eq!(d.state(), CallState::InCall);
        }
    }

    #[test]
    fn mid_call_sign_out_closes_the_record_and_opens_a_new_one() {
        let mut d = detector();
        let (t, _) = run(&mut d, 0, 2_000, || DetectorInput::Tick);
        d.feed(t, DetectorInput::SessionState { active: true });
        let (t, _) = run(&mut d, t, 500, || DetectorInput::FarVoice { voiced: true });
        assert_eq!(d.state(), CallState::InCall);

        let tr = d.feed(t + 100, DetectorInput::UserChanged { user_uid: "agent-b".into() });
        assert_eq!(tr, Transition::CallEnded(EndReason::SignedOut));
        assert_eq!(d.user_uid(), Some("agent-b"));

        // The dialer session is still live, so a fresh record opens for the new user.
        let (_, trs) = run(&mut d, t + 100, 3_000, || DetectorInput::FarVoice { voiced: true });
        assert!(trs.contains(&Transition::CallStarted), "{trs:?}");
    }

    #[test]
    fn revocation_mid_call_ends_the_call_and_blocks() {
        let mut d = detector();
        let (t, _) = run(&mut d, 0, 2_000, || DetectorInput::Tick);
        d.feed(t, DetectorInput::SessionState { active: true });
        let (t, _) = run(&mut d, t, 500, || DetectorInput::FarVoice { voiced: true });

        let tr = d.feed(t + 100, DetectorInput::Block(BlockReason::DeviceRevoked));
        assert_eq!(tr, Transition::CallEnded(EndReason::Revoked));
        assert_eq!(d.state(), CallState::Blocked);
        assert_eq!(d.block_reason(), Some(BlockReason::DeviceRevoked));

        // Nothing restarts capture while blocked, however active the softphone is.
        let (_, trs) = run(&mut d, t + 100, 30_000, || DetectorInput::FarVoice { voiced: true });
        assert!(trs.is_empty(), "{trs:?}");
    }

    #[test]
    fn headset_unplug_mid_call_ends_with_device_lost() {
        let mut d = detector();
        let (t, _) = run(&mut d, 0, 2_000, || DetectorInput::Tick);
        d.feed(t, DetectorInput::SessionState { active: true });
        let (t, _) = run(&mut d, t, 500, || DetectorInput::FarVoice { voiced: true });
        assert_eq!(
            d.feed(t + 100, DetectorInput::Block(BlockReason::PinnedDeviceMissing)),
            Transition::CallEnded(EndReason::DeviceLost)
        );
        assert_eq!(d.feed(t + 200, DetectorInput::Unblock), Transition::Unblocked);
        assert_eq!(d.state(), CallState::Idle);
    }

    #[test]
    fn no_signed_in_user_means_no_capture() {
        let mut d = Detector::new(VadConfig::default());
        assert_eq!(d.state(), CallState::Blocked);
        assert_eq!(d.block_reason(), Some(BlockReason::SignedOut));
        let (_, trs) = run(&mut d, 0, 30_000, || DetectorInput::FarVoice { voiced: true });
        assert!(trs.is_empty(), "capture must not run signed out: {trs:?}");
    }

    #[test]
    fn near_channel_speech_alone_keeps_the_call_open() {
        // An agent talking to a silent borrower is still a call.
        let mut d = detector();
        let (t, _) = run(&mut d, 0, 2_000, || DetectorInput::Tick);
        d.feed(t, DetectorInput::SessionState { active: true });
        let (t, _) = run(&mut d, t, 500, || DetectorInput::FarVoice { voiced: true });
        d.feed(t + 100, DetectorInput::SessionState { active: false });
        let (_, trs) = run(&mut d, t + 100, 20_000, || DetectorInput::NearVoice { voiced: true });
        assert!(!trs.iter().any(|t| matches!(t, Transition::CallEnded(_))), "{trs:?}");
        assert_eq!(d.state(), CallState::InCall);
    }
}
