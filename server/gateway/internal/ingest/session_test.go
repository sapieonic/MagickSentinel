package ingest

import (
	"errors"
	"fmt"
	"testing"
	"time"

	"github.com/magickvoice/sentinel/server/gateway/internal/wire"
	"github.com/oklog/ulid/v2"
)

// fakeSink records what ingest asked it to persist, and can be made to fail.
type fakeSink struct {
	calls      map[string]wire.CallStart
	segments   map[string]int // "callID/channel/seq" -> write count
	watermarks map[string]uint32
	ended      map[string]wire.CallEnd
	existing   map[string]map[uint8]uint32
	failPut    bool
	failMark   bool
	failEnsure bool
}

func newSink() *fakeSink {
	return &fakeSink{
		calls: map[string]wire.CallStart{}, segments: map[string]int{},
		watermarks: map[string]uint32{}, ended: map[string]wire.CallEnd{},
		existing: map[string]map[uint8]uint32{},
	}
}

func (f *fakeSink) EnsureCall(cs wire.CallStart, _, _ string) (bool, map[uint8]uint32, error) {
	if f.failEnsure {
		return false, nil, errors.New("boom")
	}
	if acked, ok := f.existing[cs.CallID]; ok {
		return true, acked, nil
	}
	if _, ok := f.calls[cs.CallID]; ok {
		return true, map[uint8]uint32{}, nil
	}
	f.calls[cs.CallID] = cs
	return false, map[uint8]uint32{}, nil
}

func (f *fakeSink) PutSegment(callID string, r wire.MediaRecord) error {
	if f.failPut {
		return errors.New("boom")
	}
	f.segments[fmt.Sprintf("%s/%d/%d", callID, r.Channel, r.Seq)]++
	return nil
}

func (f *fakeSink) SetWatermark(callID string, ch uint8, seq uint32) error {
	if f.failMark {
		return errors.New("boom")
	}
	f.watermarks[fmt.Sprintf("%s/%d", callID, ch)] = seq
	return nil
}

func (f *fakeSink) FinishCall(ce wire.CallEnd) error {
	f.ended[ce.CallID] = ce
	return nil
}

var (
	testCallID = ulid.MustParse("01J8ZQ8H2Q7X9K3M4N5P6R7S8T")
	peer       = Peer{TenantID: "tenant-1", UserUID: "agent-a", DeviceID: "device-1"}
	t0         = time.Date(2026, 9, 1, 10, 0, 0, 0, time.UTC)
)

func callStart() wire.CallStart {
	return wire.CallStart{
		T: wire.KindCallStart, CallID: testCallID.String(),
		StartedAt: t0.Format(time.RFC3339Nano), UserUID: "agent-a", DeviceID: "device-1",
		Tier: "A", Direction: "outbound", Codec: "opus", Rate: 16000,
	}
}

func media(ch uint8, seq uint32) wire.MediaRecord {
	return wire.MediaRecord{
		Channel: ch, Seq: seq, TimestampMS: uint64(seq) * 1000,
		CallID: testCallID, Payload: []byte{byte(seq)},
	}
}

func newSession(sink Sink) *Session {
	return NewSession(peer, sink, Config{}, 7, t0)
}

func TestCallStartOpensACallAndMediaIsStored(t *testing.T) {
	sink := newSink()
	s := newSession(sink)

	if _, err := s.OnCallStart(callStart(), t0); err != nil {
		t.Fatal(err)
	}
	if _, ok := sink.calls[testCallID.String()]; !ok {
		t.Fatal("call row not created")
	}
	for seq := uint32(0); seq < 5; seq++ {
		if _, err := s.OnMedia(media(wire.ChannelFar, seq), t0); err != nil {
			t.Fatal(err)
		}
	}
	if len(sink.segments) != 5 {
		t.Fatalf("stored %d segments, want 5", len(sink.segments))
	}
}

func TestDuplicateSegmentsAreDroppedSilently(t *testing.T) {
	// Idempotency on (call_id, channel, seq) is what makes retry-after-reconnect
	// safe. A duplicate must not be stored twice and must not be an error.
	sink := newSink()
	s := newSession(sink)
	s.OnCallStart(callStart(), t0)

	for seq := uint32(0); seq < 3; seq++ {
		s.OnMedia(media(wire.ChannelFar, seq), t0)
	}
	s.Tick(t0.Add(3 * time.Second)) // ack through 2
	for seq := uint32(0); seq < 3; seq++ {
		out, err := s.OnMedia(media(wire.ChannelFar, seq), t0.Add(4*time.Second))
		if err != nil {
			t.Fatalf("a duplicate must not be an error: %v", err)
		}
		if len(out.Errors) != 0 {
			t.Fatalf("a duplicate must not produce a call.error: %+v", out.Errors)
		}
	}
	for k, n := range sink.segments {
		if n != 1 {
			t.Fatalf("%s written %d times", k, n)
		}
	}
}

