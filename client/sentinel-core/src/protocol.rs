//! Wire protocol for `WSS /v1/ingest`, version 1.
//!
//! `contracts/wire.md` is the specification; this module is its client-side
//! implementation. The gateway's `internal/wire` package implements the same bytes,
//! and `server/gateway/internal/wire/wire_test.go` checks the two against a
//! shared fixture.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Protocol version carried in byte 0 of every media record.
pub const VERSION: u8 = 1;

/// Fixed media record header size, in bytes.
pub const HEADER_LEN: usize = 34;

/// Opus packets per one-second segment at 20 ms frames.
pub const FRAMES_PER_SEGMENT: usize = 50;

/// Segments batched into a single WebSocket binary message.
pub const SEGMENTS_PER_MESSAGE: usize = 10;

/// Maximum WebSocket message the gateway will accept.
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("record truncated: need {need} bytes, have {have}")]
    Truncated { need: usize, have: usize },
    #[error("unsupported protocol version {0}")]
    Version(u8),
    #[error("invalid channel {0}, expected 0 (far) or 1 (near)")]
    Channel(u8),
    #[error("reserved bits set in flags byte: {0:#010b}")]
    ReservedFlags(u8),
    #[error("reserved byte 3 must be zero, got {0}")]
    ReservedByte(u8),
    #[error("payload of {0} bytes exceeds the u16 length field")]
    PayloadTooLarge(usize),
    #[error("message of {0} bytes exceeds the {MAX_MESSAGE_BYTES} byte limit")]
    MessageTooLarge(usize),
}

/// Audio channel. The two are kept separate end to end: separate channels give exact
/// speaker attribution with no diarization step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum Channel {
    /// Render loopback — the borrower's voice.
    Far = 0,
    /// Microphone capture — the agent's voice.
    Near = 1,
}

impl Channel {
    pub const ALL: [Channel; 2] = [Channel::Far, Channel::Near];

    pub fn from_u8(v: u8) -> Result<Self, ProtocolError> {
        match v {
            0 => Ok(Channel::Far),
            1 => Ok(Channel::Near),
            other => Err(ProtocolError::Channel(other)),
        }
    }

    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

impl fmt::Display for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Channel::Far => "far",
            Channel::Near => "near",
        })
    }
}

/// Media record flags. Bits 2–7 are reserved and MUST be zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MediaFlags {
    /// Tier B only: loopback energy while the softphone session was Inactive. The
    /// server stores these segments so we can prove what was discarded, but MUST NOT
    /// transcribe them.
    pub foreign: bool,
    /// The record contains synthesised silence covering a glitch gap, inserted so
    /// timestamps stay aligned across the two channels.
    pub silence_inserted: bool,
}

impl MediaFlags {
    const FOREIGN: u8 = 0b0000_0001;
    const SILENCE: u8 = 0b0000_0010;
    const RESERVED: u8 = 0b1111_1100;

    pub fn to_bits(self) -> u8 {
        (if self.foreign { Self::FOREIGN } else { 0 })
            | (if self.silence_inserted { Self::SILENCE } else { 0 })
    }

    pub fn from_bits(bits: u8) -> Result<Self, ProtocolError> {
        if bits & Self::RESERVED != 0 {
            return Err(ProtocolError::ReservedFlags(bits));
        }
        Ok(MediaFlags {
            foreign: bits & Self::FOREIGN != 0,
            silence_inserted: bits & Self::SILENCE != 0,
        })
    }
}

/// One one-second segment of Opus audio for one channel of one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaRecord {
    pub channel: Channel,
    pub flags: MediaFlags,
    pub seq: u32,
    /// Call-relative, from the single call-scoped clock shared by both channels.
    pub timestamp_ms: u64,
    /// ULID in binary form.
    pub call_id: [u8; 16],
    pub payload: Vec<u8>,
}

