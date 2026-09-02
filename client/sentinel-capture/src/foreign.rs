//! Foreign-audio suppression (spec section 3, tier B mitigations).
//!
//! Endpoint loopback captures **everything** rendered to the pinned device — Spotify,
//! Teams, notification sounds. Device pinning limits that to the agent's headset;
//! this module handles the rest.
//!
//! The rule: if loopback energy exceeds the VAD threshold while the softphone's audio
//! session state is `Inactive`, the audio is not call audio. Those segments are
//! marked `foreign` in the frame header. The server stores them — so we can show a
//! reviewer exactly what was discarded and prove we did not transcribe it — but never
//! sends them to ASR.
//!
//! Two details that matter in practice:
//!
//! * **Session state leads and lags the audio.** `OnStateChanged` fires a beat after
//!   the first samples arrive and a beat before the last ones stop, so a hard edge
//!   would mark the head and tail of every call foreign. A grace window on each side
//!   fixes it.
//! * **When in doubt, mark foreign.** A foreign-marked segment loses transcript
//!   coverage on one call. A wrongly-kept segment means we transcribed an agent's
//!   music, which is the exact accusation the mitigation exists to answer.

use sentinel_core::events::{ClientEvent, EventKind};

#[derive(Debug, Clone, Copy)]
pub struct SuppressorParams {
    /// How long after the session goes Inactive audio is still treated as call audio.
    pub trailing_grace_ms: u64,
    /// How long before the session goes Active audio is retroactively treated as call
    /// audio. Implemented as a hold on the decision, not a rewrite of history.
    pub leading_grace_ms: u64,
    /// Suppressed milliseconds that must accumulate before an event is emitted, so a
    /// single notification chirp does not produce a heartbeat entry.
    pub report_after_ms: u64,
}