func TestAcksAreCumulativeAndHeldBackByGaps(t *testing.T) {
	sink := newSink()
	s := newSession(sink)
	s.OnCallStart(callStart(), t0)

	// 0, 1, then a gap, then 3 and 4.
	for _, seq := range []uint32{0, 1, 3, 4} {
		s.OnMedia(media(wire.ChannelFar, seq), t0)
	}
	out := s.Tick(t0.Add(3 * time.Second))
	if len(out.Acks) != 1 {
		t.Fatalf("expected one ack, got %+v", out.Acks)
	}
	if out.Acks[0].ThroughSeq != 1 {
		t.Fatalf("a gap must hold the watermark at 1, got %d", out.Acks[0].ThroughSeq)
	}

	// Filling the gap releases everything behind it in one jump.
	s.OnMedia(media(wire.ChannelFar, 2), t0.Add(4*time.Second))
	out = s.Tick(t0.Add(7 * time.Second))
	if len(out.Acks) != 1 || out.Acks[0].ThroughSeq != 4 {
		t.Fatalf("expected a cumulative ack through 4, got %+v", out.Acks)
	}
}

func TestChannelsAreAckedIndependently(t *testing.T) {
	sink := newSink()
	s := newSession(sink)
	s.OnCallStart(callStart(), t0)
	for seq := uint32(0); seq < 3; seq++ {
		s.OnMedia(media(wire.ChannelFar, seq), t0)
	}
	s.OnMedia(media(wire.ChannelNear, 0), t0)

	out := s.Tick(t0.Add(3 * time.Second))
	got := map[uint8]uint32{}
	for _, a := range out.Acks {
		got[a.Channel] = a.ThroughSeq
	}
	if got[wire.ChannelFar] != 2 || got[wire.ChannelNear] != 0 {
		t.Fatalf("channels not acked independently: %+v", got)
	}
}

func TestAckFiresOnSegmentCountBeforeTheInterval(t *testing.T) {
	sink := newSink()
	s := newSession(sink)
	s.OnCallStart(callStart(), t0)

	var acks []wire.Ack
	for seq := uint32(0); seq < 250; seq++ {
		out, _ := s.OnMedia(media(wire.ChannelFar, seq), t0) // no time passes at all
		acks = append(acks, out.Acks...)
	}
	if len(acks) < 2 {
		t.Fatalf("expected acks driven by segment count, got %d", len(acks))
	}
	if acks[len(acks)-1].ThroughSeq < 199 {
		t.Fatalf("last ack only reached %d", acks[len(acks)-1].ThroughSeq)
	}
}

func TestReconnectReplaysCallStartAndGetsAResume(t *testing.T) {
	sink := newSink()
	sink.existing[testCallID.String()] = map[uint8]uint32{
		wire.ChannelFar: 840, wire.ChannelNear: 839,
	}
	s := newSession(sink)

	out, err := s.OnCallStart(callStart(), t0)
	if err != nil {
		t.Fatal(err)
	}
	if len(out.Resumes) != 1 {
		t.Fatalf("a known call must be answered with a resume, got %+v", out)
	}
	if out.Resumes[0].Acked["0"] != 840 || out.Resumes[0].Acked["1"] != 839 {
		t.Fatalf("resume carried the wrong watermarks: %+v", out.Resumes[0].Acked)
	}

	// Replaying an already-acked segment after the resume is a no-op.
	s.OnMedia(media(wire.ChannelFar, 500), t0)
	if len(sink.segments) != 0 {
		t.Fatal("a segment below the resume watermark must not be re-stored")
	}
	// The next one after the watermark is stored.
	s.OnMedia(media(wire.ChannelFar, 841), t0)
	if len(sink.segments) != 1 {
		t.Fatalf("expected the post-watermark segment to be stored, got %d", len(sink.segments))
	}
}

