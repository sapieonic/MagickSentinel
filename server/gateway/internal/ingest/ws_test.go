package ingest_test

import (
	"context"
	"encoding/json"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/coder/websocket"
	"github.com/oklog/ulid/v2"

	"github.com/magickvoice/sentinel/server/gateway/internal/auth"
	"github.com/magickvoice/sentinel/server/gateway/internal/blob"
	"github.com/magickvoice/sentinel/server/gateway/internal/ingest"
	"github.com/magickvoice/sentinel/server/gateway/internal/wire"
)

// End-to-end over a real WebSocket: control frames in, media in, acks out. The
// session logic is unit-tested elsewhere; what this file covers is the transport
// wiring, which is where a protocol implementation usually breaks — framing,
// ordering, close codes, and whether the revocation poll actually disconnects a
// device inside the window the spec requires.

type memSink struct {
	mu         sync.Mutex
	blob       *blob.Memory
	segments   map[string]int
	watermarks map[string]uint32
	ended      map[string]bool
	existing   map[string]map[uint8]uint32
}

func newMemSink() *memSink {
	return &memSink{
		blob: blob.NewMemory(), segments: map[string]int{},
		watermarks: map[string]uint32{}, ended: map[string]bool{},
		existing: map[string]map[uint8]uint32{},
	}
}

func (m *memSink) EnsureCall(cs wire.CallStart, _, _ string) (bool, map[uint8]uint32, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	if acked, ok := m.existing[cs.CallID]; ok {
		return true, acked, nil
	}
	m.existing[cs.CallID] = map[uint8]uint32{}
	return false, map[uint8]uint32{}, nil
}

func (m *memSink) PutSegment(callID string, r wire.MediaRecord) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	key := blob.SegmentKey("t", "2026-09-01", callID, r.Channel, r.Seq)
	if err := m.blob.Put(context.Background(), key, r.Payload); err != nil {
		return err
	}
	m.segments[key]++
	return nil
}

func (m *memSink) SetWatermark(callID string, ch uint8, seq uint32) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.watermarks[callID+"/"+string(rune('0'+ch))] = seq
	return nil
}

func (m *memSink) FinishCall(ce wire.CallEnd) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.ended[ce.CallID] = true
	return nil
}

func (m *memSink) storedSegments() int {
	m.mu.Lock()
	defer m.mu.Unlock()
	return len(m.segments)
}

type harness struct {
	t        *testing.T
	server   *httptest.Server
	sink     *memSink
	conn     *websocket.Conn
	callID   ulid.ULID
	revokeMu sync.Mutex
	active   bool

	// A single reader goroutine owns conn.Read. Cancelling a read context in
	// coder/websocket fails the whole connection, so a per-call timeout on Read
	// would tear down the socket the moment a test waited for a frame that had not
	// arrived yet — and every timing assertion below would fail for the wrong
	// reason.
	frames chan []byte
	closed chan error
}

func newHarness(t *testing.T, cfg ingest.Config) *harness {
	t.Helper()
	h := &harness{t: t, sink: newMemSink(), callID: ulid.Make(), active: true}

	handler := &ingest.Handler{
		Log:     slog.New(slog.NewTextHandler(io.Discard, nil)),
		Config:  cfg,
		NewSink: func(ingest.Peer) ingest.Sink { return h.sink },
		PolicyVer: func(context.Context, string) int64 { return 7 },
		DeviceActive: func(context.Context, string, string) bool {
			h.revokeMu.Lock()
			defer h.revokeMu.Unlock()
			return h.active
		},
		RevokePoll: 50 * time.Millisecond,
	}

	// The transport assumes the identity was already established by mTLS plus a
	// bearer token; that middleware is tested in internal/api.
	authed := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		id := &auth.Identity{
			UserUID: "agent-a", TenantID: "tenant-1", Role: auth.RoleAgent,
			DeviceID: "device-1",
		}
		handler.ServeHTTP(w, r.WithContext(auth.WithIdentity(r.Context(), id)))
	})
	h.server = httptest.NewServer(authed)
	t.Cleanup(h.server.Close)

	conn, _, err := websocket.Dial(context.Background(),
		strings.Replace(h.server.URL, "http://", "ws://", 1)+"/v1/ingest",
		&websocket.DialOptions{Subprotocols: []string{"sentinel.v1"}})
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	h.conn = conn
	t.Cleanup(func() { conn.CloseNow() })

	h.frames = make(chan []byte, 64)
	h.closed = make(chan error, 1)
	go func() {
		defer close(h.frames)
		for {
			_, data, err := conn.Read(context.Background())
			if err != nil {
				h.closed <- err
				return
			}
			h.frames <- data
		}
	}()
	return h
}