impl MediaRecord {
    pub fn encoded_len(&self) -> usize {
        HEADER_LEN + self.payload.len()
    }

    /// Append the little-endian encoding of this record to `out`.
    pub fn encode_into(&self, out: &mut Vec<u8>) -> Result<(), ProtocolError> {
        let len = u16::try_from(self.payload.len())
            .map_err(|_| ProtocolError::PayloadTooLarge(self.payload.len()))?;
        out.reserve(self.encoded_len());
        out.push(VERSION);
        out.push(self.channel.as_u8());
        out.push(self.flags.to_bits());
        out.push(0); // reserved
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.timestamp_ms.to_le_bytes());
        out.extend_from_slice(&self.call_id);
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&self.payload);
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, ProtocolError> {
        let mut out = Vec::with_capacity(self.encoded_len());
        self.encode_into(&mut out)?;
        Ok(out)
    }

    /// Decode one record from the front of `buf`, returning it and the bytes consumed.
    pub fn decode(buf: &[u8]) -> Result<(MediaRecord, usize), ProtocolError> {
        if buf.len() < HEADER_LEN {
            return Err(ProtocolError::Truncated { need: HEADER_LEN, have: buf.len() });
        }
        if buf[0] != VERSION {
            return Err(ProtocolError::Version(buf[0]));
        }
        let channel = Channel::from_u8(buf[1])?;
        let flags = MediaFlags::from_bits(buf[2])?;
        if buf[3] != 0 {
            return Err(ProtocolError::ReservedByte(buf[3]));
        }
        let seq = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        let timestamp_ms = u64::from_le_bytes(buf[8..16].try_into().unwrap());
        let mut call_id = [0u8; 16];
        call_id.copy_from_slice(&buf[16..32]);
        let payload_len = u16::from_le_bytes(buf[32..34].try_into().unwrap()) as usize;
        let total = HEADER_LEN + payload_len;
        if buf.len() < total {
            return Err(ProtocolError::Truncated { need: total, have: buf.len() });
        }
        Ok((
            MediaRecord {
                channel,
                flags,
                seq,
                timestamp_ms,
                call_id,
                payload: buf[HEADER_LEN..total].to_vec(),
            },
            total,
        ))
    }

    /// Decode every record in a concatenated binary WebSocket message.
    pub fn decode_all(buf: &[u8]) -> Result<Vec<MediaRecord>, ProtocolError> {
        if buf.len() > MAX_MESSAGE_BYTES {
            return Err(ProtocolError::MessageTooLarge(buf.len()));
        }
        let mut out = Vec::new();
        let mut off = 0;
        while off < buf.len() {
            let (rec, used) = MediaRecord::decode(&buf[off..])?;
            off += used;
            out.push(rec);
        }
        Ok(out)
    }
}

/// A segment payload is 50 length-delimited Opus packets. A zero length means the
/// frame was dropped by a glitch and the decoder inserts 20 ms of silence.
pub fn pack_segment(frames: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::with_capacity(frames.iter().map(|f| f.len() + 2).sum());
    for frame in frames {
        out.extend_from_slice(&(frame.len() as u16).to_le_bytes());
        out.extend_from_slice(frame);
    }
    out
}

/// Inverse of [`pack_segment`].
pub fn unpack_segment(payload: &[u8]) -> Result<Vec<Vec<u8>>, ProtocolError> {
    let mut out = Vec::new();
    let mut off = 0;
    while off < payload.len() {
        if payload.len() - off < 2 {
            return Err(ProtocolError::Truncated { need: off + 2, have: payload.len() });
        }
        let len = u16::from_le_bytes(payload[off..off + 2].try_into().unwrap()) as usize;
        off += 2;
        if payload.len() - off < len {
            return Err(ProtocolError::Truncated { need: off + len, have: payload.len() });
        }
        out.push(payload[off..off + len].to_vec());
        off += len;
    }
    Ok(out)
}

// ---------------------------------------------------------------- control frames

