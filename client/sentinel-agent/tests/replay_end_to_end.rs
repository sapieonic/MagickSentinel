//! Spec section 14, integration: `WavReplaySource` drives the full client path —
//! detection, encode, spool, uplink — against a test gateway, with no audio hardware.
//!
//! The gateway here is a real WebSocket server speaking the real wire protocol from
//! `contracts/wire.md`: it decodes the media records byte for byte, acks cumulatively
//! per `(call_id, channel)` the way the contract requires, and can be told to drop the
//! connection mid-call so the resume path is exercised rather than described.

use sentinel_agent::pipeline::Pipeline;
use sentinel_agent::uplink::transport::{ConnectParams, WsTransport};
use sentinel_agent::uplink::{Transport, TransportError, TransportFactory, Uplink};
use sentinel_capture::device::Direction;
use sentinel_capture::replay::WavReplaySource;
use sentinel_capture::SAMPLE_RATE;
use sentinel_core::config::{PinnedDevice, Policy, SpoolLimits};
use sentinel_core::protocol::{CaptureTier, Channel, ControlMessage, MediaRecord};
use sentinel_core::spool::Spool;
use sentinel_core::state::Transition;
use std::collections::BTreeMap;
use std::f32::consts::TAU;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---------------------------------------------------------------- fake gateway

/// What the gateway saw, for the test to assert on.
#[derive(Default)]
struct Received {
    starts: Vec<ControlMessage>,
    ends: Vec<ControlMessage>,
    /// Every `(channel, seq)` the gateway durably "stored", in arrival order.
    stored: Vec<(u8, u32)>,
    /// Highest contiguous sequence per `(call_id, channel)` — what a cumulative ack
    /// is allowed to claim.
    watermark: BTreeMap<(String, u8), u32>,
    /// Payload bytes per `(channel, seq)`, so the test can prove the audio survived
    /// the round trip rather than just the headers.
    payloads: BTreeMap<(u8, u32), Vec<u8>>,
    connections: u32,
}

struct Gateway {
    port: u16,
    received: Arc<Mutex<Received>>,
    /// When set, the gateway closes the connection after storing this many segments,
    /// then serves the next connection normally.
    drop_after: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
}

impl Gateway {
    fn start() -> Gateway {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind gateway");
        let port = listener.local_addr().unwrap().port();
        let received = Arc::new(Mutex::new(Received::default()));
        let drop_after = Arc::new(AtomicU64::new(u64::MAX));
        let stop = Arc::new(AtomicBool::new(false));

        let g = Gateway {
            port,
            received: received.clone(),
            drop_after: drop_after.clone(),
            stop: stop.clone(),
        };

        std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        let received = received.clone();
                        let drop_after = drop_after.clone();
                        std::thread::spawn(move || serve(stream, received, drop_after));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        g
    }

    fn url(&self) -> String {
        format!("ws://127.0.0.1:{}/v1/ingest", self.port)
    }

