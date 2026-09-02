// Package wire implements version 1 of the Sentinel ingest protocol.
//
// contracts/wire.md is the specification and contracts/fixtures/wire_vectors.json
// holds the conformance vectors. The Rust client implements the same bytes in
// client/sentinel-core/src/protocol.rs; both sides run the same vectors, because two
// implementations of a binary protocol reviewed only by eye will drift.
package wire

import (
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
)

const (
	// Version carried in byte 0 of every media record.
	Version = 1
	// HeaderLen is the fixed media record header size.
	HeaderLen = 34
	// FramesPerSegment is the number of 20 ms Opus packets in a one-second segment.
	FramesPerSegment = 50
	// MaxMessageBytes is the largest WebSocket message the gateway accepts.
	MaxMessageBytes = 1 << 20
)

// Channel 0 carries the borrower (render loopback), channel 1 the agent
// (microphone). They are never mixed: separate channels give exact speaker
// attribution with no diarization step.
const (
	ChannelFar  uint8 = 0
	ChannelNear uint8 = 1
)

var (
	ErrTruncated     = errors.New("wire: record truncated")
	ErrVersion       = errors.New("wire: unsupported protocol version")
	ErrChannel       = errors.New("wire: invalid channel")
	ErrReservedFlags = errors.New("wire: reserved flag bits set")
	ErrReservedByte  = errors.New("wire: reserved byte is not zero")
	ErrPayloadSize   = errors.New("wire: payload exceeds the u16 length field")
	ErrMessageSize   = errors.New("wire: message exceeds the size limit")
)

const (
	flagForeign         uint8 = 1 << 0
	flagSilenceInserted uint8 = 1 << 1
	flagReserved        uint8 = 0xFC
)

// Flags on a media record.
type Flags struct {
	// Foreign marks tier B audio captured while the softphone session was
	// Inactive. The gateway stores these segments so we can show what was
	// discarded, but they are never transcribed.
	Foreign bool
	// SilenceInserted marks a record carrying synthesised silence over a glitch
	// gap, so timestamps stay aligned across the two channels.
	SilenceInserted bool
}

func (f Flags) bits() uint8 {
	var b uint8
	if f.Foreign {
		b |= flagForeign
	}
	if f.SilenceInserted {
		b |= flagSilenceInserted
	}
	return b
}

func flagsFromBits(b uint8) (Flags, error) {
	if b&flagReserved != 0 {
		return Flags{}, fmt.Errorf("%w: %#08b", ErrReservedFlags, b)
	}
	return Flags{
		Foreign:         b&flagForeign != 0,
		SilenceInserted: b&flagSilenceInserted != 0,
	}, nil
}

// MediaRecord is one one-second segment of Opus audio for one channel of one call.
type MediaRecord struct {
	Channel     uint8
	Flags       Flags
	Seq         uint32
	TimestampMS uint64
	// CallID is a ULID in binary form.
	CallID  [16]byte
	Payload []byte
}

// Encode appends the little-endian representation of r to dst.
func (r *MediaRecord) Encode(dst []byte) ([]byte, error) {
	if len(r.Payload) > 0xFFFF {
		return nil, fmt.Errorf("%w: %d bytes", ErrPayloadSize, len(r.Payload))
	}
	var hdr [HeaderLen]byte
	hdr[0] = Version
	hdr[1] = r.Channel
	hdr[2] = r.Flags.bits()
	hdr[3] = 0
	binary.LittleEndian.PutUint32(hdr[4:8], r.Seq)
	binary.LittleEndian.PutUint64(hdr[8:16], r.TimestampMS)
	copy(hdr[16:32], r.CallID[:])
	binary.LittleEndian.PutUint16(hdr[32:34], uint16(len(r.Payload)))
	dst = append(dst, hdr[:]...)
	return append(dst, r.Payload...), nil
}

// Decode reads one record from the front of buf and reports how many bytes it used.
func Decode(buf []byte) (MediaRecord, int, error) {
	var r MediaRecord
	if len(buf) < HeaderLen {
		return r, 0, fmt.Errorf("%w: need %d bytes, have %d", ErrTruncated, HeaderLen, len(buf))
	}
	if buf[0] != Version {
		return r, 0, fmt.Errorf("%w: %d", ErrVersion, buf[0])
	}
	if buf[1] != ChannelFar && buf[1] != ChannelNear {
		return r, 0, fmt.Errorf("%w: %d", ErrChannel, buf[1])
	}
	flags, err := flagsFromBits(buf[2])
	if err != nil {
		return r, 0, err
	}
	if buf[3] != 0 {
		return r, 0, fmt.Errorf("%w: %d", ErrReservedByte, buf[3])
	}
	payloadLen := int(binary.LittleEndian.Uint16(buf[32:34]))
	total := HeaderLen + payloadLen
	if len(buf) < total {
		return r, 0, fmt.Errorf("%w: need %d bytes, have %d", ErrTruncated, total, len(buf))
	}
	r.Channel = buf[1]
	r.Flags = flags
	r.Seq = binary.LittleEndian.Uint32(buf[4:8])
	r.TimestampMS = binary.LittleEndian.Uint64(buf[8:16])
	copy(r.CallID[:], buf[16:32])
	// Copy rather than alias: the caller's buffer is a reused read buffer, and
	// aliasing it hands the storage layer bytes that change under it.
	r.Payload = append([]byte(nil), buf[HeaderLen:total]...)
	return r, total, nil
}

