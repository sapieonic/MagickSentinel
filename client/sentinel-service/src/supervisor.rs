//! Agent watchdog policy (spec 6.1).
//!
//! The service launches `SentinelAgent.exe` into each interactive session at logon
//! and relaunches it when it dies, backing off 1 s, 2 s, 4 s … 60 s. It counts the
//! relaunches so the next heartbeat can report them: an agent that keeps dying is
//! either broken or being killed, and both need to be visible to a supervisor rather
//! than absorbed silently.
//!
//! All of that is decided here, with an injected clock and no Windows API, so the
//! backoff sequence and the counter reset are testable. `windows::launcher` does the
//! actual `CreateProcessAsUser`.

use sentinel_core::backoff::Backoff;
use std::collections::BTreeMap;

/// One interactive session's agent.
#[derive(Debug)]
struct Slot {
    /// Set while an agent process is believed to be running.
    running: bool,
    /// Earliest time a relaunch may be attempted.
    next_attempt_ms: u64,
    backoff: Backoff,
    /// When the current process started, used to decide whether it lived long enough
    /// to count as healthy.
    started_ms: Option<u64>,
}

/// What the caller should do now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Launch the agent into this session.
    Launch { session_id: u32 },
    /// Nothing is due; the earliest deadline is at this clock reading. `None` means
    /// there is no pending work at all and the caller may block indefinitely.
    Idle { next_deadline_ms: Option<u64> },
}

/// A relaunch that lasted at least this long is treated as a healthy run, and resets
/// the backoff. Without it, an agent that crashes after 55 s of work would inch its
/// way to the 60 s cap and stay there — and an agent that crashes instantly would
/// have its backoff reset by the launch itself.
pub const HEALTHY_RUN_MS: u64 = 120_000;

/// Restart counters reset daily (spec: "reset count daily"), matching the service
/// recovery configuration the installer writes.
pub const COUNTER_RESET_MS: u64 = 24 * 3_600_000;

#[derive(Debug)]
pub struct Supervisor {
    sessions: BTreeMap<u32, Slot>,
    /// Relaunches since the counter last reset, across all sessions. Reported in the
    /// heartbeat as `agent_restarts`.
    restarts: u32,
    counter_epoch_ms: u64,
}

impl Supervisor {
    pub fn new(now_ms: u64) -> Self {
        Supervisor { sessions: BTreeMap::new(), restarts: 0, counter_epoch_ms: now_ms }
    }

    /// `WTS_SESSION_LOGON`, or a session already present when the service starts.
    ///
    /// Idempotent: Windows delivers `SERVICE_CONTROL_SESSIONCHANGE` for unlock and
    /// remote-reconnect events too, and re-entering an already-tracked session must
    /// not start a second agent.
    pub fn on_logon(&mut self, session_id: u32, now_ms: u64) {
        self.sessions.entry(session_id).or_insert_with(|| Slot {
            running: false,
            next_attempt_ms: now_ms,
            backoff: Backoff::relaunch(),
            started_ms: None,
        });
    }

    /// `WTS_SESSION_LOGOFF`. The session is gone; so is any agent in it.
    pub fn on_logoff(&mut self, session_id: u32) {
        self.sessions.remove(&session_id);
    }

    /// Record that a launch succeeded.
    pub fn on_launched(&mut self, session_id: u32, now_ms: u64) {
        if let Some(slot) = self.sessions.get_mut(&session_id) {
            slot.running = true;
            slot.started_ms = Some(now_ms);
        }
    }

    /// Record that the agent process exited.
    ///
    /// `expected` is true for an orderly shutdown (the service is stopping, or the
    /// session is ending); those are not counted as restarts and do not back off.
    pub fn on_exited(&mut self, session_id: u32, now_ms: u64, expected: bool) {
        let Some(slot) = self.sessions.get_mut(&session_id) else { return };
        slot.running = false;
        let lifetime = now_ms.saturating_sub(slot.started_ms.unwrap_or(now_ms));
        slot.started_ms = None;
        if expected {
            slot.next_attempt_ms = now_ms;
            return;
        }
        if lifetime >= HEALTHY_RUN_MS {
            // It ran fine for a long stretch and then died once. Treat the next
            // relaunch as the first, not as a continuation of an old crash loop.
            slot.backoff.reset();
        }
        let delay = slot.backoff.next_delay();
        slot.next_attempt_ms = now_ms.saturating_add(delay.as_millis() as u64);
        self.restarts = self.restarts.saturating_add(1);
    }

    /// Decide what to do at `now_ms`. Also performs the daily counter reset.
    pub fn poll(&mut self, now_ms: u64) -> Action {
        if now_ms.saturating_sub(self.counter_epoch_ms) >= COUNTER_RESET_MS {
            self.restarts = 0;
            self.counter_epoch_ms = now_ms;
        }
        let mut next_deadline: Option<u64> = None;
        for (&session_id, slot) in &self.sessions {
            if slot.running {
                continue;
            }
            if now_ms >= slot.next_attempt_ms {
                return Action::Launch { session_id };
            }
            next_deadline = Some(match next_deadline {
                Some(d) => d.min(slot.next_attempt_ms),
                None => slot.next_attempt_ms,
            });
        }
        Action::Idle { next_deadline_ms: next_deadline }
    }

    /// Relaunches since the counter last reset. Reported in the heartbeat.
    pub fn restarts(&self) -> u32 {
        self.restarts
    }