    fn snapshot(&self) -> std::sync::MutexGuard<'_, Received> {
        self.received.lock().unwrap()
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

#[allow(clippy::result_large_err)] // the handshake callback's error type is tungstenite's
fn serve(stream: TcpStream, received: Arc<Mutex<Received>>, drop_after: Arc<AtomicU64>) {
    // The real gateway requires the bearer token and the sub-protocol; check both, so
    // a client that stops sending them fails the test rather than passing quietly.
    let mut saw_auth = false;
    let mut saw_protocol = false;
    let callback = |req: &tungstenite::handshake::server::Request,
                    mut resp: tungstenite::handshake::server::Response| {
        saw_auth = req
            .headers()
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("Bearer "));
        saw_protocol = req
            .headers()
            .get("Sec-WebSocket-Protocol")
            .and_then(|v| v.to_str().ok())
            == Some("sentinel.v1");
        resp.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            "sentinel.v1".parse().unwrap(),
        );
        Ok(resp)
    };

    let Ok(mut ws) = tungstenite::accept_hdr(stream, callback) else { return };
    assert!(saw_auth, "the gateway requires Authorization: Bearer");
    assert!(saw_protocol, "the gateway requires the sentinel.v1 sub-protocol");

    {
        let mut r = received.lock().unwrap();
        r.connections += 1;
    }

    let mut stored_this_connection = 0u64;
    let mut resumed: std::collections::BTreeSet<String> = Default::default();

    loop {
        let Ok(msg) = ws.read() else { return };
        match msg {
            tungstenite::Message::Text(text) => {
                let Ok(control) = ControlMessage::from_json(&text) else { continue };
                match &control {
                    ControlMessage::CallStart(start) => {
                        let known = {
                            let mut r = received.lock().unwrap();
                            let known = r
                                .starts
                                .iter()
                                .any(|s| matches!(s, ControlMessage::CallStart(p) if p.call_id == start.call_id));
                            r.starts.push(control.clone());
                            known
                        };
                        // wire.md 3.1: a repeated call.start is the reconnect path;
                        // reply with `resume`, not a new call row.
                        if known && resumed.insert(start.call_id.clone()) {
                            let acked: BTreeMap<String, u32> = {
                                let r = received.lock().unwrap();
                                r.watermark
                                    .iter()
                                    .filter(|((c, _), _)| c == &start.call_id)
                                    .map(|((_, ch), v)| (ch.to_string(), *v))
                                    .collect()
                            };
                            let resume = ControlMessage::Resume {
                                call_id: start.call_id.clone(),
                                acked,
                            };
                            let _ = ws.send(tungstenite::Message::Text(
                                resume.to_json().unwrap().into(),
                            ));
                        }
                    }
                    ControlMessage::CallEnd(_) => {
                        received.lock().unwrap().ends.push(control.clone());
                    }
                    _ => {}
                }
            }
            tungstenite::Message::Binary(bytes) => {
                let records = MediaRecord::decode_all(&bytes).expect("well-formed media message");
                let mut acks: Vec<ControlMessage> = Vec::new();
                {
                    let mut r = received.lock().unwrap();
                    for rec in &records {
                        let call_id = ulid::Ulid::from_bytes(rec.call_id).to_string();
                        let ch = rec.channel.as_u8();
                        r.stored.push((ch, rec.seq));
                        r.payloads.insert((ch, rec.seq), rec.payload.clone());
                        let entry = r.watermark.entry((call_id.clone(), ch)).or_insert(0);
                        // Cumulative, and only over a contiguous run: the contract
                        // lets an ack claim everything at or below `through_seq`, so a
                        // gateway that advanced over a hole would tell the client to
                        // delete audio it never received.
                        if rec.seq <= entry.saturating_add(1) {
                            *entry = (*entry).max(rec.seq);
                        }
                        acks.push(ControlMessage::Ack {
                            call_id,
                            channel: ch,
                            through_seq: *entry,
                        });
                    }
                    stored_this_connection += records.len() as u64;
                }
                if stored_this_connection >= drop_after.load(Ordering::SeqCst) {
                    // Sever the link mid-call without a close frame, the way a dropped
                    // Wi-Fi link does.
                    drop_after.store(u64::MAX, Ordering::SeqCst);
                    return;
                }
                for ack in acks {
                    let _ = ws.send(tungstenite::Message::Text(ack.to_json().unwrap().into()));
                }
            }
            tungstenite::Message::Close(_) => return,
            _ => {}
        }
    }
}

// -------------------------------------------------------------------- fixtures

/// Voiced audio with an amplitude envelope, loud enough for the VAD.
fn speech(ms: usize) -> Vec<i16> {
    let n = ms * SAMPLE_RATE as usize / 1000;
    (0..n)
        .map(|i| {
            let t = i as f32 / SAMPLE_RATE as f32;
            let env = 0.35 + 0.15 * (TAU * 2.5 * t).sin();
            (env * i16::MAX as f32 * (TAU * 250.0 * t).sin()) as i16
        })
        .collect()
}

fn quiet(ms: usize) -> Vec<i16> {
    vec![0i16; ms * SAMPLE_RATE as usize / 1000]
}

