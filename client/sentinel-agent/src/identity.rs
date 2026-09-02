//! The identity gate: both identities, or no capture (spec 7.2, 7.5).
//!
//! Capture requires a valid **device** identity (the mTLS certificate) and a valid
//! **user** identity (a verified Firebase ID token). When the network drops, the last
//! successful verification carries the user identity for a tenant-configurable grace
//! window — default 8 h. During grace, capture continues and audio spools. Past it,
//! capture STOPS.
//!
//! The rule this encodes is "never silently record with no verifiable identity
//! attached to the audio". A recording nobody can attribute is not evidence; in a
//! compliance product it is a liability, because it was made without a defensible
//! claim about who was speaking or on whose authority.
//!
//! Pure and clock-injected, so the boundary conditions are testable rather than
//! observed once in a staging environment eight hours after someone pulled a cable.

use sentinel_core::state::BlockReason;

/// How the identity looks right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityStatus {
    /// Verified against the server within the last successful round trip.
    Online,
    /// Offline, but inside the grace window. Capture continues; audio spools.
    Grace { remaining_ms: u64 },
    /// Offline past the grace window. Capture MUST stop.
    Expired,
    /// Nobody is signed in, or the device was revoked. Not a grace case at all.
    Blocked(BlockReason),
}

impl IdentityStatus {
    /// May capture run?
    pub fn may_capture(&self) -> bool {
        matches!(self, IdentityStatus::Online | IdentityStatus::Grace { .. })
    }

    /// The reason to hand the detector when capture may not run.
    pub fn block_reason(&self) -> Option<BlockReason> {
        match self {
            IdentityStatus::Online | IdentityStatus::Grace { .. } => None,
            IdentityStatus::Expired => Some(BlockReason::OfflineGraceExpired),
            IdentityStatus::Blocked(r) => Some(*r),
        }
    }

    /// The widget's error line. Spec 7.5 fixes this wording: an agent who is told
    /// "an error occurred" will restart the machine; one told to reconnect will
    /// reconnect.
    pub fn widget_message(&self) -> Option<&'static str> {
        match self {
            IdentityStatus::Online => None,
            IdentityStatus::Grace { .. } => Some("Working offline — recording continues"),
            IdentityStatus::Expired => Some("Reconnect to continue recording"),
            IdentityStatus::Blocked(BlockReason::SignedOut) => Some("Sign in to start"),
            IdentityStatus::Blocked(BlockReason::PinnedDeviceMissing) => {
                Some("Headset not detected")
            }
            IdentityStatus::Blocked(BlockReason::DeviceRevoked) => {
                Some("This device has been deactivated — contact your administrator")
            }
            IdentityStatus::Blocked(BlockReason::OfflineGraceExpired) => {
                Some("Reconnect to continue recording")
            }
            IdentityStatus::Blocked(BlockReason::Shutdown) => Some("Shutting down"),
        }
    }
}

/// Tracks the last successful server verification against the grace window.
#[derive(Debug, Clone)]
pub struct IdentityGate {
    grace_ms: u64,
    /// Clock reading of the last successful token verification by the server.
    last_verified_ms: Option<u64>,
    signed_in: bool,
    device_revoked: bool,
    pinned_device_present: bool,
}

impl IdentityGate {
    /// `grace_ms` comes from `Policy::offline_grace_ms()`.
    pub fn new(grace_ms: u64) -> Self {
        IdentityGate {
            grace_ms,
            last_verified_ms: None,
            signed_in: false,
            device_revoked: false,
            pinned_device_present: false,
        }
    }

    pub fn set_grace_ms(&mut self, grace_ms: u64) {
        self.grace_ms = grace_ms;
    }

    /// A request the server accepted: heartbeat, policy fetch, or an ack on the
    /// ingest socket. Any of them proves the token verified server-side just now.
    pub fn observe_verified(&mut self, now_ms: u64) {
        self.last_verified_ms = Some(now_ms);
    }

    /// Sign-in and sign-out. Signing out clears the verification timestamp: the next
    /// user's grace window must not be inherited from the last one's.
    pub fn set_signed_in(&mut self, signed_in: bool) {
        self.signed_in = signed_in;
        if !signed_in {
            self.last_verified_ms = None;
        }
    }

    /// A `4403` close, a 403 on the heartbeat, or a revocation command. Terminal until
    /// an operator acts.
    pub fn set_device_revoked(&mut self, revoked: bool) {
        self.device_revoked = revoked;
    }

    pub fn set_pinned_device_present(&mut self, present: bool) {
        self.pinned_device_present = present;
    }