func TestCallStartIsNotAnErrorTheSecondTime(t *testing.T) {
	sink := newSink()
	s := newSession(sink)
	s.OnCallStart(callStart(), t0)
	out, err := s.OnCallStart(callStart(), t0.Add(time.Second))
	if err != nil {
		t.Fatalf("replaying call.start is the reconnect path, not an error: %v", err)
	}
	if len(out.Errors) != 0 {
		t.Fatalf("unexpected errors: %+v", out.Errors)
	}
	if len(out.Resumes) != 1 {
		t.Fatalf("expected a resume, got %+v", out)
	}
}

func TestIdentityInFramesIsNeverTrusted(t *testing.T) {
	for _, c := range []struct {
		name string
		cs   func() wire.CallStart
		code string
	}{
		{"another agent's uid", func() wire.CallStart {
			cs := callStart()
			cs.UserUID = "agent-b"
			return cs
		}, wire.CodeUserMismatch},
		{"another device", func() wire.CallStart {
			cs := callStart()
			cs.DeviceID = "device-999"
			return cs
		}, wire.CodeDeviceMismatch},
	} {
		t.Run(c.name, func(t *testing.T) {
			sink := newSink()
			s := newSession(sink)
			out, err := s.OnCallStart(c.cs(), t0)
			if err != nil {
				t.Fatalf("a mismatch is a protocol error, not a transport failure: %v", err)
			}
			if len(out.Errors) != 1 || out.Errors[0].Code != c.code {
				t.Fatalf("expected %s, got %+v", c.code, out.Errors)
			}
			if !out.Errors[0].Fatal {
				t.Error("an identity mismatch must be fatal so the client discards the spool")
			}
			if len(sink.calls) != 0 {
				t.Error("no call row may be created for a mismatched identity")
			}
		})
	}
}

func TestMediaBeforeCallStartIsBufferedThenAccepted(t *testing.T) {
	sink := newSink()
	s := newSession(sink)

	// The binary frame beat the text frame.
	if _, err := s.OnMedia(media(wire.ChannelFar, 0), t0); err != nil {
		t.Fatal(err)
	}
	if len(sink.segments) != 0 {
		t.Fatal("an orphan must not be stored before its call exists")
	}
	s.OnCallStart(callStart(), t0.Add(time.Second))
	if len(sink.segments) != 1 {
		t.Fatalf("the buffered record should be stored once the call opens, got %d", len(sink.segments))
	}
}

func TestOrphansExpireWithAFatalError(t *testing.T) {
	sink := newSink()
	s := newSession(sink)
	s.OnMedia(media(wire.ChannelFar, 0), t0)

	out := s.Tick(t0.Add(31 * time.Second))
	if len(out.Errors) != 1 || out.Errors[0].Code != wire.CodeUnknownCall {
		t.Fatalf("expected an unknown_call error, got %+v", out.Errors)
	}
	if !out.Errors[0].Fatal {
		t.Error("the client must be told to stop retrying audio the server will never accept")
	}
}

func TestCallEndFlushesAcksSoTheClientCanFreeSpool(t *testing.T) {
	sink := newSink()
	s := newSession(sink)
	s.OnCallStart(callStart(), t0)
	for seq := uint32(0); seq < 3; seq++ {
		s.OnMedia(media(wire.ChannelFar, seq), t0)
	}

	end := wire.CallEnd{
		T: wire.KindCallEnd, CallID: testCallID.String(),
		EndedAt: t0.Add(time.Minute).Format(time.RFC3339Nano), Reason: "hangup",
		LastSeq: map[string]uint32{"0": 2},
	}
	out, err := s.OnCallEnd(end, t0.Add(time.Minute))
	if err != nil {
		t.Fatal(err)
	}
	if len(out.Acks) == 0 {
		t.Fatal("call.end must flush acks immediately, not wait out the interval")
	}
	if _, ok := sink.ended[testCallID.String()]; !ok {
		t.Fatal("call.end not recorded")
	}
}

func TestCallEndForAnUnknownCallIsRejected(t *testing.T) {
	s := newSession(newSink())
	out, _ := s.OnCallEnd(wire.CallEnd{CallID: testCallID.String()}, t0)
	if len(out.Errors) != 1 || out.Errors[0].Code != wire.CodeUnknownCall {
		t.Fatalf("expected unknown_call, got %+v", out.Errors)
	}
}

