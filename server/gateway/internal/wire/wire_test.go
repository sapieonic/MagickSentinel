package wire

import (
	"encoding/hex"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"testing"
)

type vectors struct {
	MediaRecords []struct {
		Name   string `json:"name"`
		Record struct {
			Channel     uint8  `json:"channel"`
			Flags       Flags  `json:"flags"`
			Seq         uint32 `json:"seq"`
			TimestampMS uint64 `json:"timestamp_ms"`
			CallIDHex   string `json:"call_id_hex"`
			PayloadHex  string `json:"payload_hex"`
		} `json:"record"`
		EncodedHex string `json:"encoded_hex"`
	} `json:"media_records"`
	SegmentPayload struct {
		FramesHex []string `json:"frames_hex"`
		PackedHex string   `json:"packed_hex"`
	} `json:"segment_payload"`
	Invalid []struct {
		Name  string `json:"name"`
		Hex   string `json:"hex"`
		Error string `json:"error"`
	} `json:"invalid"`
}

// The fixture uses snake_case; Flags uses Go field names, so give it tags here
// rather than polluting the wire type.
func (f *Flags) UnmarshalJSON(b []byte) error {
	var raw struct {
		Foreign         bool `json:"foreign"`
		SilenceInserted bool `json:"silence_inserted"`
	}
	if err := json.Unmarshal(b, &raw); err != nil {
		return err
	}
	f.Foreign, f.SilenceInserted = raw.Foreign, raw.SilenceInserted
	return nil
}

func loadVectors(t *testing.T) vectors {
	t.Helper()
	path := filepath.Join("..", "..", "..", "..", "contracts", "fixtures", "wire_vectors.json")
	b, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read fixtures: %v", err)
	}
	var v vectors
	if err := json.Unmarshal(b, &v); err != nil {
		t.Fatalf("parse fixtures: %v", err)
	}
	if len(v.MediaRecords) == 0 {
		t.Fatal("fixture file has no media records")
	}
	return v
}

func TestMediaRecordsMatchTheSharedVectors(t *testing.T) {
	v := loadVectors(t)
	for _, c := range v.MediaRecords {
		t.Run(c.Name, func(t *testing.T) {
			callID, _ := hex.DecodeString(c.Record.CallIDHex)
			payload, _ := hex.DecodeString(c.Record.PayloadHex)
			var r MediaRecord
			r.Channel = c.Record.Channel
			r.Flags = c.Record.Flags
			r.Seq = c.Record.Seq
			r.TimestampMS = c.Record.TimestampMS
			copy(r.CallID[:], callID)
			r.Payload = payload

			got, err := r.Encode(nil)
			if err != nil {
				t.Fatalf("encode: %v", err)
			}
			if hex.EncodeToString(got) != c.EncodedHex {
				t.Fatalf("encoding drifted from the contract\n got %s\nwant %s",
					hex.EncodeToString(got), c.EncodedHex)
			}

			back, used, err := Decode(got)
			if err != nil {
				t.Fatalf("decode: %v", err)
			}
			if used != len(got) {
				t.Fatalf("consumed %d of %d bytes", used, len(got))
			}
			if back.Channel != r.Channel || back.Seq != r.Seq || back.TimestampMS != r.TimestampMS ||
				back.Flags != r.Flags || back.CallID != r.CallID || string(back.Payload) != string(r.Payload) {
				t.Fatalf("round trip mismatch:\n got %+v\nwant %+v", back, r)
			}
		})
	}
}

func TestSegmentPayloadMatchesTheSharedVectors(t *testing.T) {
	v := loadVectors(t)
	frames := make([][]byte, 0, len(v.SegmentPayload.FramesHex))
	for _, f := range v.SegmentPayload.FramesHex {
		b, _ := hex.DecodeString(f)
		frames = append(frames, b)
	}
	if got := hex.EncodeToString(PackSegment(frames)); got != v.SegmentPayload.PackedHex {
		t.Fatalf("packed segment drifted\n got %s\nwant %s", got, v.SegmentPayload.PackedHex)
	}
	back, err := UnpackSegment(PackSegment(frames))
	if err != nil {
		t.Fatalf("unpack: %v", err)
	}
	if len(back) != len(frames) {
		t.Fatalf("got %d frames, want %d", len(back), len(frames))
	}
	if len(back[1]) != 0 {
		t.Error("a zero-length frame must survive as a dropped frame, not vanish")
	}
}

func TestInvalidRecordsAreRejectedWithTheDocumentedReason(t *testing.T) {
	want := map[string]error{
		"version":        ErrVersion,
		"reserved_flags": ErrReservedFlags,
		"reserved_byte":  ErrReservedByte,
		"channel":        ErrChannel,
		"truncated":      ErrTruncated,
	}
	for _, c := range loadVectors(t).Invalid {
		t.Run(c.Name, func(t *testing.T) {
			b, err := hex.DecodeString(c.Hex)
			if err != nil {
				t.Fatalf("bad fixture hex: %v", err)
			}
			_, _, err = Decode(b)
			if err == nil {
				t.Fatal("expected rejection, got a decoded record")
			}
			if !errors.Is(err, want[c.Error]) {
				t.Fatalf("expected %v, got %v", want[c.Error], err)
			}
		})
	}
}

func TestConcatenatedRecordsDecodeInOrder(t *testing.T) {
	var buf []byte
	for i := uint32(0); i < 10; i++ {
		r := MediaRecord{Channel: ChannelNear, Seq: i, TimestampMS: uint64(i) * 1000,
			Payload: []byte{byte(i)}}
		var err error
		buf, err = r.Encode(buf)
		if err != nil {
			t.Fatal(err)
		}
	}
	recs, err := DecodeAll(buf)
	if err != nil {
		t.Fatal(err)
	}
	if len(recs) != 10 {
		t.Fatalf("got %d records, want 10", len(recs))
	}
	for i, r := range recs {
		if r.Seq != uint32(i) {
			t.Fatalf("record %d has seq %d", i, r.Seq)
		}
	}
}

func TestDecodedPayloadDoesNotAliasTheReadBuffer(t *testing.T) {
	// The ingest loop reuses its read buffer. If Decode aliased it, the storage
	// layer would be handed bytes that change underneath it between the decode and
	// the write, which corrupts audio in a way that only shows up under load.
	r := MediaRecord{Channel: ChannelFar, Seq: 1, Payload: []byte{1, 2, 3}}
	buf, _ := r.Encode(nil)
	back, _, err := Decode(buf)
	if err != nil {
		t.Fatal(err)
	}
	for i := range buf {
		buf[i] = 0xFF
	}
	if back.Payload[0] != 1 || back.Payload[1] != 2 || back.Payload[2] != 3 {
		t.Fatalf("payload aliased the source buffer: %v", back.Payload)
	}
}

func TestOversizedMessageIsRejected(t *testing.T) {
	if _, err := DecodeAll(make([]byte, MaxMessageBytes+1)); !errors.Is(err, ErrMessageSize) {
		t.Fatalf("expected a size error, got %v", err)
	}
}

func TestPeekKind(t *testing.T) {
	k, err := PeekKind([]byte(`{"t":"call.start","call_id":"01J"}`))
	if err != nil || k != KindCallStart {
		t.Fatalf("got %q, %v", k, err)
	}
	if _, err := PeekKind([]byte(`{"call_id":"01J"}`)); err == nil {
		t.Fatal("a control frame with no discriminator must be rejected")
	}
}