/// A WAV fixture on disk, so the test exercises `WavReplaySource::add_wav` rather than
/// only the in-memory shortcut.
fn write_wav(dir: &std::path::Path, name: &str, samples: &[i16]) -> std::path::PathBuf {
    let path = dir.join(name);
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(&path, spec).unwrap();
    for s in samples {
        w.write_sample(*s).unwrap();
    }
    w.finalize().unwrap();
    path
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

struct WsFactory {
    url: String,
}

impl TransportFactory for WsFactory {
    fn connect(&mut self) -> Result<Box<dyn Transport>, TransportError> {
        Ok(Box::new(WsTransport::connect(&ConnectParams {
            url: self.url.clone(),
            bearer_token: "test-id-token".into(),
            // No client certificate: the loopback test gateway does not do TLS, and
            // `validate_url` only permits ws:// to loopback for exactly this reason.
            client_cert: None,
        })?))
    }
}

/// Run the replay pipeline until the call ends or the source runs out.
fn drive(
    pipeline: &mut Pipeline,
    source: &mut WavReplaySource,
    uplink: &mut Uplink,
    steps: usize,
    hangup_at: usize,
) -> (bool, bool, Option<String>) {
    let mut started = false;
    let mut ended = false;
    let mut call_id = None;
    let mut now = 0u64;

    pipeline.on_session_state(true);
    for i in 0..steps {
        if i == hangup_at {
            pipeline.on_session_state(false);
        }
        let r = pipeline.step(source, uplink).expect("pipeline step");
        if r.call_id.is_some() {
            call_id = r.call_id.clone();
        }
        match r.transition {
            Some(Transition::CallStarted) => started = true,
            Some(Transition::CallEnded(_)) => ended = true,
            _ => {}
        }
        // The uplink advances on the same tick as capture, as it does in the agent.
        uplink.pump(now);
        now += 20;
        if ended && uplink.stats().segments == 0 {
            break;
        }
    }
    (started, ended, call_id)
}

// ----------------------------------------------------------------------- tests

#[test]
fn a_replayed_call_reaches_the_gateway_and_the_spool_empties_on_ack() {
    let gateway = Gateway::start();
    let dir = tempfile::tempdir().unwrap();

    // A borrower who speaks, then a hangup: 4 s of speech, then silence long enough
    // for the 8 s hangup window plus the 3 s wrap.
    let far = write_wav(dir.path(), "far.wav", &[quiet(200), speech(4000), quiet(14_000)].concat());
    let near = write_wav(dir.path(), "near.wav", &[quiet(500), speech(3000), quiet(14_700)].concat());

    let mut source = WavReplaySource::new().without_realtime_pacing();
    source
        .add_wav("ep-render", "cont-headset", "Jabra Evolve 20", Direction::Render, &far)
        .unwrap();
    source
        .add_wav("ep-capture", "cont-headset", "Jabra Evolve 20", Direction::Capture, &near)
        .unwrap();

    let spool = Spool::open_in_memory(SpoolLimits::default()).unwrap();
    // A short receive slice: these tests drive thousands of passes over a loopback
    // socket, where the production 50 ms wait would dominate the runtime.
    let mut uplink = Uplink::new(spool, Box::new(WsFactory { url: gateway.url() }))
        .with_recv_slice(Duration::from_millis(2));
    let mut pipeline =
        Pipeline::new(&policy(), CaptureTier::A, "device-1".into(), "uid-agent-a".into()).unwrap();
    pipeline.open(&mut source, &policy()).unwrap();

    let (started, ended, call_id) = drive(&mut pipeline, &mut source, &mut uplink, 1200, 260);
    assert!(started, "the far-channel VAD must confirm the call");
    assert!(ended, "an Inactive session plus 8 s of silence must end it");
    let call_id = call_id.expect("a call id was minted");

    // Let the last acks land.
    for i in 0..200 {
        uplink.pump(30_000 + i * 50);
        if uplink.stats().segments == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    let seen = gateway.snapshot();
    assert_eq!(seen.connections, 1);
    assert_eq!(seen.starts.len(), 1, "one call.start for one call");
    match &seen.starts[0] {
        ControlMessage::CallStart(s) => {
            assert_eq!(s.call_id, call_id);
            assert_eq!(s.user_uid, "uid-agent-a", "attribution is stamped at call.start");
            assert_eq!(s.device_id, "device-1");
            assert_eq!(s.codec, "opus");
            assert_eq!(s.rate, 16_000);
            assert_eq!(s.tier, CaptureTier::A);
            // wire.md: a client-generated ULID, 26 Crockford base32 characters.
            assert_eq!(s.call_id.len(), 26);
            assert!(s.call_id.parse::<ulid::Ulid>().is_ok());
        }
        other => panic!("expected call.start, got {other:?}"),
    }
    assert_eq!(seen.ends.len(), 1, "one call.end");

    // Both channels arrived, separately, with real Opus payloads.
    let far_seen: Vec<u32> = seen.stored.iter().filter(|(c, _)| *c == 0).map(|(_, s)| *s).collect();
    let near_seen: Vec<u32> = seen.stored.iter().filter(|(c, _)| *c == 1).map(|(_, s)| *s).collect();
    assert!(!far_seen.is_empty(), "the far channel reached the gateway");
    assert!(!near_seen.is_empty(), "the near channel reached the gateway");
    assert_eq!(far_seen[0], 0, "sequences start at 0 per (call_id, channel)");
    assert_eq!(near_seen[0], 0);

    // Each payload is 50 length-delimited Opus packets, per wire.md 4.1.
    for ((_, _), payload) in seen.payloads.iter() {
        let frames = sentinel_core::protocol::unpack_segment(payload).unwrap();
        assert_eq!(frames.len(), 50, "a segment is 50 frames of 20 ms");
    }

    drop(seen);
    assert_eq!(
        uplink.stats().segments,
        0,
        "every acked segment is deleted from the spool, and only acked ones"
    );
}

#[test]
fn a_connection_dropped_mid_call_resumes_without_losing_or_duplicating_audio() {
    // Spec 14, chaos: sever the network mid-call; verify no data loss and correct
    // resume.
    let gateway = Gateway::start();
    gateway.drop_after.store(6, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();

    // The fixtures open with a moment of silence, as real capture does: the VAD's
    // noise floor initialises from the first frame it sees, so a stream that begins
    // mid-word teaches it that speech is the background level.
    let far = write_wav(dir.path(), "far.wav", &[quiet(400), speech(9000), quiet(14_000)].concat());
    let near = write_wav(dir.path(), "near.wav", &[quiet(400), speech(9000), quiet(14_000)].concat());

    let mut source = WavReplaySource::new().without_realtime_pacing();
    source
        .add_wav("ep-render", "cont-headset", "Jabra Evolve 20", Direction::Render, &far)
        .unwrap();
    source
        .add_wav("ep-capture", "cont-headset", "Jabra Evolve 20", Direction::Capture, &near)
        .unwrap();

    let spool = Spool::open_in_memory(SpoolLimits::default()).unwrap();
    // A short receive slice: these tests drive thousands of passes over a loopback
    // socket, where the production 50 ms wait would dominate the runtime.
    let mut uplink = Uplink::new(spool, Box::new(WsFactory { url: gateway.url() }))
        .with_recv_slice(Duration::from_millis(2));
    let mut pipeline =
        Pipeline::new(&policy(), CaptureTier::B, "device-1".into(), "uid-agent-a".into()).unwrap();
    pipeline.open(&mut source, &policy()).unwrap();

    let (started, _ended, call_id) = drive(&mut pipeline, &mut source, &mut uplink, 1200, 500);
    assert!(started);
    let call_id = call_id.expect("a call id was minted");

    // Drain: the backoff has full jitter with a 60 s cap, so the clock is advanced
    // rather than slept through.
    let mut clock = 60_000u64;
    for _ in 0..400 {
        uplink.pump(clock);
        clock += 61_000;
        if uplink.stats().segments == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    let seen = gateway.snapshot();
    assert!(seen.connections >= 2, "the drop forced a reconnect");
    assert!(
        seen.starts.len() >= 2,
        "call.start is replayed verbatim on reconnect so the server answers with resume"
    );
    for s in &seen.starts {
        match s {
            ControlMessage::CallStart(p) => assert_eq!(
                p.call_id, call_id,
                "the same client-generated call_id, which is what makes retry idempotent"
            ),
            other => panic!("{other:?}"),
        }
    }

    // No gaps on either channel: every sequence the client produced arrived at least
    // once, contiguously from 0.
    for channel in [0u8, 1u8] {
        let mut seqs: Vec<u32> = seen
            .stored
            .iter()
            .filter(|(c, _)| *c == channel)
            .map(|(_, s)| *s)
            .collect();
        seqs.sort_unstable();
        seqs.dedup();
        assert!(!seqs.is_empty(), "channel {channel} produced nothing");
        assert_eq!(seqs[0], 0, "channel {channel} starts at 0");
        for w in seqs.windows(2) {
            assert_eq!(w[1], w[0] + 1, "channel {channel} has a hole between {} and {}", w[0], w[1]);
        }
    }

    drop(seen);
    assert_eq!(uplink.stats().segments, 0, "everything was eventually acked");
}

#[test]
fn a_crashed_agents_spool_uploads_when_it_restarts() {
    // Spec 14, chaos: kill the agent mid-call; verify spooled segments upload on
    // restart. The spool is the only thing that survives, so the restarted agent has
    // to rebuild its work list from it.
    let dir = tempfile::tempdir().unwrap();
    let spool_path = dir.path().join("spool.db");
    let call_id = ulid::Ulid::new().to_string();

    // First "process": capture, spool, never connect.
    {
        struct Offline;
        impl TransportFactory for Offline {
            fn connect(&mut self) -> Result<Box<dyn Transport>, TransportError> {
                Err(TransportError::Connect("offline".into()))
            }
        }
        let spool = Spool::open(&spool_path, "test", SpoolLimits::default()).unwrap();
        let mut uplink = Uplink::new(spool, Box::new(Offline)).with_recv_slice(Duration::from_millis(2));
        let start = ControlMessage::CallStart(sentinel_core::protocol::CallStart {
            call_id: call_id.clone(),
            started_at: "2026-09-01T10:14:02.113Z".into(),
            user_uid: "uid-agent-a".into(),
            device_id: "device-1".into(),
            tier: CaptureTier::A,
            account_ref: None,
            dialer_call_id: None,
            direction: sentinel_core::protocol::Direction::Outbound,
            codec: "opus".into(),
            rate: 16_000,
        });
        uplink.begin_call(&call_id, &start, 0).unwrap();
        for seq in 0..5u32 {
            for channel in [Channel::Far, Channel::Near] {
                uplink
                    .push_segment(&sentinel_core::spool::SegmentRow {
                        call_id: call_id.clone(),
                        channel,
                        seq,
                        timestamp_ms: seq as u64 * 1000,
                        flags: Default::default(),
                        payload: vec![seq as u8; 120],
                        created_ms: seq as u64 * 1000,
                    })
                    .unwrap();
            }
        }
        uplink.pump(0);
        assert_eq!(uplink.stats().segments, 10, "nothing uploaded while offline");
    }

    // Second "process": a fresh Uplink over the same database, nothing in memory.
    let gateway = Gateway::start();
    let spool = Spool::open(&spool_path, "test", SpoolLimits::default()).unwrap();
    // A short receive slice: these tests drive thousands of passes over a loopback
    // socket, where the production 50 ms wait would dominate the runtime.
    let mut uplink = Uplink::new(spool, Box::new(WsFactory { url: gateway.url() }))
        .with_recv_slice(Duration::from_millis(2));
    for i in 0..200 {
        uplink.pump(i * 50);
        if uplink.stats().segments == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    assert_eq!(uplink.stats().segments, 0, "the recovered spool drained");
    let seen = gateway.snapshot();
    assert_eq!(seen.starts.len(), 1, "the stored call.start was replayed verbatim");
    match &seen.starts[0] {
        ControlMessage::CallStart(s) => {
            assert_eq!(s.call_id, call_id);
            assert_eq!(s.user_uid, "uid-agent-a", "attribution survived the crash");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(seen.stored.len(), 10, "both channels, all five seconds");
}

#[test]
fn the_gateway_sees_no_pii_on_the_control_channel() {
    // Spec 12.10. `call.start` carries `account_ref`, which is a borrower's loan
    // reference and is genuinely part of the contract — but nothing else on the
    // socket may carry call content, and with no UIA scrape configured the field is
    // null rather than fabricated.
    let gateway = Gateway::start();
    let dir = tempfile::tempdir().unwrap();
    let far = write_wav(dir.path(), "far.wav", &[quiet(400), speech(3000), quiet(14_000)].concat());
    let near = write_wav(dir.path(), "near.wav", &[quiet(400), speech(2000), quiet(15_000)].concat());

    let mut source = WavReplaySource::new().without_realtime_pacing();
    source
        .add_wav("ep-render", "cont-headset", "Jabra Evolve 20", Direction::Render, &far)
        .unwrap();
    source
        .add_wav("ep-capture", "cont-headset", "Jabra Evolve 20", Direction::Capture, &near)
        .unwrap();

    let spool = Spool::open_in_memory(SpoolLimits::default()).unwrap();
    // A short receive slice: these tests drive thousands of passes over a loopback
    // socket, where the production 50 ms wait would dominate the runtime.
    let mut uplink = Uplink::new(spool, Box::new(WsFactory { url: gateway.url() }))
        .with_recv_slice(Duration::from_millis(2));
    let mut pipeline =
        Pipeline::new(&policy(), CaptureTier::A, "device-1".into(), "uid-agent-a".into()).unwrap();
    pipeline.open(&mut source, &policy()).unwrap();
    drive(&mut pipeline, &mut source, &mut uplink, 900, 200);

    let seen = gateway.snapshot();
    for msg in seen.starts.iter().chain(seen.ends.iter()) {
        let json = msg.to_json().unwrap();
        for forbidden in ["borrower", "transcript", "amount", "display_name", "email"] {
            assert!(!json.contains(forbidden), "control frame leaked {forbidden}: {json}");
        }
    }
    match seen.starts.first() {
        Some(ControlMessage::CallStart(s)) => {
            assert_eq!(s.account_ref, None, "no UIA selector configured means null, not a guess");
        }
        other => panic!("expected a call.start, got {other:?}"),
    }
}

/// Silence a warning about the unused import in configurations where the helper is
/// not exercised; `Write` is used by `hound`'s writer through the trait.
#[allow(dead_code)]
fn _uses_write(w: &mut dyn Write) {
    let _ = w;
}
