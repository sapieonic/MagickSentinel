// Package ingest implements the server side of the WSS ingest protocol.
//
// The protocol logic lives in Session, which is pure: it takes decoded frames and
// returns the messages to send and the writes to perform. The WebSocket and database
// plumbing wrap it. That split is deliberate — idempotency, resume and the identity
// checks are the parts most likely to be got wrong, and they are the parts easiest to
// test when they do not need a socket or a database to run.
package ingest

import (
	"errors"
	"fmt"
	"time"

	"github.com/magickvoice/sentinel/server/gateway/internal/wire"
)

// Sink is the durable side of ingest. Implementations write to object storage and
// Postgres; tests use an in-memory double.
type Sink interface {
	// EnsureCall creates the call row if it does not exist. It reports whether the
	// call already existed, which is how a reconnect is distinguished from a new
	// call, and the per-channel ack watermark to resume from.
	EnsureCall(cs wire.CallStart, tenantID, deviceID string) (existed bool, acked map[uint8]uint32, err error)
	// PutSegment stores one segment. It MUST be idempotent on
	// (call_id, channel, seq): duplicates are dropped silently and still count
	// toward the next ack.
	PutSegment(callID string, r wire.MediaRecord) error
	// SetWatermark advances the durable cumulative ack.
	SetWatermark(callID string, channel uint8, throughSeq uint32) error
	// FinishCall records call.end.
	FinishCall(ce wire.CallEnd) error
}

// Config tunes the acknowledgement cadence. The defaults are the contract's: ack at
// least every 2 s or every 100 segments per channel, whichever comes first.
type Config struct {
	AckInterval    time.Duration
	AckEverySegs   uint32
	IdleTimeout    time.Duration
	OrphanGrace    time.Duration
	MaxCallsPerConn int
}

func (c Config) withDefaults() Config {
	if c.AckInterval == 0 {
		c.AckInterval = 2 * time.Second
	}
	if c.AckEverySegs == 0 {
		c.AckEverySegs = 100
	}
	if c.IdleTimeout == 0 {
		c.IdleTimeout = 120 * time.Second
	}
	if c.OrphanGrace == 0 {
		c.OrphanGrace = 30 * time.Second
	}
	if c.MaxCallsPerConn == 0 {
		c.MaxCallsPerConn = 64
	}
	return c
}

// Peer is the authenticated other end of the socket. Both identities are established
// before the Session exists: the device from the client certificate, the user from
// the bearer token. Nothing in a frame can change them.
type Peer struct {
	TenantID string
	UserUID  string
	DeviceID string
}

// Outbound is what the transport should send.
type Outbound struct {
	Acks       []wire.Ack
	Resumes    []wire.Resume
	Errors     []wire.CallError
	Heartbeats []wire.HeartbeatAck
	// Close, when non-zero, is a close code the transport must apply after
	// flushing the messages above.
	Close       int
	CloseReason string
}

func (o *Outbound) empty() bool {
	return len(o.Acks) == 0 && len(o.Resumes) == 0 && len(o.Errors) == 0 &&
		len(o.Heartbeats) == 0 && o.Close == 0
}

type channelState struct {
	// through is the highest contiguous sequence stored. Acks are cumulative, so
	// this only advances when the gap in front of it is filled.
	through uint32
	// started is false until sequence 0 has been seen, so an empty channel is
	// distinguishable from one acked through 0.
	started bool
	// ahead holds sequences received out of order, waiting for the gap to fill.
	ahead map[uint32]bool
	// sinceAck counts segments stored since the last ack was emitted.
	sinceAck uint32
	lastAck  time.Time
	// acked is the last value actually sent, so a repeated ack is not re-sent.
	acked        uint32
	everAcked    bool
}

type callState struct {
	start     wire.CallStart
	channels  map[uint8]*channelState
	ended     bool
	firstSeen time.Time
}

// Session is one ingest connection.
type Session struct {
	peer  Peer
	sink  Sink
	cfg   Config
	calls map[string]*callState
	// orphans holds records that arrived before their call.start, keyed by call id.
	orphans     map[string][]orphan
	policyVer   int64
	lastActivity time.Time
}

type orphan struct {
	rec wire.MediaRecord
	at  time.Time
}

func NewSession(peer Peer, sink Sink, cfg Config, policyVersion int64, now time.Time) *Session {
	return &Session{
		peer:         peer,
		sink:         sink,
		cfg:          cfg.withDefaults(),
		calls:        map[string]*callState{},
		orphans:      map[string][]orphan{},
		policyVer:    policyVersion,
		lastActivity: now,
	}
}

var errUnknownCall = errors.New("unknown call")