func TestNothingIsAckedThatCouldNotBeRecordedDurable(t *testing.T) {
	// The failure that would lose audio: acking a watermark the database rejected,
	// so the client deletes its only copy.
	sink := newSink()
	s := newSession(sink)
	s.OnCallStart(callStart(), t0)
	for seq := uint32(0); seq < 3; seq++ {
		s.OnMedia(media(wire.ChannelFar, seq), t0)
	}
	sink.failMark = true
	out := s.Tick(t0.Add(3 * time.Second))
	if len(out.Acks) != 0 {
		t.Fatalf("acked despite a failed watermark write: %+v", out.Acks)
	}

	// Once the write succeeds, the ack comes through on the next interval.
	sink.failMark = false
	out = s.Tick(t0.Add(6 * time.Second))
	if len(out.Acks) != 1 || out.Acks[0].ThroughSeq != 2 {
		t.Fatalf("expected recovery to ack through 2, got %+v", out.Acks)
	}
}

func TestAStorageFailureIsReportedNonFatally(t *testing.T) {
	sink := newSink()
	sink.failPut = true
	s := newSession(sink)
	s.OnCallStart(callStart(), t0)

	out, err := s.OnMedia(media(wire.ChannelFar, 0), t0)
	if err == nil {
		t.Fatal("a storage failure must surface to the transport")
	}
	if len(out.Errors) != 1 || out.Errors[0].Fatal {
		t.Fatalf("a transient failure must not be fatal — the client should retry: %+v", out.Errors)
	}
}

func TestTheSameWatermarkIsNotAckedRepeatedly(t *testing.T) {
	sink := newSink()
	s := newSession(sink)
	s.OnCallStart(callStart(), t0)
	s.OnMedia(media(wire.ChannelFar, 0), t0)
	s.Tick(t0.Add(3 * time.Second))

	out := s.Tick(t0.Add(10 * time.Second))
	if len(out.Acks) != 0 {
		t.Fatalf("a quiet channel should not generate repeated acks: %+v", out.Acks)
	}
}

func TestIdleConnectionsAreClosed(t *testing.T) {
	s := newSession(newSink())
	s.OnCallStart(callStart(), t0)
	if out := s.Tick(t0.Add(119 * time.Second)); out.Close != 0 {
		t.Fatal("closed before the idle timeout")
	}
	if out := s.Tick(t0.Add(121 * time.Second)); out.Close != wire.CloseIdle {
		t.Fatalf("expected close %d, got %d", wire.CloseIdle, out.Close)
	}
}

func TestActivityResetsTheIdleTimer(t *testing.T) {
	s := newSession(newSink())
	s.OnCallStart(callStart(), t0)
	s.OnMedia(media(wire.ChannelFar, 0), t0.Add(100*time.Second))
	if out := s.Tick(t0.Add(200 * time.Second)); out.Close != 0 {
		t.Fatal("media traffic should have reset the idle timer")
	}
}

func TestHeartbeatCarriesThePolicyVersion(t *testing.T) {
	s := newSession(newSink())
	out := s.OnHeartbeat(wire.Heartbeat{T: wire.KindHeartbeat}, t0)
	if len(out.Heartbeats) != 1 || out.Heartbeats[0].PolicyVersion != 7 {
		t.Fatalf("expected policy version 7, got %+v", out.Heartbeats)
	}
}

func TestRevocationClosesWithForbidden(t *testing.T) {
	s := newSession(newSink())
	if out := s.Revoke(); out.Close != wire.CloseForbidden {
		t.Fatalf("expected %d, got %d", wire.CloseForbidden, out.Close)
	}
}

func TestTooManyConcurrentCallsIsRefused(t *testing.T) {
	sink := newSink()
	s := NewSession(peer, sink, Config{MaxCallsPerConn: 2}, 1, t0)
	for i := 0; i < 3; i++ {
		cs := callStart()
		cs.CallID = ulid.Make().String()
		out, _ := s.OnCallStart(cs, t0)
		if i < 2 && out.Close != 0 {
			t.Fatalf("call %d refused early", i)
		}
		if i == 2 && out.Close != wire.CloseTooMany {
			t.Fatalf("expected %d, got %d", wire.CloseTooMany, out.Close)
		}
	}
}

func TestForeignSegmentsAreStoredNotDropped(t *testing.T) {
	// Storing them is what lets a reviewer see exactly what was suppressed; the
	// pipeline is what refuses to transcribe them.
	sink := newSink()
	s := newSession(sink)
	s.OnCallStart(callStart(), t0)
	rec := media(wire.ChannelFar, 0)
	rec.Flags.Foreign = true
	if _, err := s.OnMedia(rec, t0); err != nil {
		t.Fatal(err)
	}
	if len(sink.segments) != 1 {
		t.Fatal("foreign segments must still be stored")
	}
}