/// Reason a call ended, as reported in `call.end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    Hangup,
    DeviceLost,
    SignedOut,
    Shutdown,
    Revoked,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaptureTier {
    A,
    B,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Outbound,
    Inbound,
}

/// JSON control messages. `t` discriminates; the enum covers both directions so a
/// single decoder handles anything arriving on the socket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum ControlMessage {
    #[serde(rename = "call.start")]
    CallStart(CallStart),

    #[serde(rename = "call.end")]
    CallEnd(CallEnd),

    #[serde(rename = "ack")]
    Ack {
        call_id: String,
        channel: u8,
        through_seq: u32,
    },

    #[serde(rename = "resume")]
    Resume {
        call_id: String,
        /// Highest acked sequence per channel, keyed by the channel number as a
        /// string because JSON object keys are strings.
        acked: std::collections::BTreeMap<String, u32>,
    },

    #[serde(rename = "call.error")]
    CallError {
        call_id: String,
        code: String,
        message: String,
        #[serde(default)]
        fatal: bool,
    },

    #[serde(rename = "heartbeat")]
    Heartbeat {
        sent_at: String,
        capture_state: String,
        spool_depth: u64,
    },

    #[serde(rename = "heartbeat.ack")]
    HeartbeatAck {
        server_time: String,
        policy_version: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallStart {
    /// Client-generated ULID. The server MUST NOT assign one — client generation is
    /// what makes retry-after-reconnect idempotent.
    pub call_id: String,
    pub started_at: String,
    pub user_uid: String,
    pub device_id: String,
    pub tier: CaptureTier,
    pub account_ref: Option<String>,
    pub dialer_call_id: Option<String>,
    pub direction: Direction,
    pub codec: String,
    pub rate: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallEnd {
    pub call_id: String,
    pub ended_at: String,
    pub reason: EndReason,
    /// Highest sequence produced per channel, keyed by channel number as a string.
    pub last_seq: std::collections::BTreeMap<String, u32>,
}

impl ControlMessage {
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(seq: u32, ch: Channel, payload: Vec<u8>) -> MediaRecord {
        MediaRecord {
            channel: ch,
            flags: MediaFlags { foreign: false, silence_inserted: true },
            seq,
            timestamp_ms: seq as u64 * 1000,
            call_id: *b"0123456789abcdef",
            payload,
        }
    }

    #[test]
    fn record_round_trips() {
        let r = record(842, Channel::Near, vec![1, 2, 3, 4, 5]);
        let bytes = r.encode().unwrap();
        assert_eq!(bytes.len(), HEADER_LEN + 5);
        let (back, used) = MediaRecord::decode(&bytes).unwrap();
        assert_eq!(used, bytes.len());
        assert_eq!(back, r);
    }

    #[test]
    fn header_layout_matches_the_contract() {
        let r = MediaRecord {
            channel: Channel::Near,
            flags: MediaFlags { foreign: true, silence_inserted: false },
            seq: 0x0A0B0C0D,
            timestamp_ms: 0x0102030405060708,
            call_id: [0xAA; 16],
            payload: vec![0xFF, 0xFE],
        };
        let b = r.encode().unwrap();
        assert_eq!(b[0], 1, "version");
        assert_eq!(b[1], 1, "channel near");
        assert_eq!(b[2], 0b01, "foreign flag");
        assert_eq!(b[3], 0, "reserved");
        assert_eq!(&b[4..8], &[0x0D, 0x0C, 0x0B, 0x0A], "seq little-endian");
        assert_eq!(&b[8..16], &[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
        assert_eq!(&b[16..32], &[0xAA; 16]);
        assert_eq!(&b[32..34], &[2, 0], "payload length little-endian");
    }

    #[test]
    fn concatenated_records_decode_in_order() {
        let mut buf = Vec::new();
        for i in 0..SEGMENTS_PER_MESSAGE as u32 {
            record(i, Channel::Far, vec![i as u8; 60]).encode_into(&mut buf).unwrap();
        }
        let all = MediaRecord::decode_all(&buf).unwrap();
        assert_eq!(all.len(), SEGMENTS_PER_MESSAGE);
        assert_eq!(all[3].seq, 3);
        assert_eq!(all[9].payload[0], 9);
    }

    #[test]
    fn truncated_record_is_rejected_not_guessed() {
        let bytes = record(1, Channel::Far, vec![7; 20]).encode().unwrap();
        for cut in [0, 1, HEADER_LEN - 1, HEADER_LEN, HEADER_LEN + 19] {
            assert!(matches!(
                MediaRecord::decode(&bytes[..cut]),
                Err(ProtocolError::Truncated { .. })
            ), "cut at {cut} should be truncated");
        }
    }

    #[test]
    fn reserved_bits_are_rejected() {
        let mut bytes = record(1, Channel::Far, vec![]).encode().unwrap();
        bytes[2] = 0b1000_0000;
        assert!(matches!(MediaRecord::decode(&bytes), Err(ProtocolError::ReservedFlags(_))));
        let mut bytes = record(1, Channel::Far, vec![]).encode().unwrap();
        bytes[3] = 9;
        assert!(matches!(MediaRecord::decode(&bytes), Err(ProtocolError::ReservedByte(9))));
    }

    #[test]
    fn unknown_version_is_rejected() {
        let mut bytes = record(1, Channel::Far, vec![]).encode().unwrap();
        bytes[0] = 2;
        assert_eq!(MediaRecord::decode(&bytes), Err(ProtocolError::Version(2)));
    }

    #[test]
    fn segment_payload_round_trips_including_dropped_frames() {
        let mut frames: Vec<Vec<u8>> = (0..FRAMES_PER_SEGMENT).map(|i| vec![i as u8; 60]).collect();
        frames[7].clear(); // glitch: dropped frame
        let packed = pack_segment(&frames);
        let back = unpack_segment(&packed).unwrap();
        assert_eq!(back.len(), FRAMES_PER_SEGMENT);
        assert!(back[7].is_empty());
        assert_eq!(back, frames);
    }

    #[test]
    fn call_start_serialises_to_the_documented_shape() {
        let msg = ControlMessage::CallStart(CallStart {
            call_id: "01J8ZQ8H2Q7X9K3M4N5P6R7S8T".into(),
            started_at: "2026-09-01T10:14:02.113Z".into(),
            user_uid: "uid".into(),
            device_id: "dev".into(),
            tier: CaptureTier::A,
            account_ref: Some("LN-88213".into()),
            dialer_call_id: None,
            direction: Direction::Outbound,
            codec: "opus".into(),
            rate: 16000,
        });
        let v: serde_json::Value = serde_json::from_str(&msg.to_json().unwrap()).unwrap();
        assert_eq!(v["t"], "call.start");
        assert_eq!(v["tier"], "A");
        assert_eq!(v["direction"], "outbound");
        assert_eq!(v["rate"], 16000);
        assert!(v["dialer_call_id"].is_null());
        assert_eq!(ControlMessage::from_json(&msg.to_json().unwrap()).unwrap(), msg);
    }

    #[test]
    fn ack_and_resume_decode() {
        let ack: ControlMessage =
            serde_json::from_str(r#"{"t":"ack","call_id":"01J","channel":0,"through_seq":840}"#)
                .unwrap();
        assert!(matches!(ack, ControlMessage::Ack { through_seq: 840, channel: 0, .. }));
        let resume: ControlMessage =
            serde_json::from_str(r#"{"t":"resume","call_id":"01J","acked":{"0":840,"1":839}}"#)
                .unwrap();
        match resume {
            ControlMessage::Resume { acked, .. } => {
                assert_eq!(acked["0"], 840);
                assert_eq!(acked["1"], 839);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