// OnCallStart handles a call.start frame.
//
// Re-sending call.start for a known call is not an error; it is the reconnect path,
// and the reply is a resume rather than a new call row.
func (s *Session) OnCallStart(cs wire.CallStart, now time.Time) (Outbound, error) {
	var out Outbound
	s.lastActivity = now

	if err := s.checkIdentity(cs.CallID, cs.UserUID, cs.DeviceID, &out); err != nil {
		return out, nil
	}
	if len(s.calls) >= s.cfg.MaxCallsPerConn {
		out.Close = wire.CloseTooMany
		out.CloseReason = "too many concurrent calls on one connection"
		return out, nil
	}

	existed, acked, err := s.sink.EnsureCall(cs, s.peer.TenantID, s.peer.DeviceID)
	if err != nil {
		out.Errors = append(out.Errors, wire.CallError{
			T: wire.KindCallError, CallID: cs.CallID, Code: wire.CodeInternal,
			Message: "could not open the call", Fatal: false,
		})
		return out, err
	}

	st, known := s.calls[cs.CallID]
	if !known {
		st = &callState{start: cs, channels: map[uint8]*channelState{}, firstSeen: now}
		s.calls[cs.CallID] = st
	}
	for ch, through := range acked {
		cst := s.channel(st, ch)
		cst.through, cst.started = through, true
		cst.acked, cst.everAcked = through, true
	}

	if existed {
		out.Resumes = append(out.Resumes, wire.Resume{
			T: wire.KindResume, CallID: cs.CallID, Acked: watermarkMap(st),
		})
	}

	// Any records that raced ahead of this call.start can now be stored.
	if pending := s.orphans[cs.CallID]; len(pending) > 0 {
		delete(s.orphans, cs.CallID)
		for _, o := range pending {
			if err := s.storeRecord(st, o.rec, now, &out); err != nil {
				return out, err
			}
		}
	}
	s.emitDueAcks(now, &out)
	return out, nil
}

// OnMedia handles one decoded media record.
func (s *Session) OnMedia(rec wire.MediaRecord, now time.Time) (Outbound, error) {
	var out Outbound
	s.lastActivity = now
	callID := formatULID(rec.CallID)

	st, ok := s.calls[callID]
	if !ok {
		// Buffer briefly: on a lossy link the binary frame can beat the text frame
		// that opens the call.
		s.orphans[callID] = append(s.orphans[callID], orphan{rec, now})
		s.expireOrphans(now, &out)
		return out, nil
	}
	if err := s.storeRecord(st, rec, now, &out); err != nil {
		return out, err
	}
	s.emitDueAcks(now, &out)
	return out, nil
}

// OnCallEnd handles a call.end frame.
func (s *Session) OnCallEnd(ce wire.CallEnd, now time.Time) (Outbound, error) {
	var out Outbound
	s.lastActivity = now

	st, ok := s.calls[ce.CallID]
	if !ok {
		out.Errors = append(out.Errors, wire.CallError{
			T: wire.KindCallError, CallID: ce.CallID, Code: wire.CodeUnknownCall,
			Message: "no call.start for this call", Fatal: true,
		})
		return out, nil
	}
	if err := s.sink.FinishCall(ce); err != nil {
		out.Errors = append(out.Errors, wire.CallError{
			T: wire.KindCallError, CallID: ce.CallID, Code: wire.CodeInternal,
			Message: "could not finalize the call", Fatal: false,
		})
		return out, err
	}
	st.ended = true
	// Ack everything durable now, so the client can free spool immediately rather
	// than waiting out the interval on a connection that is about to go quiet.
	s.flushAcks(now, &out)
	return out, nil
}

// OnHeartbeat replies with the server clock and the current policy version, which is
// how a client learns its policy is stale without polling.
func (s *Session) OnHeartbeat(_ wire.Heartbeat, now time.Time) Outbound {
	s.lastActivity = now
	s.expireOrphans(now, &Outbound{})
	return Outbound{Heartbeats: []wire.HeartbeatAck{{
		T:             wire.KindHeartbeatAck,
		ServerTime:    now.UTC().Format(time.RFC3339Nano),
		PolicyVersion: s.policyVer,
	}}}
}

// Tick drives time-based work: the ack interval, the orphan grace period and the
// idle timeout.
func (s *Session) Tick(now time.Time) Outbound {
	var out Outbound
	s.emitDueAcks(now, &out)
	s.expireOrphans(now, &out)
	if now.Sub(s.lastActivity) >= s.cfg.IdleTimeout {
		out.Close = wire.CloseIdle
		out.CloseReason = "no frames received"
	}
	return out
}

// Revoke terminates the connection because the device was revoked. The portal
// requires this to take effect within 60 s, which the caller enforces by polling
// device status and calling here.
func (s *Session) Revoke() Outbound {
	return Outbound{Close: wire.CloseForbidden, CloseReason: "device revoked"}
}

// ---------------------------------------------------------------- internals