// DecodeAll reads every record in a concatenated binary WebSocket message.
func DecodeAll(buf []byte) ([]MediaRecord, error) {
	if len(buf) > MaxMessageBytes {
		return nil, fmt.Errorf("%w: %d bytes", ErrMessageSize, len(buf))
	}
	var out []MediaRecord
	for off := 0; off < len(buf); {
		r, used, err := Decode(buf[off:])
		if err != nil {
			return nil, fmt.Errorf("record %d at offset %d: %w", len(out), off, err)
		}
		off += used
		out = append(out, r)
	}
	return out, nil
}

// PackSegment lays out 50 length-delimited Opus packets. A zero length marks a frame
// dropped by a glitch, which the decoder replaces with 20 ms of silence.
func PackSegment(frames [][]byte) []byte {
	out := make([]byte, 0, len(frames)*62)
	var l [2]byte
	for _, f := range frames {
		binary.LittleEndian.PutUint16(l[:], uint16(len(f)))
		out = append(out, l[:]...)
		out = append(out, f...)
	}
	return out
}

// UnpackSegment is the inverse of PackSegment.
func UnpackSegment(payload []byte) ([][]byte, error) {
	var out [][]byte
	for off := 0; off < len(payload); {
		if len(payload)-off < 2 {
			return nil, ErrTruncated
		}
		n := int(binary.LittleEndian.Uint16(payload[off : off+2]))
		off += 2
		if len(payload)-off < n {
			return nil, ErrTruncated
		}
		out = append(out, payload[off:off+n])
		off += n
	}
	return out, nil
}

// ------------------------------------------------------------------- control

// ControlKind discriminates a JSON control frame.
type ControlKind string

const (
	KindCallStart    ControlKind = "call.start"
	KindCallEnd      ControlKind = "call.end"
	KindAck          ControlKind = "ack"
	KindResume       ControlKind = "resume"
	KindCallError    ControlKind = "call.error"
	KindHeartbeat    ControlKind = "heartbeat"
	KindHeartbeatAck ControlKind = "heartbeat.ack"
)

// Error codes carried in call.error.
const (
	CodeTenantMismatch = "tenant_mismatch"
	CodeUserMismatch   = "user_mismatch"
	CodeDeviceMismatch = "device_mismatch"
	CodeUnknownCall    = "unknown_call"
	CodeBadFrame       = "bad_frame"
	CodeQuotaExceeded  = "quota_exceeded"
	CodeInternal       = "internal"
)

// WebSocket close codes with Sentinel meanings.
const (
	CloseTokenInvalid = 4401
	CloseForbidden    = 4403
	CloseIdle         = 4408
	CloseTooMany      = 4429
)

type CallStart struct {
	T            ControlKind `json:"t"`
	CallID       string      `json:"call_id"`
	StartedAt    string      `json:"started_at"`
	UserUID      string      `json:"user_uid"`
	DeviceID     string      `json:"device_id"`
	Tier         string      `json:"tier"`
	AccountRef   *string     `json:"account_ref"`
	DialerCallID *string     `json:"dialer_call_id"`
	Direction    string      `json:"direction"`
	Codec        string      `json:"codec"`
	Rate         int         `json:"rate"`
}

type CallEnd struct {
	T        ControlKind       `json:"t"`
	CallID   string            `json:"call_id"`
	EndedAt  string            `json:"ended_at"`
	Reason   string            `json:"reason"`
	LastSeq  map[string]uint32 `json:"last_seq"`
}

type Ack struct {
	T          ControlKind `json:"t"`
	CallID     string      `json:"call_id"`
	Channel    uint8       `json:"channel"`
	ThroughSeq uint32      `json:"through_seq"`
}

type Resume struct {
	T      ControlKind       `json:"t"`
	CallID string            `json:"call_id"`
	Acked  map[string]uint32 `json:"acked"`
}

type CallError struct {
	T       ControlKind `json:"t"`
	CallID  string      `json:"call_id"`
	Code    string      `json:"code"`
	Message string      `json:"message"`
	Fatal   bool        `json:"fatal"`
}

type Heartbeat struct {
	T            ControlKind `json:"t"`
	SentAt       string      `json:"sent_at"`
	CaptureState string      `json:"capture_state"`
	SpoolDepth   uint64      `json:"spool_depth"`
}

type HeartbeatAck struct {
	T             ControlKind `json:"t"`
	ServerTime    string      `json:"server_time"`
	PolicyVersion int64       `json:"policy_version"`
}

// PeekKind reads the discriminator without committing to a concrete type.
func PeekKind(b []byte) (ControlKind, error) {
	var probe struct {
		T ControlKind `json:"t"`
	}
	if err := json.Unmarshal(b, &probe); err != nil {
		return "", err
	}
	if probe.T == "" {
		return "", errors.New("wire: control frame has no discriminator")
	}
	return probe.T, nil
}