impl Default for SuppressorParams {
    fn default() -> Self {
        SuppressorParams {
            trailing_grace_ms: 1_500,
            leading_grace_ms: 1_000,
            report_after_ms: 2_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Call audio. Encode and upload normally.
    Keep,
    /// Not call audio. Encode, mark `foreign`, upload, never transcribe.
    MarkForeign,
}

/// Decides per segment whether tier B loopback audio belongs to the call.
///
/// Tier A never needs this: process loopback only ever hands us the softphone's own
/// audio, so [`ForeignAudioSuppressor::for_tier_a`] returns a suppressor that keeps
/// everything.
#[derive(Debug, Clone)]
pub struct ForeignAudioSuppressor {
    params: SuppressorParams,
    enabled: bool,
    session_active: bool,
    /// Clock reading of the last Active→Inactive edge.
    inactive_since_ms: Option<u64>,
    /// Clock reading of the last Inactive→Active edge.
    active_since_ms: Option<u64>,
    suppressed_ms: u64,
    unreported_ms: u64,
}

impl ForeignAudioSuppressor {
    pub fn new(params: SuppressorParams) -> Self {
        ForeignAudioSuppressor {
            params,
            enabled: true,
            session_active: false,
            inactive_since_ms: Some(0),
            active_since_ms: None,
            suppressed_ms: 0,
            unreported_ms: 0,
        }
    }

    /// Tier A: process loopback is already scoped to the softphone, so nothing is
    /// foreign by construction.
    pub fn for_tier_a() -> Self {
        let mut s = ForeignAudioSuppressor::new(SuppressorParams::default());
        s.enabled = false;
        s
    }

    pub fn set_session_active(&mut self, now_ms: u64, active: bool) {
        if active == self.session_active {
            return;
        }
        self.session_active = active;
        if active {
            self.active_since_ms = Some(now_ms);
            self.inactive_since_ms = None;
        } else {
            self.inactive_since_ms = Some(now_ms);
            self.active_since_ms = None;
        }
    }

    /// Total milliseconds suppressed since construction. Reported per device so the
    /// portal can show a customer how much non-call audio the pinning is catching.
    pub fn suppressed_ms(&self) -> u64 {
        self.suppressed_ms
    }

    /// Judge one segment.
    ///
    /// `voiced` is the VAD's verdict on the loopback channel for this segment;
    /// `duration_ms` is its length.
    pub fn judge(&mut self, now_ms: u64, voiced: bool, duration_ms: u64) -> Decision {
        if !self.enabled {
            return Decision::Keep;
        }
        if self.session_active {
            // Inside the leading grace we still keep; the point of that window is the
            // audio *before* the edge, which the caller holds back (see `hold_ms`).
            return Decision::Keep;
        }
        if let Some(since) = self.inactive_since_ms {
            if now_ms.saturating_sub(since) < self.params.trailing_grace_ms {
                return Decision::Keep;
            }
        }
        if !voiced {
            // Silence while the softphone is idle is neither call audio nor a privacy
            // problem. Marking it foreign would drown the useful signal in noise.
            return Decision::Keep;
        }
        self.suppressed_ms += duration_ms;
        self.unreported_ms += duration_ms;
        Decision::MarkForeign
    }

    /// How long the encoder should hold a segment before committing its foreign flag,
    /// so a session that goes Active moments later reclaims it.
    pub fn hold_ms(&self) -> u64 {
        if self.enabled { self.params.leading_grace_ms } else { 0 }
    }

    /// Drain an event if enough audio has been suppressed to be worth reporting.
    pub fn take_event(&mut self, now_ms: u64) -> Option<ClientEvent> {
        if self.unreported_ms < self.params.report_after_ms {
            return None;
        }
        let ms = std::mem::take(&mut self.unreported_ms);
        Some(
            ClientEvent::new(EventKind::ForeignAudioSuppressed, now_ms)
                .with_count(ms)
                .with_detail("loopback energy while the softphone session was inactive".into()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEG: u64 = 1_000;

    fn suppressor() -> ForeignAudioSuppressor {
        ForeignAudioSuppressor::new(SuppressorParams::default())
    }

    #[test]
    fn music_while_the_softphone_is_idle_is_marked_foreign() {
        let mut s = suppressor();
        s.set_session_active(0, false);
        // Past the trailing grace, loud audio with no call is Spotify.
        assert_eq!(s.judge(5_000, true, SEG), Decision::MarkForeign);
        assert_eq!(s.suppressed_ms(), SEG);
    }

    #[test]
    fn call_audio_is_kept() {
        let mut s = suppressor();
        s.set_session_active(1_000, true);
        assert_eq!(s.judge(2_000, true, SEG), Decision::Keep);
        assert_eq!(s.judge(60_000, true, SEG), Decision::Keep);
        assert_eq!(s.suppressed_ms(), 0);
    }

    #[test]
    fn the_tail_of_a_call_is_not_marked_foreign() {
        // OnStateChanged lags the last samples. Without the trailing grace the final
        // second of every call would be discarded, which loses exactly the part of
        // the conversation where the promise to pay is confirmed.
        let mut s = suppressor();
        s.set_session_active(0, true);
        s.set_session_active(10_000, false);
        assert_eq!(s.judge(10_500, true, SEG), Decision::Keep);
        assert_eq!(s.judge(11_400, true, SEG), Decision::Keep);
        assert_eq!(s.judge(12_000, true, SEG), Decision::MarkForeign);
    }

    #[test]
    fn silence_while_idle_is_kept_not_flagged() {
        let mut s = suppressor();
        s.set_session_active(0, false);
        assert_eq!(s.judge(30_000, false, SEG), Decision::Keep);
        assert_eq!(s.suppressed_ms(), 0);
    }

    #[test]
    fn tier_a_suppresses_nothing() {
        let mut s = ForeignAudioSuppressor::for_tier_a();
        s.set_session_active(0, false);
        assert_eq!(s.judge(60_000, true, SEG), Decision::Keep);
        assert_eq!(s.hold_ms(), 0);
        assert!(s.take_event(60_000).is_none());
    }

    #[test]
    fn a_short_chirp_does_not_produce_an_event_but_a_playlist_does() {
        let mut s = suppressor();
        s.set_session_active(0, false);
        s.judge(5_000, true, 1_000);
        assert!(s.take_event(5_000).is_none(), "one second is not worth reporting");

        for i in 0..10 {
            s.judge(6_000 + i * SEG, true, SEG);
        }
        let ev = s.take_event(20_000).expect("sustained foreign audio must be reported");
        assert_eq!(ev.kind, EventKind::ForeignAudioSuppressed);
        assert!(ev.count.unwrap() >= 10_000);
        assert!(s.take_event(20_000).is_none(), "the counter resets after reporting");
    }

    #[test]
    fn resuming_a_call_stops_suppression_immediately() {
        let mut s = suppressor();
        s.set_session_active(0, false);
        assert_eq!(s.judge(10_000, true, SEG), Decision::MarkForeign);
        s.set_session_active(10_500, true);
        assert_eq!(s.judge(11_000, true, SEG), Decision::Keep);
    }

    #[test]
    fn repeated_state_reports_do_not_reset_the_grace_window() {
        let mut s = suppressor();
        s.set_session_active(0, true);
        s.set_session_active(10_000, false);
        // The session enumerator polls; the same Inactive is observed repeatedly.
        for t in [10_100, 10_500, 11_000, 11_800] {
            s.set_session_active(t, false);
        }
        assert_eq!(
            s.judge(12_000, true, SEG),
            Decision::MarkForeign,
            "grace must run from the edge, not from the last observation"
        );
    }
}
