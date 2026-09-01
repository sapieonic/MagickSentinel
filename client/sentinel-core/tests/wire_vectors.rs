//! Conformance against `contracts/fixtures/wire_vectors.json`.
//!
//! The gateway runs the same vectors in `server/gateway/internal/wire`. Two
//! independent implementations checked against a shared file is the only thing that
//! actually stops the client and the server drifting apart on a binary protocol —
//! reviewing both sides by eye does not.

use sentinel_core::protocol::{
    pack_segment, Channel, MediaFlags, MediaRecord, ProtocolError,
};
use serde_json::Value;

fn vectors() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../contracts/fixtures/wire_vectors.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture file")).unwrap()
}

fn hex_decode(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
        .collect()
}

fn hex_encode(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[test]
fn media_records_encode_to_the_documented_bytes() {
    for case in vectors()["media_records"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let r = &case["record"];
        let mut call_id = [0u8; 16];
        call_id.copy_from_slice(&hex_decode(r["call_id_hex"].as_str().unwrap()));

        let record = MediaRecord {
            channel: Channel::from_u8(r["channel"].as_u64().unwrap() as u8).unwrap(),
            flags: MediaFlags {
                foreign: r["flags"]["foreign"].as_bool().unwrap(),
                silence_inserted: r["flags"]["silence_inserted"].as_bool().unwrap(),
            },
            seq: r["seq"].as_u64().unwrap() as u32,
            timestamp_ms: r["timestamp_ms"].as_u64().unwrap(),
            call_id,
            payload: hex_decode(r["payload_hex"].as_str().unwrap()),
        };

        let encoded = record.encode().unwrap();
        assert_eq!(
            hex_encode(&encoded),
            case["encoded_hex"].as_str().unwrap(),
            "encoding mismatch for {name}"
        );

        let (decoded, used) = MediaRecord::decode(&encoded).unwrap();
        assert_eq!(used, encoded.len(), "{name}");
        assert_eq!(decoded, record, "round trip mismatch for {name}");
    }
}

#[test]
fn segment_payload_packs_to_the_documented_bytes() {
    let v = vectors();
    let frames: Vec<Vec<u8>> = v["segment_payload"]["frames_hex"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| hex_decode(f.as_str().unwrap()))
        .collect();
    assert_eq!(
        hex_encode(&pack_segment(&frames)),
        v["segment_payload"]["packed_hex"].as_str().unwrap()
    );
}

#[test]
fn invalid_records_are_rejected_with_the_documented_reason() {
    for case in vectors()["invalid"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let bytes = hex_decode(case["hex"].as_str().unwrap());
        let err = MediaRecord::decode(&bytes).expect_err(&format!("{name} should be rejected"));
        let want = case["error"].as_str().unwrap();
        let matched = matches!(
            (&err, want),
            (ProtocolError::Version(_), "version")
                | (ProtocolError::ReservedFlags(_), "reserved_flags")
                | (ProtocolError::ReservedByte(_), "reserved_byte")
                | (ProtocolError::Channel(_), "channel")
                | (ProtocolError::Truncated { .. }, "truncated")
        );
        assert!(matched, "{name}: expected {want}, got {err:?}");
    }
}
