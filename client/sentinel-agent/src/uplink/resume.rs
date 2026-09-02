//! What to re-send after a reconnect (wire.md section 6).
//!
//! Pure, so the decision can be tested against every disagreement the two ack
//! watermarks can be in. Getting it wrong is expensive in both directions: resume too
//! late and a chunk of a call is missing from the evidence, resume too early and the
//! same seconds are uploaded again — harmless for correctness, since ingest is
//! idempotent on `(call_id, channel, seq)`, but it is bandwidth a collections floor's
//! uplink does not have to spare.

use sentinel_core::protocol::Channel;
use std::collections::BTreeMap;

/// Where to restart each channel after a reconnect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumePoint {
    /// The first sequence number to send. `0` means the call has not been acked at
    /// all and everything the spool holds must go.
    pub from_seq: BTreeMap<Channel, u32>,
}

impl ResumePoint {
    pub fn from_seq(&self, ch: Channel) -> u32 {
        self.from_seq.get(&ch).copied().unwrap_or(0)
    }
}

/// Combine our own ack watermark with the one the server just sent in `resume`.
///
/// The two can disagree in both directions and both are honest:
///
/// * **Server ahead** — our ack write was lost to a crash after the server had
///   already durably stored the segments. Trusting the server skips re-sending audio
///   it already has.
/// * **Server behind** — the `resume` frame is stale, or the server's ack record for
///   this connection lags what it told us on the previous one. Trusting *our* record
///   here would be wrong: the server is the authority on what it has durably stored,
///   and a lower number only costs a re-send that ingest deduplicates.
///
/// Taking the maximum is therefore not a compromise; each side's number is a claim
/// that everything below it is durable, and the higher claim is the one supported by
/// evidence from both.
///
/// `server_acked` is keyed by channel number as a string, because JSON object keys
/// are strings — that is the shape `resume` arrives in.
pub fn plan(
    local_acked: &BTreeMap<Channel, u32>,
    server_acked: &BTreeMap<String, u32>,
) -> ResumePoint {
    let mut from_seq = BTreeMap::new();
    for ch in Channel::ALL {
        let local = local_acked.get(&ch).copied();
        let server = server_acked
            .get(&ch.as_u8().to_string())
            .copied();
        let acked = match (local, server) {
            (Some(l), Some(s)) => Some(l.max(s)),
            (Some(l), None) => Some(l),
            (None, Some(s)) => Some(s),
            (None, None) => None,
        };
        // `acked` is *through*; the next sequence to send is one past it. A channel
        // with no ack at all restarts at 0 — never at 1, which would silently drop
        // the first second of every call that reconnects before its first ack.
        from_seq.insert(ch, acked.map_or(0, |a| a.saturating_add(1)));
    }
    ResumePoint { from_seq }
}

/// Should this spooled segment be sent, given the resume point?
pub fn should_send(point: &ResumePoint, channel: Channel, seq: u32) -> bool {
    seq >= point.from_seq(channel)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(far: Option<u32>, near: Option<u32>) -> BTreeMap<Channel, u32> {
        let mut m = BTreeMap::new();
        if let Some(v) = far {
            m.insert(Channel::Far, v);
        }
        if let Some(v) = near {
            m.insert(Channel::Near, v);
        }
        m
    }

    fn server(pairs: &[(&str, u32)]) -> BTreeMap<String, u32> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn a_call_with_no_acks_anywhere_resumes_from_zero() {
        // Never from 1: that would silently drop the first second of every call that
        // reconnects before its first ack.
        let p = plan(&local(None, None), &server(&[]));
        assert_eq!(p.from_seq(Channel::Far), 0);
        assert_eq!(p.from_seq(Channel::Near), 0);
        assert!(should_send(&p, Channel::Far, 0));
    }

    #[test]
    fn resuming_starts_one_past_the_acked_sequence() {
        let p = plan(&local(Some(840), Some(839)), &server(&[("0", 840), ("1", 839)]));
        assert_eq!(p.from_seq(Channel::Far), 841);
        assert_eq!(p.from_seq(Channel::Near), 840);
        assert!(!should_send(&p, Channel::Far, 840), "840 is durable server-side");
        assert!(should_send(&p, Channel::Far, 841));
    }

    #[test]
    fn a_server_ahead_of_our_record_wins() {
        // Our ack write was lost to a crash after the server had already stored the
        // segments. Re-sending them would be wasted uplink on a floor that has none.
        let p = plan(&local(Some(100), Some(100)), &server(&[("0", 400), ("1", 402)]));
        assert_eq!(p.from_seq(Channel::Far), 401);
        assert_eq!(p.from_seq(Channel::Near), 403);
    }

    #[test]
    fn a_stale_server_resume_does_not_pull_us_backwards_into_replaying_acked_audio() {
        let p = plan(&local(Some(500), Some(500)), &server(&[("0", 100), ("1", 0)]));
        assert_eq!(p.from_seq(Channel::Far), 501);
        assert_eq!(p.from_seq(Channel::Near), 501);
    }

    #[test]
    fn a_channel_the_server_omits_falls_back_to_our_record() {
        // The near channel had no traffic yet on the server's side of the connection.
        let p = plan(&local(Some(12), Some(7)), &server(&[("0", 12)]));
        assert_eq!(p.from_seq(Channel::Far), 13);
        assert_eq!(p.from_seq(Channel::Near), 8);
    }

    #[test]
    fn a_channel_only_the_server_knows_about_is_honoured() {
        let p = plan(&local(None, None), &server(&[("1", 3)]));
        assert_eq!(p.from_seq(Channel::Far), 0);
        assert_eq!(p.from_seq(Channel::Near), 4);
    }

    #[test]
    fn both_channels_always_appear_in_the_plan() {
        // A missing entry read as "nothing to send" would strand a whole channel.
        let p = plan(&local(Some(5), None), &server(&[]));
        assert_eq!(p.from_seq.len(), 2);
        assert!(p.from_seq.contains_key(&Channel::Near));
    }

    #[test]
    fn unknown_channel_keys_from_the_server_are_ignored() {
        let p = plan(&local(None, None), &server(&[("7", 999), ("far", 999)]));
        assert_eq!(p.from_seq(Channel::Far), 0);
        assert_eq!(p.from_seq(Channel::Near), 0);
    }

    #[test]
    fn an_ack_at_the_top_of_the_sequence_space_does_not_wrap() {
        let p = plan(&local(Some(u32::MAX), None), &server(&[]));
        assert_eq!(p.from_seq(Channel::Far), u32::MAX, "saturates rather than wrapping to 0");
        assert!(!should_send(&p, Channel::Far, u32::MAX - 1));
    }
}