func (h *harness) revoke() {
	h.revokeMu.Lock()
	h.active = false
	h.revokeMu.Unlock()
}

func (h *harness) sendJSON(v any) {
	h.t.Helper()
	b, _ := json.Marshal(v)
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if err := h.conn.Write(ctx, websocket.MessageText, b); err != nil {
		h.t.Fatalf("write: %v", err)
	}
}

func (h *harness) sendMedia(records ...wire.MediaRecord) {
	h.t.Helper()
	var buf []byte
	for i := range records {
		var err error
		buf, err = records[i].Encode(buf)
		if err != nil {
			h.t.Fatal(err)
		}
	}
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if err := h.conn.Write(ctx, websocket.MessageBinary, buf); err != nil {
		h.t.Fatalf("write media: %v", err)
	}
}

// readUntil collects control frames until pred is satisfied or the deadline passes.
func (h *harness) readUntil(d time.Duration, pred func(map[string]any) bool) map[string]any {
	h.t.Helper()
	deadline := time.After(d)
	for {
		select {
		case data, ok := <-h.frames:
			if !ok {
				return nil
			}
			var m map[string]any
			if json.Unmarshal(data, &m) != nil {
				continue
			}
			if pred(m) {
				return m
			}
		case <-deadline:
			return nil
		}
	}
}

// waitClose returns the close status the server sent, or -1 on timeout.
func (h *harness) waitClose(d time.Duration) websocket.StatusCode {
	h.t.Helper()
	select {
	case err := <-h.closed:
		return websocket.CloseStatus(err)
	case <-time.After(d):
		return -1
	}
}

// eventually polls a condition, for state that becomes true just after a frame the
// test already observed.
func eventually(t *testing.T, d time.Duration, cond func() bool, msg string) {
	t.Helper()
	deadline := time.Now().Add(d)
	for time.Now().Before(deadline) {
		if cond() {
			return
		}
		time.Sleep(10 * time.Millisecond)
	}
	t.Fatal(msg)
}

func (h *harness) callStart() wire.CallStart {
	return wire.CallStart{
		T: wire.KindCallStart, CallID: h.callID.String(),
		StartedAt: time.Now().UTC().Format(time.RFC3339Nano),
		UserUID:   "agent-a", DeviceID: "device-1", Tier: "A",
		Direction: "outbound", Codec: "opus", Rate: 16000,
	}
}

func (h *harness) record(ch uint8, seq uint32) wire.MediaRecord {
	return wire.MediaRecord{
		Channel: ch, Seq: seq, TimestampMS: uint64(seq) * 1000,
		CallID: h.callID, Payload: []byte{byte(seq), 0xAA, 0xBB},
	}
}

// ------------------------------------------------------------------- tests

func TestAFullCallFlowsThroughTheSocket(t *testing.T) {
	h := newHarness(t, ingest.Config{AckInterval: 100 * time.Millisecond})
	h.sendJSON(h.callStart())

	var records []wire.MediaRecord
	for seq := uint32(0); seq < 20; seq++ {
		records = append(records, h.record(wire.ChannelFar, seq))
		records = append(records, h.record(wire.ChannelNear, seq))
	}
	// Ten segments per message, as the client batches them.
	for i := 0; i < len(records); i += 20 {
		h.sendMedia(records[i:min(i+20, len(records))]...)
	}

	ack := h.readUntil(5*time.Second, func(m map[string]any) bool {
		return m["t"] == "ack" && m["through_seq"].(float64) >= 19
	})
	if ack == nil {
		t.Fatal("no ack reached through the last segment")
	}
	if h.sink.storedSegments() != 40 {
		t.Fatalf("stored %d segments, want 40", h.sink.storedSegments())
	}

	h.sendJSON(wire.CallEnd{
		T: wire.KindCallEnd, CallID: h.callID.String(),
		EndedAt: time.Now().UTC().Format(time.RFC3339Nano), Reason: "hangup",
		LastSeq: map[string]uint32{"0": 19, "1": 19},
	})
	eventually(t, 3*time.Second, func() bool {
		h.sink.mu.Lock()
		defer h.sink.mu.Unlock()
		return h.sink.ended[h.callID.String()]
	}, "call.end not recorded")
}

func TestReplayingCallStartYieldsAResumeOverTheSocket(t *testing.T) {
	h := newHarness(t, ingest.Config{AckInterval: 100 * time.Millisecond})
	h.sink.existing[h.callID.String()] = map[uint8]uint32{0: 840, 1: 839}

	h.sendJSON(h.callStart())
	resume := h.readUntil(3*time.Second, func(m map[string]any) bool { return m["t"] == "resume" })
	if resume == nil {
		t.Fatal("no resume for a known call")
	}
	acked := resume["acked"].(map[string]any)
	if acked["0"].(float64) != 840 || acked["1"].(float64) != 839 {
		t.Fatalf("wrong watermarks: %v", acked)
	}
}