func (s *Session) checkIdentity(callID, userUID, deviceID string, out *Outbound) error {
	// Never trust identity from the frame. These fields exist so a mismatch is
	// detectable, not so the client can assert who it is.
	if userUID != s.peer.UserUID {
		out.Errors = append(out.Errors, wire.CallError{
			T: wire.KindCallError, CallID: callID, Code: wire.CodeUserMismatch,
			Message: "call.start user does not match the bearer token", Fatal: true,
		})
		return errors.New("user mismatch")
	}
	if deviceID != s.peer.DeviceID {
		out.Errors = append(out.Errors, wire.CallError{
			T: wire.KindCallError, CallID: callID, Code: wire.CodeDeviceMismatch,
			Message: "call.start device does not match the client certificate", Fatal: true,
		})
		return errors.New("device mismatch")
	}
	return nil
}

func (s *Session) channel(st *callState, ch uint8) *channelState {
	cst, ok := st.channels[ch]
	if !ok {
		cst = &channelState{ahead: map[uint32]bool{}}
		st.channels[ch] = cst
	}
	return cst
}

func (s *Session) storeRecord(st *callState, rec wire.MediaRecord, now time.Time, out *Outbound) error {
	cst := s.channel(st, rec.Channel)

	// Already durable: drop silently, per the contract. It still counts toward the
	// next ack so a client stuck resending an acked range gets told again.
	if cst.started && rec.Seq <= cst.through {
		cst.sinceAck++
		return nil
	}
	if err := s.sink.PutSegment(formatULID(rec.CallID), rec); err != nil {
		out.Errors = append(out.Errors, wire.CallError{
			T: wire.KindCallError, CallID: formatULID(rec.CallID), Code: wire.CodeInternal,
			Message: "could not store the segment", Fatal: false,
		})
		return err
	}
	cst.sinceAck++
	s.advance(cst, rec.Seq)
	return nil
}

// advance moves the contiguous watermark forward, absorbing anything buffered ahead
// of it. Acks are cumulative, so a gap holds the watermark back even though the later
// segments are already durable — which is correct: the client must keep them until we
// can say everything below is safe.
func (s *Session) advance(cst *channelState, seq uint32) {
	if !cst.started {
		if seq == 0 {
			cst.through, cst.started = 0, true
		} else {
			cst.ahead[seq] = true
			return
		}
	} else if seq == cst.through+1 {
		cst.through = seq
	} else {
		cst.ahead[seq] = true
		return
	}
	for cst.ahead[cst.through+1] {
		delete(cst.ahead, cst.through+1)
		cst.through++
	}
}

func (s *Session) emitDueAcks(now time.Time, out *Outbound) {
	for callID, st := range s.calls {
		for ch, cst := range st.channels {
			if !cst.started {
				continue
			}
			due := cst.sinceAck >= s.cfg.AckEverySegs ||
				(!cst.lastAck.IsZero() && now.Sub(cst.lastAck) >= s.cfg.AckInterval) ||
				cst.lastAck.IsZero()
			if !due {
				continue
			}
			s.ackChannel(callID, ch, cst, now, out)
		}
	}
}

func (s *Session) flushAcks(now time.Time, out *Outbound) {
	for callID, st := range s.calls {
		for ch, cst := range st.channels {
			if cst.started {
				s.ackChannel(callID, ch, cst, now, out)
			}
		}
	}
}

func (s *Session) ackChannel(callID string, ch uint8, cst *channelState, now time.Time, out *Outbound) {
	cst.lastAck = now
	cst.sinceAck = 0
	if cst.everAcked && cst.acked == cst.through {
		return // nothing new to promise
	}
	if err := s.sink.SetWatermark(callID, ch, cst.through); err != nil {
		// Do not ack what we could not record as durable. The client keeps the
		// audio and we try again on the next interval.
		return
	}
	cst.acked, cst.everAcked = cst.through, true
	out.Acks = append(out.Acks, wire.Ack{
		T: wire.KindAck, CallID: callID, Channel: ch, ThroughSeq: cst.through,
	})
}

func (s *Session) expireOrphans(now time.Time, out *Outbound) {
	for callID, recs := range s.orphans {
		if len(recs) == 0 || now.Sub(recs[0].at) < s.cfg.OrphanGrace {
			continue
		}
		delete(s.orphans, callID)
		out.Errors = append(out.Errors, wire.CallError{
			T: wire.KindCallError, CallID: callID, Code: wire.CodeUnknownCall,
			Message: fmt.Sprintf("no call.start within %s", s.cfg.OrphanGrace), Fatal: true,
		})
	}
}

func watermarkMap(st *callState) map[string]uint32 {
	out := map[string]uint32{}
	for ch, cst := range st.channels {
		if cst.started {
			out[fmt.Sprint(ch)] = cst.through
		}
	}
	return out
}
