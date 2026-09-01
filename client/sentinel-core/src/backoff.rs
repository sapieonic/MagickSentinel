//! Reconnect and relaunch backoff.
//!
//! The uplink uses exponential backoff with **full jitter** (`random(0, cap)`): 200
//! agents on one floor losing the network at the same moment must not reconnect in
//! lockstep. The service's agent-relaunch backoff is the plain doubling sequence the
//! spec names (1 s, 2 s, 4 s … 60 s) because there is only ever one agent per
//! session and a thundering herd is not possible.

use rand::Rng;
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    base: Duration,
    cap: Duration,
    attempt: u32,
    jitter: bool,
}

impl Backoff {
    /// Uplink reconnect policy: base 1 s, cap 60 s, full jitter.
    pub fn uplink() -> Self {
        Backoff { base: Duration::from_secs(1), cap: Duration::from_secs(60), attempt: 0, jitter: true }
    }

    /// Agent relaunch policy: 1 s, 2 s, 4 s … capped at 60 s, no jitter.
    pub fn relaunch() -> Self {
        Backoff { base: Duration::from_secs(1), cap: Duration::from_secs(60), attempt: 0, jitter: false }
    }

    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Deterministic ceiling for the current attempt, before jitter.
    pub fn ceiling(&self) -> Duration {
        let shift = self.attempt.min(20);
        self.base
            .checked_mul(1u32 << shift)
            .unwrap_or(self.cap)
            .min(self.cap)
    }

    /// Take the next delay and advance the attempt counter.
    pub fn next_delay(&mut self) -> Duration {
        let ceiling = self.ceiling();
        self.attempt = self.attempt.saturating_add(1);
        if self.jitter {
            let ms = rand::thread_rng().gen_range(0..=ceiling.as_millis() as u64);
            Duration::from_millis(ms)
        } else {
            ceiling
        }
    }

    /// Call on a successful round-trip.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relaunch_doubles_then_caps_at_sixty_seconds() {
        let mut b = Backoff::relaunch();
        let seq: Vec<u64> = (0..10).map(|_| b.next_delay().as_secs()).collect();
        assert_eq!(seq, vec![1, 2, 4, 8, 16, 32, 60, 60, 60, 60]);
    }

    #[test]
    fn uplink_jitter_stays_within_the_ceiling_and_varies() {
        let mut b = Backoff::uplink();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..8 {
            let ceiling = b.ceiling();
            let d = b.next_delay();
            assert!(d <= ceiling, "{d:?} exceeded ceiling {ceiling:?}");
            seen.insert(d.as_millis());
        }
        assert!(seen.len() > 1, "full jitter must not produce a constant sequence");
    }

    #[test]
    fn reset_returns_to_the_base_ceiling() {
        let mut b = Backoff::uplink();
        for _ in 0..10 {
            b.next_delay();
        }
        assert_eq!(b.ceiling(), Duration::from_secs(60));
        b.reset();
        assert_eq!(b.ceiling(), Duration::from_secs(1));
    }

    #[test]
    fn ceiling_never_overflows() {
        let mut b = Backoff::uplink();
        for _ in 0..1000 {
            b.next_delay();
        }
        assert_eq!(b.ceiling(), Duration::from_secs(60));
    }
}
