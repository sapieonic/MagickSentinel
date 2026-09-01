package ingest

import (
	"context"
	"encoding/json"
	"log/slog"
	"net/http"
	"time"

	"github.com/coder/websocket"

	"github.com/magickvoice/sentinel/server/gateway/internal/auth"
	"github.com/magickvoice/sentinel/server/gateway/internal/wire"
)

// Handler serves WSS /v1/ingest.
type Handler struct {
	Log         *slog.Logger
	Config      Config
	NewSink     func(peer Peer) Sink
	PolicyVer   func(ctx context.Context, tenantID string) int64
	// DeviceActive is polled so a revocation takes effect within 60 s, which is the
	// requirement when a laptop leaves the building.
	DeviceActive func(ctx context.Context, tenantID, deviceID string) bool
	RevokePoll   time.Duration
	Now          func() time.Time
}

func (h *Handler) now() time.Time {
	if h.Now != nil {
		return h.Now()
	}
	return time.Now()
}

func (h *Handler) revokePoll() time.Duration {
	if h.RevokePoll == 0 {
		return 15 * time.Second
	}
	return h.RevokePoll
}

func (h *Handler) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	id := auth.FromContext(r.Context())
	if id == nil || id.DeviceID == "" {
		http.Error(w, "device and user identity are both required", http.StatusUnauthorized)
		return
	}

	conn, err := websocket.Accept(w, r, &websocket.AcceptOptions{
		Subprotocols: []string{"sentinel.v1"},
		// Compression is off: Opus is already compressed, so the CPU buys nothing
		// and per-message deflate would add a shared context across frames.
		CompressionMode: websocket.CompressionDisabled,
	})
	if err != nil {
		return
	}
	conn.SetReadLimit(wire.MaxMessageBytes)
	defer conn.CloseNow()

	peer := Peer{TenantID: id.TenantID, UserUID: id.UserUID, DeviceID: id.DeviceID}
	var policyVersion int64
	if h.PolicyVer != nil {
		policyVersion = h.PolicyVer(r.Context(), id.TenantID)
	}
	session := NewSession(peer, h.NewSink(peer), h.Config, policyVersion, h.now())

	ctx, cancel := context.WithCancel(r.Context())
	defer cancel()

	// A single goroutine owns the session, so it needs no locking. Timers and the
	// revocation poll deliver into the same select.
	frames := make(chan frame, 8)
	go h.readLoop(ctx, conn, frames)

	ticker := time.NewTicker(time.Second)
	defer ticker.Stop()
	revoke := time.NewTicker(h.revokePoll())
	defer revoke.Stop()

	for {
		select {
		case <-ctx.Done():
			return

		case f, ok := <-frames:
			if !ok {
				return
			}
			out := h.dispatch(session, f)
			if !h.send(ctx, conn, out) {
				return
			}

		case <-ticker.C:
			if !h.send(ctx, conn, session.Tick(h.now())) {
				return
			}

		case <-revoke.C:
			if h.DeviceActive != nil && !h.DeviceActive(ctx, id.TenantID, id.DeviceID) {
				h.send(ctx, conn, session.Revoke())
				return
			}
		}
	}
}

type frame struct {
	binary bool
	data   []byte
	err    error
}

func (h *Handler) readLoop(ctx context.Context, conn *websocket.Conn, out chan<- frame) {
	defer close(out)
	for {
		typ, data, err := conn.Read(ctx)
		if err != nil {
			return
		}
		select {
		case out <- frame{binary: typ == websocket.MessageBinary, data: data}:
		case <-ctx.Done():
			return
		}
	}
}

func (h *Handler) dispatch(s *Session, f frame) Outbound {
	now := h.now()
	if f.binary {
		records, err := wire.DecodeAll(f.data)
		if err != nil {
			// A malformed binary frame means the two sides disagree about the
			// protocol. There is nothing useful to salvage from the rest of the
			// connection, so close it and let the client reconnect cleanly.
			h.log().Warn("ingest: bad media frame", "error", err)
			return Outbound{Close: int(websocket.StatusInvalidFramePayloadData), CloseReason: "bad media frame"}
		}
		var merged Outbound
		for _, rec := range records {
			out, err := s.OnMedia(rec, now)
			mergeOutbound(&merged, out)
			if err != nil {
				h.log().Error("ingest: store segment", "error", err)
				break
			}
		}
		return merged
	}

	kind, err := wire.PeekKind(f.data)
	if err != nil {
		return Outbound{Close: int(websocket.StatusInvalidFramePayloadData), CloseReason: "bad control frame"}
	}
	switch kind {
	case wire.KindCallStart:
		var cs wire.CallStart
		if err := json.Unmarshal(f.data, &cs); err != nil {
			return Outbound{Close: int(websocket.StatusInvalidFramePayloadData), CloseReason: "bad call.start"}
		}
		out, err := s.OnCallStart(cs, now)
		if err != nil {
			h.log().Error("ingest: open call", "error", err)
		}
		return out
	case wire.KindCallEnd:
		var ce wire.CallEnd
		if err := json.Unmarshal(f.data, &ce); err != nil {
			return Outbound{Close: int(websocket.StatusInvalidFramePayloadData), CloseReason: "bad call.end"}
		}
		out, err := s.OnCallEnd(ce, now)
		if err != nil {
			h.log().Error("ingest: finish call", "error", err)
		}
		return out
	case wire.KindHeartbeat:
		var hb wire.Heartbeat
		_ = json.Unmarshal(f.data, &hb)
		return s.OnHeartbeat(hb, now)
	default:
		// Unknown control frames are ignored rather than fatal, so the server can
		// add message types without breaking clients that predate them.
		h.log().Debug("ingest: ignoring unknown control frame", "kind", kind)
		return Outbound{}
	}
}

func mergeOutbound(dst *Outbound, src Outbound) {
	dst.Acks = append(dst.Acks, src.Acks...)
	dst.Resumes = append(dst.Resumes, src.Resumes...)
	dst.Errors = append(dst.Errors, src.Errors...)
	dst.Heartbeats = append(dst.Heartbeats, src.Heartbeats...)
	if src.Close != 0 {
		dst.Close, dst.CloseReason = src.Close, src.CloseReason
	}
}

// send writes an Outbound. It returns false when the connection is finished.
func (h *Handler) send(ctx context.Context, conn *websocket.Conn, out Outbound) bool {
	if out.empty() {
		return true
	}
	write := func(v any) bool {
		b, err := json.Marshal(v)
		if err != nil {
			return true
		}
		wctx, cancel := context.WithTimeout(ctx, 10*time.Second)
		defer cancel()
		return conn.Write(wctx, websocket.MessageText, b) == nil
	}
	for _, m := range out.Resumes {
		if !write(m) {
			return false
		}
	}
	for _, m := range out.Acks {
		if !write(m) {
			return false
		}
	}
	for _, m := range out.Errors {
		if !write(m) {
			return false
		}
	}
	for _, m := range out.Heartbeats {
		if !write(m) {
			return false
		}
	}
	if out.Close != 0 {
		_ = conn.Close(websocket.StatusCode(out.Close), truncateReason(out.CloseReason))
		return false
	}
	return true
}

// WebSocket close reasons are capped at 123 bytes by the protocol.
func truncateReason(s string) string {
	if len(s) <= 123 {
		return s
	}
	return s[:123]
}

func (h *Handler) log() *slog.Logger {
	if h.Log != nil {
		return h.Log
	}
	return slog.Default()
}