func TestAnIdentityMismatchIsReportedAsFatal(t *testing.T) {
	h := newHarness(t, ingest.Config{})
	cs := h.callStart()
	cs.UserUID = "agent-b"
	h.sendJSON(cs)

	msg := h.readUntil(3*time.Second, func(m map[string]any) bool { return m["t"] == "call.error" })
	if msg == nil {
		t.Fatal("no call.error for a mismatched user")
	}
	if msg["code"] != wire.CodeUserMismatch || msg["fatal"] != true {
		t.Fatalf("unexpected error: %v", msg)
	}
}

func TestAMalformedMediaFrameClosesTheConnection(t *testing.T) {
	// A frame we cannot parse means the two sides disagree about the protocol.
	// Nothing useful can be salvaged, so the connection closes and the client
	// reconnects cleanly rather than dribbling corrupt audio.
	h := newHarness(t, ingest.Config{})
	h.sendJSON(h.callStart())

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if err := h.conn.Write(ctx, websocket.MessageBinary, []byte{0xFF, 0x00, 0x01}); err != nil {
		t.Fatalf("write: %v", err)
	}
	if got := h.waitClose(5 * time.Second); got != websocket.StatusInvalidFramePayloadData {
		t.Fatalf("expected an invalid-frame close, got %d", got)
	}
}

func TestRevokingADeviceDisconnectsItPromptly(t *testing.T) {
	// The spec requires revocation to terminate connections within 60 s. The poll is
	// set to 50 ms here so the mechanism is tested rather than the clock.
	h := newHarness(t, ingest.Config{AckInterval: 50 * time.Millisecond})
	h.sendJSON(h.callStart())
	h.sendMedia(h.record(wire.ChannelFar, 0))
	if h.readUntil(3*time.Second, func(m map[string]any) bool { return m["t"] == "ack" }) == nil {
		t.Fatal("the connection was not established before revocation")
	}

	h.revoke()

	if got := h.waitClose(5 * time.Second); int(got) != wire.CloseForbidden {
		t.Fatalf("expected close %d, got %d", wire.CloseForbidden, got)
	}
}

func TestHeartbeatOverTheSocketReturnsThePolicyVersion(t *testing.T) {
	h := newHarness(t, ingest.Config{})
	h.sendJSON(wire.Heartbeat{
		T: wire.KindHeartbeat, SentAt: time.Now().UTC().Format(time.RFC3339Nano),
		CaptureState: "IN_CALL", SpoolDepth: 12,
	})
	msg := h.readUntil(3*time.Second, func(m map[string]any) bool {
		return m["t"] == "heartbeat.ack"
	})
	if msg == nil || msg["policy_version"].(float64) != 7 {
		t.Fatalf("unexpected heartbeat ack: %v", msg)
	}
}

func TestAnUnknownControlFrameIsIgnoredRatherThanFatal(t *testing.T) {
	// Forward compatibility: the server must be able to add message types without
	// breaking clients that predate them, and vice versa.
	h := newHarness(t, ingest.Config{AckInterval: 100 * time.Millisecond})
	h.sendJSON(map[string]any{"t": "something.new", "field": 1})
	h.sendJSON(h.callStart())
	h.sendMedia(h.record(wire.ChannelFar, 0))

	if h.readUntil(3*time.Second, func(m map[string]any) bool { return m["t"] == "ack" }) == nil {
		t.Fatal("the connection did not survive an unknown control frame")
	}
}

func TestForeignSegmentsSurviveTheRoundTrip(t *testing.T) {
	h := newHarness(t, ingest.Config{AckInterval: 100 * time.Millisecond})
	h.sendJSON(h.callStart())
	rec := h.record(wire.ChannelFar, 0)
	rec.Flags.Foreign = true
	h.sendMedia(rec)

	if h.readUntil(3*time.Second, func(m map[string]any) bool { return m["t"] == "ack" }) == nil {
		t.Fatal("no ack for a foreign segment")
	}
	// Stored, so a reviewer can be shown exactly what was suppressed. Refusing to
	// transcribe it is the pipeline's job, not the gateway's.
	if h.sink.storedSegments() != 1 {
		t.Fatal("a foreign segment must be stored, not dropped")
	}
}

func min(a, b int) int {
	if a < b {
		return a
	}
	return b
}