    pub fn tracked_sessions(&self) -> Vec<u32> {
        self.sessions.keys().copied().collect()
    }

    pub fn is_running(&self, session_id: u32) -> bool {
        self.sessions.get(&session_id).is_some_and(|s| s.running)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive one session through `n` crash/relaunch cycles, returning the delay the
    /// supervisor imposed before each relaunch.
    fn crash_loop(n: usize) -> Vec<u64> {
        let mut s = Supervisor::new(0);
        s.on_logon(1, 0);
        let mut now = 0u64;
        let mut delays = Vec::new();
        for _ in 0..n {
            assert_eq!(s.poll(now), Action::Launch { session_id: 1 });
            s.on_launched(1, now);
            now += 500; // dies half a second in
            s.on_exited(1, now, false);
            let crashed_at = now;
            // Advance to whenever the supervisor is willing to launch again.
            let deadline = match s.poll(now) {
                Action::Idle { next_deadline_ms: Some(d) } => d,
                other => panic!("expected a wait after a crash, got {other:?}"),
            };
            delays.push(deadline - crashed_at);
            now = deadline;
        }
        delays
    }

    #[test]
    fn relaunch_backoff_is_one_two_four_seconds_capped_at_sixty() {
        assert_eq!(
            crash_loop(9),
            vec![1_000, 2_000, 4_000, 8_000, 16_000, 32_000, 60_000, 60_000, 60_000]
        );
    }

    #[test]
    fn a_healthy_run_resets_the_backoff() {
        let mut s = Supervisor::new(0);
        s.on_logon(1, 0);
        let mut now = 0;
        // Two quick crashes push the backoff to 4 s.
        for _ in 0..2 {
            assert_eq!(s.poll(now), Action::Launch { session_id: 1 });
            s.on_launched(1, now);
            now += 500;
            s.on_exited(1, now, false);
            now = match s.poll(now) {
                Action::Idle { next_deadline_ms: Some(d) } => d,
                other => panic!("{other:?}"),
            };
        }
        // Now a full shift's worth of uptime, then one crash.
        assert_eq!(s.poll(now), Action::Launch { session_id: 1 });
        s.on_launched(1, now);
        now += HEALTHY_RUN_MS + 1;
        s.on_exited(1, now, false);
        match s.poll(now) {
            Action::Idle { next_deadline_ms: Some(d) } => {
                assert_eq!(d - now, 1_000, "a long healthy run must start the backoff over");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn restarts_are_counted_and_reset_daily() {
        let mut s = Supervisor::new(0);
        s.on_logon(1, 0);
        let mut now = 0;
        for _ in 0..3 {
            s.poll(now);
            s.on_launched(1, now);
            now += 500;
            s.on_exited(1, now, false);
            now = match s.poll(now) {
                Action::Idle { next_deadline_ms: Some(d) } => d,
                other => panic!("{other:?}"),
            };
        }
        assert_eq!(s.restarts(), 3);

        s.poll(now + COUNTER_RESET_MS - 1);
        assert_eq!(s.restarts(), 3, "the counter holds until a full day has passed");
        s.poll(now + COUNTER_RESET_MS);
        assert_eq!(s.restarts(), 0);
    }

    #[test]
    fn an_expected_exit_is_not_a_restart_and_does_not_back_off() {
        // Logoff, service stop and an update-driven restart all land here. Counting
        // them would make every shift change look like tampering.
        let mut s = Supervisor::new(0);
        s.on_logon(1, 0);
        s.poll(0);
        s.on_launched(1, 0);
        s.on_exited(1, 5_000, true);
        assert_eq!(s.restarts(), 0);
        assert_eq!(s.poll(5_000), Action::Launch { session_id: 1 });
    }

    #[test]
    fn logon_is_idempotent_so_unlock_events_do_not_start_a_second_agent() {
        // SERVICE_CONTROL_SESSIONCHANGE also fires for unlock and remote reconnect.
        let mut s = Supervisor::new(0);
        s.on_logon(2, 0);
        s.poll(0);
        s.on_launched(2, 0);
        s.on_logon(2, 1_000);
        assert!(s.is_running(2));
        assert_eq!(s.poll(1_000), Action::Idle { next_deadline_ms: None });
    }

    #[test]
    fn logoff_stops_the_watchdog_for_that_session() {
        let mut s = Supervisor::new(0);
        s.on_logon(3, 0);
        s.on_logoff(3);
        assert_eq!(s.poll(0), Action::Idle { next_deadline_ms: None });
        assert!(s.tracked_sessions().is_empty());
    }

    #[test]
    fn two_sessions_are_watched_independently() {
        // Two shifts sharing a desktop with fast user switching, or an RDP session
        // alongside the console.
        let mut s = Supervisor::new(0);
        s.on_logon(1, 0);
        s.on_logon(2, 0);
        assert_eq!(s.poll(0), Action::Launch { session_id: 1 });
        s.on_launched(1, 0);
        assert_eq!(s.poll(0), Action::Launch { session_id: 2 });
        s.on_launched(2, 0);
        assert_eq!(s.poll(0), Action::Idle { next_deadline_ms: None });

        s.on_exited(1, 1_000, false);
        assert_eq!(s.poll(1_000), Action::Idle { next_deadline_ms: Some(2_000) });
        assert!(s.is_running(2), "session 2 is unaffected by session 1 crashing");
    }
}