    pub fn status(&self, now_ms: u64) -> IdentityStatus {
        // Order matters. Revocation outranks everything: a revoked device must stop
        // even mid-call and even while online.
        if self.device_revoked {
            return IdentityStatus::Blocked(BlockReason::DeviceRevoked);
        }
        if !self.signed_in {
            return IdentityStatus::Blocked(BlockReason::SignedOut);
        }
        if !self.pinned_device_present {
            return IdentityStatus::Blocked(BlockReason::PinnedDeviceMissing);
        }
        let Some(last) = self.last_verified_ms else {
            // Signed in from a cached token but never yet verified by the server this
            // session. There is no verification to grant grace from, so capture waits
            // rather than starting on an unchecked credential.
            return IdentityStatus::Expired;
        };
        let elapsed = now_ms.saturating_sub(last);
        if elapsed == 0 {
            return IdentityStatus::Online;
        }
        if elapsed < self.grace_ms {
            IdentityStatus::Grace { remaining_ms: self.grace_ms - elapsed }
        } else {
            IdentityStatus::Expired
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EIGHT_HOURS: u64 = 8 * 3_600_000;

    fn ready_gate() -> IdentityGate {
        let mut g = IdentityGate::new(EIGHT_HOURS);
        g.set_signed_in(true);
        g.set_pinned_device_present(true);
        g.observe_verified(0);
        g
    }

    #[test]
    fn a_fresh_verification_is_online() {
        let g = ready_gate();
        assert_eq!(g.status(0), IdentityStatus::Online);
        assert!(g.status(0).may_capture());
        assert_eq!(g.status(0).widget_message(), None);
    }

    #[test]
    fn capture_continues_through_the_grace_window() {
        let g = ready_gate();
        for t in [1, 1_000, 3_600_000, EIGHT_HOURS - 1] {
            let s = g.status(t);
            assert!(s.may_capture(), "capture must continue at {t} ms offline: {s:?}");
            assert!(matches!(s, IdentityStatus::Grace { .. }));
        }
        assert_eq!(
            g.status(EIGHT_HOURS - 1),
            IdentityStatus::Grace { remaining_ms: 1 }
        );
    }

    #[test]
    fn capture_stops_the_moment_grace_expires() {
        let g = ready_gate();
        assert_eq!(g.status(EIGHT_HOURS), IdentityStatus::Expired, "the boundary is inclusive");
        assert!(!g.status(EIGHT_HOURS).may_capture());
        assert!(!g.status(EIGHT_HOURS + 1).may_capture());
        assert_eq!(
            g.status(EIGHT_HOURS).block_reason(),
            Some(BlockReason::OfflineGraceExpired)
        );
        assert_eq!(
            g.status(EIGHT_HOURS).widget_message(),
            Some("Reconnect to continue recording"),
            "spec 7.5 fixes this wording"
        );
    }

    #[test]
    fn a_successful_round_trip_restarts_the_window() {
        let mut g = ready_gate();
        assert!(matches!(g.status(EIGHT_HOURS - 1), IdentityStatus::Grace { .. }));
        g.observe_verified(EIGHT_HOURS - 1);
        assert_eq!(g.status(EIGHT_HOURS - 1), IdentityStatus::Online);
        assert!(g.status(2 * EIGHT_HOURS - 2).may_capture(), "the window runs from the new mark");
        assert!(!g.status(2 * EIGHT_HOURS - 1).may_capture());
    }

    #[test]
    fn a_tenant_can_shorten_the_window_and_it_takes_effect_immediately() {
        let mut g = ready_gate();
        g.set_grace_ms(3_600_000); // one hour
        assert!(g.status(3_599_999).may_capture());
        assert!(!g.status(3_600_000).may_capture());
    }

    #[test]
    fn a_zero_grace_tenant_stops_the_instant_the_link_drops() {
        let mut g = ready_gate();
        g.set_grace_ms(0);
        assert_eq!(g.status(0), IdentityStatus::Online, "still verified this instant");
        assert_eq!(g.status(1), IdentityStatus::Expired);
    }

    #[test]
    fn signing_out_does_not_hand_the_next_shift_a_grace_window() {
        // Two shifts share the desktop. The evening agent must not inherit the
        // morning agent's verification: their identity has never been checked.
        let mut g = ready_gate();
        g.set_signed_in(false);
        assert_eq!(g.status(1_000), IdentityStatus::Blocked(BlockReason::SignedOut));
        g.set_signed_in(true);
        assert_eq!(g.status(1_000), IdentityStatus::Expired);
        assert!(!g.status(1_000).may_capture());
    }

    #[test]
    fn a_cached_sign_in_that_has_never_been_verified_does_not_capture() {
        // Restored from Credential Manager at logon, network down since boot. There
        // is no verification to grant grace from.
        let mut g = IdentityGate::new(EIGHT_HOURS);
        g.set_signed_in(true);
        g.set_pinned_device_present(true);
        assert_eq!(g.status(0), IdentityStatus::Expired);
        assert!(!g.status(0).may_capture());
    }

    #[test]
    fn revocation_outranks_everything_including_a_live_verification() {
        // Spec 7.2: revoking a device MUST terminate its capture within 60 s, and the
        // wire contract makes 4403 terminal until an operator acts.
        let mut g = ready_gate();
        g.set_device_revoked(true);
        assert_eq!(g.status(0), IdentityStatus::Blocked(BlockReason::DeviceRevoked));
        assert!(!g.status(0).may_capture());
        g.observe_verified(0);
        assert!(!g.status(0).may_capture(), "a later ack must not un-revoke the device");
    }

    #[test]
    fn a_missing_headset_blocks_with_its_own_reason() {
        // Never the system default device: on tier B that records whatever else the
        // machine is playing.
        let mut g = ready_gate();
        g.set_pinned_device_present(false);
        assert_eq!(g.status(0), IdentityStatus::Blocked(BlockReason::PinnedDeviceMissing));
        assert_eq!(g.status(0).widget_message(), Some("Headset not detected"));
    }

    #[test]
    fn every_blocked_state_tells_the_agent_what_to_fix() {
        for r in [
            BlockReason::SignedOut,
            BlockReason::PinnedDeviceMissing,
            BlockReason::OfflineGraceExpired,
            BlockReason::DeviceRevoked,
            BlockReason::Shutdown,
        ] {
            let msg = IdentityStatus::Blocked(r).widget_message();
            assert!(msg.is_some(), "{r:?} has no message");
            assert!(!msg.unwrap().is_empty());
        }
    }
}
